"""docling-parity alias of ``docling.datamodel.base_models``::

    # was: from docling.datamodel.base_models import InputFormat, DocumentStream
    from docling_rs.datamodel.base_models import InputFormat, DocumentStream
"""

import enum
import math
from typing import Any, Dict, Iterable

from docling_core.types.doc import BoundingBox, CoordOrigin
from pydantic import BaseModel, Field, PrivateAttr, computed_field, field_validator, model_validator

from .. import ConversionStatus
from ..options import DocumentStream, InputFormat

ScoreValue = float


class QualityGrade(str, enum.Enum):
    """docling's score buckets for conversion confidence."""

    POOR = "poor"
    FAIR = "fair"
    GOOD = "good"
    EXCELLENT = "excellent"
    UNSPECIFIED = "unspecified"


def _nanmean(values: Iterable[float]) -> float:
    known = [value for value in values if not math.isnan(value)]
    return sum(known) / len(known) if known else math.nan


def _nanquantile(values: Iterable[float], q: float) -> float:
    known = sorted(value for value in values if not math.isnan(value))
    if not known:
        return math.nan
    pos = q * (len(known) - 1)
    lo = math.floor(pos)
    hi = math.ceil(pos)
    if lo == hi:
        return known[lo]
    fraction = pos - lo
    return known[lo] * (1.0 - fraction) + known[hi] * fraction


class PageConfidenceScores(BaseModel):
    """docling's per-page conversion-confidence scores."""

    parse_score: ScoreValue = math.nan
    layout_score: ScoreValue = math.nan
    table_score: ScoreValue = math.nan
    ocr_score: ScoreValue = math.nan

    @field_validator("parse_score", "layout_score", "table_score", "ocr_score", mode="before")
    @classmethod
    def _coerce_none_or_nan_str(cls, value):
        if value is None:
            return math.nan
        if isinstance(value, str) and value.strip().lower() in {"nan", "null", "none", ""}:
            return math.nan
        return value

    @staticmethod
    def _score_to_grade(score: ScoreValue) -> QualityGrade:
        if score < 0.5:
            return QualityGrade.POOR
        if score < 0.8:
            return QualityGrade.FAIR
        if score < 0.9:
            return QualityGrade.GOOD
        if score >= 0.9:
            return QualityGrade.EXCELLENT
        return QualityGrade.UNSPECIFIED

    @computed_field
    @property
    def mean_grade(self) -> QualityGrade:
        return self._score_to_grade(self.mean_score)

    @computed_field
    @property
    def low_grade(self) -> QualityGrade:
        return self._score_to_grade(self.low_score)

    @computed_field
    @property
    def mean_score(self) -> ScoreValue:
        return _nanmean([self.ocr_score, self.table_score, self.layout_score, self.parse_score])

    @computed_field
    @property
    def low_score(self) -> ScoreValue:
        return _nanquantile(
            [self.ocr_score, self.table_score, self.layout_score, self.parse_score], 0.05
        )


class ConfidenceReport(PageConfidenceScores):
    """Document scores plus per-page confidence, keyed by 1-based page number."""

    # Plain dict: 1-based keys make docling's 0-based pages[0] fail instead of silently defaulting.
    pages: Dict[int, PageConfidenceScores] = Field(default_factory=dict)
    _mean_score_override: ScoreValue = PrivateAttr(default=math.nan)
    _low_score_override: ScoreValue = PrivateAttr(default=math.nan)

    @staticmethod
    def _coerce_override_score(value: Any) -> ScoreValue:
        if value is None:
            return math.nan
        if isinstance(value, str) and value.strip().lower() in {"nan", "null", "none", ""}:
            return math.nan
        return float(value)

    @model_validator(mode="wrap")
    @classmethod
    def _accept_flat_confidence_scores(cls, value, handler):
        mean_override = math.nan
        low_override = math.nan
        if isinstance(value, dict):
            mean_override = cls._coerce_override_score(value.get("mean_score"))
            low_override = cls._coerce_override_score(value.get("low_score"))
            value = dict(value)
            value.pop("mean_score", None)
            value.pop("low_score", None)
            value.pop("mean_grade", None)
            value.pop("low_grade", None)
        model = handler(value)
        if not model.pages:
            model._mean_score_override = mean_override
            model._low_score_override = low_override
        return model

    @computed_field
    @property
    def mean_score(self) -> ScoreValue:
        if not math.isnan(self._mean_score_override):
            return self._mean_score_override
        if not self.pages:
            return super().mean_score
        return _nanmean(page.mean_score for page in self.pages.values())

    @computed_field
    @property
    def low_score(self) -> ScoreValue:
        if not math.isnan(self._low_score_override):
            return self._low_score_override
        if not self.pages:
            return super().low_score
        return _nanmean(page.low_score for page in self.pages.values())


class TextLineCell(BaseModel):
    """One text-line cell of a page (#1246) — the counterpart of the
    ``from_ocr`` cells in Python docling's
    ``ConversionResult.pages[i].parsed_page.textline_cells``
    (``generate_parsed_pages=True``): the recognized ``text``, its ``bbox``
    in page points with a top-left origin (``CoordOrigin.TOPLEFT``), the
    recognizer's ``confidence`` (mean emitted-character probability, the
    figure the page ``ocr_score`` averages) and ``from_ocr``. The cells are
    every crop the OCR read — the text lines, plus the word crops of a table
    region, which join the page's cells just the same. Only OCR'd cells are
    reported — text-layer cells are not, so ``from_ocr`` is always ``True``
    (a documented divergence, ``docs/MIGRATION.md``)."""

    text: str
    bbox: BoundingBox
    confidence: float
    from_ocr: bool = True

    @classmethod
    def _from_native(cls, cell: Dict[str, Any]) -> "TextLineCell":
        return cls(
            text=cell["text"],
            bbox=BoundingBox(
                l=cell["l"],
                t=cell["t"],
                r=cell["r"],
                b=cell["b"],
                coord_origin=CoordOrigin.TOPLEFT,
            ),
            # serde writes a NaN confidence as null.
            confidence=math.nan if cell.get("confidence") is None else cell["confidence"],
        )


__all__ = [
    "InputFormat",
    "DocumentStream",
    "ConversionStatus",
    "ConfidenceReport",
    "PageConfidenceScores",
    "QualityGrade",
    "TextLineCell",
]
