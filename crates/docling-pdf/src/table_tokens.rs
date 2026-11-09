//! The words TableFormer's cell matcher gets for a table on a page that mixes
//! a text layer with OCR (#999, docling#4928): a port of docling's
//! `_words_and_ocr_cells` / `_uncovered`
//! (`models/stages/table_structure/table_structure_model.py`).
//!
//! A scanned form with one typed-in value has a text layer of one word. The
//! table's OCR lines survive the OCR merge wherever no native text lies
//! under them, but the matcher used to see only the native words, so the
//! whole table collapsed to the one cell `420`. The tokens are the finest
//! cells of the text the merge kept: the native words no surviving OCR line
//! covers, the OCR engine's own word cells where it has them, and the OCR
//! lines no OCR word replaces.

use crate::pdfium_backend::TextCell;

/// Whether two boxes intersect, edges included — the R-tree query docling
/// runs (`BoundingBoxSpatialIndex.intersection`), which also reports boxes
/// that only touch.
fn touches(a: &TextCell, b: &TextCell) -> bool {
    a.l <= b.r && b.l <= a.r && a.t <= b.b && b.t <= a.b
}

/// The cells that overlap none of `covers` — docling's `_uncovered`.
pub(crate) fn uncovered<'a>(
    cells: impl IntoIterator<Item = &'a TextCell>,
    covers: &[TextCell],
) -> Vec<TextCell> {
    cells
        .into_iter()
        .filter(|c| !covers.iter().any(|v| touches(c, v)))
        .cloned()
        .collect()
}

/// A table's tokens — docling's `_words_and_ocr_cells`, with the word cells
/// split by origin (our cells carry no `from_ocr` flag):
///
/// - a native word under a surviving OCR line is dropped, as the merge
///   dropped its line;
/// - an OCR word on a kept native word is dropped, as the merge rejected its
///   line there; the other OCR words replace the OCR line they sit in.
///
/// The OCR engine here reports lines only, so `ocr_words` is empty on every
/// path today; the argument keeps the port whole.
pub(crate) fn words_and_ocr_cells(
    native_words: &[TextCell],
    ocr_words: &[TextCell],
    ocr_lines: &[TextCell],
) -> Vec<TextCell> {
    let native = uncovered(native_words, ocr_lines);
    let words = uncovered(ocr_words, &native);
    let lines = uncovered(ocr_lines, &words);
    native.into_iter().chain(words).chain(lines).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cell(l: f32, t: f32, r: f32, b: f32, text: &str) -> TextCell {
        TextCell {
            text: text.into(),
            l,
            t,
            r,
            b,
        }
    }

    const HEADER: (f32, f32, f32, f32) = (0.0, 0.0, 50.0, 10.0);
    const VALUE: (f32, f32, f32, f32) = (60.0, 20.0, 90.0, 30.0);

    fn at(b: (f32, f32, f32, f32), text: &str) -> TextCell {
        cell(b.0, b.1, b.2, b.3, text)
    }

    fn texts(cells: &[TextCell]) -> Vec<&str> {
        cells.iter().map(|c| c.text.as_str()).collect()
    }

    /// docling's `test_table_tokens_keep_ocr_lines_next_to_native_words`: a
    /// scanned form with one filled-in native value — the OCR header still
    /// reaches the table, and a native word under OCR text is not doubled.
    #[test]
    fn table_tokens_keep_ocr_lines_next_to_native_words() {
        let header = at(HEADER, "Week Begin");
        let filled_value = at(VALUE, "5000");
        let hidden_word = cell(5.0, 2.0, 20.0, 8.0, "Week");
        let tokens = words_and_ocr_cells(&[filled_value, hidden_word], &[], &[header]);
        assert_eq!(texts(&tokens), ["5000", "Week Begin"]);
    }

    /// docling's `test_table_tokens_prefer_ocr_words_over_their_line`: OCR
    /// word cells replace their OCR line, and the OCR word on the native
    /// value is dropped (the merge kept the native text there).
    #[test]
    fn table_tokens_prefer_ocr_words_over_their_line() {
        let header = at(HEADER, "Week Begin");
        let ocr_words = [
            cell(0.0, 0.0, 22.0, 10.0, "Week"),
            cell(26.0, 0.0, 50.0, 10.0, "Begin"),
            at(VALUE, "5000"),
        ];
        let native = [at(VALUE, "5000")];
        // The `5000` that comes first is the native word (docling's
        // `("5000", False)`): the OCR one on it is gone.
        let tokens = words_and_ocr_cells(&native, &ocr_words, &[header]);
        assert_eq!(texts(&tokens), ["5000", "Week", "Begin"]);
    }
}
