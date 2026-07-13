//! docling's TableFormer cell matching, ported from docling-ibm-models
//! (`tf_cell_matcher.py` + `matching_post_processor.py`, and the response
//! assembly in `tf_predictor.py`). The predicted table cells and the page's
//! word cells are matched by intersection-over-word-area, then the
//! post-processor snaps unmatched cells to their column's median position,
//! de-duplicates columns, assigns each word to its best cell, and finally
//! picks up "orphan" words (no overlap with any cell — dot leaders in a TOC,
//! stray page numbers) through row/column banding. Everything is kept
//! bug-for-bug faithful, including duplicate `good` cells weighting the
//! column medians and Python's banker's rounding on orphan depths.
//!
//! Coordinates: docling matches in its 2x-scaled page space with the table
//! bbox rounded to integers *before* scaling (`round(cluster.bbox.l) * scale`);
//! the caller reproduces that space so absolute constants (orphan-depth
//! rounding) agree.

use std::collections::BTreeMap;

/// docling's `table_cell` dict: an OTSL grid cell with its predicted box in
/// the matching coordinate space. `colspan_val`/`rowspan_val` are 0 when the
/// cell has no span (the dict key is absent in docling).
#[derive(Debug, Clone)]
pub struct TfCell {
    pub bbox: [f64; 4],
    pub cell_id: usize,
    pub row_id: usize,
    pub column_id: usize,
    pub cell_class: i64,
    pub colspan_val: usize,
    pub rowspan_val: usize,
}

/// A page word cell in the matching space (docling's `pdf_cells` token).
#[derive(Debug, Clone)]
pub struct PdfWord {
    pub id: usize,
    pub bbox: [f64; 4],
    pub text: String,
}

/// One `{table_cell_id, iopdf|post}` match entry.
#[derive(Debug, Clone, PartialEq)]
pub struct MatchEntry {
    pub table_cell_id: usize,
    pub score: f64,
}

/// pdf-cell-id → its match list. BTreeMap keeps deterministic ascending-id
/// iteration; docling's insertion-ordered dicts never depend on their order
/// for the final result (the response is re-sorted by pdf cell id).
pub type Matches = BTreeMap<usize, Vec<MatchEntry>>;

/// docling's `find_intersection`, including the source's harmless
/// `b2[1] > b2[3]` self-comparison typo (never true for a valid box).
fn find_intersection(b1: &[f64; 4], b2: &[f64; 4]) -> Option<[f64; 4]> {
    if b1[2] < b2[0] || b2[2] < b1[0] || b1[1] > b2[3] {
        return None;
    }
    Some([
        b1[0].max(b2[0]),
        b1[1].max(b2[1]),
        b1[2].min(b2[2]),
        b1[3].min(b2[3]),
    ])
}

/// `CellMatcher._intersection_over_pdf_match`: every (table cell, word) pair
/// with a positive intersection-over-word-area becomes a match entry; exact
/// duplicates (same cell id *and* score — duplicated `good` cells) are
/// suppressed like Python's `match not in matches[id]`.
fn intersection_over_pdf_match(table_cells: &[TfCell], pdf_cells: &[PdfWord]) -> Matches {
    let mut matches: Matches = BTreeMap::new();
    for cell in table_cells {
        for word in pdf_cells {
            let Some(ib) = find_intersection(&cell.bbox, &word.bbox) else {
                continue;
            };
            let warea = (word.bbox[2] - word.bbox[0]) * (word.bbox[3] - word.bbox[1]);
            let iarea = (ib[2] - ib[0]) * (ib[3] - ib[1]);
            let iopdf = if warea > 0.0 { iarea / warea } else { 0.0 };
            if iopdf > 0.0 {
                let entry = MatchEntry {
                    table_cell_id: cell.cell_id,
                    score: iopdf,
                };
                let list = matches.entry(word.id).or_default();
                if !list.contains(&entry) {
                    list.push(entry);
                }
            }
        }
    }
    matches
}

/// Step 0: `_get_table_dimension` — (columns, rows, max_cell_id).
fn get_table_dimension(table_cells: &[TfCell]) -> (usize, usize, usize) {
    let mut columns = 1;
    let mut rows = 1;
    let mut max_cell_id = 0;
    for cell in table_cells {
        columns = columns.max(cell.column_id);
        rows = rows.max(cell.row_id);
        max_cell_id = max_cell_id.max(cell.cell_id);
    }
    (columns + 1, rows + 1, max_cell_id)
}

/// Step 1: `_get_good_bad_cells_in_column`. A cell is `good` once per match
/// entry pointing at it (docling appends without breaking, so a cell matched
/// by N words appears N times and weights the medians N-fold); a cell whose
/// predicted class is empty (`cell_class <= 1`) is always `bad`.
fn good_bad_cells_in_column(
    table_cells: &[TfCell],
    column: usize,
    matches: &Matches,
) -> (Vec<TfCell>, Vec<TfCell>) {
    let mut good = Vec::new();
    let mut bad = Vec::new();
    for cell in table_cells {
        if cell.column_id != column {
            continue;
        }
        let mut bad_match = true;
        if cell.cell_class > 1 {
            for list in matches.values() {
                for m in list {
                    if m.table_cell_id == cell.cell_id {
                        good.push(cell.clone());
                        bad_match = false;
                    }
                }
            }
        }
        if bad_match {
            bad.push(cell.clone());
        }
    }
    (good, bad)
}

#[derive(Clone, Copy, PartialEq)]
enum Alignment {
    Left,
    Middle,
    Right,
}

/// Step 2: `_find_alignment_in_column` — smallest min-max spread of the
/// left / middle / right cell edges decides the column alignment.
fn find_alignment_in_column(cells: &[TfCell]) -> Alignment {
    if cells.is_empty() {
        return Alignment::Left;
    }
    let (mut lmin, mut lmax) = (f64::INFINITY, f64::NEG_INFINITY);
    let (mut mmin, mut mmax) = (f64::INFINITY, f64::NEG_INFINITY);
    let (mut rmin, mut rmax) = (f64::INFINITY, f64::NEG_INFINITY);
    for cell in cells {
        let l = cell.bbox[0];
        let r = cell.bbox[2];
        let m = (l + r) / 2.0;
        lmin = lmin.min(l);
        lmax = lmax.max(l);
        mmin = mmin.min(m);
        mmax = mmax.max(m);
        rmin = rmin.min(r);
        rmax = rmax.max(r);
    }
    let deltas = [lmax - lmin, mmax - mmin, rmax - rmin];
    // Python's list.index(min(...)) keeps the first minimum.
    let mut best = 0;
    for (i, d) in deltas.iter().enumerate() {
        if *d < deltas[best] {
            best = i;
        }
    }
    match best {
        0 => Alignment::Left,
        1 => Alignment::Middle,
        _ => Alignment::Right,
    }
}

/// `statistics.median`: mean of the two middle values for an even count.
fn median(values: &mut [f64]) -> f64 {
    values.sort_by(|a, b| a.total_cmp(b));
    let n = values.len();
    if n % 2 == 1 {
        values[n / 2]
    } else {
        (values[n / 2 - 1] + values[n / 2]) / 2.0
    }
}

/// Step 3: `_get_median_pos_size` — median alignment-edge X / width / height
/// over the good cells, skipping spans and predicted-empty cells. Defaults
/// (0, 1, 1) when nothing qualifies, exactly like the source.
fn get_median_pos_size(cells: &[TfCell], alignment: Alignment) -> (f64, f64, f64) {
    let mut xs = Vec::new();
    let mut ws = Vec::new();
    let mut hs = Vec::new();
    for cell in cells {
        if cell.rowspan_val > 0 || cell.colspan_val > 0 || cell.cell_class <= 1 {
            continue;
        }
        let x = match alignment {
            Alignment::Left => cell.bbox[0],
            Alignment::Middle => (cell.bbox[2] + cell.bbox[0]) / 2.0,
            Alignment::Right => cell.bbox[2],
        };
        xs.push(x);
        ws.push(cell.bbox[2] - cell.bbox[0]);
        hs.push(cell.bbox[3] - cell.bbox[1]);
    }
    let median_x = if xs.is_empty() { 0.0 } else { median(&mut xs) };
    let median_w = if ws.is_empty() { 1.0 } else { median(&mut ws) };
    let median_h = if hs.is_empty() { 1.0 } else { median(&mut hs) };
    (median_x, median_w, median_h)
}

/// Step 4: `_move_cells_to_left_pos` with `rescale=False` (docling's call):
/// slide each bad cell to the column's median alignment edge, keeping its
/// original width.
fn move_cells_to_left_pos(cells: &[TfCell], median_x: f64, alignment: Alignment) -> Vec<TfCell> {
    cells
        .iter()
        .map(|cell| {
            let width = cell.bbox[2] - cell.bbox[0];
            let (new_x1, new_x2) = match alignment {
                Alignment::Left => (median_x, median_x + width),
                // Bit-faithful to the source: `new_x2 = new_x1 + original_width`.
                Alignment::Middle => {
                    let x1 = median_x - width / 2.0;
                    (x1, x1 + width)
                }
                Alignment::Right => (median_x - width, median_x),
            };
            TfCell {
                bbox: [new_x1, cell.bbox[1], new_x2, cell.bbox[3]],
                ..cell.clone()
            }
        })
        .collect()
}

/// Step 7: `_deduplicate_cells` — when two *adjacent* structural columns share
/// more than 60 % of their matched words, drop the lower-scoring column (its
/// cells and their match entries).
fn deduplicate_cells(
    tab_columns: usize,
    table_cells: &[TfCell],
    iou_matches: &Matches,
    ioc_matches: &Matches,
) -> (Vec<TfCell>, Matches) {
    use std::collections::BTreeSet;
    let mut pdf_cells_in_columns: Vec<BTreeSet<usize>> = Vec::with_capacity(tab_columns);
    let mut total_score_in_columns: Vec<f64> = Vec::with_capacity(tab_columns);
    for col in 0..tab_columns {
        let column_cell_ids: BTreeSet<usize> = table_cells
            .iter()
            .filter(|c| c.column_id == col)
            .map(|c| c.cell_id)
            .collect();
        let mut ids = BTreeSet::new();
        let mut score = 0.0;
        for matches in [iou_matches, ioc_matches] {
            for (&pdf_id, list) in matches {
                for m in list {
                    if column_cell_ids.contains(&m.table_cell_id) {
                        score += m.score;
                        ids.insert(pdf_id);
                    }
                }
            }
        }
        pdf_cells_in_columns.push(ids);
        total_score_in_columns.push(score);
    }

    let mut cols_to_eliminate: Vec<usize> = Vec::new();
    for cl in 0..tab_columns.saturating_sub(1) {
        let col_a = &pdf_cells_in_columns[cl];
        let col_b = &pdf_cells_in_columns[cl + 1];
        let int_prc = if col_a.is_empty() {
            0.0
        } else {
            col_a.intersection(col_b).count() as f64 / col_a.len() as f64
        };
        if int_prc > 0.6 {
            if total_score_in_columns[cl] >= total_score_in_columns[cl + 1] {
                cols_to_eliminate.push(cl + 1);
            } else {
                cols_to_eliminate.push(cl);
            }
        }
    }

    let mut removed_ids: BTreeSet<usize> = BTreeSet::new();
    let mut new_table_cells = Vec::new();
    for cell in table_cells {
        if cols_to_eliminate.contains(&cell.column_id) {
            removed_ids.insert(cell.cell_id);
        } else {
            new_table_cells.push(cell.clone());
        }
    }
    let mut new_matches: Matches = BTreeMap::new();
    for (&pdf_id, list) in ioc_matches {
        let kept: Vec<MatchEntry> = list
            .iter()
            .filter(|m| !removed_ids.contains(&m.table_cell_id))
            .cloned()
            .collect();
        if !kept.is_empty() {
            new_matches.insert(pdf_id, kept);
        }
    }
    (new_table_cells, new_matches)
}

/// Step 8: `_do_final_asignment` — each word keeps only its highest-scoring
/// match (Python's `max` keeps the first of equals).
fn do_final_assignment(ioc_matches: &Matches) -> Matches {
    let mut new_matches: Matches = BTreeMap::new();
    for (&pdf_id, list) in ioc_matches {
        let mut best = &list[0];
        for m in &list[1..] {
            if m.score > best.score {
                best = m;
            }
        }
        new_matches.insert(pdf_id, vec![best.clone()]);
    }
    new_matches
}

/// Step 8.a: `_align_table_cells_to_pdf` — matched cells take the union box
/// of their matched words; cells with no match are dropped.
fn align_table_cells_to_pdf(
    table_cells: &[TfCell],
    pdf_cells: &[PdfWord],
    matches: &Matches,
) -> Vec<TfCell> {
    use std::collections::HashMap;
    let word_boxes: HashMap<usize, [f64; 4]> = pdf_cells.iter().map(|w| (w.id, w.bbox)).collect();
    let mut boxes_per_cell: BTreeMap<usize, [f64; 4]> = BTreeMap::new();
    let mut order: Vec<usize> = Vec::new();
    for (pdf_id, list) in matches {
        let Some(&wb) = word_boxes.get(pdf_id) else {
            continue;
        };
        let mut seen = std::collections::BTreeSet::new();
        for m in list {
            if !seen.insert(m.table_cell_id) {
                continue;
            }
            match boxes_per_cell.entry(m.table_cell_id) {
                std::collections::btree_map::Entry::Vacant(e) => {
                    e.insert(wb);
                    order.push(m.table_cell_id);
                }
                std::collections::btree_map::Entry::Occupied(mut e) => {
                    let b = e.get_mut();
                    b[0] = b[0].min(wb[0]);
                    b[1] = b[1].min(wb[1]);
                    b[2] = b[2].max(wb[2]);
                    b[3] = b[3].max(wb[3]);
                }
            }
        }
    }
    let by_id: HashMap<usize, &TfCell> = table_cells.iter().map(|c| (c.cell_id, c)).collect();
    let mut out = Vec::new();
    for cell_id in order {
        if let Some(&cell) = by_id.get(&cell_id) {
            let mut cell = cell.clone();
            cell.bbox = boxes_per_cell[&cell_id];
            out.push(cell);
        }
    }
    out
}

fn merge_two_bboxes(a: &[f64; 4], b: &[f64; 4]) -> [f64; 4] {
    [
        a[0].min(b[0]),
        a[1].min(b[1]),
        a[2].max(b[2]),
        a[3].max(b[3]),
    ]
}

/// Python 3 `round()` — banker's rounding to the nearest integer.
fn py_round(v: f64) -> i64 {
    v.round_ties_even() as i64
}

/// A per-band orphan record: (pdf id, rounded depth, word bbox).
type OrphanRecord = (usize, i64, [f64; 4]);

/// Shared row/column banding scan of step 9: for each band (min/max of the
/// non-span, non-empty member cells' `lo`/`hi` edges), collect the unmatched
/// words whose edge or centroid falls inside, resolving a word seen in an
/// earlier band by the smaller rounded centroid distance.
fn band_orphans(
    n_bands: usize,
    cells_in_band: impl Fn(usize) -> Vec<[f64; 4]>,
    pdf_cells: &[PdfWord],
    matches: &Matches,
    lo_ix: usize,
    hi_ix: usize,
) -> Vec<Vec<OrphanRecord>> {
    let mut bands: Vec<Vec<OrphanRecord>> = Vec::with_capacity(n_bands);
    // pdf id → (band index, index within band) of its current record.
    let mut used: BTreeMap<usize, usize> = BTreeMap::new();
    for band_ix in 0..n_bands {
        let boxes = cells_in_band(band_ix);
        let mut lo = -1.0f64;
        let mut hi = -1.0f64;
        if !boxes.is_empty() {
            lo = boxes.iter().map(|b| b[lo_ix]).fold(f64::INFINITY, f64::min);
            hi = boxes
                .iter()
                .map(|b| b[hi_ix])
                .fold(f64::NEG_INFINITY, f64::max);
        }
        let mut in_band: Vec<OrphanRecord> = Vec::new();
        for word in pdf_cells {
            if matches.contains_key(&word.id) {
                continue;
            }
            let centroid_band = (hi + lo) / 2.0;
            let centroid_cell = (word.bbox[hi_ix] + word.bbox[lo_ix]) / 2.0;
            let within = (word.bbox[lo_ix] >= lo && word.bbox[lo_ix] <= hi)
                || (word.bbox[hi_ix] >= lo && word.bbox[hi_ix] <= hi)
                || (word.bbox[lo_ix] <= lo && word.bbox[hi_ix] >= hi);
            if !within {
                continue;
            }
            let depth = py_round((centroid_band - centroid_cell).abs());
            match used.get(&word.id) {
                None => {
                    used.insert(word.id, band_ix);
                    in_band.push((word.id, depth, word.bbox));
                }
                Some(&old_band) => {
                    let Some(old_pos) = bands[old_band].iter().position(|r| r.0 == word.id) else {
                        continue;
                    };
                    if depth < bands[old_band][old_pos].1 {
                        bands[old_band].remove(old_pos);
                        used.insert(word.id, band_ix);
                        in_band.push((word.id, depth, word.bbox));
                    }
                }
            }
        }
        bands.push(in_band);
    }
    bands
}

/// Step 9: `_pick_orphan_cells` — words with no match are placed by the row
/// band containing them vertically and the column band containing them
/// horizontally; the (row, column) either reuses an existing structural cell
/// (growing its box) or creates a new one.
fn pick_orphan_cells(
    tab_rows: usize,
    tab_cols: usize,
    mut max_cell_id: usize,
    mut table_cells: Vec<TfCell>,
    pdf_cells: &[PdfWord],
    mut matches: Matches,
) -> (Matches, Vec<TfCell>, usize) {
    // NOTE: docling's row-band pass exposes an aliasing quirk — the band's
    // orphan test reads `matches` as it was on entry (orphans found later
    // never re-enter it), which we reproduce by collecting all bands before
    // assigning anything.
    let orphan_rows = band_orphans(
        tab_rows,
        |row| {
            table_cells
                .iter()
                .filter(|c| c.row_id == row && c.rowspan_val == 0 && c.cell_class > 1)
                .map(|c| c.bbox)
                .collect()
        },
        pdf_cells,
        &matches,
        1,
        3,
    );
    let orphan_columns = band_orphans(
        tab_cols,
        |col| {
            table_cells
                .iter()
                .filter(|c| c.column_id == col && c.colspan_val == 0 && c.cell_class > 1)
                .map(|c| c.bbox)
                .collect()
        },
        pdf_cells,
        &matches,
        0,
        2,
    );

    // pdf id → row / column of its accepted band record.
    let mut row_of: BTreeMap<usize, usize> = BTreeMap::new();
    for (row_id, records) in orphan_rows.iter().enumerate() {
        for r in records {
            row_of.insert(r.0, row_id);
        }
    }
    let mut col_of: BTreeMap<usize, (usize, i64, [f64; 4])> = BTreeMap::new();
    for (col_id, records) in orphan_columns.iter().enumerate() {
        for r in records {
            col_of.insert(r.0, (col_id, r.1, r.2));
        }
    }

    // Ascending pdf id, matching the sorted C++-parity loop in the source.
    for (&pdf_id, &new_row_id) in row_of.iter() {
        let Some(&(new_column_id, confidence, pdf_bbox)) = col_of.get(&pdf_id) else {
            continue;
        };
        let existing = table_cells
            .iter()
            .position(|c| c.row_id == new_row_id && c.column_id == new_column_id);
        let table_cell_id = match existing {
            Some(ix) => {
                let merged = merge_two_bboxes(&table_cells[ix].bbox, &pdf_bbox);
                table_cells[ix].bbox = merged;
                table_cells[ix].cell_id
            }
            None => {
                max_cell_id += 1;
                table_cells.push(TfCell {
                    bbox: pdf_bbox,
                    cell_id: max_cell_id,
                    column_id: new_column_id,
                    row_id: new_row_id,
                    cell_class: 2,
                    colspan_val: 0,
                    rowspan_val: 0,
                });
                max_cell_id
            }
        };
        matches.insert(
            pdf_id,
            vec![MatchEntry {
                table_cell_id,
                score: confidence as f64,
            }],
        );
    }
    (matches, table_cells, max_cell_id)
}

/// `MatchingPostProcessor.process` with docling's defaults
/// (`correct_overlapping_cells=False`): returns the post-processed table
/// cells and the final one-cell-per-word matches.
pub fn match_and_post_process(
    table_cells: Vec<TfCell>,
    pdf_cells: &[PdfWord],
) -> (Vec<TfCell>, Matches) {
    let matches = intersection_over_pdf_match(&table_cells, pdf_cells);

    let (tab_columns, tab_rows, max_cell_id) = get_table_dimension(&table_cells);

    // Steps 1-4 per column: snap the unmatched cells to the matched cells'
    // median alignment edge.
    let mut fixed: Vec<TfCell> = Vec::new();
    for col in 0..tab_columns {
        let (good, bad) = good_bad_cells_in_column(&table_cells, col, &matches);
        let alignment = find_alignment_in_column(&good);
        let (median_x, median_w, median_h) = get_median_pos_size(&good, alignment);
        let _ = (median_w, median_h); // rescale=False: medians only steer X.
        let moved = move_cells_to_left_pos(&bad, median_x, alignment);
        fixed.extend(good);
        fixed.extend(moved);
    }
    fixed.sort_by_key(|c| c.cell_id);

    // Step 5: re-match against the fixed cells.
    let ioc = intersection_over_pdf_match(&fixed, pdf_cells);

    // Step 7: drop duplicated columns.
    let (dedupl_cells, dedupl_matches) = deduplicate_cells(tab_columns, &fixed, &matches, &ioc);

    // Step 8: one table cell per word.
    let final_matches = do_final_assignment(&dedupl_matches);

    // Step 8.a: align cell boxes to their matched words (dropping unmatched
    // cells) — skipped for large pages, as in the source.
    let mut dedupl_sorted = dedupl_cells;
    dedupl_sorted.sort_by_key(|c| c.cell_id);
    let aligned = if pdf_cells.len() > 300 {
        dedupl_sorted
    } else {
        align_table_cells_to_pdf(&dedupl_sorted, pdf_cells, &final_matches)
    };

    // Step 9: place the leftover words by row/column banding. docling passes
    // the *pre-dedup* column count into the orphan scan.
    let (final_matches, cells_wo, _) = pick_orphan_cells(
        tab_rows,
        tab_columns,
        max_cell_id,
        aligned,
        pdf_cells,
        final_matches,
    );
    (cells_wo, final_matches)
}
