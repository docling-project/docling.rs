//! One packed cell of a `TST` table, and how a spreadsheet shows its value —
//! docling's `iwork/cells.py` and the rendering half of
//! `iwork/numbers_content.py` (docling#4113, docling#5101, docling#5087;
//! #1031).
//!
//! A table does not store its cells as messages. Each row of a tile carries
//! them packed into one buffer, and a cell inside it is a short header, a
//! bitmask saying which fields follow, and then those fields. Apple has
//! written two layouts of it:
//!
//! * **Version 5**, written since 2017, describes itself: the bitmask says
//!   which of a fixed list of fields the cell carries, in that order. A
//!   numeric cell holds an IEEE 754-2008 decimal128.
//! * **Version 4**, written before it, does not describe itself: one
//!   four-byte identifier per set bit, a single IEEE double in front of the
//!   last identifier when the cell's type carries a value, and the key of a
//!   text cell's string *as* that last identifier. The length that implies
//!   is checked against the bytes present, so a cell that does not fit the
//!   layout yields nothing rather than misread bytes.
//!
//! Pages reads the text of a cell through here too (docling's shared
//! `iwa_cell_text`), Numbers its typed values.

use std::collections::HashMap;

/// A table's shared value lists, keyed as its cells reference them.
#[derive(Default)]
pub(crate) struct CellValues {
    pub strings: HashMap<u32, String>,
    pub rich_text: HashMap<u32, String>,
}

/// One packed cell, decoded into whichever pieces it turned out to carry: text
/// from one of the shared value lists, or a number whose meaning the type
/// names (a date and a duration are both spans of seconds, a boolean a number
/// that is positive or not). `style` is the key of the cell's style in the
/// table's style list — read only from a version 5 cell.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Cell {
    pub ty: u8,
    pub text: Option<String>,
    pub number: Option<Number>,
    pub style: Option<u32>,
}

/// A cell's number: exact when it came from a decimal128, a double otherwise.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum Number {
    Decimal(Decimal),
    Double(f64),
}

impl Number {
    pub(crate) fn as_f64(self) -> f64 {
        match self {
            Number::Decimal(d) => d.to_f64(),
            Number::Double(v) => v,
        }
    }
}

/// A finite decimal: `(-1)^negative × coefficient × 10^exponent`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct Decimal {
    pub negative: bool,
    pub coefficient: u128,
    pub exponent: i32,
}

impl Decimal {
    fn to_f64(self) -> f64 {
        format!(
            "{}{}e{}",
            if self.negative { "-" } else { "" },
            self.coefficient,
            self.exponent
        )
        .parse()
        .unwrap_or(f64::NAN)
    }
}

pub(crate) const CELL_TYPE_EMPTY: u8 = 0;
pub(crate) const CELL_TYPE_NUMBER: u8 = 2;
pub(crate) const CELL_TYPE_TEXT: u8 = 3;
pub(crate) const CELL_TYPE_DATE: u8 = 5;
pub(crate) const CELL_TYPE_BOOL: u8 = 6;
pub(crate) const CELL_TYPE_DURATION: u8 = 7;
pub(crate) const CELL_TYPE_RICH_TEXT: u8 = 9;
pub(crate) const CELL_TYPE_CURRENCY: u8 = 10;

const CELL_VERSION_LEGACY: u8 = 4;
const CELL_VERSION_CURRENT: u8 = 5;
/// Where a version 4 cell keeps the bitmask of the fields it carries.
const CELL_FLAGS_OFFSET_LEGACY: usize = 4;
const CELL_VALUE_WIDTH: usize = 8;
const CELL_IDENTIFIER_WIDTH: usize = 4;
/// Where a version 5 cell keeps its flags, and where its values begin.
const CELL_FLAGS_OFFSET: usize = 8;
const CELL_VALUES_OFFSET: usize = 12;
const CELL_FLAG_DECIMAL: u32 = 0x1;
const CELL_FLAG_DOUBLE: u32 = 0x2;
const CELL_FLAG_SECONDS: u32 = 0x4;
const CELL_FLAG_STRING: u32 = 0x8;
const CELL_FLAG_RICH_TEXT: u32 = 0x10;
const CELL_FLAG_CELL_STYLE: u32 = 0x20;
/// The values a version 5 cell may hold, in the order they are laid out;
/// nothing after the style key is wanted, so the walk stops there.
const CELL_VALUE_WIDTHS: [(u32, usize); 6] = [
    (CELL_FLAG_DECIMAL, 16),
    (CELL_FLAG_DOUBLE, 8),
    (CELL_FLAG_SECONDS, 8),
    (CELL_FLAG_STRING, 4),
    (CELL_FLAG_RICH_TEXT, 4),
    (CELL_FLAG_CELL_STYLE, 4),
];
/// Subtracted from a decimal128's stored exponent to get the real one.
const DECIMAL128_BIAS: i32 = 6176;

fn is_numeric(ty: u8) -> bool {
    matches!(
        ty,
        CELL_TYPE_NUMBER
            | CELL_TYPE_DATE
            | CELL_TYPE_BOOL
            | CELL_TYPE_DURATION
            | CELL_TYPE_CURRENCY
    )
}

/// `iwa_cell_text`: the text of one packed cell, `None` for an empty cell or
/// one holding a number.
pub(crate) fn cell_text(storage: &[u8], start: usize, values: &CellValues) -> Option<String> {
    cell(storage, start, values).and_then(|c| c.text)
}

/// `iwa_cell`: one packed cell in either layout, `None` when the bytes there
/// do not describe one this reader recognises.
pub(crate) fn cell(storage: &[u8], start: usize, values: &CellValues) -> Option<Cell> {
    if start.checked_add(CELL_VALUES_OFFSET)? > storage.len() {
        return None;
    }
    match storage[start] {
        CELL_VERSION_LEGACY => legacy_cell(storage, start, values),
        CELL_VERSION_CURRENT => current_cell(storage, start, values),
        _ => None,
    }
}

fn empty(ty: u8) -> Cell {
    Cell {
        ty,
        text: None,
        number: None,
        style: None,
    }
}

fn legacy_cell(storage: &[u8], start: usize, values: &CellValues) -> Option<Cell> {
    let ty = storage[start + 1];
    let flags = read_u32(storage, start + CELL_FLAGS_OFFSET_LEGACY)?;
    let identifiers = flags.count_ones() as usize;
    if identifiers == 0 {
        return None;
    }
    let numeric = is_numeric(ty);
    let length = CELL_VALUES_OFFSET
        + CELL_IDENTIFIER_WIDTH * identifiers
        + if numeric { CELL_VALUE_WIDTH } else { 0 };
    if start + length > storage.len() {
        return None;
    }
    let last = start + length - CELL_IDENTIFIER_WIDTH;
    if numeric {
        return Some(Cell {
            number: read_double(storage, last - CELL_VALUE_WIDTH).map(Number::Double),
            ..empty(ty)
        });
    }
    if ty != CELL_TYPE_TEXT {
        return None;
    }
    // A key that names nothing in the table means the layout was read wrong.
    let text = values.strings.get(&read_u32(storage, last)?)?.clone();
    Some(Cell {
        text: Some(text),
        ..empty(ty)
    })
}

fn current_cell(storage: &[u8], start: usize, values: &CellValues) -> Option<Cell> {
    let ty = storage[start + 1];
    let flags = read_u32(storage, start + CELL_FLAGS_OFFSET)?;
    let mut offset = start + CELL_VALUES_OFFSET;
    let mut decoded = empty(ty);
    for (flag, width) in CELL_VALUE_WIDTHS {
        if flags & flag == 0 {
            continue;
        }
        if offset + width > storage.len() {
            return None;
        }
        match flag {
            CELL_FLAG_DECIMAL => {
                decoded.number = read_decimal128(storage, offset).map(Number::Decimal)
            }
            CELL_FLAG_DOUBLE | CELL_FLAG_SECONDS => {
                decoded.number = read_double(storage, offset).map(Number::Double)
            }
            CELL_FLAG_STRING => {
                decoded.text =
                    read_u32(storage, offset).and_then(|k| values.strings.get(&k).cloned())
            }
            CELL_FLAG_RICH_TEXT => {
                decoded.text =
                    read_u32(storage, offset).and_then(|k| values.rich_text.get(&k).cloned())
            }
            _ => decoded.style = read_u32(storage, offset),
        }
        offset += width;
    }
    Some(decoded)
}

fn read_u32(buffer: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_le_bytes(buffer.get(at..at + 4)?.try_into().ok()?))
}

fn read_double(buffer: &[u8], at: usize) -> Option<f64> {
    Some(f64::from_le_bytes(buffer.get(at..at + 8)?.try_into().ok()?))
}

/// `read_decimal128`: an IEEE 754-2008 decimal128 in its binary integer
/// encoding — a sign bit, a biased fourteen-bit exponent and a coefficient.
/// The combinations that mean an infinity or a NaN are rejected.
pub(crate) fn read_decimal128(buffer: &[u8], at: usize) -> Option<Decimal> {
    let field = buffer.get(at..at + 16)?;
    if field[15] & 0x78 == 0x78 {
        return None;
    }
    let exponent =
        ((i32::from(field[15] & 0x7F) << 7) | i32::from(field[14] >> 1)) - DECIMAL128_BIAS;
    let mut coefficient = u128::from(field[14] & 0x1);
    for &byte in field[..14].iter().rev() {
        coefficient = coefficient * 256 + u128::from(byte);
    }
    Some(Decimal {
        negative: field[15] & 0x80 != 0,
        coefficient,
        exponent,
    })
}

// --- rendering (numbers_content.py) ----------------------------------------

/// `format_number`: a numeric cell the way the spreadsheet shows it, near
/// enough — Python's `f"{value.normalize(_EXACT_CONTEXT):f}"`, every digit
/// a decimal128 encodes kept, no exponent; a double through its shortest
/// round-tripping form (`repr`), so `4650` and not `4650.0`.
pub(crate) fn format_number(value: Number) -> String {
    match value {
        Number::Decimal(d) => format_decimal(d),
        Number::Double(v) if v.is_nan() => "nan".into(),
        Number::Double(v) if v.is_infinite() => if v > 0.0 { "inf" } else { "-inf" }.into(),
        // Rust's `Display` for a finite double is its shortest round-trip
        // digits, written out without an exponent — what `Decimal(repr(v))`
        // normalized and formatted with `f` gives.
        Number::Double(v) => {
            let s = format!("{v}");
            if s == "-0" {
                "-0".into()
            } else {
                s
            }
        }
    }
}

/// Python's `Decimal.normalize()` then `format(…, "f")`.
fn format_decimal(d: Decimal) -> String {
    let sign = if d.negative { "-" } else { "" };
    if d.coefficient == 0 {
        return format!("{sign}0");
    }
    let (mut coefficient, mut exponent) = (d.coefficient, d.exponent);
    while coefficient % 10 == 0 {
        coefficient /= 10;
        exponent += 1;
    }
    let digits = coefficient.to_string();
    if exponent >= 0 {
        return format!("{sign}{digits}{}", "0".repeat(exponent as usize));
    }
    let point = digits.len() as i64 + i64::from(exponent);
    if point > 0 {
        let (int, frac) = digits.split_at(point as usize);
        format!("{sign}{int}.{frac}")
    } else {
        format!("{sign}0.{}{digits}", "0".repeat((-point) as usize))
    }
}

/// `format_bool`: Numbers stores a boolean as a number.
pub(crate) fn format_bool(value: f64) -> String {
    if value > 0.0 { "True" } else { "False" }.into()
}

/// Seconds from 0001-01-01 to the Apple epoch, 2001-01-01 (UTC).
const APPLE_EPOCH_SECONDS: i128 = 63_113_904_000;
/// Python's `datetime` range: 0001-01-01 to 9999-12-31 23:59:59.999999.
const MAX_DATETIME_SECONDS: i128 = 315_537_897_600;
/// Python's `timedelta` limit: 999999999 days.
const MAX_TIMEDELTA_DAYS: i128 = 999_999_999;

/// `timedelta(seconds=float)`: the span in whole microseconds, rounded half
/// to even as Python rounds it; `None` past what a `timedelta` holds.
fn microseconds(seconds: f64) -> Option<i128> {
    if !seconds.is_finite() {
        return None;
    }
    let micros = (seconds * 1e6).round_ties_even();
    let limit = (MAX_TIMEDELTA_DAYS + 1) as f64 * 86_400e6;
    (micros.abs() < limit).then_some(micros as i128)
}

/// `moment`: an instant from seconds after the Apple epoch, as seconds from
/// 0001-01-01 plus microseconds; `None` outside what a `datetime` holds.
fn moment(seconds: f64) -> Option<(i128, i128)> {
    let micros = microseconds(seconds)?;
    let total = APPLE_EPOCH_SECONDS * 1_000_000 + micros;
    let (secs, micro) = (total.div_euclid(1_000_000), total.rem_euclid(1_000_000));
    (0..MAX_DATETIME_SECONDS)
        .contains(&secs)
        .then_some((secs, micro))
}

/// The civil date of a day count from 0001-01-01 (proleptic Gregorian).
fn civil(days: i128) -> (i128, u32, u32) {
    // Days from 0000-03-01, Howard Hinnant's algorithm.
    let z = days + 306;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// `format_date`: a date cell, `%Y-%m-%d %H:%M:%S`; `None` when it lands
/// outside what a `datetime` can hold (docling leaves such a cell empty).
pub(crate) fn format_date(seconds: f64) -> Option<String> {
    let (secs, _) = moment(seconds)?;
    let (y, m, d) = civil(secs.div_euclid(86_400));
    let s = secs.rem_euclid(86_400);
    Some(format!(
        "{y:04}-{m:02}-{d:02} {:02}:{:02}:{:02}",
        s / 3600,
        s / 60 % 60,
        s % 60
    ))
}

/// The ISO form of an instant, `isoformat(timespec="milliseconds")` with the
/// UTC offset — a comment's time.
pub(crate) fn format_timestamp(seconds: f64) -> Option<String> {
    let (secs, micro) = moment(seconds)?;
    let (y, m, d) = civil(secs.div_euclid(86_400));
    let s = secs.rem_euclid(86_400);
    Some(format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}.{:03}+00:00",
        s / 3600,
        s / 60 % 60,
        s % 60,
        micro / 1000
    ))
}

/// `format_duration`: `str(timedelta(seconds=…))` — `H:MM:SS[.ffffff]`, a
/// `D day[s], ` prefix past a day, a negative span as negative days and a
/// positive time (`-1 day, 23:59:50`); `None` past a `timedelta`'s range.
pub(crate) fn format_duration(seconds: f64) -> Option<String> {
    let micros = microseconds(seconds)?;
    let days = micros.div_euclid(86_400_000_000);
    let rest = micros.rem_euclid(86_400_000_000);
    if days.abs() > MAX_TIMEDELTA_DAYS {
        return None;
    }
    let (secs, micro) = (rest / 1_000_000, rest % 1_000_000);
    let mut out = String::new();
    if days != 0 {
        let plural = if days.abs() != 1 { "s" } else { "" };
        out.push_str(&format!("{days} day{plural}, "));
    }
    out.push_str(&format!(
        "{}:{:02}:{:02}",
        secs / 3600,
        secs / 60 % 60,
        secs % 60
    ));
    if micro != 0 {
        out.push_str(&format!(".{micro:06}"));
    }
    Some(out)
}

/// `render`: the text the spreadsheet shows in a decoded cell, `None` for an
/// empty cell or a value type this reader has not been shown how to render.
pub(crate) fn render(decoded: Option<&Cell>) -> Option<String> {
    let decoded = decoded?;
    match decoded.ty {
        CELL_TYPE_EMPTY => None,
        CELL_TYPE_TEXT | CELL_TYPE_RICH_TEXT => decoded.text.clone(),
        ty => {
            let number = decoded.number?;
            match ty {
                CELL_TYPE_NUMBER | CELL_TYPE_CURRENCY => Some(format_number(number)),
                CELL_TYPE_DATE => format_date(number.as_f64()),
                CELL_TYPE_DURATION => format_duration(number.as_f64()),
                CELL_TYPE_BOOL => Some(format_bool(number.as_f64())),
                _ => None,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A version 5 cell: type, flags, then the values the flags name.
    fn v5(ty: u8, flags: u32, values: &[u8]) -> Vec<u8> {
        let mut b = vec![5, ty, 0, 0, 0, 0, 0, 0];
        b.extend(flags.to_le_bytes());
        b.extend(values);
        b
    }

    fn decimal128(coefficient: i128, exponent: i32) -> Vec<u8> {
        let mut bits = coefficient.unsigned_abs();
        bits |= ((exponent + DECIMAL128_BIAS) as u128) << 113;
        if coefficient < 0 {
            bits |= 1 << 127;
        }
        bits.to_le_bytes().to_vec()
    }

    fn values() -> CellValues {
        CellValues {
            strings: HashMap::from([(7, "seven".to_string())]),
            rich_text: HashMap::from([(3, "rich".to_string())]),
        }
    }

    /// docling's `test_version_5_cell_storage_is_decoded`.
    #[test]
    fn version_5_cell_storage_is_decoded() {
        let render_of = |b: Vec<u8>| render(cell(&b, 0, &values()).as_ref());
        assert_eq!(
            render_of(v5(CELL_TYPE_TEXT, 0x8, &7u32.to_le_bytes())),
            Some("seven".into())
        );
        assert_eq!(
            render_of(v5(CELL_TYPE_RICH_TEXT, 0x10, &3u32.to_le_bytes())),
            Some("rich".into())
        );
        assert_eq!(
            render_of(v5(CELL_TYPE_NUMBER, 0x1, &decimal128(46500, -1))),
            Some("4650".into())
        );
        assert_eq!(
            render_of(v5(CELL_TYPE_NUMBER, 0x2, &(-97.4f64).to_le_bytes())),
            Some("-97.4".into())
        );
        assert_eq!(
            render_of(v5(CELL_TYPE_BOOL, 0x2, &1f64.to_le_bytes())),
            Some("True".into())
        );
        assert_eq!(render_of(v5(CELL_TYPE_EMPTY, 0, &[])), None);
        // A flag whose value runs past the buffer yields nothing.
        assert_eq!(render_of(v5(CELL_TYPE_NUMBER, 0x1, &[0; 8])), None);
    }

    /// docling's `test_decimal128_keeps_every_encoded_digit`.
    #[test]
    fn decimal128_keeps_every_encoded_digit() {
        for (coefficient, exponent, want) in [
            (1i128, -1, "0.1"),
            (
                1234567890123456789012345678901234,
                -33,
                "1.234567890123456789012345678901234",
            ),
            (-25, 0, "-25"),
            (5, 3, "5000"),
            (120, -2, "1.2"),
            (7, -10, "0.0000000007"),
        ] {
            let d = read_decimal128(&decimal128(coefficient, exponent), 0).unwrap();
            assert_eq!(format_number(Number::Decimal(d)), want);
        }
        // Infinity and NaN combinations are rejected.
        let mut inf = vec![0u8; 16];
        inf[15] = 0x78;
        assert_eq!(read_decimal128(&inf, 0), None);
    }

    #[test]
    fn doubles_render_like_python_repr() {
        for (v, want) in [
            (4650.0, "4650"),
            (-872.4, "-872.4"),
            (0.1 + 0.2, "0.30000000000000004"),
            (1e21, "1000000000000000000000"),
            (1e-7, "0.0000001"),
        ] {
            assert_eq!(format_number(Number::Double(v)), want);
        }
    }

    #[test]
    fn dates_and_durations_follow_python() {
        // 2009-10-01, seconds from 2001-01-01.
        assert_eq!(
            format_date(276_048_000.0).as_deref(),
            Some("2009-10-01 00:00:00")
        );
        assert_eq!(format_date(-0.5).as_deref(), Some("2000-12-31 23:59:59"));
        assert_eq!(format_date(1e300), None);
        assert_eq!(format_duration(3661.0).as_deref(), Some("1:01:01"));
        assert_eq!(
            format_duration(90_000.5).as_deref(),
            Some("1 day, 1:00:00.500000")
        );
        assert_eq!(format_duration(-10.0).as_deref(), Some("-1 day, 23:59:50"));
        assert_eq!(format_duration(1e300), None);
        assert_eq!(
            format_timestamp(484_060_088.123).as_deref(),
            Some("2016-05-04T13:08:08.123+00:00")
        );
    }

    fn hex(s: &str) -> Vec<u8> {
        (0..s.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
            .collect()
    }

    /// docling's `test_version_5_cell_storage_is_decoded`, on the very cells
    /// it captured from documents newer than either fixture: a string
    /// reference, an exact decimal128, a date, a duration and an empty cell.
    #[test]
    fn upstream_version_5_buffers_are_decoded() {
        let values = CellValues {
            strings: HashMap::from([(14, "YYY_2_1".to_string())]),
            rich_text: HashMap::new(),
        };
        for (buffer, want) in [
            (
                "0503000000000000081002000e0000000500000001000000",
                Some("YYY_2_1"),
            ),
            (
                "05020000000000004130000\
                 0d00700000000000000000000\
                 00004030020000000100000003000000",
                Some("2000"),
            ),
            (
                "05050000000008006490000000000000d0337a4110000000130000000300000002000000",
                Some("2001-11-15 00:00:00"),
            ),
            (
                "050700000000000042120100000000000075224102000000090000000400000002000000",
                Some("7 days, 0:00:00"),
            ),
            ("05000000000000004000000002000000", None),
        ] {
            let buffer: String = buffer.split_whitespace().collect();
            let got = render(cell(&hex(&buffer), 0, &values).as_ref());
            assert_eq!(got.as_deref(), want, "{buffer}");
        }
    }

    /// docling's `test_decimal128_keeps_every_encoded_digit`: a decimal128
    /// carries 34 digits, more than Python's default decimal context keeps,
    /// so it is read and rendered without arithmetic.
    #[test]
    fn upstream_decimal128_cells_keep_every_digit() {
        for (coefficient, exponent, want) in [
            (
                "3333333333333333333333333333333333"
                    .parse::<i128>()
                    .unwrap(),
                -34,
                "0.3333333333333333333333333333333333",
            ),
            (
                1234567890123456789012345678901,
                -2,
                "12345678901234567890123456789.01",
            ),
            (
                9999999999999999999999999999999999,
                -20,
                "99999999999999.99999999999999999999",
            ),
        ] {
            let mut buffer = vec![CELL_VERSION_CURRENT, CELL_TYPE_NUMBER, 0, 0, 0, 0, 0, 0];
            buffer.extend(CELL_FLAG_DECIMAL.to_le_bytes());
            buffer.extend(decimal128(coefficient, exponent));
            let got = render(cell(&buffer, 0, &CellValues::default()).as_ref());
            assert_eq!(got.as_deref(), Some(want));
        }
    }

    /// docling's `test_render_leaves_a_date_or_duration_no_timedelta_can_hold_empty`:
    /// a decimal128 date whose exponent no double holds, and an infinite
    /// duration, render to nothing rather than fail.
    #[test]
    fn dates_and_durations_out_of_range_render_empty() {
        let huge = Cell {
            ty: CELL_TYPE_DATE,
            text: None,
            number: Some(Number::Decimal(Decimal {
                negative: false,
                coefficient: 1,
                exponent: 6100,
            })),
            style: None,
        };
        assert_eq!(render(Some(&huge)), None);
        let endless = Cell {
            ty: CELL_TYPE_DURATION,
            text: None,
            number: Some(Number::Double(f64::INFINITY)),
            style: None,
        };
        assert_eq!(render(Some(&endless)), None);
        assert_eq!(format_date(f64::NAN), None);
    }

    /// docling's version-4 layout: one identifier per set bit, the double in
    /// front of the last one for a numeric cell, the string key as the last.
    #[test]
    fn version_4_cells_follow_the_inferred_layout() {
        let mut text = vec![4, CELL_TYPE_TEXT, 0, 0];
        text.extend(0b11u32.to_le_bytes()); // two identifiers
        text.extend([0; 4]); // up to the values offset
        text.extend(1u32.to_le_bytes());
        text.extend(7u32.to_le_bytes());
        assert_eq!(cell_text(&text, 0, &values()).as_deref(), Some("seven"));
        let mut number = vec![4, CELL_TYPE_NUMBER, 0, 0];
        number.extend(0b1u32.to_le_bytes());
        number.extend([0; 4]);
        number.extend(2.5f64.to_le_bytes());
        number.extend(9u32.to_le_bytes());
        assert_eq!(
            render(cell(&number, 0, &values()).as_ref()).as_deref(),
            Some("2.5")
        );
        // Truncated: the layout does not fit, nothing is read.
        assert_eq!(cell(&number[..20], 0, &values()), None);
    }
}
