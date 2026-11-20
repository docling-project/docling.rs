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

/// Where one structure of rich value keeps its images (docling's
/// `_RichImageKind`): the workbook part, the rich value key holding the
/// image's position in that part, the part's entries, and the child of an
/// entry carrying the relationship id (`None`: the entry itself).
struct ImageKind {
    structure: &'static str,
    part: &'static str,
    position_key: &'static str,
    entry: &'static str,
    id_child: Option<&'static str>,
}

/// "Place in Cell" images (docling#5015) and images from the `IMAGE()`
/// function (docling#5271, #1169), the downloaded copy Excel stores in
/// `rdRichValueWebImage.xml`.
const IMAGE_KINDS: [ImageKind; 2] = [
    ImageKind {
        structure: "_localImage",
        part: "richValueRel",
        position_key: "_rvRel:LocalImageIdentifier",
        entry: "rel",
        id_child: None,
    },
    ImageKind {
        structure: "_webimage",
        part: "rdRichValueWebImage",
        position_key: "WebImageIdentifier",
        entry: "webImageSrd",
        id_child: Some("blip"),
    },
];

/// The workbook's images placed in cells, by the 1-based `vm` index a cell
/// refers to them with. Empty unless the workbook relates the three
/// rich-data parts and at least one image part (docling's
/// `_rich_data_parts`: the relationship types are matched by their last
/// segment, whose namespace differs between Excel versions).
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
    let (Some(meta), Some(rv), Some(rvs)) = (
        part("sheetMetadata"),
        part("rdRichValue"),
        part("rdRichValueStructure"),
    ) else {
        return HashMap::new();
    };
    // Each kind's image parts, by position in its part; an entry without an
    // image (a web image without a `blip`) keeps its position.
    let mut targets: HashMap<&'static str, Vec<Option<String>>> = HashMap::new();
    for kind in &IMAGE_KINDS {
        let Some(path) = part(kind.part) else {
            continue;
        };
        let Some(xml) = pkg.read(&path) else {
            continue;
        };
        let dir = path.rsplit_once('/').map(|(d, _)| d).unwrap_or("");
        let rels: HashMap<String, String> = pkg
            .rels_for(&path)
            .into_iter()
            .filter(|r| r.rel_type.ends_with("/image"))
            .map(|r| (r.id, resolve(dir, &r.target)))
            .collect();
        let ids = image_entries(&xml, kind);
        targets.insert(
            kind.structure,
            ids.into_iter()
                .map(|id| id.and_then(|id| rels.get(&id).cloned()))
                .collect(),
        );
    }
    if targets.is_empty() {
        return HashMap::new();
    }
    let (Some(meta_xml), Some(rv_xml), Some(rvs_xml)) =
        (pkg.read(&meta), pkg.read(&rv), pkg.read(&rvs))
    else {
        return HashMap::new();
    };
    let index = parse_rich_values(&meta_xml, &rv_xml, &rvs_xml, &targets);
    // Each image part is decoded once, however many cells show it; one that
    // is missing, oversized or undecodable loses its own data only.
    let mut decoded: HashMap<String, Option<PictureImage>> = HashMap::new();
    let mut out = HashMap::new();
    for (vm, (target, alt_text)) in index {
        let image = decoded
            .entry(target.clone())
            .or_insert_with(|| {
                pkg.read_bytes(&target)
                    .filter(|bytes| decodes(bytes))
                    .and_then(|bytes| super::ooxml::picture_image(&target, bytes))
            })
            .clone();
        out.insert(vm, Arc::new(CellImage { image, alt_text }));
    }
    out
}

/// Whether a cell image decodes in full, within the `image` crate's
/// allocation limits — docling loads it through Pillow, so a truncated
/// image, or one whose header declares more pixels than Pillow's limit,
/// loses its data (docling#5271's isolation tests) while the cell stays a
/// picture. A metafile is kept undecoded, as docling keeps it.
fn decodes(bytes: &[u8]) -> bool {
    super::ooxml::is_metafile(bytes)
        || image::ImageReader::new(std::io::Cursor::new(bytes))
            .with_guessed_format()
            .ok()
            .is_some_and(|r| r.decode().is_ok())
}

/// The relationship id of each entry of an image part, by position (`None`
/// for an entry without one).
fn image_entries(xml: &str, kind: &ImageKind) -> Vec<Option<String>> {
    let Ok(doc) = Document::parse(xml) else {
        return Vec::new();
    };
    children(doc.root_element(), kind.entry)
        .map(|entry| {
            let node = match kind.id_child {
                None => Some(entry),
                Some(child) => children(entry, child).next(),
            }?;
            node.attributes()
                .find(|a| {
                    a.name() == "id"
                        && a.namespace()
                            .is_some_and(|ns| ns.ends_with("/relationships"))
                })
                .map(|a| a.value().to_string())
        })
        .collect()
}

/// The XML half of [`rich_value_images`]: `vm` → (the image's part, its
/// alternative text), `targets` being each image structure's parts by
/// position. Malformed parts yield nothing.
fn parse_rich_values(
    metadata: &str,
    rich_values: &str,
    structures: &str,
    targets: &HashMap<&'static str, Vec<Option<String>>>,
) -> HashMap<u32, (String, Option<String>)> {
    let mut out = HashMap::new();
    let (Ok(meta), Ok(rv), Ok(rvs)) = (
        Document::parse(metadata),
        Document::parse(rich_values),
        Document::parse(structures),
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
    // Rich value index → (image part, alt text), image structures only.
    let images: HashMap<usize, (String, Option<String>)> = children(rv.root_element(), "rv")
        .enumerate()
        .filter_map(|(index, node)| {
            let s: usize = node.attribute("s")?.parse().ok()?;
            let (kind, keys) = structs.get(s)?;
            let kind = IMAGE_KINDS.iter().find(|k| Some(k.structure) == *kind)?;
            let values: Vec<String> = children(node, "v")
                .map(|v| v.text().unwrap_or("").to_string())
                .collect();
            let field = |key: &str| {
                let at = keys.iter().position(|k| *k == Some(key))?;
                values.get(at)
            };
            let position: usize = field(kind.position_key)?.trim().parse().ok()?;
            let target = targets.get(kind.structure)?.get(position)?.clone()?;
            let alt = field("Text").filter(|t| !t.is_empty()).cloned();
            Some((index, (target, alt)))
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
    fn targets(entries: Vec<Option<String>>) -> HashMap<&'static str, Vec<Option<String>>> {
        HashMap::from([("_localImage", entries)])
    }

    #[test]
    fn resolves_vm_through_the_rich_value_chain() {
        let ids = image_entries(RELS, &IMAGE_KINDS[0]);
        assert_eq!(ids, [Some("rIdA".to_string()), Some("rIdB".to_string())]);
        let got = parse_rich_values(META, RV, RVS, &targets(ids));
        assert_eq!(got.len(), 2, "{got:?}");
        assert_eq!(got[&1], ("rIdB".to_string(), Some("A cat".to_string())));
        // vm 2 is an `XLDAPR` block; vm 4 is the linked entity.
        assert_eq!(got[&3], ("rIdA".to_string(), None));
    }

    /// docling#5271 (#1169): an `IMAGE()` rich value (`_webimage`) points
    /// into `rdRichValueWebImage.xml` by position; a web image without a
    /// `blip` keeps its position, so the images after it stay aligned, and
    /// its cell keeps `#VALUE!`.
    #[test]
    fn web_images_resolve_by_position_and_skip_blipless_entries() {
        let web = r#"<webImagesSrd xmlns="http://schemas.microsoft.com/office/spreadsheetml/2020/richdatawebimage" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships"><webImageSrd><address r:id="rIdU1"/><blip r:id="rIdI1"/></webImageSrd><webImageSrd><address r:id="rIdU2"/></webImageSrd><webImageSrd><address r:id="rIdU3"/><blip r:id="rIdI3"/></webImageSrd></webImagesSrd>"#;
        let ids = image_entries(web, &IMAGE_KINDS[1]);
        assert_eq!(
            ids,
            [Some("rIdI1".to_string()), None, Some("rIdI3".to_string())]
        );
        let meta = r#"<metadata xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main" xmlns:xlrd="http://schemas.microsoft.com/office/spreadsheetml/2017/richdata"><metadataTypes><metadataType name="XLRICHVALUE"/></metadataTypes><futureMetadata name="XLRICHVALUE"><bk><extLst><ext><xlrd:rvb i="0"/></ext></extLst></bk><bk><extLst><ext><xlrd:rvb i="1"/></ext></extLst></bk><bk><extLst><ext><xlrd:rvb i="2"/></ext></extLst></bk></futureMetadata><valueMetadata><bk><rc t="1" v="0"/></bk><bk><rc t="1" v="1"/></bk><bk><rc t="1" v="2"/></bk></valueMetadata></metadata>"#;
        let rvs = r#"<rvStructures xmlns="http://schemas.microsoft.com/office/spreadsheetml/2017/richdata"><s t="_webimage"><k n="WebImageIdentifier" t="i"/><k n="CalcOrigin" t="i"/><k n="ComputedImage" t="b"/><k n="ImageSizing" t="i"/><k n="Text" t="s"/></s></rvStructures>"#;
        let rv = r#"<rvData xmlns="http://schemas.microsoft.com/office/spreadsheetml/2017/richdata"><rv s="0"><v>0</v><v>5</v><v>1</v><v>1</v><v>A red square</v></rv><rv s="0"><v>1</v><v>5</v><v>1</v><v>1</v><v></v></rv><rv s="0"><v>2</v><v>5</v><v>1</v><v>1</v><v></v></rv></rvData>"#;
        let targets = HashMap::from([(
            "_webimage",
            ids.into_iter()
                .map(|i| i.map(|i| format!("xl/media/{i}.png")))
                .collect(),
        )]);
        let got = parse_rich_values(meta, rv, rvs, &targets);
        assert_eq!(got.len(), 2, "{got:?}");
        assert_eq!(
            got[&1],
            (
                "xl/media/rIdI1.png".to_string(),
                Some("A red square".to_string())
            )
        );
        assert!(
            !got.contains_key(&2),
            "the blip-less web image stays #VALUE!"
        );
        assert_eq!(got[&3], ("xl/media/rIdI3.png".to_string(), None));
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

/// docling#5271 (#1169): the `IMAGE()` fixtures end to end, and an image
/// that cannot be read losing its own data only
/// (`test_oversized_image_keeps_the_other_images`,
/// `test_truncated_image_keeps_the_other_images`).
#[cfg(test)]
mod image_function_tests {
    use crate::backend::DeclarativeBackend;
    use crate::format::InputFormat;
    use crate::source::SourceDocument;
    use std::io::{Read, Write};

    fn fixture(name: &str) -> Vec<u8> {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../tests/data/xlsx/sources")
            .join(name);
        std::fs::read(path).expect(name)
    }

    /// The fixture with `member` replaced by `bytes`.
    fn replaced(name: &str, member: &str, bytes: &[u8]) -> Vec<u8> {
        let mut zin = zip::ZipArchive::new(std::io::Cursor::new(fixture(name))).unwrap();
        let mut zw = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        for i in 0..zin.len() {
            let mut f = zin.by_index(i).unwrap();
            let entry = f.name().unwrap().to_string();
            let mut data = Vec::new();
            f.read_to_end(&mut data).unwrap();
            if entry == member {
                data = bytes.to_vec();
            }
            zw.start_file(entry, zip::write::SimpleFileOptions::default())
                .unwrap();
            zw.write_all(&data).unwrap();
        }
        zw.finish().unwrap().into_inner()
    }

    /// Each picture's (description, has image data), in document order, and
    /// the Markdown.
    fn pictures(bytes: Vec<u8>) -> (Vec<(Option<String>, bool)>, String) {
        let src = SourceDocument::from_bytes("t.xlsx", InputFormat::Xlsx, bytes);
        let doc = crate::backend::XlsxBackend::default()
            .convert(&src)
            .expect("converts");
        let v: serde_json::Value = serde_json::from_str(&doc.export_to_json()).unwrap();
        let pics = v["pictures"]
            .as_array()
            .unwrap()
            .iter()
            .map(|p| {
                (
                    p["meta"]["description"]["text"]
                        .as_str()
                        .map(str::to_string),
                    p["image"].is_object(),
                )
            })
            .collect();
        (pics, doc.export_to_markdown())
    }

    #[test]
    fn images_from_the_image_function() {
        let (pics, md) = pictures(fixture("xlsx_11_image_function.xlsx"));
        assert_eq!(
            pics,
            [(Some("A red square".to_string()), true), (None, true)]
        );
        let row = |name: &str| {
            md.lines()
                .find(|l| l.starts_with(&format!("| {name} ")))
                .unwrap_or_default()
                .to_string()
        };
        assert!(row("red").contains("<!-- image -->  A red square"), "{md}");
        assert!(row("lost").contains("#VALUE!"), "{md}");
    }

    #[test]
    fn image_function_next_to_an_image_placed_in_a_cell() {
        let (pics, md) = pictures(fixture("xlsx_12_image_function_and_place_in_cell.xlsx"));
        assert_eq!(
            pics,
            [
                (Some("A yellow square".to_string()), true),
                (Some("A blue square".to_string()), true)
            ]
        );
        assert!(
            md.lines().any(
                |l| l.starts_with("| function ") && l.contains("<!-- image -->  A blue square")
            ),
            "{md}"
        );
    }

    #[test]
    fn an_unreadable_image_keeps_the_other_images() {
        let name = "xlsx_12_image_function_and_place_in_cell.xlsx";
        let original = {
            let mut zin = zip::ZipArchive::new(std::io::Cursor::new(fixture(name))).unwrap();
            let mut data = Vec::new();
            zin.by_name("xl/media/image1.png")
                .unwrap()
                .read_to_end(&mut data)
                .unwrap();
            data
        };
        // A PNG cut short, and one whose header declares 100000 × 100000.
        let truncated = original[..original.len() / 2].to_vec();
        let mut huge = original.clone();
        huge[16..20].copy_from_slice(&100_000u32.to_be_bytes());
        huge[20..24].copy_from_slice(&100_000u32.to_be_bytes());
        for broken in [truncated, huge] {
            for member in ["xl/media/image1.png", "xl/media/image2.png"] {
                let (pics, _) = pictures(replaced(name, member, &broken));
                assert_eq!(pics.len(), 2, "{member}: both cells stay pictures");
                let with_data = pics.iter().filter(|(_, data)| *data).count();
                assert_eq!(
                    with_data, 1,
                    "{member}: only the broken image loses its data"
                );
            }
        }
    }
}
