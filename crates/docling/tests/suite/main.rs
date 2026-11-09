//! The integration tests of `docling`, as one test binary (`--test suite`).
//!
//! Each file here was its own test binary, and each binary links the whole
//! crate with every backend: with thin LTO that link was most of a test run's
//! time. As modules of one binary they share one link. Run one module with a
//! filter on its path, e.g.
//!
//! ```sh
//! cargo test -p docling --test suite pptx_list_cascade::
//! ```
//!
//! A test file that changes the process environment (`std::env::set_var`)
//! stays a binary of its own (see `[[test]]` in Cargo.toml), so it cannot
//! leak into these: `image_sources`, `no_ocr`, `pages`,
//! `picture_description`, `textline_orientation_no_model`, `youtube`. So
//! does `regression`, which the fixture docs name (`DOCLING_RS_REGEN=1 cargo
//! test --release -p docling --test regression`).

mod afp;
mod asciidoc_upstream;
mod calendar;
mod charset;
mod chunk_refs;
mod dclx_roundtrip;
mod detect;
mod djvu_prov;
mod doclang_reading;
mod doclang_upstream;
mod docling_json_prov;
mod document_metadata;
mod document_timeout;
mod docx_upstream;
mod encrypted;
mod feed;
mod heif;
mod html_depth_cap;
mod html_export;
mod html_source_blocks;
mod html_upstream;
mod image_output;
mod input_robustness;
mod ipynb;
mod iwork;
mod jpx_pictures;
mod json_prov;
mod latex_upstream;
mod legacy_216;
mod md_upstream;
mod odf_upstream;
mod options_inventory;
mod page_breaks;
mod pandoc;
mod partial_text_layer;
mod pdf_repairs;
mod picture_alt_text;
mod picture_ocr;
mod plain_text;
mod plugins;
mod portable_paths;
mod pptx_charts;
mod pptx_hidden_layers;
mod pptx_list_cascade;
mod pptx_shapes;
mod progress;
mod redact;
mod scanned;
mod script_position;
mod security_doc;
mod sideways_outlines;
mod svg;
mod tesseract;
mod textline_orientation;
mod vector_text_ocr;
mod vlm;
mod vtt_export;
mod warm_pipeline;
mod word6;
mod xlsx_number_formats;
