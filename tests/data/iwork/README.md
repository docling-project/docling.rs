# Apple iWork fixtures (#213, #318, #383)

Our own iWork fixtures for the `IworkBackend`, pinned by
`crates/docling/tests/iwork.rs` against `groundtruth/` (regenerate with
`DOCLING_RS_REGEN=1`). Upstream docling's Pages corpus — the fixtures and the
groundtruth it commits (docling#4062) — is mirrored separately in
`tests/data/pages/`, since `.pages` is a conformance format; only fixtures
upstream does not have live here.

The libetonyek Pages 5 bundles pin what upstream's reader does on a template
document: upstream rejects the nested-directory layout of
`proposal_nested_dir.pages` ("does not look like a Pages document"), so its
groundtruth is ours — produced by the same reader that matches upstream on its
own corpus. `pages_password_protected.pages` pins the error path (docling's
"password-protected" message); it has no groundtruth.

Keynote and Numbers are conformance formats too, with upstream's corpora
mirrored in `tests/data/keynote/` (#466) and `tests/data/numbers/` (#1031);
the Numbers and Keynote fixtures here are extra coverage from the
numbers-parser and keynote-parser test data.

Provenance:

| File | Origin |
|---|---|
| `pages_password_protected.pages` | [Apache Tika](https://github.com/apache/tika) test corpus (`tika-parser-apple-module`, Apache License 2.0), via upstream docling's `tests/data/pages/` |
| `proposal_simple.pages` (`pages5-file.pages`), `proposal_nested_dir.pages` (`pages5-extra-dir.pages`) | [libetonyek](https://github.com/LibreOffice/libetonyek) test data (MPL 2.0) |
| `two_sheets.numbers` (`test-1.numbers`), `account_statement.numbers` (`test-2.numbers`) | [numbers-parser](https://github.com/masaccio/numbers-parser) test data (MIT) |
| `one_slide.key` (`simple-oneslide.key`), `numeric_table.key` (`table.key`) | [keynote-parser](https://github.com/psobot/keynote-parser) test data (MIT) |

`numeric_table.key` pins the current v1 limitation on purpose: its table is
numeric-only, and numeric cell values live in the tile storage (not the
shared string table), so only the table name extracts until tile decoding
lands.
