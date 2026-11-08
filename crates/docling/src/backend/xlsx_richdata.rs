//! Images placed in cells — Excel 365's "Place in Cell" (docling#5015).
//!
//! Such an image is not a drawing: it is a *rich value*. The cell keeps the
//! error value `#VALUE!` as a fallback for readers without rich-data support
//! and refers to the image through its `vm` (value metadata) attribute:
//!
//! ```text
//! <c vm="1"> -> metadata valueMetadata -> futureMetadata XLRICHVALUE
//!            -> rdrichvalue rv -> rdrichvaluestructure "_localImage"
//!            -> richValueRel rel -> image part
//! ```
//!
//! A port of docling's `_rich_value_images` / `_cells_with_rich_values`: only
//! rich values with the `_localImage` structure are images (linked data types
//! and the like are other structures and stay their fallback text); the
//! value's `Text` key is Excel's alternative text, which docling writes as the
//! picture's description.

use std::collections::HashMap;
use std::sync::Arc;

use docling_core::PictureImage;
use roxmltree::Document;

use super::ooxml::{resolve, Package, Relationship};

/// One image placed in a cell: the decoded image (`None` when the part is
/// missing or undecodable — the cell is still a picture, as in docling) and
/// its alternative text.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct CellImage {
    pub(crate) image: Option<PictureImage>,
    pub(crate) alt_text: Option<String>,
}

/// The workbook's images placed in cells, by the 1-based `vm` index a cell
/// refers to them with. Empty unless the workbook relates all four rich-data
/// parts (docling's `_rich_data_parts`: the relationship types are matched by
/// their last segment, whose namespace differs between Excel versions).
pub(crate) fn rich_value_images(
    pkg: &mut Package,
    workbook_rels: &[Relationship],
) -> HashMap<u32, Arc<CellImage>> {
    let part = |name: &str| {
        workbook_rels
            .iter()
            .find(|r| r.rel_type.rsplit('/').next() == Some(name))
            .map(|r| resolve("xl", &r.target))
    };
    let (Some(meta), Some(rv), Some(rvs), Some(rvrel)) = (
        part("sheetMetadata"),
        part("rdRichValue"),
        part("rdRichValueStructure"),
        part("richValueRel"),
    ) else {
        return HashMap::new();
    };
    let (Some(meta_xml), Some(rv_xml), Some(rvs_xml), Some(rvrel_xml)) = (
        pkg.read(&meta),
        pkg.read(&rv),
        pkg.read(&rvs),
        pkg.read(&rvrel),
    ) else {
        return HashMap::new();
    };
    let rvrel_dir = rvrel.rsplit_once('/').map(|(d, _)| d).unwrap_or("");
    let targets: HashMap<String, String> = pkg
        .rels_for(&rvrel)
        .into_iter()
        .filter(|r| r.rel_type.ends_with("/image"))
        .map(|r| (r.id, resolve(rvrel_dir, &r.target)))
        .collect();
    let index = parse_rich_values(&meta_xml, &rv_xml, &rvs_xml, &rvrel_xml);
    // Each image part is decoded once, however many cells show it.
    let mut decoded: HashMap<String, Option<PictureImage>> = HashMap::new();
    let mut out = HashMap::new();
    for (vm, (rel_id, alt_text)) in index {
        let Some(target) = targets.get(&rel_id) else {
            continue;
        };
        let image = decoded
            .entry(target.clone())
            .or_insert_with(|| {
                pkg.read_bytes(target)
                    .and_then(|bytes| super::ooxml::picture_image(target, bytes))
            })
            .clone();
        out.insert(vm, Arc::new(CellImage { image, alt_text }));
    }
    out
}

/// The XML half of [`rich_value_images`]: `vm` → (the image's relationship
/// id in `richValueRel`, its alternative text). Malformed parts yield nothing.
fn parse_rich_values(
    metadata: &str,
    rich_values: &str,
    structures: &str,
    rel_list: &str,
) -> HashMap<u32, (String, Option<String>)> {
    let mut out = HashMap::new();
    let (Ok(meta), Ok(rv), Ok(rvs), Ok(rels)) = (
        Document::parse(metadata),
        Document::parse(rich_values),
        Document::parse(structures),
        Document::parse(rel_list),
    ) else {
        return out;
    };
    let root = meta.root_element();
    // Metadata types are 1-based in `rc/@t`.
    let Some(rich_type) = children(root, "metadataTypes")
        .flat_map(|n| children(n, "metadataType"))
        .position(|n| n.attribute("name") == Some("XLRICHVALUE"))
        .map(|i| (i + 1).to_string())
    else {
        return out;
    };
    // `futureMetadata[@name='XLRICHVALUE']/bk` → its `rvb/@i`, the rich
    // value index.
    let future: Vec<Option<usize>> = children(root, "futureMetadata")
        .filter(|n| n.attribute("name") == Some("XLRICHVALUE"))
        .flat_map(|n| children(n, "bk"))
        .map(|bk| {
            bk.descendants()
                .find(|d| d.is_element() && d.tag_name().name() == "rvb")
                .and_then(|d| d.attribute("i")?.parse().ok())
        })
        .collect();
    let structs: Vec<(Option<&str>, Vec<Option<&str>>)> = children(rvs.root_element(), "s")
        .map(|s| {
            let keys = children(s, "k").map(|k| k.attribute("n")).collect();
            (s.attribute("t"), keys)
        })
        .collect();
    let rel_ids: Vec<Option<&str>> = children(rels.root_element(), "rel")
        .map(|r| {
            r.attributes()
                .find(|a| {
                    a.name() == "id"
                        && a.namespace()
                            .is_some_and(|ns| ns.ends_with("/relationships"))
                })
                .map(|a| a.value())
        })
        .collect();
    // Rich value index → (relationship id, alt text), `_localImage` only.
    let images: HashMap<usize, (String, Option<String>)> = children(rv.root_element(), "rv")
        .enumerate()
        .filter_map(|(index, node)| {
            let s: usize = node.attribute("s")?.parse().ok()?;
            let (kind, keys) = structs.get(s)?;
            if *kind != Some("_localImage") {
                return None;
            }
            let values: Vec<String> = children(node, "v")
                .map(|v| v.text().unwrap_or("").to_string())
                .collect();
            let field = |key: &str| {
                let at = keys.iter().position(|k| *k == Some(key))?;
                values.get(at)
            };
            let rel: usize = field("_rvRel:LocalImageIdentifier")?.trim().parse().ok()?;
            let rel_id = (*rel_ids.get(rel)?)?.to_string();
            let alt = field("Text").filter(|t| !t.is_empty()).cloned();
            Some((index, (rel_id, alt)))
        })
        .collect();
    let blocks = children(root, "valueMetadata").flat_map(|n| children(n, "bk"));
    for (vm, bk) in blocks.enumerate() {
        // docling reads the block's (first) `rc` record.
        let Some(rc) = children(bk, "rc").next() else {
            continue;
        };
        if rc.attribute("t") != Some(rich_type.as_str()) {
            continue;
        }
        let Some(Some(rv_index)) = rc
            .attribute("v")
            .and_then(|v| v.parse::<usize>().ok())
            .and_then(|v| future.get(v))
        else {
            continue;
        };
        if let Some(image) = images.get(rv_index) {
            out.insert(vm as u32 + 1, image.clone());
        }
    }
    out
}

/// The element children of `n` named `name` (any namespace).
fn children<'a, 'i>(
    n: roxmltree::Node<'a, 'i>,
    name: &'static str,
) -> impl Iterator<Item = roxmltree::Node<'a, 'i>> {
    n.children()
        .filter(move |c| c.is_element() && c.tag_name().name() == name)
}

/// The `vm` index of every cell of a worksheet that has one, by absolute
/// 0-based `(row, col)` — calamine does not keep the attribute, so the sheet
/// XML is scanned again (docling's `_cells_with_rich_values`, which re-reads
/// it past openpyxl the same way).
pub(crate) fn cells_with_rich_values(sheet_xml: &str) -> HashMap<(u32, u32), u32> {
    let mut out = HashMap::new();
    let tag = cached_regex!(r"<c\s[^>]*>");
    let attr_r = cached_regex!(r#"\br="([A-Z]+)(\d+)""#);
    let attr_vm = cached_regex!(r#"\bvm="(\d+)""#);
    for m in tag.find_iter(sheet_xml) {
        let t = m.as_str();
        let Some(vm) = attr_vm.captures(t).and_then(|c| c[1].parse::<u32>().ok()) else {
            continue;
        };
        let Some(r) = attr_r.captures(t) else {
            continue;
        };
        let col = r[1].bytes().fold(0u32, |acc, b| {
            acc.saturating_mul(26).saturating_add((b - b'A' + 1) as u32)
        });
        let Ok(row) = r[2].parse::<u32>() else {
            continue;
        };
        if row == 0 || col == 0 {
            continue;
        }
        out.insert((row - 1, col - 1), vm);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const META: &str = r#"<metadata xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main" xmlns:xlrd="http://schemas.microsoft.com/office/spreadsheetml/2017/richdata"><metadataTypes count="2"><metadataType name="XLDAPR"/><metadataType name="XLRICHVALUE"/></metadataTypes><futureMetadata name="XLRICHVALUE" count="3"><bk><extLst><ext><xlrd:rvb i="0"/></ext></extLst></bk><bk><extLst><ext><xlrd:rvb i="1"/></ext></extLst></bk><bk><extLst><ext><xlrd:rvb i="2"/></ext></extLst></bk></futureMetadata><valueMetadata count="4"><bk><rc t="2" v="0"/></bk><bk><rc t="1" v="1"/></bk><bk><rc t="2" v="1"/></bk><bk><rc t="2" v="2"/></bk></valueMetadata></metadata>"#;
    const RV: &str = r#"<rvData xmlns="http://schemas.microsoft.com/office/spreadsheetml/2017/richdata"><rv s="0"><v>1</v><v>5</v><v>A cat</v></rv><rv s="1"><v>0</v><v>5</v></rv><rv s="2"><v>Seattle</v></rv></rvData>"#;
    const RVS: &str = r#"<rvStructures xmlns="http://schemas.microsoft.com/office/spreadsheetml/2017/richdata"><s t="_localImage"><k n="_rvRel:LocalImageIdentifier" t="i"/><k n="CalcOrigin" t="i"/><k n="Text" t="s"/></s><s t="_localImage"><k n="_rvRel:LocalImageIdentifier" t="i"/><k n="CalcOrigin" t="i"/></s><s t="_linkedEntity"><k n="Name" t="s"/></s></rvStructures>"#;
    const RELS: &str = r#"<richValueRels xmlns="http://schemas.microsoft.com/office/spreadsheetml/2022/richvaluerel" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships"><rel r:id="rIdA"/><rel r:id="rIdB"/></richValueRels>"#;

    /// The `vm` chain: a block of another metadata type, and a rich value of
    /// a non-image structure (a linked data type), are not images.
    #[test]
    fn resolves_vm_through_the_rich_value_chain() {
        let got = parse_rich_values(META, RV, RVS, RELS);
        assert_eq!(got.len(), 2, "{got:?}");
        assert_eq!(got[&1], ("rIdB".to_string(), Some("A cat".to_string())));
        // vm 2 is an `XLDAPR` block; vm 4 is the linked entity.
        assert_eq!(got[&3], ("rIdA".to_string(), None));
    }

    #[test]
    fn scans_vm_cells() {
        let xml = r#"<sheetData><row r="2"><c r="B2" t="e" vm="1"><v>#VALUE!</v></c><c r="C2" t="n"><v>1</v></c><c r="AA10" vm="3" t="e"/></row></sheetData>"#;
        let got = cells_with_rich_values(xml);
        assert_eq!(got.len(), 2);
        assert_eq!(got[&(1, 1)], 1);
        assert_eq!(got[&(9, 26)], 3);
    }
}
