//! Excel number formats — the value a cell *displays* (#634).
//!
//! docling reads workbooks with `data_only=True` and wrote `str(cell.value)`,
//! so `$12.50` came out as `12.5`, `10%` as `0.1` and `Feb-25` as
//! `2025-02-01 00:00:00` (docling#4619). docling PR #4628 applies the cell's
//! stored number format instead, and this module is a port of its rules —
//! `_format_excel_value`, `_format_excel_number`, `_format_excel_date` and
//! openpyxl's `is_date_format` — taken as the specification: a *section* of
//! a numeric format is rendered when it is a plain `$`/`%` pattern
//! (`"$"#,##0.00`, `0%`, `($#,##0.00)`, `[$$-409]…`), with Excel's
//! half-up rounding (`0.125` at `0%` is `13%`), negative and zero sections
//! honoured; a date format is rendered token by token (`mmm-yy`,
//! `"Due "mmm d`, `h:mm AM/PM`), the built-in `mm-dd-yy` (14) and
//! `m/d/yy h:mm` (22) as ISO so the century and the day/month order
//! survive, `mm` next to `h`/`ss` as minutes, `h`/`d`/`m` unpadded; and
//! everything the PR does not interpret — elapsed `[h]:mm:ss`, conditions,
//! other locale markers, optional `#` decimals, scaling, `"EUR"` and other
//! currency words, plain `#,##0` and `00000` — stays `str(value)`, as it did,
//! so nothing reads worse than before. Formulas keep their cached results.
//!
//! The format codes come from the workbook itself: `xl/styles.xml`
//! (`numFmts` + `cellXfs`, [`xlsx_xf_formats`]) and each sheet's `c/@s`
//! style indexes ([`xlsx_cell_styles`]); for `.xls`, the Workbook stream's
//! `FORMAT` / `XF` records and the cells' `ixfe` ([`biff_formats`]). Built-in
//! ids are openpyxl's `BUILTIN_FORMATS` table. calamine, which reads the
//! values, exposes none of this (it only classifies a format as date or
//! duration, which is where `Data::DateTime` comes from).

use std::collections::HashMap;

use calamine::Data;

/// openpyxl's `BUILTIN_FORMATS` (`openpyxl.styles.numbers`) — the codes an
/// `xf` without a custom `numFmt` means. Ids 5–8 and 41–44 are what the US
/// build of Excel writes; a workbook may redefine them in its own `numFmts`
/// / `FORMAT` records, which take precedence.
pub(crate) fn builtin_format(id: u32) -> Option<&'static str> {
    Some(match id {
        0 => "General",
        1 => "0",
        2 => "0.00",
        3 => "#,##0",
        4 => "#,##0.00",
        5 => r##""$"#,##0_);("$"#,##0)"##,
        6 => r##""$"#,##0_);[Red]("$"#,##0)"##,
        7 => r##""$"#,##0.00_);("$"#,##0.00)"##,
        8 => r##""$"#,##0.00_);[Red]("$"#,##0.00)"##,
        9 => "0%",
        10 => "0.00%",
        11 => "0.00E+00",
        12 => "# ?/?",
        13 => "# ??/??",
        14 => "mm-dd-yy",
        15 => "d-mmm-yy",
        16 => "d-mmm",
        17 => "mmm-yy",
        18 => "h:mm AM/PM",
        19 => "h:mm:ss AM/PM",
        20 => "h:mm",
        21 => "h:mm:ss",
        22 => "m/d/yy h:mm",
        37 => "#,##0_);(#,##0)",
        38 => "#,##0_);[Red](#,##0)",
        39 => "#,##0.00_);(#,##0.00)",
        40 => "#,##0.00_);[Red](#,##0.00)",
        41 => r#"_(* #,##0_);_(* \(#,##0\);_(* "-"_);_(@_)"#,
        42 => r#"_("$"* #,##0_);_("$"* \(#,##0\);_("$"* "-"_);_(@_)"#,
        43 => r#"_(* #,##0.00_);_(* \(#,##0.00\);_(* "-"??_);_(@_)"#,
        44 => r#"_("$"* #,##0.00_)_("$"* \(#,##0.00\)_("$"* "-"??_)_(@_)"#,
        45 => "mm:ss",
        46 => "[h]:mm:ss",
        47 => "mmss.0",
        48 => "##0.0E+0",
        49 => "@",
        _ => return None,
    })
}

/// A number-format id's code: the workbook's own definition first, then the
/// built-in table. `None` for General (id 0) and unknown ids — "no format".
fn format_code(id: u32, custom: &HashMap<u32, String>) -> Option<String> {
    if let Some(code) = custom.get(&id) {
        return Some(code.clone());
    }
    if id == 0 {
        return None;
    }
    builtin_format(id).map(str::to_string)
}

/// The cell formats of one sheet: `xf` index → format code (shared by every
/// sheet of the workbook) and the cells carrying a non-default `xf`.
#[derive(Clone, Default)]
pub(crate) struct SheetFormats {
    pub(crate) xfs: std::sync::Arc<Vec<Option<String>>>,
    /// Absolute 0-based `(row, col)` → `xf` index, for the cells that name
    /// one; a cell without an `s` attribute uses `xf` 0 (General).
    pub(crate) cells: HashMap<(u32, u32), u32>,
}

impl SheetFormats {
    /// The format code of the cell at absolute `(row, col)`, when it has one.
    pub(crate) fn at(&self, row: usize, col: usize) -> Option<&str> {
        let xf = *self.cells.get(&(row as u32, col as u32))?;
        self.xfs.get(xf as usize)?.as_deref()
    }
}

// ---------------------------------------------------------------------------
// XLSX: styles.xml + a sheet's cell style indexes.

/// `xl/styles.xml` → `cellXfs` index → format code (`numFmts` first, then
/// the built-ins). A missing or unreadable part is an empty table — every
/// cell then prints its raw value, as before.
pub(crate) fn xlsx_xf_formats(styles_xml: &str) -> Vec<Option<String>> {
    let Ok(dom) = roxmltree::Document::parse(styles_xml) else {
        return Vec::new();
    };
    let root = dom.root_element();
    let custom: HashMap<u32, String> = root
        .children()
        .filter(|n| n.has_tag_name("numFmts"))
        .flat_map(|n| n.children())
        .filter(|n| n.has_tag_name("numFmt"))
        .filter_map(|n| {
            let id = n.attribute("numFmtId")?.parse::<u32>().ok()?;
            Some((id, n.attribute("formatCode")?.to_string()))
        })
        .collect();
    root.children()
        .filter(|n| n.has_tag_name("cellXfs"))
        .flat_map(|n| n.children())
        .filter(|n| n.has_tag_name("xf"))
        .map(|xf| {
            let id = xf
                .attribute("numFmtId")
                .and_then(|v| v.parse::<u32>().ok())
                .unwrap_or(0);
            format_code(id, &custom)
        })
        .collect()
}

/// The `s` (style index) attribute of every `<c>` of a worksheet part, keyed
/// by absolute 0-based `(row, col)` — a linear scan of the start tags, not a
/// DOM: a sheet may hold millions of cells. Cells without `s` are left out.
pub(crate) fn xlsx_cell_styles(sheet_xml: &str) -> HashMap<(u32, u32), u32> {
    let mut out = HashMap::new();
    let tag = cached_regex!(r"<c\s[^>]*>");
    let attr_r = cached_regex!(r#"\br="([A-Z]+)(\d+)""#);
    let attr_s = cached_regex!(r#"\bs="(\d+)""#);
    for m in tag.find_iter(sheet_xml) {
        let t = m.as_str();
        let Some(s) = attr_s.captures(t).and_then(|c| c[1].parse::<u32>().ok()) else {
            continue;
        };
        let Some(r) = attr_r.captures(t) else {
            continue;
        };
        let col = r[1]
            .bytes()
            .fold(0u32, |acc, b| acc * 26 + (b - b'A' + 1) as u32);
        let Ok(row) = r[2].parse::<u32>() else {
            continue;
        };
        if row == 0 || col == 0 {
            continue;
        }
        out.insert((row - 1, col - 1), s);
    }
    out
}

// ---------------------------------------------------------------------------
// XLS (BIFF5/8): FORMAT + XF records of the Workbook globals, the cells' ixfe.

/// `xf` index → format code, and each sheet's numeric cells' `xf` indexes.
pub(crate) type BiffFormats = (Vec<Option<String>>, Vec<HashMap<(u32, u32), u32>>);

/// The number formats of a BIFF workbook stream: `xf` index → code, and per
/// `BOUNDSHEET` (in file order, which is calamine's sheet order) the numeric
/// cells' `ixfe` — `NUMBER`, `RK`, `MULRK` and `FORMULA` records, the only
/// kinds a number format changes. Anything malformed ends the walk with what
/// was read; a cell without an entry prints raw.
pub(crate) fn biff_formats(stream: &[u8]) -> BiffFormats {
    let u16_at = |o: usize| -> Option<u16> {
        Some(u16::from_le_bytes(stream.get(o..o + 2)?.try_into().ok()?))
    };
    let u32_at = |o: usize| -> Option<u32> {
        Some(u32::from_le_bytes(stream.get(o..o + 4)?.try_into().ok()?))
    };
    let mut custom: HashMap<u32, String> = HashMap::new();
    let mut xf_ids: Vec<u32> = Vec::new();
    let mut sheet_offsets: Vec<usize> = Vec::new();
    let mut biff8 = true;
    // Workbook globals: from the first BOF to its EOF.
    let mut pos = 0usize;
    while let (Some(id), Some(len)) = (u16_at(pos), u16_at(pos + 2)) {
        let data = pos + 4;
        let len = len as usize;
        match id {
            0x0809 => biff8 = u16_at(data).is_some_and(|v| v >= 0x0600),
            // FORMAT: ifmt, then the code (XLUnicodeString in BIFF8, a byte
            // string in BIFF5).
            0x041E => {
                if let (Some(ifmt), Some(code)) =
                    (u16_at(data), biff_string(stream, data + 2, biff8))
                {
                    custom.insert(ifmt as u32, code);
                }
            }
            // XF: ifnt, ifmt, …
            0x00E0 => xf_ids.push(u16_at(data + 2).unwrap_or(0) as u32),
            // BOUNDSHEET: lbPlyPos, grbit, name.
            0x0085 => {
                if let Some(off) = u32_at(data) {
                    sheet_offsets.push(off as usize);
                }
            }
            0x000A => break,
            _ => {}
        }
        pos = data + len;
    }
    let xfs: Vec<Option<String>> = xf_ids.iter().map(|&id| format_code(id, &custom)).collect();
    let mut sheets = Vec::with_capacity(sheet_offsets.len());
    for start in sheet_offsets {
        let mut cells: HashMap<(u32, u32), u32> = HashMap::new();
        let mut pos = start;
        let mut first = true;
        while let (Some(id), Some(len)) = (u16_at(pos), u16_at(pos + 2)) {
            let data = pos + 4;
            let len = len as usize;
            match id {
                // A BOF that is not the substream's own opens an embedded
                // object's stream; the sheet's cells end there.
                0x0809 if !first => break,
                // NUMBER / RK / FORMULA: rw, col, ixfe, …
                0x0203 | 0x027E | 0x0006 => {
                    if let (Some(rw), Some(col), Some(ixfe)) =
                        (u16_at(data), u16_at(data + 2), u16_at(data + 4))
                    {
                        cells.insert((rw as u32, col as u32), ixfe as u32);
                    }
                }
                // MULRK: rw, colFirst, (ixfe, rk)*, colLast.
                0x00BD => {
                    if let (Some(rw), Some(col_first)) = (u16_at(data), u16_at(data + 2)) {
                        let n = len.saturating_sub(6) / 6;
                        for i in 0..n {
                            if let Some(ixfe) = u16_at(data + 4 + i * 6) {
                                cells.insert((rw as u32, col_first as u32 + i as u32), ixfe as u32);
                            }
                        }
                    }
                }
                0x000A => break,
                _ => {}
            }
            first = false;
            pos = data + len;
        }
        sheets.push(cells);
    }
    (xfs, sheets)
}

/// A BIFF string at `at`: BIFF8's `XLUnicodeString` (cch u16, flags, the
/// optional rich-text / extended headers, then 8-bit or UTF-16LE chars) or
/// BIFF5's byte string (cch u8, Windows-1252 chars).
fn biff_string(stream: &[u8], at: usize, biff8: bool) -> Option<String> {
    if !biff8 {
        let cch = *stream.get(at)? as usize;
        let bytes = stream.get(at + 1..at + 1 + cch)?;
        return Some(bytes.iter().map(|&b| super::doc::cp1252(b)).collect());
    }
    let cch = u16::from_le_bytes(stream.get(at..at + 2)?.try_into().ok()?) as usize;
    let flags = *stream.get(at + 2)?;
    let mut p = at + 3;
    if flags & 0x08 != 0 {
        p += 2; // cRun
    }
    if flags & 0x04 != 0 {
        p += 4; // cbExtRst
    }
    if flags & 0x01 != 0 {
        let bytes = stream.get(p..p + cch * 2)?;
        let units: Vec<u16> = bytes
            .chunks_exact(2)
            .map(|c| u16::from_le_bytes([c[0], c[1]]))
            .collect();
        Some(String::from_utf16_lossy(&units))
    } else {
        let bytes = stream.get(p..p + cch)?;
        Some(bytes.iter().map(|&b| super::doc::cp1252(b)).collect())
    }
}

// ---------------------------------------------------------------------------
// The value a cell displays.

/// A cell value as openpyxl types it: `int`/`float`, `datetime`, `time`
/// (an Excel serial in `[0, 1)`), `timedelta` (a duration format).
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum CellValue {
    Number(f64),
    /// Year, month, day, hour, minute, second, microsecond.
    DateTime(i64, u32, u32, u32, u32, u32, u32),
    /// Hour, minute, second, microsecond.
    Time(u32, u32, u32, u32),
    /// Whole microseconds (negative for a negative duration).
    Duration(i64),
}

/// openpyxl's `from_excel`: a serial in `[0, 1)` is a `time`; otherwise the
/// epoch (1899-12-30, or 1904-01-01) plus the days, the fraction rounded to
/// milliseconds, and Excel's phantom 1900-02-29 skipped (`0 < v < 60` adds
/// a day).
fn from_excel(serial: f64, is_1904: bool) -> CellValue {
    let day = serial.floor();
    let fraction = serial - day;
    let ms = (fraction * 86_400_000.0).round() as i64;
    if (0.0..1.0).contains(&serial) && ms < 86_400_000 {
        let (h, m, s, us) = split_ms(ms);
        return CellValue::Time(h, m, s, us);
    }
    let mut days = day as i64;
    if !is_1904 && serial > 0.0 && serial < 60.0 {
        days += 1;
    }
    let epoch = if is_1904 {
        days_from_civil(1904, 1, 1)
    } else {
        days_from_civil(1899, 12, 30)
    };
    let total_ms = (epoch + days) * 86_400_000 + ms;
    let civil_days = total_ms.div_euclid(86_400_000);
    let (h, m, s, us) = split_ms(total_ms.rem_euclid(86_400_000));
    let (y, mo, d) = civil_from_days(civil_days);
    CellValue::DateTime(y, mo, d, h, m, s, us)
}

/// openpyxl's `from_excel(…, timedelta=True)`: the duration in whole
/// microseconds, rounded to milliseconds when it has a fraction.
fn duration_from_excel(serial: f64) -> CellValue {
    let us = (serial * 86_400_000_000.0).round() as i64;
    let us = if us % 1_000_000 != 0 {
        let secs = us.div_euclid(1_000_000);
        let sub = us.rem_euclid(1_000_000);
        // `round(microseconds, -3)`: Python's half-even on an integer.
        let (q, r) = (sub / 1000, sub % 1000);
        let q = match r.cmp(&500) {
            std::cmp::Ordering::Greater => q + 1,
            std::cmp::Ordering::Equal if q % 2 == 1 => q + 1,
            _ => q,
        };
        secs * 1_000_000 + q * 1000
    } else {
        us
    };
    CellValue::Duration(us)
}

fn split_ms(ms: i64) -> (u32, u32, u32, u32) {
    let h = (ms / 3_600_000) as u32;
    let m = (ms / 60_000 % 60) as u32;
    let s = (ms / 1000 % 60) as u32;
    let us = (ms % 1000 * 1000) as u32;
    (h, m, s, us)
}

/// Days since 1970-01-01 of a proleptic Gregorian date (Howard Hinnant's
/// `days_from_civil`).
pub(crate) fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (m as i64 + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// The inverse of [`days_from_civil`].
pub(crate) fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// Monday = 0 … Sunday = 6 (Python's `weekday()`).
fn weekday(y: i64, m: u32, d: u32) -> usize {
    // 1970-01-01 was a Thursday (3).
    (days_from_civil(y, m, d) + 3).rem_euclid(7) as usize
}

const MONTHS: [&str; 12] = [
    "January",
    "February",
    "March",
    "April",
    "May",
    "June",
    "July",
    "August",
    "September",
    "October",
    "November",
    "December",
];
const WEEKDAYS: [&str; 7] = [
    "Monday",
    "Tuesday",
    "Wednesday",
    "Thursday",
    "Friday",
    "Saturday",
    "Sunday",
];

/// Python's `str()` of the value — docling's text before PR #4628 and the
/// fallback everywhere the PR declines to interpret a format.
fn python_str(value: CellValue) -> String {
    match value {
        CellValue::Number(f) => python_float(f),
        CellValue::DateTime(y, mo, d, h, m, s, us) => {
            let mut out = format!("{y:04}-{mo:02}-{d:02} {h:02}:{m:02}:{s:02}");
            if us != 0 {
                out.push_str(&format!(".{us:06}"));
            }
            out
        }
        CellValue::Time(h, m, s, us) => {
            let mut out = format!("{h:02}:{m:02}:{s:02}");
            if us != 0 {
                out.push_str(&format!(".{us:06}"));
            }
            out
        }
        CellValue::Duration(us) => {
            // `timedelta.__str__`: normalized days, then `H:MM:SS[.ffffff]`.
            let days = us.div_euclid(86_400_000_000);
            let rest = us.rem_euclid(86_400_000_000);
            let (h, m, s) = (
                rest / 3_600_000_000,
                rest / 60_000_000 % 60,
                rest / 1_000_000 % 60,
            );
            let frac = rest % 1_000_000;
            let mut out = String::new();
            if days != 0 {
                out.push_str(&format!(
                    "{days} day{}, ",
                    if days.abs() == 1 { "" } else { "s" }
                ));
            }
            out.push_str(&format!("{h}:{m:02}:{s:02}"));
            if frac != 0 {
                out.push_str(&format!(".{frac:06}"));
            }
            out
        }
    }
}

/// openpyxl hands docling an `int` for a whole number (no `.0`) and a
/// `float` otherwise — the backend's long-standing `format_number`.
pub(crate) fn python_float(f: f64) -> String {
    if f.is_finite() && f.fract() == 0.0 && f.abs() < 1e15 {
        format!("{}", f as i64)
    } else {
        format!("{f}")
    }
}

/// `str(value)` of a calamine cell as openpyxl would type it, for the paths
/// that apply no format (`format_cell` without a code, charts' raw grids).
pub(crate) fn raw_text(value: &Data, is_1904: bool) -> String {
    match value {
        Data::Empty => String::new(),
        // openpyxl reads strings through an XML parser, which normalises line
        // endings (`\r\n`/`\r` → `\n`); calamine keeps them raw, so do it here.
        Data::String(s) => s.replace("\r\n", "\n").replace('\r', "\n"),
        Data::Int(i) => i.to_string(),
        Data::Float(f) => python_float(*f),
        Data::Bool(b) => if *b { "True" } else { "False" }.to_string(),
        Data::DateTime(dt) => python_str(cell_value(dt, is_1904)),
        Data::DateTimeIso(s) => s.clone(),
        Data::DurationIso(s) => s.clone(),
        // openpyxl keeps an error cell's own text (`#N/A`, `#VALUE!`,
        // `#DIV/0!`), which is calamine's `Display`; its `Debug` is the enum
        // variant (`NA`, `Value`, `Div0`) and is never what a sheet shows
        // (#1020).
        Data::Error(e) => e.to_string(),
    }
}

/// A chart data cell (docling's `_chart_cell_text`, docling#4812): a date
/// in ISO 8601 — the date alone at midnight, else to the second
/// (`datetime_to_iso`) — anything else `str(value)`.
pub(crate) fn chart_text(value: &Data, is_1904: bool) -> String {
    match value {
        Data::DateTime(dt) if !dt.is_duration() => match cell_value(dt, is_1904) {
            CellValue::DateTime(y, mo, d, 0, 0, 0, 0) => format!("{y:04}-{mo:02}-{d:02}"),
            CellValue::DateTime(y, mo, d, h, m, s, _) => {
                format!("{y:04}-{mo:02}-{d:02}T{h:02}:{m:02}:{s:02}")
            }
            other => python_str(other),
        },
        other => raw_text(other, is_1904),
    }
}

fn cell_value(dt: &calamine::ExcelDateTime, is_1904: bool) -> CellValue {
    if dt.is_duration() {
        duration_from_excel(dt.as_f64())
    } else {
        from_excel(dt.as_f64(), is_1904)
    }
}

/// The text a cell displays: its value through its number format (`fmt`,
/// `None` = General), PR #4628's `_displayed_cell_text`.
pub(crate) fn displayed_text(value: &Data, fmt: Option<&str>, is_1904: bool) -> String {
    let Some(fmt) = fmt else {
        return raw_text(value, is_1904);
    };
    let typed = match value {
        Data::Int(i) => CellValue::Number(*i as f64),
        Data::Float(f) => CellValue::Number(*f),
        Data::DateTime(dt) => cell_value(dt, is_1904),
        other => return raw_text(other, is_1904),
    };
    // An `int` prints as an int wherever the PR writes `str(value)`.
    let raw = match value {
        Data::Int(i) => i.to_string(),
        _ => python_str(typed),
    };
    format_value(typed, fmt).unwrap_or(raw)
}

/// `_format_excel_value`: `None` where the PR writes `str(value)`.
pub(crate) fn format_value(value: CellValue, fmt: &str) -> Option<String> {
    let lower = fmt.to_ascii_lowercase();
    if matches!(lower.as_str(), "general" | "@" | "") {
        return None;
    }
    match value {
        CellValue::Number(f) => {
            // A number under a date format is not reinterpreted with an
            // assumed epoch (the workbook's own epoch typed the dates).
            if is_date_format(fmt) {
                return None;
            }
            format_number(f, fmt)
        }
        CellValue::Duration(_) => None,
        CellValue::DateTime(..) | CellValue::Time(..) => format_date(value, fmt),
    }
}

/// openpyxl's `is_date_format`: the first section, quoted text and bracket
/// groups other than `[h]`/`[hh]`/`[m]`/`[mm]`/`[s]`/`[ss]` stripped, holds
/// a `d`/`m`/`h`/`y`/`s` not escaped by `_` or `\`.
pub(crate) fn is_date_format(fmt: &str) -> bool {
    let first = fmt.split(';').next().unwrap_or("");
    let chars: Vec<char> = first.chars().collect();
    let mut stripped: Vec<char> = Vec::with_capacity(chars.len());
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if c == '"' {
            if let Some(end) = chars[i + 1..].iter().position(|&x| x == '"') {
                i += end + 2;
                continue;
            }
        } else if c == '[' {
            if let Some(end) = chars[i + 1..].iter().position(|&x| x == ']') {
                let inner: String = chars[i + 1..i + 1 + end].iter().collect();
                if !matches!(inner.as_str(), "h" | "hh" | "m" | "mm" | "s" | "ss") {
                    i += end + 2;
                    continue;
                }
            }
        }
        stripped.push(c);
        i += 1;
    }
    stripped.iter().enumerate().any(|(i, &c)| {
        "dmhysDMHYS".contains(c)
            && !matches!(
                i.checked_sub(1).map(|p| stripped[p]),
                Some('_') | Some('\\')
            )
    })
}

/// Python's `re.split(r';(?=(?:[^"]*"[^"]*")*[^"]*$)', s)`: split on the
/// semicolons outside double quotes.
fn split_sections(s: &str) -> Vec<String> {
    let mut out = vec![String::new()];
    let mut in_quotes = false;
    for c in s.chars() {
        match c {
            '"' => {
                in_quotes = !in_quotes;
                out.last_mut().unwrap().push(c);
            }
            ';' if !in_quotes => out.push(String::new()),
            _ => out.last_mut().unwrap().push(c),
        }
    }
    out
}

/// `_format_excel_number`: a plain currency/percent section, rendered with
/// Excel's half-up rounding; `None` for everything else.
fn format_number(value: f64, fmt: &str) -> Option<String> {
    if !value.is_finite() {
        return None;
    }
    let without_colors =
        cached_regex!(r"(?i)\[(?:Black|Blue|Cyan|Green|Magenta|Red|White|Yellow)\]")
            .replace_all(fmt, "");
    let currency_format =
        cached_regex!(r"\[\$\$-[0-9a-fA-F]+\]").replace_all(&without_colors, "\"$$\"");
    if currency_format.contains('[') || currency_format.contains("\\;") {
        return None;
    }
    let sections = split_sections(&currency_format);
    if sections.len() > 4 {
        return None;
    }
    let section_index = if value < 0.0 && sections.len() > 1 {
        1
    } else if value == 0.0 && sections.len() > 2 {
        2
    } else {
        0
    };
    let section = sections[section_index].as_str();
    if section.is_empty() {
        return Some(String::new());
    }
    if cached_regex!(r##"^"[^"]*"$"##).is_match(section) {
        return Some(section[1..section.len() - 1].to_string());
    }
    let literal_has_digit = cached_regex!(r#""([^"]*)""#)
        .captures_iter(section)
        .any(|c| c[1].contains(['0', '#', '.', ',', '%']));
    if literal_has_digit {
        return None;
    }
    let normalized = cached_regex!(r"_.")
        .replace_all(section, "")
        .replace('"', "");
    let normalized = cached_regex!(r"\\([()$ -])").replace_all(&normalized, "$1");
    let caps =
        cached_regex!(r"^([$( -]*)(#,##0|0)(?:\.(0{1,15}))?([$% )-]*)$").captures(&normalized)?;
    if normalized.matches('%').count() > 1 || normalized.matches('$').count() > 1 {
        return None;
    }
    if !normalized.contains('$') && !normalized.contains('%') {
        return None;
    }
    let decimals = caps.get(3).map_or(0, |m| m.len());
    let mut amount = DecimalText::from_f64(value.abs());
    if normalized.contains('%') {
        amount.shift(2);
    }
    let rounded = amount.quantize_half_up(decimals);
    let grouping = caps[2].contains(',');
    let sign = if value < 0.0 && section_index == 0 {
        "-"
    } else {
        ""
    };
    Some(format!(
        "{sign}{}{}{}",
        &caps[1],
        rounded.render(grouping, decimals),
        caps.get(4).map_or("", |m| m.as_str())
    ))
}

/// An exact decimal built from a float's shortest round-trip text — what
/// Python's `Decimal(str(value))` holds — for half-up rounding and grouping.
struct DecimalText {
    int: Vec<u8>,
    frac: Vec<u8>,
}

impl DecimalText {
    fn from_f64(f: f64) -> Self {
        let text = format!("{f}");
        let (i, fr) = text.split_once('.').unwrap_or((&text, ""));
        Self {
            int: i.bytes().map(|b| b - b'0').collect(),
            frac: fr.bytes().map(|b| b - b'0').collect(),
        }
    }

    /// Multiply by `10^places` (the `%` scaling).
    fn shift(&mut self, places: usize) {
        for _ in 0..places {
            let digit = if self.frac.is_empty() {
                0
            } else {
                self.frac.remove(0)
            };
            self.int.push(digit);
        }
        while self.int.len() > 1 && self.int[0] == 0 {
            self.int.remove(0);
        }
    }

    /// `quantize(Decimal(1).scaleb(-decimals), ROUND_HALF_UP)`.
    fn quantize_half_up(mut self, decimals: usize) -> Self {
        if self.frac.len() > decimals {
            let round_up = self.frac[decimals] >= 5;
            self.frac.truncate(decimals);
            if round_up {
                let mut carry = true;
                for d in self.frac.iter_mut().rev() {
                    if !carry {
                        break;
                    }
                    *d += 1;
                    carry = *d == 10;
                    if carry {
                        *d = 0;
                    }
                }
                if carry {
                    for d in self.int.iter_mut().rev() {
                        *d += 1;
                        carry = *d == 10;
                        if carry {
                            *d = 0;
                        } else {
                            break;
                        }
                    }
                    if carry {
                        self.int.insert(0, 1);
                    }
                }
            }
        }
        while self.frac.len() < decimals {
            self.frac.push(0);
        }
        self
    }

    /// Python's `f"{d:,.{decimals}f}"`.
    fn render(&self, grouping: bool, decimals: usize) -> String {
        let mut out = String::new();
        let n = self.int.len();
        for (i, d) in self.int.iter().enumerate() {
            if grouping && i > 0 && (n - i).is_multiple_of(3) {
                out.push(',');
            }
            out.push((b'0' + d) as char);
        }
        if decimals > 0 {
            out.push('.');
            for d in &self.frac[..decimals] {
                out.push((b'0' + d) as char);
            }
        }
        out
    }
}

/// `_format_excel_date`: the format's tokens rendered from the value; `None`
/// where the PR falls back to `str(value)`.
fn format_date(value: CellValue, fmt: &str) -> Option<String> {
    let lower = fmt.to_ascii_lowercase();
    let is_time = matches!(value, CellValue::Time(..));
    let (y, mo, d, h, mi, s, us) = match value {
        CellValue::DateTime(y, mo, d, h, mi, s, us) => (y, mo, d, h, mi, s, us),
        // `datetime.combine(date(1900, 1, 1), value)`.
        CellValue::Time(h, mi, s, us) => (1900, 1, 1, h, mi, s, us),
        _ => return None,
    };
    if lower == "mm-dd-yy" && !is_time {
        return Some(format!("{y:04}-{mo:02}-{d:02}"));
    }
    if lower == "m/d/yy h:mm" && !is_time {
        let mut out = format!("{y:04}-{mo:02}-{d:02} {h:02}:{mi:02}:{s:02}");
        if us != 0 {
            out.push_str(&format!(".{us:06}"));
        }
        return Some(out);
    }
    let token_re = cached_regex!(
        r#"(?i)"[^"]*"|\\.|am/pm|yyyy|yy|mmmm|mmm|mm|m|dddd|ddd|dd|d|hh|h|ss|s|[ :/.,-]"#
    );
    let parts: Vec<&str> = token_re.find_iter(fmt).map(|m| m.as_str()).collect();
    if parts.concat() != fmt {
        return None;
    }
    let tokens: Vec<String> = parts
        .iter()
        .filter(|p| p.chars().next().is_some_and(|c| c.is_alphabetic()))
        .map(|p| p.to_ascii_lowercase())
        .collect();
    if is_time
        && tokens
            .iter()
            .any(|t| t.starts_with('y') || t.starts_with('d'))
    {
        return None;
    }
    let twelve_hour = tokens.iter().any(|t| t == "am/pm");
    let mut rendered = String::new();
    let mut token_index = 0;
    for part in parts {
        let first = part.chars().next()?;
        if first == '"' {
            rendered.push_str(&part[1..part.len() - 1]);
        } else if first == '\\' {
            rendered.push_str(&part[1..]);
        } else if !first.is_alphabetic() {
            rendered.push_str(part);
        } else {
            let token = tokens[token_index].as_str();
            let before = if token_index > 0 {
                tokens[token_index - 1].as_str()
            } else {
                ""
            };
            let after = tokens.get(token_index + 1).map_or("", |t| t.as_str());
            token_index += 1;
            match token {
                "m" | "mm" | "d" | "dd" | "h" | "hh" | "s" | "ss" => {
                    let minute = matches!(token, "m" | "mm")
                        && (matches!(before, "h" | "hh") || matches!(after, "s" | "ss"));
                    if is_time && matches!(token, "m" | "mm") && !minute {
                        return None;
                    }
                    let number = if minute {
                        mi
                    } else if token.starts_with('s') {
                        s
                    } else if token.starts_with('h') {
                        if twelve_hour {
                            let n = h % 12;
                            if n == 0 {
                                12
                            } else {
                                n
                            }
                        } else {
                            h
                        }
                    } else if token.starts_with('d') {
                        d
                    } else {
                        mo
                    };
                    rendered.push_str(&format!("{number:0width$}", width = token.len()));
                }
                "am/pm" => rendered.push_str(if h < 12 { "AM" } else { "PM" }),
                "yyyy" => rendered.push_str(&format!("{y:04}")),
                "yy" => rendered.push_str(&format!("{:02}", y.rem_euclid(100))),
                "mmm" => rendered.push_str(&MONTHS[(mo - 1) as usize][..3]),
                "mmmm" => rendered.push_str(MONTHS[(mo - 1) as usize]),
                "ddd" => rendered.push_str(&WEEKDAYS[weekday(y, mo, d)][..3]),
                "dddd" => rendered.push_str(WEEKDAYS[weekday(y, mo, d)]),
                _ => return None,
            }
        }
    }
    Some(rendered)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dt(y: i64, mo: u32, d: u32, h: u32, mi: u32, s: u32) -> CellValue {
        CellValue::DateTime(y, mo, d, h, mi, s, 0)
    }
    fn t(h: u32, mi: u32, s: u32) -> CellValue {
        CellValue::Time(h, mi, s, 0)
    }
    fn n(f: f64) -> CellValue {
        CellValue::Number(f)
    }
    /// `_format_excel_value` with its `str(value)` fallback.
    fn fmt(value: CellValue, code: &str) -> String {
        format_value(value, code).unwrap_or_else(|| python_str(value))
    }

    /// #1020: an error cell reads as the sheet shows it — `#N/A`, `#VALUE!`,
    /// `#DIV/0!` — not as calamine's enum variant (`NA`, `Value`, `Div0`).
    #[test]
    fn error_cells_keep_their_excel_text() {
        use calamine::CellErrorType as E;
        for (err, text) in [(E::NA, "#N/A"), (E::Value, "#VALUE!"), (E::Div0, "#DIV/0!")] {
            assert_eq!(raw_text(&Data::Error(err), false), text);
        }
    }

    /// PR #4628's `test_format_excel_value_matches_excel_display` and
    /// `test_excel_review_number_format_regressions`, verbatim.
    #[test]
    fn matches_docling_pr_4628() {
        assert_eq!(fmt(n(12.5), r##""$"#,##0.00"##), "$12.50");
        assert_eq!(fmt(n(0.1), "0%"), "10%");
        assert_eq!(fmt(dt(2025, 2, 1, 0, 0, 0), "mmm-yy"), "Feb-25");
        assert_eq!(
            fmt(dt(2024, 1, 2, 0, 0, 0), r"yyyy\-mm\-dd\ hh:mm:ss"),
            "2024-01-02 00:00:00"
        );
        assert_eq!(fmt(n(12.5), "General"), "12.5");
        assert_eq!(fmt(n(1.0), r"\1\.0#"), "1");
        let cases: Vec<(CellValue, &str, &str)> = vec![
            (dt(2025, 2, 1, 0, 0, 0), "mm-dd-yy", "2025-02-01"),
            (
                dt(2025, 2, 1, 13, 30, 0),
                "m/d/yy h:mm",
                "2025-02-01 13:30:00",
            ),
            (t(0, 7, 45), "mm:ss", "07:45"),
            (t(13, 30, 0), "h:mm AM/PM", "1:30 PM"),
            (t(0, 7, 0), "hh:mm AM/PM", "12:07 AM"),
            (t(13, 30, 0), "h:mm", "13:30"),
            (dt(2025, 2, 1, 0, 0, 0), "d-mmm-yy", "1-Feb-25"),
            (dt(2025, 2, 1, 0, 0, 0), r#""Due "mmm d"#, "Due Feb 1"),
            (dt(2025, 2, 1, 0, 0, 0), r#""Due; "mmm d"#, "Due; Feb 1"),
            (dt(2025, 2, 1, 0, 0, 0), "yyyy-mm-dd", "2025-02-01"),
            (n(12.5), "[$$-409]#,##0.00", "$12.50"),
            (n(-5.0), r##""$"#,##0.00_);\("$"#,##0.00\)"##, "($5.00)"),
            (n(-5.0), r##""$"#,##0.00;[Red]"$"-#,##0.00"##, "$-5.00"),
            (n(-5.0), r##""$"#,##0.00"##, "-$5.00"),
            (n(0.125), "0%", "13%"),
            (n(-0.125), "0%", "-13%"),
            (n(0.0125), "0.00%", "1.25%"),
            (n(12.5), r##""$"#,##0"##, "$13"),
            (n(-12.5), r##""$"#,##0"##, "-$13"),
            (n(1234.565), r##""$"#,##0.00"##, "$1,234.57"),
            (n(0.0), r#""$"0;("$"0);"-""#, "-"),
            (n(0.0), r#""$"0;("$"0);"#, ""),
            (n(12.5), r#"0.00"$""#, "12.50$"),
            (
                CellValue::Duration(26 * 3_600_000_000 + 7 * 60_000_000),
                "[h]:mm:ss",
                "1 day, 2:07:00",
            ),
            (
                dt(2025, 2, 1, 0, 0, 0),
                "[$-409]mmmm d, yyyy",
                "2025-02-01 00:00:00",
            ),
            (n(0.125), "[>=1]0%;0.0%", "0.125"),
            (n(12.5), r#""$"0.##"#, "12.5"),
            (n(12.5), r#""$0"0.00"#, "12.5"),
        ];
        for (value, code, expected) in cases {
            assert_eq!(fmt(value, code), expected, "{value:?} with {code:?}");
        }
    }

    /// Beyond the PR's table: weekday and month names, large and tiny
    /// amounts, the reporter's formats (#634) — `0.0%` renders, `"EUR"`,
    /// `#,##0` and `00000` stay raw — and the serial → value typing.
    #[test]
    fn further_cases_match_the_pr_run_through_openpyxl() {
        assert_eq!(
            fmt(dt(2025, 2, 1, 0, 0, 0), "dddd, mmmm d, yyyy"),
            "Saturday, February 1, 2025"
        );
        assert_eq!(
            fmt(dt(2024, 1, 2, 0, 0, 0), r"yyyy\-mm\-dd\ h:mm:ss"),
            "2024-01-02 0:00:00"
        );
        assert_eq!(
            fmt(n(123456789012.345), r##""$"#,##0.00"##),
            "$123,456,789,012.35"
        );
        assert_eq!(fmt(n(0.000001234), "0.00%"), "0.00%");
        assert_eq!(fmt(t(1, 2, 3), "h:mm:ss AM/PM"), "1:02:03 AM");
        assert_eq!(fmt(dt(2024, 1, 2, 5, 6, 7), "h:mm:ss"), "5:06:07");
        assert_eq!(fmt(n(0.15), "0.0%"), "15.0%");
        assert_eq!(fmt(n(1234.5), r#""EUR" #,##0.00"#), "1234.5");
        assert_eq!(fmt(dt(2024, 1, 31, 0, 0, 0), "dd/mm/yyyy"), "31/01/2024");
        assert_eq!(fmt(n(1234567.891), "#,##0"), "1234567.891");
        assert_eq!(fmt(n(42.0), "00000"), "42");
        assert_eq!(fmt(n(12345.678), "0.00E+00"), "12345.678");
        assert_eq!(fmt(n(3.5), "@"), "3.5");
        // Serials: a fraction is a time, 1900's phantom leap day, 1904 epoch.
        assert_eq!(from_excel(0.5, false), t(12, 0, 0));
        assert_eq!(from_excel(1.0, false), dt(1900, 1, 1, 0, 0, 0));
        assert_eq!(from_excel(59.0, false), dt(1900, 2, 28, 0, 0, 0));
        assert_eq!(from_excel(61.0, false), dt(1900, 3, 1, 0, 0, 0));
        assert_eq!(from_excel(45658.5625, false), dt(2025, 1, 1, 13, 30, 0));
        assert_eq!(from_excel(44196.0, true), dt(2025, 1, 1, 0, 0, 0));
        assert_eq!(
            python_str(from_excel(45658.5, false)),
            "2025-01-01 12:00:00"
        );
        assert_eq!(
            python_str(duration_from_excel(1.0 + 2.0 / 24.0 + 7.0 / 1440.0)),
            "1 day, 2:07:00"
        );
        assert_eq!(
            python_str(CellValue::Duration(-3_600_000_000)),
            "-1 day, 23:00:00"
        );
        // openpyxl's date detection.
        assert!(is_date_format("mmm-yy"));
        assert!(is_date_format("[h]:mm:ss"));
        assert!(!is_date_format(r##""$"#,##0.00"##));
        assert!(!is_date_format("[$-409]0.00"));
        assert!(!is_date_format(r"\d0"));
        assert!(!is_date_format("0.0%"));
        assert!(is_date_format(r#""Due "mmm d"#));
    }

    /// The cell-style readers: a styles part with custom and built-in ids, a
    /// sheet's `s` attributes in either attribute order.
    #[test]
    fn xlsx_style_tables_resolve_codes() {
        let styles = r#"<styleSheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"><numFmts count="1"><numFmt numFmtId="164" formatCode="0.0%"/></numFmts><cellXfs count="4"><xf numFmtId="0"/><xf numFmtId="164"/><xf numFmtId="7"/><xf numFmtId="999"/></cellXfs></styleSheet>"#;
        let xfs = xlsx_xf_formats(styles);
        assert_eq!(
            xfs,
            vec![
                None,
                Some("0.0%".into()),
                Some(r##""$"#,##0.00_);("$"#,##0.00)"##.into()),
                None
            ]
        );
        let sheet = r#"<worksheet><sheetData><row r="1"><c r="A1" s="1"><v>0.15</v></c><c s="2" r="AB3" t="n"><v>1</v></c><c r="C1"><v>2</v></c></row></sheetData></worksheet>"#;
        let cells = xlsx_cell_styles(sheet);
        assert_eq!(cells.get(&(0, 0)), Some(&1));
        assert_eq!(cells.get(&(2, 27)), Some(&2));
        assert_eq!(cells.get(&(0, 2)), None);
        let formats = SheetFormats {
            xfs: std::sync::Arc::new(xfs),
            cells,
        };
        assert_eq!(formats.at(0, 0), Some("0.0%"));
        assert_eq!(
            displayed_text(&Data::Float(0.15), formats.at(0, 0), false),
            "15.0%"
        );
        assert_eq!(
            displayed_text(&Data::Float(0.15), formats.at(0, 2), false),
            "0.15"
        );
        assert_eq!(displayed_text(&Data::Int(3), Some("0%"), false), "300%");
        assert_eq!(displayed_text(&Data::Int(3), Some("[h]:mm"), false), "3");
        assert_eq!(displayed_text(&Data::Bool(true), Some("0%"), false), "True");
    }
}
