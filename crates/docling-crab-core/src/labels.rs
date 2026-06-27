//! Document item labels.
//!
//! A subset of docling-core's `DocItemLabel`. The full enum is large; we grow
//! this as backends start emitting richer structure.

/// Semantic role of a document item, mirroring docling-core's `DocItemLabel`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DocItemLabel {
    Title,
    SectionHeader,
    Paragraph,
    Text,
    ListItem,
    Code,
    Formula,
    Caption,
    Footnote,
    Table,
    Picture,
    PageHeader,
    PageFooter,
}

impl DocItemLabel {
    /// The wire-format string used by docling-core's JSON serialization.
    pub fn as_str(self) -> &'static str {
        match self {
            DocItemLabel::Title => "title",
            DocItemLabel::SectionHeader => "section_header",
            DocItemLabel::Paragraph => "paragraph",
            DocItemLabel::Text => "text",
            DocItemLabel::ListItem => "list_item",
            DocItemLabel::Code => "code",
            DocItemLabel::Formula => "formula",
            DocItemLabel::Caption => "caption",
            DocItemLabel::Footnote => "footnote",
            DocItemLabel::Table => "table",
            DocItemLabel::Picture => "picture",
            DocItemLabel::PageHeader => "page_header",
            DocItemLabel::PageFooter => "page_footer",
        }
    }
}
