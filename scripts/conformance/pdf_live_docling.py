#!/usr/bin/env python3
"""Convert PDFs with the installed docling exactly as docling's
`tests/test_e2e_conversion.py` does — `do_ocr=False`, TableFormer with cell
matching, CPU, generated parsed pages, AcroForm form fields — and write
`export_to_markdown(compact_tables=True)` per stem: the live-docling column of
docs/PDF_CONFORMANCE.md, comparable to the committed groundtruth and to
`pdf_groundtruth.sh`'s output.

    scripts/conformance/setup-docling.sh        # .venv-compare with docling
    .venv-compare/bin/python scripts/conformance/pdf_live_docling.py OUTDIR \\
        tests/data/pdf/sources/*.pdf
"""

import sys
import time
from pathlib import Path

from docling.datamodel.accelerator_options import AcceleratorDevice
from docling.datamodel.base_models import InputFormat
from docling.datamodel.pipeline_options import PdfPipelineOptions
from docling.document_converter import DocumentConverter, PdfFormatOption

# docling keeps these out of its e2e test (test_failed_pages.py fixtures).
SKIP = {"skipped_1page", "skipped_2pages"}


def main() -> None:
    if len(sys.argv) < 3:
        sys.exit(__doc__)
    out = Path(sys.argv[1])
    out.mkdir(parents=True, exist_ok=True)
    opts = PdfPipelineOptions()
    opts.do_ocr = False
    opts.do_table_structure = True
    opts.table_structure_options.do_cell_matching = True
    opts.accelerator_options.device = AcceleratorDevice.CPU
    opts.generate_parsed_pages = True
    opts.extract_form_fields = True
    conv = DocumentConverter(
        format_options={
            InputFormat.PDF: PdfFormatOption(
                pipeline_options=opts, backend=PdfFormatOption().backend
            )
        }
    )
    for src in map(Path, sys.argv[2:]):
        if src.stem in SKIP:
            continue
        t = time.time()
        try:
            doc = conv.convert(src).document
            (out / f"{src.stem}.md").write_text(
                doc.export_to_markdown(compact_tables=True)
            )
            print(f"{src.stem}: {time.time() - t:.1f}s", flush=True)
        except Exception as e:  # one bad file must not hide the others
            print(f"{src.stem}: ERROR {type(e).__name__}: {e}", flush=True)


if __name__ == "__main__":
    main()
