//! docling-core's inline whitespace contract (schema 1.11.0, docling-core#693,
//! docling#3891).
//!
//! The runs of an `inline` group carry their own boundary whitespace, and the
//! Markdown, HTML and LaTeX serializers concatenate them as they are. Before
//! the contract every serializer joined runs with a hard `" "`, inventing
//! spaces the source never had — `2nd` → `2 nd`, `H₂O.` → `H 2 O .`,
//! `**bold** .` — which no backend could undo while the separator lived in the
//! serializer. Three helpers carry the contract:
//!
//! * [`hoist`] — docling-core's `post_process` with
//!   `hoist_decoration_whitespace`: a run's edge whitespace goes *outside* its
//!   whole decoration stack (formatting and hyperlink together), so
//!   `"bold "` renders `**bold** ` and never `**bold **`, which CommonMark
//!   prints as literal asterisks; a whitespace-only run is returned verbatim,
//!   undecorated and unlinked.
//! * [`trim_run_edges`] — docling's `trim_run_edges` (`docling/utils/_inline_runs.py`):
//!   a backend that keeps each run's whitespace trims only the block's outer
//!   edges.
//! * [`separate_legacy_runs`] — docling-core's
//!   `_migrate_legacy_inline_separators`: the old `" "` join re-injected into
//!   the runs themselves, for a document stamped below 1.11.0 and for an item
//!   tree whose backend still builds trimmed runs ([`crate::tree::ItemTree::inline_whitespace`]
//!   unset), so their visible output — extra-space bugs included — is what it
//!   was.

/// Python's `str.isspace` set, which docling-core's `strip()` removes: Unicode
/// `White_Space` plus the ASCII information separators U+001C–U+001F.
pub fn is_space(c: char) -> bool {
    c.is_whitespace() || ('\u{1c}'..='\u{1f}').contains(&c)
}

/// `text` split into its leading whitespace, its core and its trailing
/// whitespace (Python's `lstrip` / `strip` / `rstrip` boundaries). A
/// whitespace-only text is all lead.
pub fn split_edges(text: &str) -> (&str, &str, &str) {
    let core_start = text.find(|c| !is_space(c)).unwrap_or(text.len());
    let rest = &text[core_start..];
    let core_len = rest
        .char_indices()
        .rev()
        .find(|&(_, c)| !is_space(c))
        .map_or(0, |(i, c)| i + c.len_utf8());
    (&text[..core_start], &rest[..core_len], &rest[core_len..])
}

/// docling-core's `post_process` under `hoist_decoration_whitespace`: decorate
/// the run's core and keep its edge whitespace outside the markers. A
/// whitespace-only (or empty) run comes back verbatim — no markers, no empty
/// hyperlink.
pub fn hoist(text: &str, decorate: impl FnOnce(&str) -> String) -> String {
    let (lead, core, trail) = split_edges(text);
    if core.is_empty() {
        return text.to_string();
    }
    format!("{lead}{}{trail}", decorate(core))
}

/// Whether `text` is empty or whitespace only.
pub fn is_blank(text: &str) -> bool {
    text.chars().all(is_space)
}

/// docling's `trim_run_edges`: drop the whitespace-only runs at either edge
/// of a block, strip the outer whitespace of the first and last remaining
/// run, keep every interior run as it is, and drop trimmable runs left
/// without text. A run `trimmable` refuses (a line break, an image) stops the
/// trim at its edge and is always kept.
pub fn trim_run_edges<T>(
    mut runs: Vec<T>,
    text: impl Fn(&T) -> &str,
    set_text: impl Fn(&mut T, String),
    trimmable: impl Fn(&T) -> bool,
) -> Vec<T> {
    let mut start = 0;
    while start < runs.len() && trimmable(&runs[start]) {
        let head = text(&runs[start]).trim_start_matches(is_space).to_string();
        if !head.is_empty() {
            set_text(&mut runs[start], head);
            break;
        }
        start += 1;
    }
    let mut end = runs.len();
    while end > start && trimmable(&runs[end - 1]) {
        let tail = text(&runs[end - 1]).trim_end_matches(is_space).to_string();
        if !tail.is_empty() {
            set_text(&mut runs[end - 1], tail);
            break;
        }
        end -= 1;
    }
    runs.truncate(end);
    runs.drain(..start);
    runs.retain(|r| !text(r).is_empty() || !trimmable(r));
    runs
}

/// One child of an inline group, as [`separate_legacy_runs`] sees it.
pub struct LegacyRun<'a> {
    /// The run's own text (empty for a nested group or an empty host).
    pub text: &'a mut String,
    /// Whether the run or its subtree renders any text — docling-core's
    /// `renders_text`; a run that does not takes no part in the join.
    pub renders_text: bool,
    /// A plain text run (not code, not a formula, and with no children of its
    /// own): only such a run with text takes the separator.
    pub plain: bool,
    /// An empty plain `text` run without children — the separator a 1.11 →
    /// 1.10 projection stripped, refilled instead of inserting another.
    pub empty_plain: bool,
}

/// docling-core's `_migrate_legacy_inline_separators`, for one inline group:
/// one `" "` at every boundary the old serializer joined. Between two
/// consecutive runs that render text the left run takes a trailing space when
/// it is a plain run with text, else the right one a leading space, else an
/// empty plain run between them is refilled, else — both neighbours code or
/// formula runs, where a space would land inside `` ` ``/`$` — a standalone
/// `" "` run is needed. Returns the child positions such runs go in front of,
/// in ascending order (the caller inserts them, shifting as it goes).
pub fn separate_legacy_runs(runs: &mut [LegacyRun<'_>]) -> Vec<usize> {
    let rendered: Vec<usize> = (0..runs.len()).filter(|&i| runs[i].renders_text).collect();
    let mut standalone = Vec::new();
    for w in rendered.windows(2) {
        let (l, r) = (w[0], w[1]);
        let carries = |run: &LegacyRun<'_>| run.plain && !run.text.is_empty();
        if carries(&runs[l]) {
            runs[l].text.push(' ');
        } else if carries(&runs[r]) {
            runs[r].text.insert(0, ' ');
        } else if let Some(e) = (l + 1..r).find(|&i| runs[i].empty_plain) {
            runs[e].text.push(' ');
        } else {
            standalone.push(r);
        }
    }
    standalone
}

/// Whether a docling-JSON `version` predates the inline whitespace contract
/// (below 1.11.0). An unparsable or missing version is not migrated —
/// docling-core leaves versionless input alone rather than guess.
pub fn predates_contract(version: &str) -> bool {
    let mut it = version.split('.').map(|p| p.trim().parse::<u64>());
    match (it.next(), it.next()) {
        (Some(Ok(major)), Some(Ok(minor))) => (major, minor) < (1, 11),
        _ => false,
    }
}

/// docling-core's `_migrate_legacy_inline_separators` on a docling-JSON
/// document: when its `version` predates 1.11.0, every `inline` group's runs
/// get the `" "` the old join put between them ([`separate_legacy_runs`]),
/// a standalone separator run appended to `texts` (with the left run's
/// location, `charspan` `[0, 1]`, when both runs share a page) where both
/// neighbours are delimited. Returns whether anything changed. Only `text`
/// moves; `orig` keeps what the producer wrote.
pub fn migrate_legacy_json(doc: &mut serde_json::Value) -> bool {
    use serde_json::{json, Value};
    if !doc
        .get("version")
        .and_then(Value::as_str)
        .is_some_and(predates_contract)
    {
        return false;
    }
    fn locate(r: &Value) -> Option<(bool, usize)> {
        let r = r.get("$ref").or_else(|| r.get("cref"))?.as_str()?;
        if let Some(i) = r.strip_prefix("#/texts/") {
            return Some((true, i.parse().ok()?));
        }
        Some((false, r.strip_prefix("#/groups/")?.parse().ok()?))
    }
    fn get(doc: &Value, at: (bool, usize)) -> Option<&Value> {
        doc.get(if at.0 { "texts" } else { "groups" })?.get(at.1)
    }
    fn renders_text(doc: &Value, at: (bool, usize), seen: &mut Vec<(bool, usize)>) -> bool {
        if seen.contains(&at) {
            return false;
        }
        seen.push(at);
        let Some(item) = get(doc, at) else {
            return false;
        };
        if item
            .get("text")
            .and_then(Value::as_str)
            .is_some_and(|t| !t.is_empty())
        {
            return true;
        }
        item.get("children")
            .and_then(Value::as_array)
            .is_some_and(|cs| {
                cs.iter()
                    .filter_map(locate)
                    .collect::<Vec<_>>()
                    .into_iter()
                    .any(|c| renders_text(doc, c, seen))
            })
    }
    let n_groups = doc
        .get("groups")
        .and_then(Value::as_array)
        .map_or(0, Vec::len);
    if doc.get("texts").and_then(Value::as_array).is_none() {
        return false;
    }
    let mut migrated = false;
    for g in 0..n_groups {
        let group = &doc["groups"][g];
        if group.get("label").and_then(Value::as_str) != Some("inline") {
            continue;
        }
        let Some(children) = group.get("children").and_then(Value::as_array) else {
            continue;
        };
        let locs: Vec<Option<(bool, usize)>> = children.iter().map(locate).collect();
        // (renders_text, plain, empty_plain, text) per child.
        let mut info = Vec::with_capacity(locs.len());
        for &loc in &locs {
            let item = loc.and_then(|at| get(doc, at));
            let text = item
                .and_then(|i| i.get("text"))
                .and_then(Value::as_str)
                .map(str::to_string);
            let label = item.and_then(|i| i.get("label")).and_then(Value::as_str);
            let no_children = item
                .and_then(|i| i.get("children"))
                .and_then(Value::as_array)
                .is_none_or(Vec::is_empty);
            let renders = loc.is_some_and(|at| renders_text(doc, at, &mut Vec::new()));
            let plain = text.is_some() && !matches!(label, Some("code" | "formula"));
            let empty_plain = text.as_deref() == Some("") && label == Some("text") && no_children;
            info.push((renders, plain, empty_plain, text.unwrap_or_default()));
        }
        let mut texts: Vec<String> = info.iter().map(|i| i.3.clone()).collect();
        let standalone = {
            let mut runs: Vec<LegacyRun<'_>> = texts
                .iter_mut()
                .zip(&info)
                .map(|(text, i)| LegacyRun {
                    text,
                    renders_text: i.0,
                    plain: i.1,
                    empty_plain: i.2,
                })
                .collect();
            let before = runs.iter().map(|r| r.text.len()).sum::<usize>();
            let s = separate_legacy_runs(&mut runs);
            migrated |= !s.is_empty() || runs.iter().map(|r| r.text.len()).sum::<usize>() != before;
            s
        };
        for ((loc, new), i) in locs.iter().zip(texts).zip(&info) {
            if let Some(at) = *loc {
                if new != i.3 {
                    doc[if at.0 { "texts" } else { "groups" }][at.1]["text"] = Value::String(new);
                }
            }
        }
        let self_ref = doc["groups"][g]
            .get("self_ref")
            .cloned()
            .unwrap_or(Value::Null);
        let layer = doc["groups"][g]
            .get("content_layer")
            .cloned()
            .unwrap_or_else(|| json!("body"));
        for (shift, at) in standalone.into_iter().enumerate() {
            let prov_of = |loc: Option<(bool, usize)>, last: bool| {
                loc.and_then(|l| get(doc, l))
                    .and_then(|i| i.get("prov"))
                    .and_then(Value::as_array)
                    .and_then(|p| if last { p.last() } else { p.first() })
                    .cloned()
            };
            let (left, right) = (prov_of(locs[at - 1], true), prov_of(locs[at], false));
            let prov = match (left, right) {
                (Some(Value::Object(mut l)), Some(r))
                    if r.is_object() && l.get("page_no") == r.get("page_no") =>
                {
                    l.insert("charspan".into(), json!([0, 1]));
                    vec![Value::Object(l)]
                }
                _ => Vec::new(),
            };
            let texts = doc["texts"].as_array_mut().expect("checked above");
            let r = format!("#/texts/{}", texts.len());
            texts.push(json!({
                "self_ref": r,
                "parent": { "$ref": self_ref },
                "label": "text",
                "prov": prov,
                "orig": " ",
                "text": " ",
                "children": [],
                "content_layer": layer,
            }));
            if let Some(cs) = doc["groups"][g]["children"].as_array_mut() {
                cs.insert(at + shift, json!({ "$ref": r }));
            }
        }
    }
    migrated
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn edges_split_like_python_strip() {
        assert_eq!(split_edges("  bold \n"), ("  ", "bold", " \n"));
        assert_eq!(split_edges("x"), ("", "x", ""));
        assert_eq!(split_edges("   "), ("   ", "", ""));
        assert_eq!(split_edges(""), ("", "", ""));
        assert_eq!(split_edges("\u{a0}a b\u{1f}"), ("\u{a0}", "a b", "\u{1f}"));
    }

    #[test]
    fn hoisting_keeps_whitespace_outside_the_decoration() {
        let bold = |s: &str| format!("**{s}**");
        assert_eq!(hoist("bold ", bold), "**bold** ");
        assert_eq!(hoist(" x ", bold), " **x** ");
        assert_eq!(hoist("  ", bold), "  ");
        assert_eq!(hoist("", bold), "");
    }

    #[test]
    fn trim_run_edges_matches_docling() {
        let t = |v: &[&str]| {
            trim_run_edges(
                v.iter().map(|s| s.to_string()).collect(),
                |s: &String| s.as_str(),
                |s, n| *s = n,
                |s| s != "<br>",
            )
        };
        assert_eq!(t(&["  ", " a ", " b", " "]), ["a ", " b"]);
        assert_eq!(t(&[" a", "", "b "]), ["a", "b"]);
        assert_eq!(t(&["<br>", " a "]), ["<br>", " a"]);
        assert_eq!(t(&["   "]), Vec::<String>::new());
    }

    fn run(text: &mut String, plain: bool) -> LegacyRun<'_> {
        let renders_text = !text.is_empty();
        LegacyRun {
            empty_plain: plain && text.is_empty(),
            text,
            renders_text,
            plain,
        }
    }

    #[test]
    fn legacy_separators_land_outside_delimiters() {
        // Normal / italic / bold → `Normal ` `italic ` `bold`.
        let (mut a, mut b, mut c) = (
            "Normal".to_string(),
            "italic".to_string(),
            "bold".to_string(),
        );
        let ins =
            separate_legacy_runs(&mut [run(&mut a, true), run(&mut b, true), run(&mut c, true)]);
        assert!(ins.is_empty());
        assert_eq!(
            (a.as_str(), b.as_str(), c.as_str()),
            ("Normal ", "italic ", "bold")
        );
        // code, plain → the plain run takes a leading space.
        let (mut a, mut b) = ("x".to_string(), "y".to_string());
        separate_legacy_runs(&mut [run(&mut a, false), run(&mut b, true)]);
        assert_eq!((a.as_str(), b.as_str()), ("x", " y"));
        // code, empty plain, code → the empty run is refilled.
        let (mut a, mut e, mut b) = ("a".to_string(), String::new(), "b".to_string());
        let ins =
            separate_legacy_runs(&mut [run(&mut a, false), run(&mut e, true), run(&mut b, false)]);
        assert!(ins.is_empty());
        assert_eq!(e, " ");
        // code, code → a standalone run in front of the second.
        let (mut a, mut b) = ("a".to_string(), "b".to_string());
        assert_eq!(
            separate_legacy_runs(&mut [run(&mut a, false), run(&mut b, false)]),
            [1]
        );
    }

    #[test]
    fn a_pre_contract_document_gets_its_separators_back() {
        let mut doc = serde_json::json!({
            "version": "1.10.0",
            "texts": [
                {"self_ref": "#/texts/0", "label": "text", "text": "Normal", "orig": "Normal", "prov": []},
                {"self_ref": "#/texts/1", "label": "code", "text": "a", "orig": "a",
                 "prov": [{"page_no": 1, "charspan": [0, 1]}]},
                {"self_ref": "#/texts/2", "label": "code", "text": "b", "orig": "b",
                 "prov": [{"page_no": 1, "charspan": [0, 1]}]},
            ],
            "groups": [{"self_ref": "#/groups/0", "label": "inline", "content_layer": "body",
                        "children": [{"$ref": "#/texts/0"}, {"$ref": "#/texts/1"}, {"$ref": "#/texts/2"}]}],
        });
        assert!(migrate_legacy_json(&mut doc));
        assert_eq!(doc["texts"][0]["text"], "Normal ");
        assert_eq!(doc["texts"][0]["orig"], "Normal");
        assert_eq!(doc["texts"][3]["text"], " ");
        assert_eq!(
            doc["texts"][3]["prov"][0]["charspan"],
            serde_json::json!([0, 1])
        );
        assert_eq!(doc["groups"][0]["children"][2]["$ref"], "#/texts/3");
        // A current or versionless document is left alone.
        for v in [serde_json::json!("1.11.0"), serde_json::Value::Null] {
            let mut d = doc.clone();
            d["version"] = v;
            assert!(!migrate_legacy_json(&mut d));
        }
        assert!(predates_contract("1.9.2") && !predates_contract("2.0.0"));
    }
}
