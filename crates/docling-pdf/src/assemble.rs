//! Layout-driven assembly: map detected [`Region`]s + text cells to a
//! [`DoclingDocument`], mirroring docling's page-assembly + reading-order.
//!
//! Overlapping detections are resolved greedily by score, each text cell is
//! assigned to its best-containing region, regions are ordered in reading order
//! (two-column aware), and each becomes a typed node by its layout label.

use docling_core::{Node, PictureImage, Table};

use crate::layout::Region;
use crate::pdfium_backend::{PdfPage, TextCell};

fn area(l: f32, t: f32, r: f32, b: f32) -> f32 {
    ((r - l).max(0.0)) * ((b - t).max(0.0))
}

/// Intersection area of two boxes.
fn inter(a: &Region, l: f32, t: f32, r: f32, b: f32) -> f32 {
    let il = a.l.max(l);
    let it = a.t.max(t);
    let ir = a.r.min(r);
    let ib = a.b.min(b);
    area(il, it, ir, ib)
}

/// Greedily keep regions by descending score, dropping a region that is mostly
/// covered by an already-kept one (RT-DETR emits overlapping duplicates).
pub fn resolve(mut regions: Vec<Region>) -> Vec<Region> {
    regions.sort_by(|a, b| b.score.total_cmp(&a.score));
    let mut kept: Vec<Region> = Vec::new();
    for r in regions {
        let ra = area(r.l, r.t, r.r, r.b).max(1.0);
        let covered = kept.iter().any(|k| {
            let i = inter(&r, k.l, k.t, k.r, k.b);
            let ka = area(k.l, k.t, k.r, k.b).max(1.0);
            // drop if most of r is inside k, or they strongly mutually overlap
            i / ra > 0.7 || i / (ra + ka - i) > 0.5
        });
        if !covered {
            kept.push(r);
        }
    }
    dedup_nested_code(&mut kept);
    kept
}

/// True for a bare, single-token source-code language label (`XML`, `C#`, `JSON`,
/// `bash`, …) — the little header the docs render above a code block. Matched
/// case-insensitively; anything with whitespace or longer than a token is out.
fn is_code_language(t: &str) -> bool {
    let t = t.trim();
    if t.is_empty() || t.chars().any(char::is_whitespace) || t.chars().count() > 12 {
        return false;
    }
    const LANGS: &[&str] = &[
        "xml",
        "html",
        "xhtml",
        "json",
        "jsonc",
        "yaml",
        "yml",
        "toml",
        "ini",
        "c#",
        "csharp",
        "f#",
        "fsharp",
        "vb",
        "c",
        "c++",
        "cpp",
        "java",
        "kotlin",
        "scala",
        "go",
        "golang",
        "rust",
        "swift",
        "javascript",
        "js",
        "typescript",
        "ts",
        "jsx",
        "tsx",
        "python",
        "py",
        "ruby",
        "rb",
        "php",
        "perl",
        "lua",
        "r",
        "dart",
        "bash",
        "sh",
        "shell",
        "powershell",
        "zsh",
        "batch",
        "cmd",
        "sql",
        "tsql",
        "plsql",
        "graphql",
        "dockerfile",
        "makefile",
        "css",
        "scss",
        "sass",
        "less",
        "markdown",
        "md",
        "tex",
        "latex",
        "diff",
        "proto",
        "razor",
        "cshtml",
        "xaml",
        "aspx",
        "http",
    ];
    let lower = t.to_ascii_lowercase();
    LANGS.contains(&lower.as_str())
}

/// Mark the region indices that are a code block's **language label** — a bare
/// `XML`/`C#`/… token sitting directly above a `code` region — so they are consumed
/// rather than emitted as their own stray paragraph/heading. The label may also be
/// captured inside a wider code box (rendered as the fence's first line); dropping
/// the standalone copy just removes the duplicate.
fn code_language_labels(regions: &[Region], cells: &[TextCell]) -> Vec<bool> {
    let mut drop = vec![false; regions.len()];
    for (i, r) in regions.iter().enumerate() {
        if matches!(r.label, "code" | "picture" | "table") {
            continue;
        }
        if !is_code_language(&region_text(r, cells)) {
            continue;
        }
        // The label sits just above the code (a blank line's gap) or is swallowed
        // into the top of a wider code box; either way it is that block's label.
        // The window is generous because the label's own font is small, so a
        // one-line gap is several times its height.
        let line_h = (r.b - r.t).abs().max(1.0);
        let window = (line_h * 4.0).max(28.0);
        let labels_code = regions.iter().enumerate().any(|(j, c)| {
            if j == i || c.label != "code" {
                return false;
            }
            let gap = c.t - r.b; // >0 when the code is below the label
            let h_overlap = (r.r.min(c.r) - r.l.max(c.l)).max(0.0);
            gap > -line_h * 3.0 && gap < window && h_overlap > 0.0
        });
        if labels_code {
            drop[i] = true;
        }
    }
    drop
}

/// Collapse `code` regions where one is nested inside another, keeping the larger.
///
/// RT-DETR sometimes emits a tight code box *and* a wider near-duplicate that also
/// captures the block's language label (`XML`, `C#`, …). When the tight box scores
/// higher it is kept first, and the wider container — not "mostly inside" the tight
/// box — survives [`resolve`]'s greedy pass, so the block is emitted twice. Keeping
/// the **larger** box (rather than dropping it) collapses the pair without leaking
/// the container's extra cells back out as orphan text, since the larger box still
/// covers every cell. Restricted to `code` so genuinely distinct nested regions of
/// other kinds are untouched.
fn dedup_nested_code(kept: &mut Vec<Region>) {
    let mut drop = vec![false; kept.len()];
    for i in 0..kept.len() {
        if kept[i].label != "code" {
            continue;
        }
        let ai = area(kept[i].l, kept[i].t, kept[i].r, kept[i].b).max(1.0);
        for j in 0..kept.len() {
            if i == j || drop[j] || kept[j].label != "code" {
                continue;
            }
            let aj = area(kept[j].l, kept[j].t, kept[j].r, kept[j].b).max(1.0);
            // Drop i when it is mostly inside a strictly larger code box j.
            let overlap = inter(&kept[i], kept[j].l, kept[j].t, kept[j].r, kept[j].b);
            if aj > ai && overlap / ai > 0.7 {
                drop[i] = true;
                break;
            }
        }
    }
    let mut keep = drop.iter();
    kept.retain(|_| !*keep.next().unwrap());
}

/// Append `text` regions for cells the layout left uncovered ("orphan cells"),
/// the way docling's `LayoutPostprocessor` does (`create_orphan_clusters`): any
/// non-empty cell that no kept region covers (>50% of the cell's area) becomes a
/// text region of its own, so text the detector missed (a stray `.`, a small
/// label) is still emitted instead of silently dropped. Adjacent orphan cells on a
/// line are merged so a missed paragraph doesn't shatter into one block per line.
pub fn add_orphan_regions(regions: &mut Vec<Region>, cells: &[TextCell]) {
    // docling assigns a cell to its best-overlapping cluster at
    // intersection-over-self > 0.2; only cells below that for *every* region are
    // orphans. (Our text extraction uses a stricter 0.5, but matching docling's
    // 0.2 here avoids emitting cells it already placed in a neighbouring region.)
    let assigned = |c: &TextCell| {
        let ca = area(c.l, c.t, c.r, c.b).max(1.0);
        regions
            .iter()
            .any(|r| inter(r, c.l, c.t, c.r, c.b) / ca > 0.2)
    };
    // Collect orphan cells (non-empty, unassigned), in page order.
    let mut orphans: Vec<&TextCell> = cells
        .iter()
        .filter(|c| !c.text.trim().is_empty() && !assigned(c))
        .collect();
    if orphans.is_empty() {
        return;
    }
    orphans.sort_by(|a, b| a.t.total_cmp(&b.t).then(a.l.total_cmp(&b.l)));
    // Merge cells that sit on the same line and nearly touch into one region, so a
    // dropped multi-word line stays one block (docling's refinement merges these).
    let mut merged: Vec<Region> = Vec::new();
    for c in orphans {
        let h = (c.b - c.t).abs().max(1.0);
        if let Some(last) = merged.last_mut() {
            let same_line = (last.t - c.t).abs() < h * 0.5;
            let touching = c.l <= last.r + h && c.l >= last.l - h;
            if same_line && touching {
                last.l = last.l.min(c.l);
                last.r = last.r.max(c.r);
                last.t = last.t.min(c.t);
                last.b = last.b.max(c.b);
                continue;
            }
        }
        merged.push(Region {
            label: "text",
            score: 0.0,
            l: c.l,
            t: c.t,
            r: c.r,
            b: c.b,
        });
    }
    regions.extend(merged);
}

/// Drop a `picture` detection that is a small, empty, low-confidence margin box on
/// a **text page** — a false positive the RT-DETR layout sometimes emits (e.g.
/// `right_to_left_02`'s phantom right-column picture, score 0.40); docling does not
/// emit it. The gate is deliberately narrow so a genuine figure is never dropped:
/// (1) only on pages with a digital text layer — image/scanned/figure pages have
/// no `cells` yet at this point (OCR runs later), so their pictures, which *are*
/// the content, are kept; (2) only a box covering < 25 % of the page (a margin
/// artifact, not a dominant figure); (3) only when it contains no text and scores
/// below 0.5 (real empty figures in the corpus all score ≥ 0.86).
pub fn drop_false_pictures(
    regions: &mut Vec<Region>,
    cells: &[TextCell],
    page_w: f32,
    page_h: f32,
) {
    if cells.iter().all(|c| c.text.trim().is_empty()) {
        return; // no digital text layer (image/scanned page) — keep all pictures
    }
    // A text-document page carries several text-bearing non-picture regions (so a
    // spurious margin picture is clearly extra). A slide / figure page has at most
    // one — there the picture is the content, so never drop it.
    let content_regions = regions
        .iter()
        .filter(|r| r.label != "picture" && !region_text(r, cells).trim().is_empty())
        .count();
    if content_regions < 2 {
        return;
    }
    let page_area = (page_w * page_h).max(1.0);
    regions.retain(|r| {
        if r.label != "picture" || r.score >= 0.5 {
            return true;
        }
        if area(r.l, r.t, r.r, r.b) / page_area >= 0.25 {
            return true; // a dominant figure, not a margin artifact
        }
        // Keep it if any text cell falls mostly inside (a real captioned/labelled
        // figure); drop only the genuinely empty low-confidence boxes.
        cells.iter().any(|c| {
            let ca = area(c.l, c.t, c.r, c.b).max(1.0);
            !c.text.trim().is_empty() && inter(r, c.l, c.t, c.r, c.b) / ca > 0.5
        })
    });
}

/// A small digit-only region in the top/bottom margin: a page number. docling
/// emits `right_to_left_02`'s bottom `11` as the page's *first* text item (its
/// reading-order model floats the page number to the front), whereas our
/// position-based ordering would place a bottom region last.
fn is_page_number(region: &Region, cells: &[TextCell], page_h: f32) -> bool {
    let t = region_text(region, cells);
    let t = t.trim();
    !t.is_empty()
        && t.chars().all(|c| c.is_ascii_digit())
        && (region.b - region.t).abs() < 30.0
        && (region.t < page_h * 0.12 || region.b > page_h * 0.88)
}

/// Furniture / not-yet-emitted labels.
fn is_skipped(label: &str) -> bool {
    matches!(
        label,
        "page_header"
            | "page_footer"
            | "checkbox_selected"
            | "checkbox_unselected"
            | "form"
            | "key_value_region"
            | "document_index"
    )
}

/// Reading-order sort of regions, with two-column detection on the page.
fn order_regions<T>(items: &mut [T], page_w: f32, reg: impl Fn(&T) -> &Region) {
    let cx = page_w / 2.0;
    let band = page_w * 0.08;
    let crossing = items
        .iter()
        .filter(|t| {
            let r = reg(t);
            r.l < cx - band && r.r > cx + band
        })
        .count();
    let two_col = !items.is_empty()
        && (crossing as f32) / (items.len() as f32) < 0.25
        && items.iter().any(|t| reg(t).r <= cx)
        && items.iter().any(|t| reg(t).l >= cx);
    if two_col {
        // Full-width regions (title, figures, wide tables spanning both columns)
        // break the two-column flow into horizontal bands: within a band the left
        // column reads fully then the right, and a full-width region reads after
        // the band above it and before the band below. Band index = number of
        // full-width regions above a region's top; column 1=left, 2=right,
        // 3=full-width (so it sorts after that band's columns).
        // Only a region spanning *most* of the page width is a band break (a
        // title, a full-width figure/table) — a merely wide column region is not.
        let full_band = page_w * 0.2;
        let is_full = |r: &Region| r.l < cx - full_band && r.r > cx + full_band;
        let full_tops: Vec<f32> = items
            .iter()
            .map(&reg)
            .filter(|r| is_full(r))
            .map(|r| r.t)
            .collect();
        let key = |r: &Region| -> (usize, u8) {
            let bnd = full_tops.iter().filter(|&&ft| ft < r.t - 1.0).count();
            let col = if is_full(r) {
                3
            } else if (r.l + r.r) / 2.0 >= cx {
                2
            } else {
                1
            };
            (bnd, col)
        };
        items.sort_by(|a, b| {
            let (a, b) = (reg(a), reg(b));
            key(a)
                .cmp(&key(b))
                .then(a.t.total_cmp(&b.t))
                .then(a.l.total_cmp(&b.l))
        });
    } else {
        items.sort_by(|a, b| {
            let (a, b) = (reg(a), reg(b));
            a.t.total_cmp(&b.t).then(a.l.total_cmp(&b.l))
        });
    }
}

/// Clean a region's assembled text: undo soft-hyphen line wraps, map curly
/// quotes and the ellipsis to ASCII (matching docling), and collapse runs of
/// whitespace. pdfium emits the line-wrap hyphen as U+0002 in this corpus
/// (U+00AD elsewhere), so `word\u{2} continuation` is one hyphenated word —
/// drop the hyphen + the joining space and merge (`com\u{2} pact` → `compact`,
/// `end-to\u{2} end` → `end-toend`), exactly as docling does.
///
/// Token spacing is otherwise left as the geometric join produced it. We do not
/// tighten punctuation spacing: docling preserves the PDF's own spaces (it keeps
/// `{ ahn }`, `Name 1 .`, `[ 9 ]`), and a geometric gap heuristic diverges from
/// it more than a plain single-space join does.
/// An ordered-list enumeration marker at the start of a list item: leading ASCII
/// digits followed by `.`, e.g. `1. Undo/Redo` → `(1, "Undo/Redo")`. Returns
/// `None` when the text doesn't start with `digits.`.
fn parse_ordered_marker(s: &str) -> Option<(u64, String)> {
    let digits: String = s.chars().take_while(|c| c.is_ascii_digit()).collect();
    if digits.is_empty() {
        return None;
    }
    let rest = s[digits.len()..].strip_prefix('.')?;
    let number = digits.parse().ok()?;
    Some((number, rest.trim_start().to_string()))
}

/// Escape markdown special characters the way docling-core's markdown serializer
/// does (`markdown.py` post_process): `_` → `\_`, then HTML-escape `&`, `<`, `>`
/// (quote=False, so quotes are left). Applied to prose (headings, list items,
/// paragraphs); code blocks, the formula placeholder, and table cells are left raw.
fn md_escape(text: &str) -> String {
    text.replace('_', "\\_")
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

fn clean_text(text: &str) -> String {
    // Korean (Hangul) bodies use single straight quotes where the font's double
    // curly glyph maps; docling renders `“ ”` as `'` for these fonts (normal_4pages
    // `‘코로나’`), not the Latin `"`. Key on Hangul syllables so Latin docs (2305's
    // genuine `quotedbl` → `"`) are unaffected.
    let hangul = text.chars().any(|c| ('\u{AC00}'..='\u{D7A3}').contains(&c));
    let dquote = if hangul { "'" } else { "\"" };
    let replaced = text
        .replace("\u{2} ", "")
        .replace("\u{ad} ", "")
        .replace(['\u{2}', '\u{ad}'], "") // any stray wrap hyphens not at a join
        .replace(['\u{2018}', '\u{2019}'], "'") // ‘ ’ → '
        .replace(['\u{201c}', '\u{201d}'], dquote) // “ ” → " (or ' for Hangul)
        .replace(['\u{2013}', '\u{2014}', '\u{2212}'], "-") // – — − → -
        .replace('\u{2044}', "/") // ⁄ fraction slash → /
        .replace('\u{2026}', "..."); // … → ...
    let out = if crate::pdfium_backend::use_dp_lines() {
        // The docling-parse sanitizer already placed the correct spacing (e.g.
        // justified double spaces); preserve internal runs of spaces, only
        // normalizing line breaks/tabs and trimming the ends.
        replaced.replace(['\n', '\r', '\t'], " ").trim().to_string()
    } else {
        // Legacy: collapse all whitespace runs to single spaces.
        replaced.split_whitespace().collect::<Vec<_>>().join(" ")
    };
    fix_arabic_lam_alef(&out)
}

/// pdfium decomposes the Arabic lam-alef ligature (لا / لإ / لأ / لآ) into its
/// glyph constituents in *visual* order — `alef-variant, lam` — but docling keeps
/// logical order, `lam, alef-variant`. Swap a mid-word `alef-variant + lam` back
/// to `lam + alef-variant`. "Mid-word" (the previous char is an Arabic letter)
/// distinguishes the ligature from the definite article `ال` (word-initial
/// `alef + lam`), which must stay. No-op for non-Arabic text.
fn fix_arabic_lam_alef(s: &str) -> String {
    let is_arabic_letter = |c: char| ('\u{0620}'..='\u{064A}').contains(&c);
    let chars: Vec<char> = s.chars().collect();
    if !chars.iter().any(|&c| is_arabic_letter(c)) {
        return s.to_string(); // no-op for non-Arabic text
    }
    // Pass 1: swap mid-word `alef-variant + lam` → `lam + alef-variant`. Only the
    // hamza/madda alef variants (إ أ آ) are safe: the definite article is always
    // plain `ا + ل`, so plain `alef + lam` is ambiguous (a legitimate `فعالة` vs a
    // reversed `لا` ligature look identical) — leaving plain alef alone avoids
    // corrupting legitimate words.
    let mut a: Vec<char> = Vec::with_capacity(chars.len());
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if matches!(c, '\u{0622}' | '\u{0623}' | '\u{0625}')
            && chars.get(i + 1) == Some(&'\u{0644}')
            && i > 0
            && is_arabic_letter(chars[i - 1])
            // A preceding lam means this alef-variant is *already* the logical
            // `lam + alef` ligature; the following lam is the next syllable's
            // letter, not a reversed ligature — swapping it corrupts `لآل` → `للآ`
            // (e.g. التعلم الآلي → الآلي, not اللآي).
            && chars[i - 1] != '\u{0644}'
        {
            a.push('\u{0644}');
            a.push(c);
            i += 2;
            continue;
        }
        a.push(c);
        i += 1;
    }
    // Pass 2: insert a space at Arabic↔Latin boundaries (bidi script switch) that
    // pdfium runs together — docling separates the embedded Latin run (`وPython`
    // → `و Python`).
    let mut out: Vec<char> = Vec::with_capacity(a.len());
    for (j, &c) in a.iter().enumerate() {
        if j > 0 {
            let p = a[j - 1];
            if (is_arabic_letter(p) && c.is_ascii_alphabetic())
                || (p.is_ascii_alphabetic() && is_arabic_letter(c))
            {
                out.push(' ');
            }
        }
        out.push(c);
    }
    out.into_iter().collect()
}

/// Resolve each page hyperlink to the visible text it covers, as `(anchor, uri)`
/// in reading order. The anchor is the cells whose centre falls in the link rect,
/// joined left-to-right and cleaned the same way prose is (so it matches the
/// serialized text), deduped against the immediately-preceding link so pdfium's
/// occasional duplicate annotation doesn't double-list. Empty anchors are dropped.
pub(crate) fn resolve_link_anchors(page: &PdfPage) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = Vec::new();
    // Use per-word cells, not the line-merged `cells`: a link rect covers a few
    // words on a line, and a whole merged line cell would over-capture (its centre
    // lands in one link's rect, grabbing the entire line as that link's anchor).
    let words = if page.word_cells.is_empty() {
        &page.cells
    } else {
        &page.word_cells
    };
    for link in &page.links {
        // A cell participates when its centre row is inside the rect and it
        // overlaps the rect horizontally. A cell can be *wider* than the rect:
        // PDFs often draw a whole header line as one text run ("LinkedIn |
        // GitHub | Credly"), which docling-parse's word grouping keeps as one
        // cell even though each label carries its own link annotation —
        // centre-in-rect alone would hand the entire line to every link.
        // [`cell_text_in_rect`] clips such a cell to the tokens under the rect.
        let mut inside: Vec<(&TextCell, String)> = words
            .iter()
            .filter(|c| {
                let cy = (c.t + c.b) / 2.0;
                cy >= link.t && cy <= link.b && c.r.min(link.r) > c.l.max(link.l)
            })
            .filter_map(|c| {
                let text = cell_text_in_rect(c, link.l, link.r);
                (!text.is_empty()).then_some((c, text))
            })
            .collect();
        // Reading order: top band then left-to-right (link anchors are LTR).
        let band = inside
            .iter()
            .map(|(c, _)| (c.b - c.t).abs())
            .fold(0.0f32, f32::max)
            .max(1.0);
        inside.sort_by_key(|(c, _)| ((c.t / band).round() as i64, (c.l * 10.0) as i64));
        let anchor = clean_text(
            &inside
                .iter()
                .map(|(_, t)| t.trim())
                .filter(|t| !t.is_empty())
                .collect::<Vec<_>>()
                .join(" "),
        );
        if anchor.is_empty() {
            continue;
        }
        if out
            .last()
            .is_some_and(|(a, u)| a == &anchor && u == &link.uri)
        {
            continue;
        }
        out.push((anchor, link.uri.clone()));
    }
    out
}

/// The part of a cell's text that lies under a link rect's x-range. A cell
/// fully inside the rect (by centre) returns its whole text. A wider cell is
/// split into whitespace tokens whose x-spans are estimated proportionally to
/// their character positions (kerning makes this approximate, so selection
/// snaps to whole tokens, never characters); tokens whose estimated centre
/// falls inside the rect are kept. Returns "" when nothing falls inside.
fn cell_text_in_rect(c: &TextCell, l: f32, r: f32) -> String {
    let cx = (c.l + c.r) / 2.0;
    if cx >= l && cx <= r && c.l >= l - (c.r - c.l) * 0.25 && c.r <= r + (c.r - c.l) * 0.25 {
        return c.text.trim().to_string();
    }
    let chars: Vec<char> = c.text.chars().collect();
    let n = chars.len();
    if n == 0 || c.r <= c.l {
        return String::new();
    }
    let per = (c.r - c.l) / n as f32;
    let mut out: Vec<String> = Vec::new();
    let mut token = String::new();
    let mut start = 0usize;
    // A trailing sentinel space flushes the last token.
    for (i, &ch) in chars.iter().enumerate().chain(std::iter::once((n, &' '))) {
        if ch.is_whitespace() {
            if !token.is_empty() {
                let mid = c.l + (start as f32 + (i - start) as f32 / 2.0) * per;
                if mid >= l && mid <= r {
                    out.push(std::mem::take(&mut token));
                } else {
                    token.clear();
                }
            }
        } else {
            if token.is_empty() {
                start = i;
            }
            token.push(ch);
        }
    }
    out.join(" ")
}

/// Cells assigned to a region (best container), in reading order, joined.
fn region_text(region: &Region, cells: &[TextCell]) -> String {
    let mut inside: Vec<&TextCell> = cells
        .iter()
        .filter(|c| {
            let ca = area(c.l, c.t, c.r, c.b).max(1.0);
            inter(region, c.l, c.t, c.r, c.b) / ca > 0.5
        })
        .collect();
    // Quantize the top coordinate into ~line bands so cells on the same line
    // sort in reading order; this is a strict total order (a raw fuzzy comparator
    // is not transitive and makes Rust's sort panic). For a right-to-left
    // (Arabic-majority) region, cells on a line read right→left, so sort the band
    // by descending left edge.
    let band = inside
        .iter()
        .map(|c| (c.b - c.t).abs())
        .fold(0.0f32, f32::max)
        .max(1.0);
    let arabic = inside
        .iter()
        .flat_map(|c| c.text.chars())
        .filter(|&c| ('\u{0600}'..='\u{06FF}').contains(&c))
        .count();
    let latin = inside
        .iter()
        .flat_map(|c| c.text.chars())
        .filter(|c| c.is_ascii_alphabetic())
        .count();
    let rtl = arabic > latin;
    inside.sort_by_key(|c| {
        let x = (c.l * 10.0) as i64;
        ((c.t / band).round() as i64, if rtl { -x } else { x })
    });
    // Join cells in reading order. With the docling-parse sanitizer the cells are
    // already correctly spaced words/lines, so adjacent cells join with a single
    // space (docling joins its line cells with a space) — matching e.g. a bold
    // label and its value, `LABEL` | `: value` → `LABEL : value`. The legacy
    // reconstruction instead joins same-band cells with a space only across a real
    // gap, because it can split a word into abutting segments (`الت`|`ي` → `التي`).
    let dp = crate::pdfium_backend::use_dp_lines();
    let mut joined = String::new();
    let mut prev: Option<&&TextCell> = None;
    for c in &inside {
        let t = c.text.trim();
        // Skip whitespace-only cells (e.g. a justified line's trailing space glyph
        // at a wrap): the join already inserts a separator, so an empty cell would
        // double it (`all-metal  construction`).
        if t.is_empty() {
            continue;
        }
        if let Some(p) = prev {
            let same_band = ((p.t / band).round() as i64) == ((c.t / band).round() as i64);
            let h = (c.b - c.t).abs().max((p.b - p.t).abs()).max(1.0);
            let gap = if rtl { p.l - c.r } else { c.l - p.r };
            // Dehyphenate a wrapped word: a line ending in a hyphen/dash followed
            // by a continuation joins without the dash or a space (`platforms—` +
            // `reflects` → `platformsreflects`). The dash is still raw here
            // (clean_text normalizes em/en dashes later), so match them all.
            let ends_dash = matches!(
                joined.chars().last(),
                Some('-' | '\u{2010}' | '\u{2013}' | '\u{2014}')
            );
            let before = joined.chars().nth_back(1); // char before the dash
            let next = t.chars().next();
            let dehyph = dp
                && ends_dash
                && before.is_some_and(|c| c.is_alphabetic())
                && next.is_some_and(|n| {
                    // Ordinary hyphenation (lowercase continuation), or a CamelCase
                    // compound name wrapped at the hyphen — a lowercase letter before
                    // the dash continuing with an uppercase one (`PubTab-Net` →
                    // `PubTabNet`, `Table-Former` → `TableFormer`). Excludes runs like
                    // `MS-COCO` (uppercase before the dash) and `PubTables-1M` (digit).
                    n.is_lowercase()
                        || (n.is_uppercase() && before.is_some_and(|b| b.is_lowercase()))
                });
            if dehyph {
                joined.pop();
            } else if dp || !same_band || gap > h * 0.25 {
                joined.push(' ');
            }
        }
        joined.push_str(t);
        prev = Some(c);
    }
    clean_text(&joined)
}

/// Tighten the spaces pdfium leaves around tight punctuation in a code line
/// (`console .log` → `console.log`, `add (3 , 5)` → `add(3, 5)`), matching
/// docling-parse's source spacing.
fn tighten_code_punct(s: &str) -> String {
    s.replace(" .", ".")
        .replace(" ,", ",")
        .replace(" ;", ";")
        .replace(" )", ")")
        .replace(" (", "(")
}

/// Assemble a **code** region's text with its line structure preserved.
///
/// Unlike [`region_text`] — which joins every cell with a single space, the right
/// thing for prose reflow — a code block's line breaks and indentation are
/// significant. The `code_cells` are already one physical source line each
/// (grouped space-glyph-only, so monospace runs keep their spacing), so this:
///
/// 1. groups the cells into vertical line bands and orders them top→bottom,
///    left→right;
/// 2. joins the lines with `\n` (rather than spaces), keeping the carriage
///    returns; and
/// 3. reconstructs each line's leading indentation from its left offset, in units
///    of the block's estimated monospace character width, so nesting survives.
///
/// Typography is normalized per line via [`clean_text`] (smart quotes, dashes,
/// ellipsis), which never merges lines. Returns an empty string if the region has
/// no code cells (the caller falls back to the prose text).
fn code_region_text(region: &Region, cells: &[TextCell]) -> String {
    let mut inside: Vec<&TextCell> = cells
        .iter()
        .filter(|c| {
            let ca = area(c.l, c.t, c.r, c.b).max(1.0);
            inter(region, c.l, c.t, c.r, c.b) / ca > 0.5
        })
        .filter(|c| !c.text.trim().is_empty())
        .collect();
    if inside.is_empty() {
        return String::new();
    }

    // Quantize the top edge into ~line bands (like `region_text`), then order the
    // cells by band (top→bottom) and, within a band, by left edge.
    let band = inside
        .iter()
        .map(|c| (c.b - c.t).abs())
        .fold(0.0f32, f32::max)
        .max(1.0);
    let line_of = |c: &TextCell| (c.t / band).round() as i64;
    inside.sort_by_key(|c| (line_of(c), (c.l * 10.0) as i64));

    // Estimate one monospace character's width (total ink width / total glyphs) to
    // convert a line's left offset into a count of leading spaces. Measured over
    // all lines so a single short line can't skew it.
    let (mut total_w, mut total_chars) = (0.0f32, 0usize);
    for c in &inside {
        let n = c.text.trim().chars().count();
        if n > 0 {
            total_w += (c.r - c.l).max(0.0);
            total_chars += n;
        }
    }
    let char_w = if total_chars > 0 {
        (total_w / total_chars as f32).max(1.0)
    } else {
        1.0
    };
    // The block's own left margin is the zero-indent baseline.
    let base_l = inside.iter().map(|c| c.l).fold(f32::INFINITY, f32::min);

    let mut lines: Vec<String> = Vec::new();
    let mut cur: Option<i64> = None;
    for c in &inside {
        // Tighten pdfium's spaced punctuation per line (on the trimmed content, so
        // the reconstructed leading indentation is never nibbled).
        let text = tighten_code_punct(&clean_text(c.text.trim()));
        if Some(line_of(c)) == cur {
            // A second cell sharing this band (rare — e.g. split columns): keep it
            // on the same source line, separated by a space.
            if let Some(last) = lines.last_mut() {
                last.push(' ');
                last.push_str(&text);
            }
            continue;
        }
        let indent = ((c.l - base_l) / char_w).round().max(0.0) as usize;
        lines.push(format!("{}{}", " ".repeat(indent), text));
        cur = Some(line_of(c));
    }
    lines.join("\n")
}

/// Reconstruct a table's grid geometrically from the text cells inside its
/// region: cluster cells into rows (by vertical centre) and columns (by clustered
/// left edges), then place each cell. A model-free stand-in for TableFormer that
/// recovers grid-aligned tables from the precise PDF text layer (it does not
/// resolve row/column spans).
fn reconstruct_table(region: &Region, cells: &[TextCell]) -> Vec<Vec<String>> {
    let mut inside: Vec<&TextCell> = cells
        .iter()
        .filter(|c| {
            let ca = area(c.l, c.t, c.r, c.b).max(1.0);
            inter(region, c.l, c.t, c.r, c.b) / ca > 0.5
        })
        .collect();
    if inside.is_empty() {
        return Vec::new();
    }
    inside.sort_by(|a, b| a.t.total_cmp(&b.t));

    // Rows: consecutive cells whose vertical centre is within ~0.7 line height.
    let mut rows: Vec<(f32, Vec<&TextCell>)> = Vec::new();
    for c in &inside {
        let cyc = (c.t + c.b) / 2.0;
        let lh = (c.b - c.t).abs().max(1.0);
        if let Some((ryc, row)) = rows.last_mut() {
            if (cyc - *ryc).abs() < lh * 0.7 {
                row.push(c);
                continue;
            }
        }
        rows.push((cyc, vec![c]));
    }

    // Columns: cluster left edges (merge those within a tolerance).
    let tol = {
        let mut hs: Vec<f32> = inside.iter().map(|c| (c.b - c.t).abs()).collect();
        hs.sort_by(f32::total_cmp);
        hs[hs.len() / 2].max(4.0) * 1.5
    };
    let mut lefts: Vec<f32> = inside.iter().map(|c| c.l).collect();
    lefts.sort_by(f32::total_cmp);
    let mut col_starts: Vec<f32> = Vec::new();
    for l in lefts {
        if col_starts.last().is_none_or(|&last| l - last > tol) {
            col_starts.push(l);
        }
    }
    let ncols = col_starts.len().max(1);
    let col_of = |l: f32| -> usize {
        col_starts
            .iter()
            .rposition(|&s| l + tol * 0.5 >= s)
            .unwrap_or(0)
            .min(ncols - 1)
    };

    let mut grid = Vec::with_capacity(rows.len());
    for (_, mut row) in rows {
        row.sort_by(|a, b| a.l.total_cmp(&b.l));
        let mut cols = vec![String::new(); ncols];
        for c in row {
            let ci = col_of(c.l);
            // Strip the wrap-hyphen control char so it never lands in a cell.
            let t = c.text.trim().replace(['\u{2}', '\u{ad}'], "");
            if cols[ci].is_empty() {
                cols[ci] = t;
            } else {
                cols[ci].push(' ');
                cols[ci].push_str(&t);
            }
        }
        grid.push(cols);
    }
    grid
}

/// Crop a layout region from the rendered page image and encode it as PNG (the
/// figure bytes docling stores on a `PictureItem`). Region coordinates are page
/// points; the image is rendered at `page.scale`.
fn crop_region(page: &PdfPage, region: &Region) -> Option<PictureImage> {
    let s = page.scale;
    let (iw, ih) = (page.image.width(), page.image.height());
    let x = (region.l * s).max(0.0) as u32;
    let y = (region.t * s).max(0.0) as u32;
    if x >= iw || y >= ih {
        return None;
    }
    let w = (((region.r - region.l) * s) as u32).min(iw - x);
    let h = (((region.b - region.t) * s) as u32).min(ih - y);
    if w == 0 || h == 0 {
        return None;
    }
    let sub = image::imageops::crop_imm(&page.image, x, y, w, h).to_image();
    let mut buf = std::io::Cursor::new(Vec::new());
    sub.write_to(&mut buf, image::ImageFormat::Png).ok()?;
    Some(PictureImage {
        mimetype: "image/png".into(),
        width: w,
        height: h,
        data: buf.into_inner(),
    })
}

/// For each `picture` region, find the `caption` region closest below it (and
/// horizontally overlapping); docling pairs them and emits the caption first.
/// Each caption is claimed by at most one picture.
fn pair_captions(regions: &[Region]) -> Vec<Option<usize>> {
    let mut pairs = vec![None; regions.len()];
    let mut taken = vec![false; regions.len()];
    for (pi, p) in regions.iter().enumerate() {
        if p.label != "picture" {
            continue;
        }
        let mut best: Option<(usize, f32)> = None;
        for (ci, c) in regions.iter().enumerate() {
            if c.label != "caption" || taken[ci] {
                continue;
            }
            let line_h = (c.b - c.t).abs().max(1.0);
            let gap = c.t - p.b; // caption sits below the picture
            let h_overlap = (p.r.min(c.r) - p.l.max(c.l)).max(0.0);
            if gap > -line_h && gap < line_h * 3.0 && h_overlap > 0.0 {
                let dist = gap.abs();
                if best.is_none_or(|(_, bd)| dist < bd) {
                    best = Some((ci, dist));
                }
            }
        }
        if let Some((ci, _)) = best {
            pairs[pi] = Some(ci);
            taken[ci] = true;
        }
    }
    pairs
}

/// Pair each `code` region with the `caption` region just **above** it (a
/// `Listing N:` label). docling renders the code block first, then its caption,
/// so the caption is consumed from its own (earlier) reading-order slot and
/// re-emitted after the code.
fn pair_code_captions(regions: &[Region]) -> Vec<Option<usize>> {
    let mut pairs = vec![None; regions.len()];
    let mut taken = vec![false; regions.len()];
    for (pi, p) in regions.iter().enumerate() {
        if p.label != "code" {
            continue;
        }
        let mut best: Option<(usize, f32)> = None;
        for (ci, c) in regions.iter().enumerate() {
            if c.label != "caption" || taken[ci] {
                continue;
            }
            let line_h = (c.b - c.t).abs().max(1.0);
            let gap = p.t - c.b; // caption sits above the code
            let h_overlap = (p.r.min(c.r) - p.l.max(c.l)).max(0.0);
            if gap > -line_h && gap < line_h * 3.0 && h_overlap > 0.0 {
                let dist = gap.abs();
                if best.is_none_or(|(_, bd)| dist < bd) {
                    best = Some((ci, dist));
                }
            }
        }
        if let Some((ci, _)) = best {
            pairs[pi] = Some(ci);
            taken[ci] = true;
        }
    }
    pairs
}

/// Assemble one page from its (already overlap-resolved) layout regions and
/// text cells.
pub fn assemble_page(
    page: &PdfPage,
    regions: Vec<Region>,
    table_rows: &[Option<Vec<Vec<String>>>],
) -> (Vec<Node>, Vec<(String, String)>) {
    let mut nodes: Vec<Node> = Vec::new();
    // Recover this page's hyperlinks (rendered only in strict Markdown).
    let links = resolve_link_anchors(page);
    // Pair each region with its precomputed TableFormer grid (indexed by original
    // order) and order by reading order together, so they stay aligned.
    let mut items: Vec<(Region, Option<Vec<Vec<String>>>)> = regions
        .into_iter()
        .enumerate()
        .map(|(i, r)| (r, table_rows.get(i).cloned().flatten()))
        .collect();
    order_regions(&mut items, page.width, |it| &it.0);
    // Float a margin page number to the front of reading order (docling parity:
    // right_to_left_02's bottom `11` is its first item). Stable, so everything
    // else keeps its order; no-op on pages without such a region.
    let page_h = page.height;
    items.sort_by_key(|(r, _)| !is_page_number(r, &page.cells, page_h));
    let table_rows: Vec<Option<Vec<Vec<String>>>> = items.iter().map(|(_, t)| t.clone()).collect();
    let regions: Vec<Region> = items.into_iter().map(|(r, _)| r).collect();
    // docling emits a figure's caption *before* the image marker. Pair each
    // picture with the caption region nearest below it and consume that caption,
    // so it isn't also emitted in its own (lower) reading-order position.
    let caption_for = pair_captions(&regions);
    let code_caption_for = pair_code_captions(&regions);
    let mut consumed = vec![false; regions.len()];
    for ci in caption_for.iter().flatten() {
        consumed[*ci] = true;
    }
    for ci in code_caption_for.iter().flatten() {
        consumed[*ci] = true;
    }
    // A code block's language label (`XML`, `C#`, …) is chrome, not content — the
    // detector emits it as its own region above the code; consume it.
    for (i, is_label) in code_language_labels(&regions, &page.cells)
        .into_iter()
        .enumerate()
    {
        if is_label {
            consumed[i] = true;
        }
    }

    for (i, region) in regions.iter().enumerate() {
        if is_skipped(region.label) || consumed[i] {
            continue;
        }
        if region.label == "picture" {
            // The figure pixels are cropped from the page render for image export.
            let caption = caption_for[i]
                .map(|ci| region_text(&regions[ci], &page.cells))
                .filter(|t| !t.is_empty());
            nodes.push(Node::Picture {
                caption,
                image: crate::timing::timed("crop_region", || crop_region(page, region)),
            });
            continue;
        }
        let text = region_text(region, &page.cells);
        if text.is_empty() {
            continue;
        }
        match region.label {
            // docling renders both the document title and section headers as
            // `##` (it never emits a top-level `#` for PDFs), so match that.
            "title" | "section_header" => nodes.push(Node::Heading {
                level: 2,
                text: md_escape(&text),
            }),
            // docling drops the rendered bullet glyph; the Markdown serializer
            // adds its own `- ` marker. An item whose text opens with an `N.`
            // enumeration marker is an ordered item (rendered `N. text`).
            "list_item" => {
                let stripped = text
                    .trim_start_matches(['•', '◦', '▪', '·', '*', '-'])
                    .trim_start()
                    .to_string();
                if let Some((number, rest)) = parse_ordered_marker(&stripped) {
                    nodes.push(Node::ListItem {
                        ordered: true,
                        number,
                        first_in_list: false,
                        text: md_escape(&rest),
                        level: 0,
                    });
                } else {
                    nodes.push(Node::ListItem {
                        ordered: false,
                        number: 0,
                        first_in_list: false,
                        text: md_escape(&stripped),
                        level: 0,
                    });
                }
            }
            // TableFormer structure (cells + spans, text matched from word cells)
            // when available; otherwise geometric grid reconstruction; finally a
            // single cell.
            "table" => {
                let rows = table_rows[i].clone().unwrap_or_else(|| {
                    let rows = reconstruct_table(region, &page.cells);
                    if rows.iter().any(|r| r.len() > 1) {
                        rows
                    } else {
                        vec![vec![text.clone()]]
                    }
                });
                nodes.push(Node::Table(Table {
                    rows,
                    location: None,
                }));
            }
            // docling does not decode formulas in the standard pipeline; it emits
            // a placeholder comment rather than the (garbled) raw glyph text.
            "formula" => nodes.push(Node::Paragraph {
                text: "<!-- formula-not-decoded -->".into(),
            }),
            // Code blocks: use the space-glyph-only grouping (monospace keeps its
            // source spacing) and emit a fenced block, preserving the line breaks
            // and indentation of the source (unlike prose, which reflows). pdfium
            // still inserts spaces around tight punctuation (`console .log`,
            // `add (3 , 5)`); tighten them to match docling-parse's source spacing.
            "code" => {
                // `code_region_text` preserves line breaks/indentation and tightens
                // each line itself; the fallback prose `text` is tightened here.
                let code = code_region_text(region, &page.code_cells);
                let code = if code.is_empty() {
                    tighten_code_punct(&text)
                } else {
                    code
                };
                nodes.push(Node::Code {
                    language: None,
                    text: code,
                });
                // docling emits the `Listing N:` caption after the code block.
                if let Some(ci) = code_caption_for[i] {
                    let cap = region_text(&regions[ci], &page.cells);
                    if !cap.is_empty() {
                        nodes.push(Node::Paragraph { text: cap });
                    }
                }
            }
            // text, caption, footnote → paragraph
            _ => nodes.push(Node::Paragraph {
                text: md_escape(&text),
            }),
        }
    }
    (nodes, links)
}

/// Merge paragraph fragments split across a column or page break. docling joins a
/// paragraph whose previous fragment ends mid-sentence (a letter, not sentence
/// punctuation) with a lowercase continuation: `…definition of` + `lists in…` →
/// `…definition of lists in…`. The fragments are consecutive paragraphs, or
/// separated only by figure(s) the text wraps around: a column whose body flows
/// past a figure resumes below it (`…The wing type that is` ⟶[figure]⟶ `the most
/// common…`), and docling emits the whole paragraph before the figure. A heading,
/// table, or list between them ends the paragraph (no merge).
/// A paragraph that is really a figure/table caption (`Fig. 1. …`, `Table 2 …`).
/// Used to skip an unpaired caption when stitching a paragraph that wraps around
/// a figure.
fn looks_like_caption(text: &str) -> bool {
    let head: String = text.trim_start().chars().take(14).collect();
    (head.starts_with("Fig") || head.starts_with("Table"))
        && head.contains(|c: char| c.is_ascii_digit())
}

/// A paragraph fragment is "open" — i.e. it might continue into the next
/// paragraph — when it ends mid-word (a letter) or with a wrap hyphen/dash.
/// docling joins `vocab-` + `ulary` → `vocab- ulary`.
fn paragraph_is_open(text: &str) -> bool {
    text.trim_end().chars().next_back().is_some_and(|c| {
        c.is_alphabetic() || matches!(c, '-' | '\u{2010}' | '\u{2013}' | '\u{2014}')
    })
}

pub(crate) fn merge_continuations(nodes: &mut Vec<Node>) {
    let mut i = 0;
    while i + 1 < nodes.len() {
        let Node::Paragraph { text: a } = &nodes[i] else {
            i += 1;
            continue;
        };
        // A figure/table caption is a self-contained unit; body text resuming
        // after a figure is the continuation case, not the caption itself. Never
        // stitch *from* a caption — otherwise a caption that ends in a lone glyph
        // (`Fig. 5. … PubTabNet. μ`) would swallow a following stray figure label
        // (a standalone `μ`) into `… μ μ`.
        if looks_like_caption(a) {
            i += 1;
            continue;
        }
        if !paragraph_is_open(a) {
            i += 1;
            continue;
        }
        // The continuation is the next paragraph, looking past any figures the
        // text wraps around — and a figure/table caption that was emitted as its
        // own paragraph (an above-the-figure caption that didn't pair), since the
        // body text resumes after the whole figure+caption block.
        let mut j = i + 1;
        while matches!(nodes.get(j), Some(Node::Picture { .. }))
            || matches!(nodes.get(j), Some(Node::Paragraph { text }) if looks_like_caption(text))
        {
            j += 1;
        }
        let cont = matches!(nodes.get(j), Some(Node::Paragraph { text: b })
            if b.trim_start().chars().next().is_some_and(char::is_lowercase));
        if cont {
            let a = match &nodes[i] {
                Node::Paragraph { text } => text.trim_end().to_string(),
                _ => unreachable!(),
            };
            let b = match &nodes[j] {
                Node::Paragraph { text } => text.trim_start().to_string(),
                _ => unreachable!(),
            };
            nodes[i] = Node::Paragraph {
                text: format!("{a} {b}"),
            };
            nodes.remove(j);
            // Re-check i: the merged paragraph may continue further.
        } else {
            i += 1;
        }
    }
}

/// How many leading nodes of `nodes` are safe to flush now — i.e. cannot be
/// rewritten by a future [`merge_continuations`] once more pages are appended.
///
/// A forward merge can only start from an "open" paragraph (ends mid-word) and
/// only reaches across trailing pictures and figure/table captions. So we scan
/// from the end past those skippable trailers: if the first non-skippable node is
/// an open paragraph, it (and the trailers after it) must be held; anything else —
/// a closed paragraph, a heading, a table, a list — blocks any forward merge, so
/// the whole buffer is safe to flush.
fn hold_start(nodes: &[Node]) -> usize {
    for k in (0..nodes.len()).rev() {
        match &nodes[k] {
            // Skippable trailers: a forward merge looks straight past them.
            Node::Picture { .. } => continue,
            Node::Paragraph { text } if looks_like_caption(text) => continue,
            // An open body paragraph might still pull a continuation off the next
            // page — hold from here to the end.
            Node::Paragraph { text } if paragraph_is_open(text) => return k,
            // A closed paragraph, heading, table, list, etc. ends the paragraph:
            // nothing after it can merge backwards across it. Flush everything.
            _ => return nodes.len(),
        }
    }
    // Only skippable trailers (or empty) and no open paragraph to anchor a merge.
    nodes.len()
}

/// Streaming counterpart of [`merge_continuations`]: feed per-page node batches in
/// document order and get back the prefix that is final (its cross-page merges are
/// resolved and no future page can change it), holding back only the small tail
/// that might still merge into the next page. Concatenating every flushed batch
/// (then [`finish`](Self::finish)) yields exactly the same nodes as running
/// [`merge_continuations`] once over the whole document.
pub(crate) struct StreamAssembler {
    pending: Vec<Node>,
}

impl StreamAssembler {
    pub(crate) fn new() -> Self {
        Self {
            pending: Vec::new(),
        }
    }

    /// Append one page's nodes, resolve merges within the buffer, and return the
    /// now-final prefix to emit (possibly empty).
    pub(crate) fn push(&mut self, mut nodes: Vec<Node>) -> Vec<Node> {
        self.pending.append(&mut nodes);
        merge_continuations(&mut self.pending);
        let cut = hold_start(&self.pending);
        let tail = self.pending.split_off(cut);
        std::mem::replace(&mut self.pending, tail)
    }

    /// Flush whatever is left after the last page (the held tail is final once no
    /// more pages can follow).
    pub(crate) fn finish(self) -> Vec<Node> {
        self.pending
    }
}

#[cfg(test)]
mod tests {
    use super::clean_text;
    use super::{code_region_text, merge_continuations, resolve_link_anchors, StreamAssembler};
    use crate::layout::Region;
    use crate::pdfium_backend::{LinkAnnot, PdfPage, TextCell};
    use docling_core::Node;

    #[test]
    fn link_anchors_split_a_shared_word_cell_between_adjacent_links() {
        // A common header layout: one text run holds several pipe-separated
        // labels, each carrying its own link annotation. Every link must get
        // its own label as the anchor (and the "|" separators must belong to
        // none), not the whole run.
        let annot = |l: f32, r: f32, uri: &str| LinkAnnot {
            l,
            t: 100.0,
            r,
            b: 114.0,
            uri: uri.into(),
        };
        let page = PdfPage {
            width: 600.0,
            height: 800.0,
            scale: 2.0,
            cells: Vec::new(),
            code_cells: Vec::new(),
            // "LinkedIn | GitHub | Credly" = 26 chars over x 100..360.
            word_cells: vec![cell(
                "LinkedIn | GitHub | Credly",
                100.0,
                100.0,
                360.0,
                114.0,
            )],
            image: image::RgbImage::new(1, 1),
            links: vec![
                annot(100.0, 180.0, "https://l"),
                annot(200.0, 260.0, "https://g"),
                annot(290.0, 360.0, "https://c"),
            ],
        };
        assert_eq!(
            resolve_link_anchors(&page),
            vec![
                ("LinkedIn".to_string(), "https://l".to_string()),
                ("GitHub".to_string(), "https://g".to_string()),
                ("Credly".to_string(), "https://c".to_string()),
            ]
        );
    }

    /// A one-line code cell at `[l, r] × [t, b]` (top-left coords).
    fn cell(text: &str, l: f32, t: f32, r: f32, b: f32) -> TextCell {
        TextCell {
            text: text.into(),
            l,
            t,
            r,
            b,
        }
    }

    fn region(label: &'static str, score: f32, l: f32, t: f32, r: f32, b: f32) -> Region {
        Region {
            label,
            score,
            l,
            t,
            r,
            b,
        }
    }

    #[test]
    fn resolve_collapses_nested_code_keeping_the_larger_box() {
        // A tight high-score `code` box and a taller lower-score near-duplicate that
        // contains it must collapse to one — the *larger* box, so every cell stays
        // covered and nothing leaks out as orphan text.
        let tight = region("code", 0.95, 78.0, 292.0, 300.0, 330.0);
        let wide = region("code", 0.66, 63.0, 260.0, 320.0, 346.0);
        let kept = super::resolve(vec![tight, wide]);
        assert_eq!(kept.len(), 1, "nested code boxes must collapse to one");
        assert!(
            kept[0].l == 63.0 && kept[0].b == 346.0,
            "the larger containing box is kept"
        );
    }

    #[test]
    fn resolve_keeps_distinct_and_differently_typed_regions() {
        // A text box fully inside a lower-score *table* must NOT be collapsed (the
        // code dedup is code-only), and two separate code blocks stay separate.
        let text = region("text", 0.95, 90.0, 210.0, 200.0, 230.0);
        let table = region("table", 0.60, 80.0, 200.0, 400.0, 500.0);
        assert_eq!(super::resolve(vec![text, table]).len(), 2);

        let code_a = region("code", 0.9, 78.0, 100.0, 300.0, 140.0);
        let code_b = region("code", 0.9, 78.0, 300.0, 300.0, 360.0); // far below, no overlap
        assert_eq!(super::resolve(vec![code_a, code_b]).len(), 2);
    }

    #[test]
    fn code_language_label_above_code_is_detected() {
        // A bare "XML" token directly above a code box is a language label; a real
        // heading above the same code is not; a language word with no code below is
        // left alone.
        let label = region("section_header", 0.9, 76.0, 540.0, 96.0, 549.0);
        let code = region("code", 0.7, 77.0, 552.0, 290.0, 640.0);
        let heading = region("section_header", 0.9, 76.0, 500.0, 260.0, 512.0);
        let cells = vec![
            cell("XML", 78.0, 541.0, 94.0, 548.0),       // inside `label`
            cell("Overview", 78.0, 501.0, 250.0, 511.0), // inside `heading`
        ];
        let drop = super::code_language_labels(&[label, code, heading], &cells);
        assert_eq!(drop, vec![true, false, false], "only the label is consumed");

        // Same label with no code region present → not consumed.
        let label2 = region("section_header", 0.9, 76.0, 540.0, 96.0, 549.0);
        let only = vec![cell("XML", 78.0, 541.0, 94.0, 548.0)];
        assert_eq!(super::code_language_labels(&[label2], &only), vec![false]);

        // A label swallowed into the top of a wider code box (negative gap) is still
        // recognized.
        let inside_lbl = region("text", 0.9, 76.0, 540.0, 96.0, 549.0);
        let wide_code = region("code", 0.7, 63.0, 531.0, 320.0, 654.0);
        let cells2 = vec![cell("XML", 78.0, 541.0, 94.0, 548.0)];
        assert_eq!(
            super::code_language_labels(&[inside_lbl, wide_code], &cells2),
            vec![true, false]
        );

        assert!(super::is_code_language("XML") && super::is_code_language("c#"));
        assert!(!super::is_code_language("Configure") && !super::is_code_language("XML schema"));
    }

    #[test]
    fn code_region_text_keeps_lines_and_indentation() {
        // Three source lines; each glyph is 6 units wide (width / chars = 6), so the
        // `int X;` line indented to x=22 is (22-10)/6 = 2 spaces in.
        let region = Region {
            label: "code",
            score: 1.0,
            l: 0.0,
            t: -5.0,
            r: 100.0,
            b: 40.0,
        };
        let cells = vec![
            cell("struct P {", 10.0, 0.0, 70.0, 10.0),
            cell("int X;", 22.0, 12.0, 58.0, 22.0),
            cell("}", 10.0, 24.0, 16.0, 34.0),
        ];
        assert_eq!(code_region_text(&region, &cells), "struct P {\n  int X;\n}");
    }

    #[test]
    fn code_region_text_tightens_punctuation_without_eating_indentation() {
        // A fluent `.Foo()` line at x=22 (2 chars in). Per-line tightening must not
        // consume the leading indent space by matching " ." across it.
        let region = Region {
            label: "code",
            score: 1.0,
            l: 0.0,
            t: -5.0,
            r: 100.0,
            b: 40.0,
        };
        let cells = vec![
            cell("builder", 10.0, 0.0, 52.0, 10.0),
            // pdfium spaced the call: ".Foo (x)" tightens to ".Foo(x)", still 2-indented.
            cell(".Foo (x)", 22.0, 12.0, 70.0, 22.0),
        ];
        assert_eq!(code_region_text(&region, &cells), "builder\n  .Foo(x)");
    }

    #[test]
    fn code_region_text_orders_out_of_order_cells_and_ignores_blank_lines() {
        let region = Region {
            label: "code",
            score: 1.0,
            l: 0.0,
            t: -5.0,
            r: 100.0,
            b: 60.0,
        };
        // Fed bottom-up and with a whitespace-only cell; output is top-down, no blank.
        let cells = vec![
            cell("b();", 10.0, 24.0, 34.0, 34.0),
            cell("   ", 10.0, 12.0, 20.0, 22.0),
            cell("a();", 10.0, 0.0, 34.0, 10.0),
        ];
        assert_eq!(code_region_text(&region, &cells), "a();\nb();");
        // No code cells → empty, so the caller falls back to the prose text.
        assert_eq!(code_region_text(&region, &[]), "");
    }

    fn para(text: &str) -> Node {
        Node::Paragraph { text: text.into() }
    }

    /// Run a node sequence through [`StreamAssembler`] with the given page splits
    /// and assert the flushed result equals one-shot [`merge_continuations`].
    fn assert_stream_eq(nodes: &[Node], splits: &[usize]) {
        let mut want = nodes.to_vec();
        merge_continuations(&mut want);

        let mut asm = StreamAssembler::new();
        let mut got = Vec::new();
        let mut start = 0;
        for &end in splits {
            got.extend(asm.push(nodes[start..end].to_vec()));
            start = end;
        }
        got.extend(asm.push(nodes[start..].to_vec()));
        got.extend(asm.finish());
        assert_eq!(got, want, "stream assembly diverged (splits={splits:?})");
    }

    #[test]
    fn stream_assembler_matches_merge_continuations() {
        // Open fragment + lowercase continuation split across a page boundary.
        let cross = [para("the definition of"), para("lists in scope")];
        assert_stream_eq(&cross, &[1]);
        assert_stream_eq(&cross, &[]);

        // Continuation that wraps around a figure (+ its caption) on the boundary.
        let wrap = [
            para("the wing type that is"),
            Node::Picture {
                caption: None,
                image: None,
            },
            para("Fig. 1. a diagram"),
            para("the most common kind"),
        ];
        for splits in [&[][..], &[1][..], &[2][..], &[3][..], &[1, 3][..]] {
            assert_stream_eq(&wrap, splits);
        }

        // A heading between fragments blocks the merge (must still flush correctly).
        let blocked = [
            para("ends mid word and"),
            Node::Heading {
                level: 2,
                text: "New Section".into(),
            },
            para("more body here"),
        ];
        for splits in [&[][..], &[1][..], &[2][..]] {
            assert_stream_eq(&blocked, splits);
        }

        // A chain across three pages: each page is one open lowercase fragment.
        let chain = [
            para("alpha beta"),
            para("gamma delta"),
            para("epsilon zeta"),
        ];
        assert_stream_eq(&chain, &[1, 2]);
    }

    #[test]
    fn clean_text_dehyphenates_and_normalizes_typography() {
        // U+0002 line-wrap hyphen + the join space → merged word (like docling).
        assert_eq!(clean_text("com\u{2} pact"), "compact");
        assert_eq!(clean_text("end-to\u{2} end deep"), "end-toend deep");
        // A stray wrap hyphen (no following join) is dropped.
        assert_eq!(clean_text("word\u{2}"), "word");
        // Typographic punctuation → ASCII.
        assert_eq!(
            clean_text("Graph\u{2019}s \u{201c}x\u{201d}"),
            "Graph's \"x\""
        );
        assert_eq!(clean_text("a\u{2026}"), "a...");
        // The dp default (the docling-parse sanitizer) preserves internal spacing
        // it placed deliberately; line breaks/tabs normalize to a space, ends trim.
        assert_eq!(clean_text("a   b\nc"), "a   b c");
    }

    #[test]
    fn lam_alef_only_swaps_a_genuinely_reversed_ligature() {
        // A mid-word `alef-variant + lam` is pdfium's reversed lam-alef ligature and
        // is swapped back to logical `lam + alef-variant` (`ب أ ل` → `ب ل أ`).
        assert_eq!(
            clean_text("\u{0628}\u{0623}\u{0644}"),
            "\u{0628}\u{0644}\u{0623}"
        );
        // But when the alef-variant is *already* preceded by a lam it is the logical
        // ligature `لآ`; the following lam is the next syllable's letter and must not
        // move. `التعلم الآلي` must stay `الآلي`, not become `اللآي`.
        assert_eq!(
            clean_text("\u{0627}\u{0644}\u{0622}\u{0644}\u{064a}"),
            "\u{0627}\u{0644}\u{0622}\u{0644}\u{064a}"
        );
    }
}
