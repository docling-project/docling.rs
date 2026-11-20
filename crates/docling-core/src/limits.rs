//! Table cell budgets (#861; docling-core 2.103.1's `settings.max_table_cells`
//! and `max_total_table_cells`, its answer to a DoS advisory).
//!
//! Every reader of a table materializes its grid — the dense `rows` the
//! serializers print, the `num_rows × num_cols` slot map of the JSON export —
//! so a table's cost is its declared area, not its bytes: a docling-JSON
//! `TableData` can declare `num_rows = num_cols = 10^6` around one cell, and
//! an OTSL stream (DocTags, DocLang) of one row of 10^6 tokens and 10^6
//! rows of one token asks for 10^12 slots from a few megabytes. docling-core
//! rejects both at load; so do the readers here, through one
//! [`TableCellBudget`] per document, with the typed [`LimitExceeded`] on the
//! error's source chain (read with `ConversionError::limit()`), never a bare
//! parse error.
//!
//! The rows and columns are bounded on their own too, as upstream does: a
//! table of many rows and no columns still allocates one (empty) row each.

use std::fmt;

/// Cells (`rows × cols`, and rows or columns alone) one table may declare;
/// docling-core's `max_table_cells`.
pub const MAX_TABLE_CELLS_VAR: &str = "DOCLING_RS_MAX_TABLE_CELLS";
/// Cells every table of one document may declare together; docling-core's
/// `max_total_table_cells`. Bounds a document of many tables each just
/// under the per-table cap.
pub const MAX_TOTAL_TABLE_CELLS_VAR: &str = "DOCLING_RS_MAX_TOTAL_TABLE_CELLS";

/// `DOCLING_RS_MAX_TABLE_CELLS` (1 000 000, upstream's default).
pub fn max_table_cells() -> usize {
    crate::env::parse::<usize>(MAX_TABLE_CELLS_VAR).unwrap_or(1_000_000)
}

/// `DOCLING_RS_MAX_TOTAL_TABLE_CELLS` (5 000 000, upstream's default).
pub fn max_total_table_cells() -> usize {
    crate::env::parse::<usize>(MAX_TOTAL_TABLE_CELLS_VAR).unwrap_or(5_000_000)
}

/// An input asked for more than a safety limit allows. `variable` names the
/// environment variable that raises the limit; `what` the input part.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LimitExceeded {
    pub variable: &'static str,
    pub limit: usize,
    pub actual: usize,
    pub what: String,
}

impl fmt::Display for LimitExceeded {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{}: {} {} exceeds the limit of {} ({})",
            self.what,
            self.actual,
            self.unit(),
            self.limit,
            self.variable
        )
    }
}

impl LimitExceeded {
    /// What `limit` and `actual` count, read off the variable: bytes for a
    /// `…_MB` size cap (the counts themselves are bytes), members for a
    /// member-count cap, cells for the table budgets.
    pub fn unit(&self) -> &'static str {
        if self.variable.ends_with("_MB") {
            "bytes"
        } else if self.variable.ends_with("_MEMBERS") {
            "members"
        } else {
            "cells"
        }
    }
}

impl std::error::Error for LimitExceeded {}

/// The cell budget of one document: each table is checked against the
/// per-table cap and added to the running total, which is checked against
/// the per-document cap. One budget per conversion, read from the
/// environment when it is created.
#[derive(Debug, Clone)]
pub struct TableCellBudget {
    per_table: usize,
    total_cap: usize,
    total: usize,
}

impl Default for TableCellBudget {
    fn default() -> Self {
        Self::new()
    }
}

impl TableCellBudget {
    pub fn new() -> Self {
        Self {
            per_table: max_table_cells(),
            total_cap: max_total_table_cells(),
            total: 0,
        }
    }

    /// Account for a table of `rows × cols` declared cells. `what` names it
    /// in the error (`docling-json #/tables/3`, `doclang <table>`).
    pub fn table(&mut self, what: &str, rows: usize, cols: usize) -> Result<(), LimitExceeded> {
        let area = rows.saturating_mul(cols);
        if rows > self.per_table || cols > self.per_table || area > self.per_table {
            return Err(LimitExceeded {
                variable: MAX_TABLE_CELLS_VAR,
                limit: self.per_table,
                actual: area.max(rows).max(cols),
                what: format!("{what} of {rows}x{cols}"),
            });
        }
        self.total = self.total.saturating_add(area);
        if self.total > self.total_cap {
            return Err(LimitExceeded {
                variable: MAX_TOTAL_TABLE_CELLS_VAR,
                limit: self.total_cap,
                actual: self.total,
                what: format!("{what}: the document's tables together"),
            });
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn per_table_and_total_caps() {
        let mut b = TableCellBudget {
            per_table: 100,
            total_cap: 250,
            total: 0,
        };
        assert!(b.table("t", 10, 10).is_ok());
        let e = b.table("t", 10, 11).unwrap_err();
        assert_eq!(e.variable, MAX_TABLE_CELLS_VAR);
        assert_eq!(e.actual, 110);
        // Rows or columns alone past the cap, even with no area.
        assert_eq!(b.table("t", 101, 0).unwrap_err().actual, 101);
        assert!(b.table("t", 0, 101).is_err());
        assert!(b.table("t", 10, 10).is_ok());
        let e = b.table("t", 10, 10).unwrap_err();
        assert_eq!(e.variable, MAX_TOTAL_TABLE_CELLS_VAR);
        assert_eq!(e.actual, 300);
        // No overflow on absurd dimensions.
        let mut b = TableCellBudget::new();
        assert!(b.table("t", usize::MAX, usize::MAX).is_err());
        assert!(b.table("t", usize::MAX, 0).is_err());
    }
}
