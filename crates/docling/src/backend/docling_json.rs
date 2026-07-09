//! docling JSON backend — reads docling's native `DoclingDocument` JSON
//! serialization and re-exports it. docling just pydantic-loads the model; here
//! we walk the `body` tree (children resolved through `$ref` into the
//! `texts`/`groups`/`tables`/`pictures` arrays, skipping `furniture`) and map
//! each item onto the crate's [`Node`] model so the shared Markdown serializer
//! reproduces docling-core's output.

use serde_json::Value;

use crate::backend::markdown::escape_text;
use crate::backend::DeclarativeBackend;
use crate::error::ConversionError;
use crate::source::SourceDocument;
use docling_core::{DoclingDocument, Node, Table};

pub struct DoclingJsonBackend;

impl DeclarativeBackend for DoclingJsonBackend {
    fn convert(&self, source: &SourceDocument) -> Result<DoclingDocument, ConversionError> {
        let root: Value = serde_json::from_str(source.text()?)
            .map_err(|e| ConversionError::Parse(format!("docling-json: {e}")))?;
        let name = root["name"].as_str().unwrap_or(&source.name).to_string();
        let mut doc = DoclingDocument::new(name);
        if let Some(children) = root["body"]["children"].as_array() {
            for c in children {
                walk(c, &root, 0, &mut doc);
            }
        }
        Ok(doc)
    }
}

/// Resolve a `{"$ref": "#/texts/3"}` reference into its array element.
fn resolve<'a>(reference: &Value, root: &'a Value) -> Option<&'a Value> {
    let path = reference["$ref"].as_str()?.strip_prefix("#/")?;
    let (kind, idx) = path.rsplit_once('/')?;
    root.get(kind)?.get(idx.parse::<usize>().ok()?)
}

fn ref_kind(reference: &Value) -> &str {
    reference["$ref"].as_str().unwrap_or("")
}

fn text_of(reference: &Value, root: &Value) -> String {
    resolve(reference, root)
        .map(formatted_text)
        .unwrap_or_default()
}

/// Escaped item text with docling-core's inline markers applied in order:
/// bold → italic → strikethrough → hyperlink (underline/script are no-ops in
/// Markdown).
fn formatted_text(item: &Value) -> String {
    let mut res = escape_text(item["text"].as_str().unwrap_or(""));
    let fmt = &item["formatting"];
    if fmt["bold"].as_bool() == Some(true) {
        res = format!("**{res}**");
    }
    if fmt["italic"].as_bool() == Some(true) {
        res = format!("*{res}*");
    }
    if fmt["strikethrough"].as_bool() == Some(true) {
        res = format!("~~{res}~~");
    }
    if let Some(url) = item["hyperlink"].as_str() {
        res = format!("[{res}]({url})");
    }
    res
}

/// Dispatch one body/child reference by the array it points into.
fn walk(reference: &Value, root: &Value, level: u8, doc: &mut DoclingDocument) {
    let Some(item) = resolve(reference, root) else {
        return;
    };
    if item["content_layer"].as_str() == Some("furniture") {
        return;
    }
    let kind = ref_kind(reference);
    if kind.starts_with("#/texts/") {
        text_item(item, level, doc);
    } else if kind.starts_with("#/groups/") {
        group_item(item, root, level, doc);
    } else if kind.starts_with("#/tables/") {
        table_item(item, root, doc);
    } else if kind.starts_with("#/pictures/") {
        picture_item(item, root, doc);
    }
}

fn text_item(item: &Value, level: u8, doc: &mut DoclingDocument) {
    let label = item["label"].as_str().unwrap_or("text");
    // docling does not serialize empty text items (an undecoded formula is the
    // one exception — it becomes a placeholder comment).
    if item["text"].as_str().unwrap_or("").is_empty() {
        if label == "formula" {
            doc.push(Node::Paragraph {
                text: "<!-- formula-not-decoded -->".into(),
            });
        }
        return;
    }
    let text = formatted_text(item);
    match label {
        "title" => doc.push(Node::Heading { level: 1, text }),
        "section_header" => {
            let lvl = item["level"].as_u64().unwrap_or(1) as u8;
            doc.push(Node::Heading {
                level: lvl + 1,
                text,
            });
        }
        "code" => doc.push(Node::Code {
            language: item["code_language"]
                .as_str()
                .filter(|s| !s.is_empty() && *s != "unknown")
                .map(String::from),
            text,
        }),
        "list_item" => doc.push(Node::ListItem {
            ordered: item["enumerated"].as_bool().unwrap_or(false),
            number: 1,
            first_in_list: true,
            text,
            level,
            marker: None,
        }),
        "caption" => {} // rendered with its parent table/picture
        _ => doc.push(Node::Paragraph { text }), // text, paragraph, formula, footnote, …
    }
}

fn group_item(item: &Value, root: &Value, level: u8, doc: &mut DoclingDocument) {
    let label = item["label"].as_str().unwrap_or("unspecified");
    let empty = Vec::new();
    let children = item["children"].as_array().unwrap_or(&empty);
    match label {
        "list" | "ordered_list" => list_group(children, root, level, doc),
        "inline" => {
            // An inline group is one line: serialize each child and join with " ".
            let joined = children
                .iter()
                .map(|c| text_of(c, root))
                .collect::<Vec<_>>()
                .join(" ");
            if !joined.is_empty() {
                doc.push(Node::Paragraph { text: joined });
            }
        }
        // section / chapter / unspecified / sheet / comment_section → transparent
        _ => {
            for c in children {
                walk(c, root, level, doc);
            }
        }
    }
}

/// Emit a list group's items, recursing into nested lists at the next level.
fn list_group(children: &[Value], root: &Value, level: u8, doc: &mut DoclingDocument) {
    let mut number = 0u64;
    let mut first = true;
    for c in children {
        let kind = ref_kind(c);
        if kind.starts_with("#/groups/") {
            // A bare nested list (no enclosing item).
            walk(c, root, level + 1, doc);
            continue;
        }
        let Some(item) = resolve(c, root) else {
            continue;
        };
        if item["label"].as_str() != Some("list_item") {
            continue;
        }
        number += 1;
        doc.push(Node::ListItem {
            ordered: item["enumerated"].as_bool().unwrap_or(false),
            number,
            first_in_list: first,
            text: formatted_text(item),
            level,
            marker: None,
        });
        first = false;
        if let Some(sub) = item["children"].as_array() {
            for s in sub {
                walk(s, root, level + 1, doc);
            }
        }
    }
}

fn table_item(item: &Value, root: &Value, doc: &mut DoclingDocument) {
    let mut rows = Vec::new();
    if let Some(grid) = item["data"]["grid"].as_array() {
        for row in grid {
            if let Some(cells) = row.as_array() {
                rows.push(
                    cells
                        .iter()
                        .map(|cell| cell["text"].as_str().unwrap_or("").to_string())
                        .collect::<Vec<_>>(),
                );
            }
        }
    }
    if !rows.is_empty() {
        doc.push(Node::Table(Table {
            rows,
            location: None,
            structure: None,
        }));
    }
    push_captions(item, root, doc);
}

fn picture_item(item: &Value, root: &Value, doc: &mut DoclingDocument) {
    // docling renders the image marker first, then each caption as a paragraph.
    doc.push(Node::Picture {
        caption: None,
        image: None,
    });
    push_captions(item, root, doc);
}

/// Emit an item's `captions` (refs into `texts`) as paragraphs, after the element.
fn push_captions(item: &Value, root: &Value, doc: &mut DoclingDocument) {
    if let Some(caps) = item["captions"].as_array() {
        for c in caps {
            let t = text_of(c, root);
            if !t.is_empty() {
                doc.push(Node::Paragraph { text: t });
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::format::InputFormat;

    #[test]
    fn walks_body_tree_with_formatting_and_lists() {
        let json = r##"{
          "schema_name": "DoclingDocument", "name": "t",
          "body": {"children": [{"$ref":"#/texts/0"},{"$ref":"#/texts/1"},{"$ref":"#/groups/0"}]},
          "texts": [
            {"self_ref":"#/texts/0","label":"title","text":"Doc"},
            {"self_ref":"#/texts/1","label":"section_header","level":1,"text":"Sec","hyperlink":"http://x"},
            {"self_ref":"#/texts/2","label":"list_item","text":"one","enumerated":false},
            {"self_ref":"#/texts/3","label":"list_item","text":"two","enumerated":false,
             "formatting":{"bold":true,"italic":false,"strikethrough":false}}
          ],
          "groups": [{"self_ref":"#/groups/0","label":"list",
                      "children":[{"$ref":"#/texts/2"},{"$ref":"#/texts/3"}]}],
          "tables": [], "pictures": []
        }"##;
        let src =
            SourceDocument::from_bytes("t", InputFormat::JsonDocling, json.as_bytes().to_vec());
        let md = DoclingJsonBackend
            .convert(&src)
            .unwrap()
            .export_to_markdown();
        assert!(
            md.starts_with("# Doc\n\n## [Sec](http://x)\n\n- one\n- **two**"),
            "got:\n{md}"
        );
    }
}
