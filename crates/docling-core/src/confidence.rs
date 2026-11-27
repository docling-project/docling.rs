//! Conversion-confidence report — the Rust counterpart of docling's
//! `ConfidenceReport` / `PageConfidenceScores` (`docling.datamodel.base_models`,
//! surfaced per conversion by Python docling-serve v1.25+, #183).
//!
//! Semantics mirror docling exactly:
//!
//! - Four per-page scores, each in `[0, 1]` or *unset* (docling uses `NaN`;
//!   here `Option<f64>` so JSON serializes as `null` instead of an invalid
//!   `NaN` literal): `layout_score` (mean confidence of the kept layout
//!   clusters), `ocr_score` (mean confidence of OCR-recognized cells),
//!   `parse_score` (10th-percentile text-layer quality — the quantile
//!   emphasises problems), `table_score` (unset; docling never assigns it
//!   either, the field exists for wire compatibility).
//! - A page's `mean_score`/`low_score` are the NaN-ignoring mean / 5th
//!   percentile of its four scores; document-level `mean_score`/`low_score`
//!   are the plain means of the per-page values (docling's
//!   `ConfidenceReport` overrides — note: *mean*, not quantile, for both).
//! - Document-level per-field aggregation: mean for layout/table/ocr,
//!   10th percentile for parse.
//! - Grades: `< 0.5` poor, `< 0.8` fair, `< 0.9` good, `≥ 0.9` excellent,
//!   unset → unspecified.

use std::collections::BTreeMap;

/// docling's `QualityGrade`: a score bucketed for human consumption.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QualityGrade {
    Poor,
    Fair,
    Good,
    Excellent,
    /// No score available (e.g. a declarative conversion with no ML stages).
    Unspecified,
}

impl QualityGrade {
    /// docling's `_score_to_grade` thresholds.
    pub fn from_score(score: Option<f64>) -> Self {
        match score {
            Some(s) if s < 0.5 => Self::Poor,
            Some(s) if s < 0.8 => Self::Fair,
            Some(s) if s < 0.9 => Self::Good,
            Some(_) => Self::Excellent,
            None => Self::Unspecified,
        }
    }

    /// The wire spelling (docling's lowercase enum values).
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Poor => "poor",
            Self::Fair => "fair",
            Self::Good => "good",
            Self::Excellent => "excellent",
            Self::Unspecified => "unspecified",
        }
    }
}

/// NaN-ignoring mean (docling's `np.nanmean`): `None` entries are skipped;
/// all-unset yields `None` (where numpy would warn and return `NaN`).
pub fn nanmean(values: &[Option<f64>]) -> Option<f64> {
    let known: Vec<f64> = values.iter().filter_map(|v| *v).collect();
    if known.is_empty() {
        return None;
    }
    Some(known.iter().sum::<f64>() / known.len() as f64)
}

/// NaN-ignoring quantile with numpy's default linear interpolation
/// (docling's `np.nanquantile(..., q)`).
pub fn nanquantile(values: &[Option<f64>], q: f64) -> Option<f64> {
    let mut known: Vec<f64> = values.iter().filter_map(|v| *v).collect();
    if known.is_empty() {
        return None;
    }
    known.sort_by(|a, b| a.total_cmp(b));
    let pos = q * (known.len() - 1) as f64;
    let lo = pos.floor() as usize;
    let hi = pos.ceil() as usize;
    if lo == hi {
        return Some(known[lo]);
    }
    let frac = pos - lo as f64;
    Some(known[lo] * (1.0 - frac) + known[hi] * frac)
}

/// One page's confidence scores (docling's `PageConfidenceScores`).
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct PageConfidence {
    pub parse_score: Option<f64>,
    pub layout_score: Option<f64>,
    pub table_score: Option<f64>,
    pub ocr_score: Option<f64>,
}

impl PageConfidence {
    fn scores(&self) -> [Option<f64>; 4] {
        // docling's aggregation order: ocr, table, layout, parse.
        [
            self.ocr_score,
            self.table_score,
            self.layout_score,
            self.parse_score,
        ]
    }

    /// NaN-ignoring mean of the four scores.
    pub fn mean_score(&self) -> Option<f64> {
        nanmean(&self.scores())
    }

    /// 5th percentile of the four scores (docling's `low_score`).
    pub fn low_score(&self) -> Option<f64> {
        nanquantile(&self.scores(), 0.05)
    }

    fn to_json(self) -> serde_json::Value {
        serde_json::json!({
            "parse_score": self.parse_score,
            "layout_score": self.layout_score,
            "table_score": self.table_score,
            "ocr_score": self.ocr_score,
            "mean_grade": QualityGrade::from_score(self.mean_score()).as_str(),
            "low_grade": QualityGrade::from_score(self.low_score()).as_str(),
            "mean_score": self.mean_score(),
            "low_score": self.low_score(),
        })
    }
}

/// One OCR-recognized cell with the recognition confidence the pipeline
/// computed for it (#915): the mean emitted-character probability of the
/// line or word crop, the figure the page `ocr_score` averages. A docling
/// extension — docling's report stops at the page — so it lives beside the
/// report, never in the document JSON (docling-core's schema has no field for
/// it). Boxes are page points, top-left origin, like the pipeline's cells.
#[derive(Debug, Clone, PartialEq)]
pub struct OcrCellConfidence {
    pub text: String,
    pub l: f32,
    pub t: f32,
    pub r: f32,
    pub b: f32,
    pub confidence: f32,
}

/// The document-level report (docling's `ConfidenceReport`): the four scores
/// aggregated across pages, plus the per-page breakdown. Page keys are the
/// **real 1-based page numbers** — the same numbering as the JSON export's
/// `pages` map (#171), `--pages` windows included. (docling keys by its
/// 0-based internal page index; ours is the more useful spelling and the
/// difference is documented in `docs/MIGRATION.md`.)
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ConfidenceReport {
    pub pages: BTreeMap<usize, PageConfidence>,
    /// Per-cell OCR confidences by 1-based page number (#915); empty for
    /// pages read from a text layer. Not part of [`ConfidenceReport::to_json`]
    /// (docling's wire shape) — read it with [`ConfidenceReport::ocr_cells`].
    ocr_cells: BTreeMap<usize, Vec<OcrCellConfidence>>,
}

impl ConfidenceReport {
    /// Build from per-page scores keyed by 1-based page number.
    pub fn from_pages(pages: BTreeMap<usize, PageConfidence>) -> Self {
        Self {
            pages,
            ocr_cells: BTreeMap::new(),
        }
    }

    /// Attach the per-cell OCR confidences of a page (1-based). Empty lists
    /// are not stored.
    pub fn with_ocr_cells(mut self, page_no: usize, cells: Vec<OcrCellConfidence>) -> Self {
        if !cells.is_empty() {
            self.ocr_cells.insert(page_no, cells);
        }
        self
    }

    /// The OCR cells of a page with their recognition confidence, in the
    /// order the pipeline produced them; empty when the page was not OCR'd.
    pub fn ocr_cells(&self, page_no: usize) -> &[OcrCellConfidence] {
        self.ocr_cells.get(&page_no).map_or(&[], Vec::as_slice)
    }

    /// The per-cell OCR confidences of every page as JSON (#1246), for the
    /// bindings: `{"<page_no>": [{"text", "l", "t", "r", "b",
    /// "confidence"}, …]}` — 1-based page keys (JSON object keys are
    /// strings), boxes in page points with a top-left origin, cells in
    /// pipeline order; `{}` when no page was OCR'd. Like
    /// [`ConfidenceReport::ocr_cells`] a docling extension outside docling's
    /// wire shape ([`ConfidenceReport::to_json`]): it is what Python
    /// docling's `generate_parsed_pages` exposes as the `from_ocr` text-line
    /// cells of `ConversionResult.pages[i].parsed_page`.
    pub fn ocr_cells_json(&self) -> serde_json::Value {
        let pages = self.ocr_cells.iter().map(|(n, cells)| {
            let cells = cells
                .iter()
                .map(|c| {
                    serde_json::json!({
                        "text": c.text,
                        "l": c.l,
                        "t": c.t,
                        "r": c.r,
                        "b": c.b,
                        "confidence": c.confidence,
                    })
                })
                .collect();
            (n.to_string(), serde_json::Value::Array(cells))
        });
        serde_json::Value::Object(pages.collect())
    }

    /// The cells of every page whose confidence is below `threshold`,
    /// as `(page_no, cell)`.
    pub fn low_confidence_cells(
        &self,
        threshold: f32,
    ) -> impl Iterator<Item = (usize, &OcrCellConfidence)> {
        self.ocr_cells.iter().flat_map(move |(n, cells)| {
            cells
                .iter()
                .filter(move |c| c.confidence < threshold)
                .map(move |c| (*n, c))
        })
    }

    fn field(&self, get: impl Fn(&PageConfidence) -> Option<f64>) -> Vec<Option<f64>> {
        self.pages.values().map(get).collect()
    }

    /// Document `layout_score`: mean of the per-page values.
    pub fn layout_score(&self) -> Option<f64> {
        nanmean(&self.field(|p| p.layout_score))
    }

    /// Document `parse_score`: 10th percentile of the per-page values
    /// (docling: quantile here too, to emphasise problem pages).
    pub fn parse_score(&self) -> Option<f64> {
        nanquantile(&self.field(|p| p.parse_score), 0.1)
    }

    /// Document `table_score`: mean of the per-page values.
    pub fn table_score(&self) -> Option<f64> {
        nanmean(&self.field(|p| p.table_score))
    }

    /// Document `ocr_score`: mean of the per-page values.
    pub fn ocr_score(&self) -> Option<f64> {
        nanmean(&self.field(|p| p.ocr_score))
    }

    /// Document `mean_score`: mean of the per-page mean scores.
    pub fn mean_score(&self) -> Option<f64> {
        nanmean(&self.field(|p| p.mean_score()))
    }

    /// Document `low_score`: mean (sic — docling's override uses `nanmean`,
    /// not a quantile) of the per-page low scores.
    pub fn low_score(&self) -> Option<f64> {
        nanmean(&self.field(|p| p.low_score()))
    }

    pub fn mean_grade(&self) -> QualityGrade {
        QualityGrade::from_score(self.mean_score())
    }

    pub fn low_grade(&self) -> QualityGrade {
        QualityGrade::from_score(self.low_score())
    }

    /// The full report as JSON — docling's `ConfidenceReport` dump shape
    /// (unset scores as `null`, grades as lowercase strings, `pages` keyed by
    /// stringified page number).
    pub fn to_json(&self) -> serde_json::Value {
        let mut value = self.summary_json();
        value["pages"] = serde_json::Value::Object(
            self.pages
                .iter()
                .map(|(n, p)| (n.to_string(), p.to_json()))
                .collect(),
        );
        value
    }

    /// The document-level scores/grades only (no `pages`) — compact enough
    /// for a response header.
    pub fn summary_json(&self) -> serde_json::Value {
        serde_json::json!({
            "parse_score": self.parse_score(),
            "layout_score": self.layout_score(),
            "table_score": self.table_score(),
            "ocr_score": self.ocr_score(),
            "mean_grade": self.mean_grade().as_str(),
            "low_grade": self.low_grade().as_str(),
            "mean_score": self.mean_score(),
            "low_score": self.low_score(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ocr_cells_are_kept_beside_the_scores() {
        let cell = |t: &str, c: f32| OcrCellConfidence {
            text: t.into(),
            l: 0.0,
            t: 0.0,
            r: 1.0,
            b: 1.0,
            confidence: c,
        };
        let report = ConfidenceReport::from_pages(BTreeMap::new())
            .with_ocr_cells(2, vec![cell("ok", 0.99), cell("smudge", 0.31)])
            .with_ocr_cells(3, Vec::new());
        assert_eq!(report.ocr_cells(2).len(), 2);
        assert!(report.ocr_cells(1).is_empty() && report.ocr_cells(3).is_empty());
        let low: Vec<_> = report.low_confidence_cells(0.5).collect();
        assert_eq!(low.len(), 1);
        assert_eq!((low[0].0, low[0].1.text.as_str()), (2, "smudge"));
        // docling's wire shape is untouched.
        assert!(!report.to_json().to_string().contains("smudge"));
    }

    #[test]
    fn ocr_cells_json_keys_pages_and_keeps_boxes() {
        // #1246: the bindings read every page's cells from this one export.
        let cell = OcrCellConfidence {
            text: "Invoice".into(),
            l: 10.0,
            t: 20.0,
            r: 60.5,
            b: 32.0,
            confidence: 0.75,
        };
        let report = ConfidenceReport::from_pages(BTreeMap::new()).with_ocr_cells(2, vec![cell]);
        assert_eq!(
            report.ocr_cells_json(),
            serde_json::json!({"2": [{
                "text": "Invoice", "l": 10.0, "t": 20.0, "r": 60.5, "b": 32.0,
                "confidence": 0.75,
            }]})
        );
        assert_eq!(
            ConfidenceReport::default().ocr_cells_json(),
            serde_json::json!({})
        );
    }

    #[test]
    fn grades_follow_docling_thresholds() {
        assert_eq!(QualityGrade::from_score(Some(0.49)), QualityGrade::Poor);
        assert_eq!(QualityGrade::from_score(Some(0.5)), QualityGrade::Fair);
        assert_eq!(QualityGrade::from_score(Some(0.79)), QualityGrade::Fair);
        assert_eq!(QualityGrade::from_score(Some(0.8)), QualityGrade::Good);
        assert_eq!(QualityGrade::from_score(Some(0.89)), QualityGrade::Good);
        assert_eq!(QualityGrade::from_score(Some(0.9)), QualityGrade::Excellent);
        assert_eq!(QualityGrade::from_score(None), QualityGrade::Unspecified);
    }

    #[test]
    fn nan_handling_matches_numpy() {
        // nanmean skips unset entries; all-unset is unset.
        assert_eq!(nanmean(&[Some(0.5), None, Some(1.0)]), Some(0.75));
        assert_eq!(nanmean(&[None, None]), None);
        // Linear interpolation: quantile 0.05 over [0.2, 1.0] = 0.2 + 0.05*0.8.
        let q = nanquantile(&[Some(1.0), Some(0.2)], 0.05).unwrap();
        assert!((q - 0.24).abs() < 1e-12, "{q}");
        // Single value: every quantile is that value.
        assert_eq!(nanquantile(&[None, Some(0.7)], 0.1), Some(0.7));
    }

    #[test]
    fn report_aggregates_like_docling() {
        let mut pages = BTreeMap::new();
        pages.insert(
            1,
            PageConfidence {
                parse_score: Some(1.0),
                layout_score: Some(0.9),
                table_score: None,
                ocr_score: None,
            },
        );
        pages.insert(
            2,
            PageConfidence {
                parse_score: Some(0.6),
                layout_score: Some(0.7),
                table_score: None,
                ocr_score: Some(0.8),
            },
        );
        let report = ConfidenceReport::from_pages(pages);
        // layout: mean(0.9, 0.7); ocr: mean over the one page that has it.
        assert_eq!(report.layout_score(), Some(0.8));
        assert_eq!(report.ocr_score(), Some(0.8));
        assert_eq!(report.table_score(), None);
        // parse: 10th percentile of [0.6, 1.0] = 0.6 + 0.1*0.4.
        let parse = report.parse_score().unwrap();
        assert!((parse - 0.64).abs() < 1e-12, "{parse}");
        // Document mean = mean of the page means: page1 (0.9+1.0)/2 = 0.95,
        // page2 (0.8+0.7+0.6)/3 = 0.7 → 0.825.
        let mean = report.mean_score().unwrap();
        assert!((mean - 0.825).abs() < 1e-12, "{mean}");
        assert_eq!(report.mean_grade(), QualityGrade::Good);

        let json = report.to_json();
        assert_eq!(json["mean_grade"], "good");
        assert_eq!(json["table_score"], serde_json::Value::Null);
        assert!(json["pages"]["1"]["layout_score"].as_f64().is_some());
    }

    #[test]
    fn empty_report_is_unspecified() {
        let report = ConfidenceReport::default();
        assert_eq!(report.mean_score(), None);
        assert_eq!(report.mean_grade(), QualityGrade::Unspecified);
        assert_eq!(report.to_json()["low_grade"], "unspecified");
    }
}
