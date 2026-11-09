//! Superscript and subscript text from the size and baseline of PDF characters
//! (#1005) — a port of docling's `utils/script_position.py` (docling#4815).
//!
//! A character is a superscript or subscript when it is drawn smaller than the
//! body text of its line and raised or lowered against the body's baseline,
//! within the line. [`script_runs`] then splits a text item's text into
//! baseline, superscript and subscript runs, after matching the text to the
//! characters (they differ in ligatures, dashes and spaces).
//!
//! Every constant and rule is docling's; the comments are its reasons.

use std::collections::HashMap;

use docling_core::{Node, Script};
use unicode_normalization::UnicodeNormalization;

/// A script is drawn at most this share of the body size…
const MAX_SCRIPT_SIZE: f32 = 0.85;
/// …and shifted by at least this share of it. Word lowers a subscript by
/// under a tenth.
const MIN_SHIFT: f32 = 0.05;
const MIN_SHIFT_POINTS: f32 = 0.4;
/// Baselines this close (share of the largest size) are the same baseline.
const BASELINE_TOLERANCE: f32 = 0.04;
const BASELINE_TOLERANCE_POINTS: f32 = 0.25;
/// Baseline groups at least this share of the largest letter size can be the
/// body.
const BODY_SIZE_RATIO: f32 = 0.9;
/// Largest distance (share of the body size) between a script and the text it
/// marks.
const TOUCH_GAP: f32 = 0.6;
/// Letters this close to body letters on both sides sit inside a word
/// ("LaTeX").
const IN_WORD_GAP: f32 = 0.15;
/// Smallest gap (share of the larger size) for a space beside a script to be
/// real. The parser also adds a space where a raised script touches its text.
const MIN_WORD_GAP: f32 = 0.1;
/// A new line starts where the baseline moves by this share of the line's
/// size, or where the pen returns to the line's start (within this share of
/// the size; a narrow base letter, as under a stacked script, is wider).
const LINE_JUMP: f32 = 0.85;
const LINE_START: f32 = 0.15;
/// Marks raised as footnote references with no letter or digit.
const FOOTNOTE_MARKS: &[char] = &['*', '†', '‡', '§', '¶', '‖', '#'];
/// Share of an item's characters that must be found among the PDF
/// characters.
const MIN_ALIGNED: f32 = 0.8;
/// Matching is quadratic, so longer items are skipped.
const MAX_ALIGN_CHARS: usize = 5000;

/// A character as detection needs it — docling's `PdfChar`: the font-metric
/// box, the baseline and the drawn size, PDF points with a bottom-left
/// origin.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ScriptChar {
    pub text: char,
    pub left: f32,
    pub bottom: f32,
    pub right: f32,
    pub top: f32,
    pub baseline: f32,
    /// The font size scaled by the text matrix.
    pub size: f32,
    pub horizontal: bool,
}

/// A piece of a text item's text at one script position — docling's
/// `ScriptRun`.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ScriptRun {
    pub text: String,
    pub script: Script,
    /// Indexes of the PDF characters matched to the run.
    pub chars: Vec<usize>,
    /// Position (in chars) in `text` of each of `chars`.
    pub offsets: Vec<usize>,
}

/// The script position of each character, in content order.
pub(crate) fn script_positions(chars: &[ScriptChar]) -> Vec<Script> {
    let mut positions = vec![Script::Baseline; chars.len()];
    for line in split_lines(chars) {
        for (index, script) in line_scripts(chars, &line) {
            positions[index] = script;
        }
    }
    positions
}

/// Split an item's text into baseline, superscript and subscript runs.
///
/// The text is matched to the PDF characters first, and only the matched
/// characters are measured. Runs carry the spaces between them, since
/// serializers join runs with nothing, and a space that the PDF does not show
/// beside a script is dropped. `None` when the text has no script or cannot
/// be matched.
pub(crate) fn script_runs(text: &str, chars: &[ScriptChar]) -> Option<Vec<ScriptRun>> {
    let text: Vec<char> = text.chars().collect();
    let mapping = align(&text, chars)?;
    let mut matched: Vec<usize> = mapping.values().copied().collect();
    matched.sort_unstable();
    matched.dedup();
    let subset: Vec<ScriptChar> = matched.iter().map(|&i| chars[i].clone()).collect();
    let positions: HashMap<usize, Script> = matched
        .iter()
        .copied()
        .zip(script_positions(&subset))
        .collect();

    let mut roles: Vec<Option<Script>> = vec![None; text.len()];
    for (&index, char_index) in &mapping {
        roles[index] = Some(positions[char_index]);
    }
    if roles
        .iter()
        .all(|r| matches!(r, None | Some(Script::Baseline)))
    {
        return None;
    }

    let solid: Vec<usize> = (0..text.len()).filter(|&i| !is_space(text[i])).collect();
    // Unmatched characters take the position of matched neighbours that agree.
    for k in 0..solid.len() {
        let index = solid[k];
        if roles[index].is_some() {
            continue;
        }
        let before = solid[..k].iter().rev().find_map(|&j| roles[j]);
        let after = solid[k + 1..].iter().find_map(|&j| roles[j]);
        roles[index] = match (before, after) {
            (Some(b), Some(a)) if b == a => Some(b),
            _ => Some(Script::Baseline),
        };
    }
    // Whitespace inside a run belongs to it; whitespace between runs stays
    // where the PDF shows it, in the baseline run and out of the script.
    for w in solid.windows(2) {
        let (left, right) = (w[0], w[1]);
        if roles[left] != roles[right] {
            if let (Some(&l), Some(&r)) = (mapping.get(&left), mapping.get(&right)) {
                if !is_word_gap(&chars[l], &chars[r]) {
                    continue;
                }
            }
        }
        let owner = if roles[right] == Some(Script::Baseline) {
            roles[right]
        } else {
            roles[left]
        };
        for role in &mut roles[left + 1..right] {
            *role = owner;
        }
    }

    let mut pieces: Vec<ScriptRun> = Vec::new();
    for (i, &ch) in text.iter().enumerate() {
        let Some(role) = roles[i] else { continue };
        if pieces.last().is_none_or(|p| p.script != role) {
            pieces.push(ScriptRun {
                text: String::new(),
                script: role,
                chars: Vec::new(),
                offsets: Vec::new(),
            });
        }
        let piece = pieces.last_mut().expect("just pushed");
        if let Some(&c) = mapping.get(&i) {
            if !piece.chars.contains(&c) {
                piece.chars.push(c);
                piece.offsets.push(piece.text.chars().count());
            }
        }
        piece.text.push(ch);
    }
    Some(pieces)
}

/// Python's `str.isspace` for one character (Rust's whitespace plus the
/// information separators U+001C–U+001F).
fn is_space(c: char) -> bool {
    c.is_whitespace() || ('\u{1c}'..='\u{1f}').contains(&c)
}

fn measurable(c: &ScriptChar) -> bool {
    c.horizontal && c.size > 0.0 && !is_space(c.text)
}

/// Split the measurable characters into visual lines, as lists of indexes.
///
/// A line ends where the baseline moves by most of a line height, or where the
/// pen returns to the line's start. A script stacked over another (x with i
/// below, 2 above) does neither, however wide it is.
fn split_lines(chars: &[ScriptChar]) -> Vec<Vec<usize>> {
    let mut lines = Vec::new();
    let mut line: Vec<usize> = Vec::new();
    let mut line_size = 0f32;
    for (index, c) in chars.iter().enumerate() {
        if !measurable(c) {
            continue;
        }
        if let (Some(&last), Some(&first)) = (line.last(), line.first()) {
            let size = c.size.max(line_size);
            let previous = &chars[last];
            let returns_to_start =
                c.left < previous.left && c.left <= chars[first].left + LINE_START * size;
            if (c.baseline - previous.baseline).abs() > LINE_JUMP * size || returns_to_start {
                lines.push(std::mem::take(&mut line));
                line_size = 0.0;
            }
        }
        line.push(index);
        line_size = line_size.max(c.size);
    }
    if !line.is_empty() {
        lines.push(line);
    }
    lines
}

/// Python's `statistics.median`.
fn median(mut v: Vec<f32>) -> f32 {
    v.sort_by(f32::total_cmp);
    let n = v.len();
    if n % 2 == 1 {
        v[n / 2]
    } else {
        (v[n / 2 - 1] + v[n / 2]) / 2.0
    }
}

fn baseline_groups(chars: &[ScriptChar], line: &[usize]) -> Vec<Vec<usize>> {
    let largest = line.iter().map(|&i| chars[i].size).fold(f32::MIN, f32::max);
    let tolerance = BASELINE_TOLERANCE_POINTS.max(BASELINE_TOLERANCE * largest);
    let mut sorted = line.to_vec();
    sorted.sort_by(|&a, &b| chars[a].baseline.total_cmp(&chars[b].baseline));
    let mut groups: Vec<Vec<usize>> = Vec::new();
    for index in sorted {
        let joins = groups.last().is_some_and(|g| {
            let m = median(g.iter().map(|&i| chars[i].baseline).collect());
            (chars[index].baseline - m).abs() <= tolerance
        });
        if joins {
            groups.last_mut().expect("checked").push(index);
        } else {
            groups.push(vec![index]);
        }
    }
    groups
}

/// The largest size of the group's letters and digits, 0 if it has none.
fn letter_size(chars: &[ScriptChar], group: &[usize]) -> f32 {
    group
        .iter()
        .filter(|&&i| chars[i].text.is_alphanumeric())
        .map(|&i| chars[i].size)
        .fold(0.0, f32::max)
}

/// The superscript and subscript characters of one visual line.
fn line_scripts(chars: &[ScriptChar], line: &[usize]) -> Vec<(usize, Script)> {
    let groups = baseline_groups(chars, line);
    if groups.len() < 2 {
        return Vec::new();
    }
    // The body: among the groups set in the largest letters, the one with the
    // most letters. Letters set the size of text; a large symbol (a sum sign)
    // does not.
    let largest = groups
        .iter()
        .map(|g| letter_size(chars, g))
        .fold(0.0, f32::max);
    if largest == 0.0 {
        return Vec::new();
    }
    let key = |g: &Vec<usize>| {
        (
            g.iter()
                .filter(|&&i| chars[i].text.is_alphanumeric())
                .count(),
            g.len(),
        )
    };
    // Python's `max` keeps the first of equal keys.
    let mut body: Option<&Vec<usize>> = None;
    for g in groups
        .iter()
        .filter(|g| letter_size(chars, g) >= BODY_SIZE_RATIO * largest)
    {
        if body.is_none_or(|b| key(g) > key(b)) {
            body = Some(g);
        }
    }
    let Some(body) = body else {
        return Vec::new();
    };
    let body_size = median(body.iter().map(|&i| chars[i].size).collect());
    let body_baseline = median(body.iter().map(|&i| chars[i].baseline).collect());
    let min_shift = MIN_SHIFT_POINTS.max(MIN_SHIFT * body_size);

    let mut shifted: HashMap<usize, Script> = HashMap::new();
    for &index in line {
        let c = &chars[index];
        let shift = c.baseline - body_baseline;
        // A script sits within its line: it is moved by less than its own
        // size. A drop cap or text from another line is moved further.
        if c.size <= MAX_SCRIPT_SIZE * body_size
            && min_shift <= shift.abs()
            && shift.abs() <= c.size
        {
            shifted.insert(
                index,
                if shift > 0.0 {
                    Script::Super
                } else {
                    Script::Sub
                },
            );
        }
    }

    let mut scripts = Vec::new();
    let mut start = 0;
    while start < line.len() {
        let script = shifted.get(&line[start]).copied();
        let mut end = start + 1;
        while end < line.len() && shifted.get(&line[end]).copied() == script {
            end += 1;
        }
        let run = &line[start..end];
        if let Some(script) = script {
            if keep_run(chars, run, start, line, &shifted, body_size) {
                scripts.extend(run.iter().map(|&i| (i, script)));
            }
        }
        start = end;
    }
    scripts
}

/// Tell a script from other small, shifted text on the line. `position` is
/// the run's start in `line`.
fn keep_run(
    chars: &[ScriptChar],
    run: &[usize],
    position: usize,
    line: &[usize],
    shifted: &HashMap<usize, Script>,
    body_size: f32,
) -> bool {
    let neighbours: Vec<usize> = [
        position.checked_sub(1).map(|p| line[p]),
        line.get(position + run.len()).copied(),
    ]
    .into_iter()
    .flatten()
    .collect();
    let distances: Vec<f32> = neighbours
        .iter()
        .map(|&k| distance(&chars[k], chars, run))
        .collect();

    // It touches the text it marks, unlike a small page number or URL
    // further along.
    let touches_body = neighbours
        .iter()
        .zip(&distances)
        .any(|(k, &d)| !shifted.contains_key(k) && d <= TOUCH_GAP * body_size);
    // A subscript and a superscript stacked over one base.
    let own = shifted.get(&run[0]);
    let stacked = neighbours
        .iter()
        .any(|k| shifted.get(k).is_some_and(|s| Some(s) != own));
    if !(touches_body || stacked) {
        return false;
    }

    // Symbol fonts (arrows, bullets) are often drawn small and off the
    // baseline.
    let text: Vec<char> = run.iter().map(|&i| chars[i].text).collect();
    if !(text.iter().any(|c| c.is_alphanumeric())
        || text.iter().all(|c| FOOTNOTE_MARKS.contains(c)))
    {
        return false;
    }

    let in_word = neighbours.len() == 2
        && text.iter().all(|c| c.is_alphabetic())
        && neighbours.iter().zip(&distances).all(|(k, &d)| {
            !shifted.contains_key(k)
                && chars[*k].text.is_alphabetic()
                && d <= IN_WORD_GAP * body_size
        });
    !in_word
}

/// The horizontal distance between a character and a run, 0 if they overlap.
fn distance(c: &ScriptChar, chars: &[ScriptChar], run: &[usize]) -> f32 {
    let left = run.iter().map(|&i| chars[i].left).fold(f32::MAX, f32::min);
    let right = run.iter().map(|&i| chars[i].right).fold(f32::MIN, f32::max);
    0f32.max(c.left - right).max(left - c.right)
}

/// Whether the PDF shows a space between two characters, or a line break.
fn is_word_gap(left: &ScriptChar, right: &ScriptChar) -> bool {
    let size = left.size.max(right.size);
    if (right.baseline - left.baseline).abs() > LINE_JUMP * size || right.left < left.left {
        return true;
    }
    right.left - left.right >= MIN_WORD_GAP * size
}

/// The fold docling applies to punctuation before matching.
fn fold_punctuation(c: char) -> char {
    match c {
        '\u{2018}' | '\u{2019}' | '\u{201A}' | '\u{2032}' => '\'',
        '\u{201C}' | '\u{201D}' | '\u{201E}' | '\u{2033}' => '"',
        '\u{2010}' | '\u{2011}' | '\u{2012}' | '\u{2013}' | '\u{2014}' | '\u{2212}' => '-',
        c => c,
    }
}

/// The non-space characters folded for matching, each with its index in
/// `text`. NFKC unfolds ligatures into several characters.
fn folded(text: &[char]) -> Vec<(char, usize)> {
    let mut out = Vec::new();
    for (index, &ch) in text.iter().enumerate() {
        if is_space(ch) {
            continue;
        }
        for part in std::iter::once(ch).nfkc().map(fold_punctuation) {
            if !is_space(part) {
                out.push((part, index));
            }
        }
    }
    out
}

/// Map the characters of `text` to PDF character indexes, `None` if too few
/// match.
fn align(text: &[char], chars: &[ScriptChar]) -> Option<HashMap<usize, usize>> {
    let ours = folded(text);
    let joined: Vec<char> = chars.iter().map(|c| c.text).collect();
    let theirs = folded(&joined);
    if ours.is_empty() {
        return None;
    }
    let a: Vec<char> = ours.iter().map(|p| p.0).collect();
    let b: Vec<char> = theirs.iter().map(|p| p.0).collect();
    let blocks = if a == b {
        vec![(0, 0, a.len())]
    } else if a.len().max(b.len()) > MAX_ALIGN_CHARS {
        return None;
    } else {
        matching_blocks(&a, &b)
    };
    let mut mapping: HashMap<usize, usize> = HashMap::new();
    for (i, j, size) in blocks {
        for k in 0..size {
            mapping.entry(ours[i + k].1).or_insert(theirs[j + k].1);
        }
    }
    let solid = text.iter().filter(|c| !is_space(**c)).count();
    ((mapping.len() as f32) >= MIN_ALIGNED * solid as f32).then_some(mapping)
}

/// difflib's `SequenceMatcher(None, a, b, autojunk=False).get_matching_blocks()`.
fn matching_blocks(a: &[char], b: &[char]) -> Vec<(usize, usize, usize)> {
    let mut b2j: HashMap<char, Vec<usize>> = HashMap::new();
    for (j, &c) in b.iter().enumerate() {
        b2j.entry(c).or_default().push(j);
    }
    let longest = |alo: usize, ahi: usize, blo: usize, bhi: usize| {
        let (mut besti, mut bestj, mut bestsize) = (alo, blo, 0usize);
        let mut j2len: HashMap<usize, usize> = HashMap::new();
        for (i, c) in a.iter().enumerate().take(ahi).skip(alo) {
            let mut next: HashMap<usize, usize> = HashMap::new();
            for &j in b2j.get(c).map_or(&[][..], Vec::as_slice) {
                if j < blo {
                    continue;
                }
                if j >= bhi {
                    break;
                }
                let k = j
                    .checked_sub(1)
                    .and_then(|p| j2len.get(&p))
                    .copied()
                    .unwrap_or(0)
                    + 1;
                next.insert(j, k);
                if k > bestsize {
                    (besti, bestj, bestsize) = (i + 1 - k, j + 1 - k, k);
                }
            }
            j2len = next;
        }
        while besti > alo && bestj > blo && a[besti - 1] == b[bestj - 1] {
            (besti, bestj, bestsize) = (besti - 1, bestj - 1, bestsize + 1);
        }
        while besti + bestsize < ahi
            && bestj + bestsize < bhi
            && a[besti + bestsize] == b[bestj + bestsize]
        {
            bestsize += 1;
        }
        (besti, bestj, bestsize)
    };
    let mut queue = vec![(0, a.len(), 0, b.len())];
    let mut blocks = Vec::new();
    while let Some((alo, ahi, blo, bhi)) = queue.pop() {
        let (i, j, k) = longest(alo, ahi, blo, bhi);
        if k > 0 {
            blocks.push((i, j, k));
            if alo < i && blo < j {
                queue.push((alo, i, blo, j));
            }
            if i + k < ahi && j + k < bhi {
                queue.push((i + k, ahi, j + k, bhi));
            }
        }
    }
    blocks.sort_unstable();
    let mut out = Vec::new();
    let (mut i1, mut j1, mut k1) = (0, 0, 0);
    for (i2, j2, k2) in blocks {
        if i1 + k1 == i2 && j1 + k1 == j2 {
            k1 += k2;
        } else {
            if k1 > 0 {
                out.push((i1, j1, k1));
            }
            (i1, j1, k1) = (i2, j2, k2);
        }
    }
    if k1 > 0 {
        out.push((i1, j1, k1));
    }
    out
}

/// Slack (points) when testing whether a character's centre lies in an
/// item's box — docling's `_BOX_TOLERANCE`, plus one step of the 0–511 grid
/// the box is kept on here.
const BOX_TOLERANCE: f32 = 0.5;

/// The opt-in script-position stage (#1005, docling's `ScriptPositionModel`,
/// run after the heading hierarchy): every paragraph and footnote with
/// layout provenance whose characters hold a superscript or subscript
/// becomes a [`Node::ScriptText`] of its runs. Headings, captions, tables,
/// code, formulas, list items and OCR'd text are left as they are (OCR'd
/// text has no characters in the text layer to measure). A page the parser
/// cannot read keeps its items.
///
/// List items are the one gap against docling, which also rewrites them:
/// our list item carries its own location and is grouped into its list by
/// the serializers, and a wrapper would split the list.
pub(crate) fn apply(nodes: &mut [Node], bytes: &[u8]) {
    let Some(mut parser) = crate::textparse::PageTextParser::open(bytes) else {
        return;
    };
    let mut pages: HashMap<usize, Vec<(ScriptChar, [f32; 4])>> = HashMap::new();
    let (mut page_no, mut page_w, mut page_h) = (0usize, 0f32, 0f32);
    for node in nodes.iter_mut() {
        match node {
            Node::PageInfo {
                page_no: p,
                width,
                height,
            } => (page_no, page_w, page_h) = (*p, *width, *height),
            Node::Located { location, inner } if page_no > 0 && page_w > 0.0 => {
                let Some(text) = candidate_text(inner) else {
                    continue;
                };
                let chars = pages
                    .entry(page_no)
                    .or_insert_with(|| parser.script_chars(page_no - 1));
                let page = Page {
                    no: page_no,
                    w: page_w,
                    h: page_h,
                    chars,
                };
                if let Some(runs) = rewrite(&text, *location, &page) {
                    let plain = std::mem::replace(inner.as_mut(), Node::PageBreak);
                    **inner = marked(plain, runs);
                }
            }
            _ => {}
        }
    }
}

/// One page's characters with its display size.
struct Page<'a> {
    no: usize,
    w: f32,
    h: f32,
    chars: &'a [(ScriptChar, [f32; 4])],
}

/// The raw text of a node the stage may rewrite: a body paragraph (not a
/// display formula) or a footnote.
fn candidate_text(node: &Node) -> Option<String> {
    let text = match node {
        Node::Paragraph { text } => text,
        Node::LabeledText { label, text, .. } if label == "footnote" => text,
        _ => return None,
    };
    let t = text.trim();
    // A display formula, or text carrying Markdown link markup (a strict
    // export's hyperlinks), whose markup the PDF characters do not show.
    if t.is_empty() || (t.starts_with("$$") && t.ends_with("$$")) || t.contains("](") {
        return None;
    }
    Some(unescape(text))
}

/// [`crate::assemble::md_escape`] undone.
fn unescape(text: &str) -> String {
    text.replace("&gt;", ">")
        .replace("&lt;", "<")
        .replace("&amp;", "&")
        .replace("\\_", "_")
}

/// The runs of the item at `location` (0–511 grid, top-left) on `page`, each
/// with its provenance — `None` when it holds no script or its text does not
/// match its characters.
fn rewrite(
    text: &str,
    location: [u16; 4],
    page: &Page<'_>,
) -> Option<Vec<docling_core::ScriptRun>> {
    let grid = |v: u16, side: f32| f32::from(v) / 512.0 * side;
    let (l, t, r, b) = (
        grid(location[0], page.w),
        grid(location[1], page.h),
        grid(location[2], page.w),
        grid(location[3], page.h),
    );
    let (tx, ty) = (
        BOX_TOLERANCE + page.w / 512.0,
        BOX_TOLERANCE + page.h / 512.0,
    );
    let inside: Vec<&(ScriptChar, [f32; 4])> = page
        .chars
        .iter()
        .filter(|(_, [cl, ct, cr, cb])| {
            let (x, y) = ((cl + cr) / 2.0, (ct + cb) / 2.0);
            x >= l - tx && x <= r + tx && y >= t - ty && y <= b + ty
        })
        .collect();
    let chars: Vec<ScriptChar> = inside.iter().map(|(c, _)| c.clone()).collect();
    // Cheap check first: matching the text to the characters costs more.
    if script_positions(&chars)
        .iter()
        .all(|s| *s == Script::Baseline)
    {
        return None;
    }
    let runs = script_runs(text, &chars)?;
    Some(
        runs.into_iter()
            .map(|run| {
                let len = run.text.chars().count();
                // The box of the run's own characters, bottom-left origin as
                // docling's provenance; the item's box when none matched.
                let boxes: Vec<[f32; 4]> = run.chars.iter().map(|&i| inside[i].1).collect();
                let [bl, bt, br, bb] = if boxes.is_empty() {
                    [l, t, r, b]
                } else {
                    boxes.iter().fold(
                        [f32::MAX, f32::MAX, f32::MIN, f32::MIN],
                        |[l, t, r, b], &[cl, ct, cr, cb]| {
                            [l.min(cl), t.min(ct), r.max(cr), b.max(cb)]
                        },
                    )
                };
                docling_core::ScriptRun {
                    text: run.text,
                    script: run.script,
                    prov: vec![docling_core::ScriptRunProv {
                        page_no: page.no,
                        bbox: [bl, page.h - bt, br, page.h - bb],
                        charspan: [0, len],
                    }],
                    hyperlink: None,
                }
            })
            .collect(),
    )
}

/// The item `plain` holding `runs`: its text is the runs' joined text (a
/// footnote's link on its baseline runs, so a marker stays out of it).
fn marked(plain: Node, mut runs: Vec<docling_core::ScriptRun>) -> Node {
    let link = match &plain {
        Node::LabeledText { href, .. } => href.clone(),
        _ => None,
    };
    let mut md = String::new();
    for run in &mut runs {
        let text = crate::assemble::md_escape(&run.text);
        match (&link, run.script) {
            (Some(url), Script::Baseline) => {
                run.hyperlink = Some(url.clone());
                let trimmed = text.trim_start();
                md.push_str(&text[..text.len() - trimmed.len()]);
                md.push_str(&docling_core::hyperlink::markdown_link(trimmed, url));
            }
            _ => md.push_str(&text),
        }
    }
    let inner = match plain {
        Node::LabeledText { label, .. } => Node::LabeledText {
            label,
            text: md,
            href: None,
        },
        _ => Node::Paragraph { text: md },
    };
    Node::ScriptText {
        inner: Box::new(inner),
        runs,
    }
}

#[cfg(test)]
mod tests {
    //! docling's `tests/test_script_position.py`, the geometry half.
    use super::*;
    use Script::{Baseline as B, Sub as SUB, Super as SUP};

    /// Lay out (text, size, rise) segments left to right on one baseline.
    fn line_at(segments: &[(&str, f32, f32)], y: f32, x: f32) -> Vec<ScriptChar> {
        let mut x = x;
        let mut chars = Vec::new();
        for &(text, size, rise) in segments {
            for ch in text.chars() {
                let width = 0.5 * size;
                chars.push(ScriptChar {
                    text: ch,
                    left: x,
                    bottom: y + rise - 0.2 * size,
                    right: x + width,
                    top: y + rise + 0.8 * size,
                    baseline: y + rise,
                    size,
                    horizontal: true,
                });
                x += width;
            }
        }
        chars
    }

    fn line(segments: &[(&str, f32, f32)]) -> Vec<ScriptChar> {
        line_at(segments, 700.0, 50.0)
    }

    fn runs(text: &str, chars: &[ScriptChar]) -> Vec<(String, Script)> {
        script_runs(text, chars)
            .expect("runs")
            .into_iter()
            .map(|r| (r.text, r.script))
            .collect()
    }

    fn r(text: &str, s: Script) -> (String, Script) {
        (text.to_string(), s)
    }

    /// The positions found as `<super>`/`<sub>` markup, spaces ignored.
    fn marked(chars: &[ScriptChar]) -> String {
        let mut out = String::new();
        for (c, s) in chars.iter().zip(script_positions(chars)) {
            if is_space(c.text) {
                continue;
            }
            match s {
                B => out.push(c.text),
                SUP => out.push_str(&format!("<super>{}</super>", c.text)),
                SUB => out.push_str(&format!("<sub>{}</sub>", c.text)),
            }
        }
        out.replace("</super><super>", "")
            .replace("</sub><sub>", "")
    }

    /// One line's `(text, size, rise)` segments.
    type Segments<'a> = &'a [(&'a str, f32, f32)];

    #[test]
    fn marks_scripts() {
        let cases: &[(Segments, &str)] = &[
            (
                &[("E = mc", 12.0, 0.0), ("2", 7.0, 4.5)],
                "E=mc<super>2</super>",
            ),
            (
                &[("H", 12.0, 0.0), ("2", 7.0, -2.5), ("O", 12.0, 0.0)],
                "H<sub>2</sub>O",
            ),
            // Word lowers a subscript by under a tenth of the font size.
            (
                &[("CO", 12.0, 0.0), ("2", 7.0, -1.1), (" gas", 12.0, 0.0)],
                "CO<sub>2</sub>gas",
            ),
            (
                &[
                    ("the 1", 12.0, 0.0),
                    ("st", 7.0, 4.5),
                    (" of May", 12.0, 0.0),
                ],
                "the1<super>st</super>ofMay",
            ),
            (
                &[
                    ("as shown", 12.0, 0.0),
                    ("1,2\u{2013}4", 7.0, 4.5),
                    (". Next", 12.0, 0.0),
                ],
                "asshown<super>1,2\u{2013}4</super>.Next",
            ),
            (
                &[("3", 6.0, 3.0), ("This footnote", 9.0, 0.0)],
                "<super>3</super>Thisfootnote",
            ),
            (
                &[("Author", 12.0, 0.0), ("*", 7.0, 4.5), (" and", 12.0, 0.0)],
                "Author<super>*</super>and",
            ),
            // The last line of a paragraph can hold fewer letters than its
            // citation.
            (
                &[("al.", 12.0, 0.0), ("[12]", 7.0, 4.5)],
                "al.<super>[12]</super>",
            ),
        ];
        for (segments, want) in cases {
            assert_eq!(marked(&line(segments)), *want);
        }
    }

    #[test]
    fn leaves_other_text_alone() {
        let cases: &[Segments] = &[
            // Drawn high but full size: not a script.
            &[("grew 12% at 25\u{b0}C", 12.0, 0.0)],
            // Smaller but on the baseline.
            &[
                ("S", 12.0, 0.0),
                ("MALL ", 9.0, 0.0),
                ("C", 12.0, 0.0),
                ("APS", 9.0, 0.0),
            ],
            // Shifted but full size.
            &[
                ("baseline ", 12.0, 0.0),
                ("jitter", 12.0, 1.5),
                (" here", 12.0, 0.0),
            ],
            // A symbol font drawn small and raised.
            &[
                ("Concepts", 12.0, 0.0),
                ("\u{2192}", 7.0, 2.0),
                ("Docs", 12.0, 0.0),
            ],
            // The raised A of the LaTeX logo sits inside the word.
            &[("L", 12.0, 0.0), ("A", 8.0, 3.0), ("TeX", 12.0, 0.0)],
            // A drop cap is set larger and lower than the line it starts.
            &[("T", 36.0, -24.0), ("his line goes on", 12.0, 0.0)],
            // A large symbol does not set the size of the text around it.
            &[
                ("sum ", 10.0, 0.0),
                ("\u{2211}", 18.0, -3.0),
                (" of x", 10.0, 0.0),
            ],
        ];
        for segments in cases {
            let chars = line(segments);
            assert!(
                script_positions(&chars).iter().all(|s| *s == B),
                "{segments:?}"
            );
        }
    }

    #[test]
    fn small_text_away_from_body_is_not_a_script() {
        let mut chars = line(&[("Report 2024", 12.0, 0.0)]);
        chars.extend(line_at(&[("page 3", 7.0, 4.5)], 700.0, 400.0));
        assert!(script_positions(&chars).iter().all(|s| *s == B));
    }

    #[test]
    fn measures_each_line_against_its_own_baseline() {
        let mut chars = line_at(&[("x", 12.0, 0.0), ("2", 7.0, 4.5)], 700.0, 50.0);
        chars.extend(line_at(
            &[("H", 12.0, 0.0), ("2", 7.0, -2.5), ("O", 12.0, 0.0)],
            680.0,
            50.0,
        ));
        assert_eq!(marked(&chars), "x<super>2</super>H<sub>2</sub>O");
    }

    #[test]
    fn stacked_sub_and_superscript() {
        for subscript in ["i", "ijk"] {
            let mut chars = line(&[("x", 12.0, 0.0), (subscript, 7.0, -2.5)]);
            // The superscript is drawn back over the start of the subscript,
            // however wide.
            let x = chars[1].left;
            chars.extend(line_at(&[("2", 7.0, 4.5)], 700.0, x));
            let mut want = vec![B];
            want.extend(std::iter::repeat_n(SUB, subscript.len()));
            want.push(SUP);
            assert_eq!(script_positions(&chars), want, "{subscript}");
        }
    }

    #[test]
    fn footnote_marker_starting_a_tightly_set_line() {
        // Lines 9 points apart: the raised marker is less than a line below
        // the line above.
        let mut chars = line_at(&[("the line above it", 9.0, 0.0)], 700.0, 50.0);
        chars.extend(line_at(
            &[("3", 6.0, 3.0), ("Note text", 9.0, 0.0)],
            691.0,
            50.0,
        ));
        assert_eq!(marked(&chars), "thelineaboveit<super>3</super>Notetext");
    }

    #[test]
    fn runs_keep_the_spaces_the_pdf_shows() {
        // The parser reports a space before the raised "2", where the
        // characters touch.
        let chars = line(&[
            ("E = mc", 12.0, 0.0),
            ("2", 7.0, 4.5),
            (" in 1905.", 12.0, 0.0),
        ]);
        let text = " E = mc 2 in 1905. ";
        assert_eq!(
            runs(text, &chars),
            [r("E = mc", B), r("2", SUP), r(" in 1905.", B)]
        );
        // The offsets count the space that a run starts with.
        for run in script_runs(text, &chars).unwrap() {
            let t: Vec<char> = run.text.chars().collect();
            for (&index, &offset) in run.chars.iter().zip(&run.offsets) {
                assert_eq!(t[offset], chars[index].text);
            }
        }
    }

    #[test]
    fn space_beside_a_script_needs_a_word_gap() {
        for (gap, after) in [(0.09, "in"), (0.11, " in")] {
            let mut chars = line(&[("mc", 12.0, 0.0), ("2", 7.0, 4.5)]);
            let x = chars.last().unwrap().right + gap * 12.0;
            chars.extend(line_at(&[("in", 12.0, 0.0)], 700.0, x));
            assert_eq!(
                runs("mc 2 in", &chars),
                [r("mc", B), r("2", SUP), r(after, B)]
            );
        }
    }

    #[test]
    fn space_is_kept_at_a_line_break() {
        let mut chars = line_at(&[("x", 12.0, 0.0), ("2", 7.0, 4.5)], 700.0, 50.0);
        chars.extend(line_at(&[("more", 12.0, 0.0)], 680.0, 50.0));
        assert_eq!(
            runs("x 2 more", &chars),
            [r("x", B), r("2", SUP), r(" more", B)]
        );
    }

    #[test]
    fn space_is_kept_beside_an_unmatched_character() {
        // The gap cannot be measured to the "Q", which is not among the PDF
        // characters.
        let chars = line(&[("abcde", 12.0, 0.0), ("2", 7.0, 4.5), ("fgh", 12.0, 0.0)]);
        assert_eq!(
            runs("abcde 2 Q fgh", &chars),
            [r("abcde", B), r("2", SUP), r(" Q fgh", B)]
        );
    }

    #[test]
    fn runs_never_add_a_space() {
        let chars = line(&[("H", 12.0, 0.0), ("2", 7.0, -2.5), ("  O", 12.0, 0.0)]);
        assert_eq!(runs("H2O", &chars), [r("H", B), r("2", SUB), r("O", B)]);
    }

    #[test]
    fn runs_match_through_ligatures_and_dashes() {
        let chars = line(&[("\u{fb01}rst", 12.0, 0.0), ("1\u{2013}2", 7.0, 4.5)]);
        assert_eq!(runs("first1-2", &chars), [r("first", B), r("1-2", SUP)]);
    }

    #[test]
    fn runs_point_to_their_characters() {
        let chars = line(&[("H", 12.0, 0.0), ("2", 7.0, -2.5), ("O", 12.0, 0.0)]);
        let got: Vec<Vec<usize>> = script_runs("H2O", &chars)
            .unwrap()
            .into_iter()
            .map(|r| r.chars)
            .collect();
        assert_eq!(got, [vec![0], vec![1], vec![2]]);
    }

    #[test]
    fn no_runs_without_scripts_or_when_text_does_not_match() {
        let plain = line(&[("plain text", 12.0, 0.0)]);
        assert_eq!(script_runs("plain text", &plain), None);
        let chars = line(&[("E = mc", 12.0, 0.0), ("2", 7.0, 4.5)]);
        assert_eq!(script_runs("something else entirely", &chars), None);
    }

    #[test]
    fn runs_ignore_characters_of_a_neighbouring_line() {
        // The item's box also catches a raised character of the line below it.
        let mut chars = line_at(&[("E = mc", 12.0, 0.0), ("2", 7.0, 4.5)], 700.0, 50.0);
        chars.extend(line_at(&[("x", 12.0, 0.0), ("3", 7.0, 4.5)], 686.0, 50.0));
        assert_eq!(runs("E = mc 2", &chars), [r("E = mc", B), r("2", SUP)]);
    }

    /// difflib's matching blocks on a classic pair.
    #[test]
    fn matching_blocks_follow_difflib() {
        let a: Vec<char> = "abxcd".chars().collect();
        let b: Vec<char> = "abcd".chars().collect();
        assert_eq!(matching_blocks(&a, &b), [(0, 0, 2), (3, 2, 2)]);
        let a: Vec<char> = "qabxcd".chars().collect();
        let b: Vec<char> = "abycdf".chars().collect();
        assert_eq!(matching_blocks(&a, &b), [(1, 0, 2), (4, 3, 2)]);
    }
}
