//! DeepSeek-OCR annotated-Markdown backend.
//!
//! The DeepSeek-OCR vision model emits Markdown where every block is preceded by
//! an annotation token carrying a layout label and a detection bounding box:
//!
//! ```text
//! <|ref|>sub_title<|/ref|><|det|>[[217, 209, 520, 225]]<|/det|>
//! ### 5.1 Hyper Parameter Optimization
//! ```
//!
//! This backend ports docling's `parse_deepseekocr_markdown`: it splits the text
//! on the annotation tokens, drops any content before the first one, and turns
//! each labelled block into a document node (the bounding boxes are discarded —
//! they only feed the page-image provenance we don't model here).

use fleischwolf_core::{DoclingDocument, Node};
use regex::Regex;

use crate::backend::html::append_fragment;
use crate::backend::markdown::escape_text;
use crate::backend::DeclarativeBackend;
use crate::error::ConversionError;
use crate::source::SourceDocument;

/// `<|ref|>label<|/ref|><|det|>[[x1, y1, x2, y2]]<|/det|>` (tokens optional, so
/// the bare `label[[…]]` form also matches). Mirrors docling's `annotation_pattern`.
fn annotation_re() -> &'static Regex {
    cached_regex!(
        r"^(?:<\|ref\|>)?(\w+)(?:<\|/ref\|>)?(?:<\|det\|>)?\[\[([0-9., ]+)\]\](?:<\|/det\|>)?\s*$"
    )
}

/// True when the Markdown carries DeepSeek-OCR annotation tokens.
pub fn is_deepseek_markdown(text: &str) -> bool {
    text.lines().any(|l| annotation_re().is_match(l.trim()))
}

pub struct DeepSeekBackend;

impl DeclarativeBackend for DeepSeekBackend {
    fn convert(&self, source: &SourceDocument) -> Result<DoclingDocument, ConversionError> {
        let text = source.text()?;
        let mut doc = DoclingDocument::new(&source.name);

        let lines: Vec<&str> = text.split('\n').collect();
        let annotations = collect_annotations(&lines);

        for (idx, ann) in annotations.iter().enumerate() {
            // A caption that directly follows its table/figure/image was already
            // consumed by that element below — skip the standalone copy.
            if is_caption_label(&ann.label) && idx > 0 {
                let prev = &annotations[idx - 1].label;
                if caption_matches(prev, &ann.label) {
                    continue;
                }
            }

            // Pull a trailing caption for tables/figures/images.
            let caption = if matches!(ann.label.as_str(), "table" | "figure" | "image") {
                annotations.get(idx + 1).and_then(|next| {
                    caption_matches(&ann.label, &next.label).then(|| escape_text(&next.content))
                })
            } else {
                None
            };

            emit(&ann.label, &ann.content, caption, &mut doc);
        }

        Ok(doc)
    }
}

struct Annotation {
    label: String,
    content: String,
}

fn collect_annotations(lines: &[&str]) -> Vec<Annotation> {
    let mut annotations = Vec::new();
    let mut visited = vec![false; lines.len()];
    let mut i = 0;
    while i < lines.len() {
        if visited[i] {
            i += 1;
            continue;
        }
        let line = lines[i].trim();
        if let Some(caps) = annotation_re().captures(line) {
            let label = caps.get(1).unwrap().as_str().to_string();
            i += 1;
            let content = collect_content(lines, &mut i, &label, &mut visited);
            annotations.push(Annotation { label, content });
            continue;
        }
        i += 1;
    }
    annotations
}

/// Collect one annotation's content. Tables grab their `<table>…</table>` block;
/// figures/images grab consecutive non-empty lines; everything else takes the
/// single next non-empty line. Mirrors docling's `_collect_annotation_content`.
fn collect_content(lines: &[&str], i: &mut usize, label: &str, visited: &mut [bool]) -> String {
    let mut content = Vec::new();

    if label == "table" {
        let mut started = false;
        let mut ii = *i;
        while ii < lines.len() {
            let lower = lines[ii].to_lowercase();
            if lower.contains("<table") {
                started = true;
            }
            if started {
                visited[ii] = true;
                content.push(lines[ii].trim_end().to_string());
            }
            if started && lower.contains("</table>") {
                break;
            }
            ii += 1;
        }
        return content.join("\n");
    }

    let multiline = matches!(label, "figure" | "image");
    while *i < lines.len() {
        let trimmed = lines[*i].trim();
        if !trimmed.is_empty() {
            if annotation_re().is_match(trimmed) {
                break;
            }
            visited[*i] = true;
            content.push(lines[*i].trim_end().to_string());
            *i += 1;
            if !multiline {
                break;
            }
        } else {
            *i += 1;
            if !content.is_empty() {
                break;
            }
        }
    }
    content.join("\n")
}

fn emit(label: &str, content: &str, caption: Option<String>, doc: &mut DoclingDocument) {
    match label {
        "figure" | "image" => doc.push(Node::Picture {
            caption,
            image: None,
        }),
        "table" => {
            if let Some(cap) = caption {
                doc.push(Node::Paragraph { text: cap });
            }
            append_fragment(content, &mut doc.nodes, &crate::backend::images::NoFetch);
        }
        "title" => doc.push(Node::Heading {
            level: 1,
            text: escape_text(strip_hashes(content).0),
        }),
        "sub_title" => {
            let (text, hashes) = strip_hashes(content);
            // docling: heading_level = hashes-1 (if >1) else 1; the serializer
            // then renders `#` * (level + 1). So our level = that + 1.
            let level = if hashes > 1 { hashes } else { 2 };
            doc.push(Node::Heading {
                level: level as u8,
                text: escape_text(text),
            });
        }
        // text, header, footer, captions reaching here, …
        _ => doc.push(Node::Paragraph {
            text: escape_text(content),
        }),
    }
}

/// Strip a leading run of `#`s (a Markdown heading marker), returning the
/// remaining text and how many `#`s were removed.
fn strip_hashes(content: &str) -> (&str, usize) {
    if !content.starts_with('#') {
        return (content, 0);
    }
    let hashes = content.chars().take_while(|c| *c == '#').count();
    (content[hashes..].trim_start(), hashes)
}

fn is_caption_label(label: &str) -> bool {
    matches!(label, "table_caption" | "figure_caption" | "image_caption")
}

/// Whether `caption` is the caption kind for element `elem`.
fn caption_matches(elem: &str, caption: &str) -> bool {
    matches!(
        (elem, caption),
        ("table", "table_caption") | ("figure", "figure_caption") | ("image", "image_caption")
    )
}
