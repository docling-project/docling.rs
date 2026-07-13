"""Parity harness for the tf_match.rs port of docling's table-cell matching.

Run the Rust pipeline with DOCLING_RS_TF_MATCH_DUMP=<dir> to record each
table's matcher inputs (<dir>/tf_match_dump.jsonl), then replay them through
docling_ibm_models' reference CellMatcher + MatchingPostProcessor and rebuild
the markdown grid the same way the Rust side does, for a side-by-side check:

    python scripts/test/tf_match_reference.py <dir>/tf_match_dump.jsonl [text-filter]

Needs a Python env with docling-ibm-models installed. If the printed grid
matches the Rust output, a conformance gap is in the model predictions (or
upstream geometry), not in the ported matching.
"""
import json
import sys

from docling_ibm_models.tableformer.data_management.matching_post_processor import (
    MatchingPostProcessor,
)
from docling_ibm_models.tableformer.data_management.tf_cell_matcher import CellMatcher

config = {"predict": {"pdf_cell_iou_thres": 0.05}}
matcher = CellMatcher(config)
post = MatchingPostProcessor(config)

dump_path = sys.argv[1]
want = sys.argv[2] if len(sys.argv) > 2 else None  # substring to select a table

for line_no, line in enumerate(open(dump_path)):
    rec = json.loads(line)
    words = rec["pdf_cells"]
    if want and not any(want in w["text"] for w in words):
        continue
    table_cells = []
    for c in rec["table_cells"]:
        cell = {
            "cell_id": c["cell_id"],
            "row_id": c["row_id"],
            "column_id": c["column_id"],
            "bbox": c["bbox"],
            "cell_class": c["cell_class"],
            "label": "body",
        }
        if c["colspan_val"] > 0:
            cell["colspan_val"] = c["colspan_val"]
        if c["rowspan_val"] > 0:
            cell["rowspan_val"] = c["rowspan_val"]
        table_cells.append(cell)
    pdf_cells = [{"id": w["id"], "bbox": w["bbox"], "text": w["text"]} for w in words]

    matches, _ = matcher._intersection_over_pdf_match(table_cells, pdf_cells)
    matching_details = {
        "iou_threshold": 0.05,
        "table_cells": table_cells,
        "matches": matches,
        "pdf_cells": pdf_cells,
    }
    out = post.process(matching_details, False)
    cells_wo = out["table_cells"]
    final = out["matches"]

    # Response assembly identical to the Rust side.
    by_id = {}
    for c in cells_wo:
        by_id.setdefault(c["cell_id"], c)
    merged = {}
    order = []
    for pdf_id in sorted(int(k) for k in final):
        m = final[str(pdf_id)][0]
        cell = by_id.get(m["table_cell_id"])
        if cell is None:
            continue
        key = (cell["column_id"], cell["row_id"])
        if key not in merged:
            merged[key] = {
                "start_row": cell["row_id"],
                "start_col": cell["column_id"],
                "row_span": cell.get("rowspan_val", 1),
                "col_span": cell.get("colspan_val", 1),
                "word_ids": [],
            }
            order.append(key)
        merged[key]["word_ids"].append(pdf_id)

    start_cols = sorted({merged[k]["start_col"] for k in order})
    start_rows = sorted({merged[k]["start_row"] for k in order})
    num_rows = num_cols = 0
    for k in order:
        m = merged[k]
        m["ci"] = start_cols.index(m["start_col"])
        m["ri"] = start_rows.index(m["start_row"])
        num_cols = max(num_cols, m["ci"] + m["col_span"])
        num_rows = max(num_rows, m["ri"] + m["row_span"])

    wtext = {w["id"]: w["text"] for w in pdf_cells}
    grid = [["" for _ in range(num_cols)] for _ in range(num_rows)]
    for k in order:
        m = merged[k]
        text = " ".join(wtext[i].strip() for i in m["word_ids"]).replace("@ ", "@")
        for r in range(m["ri"], min(m["ri"] + m["row_span"], num_rows)):
            for c in range(m["ci"], min(m["ci"] + m["col_span"], num_cols)):
                grid[r][c] = text
    print(f"--- table (dump line {line_no}) {num_rows}x{num_cols}")
    for row in grid:
        print("| " + " | ".join(row) + " |")
    break
