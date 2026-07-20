"""The docling-parity namespaces: every common `from docling.X import Y`
must work verbatim after swapping the package name to `docling_rs`, and the
aliases must be the *same objects* as the flat `docling_rs` exports."""

import docling_rs


def test_document_converter_module():
    from docling_rs.document_converter import (
        ConversionResult,
        DocumentConverter,
        PdfFormatOption,
    )

    assert DocumentConverter is docling_rs.DocumentConverter
    assert PdfFormatOption is docling_rs.PdfFormatOption
    assert ConversionResult is docling_rs.ConversionResult


def test_datamodel_base_models():
    from docling_rs.datamodel.base_models import (
        ConversionStatus,
        DocumentStream,
        InputFormat,
    )

    assert InputFormat is docling_rs.InputFormat
    assert DocumentStream is docling_rs.DocumentStream
    assert ConversionStatus is docling_rs.ConversionStatus


def test_datamodel_pipeline_options():
    from docling_rs.datamodel.pipeline_options import (
        AcceleratorDevice,
        AcceleratorOptions,
        PdfPipelineOptions,
        TableFormerMode,
        TableStructureOptions,
    )

    assert PdfPipelineOptions is docling_rs.PdfPipelineOptions
    assert TableStructureOptions is docling_rs.TableStructureOptions
    assert TableFormerMode is docling_rs.TableFormerMode
    assert AcceleratorOptions is docling_rs.AcceleratorOptions
    assert AcceleratorDevice is docling_rs.AcceleratorDevice


def test_datamodel_accelerator_options():
    # docling >= 2.29 location; pipeline_options keeps the back-compat alias.
    from docling_rs.datamodel.accelerator_options import (
        AcceleratorDevice,
        AcceleratorOptions,
    )

    assert AcceleratorOptions is docling_rs.AcceleratorOptions
    assert AcceleratorDevice is docling_rs.AcceleratorDevice


def test_datamodel_document():
    from docling_rs.datamodel.document import ConversionResult, InputDocument

    assert ConversionResult is docling_rs.ConversionResult
    assert InputDocument is docling_rs.InputDocument


def test_exceptions():
    from docling_rs.exceptions import ConversionError

    assert ConversionError is docling_rs.ConversionError


def test_chunking_parity_names():
    from docling_rs.chunking import (
        BaseChunk,
        BaseChunker,
        DocChunk,
        HierarchicalChunker,
        HybridChunker,
    )

    assert BaseChunk is DocChunk
    assert issubclass(HierarchicalChunker, BaseChunker)
    assert issubclass(HybridChunker, BaseChunker)


def test_utils_model_downloader():
    from docling_rs.utils.model_downloader import download_models

    assert download_models is docling_rs.download_models


def test_datamodel_package_imports_whole():
    # `import docling_rs.datamodel` alone must expose the submodules, like
    # docling's package does.
    import docling_rs.datamodel as dm

    assert dm.base_models.InputFormat is docling_rs.InputFormat
    assert dm.pipeline_options.PdfPipelineOptions is docling_rs.PdfPipelineOptions
    assert dm.document.ConversionResult is docling_rs.ConversionResult


def test_migrated_docling_snippet_end_to_end(tmp_path):
    # A verbatim docling snippet after s/docling/docling_rs/ on the imports.
    from docling_rs.datamodel.base_models import InputFormat
    from docling_rs.datamodel.pipeline_options import PdfPipelineOptions
    from docling_rs.document_converter import DocumentConverter, PdfFormatOption

    src = tmp_path / "note.md"
    src.write_text("# Title\n\nSome body text.\n")

    conv = DocumentConverter(
        format_options={
            InputFormat.PDF: PdfFormatOption(pipeline_options=PdfPipelineOptions(do_ocr=False))
        }
    )
    result = conv.convert(src)
    assert result.status == "success"
    assert "Some body text." in result.document.export_to_markdown()
