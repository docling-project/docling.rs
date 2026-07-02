//! Splitting Markdown into heading-bounded sections of plain words.

use pulldown_cmark::{Event, HeadingLevel, Parser, Tag, TagEnd};

/// A contiguous run of body text under a heading path.
#[derive(Debug, Clone, Default)]
pub struct Section {
    /// The heading stack in effect for this section, outermost first
    /// (e.g. `["Guide", "Setup"]`). Empty for pre-heading / body-only text.
    pub heading_path: Vec<String>,
    /// The plain words of the section body, markup stripped.
    pub words: Vec<String>,
}

impl Section {
    /// The heading path rendered as a single context line, e.g. `# Guide > Setup`.
    /// Empty string when there is no heading.
    pub fn heading_context(&self) -> String {
        if self.heading_path.is_empty() {
            String::new()
        } else {
            format!("# {}", self.heading_path.join(" > "))
        }
    }
}

fn level_index(level: HeadingLevel) -> usize {
    match level {
        HeadingLevel::H1 => 1,
        HeadingLevel::H2 => 2,
        HeadingLevel::H3 => 3,
        HeadingLevel::H4 => 4,
        HeadingLevel::H5 => 5,
        HeadingLevel::H6 => 6,
    }
}

/// Parse Markdown into sections. A new section starts at every heading; the
/// heading path is maintained as a stack keyed by heading level.
pub fn parse_sections(markdown: &str) -> Vec<Section> {
    // heading_stack[i] holds the current heading text at level i+1 (may be empty).
    let mut heading_stack: Vec<String> = Vec::new();
    let mut sections: Vec<Section> = Vec::new();
    let mut current = Section::default();

    let mut in_heading = false;
    let mut heading_level = 0usize;
    let mut heading_buf = String::new();

    let push_words = |section: &mut Section, text: &str| {
        for w in text.split_whitespace() {
            section.words.push(w.to_string());
        }
    };

    let flush = |sections: &mut Vec<Section>, section: &mut Section| {
        if !section.words.is_empty() {
            sections.push(std::mem::take(section));
        } else {
            *section = Section::default();
        }
    };

    for event in Parser::new(markdown) {
        match event {
            Event::Start(Tag::Heading { level, .. }) => {
                in_heading = true;
                heading_level = level_index(level);
                heading_buf.clear();
            }
            Event::End(TagEnd::Heading(_)) => {
                in_heading = false;
                // Update the heading stack: set this level, drop anything deeper.
                let idx = heading_level.saturating_sub(1);
                if heading_stack.len() <= idx {
                    heading_stack.resize(idx + 1, String::new());
                } else {
                    heading_stack.truncate(idx + 1);
                }
                heading_stack[idx] = heading_buf.trim().to_string();
                // A heading begins a new section.
                flush(&mut sections, &mut current);
                current.heading_path = heading_stack
                    .iter()
                    .filter(|h| !h.is_empty())
                    .cloned()
                    .collect();
            }
            Event::Text(t) | Event::Code(t) => {
                if in_heading {
                    if !heading_buf.is_empty() {
                        heading_buf.push(' ');
                    }
                    heading_buf.push_str(&t);
                } else {
                    push_words(&mut current, &t);
                }
            }
            // Treat hard/soft breaks and rules as whitespace (words already split).
            Event::SoftBreak | Event::HardBreak | Event::Rule => {}
            _ => {}
        }
    }
    flush(&mut sections, &mut current);
    sections
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_on_headings_and_tracks_path() {
        let md = "\
intro words
# Chapter 1
para one
## Section 1.1
para two
# Chapter 2
para three";
        let secs = parse_sections(md);
        // pre-heading intro, Chapter 1, Section 1.1, Chapter 2.
        assert_eq!(secs.len(), 4);
        assert!(secs[0].heading_path.is_empty());
        assert_eq!(secs[1].heading_path, vec!["Chapter 1"]);
        assert_eq!(secs[2].heading_path, vec!["Chapter 1", "Section 1.1"]);
        // A deeper heading is dropped when we return to H1.
        assert_eq!(secs[3].heading_path, vec!["Chapter 2"]);
    }

    #[test]
    fn strips_markup_to_plain_words() {
        let md = "# T\n\nSome **bold** and `code` and [a link](http://x).";
        let secs = parse_sections(md);
        let words = &secs[0].words;
        assert!(words.contains(&"bold".to_string()));
        assert!(words.contains(&"code".to_string()));
        assert!(words.contains(&"link".to_string()));
        // No markdown punctuation survives as its own token.
        assert!(!words.iter().any(|w| w.contains('*') || w.contains('`')));
    }
}
