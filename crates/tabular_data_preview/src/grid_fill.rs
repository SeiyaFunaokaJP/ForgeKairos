//! Excel's two pointer gestures on the edge of a selection: the fill handle at
//! its bottom right corner, which stretches the block along one axis, and the
//! border itself, which moves the block (or copies it with the secondary
//! modifier held).
//!
//! Both are single block gestures - the handle and the grabbable border are not
//! drawn at all while several blocks are selected - and both land as one batch
//! of cell writes, so that one gesture is one undo step.
//!
//! The values a fill produces are worked out over the grid's own order, top to
//! bottom and left to right, whichever way the pointer went: a series is a
//! property of that order, so dragging upwards out of `1, 2, 3` extrapolates
//! backwards to `0, -1, -2` rather than continuing to `4, 5, 6`.

use gpui::{AnyElement, CursorStyle, Hsla, MouseButton, MouseDownEvent, actions};
use ui::{div, prelude::*};

use crate::{
    TabularDataPreviewPane,
    grid::{GridCell, GridDrag, GridRect, GridRow, GridSelectionKind},
    types::AnyColumn,
};

actions!(
    tabular_data,
    [
        /// 選択範囲の先頭行を範囲全体にコピーします。
        FillDown,
        /// 選択範囲の先頭列を範囲全体にコピーします。
        FillRight,
    ]
);

/// The side of the fill handle square, which is what the pointer has to find at
/// the corner of the selection.
const FILL_HANDLE_SIZE: Pixels = px(6.);

/// How wide the band along the selection's border is that grabs the block
/// instead of starting a new selection. It lies over the outline below and has
/// to stay wider than it, or the line the user aims at would be partly outside
/// the band that answers.
const MOVE_BORDER_WIDTH: Pixels = px(4.);

/// How thick the line around each selected block is.
pub(crate) const SELECTION_OUTLINE_WIDTH: Pixels = px(2.);

/// How thick the outline is that shows where a fill or a move would land.
const DRAG_PREVIEW_WIDTH: Pixels = px(2.);

/// A number as a cell spells it, kept as an integer scaled by its decimals so
/// that extending a series never has to format a float back: `12.50` is `1250`
/// with two decimals, and `12.50 + 0.25` stays `12.75` rather than becoming
/// `12.749999999999998`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct CellNumber {
    scaled: i128,
    decimals: u32,
}

/// Longest run of digits a value may hold and still be read as a number. Past
/// this the scaled form would overflow, and a cell holding a 40 digit account
/// number is not a series anyway.
const MAX_NUMBER_DIGITS: usize = 30;

impl CellNumber {
    fn parse(value: &str) -> Option<Self> {
        let value = value.trim();
        let (negative, digits) = match value.strip_prefix('-') {
            Some(rest) => (true, rest),
            None => (false, value.strip_prefix('+').unwrap_or(value)),
        };
        let (integer, fraction) = match digits.split_once('.') {
            Some((integer, fraction)) => (integer, fraction),
            None => (digits, ""),
        };
        // A lone sign, a bare `.5` and a trailing `5.` are all left to the
        // copying rule rather than rewritten into a spelling of their own.
        if integer.is_empty() || (digits.contains('.') && fraction.is_empty()) {
            return None;
        }
        if integer.len() + fraction.len() > MAX_NUMBER_DIGITS {
            return None;
        }
        let mut scaled: i128 = 0;
        for byte in integer.bytes().chain(fraction.bytes()) {
            if !byte.is_ascii_digit() {
                return None;
            }
            scaled = scaled
                .checked_mul(10)?
                .checked_add(i128::from(byte - b'0'))?;
        }
        Some(Self {
            scaled: if negative { -scaled } else { scaled },
            decimals: u32::try_from(fraction.len()).ok()?,
        })
    }

    /// The same number written with more decimals, so that `1` and `1.5` can be
    /// compared and stepped in one scale.
    fn rescaled(self, decimals: u32) -> Option<Self> {
        let factor = 10i128.checked_pow(decimals.checked_sub(self.decimals)?)?;
        Some(Self {
            scaled: self.scaled.checked_mul(factor)?,
            decimals,
        })
    }

    fn to_text(self) -> String {
        let sign = if self.scaled < 0 { "-" } else { "" };
        let magnitude = self.scaled.unsigned_abs();
        if self.decimals == 0 {
            return format!("{sign}{magnitude}");
        }
        let divisor = 10u128.pow(self.decimals);
        let width = self.decimals as usize;
        format!(
            "{sign}{}.{:0width$}",
            magnitude / divisor,
            magnitude % divisor
        )
    }
}

/// The numbers `source` spells out, brought to one scale, together with the
/// step between them. `None` when a value is not a number, or when the steps
/// are uneven - neither is a series to extend.
fn numeric_series(source: &[String]) -> Option<(Vec<CellNumber>, i128)> {
    let parsed: Vec<CellNumber> = source
        .iter()
        .map(|value| CellNumber::parse(value))
        .collect::<Option<_>>()?;
    let decimals = parsed.iter().map(|number| number.decimals).max()?;
    let numbers: Vec<CellNumber> = parsed
        .iter()
        .map(|number| number.rescaled(decimals))
        .collect::<Option<_>>()?;

    let mut step = None;
    for pair in numbers.windows(2) {
        let [previous, next] = pair else {
            continue;
        };
        let difference = next.scaled.checked_sub(previous.scaled)?;
        match step {
            None => step = Some(difference),
            Some(step) if step == difference => {}
            Some(_) => return None,
        }
    }
    // A single value has no step of its own; the caller supplies Excel's 1.
    Some((numbers, step.unwrap_or(0)))
}

/// What a fill is allowed to make of the series it recognises.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum FillRule {
    /// Dragging the fill handle: a run of values is extended, and a lone date
    /// is a series of its own.
    Extend,
    /// The same drag with Excel's Ctrl held, which flips the default both ways:
    /// a lone number becomes a counted series, and everything that would have
    /// been extended is stamped as a plain copy instead.
    Invert,
    /// Ctrl+D and Ctrl+R, which say "put this line over the rest of the block"
    /// and nothing more. Filling a column of dates down must not quietly write
    /// the week after them.
    Copy,
}

/// The values a fill writes, in the order it writes them: the first is the cell
/// next to the source block in the direction the pointer went.
///
/// `source` is always in the grid's own order, whichever way the fill goes.
pub(crate) fn fill_values(
    source: &[String],
    count: usize,
    forward: bool,
    rule: FillRule,
) -> Vec<String> {
    if source.is_empty() || count == 0 {
        return Vec::new();
    }
    // Dates are read before numbers: only a spelling with a separator or a
    // weekday's name in it is a date, so a run of digits like `20260918` is
    // still the number it looks like.
    if let Some(values) = date_fill(source, count, forward, rule) {
        return values;
    }
    if let Some(values) = numeric_fill(source, count, forward, rule) {
        return values;
    }
    // Repeating stamps the whole block again and again, which going backwards
    // means its last value lands first.
    (0..count)
        .map(|offset| {
            let index = offset % source.len();
            let index = if forward {
                index
            } else {
                source.len() - 1 - index
            };
            source.get(index).cloned().unwrap_or_default()
        })
        .collect()
}

fn numeric_fill(
    source: &[String],
    count: usize,
    forward: bool,
    rule: FillRule,
) -> Option<Vec<String>> {
    let (numbers, step) = numeric_series(source)?;
    let single = numbers.len() == 1;
    let extend = match rule {
        FillRule::Extend => !single,
        FillRule::Invert => single,
        FillRule::Copy => false,
    };
    if !extend {
        return None;
    }
    let decimals = numbers.first()?.decimals;
    let step = if single {
        10i128.checked_pow(decimals)?
    } else {
        step
    };
    let anchor = if forward {
        numbers.last()?.scaled
    } else {
        numbers.first()?.scaled
    };

    let mut values = Vec::with_capacity(count);
    for offset in 1..=count {
        let delta = step.checked_mul(i128::try_from(offset).ok()?)?;
        let scaled = if forward {
            anchor.checked_add(delta)?
        } else {
            anchor.checked_sub(delta)?
        };
        values.push(CellNumber { scaled, decimals }.to_text());
    }
    Some(values)
}

/// The dates `source` spells out, counted on their own format's scale, together
/// with the step in days between them. `None` when a value is not a date, when
/// the values are not all spelled the same way - one series cannot be written
/// back in two spellings - or when the steps are uneven.
fn date_series(source: &[String]) -> Option<(dates::DateFormat, Vec<i64>, i64)> {
    let parsed: Vec<(dates::DateFormat, i64)> = source
        .iter()
        .map(|value| dates::parse(value))
        .collect::<Option<_>>()?;
    let (format, _) = *parsed.first()?;
    if parsed.iter().any(|(other, _)| *other != format) {
        return None;
    }
    let days: Vec<i64> = parsed.iter().map(|(_, days)| *days).collect();

    let mut step = None;
    for pair in days.windows(2) {
        let [previous, next] = pair else {
            continue;
        };
        let difference = next.checked_sub(*previous)?;
        let difference = match format.cycle() {
            // A weekday, and a date written without a year, run round a cycle:
            // the step from Saturday to Sunday is one day forwards rather than
            // six backwards.
            Some(cycle) => difference.rem_euclid(cycle),
            None => difference,
        };
        match step {
            None => step = Some(difference),
            Some(step) if step == difference => {}
            Some(_) => return None,
        }
    }
    // A single date has no step of its own, and a spreadsheet's answer for one
    // dragged date is the day after it.
    Some((format, days, step.unwrap_or(1)))
}

/// Excel's date fill. A date is the one kind of value a drag extends without
/// being asked, so Ctrl here means the opposite of what it means for a number:
/// it asks for the plain copy instead.
fn date_fill(
    source: &[String],
    count: usize,
    forward: bool,
    rule: FillRule,
) -> Option<Vec<String>> {
    if rule != FillRule::Extend {
        return None;
    }
    let (format, days, step) = date_series(source)?;
    if format.needs_a_run() && days.len() < 2 {
        return None;
    }
    let anchor = if forward {
        *days.last()?
    } else {
        *days.first()?
    };

    let mut values = Vec::with_capacity(count);
    for offset in 1..=count {
        let delta = step.checked_mul(i64::try_from(offset).ok()?)?;
        let day = if forward {
            anchor.checked_add(delta)?
        } else {
            anchor.checked_sub(delta)?
        };
        // A date the spelling cannot hold - a year of five digits - takes the
        // whole fill back to copying rather than writing one value in a shape
        // the column does not use.
        values.push(format.spell(day)?);
    }
    Some(values)
}

/// Dates as a cell spells them, which is what a fill has to read and write back
/// in: the same separator, the same zero padding, the same language.
///
/// Only a whole date with a separator in it, or a weekday written out by name,
/// counts - a value carrying a time of day is left to the copying rule, a run of
/// digits stays a number, and a spelling that could be read two ways (`9/18`,
/// `26/9/1`) is refused rather than guessed at - which is what keeps this from
/// quietly rewriting values that were never dates.
mod dates {
    use chrono::{Datelike as _, NaiveDate};

    /// The year a date written without one is worked out in. It is deliberately
    /// not a leap year: with no year given there is nothing to say whether
    /// `2月29日` exists, so it is left to the copying rule.
    const YEARLESS_REFERENCE_YEAR: i32 = 2001;
    const DAYS_IN_YEARLESS_REFERENCE_YEAR: i64 = 365;
    const DAYS_IN_WEEK: i64 = 7;

    /// Sunday first, which is the order [`chrono`] counts weekdays in and the
    /// order a Japanese calendar prints them in.
    const JAPANESE_WEEKDAYS: [&str; 7] = ["日", "月", "火", "水", "木", "金", "土"];
    const ENGLISH_WEEKDAYS: [(&str, &str); 7] = [
        ("Sun", "Sunday"),
        ("Mon", "Monday"),
        ("Tue", "Tuesday"),
        ("Wed", "Wednesday"),
        ("Thu", "Thursday"),
        ("Fri", "Friday"),
        ("Sat", "Saturday"),
    ];

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub(super) enum DateFormat {
        /// `2026-09-18`, `2026/9/1`: a four digit year, then the month and the
        /// day, each either zero padded or not.
        Ordered {
            separator: char,
            month_padded: bool,
            day_padded: bool,
        },
        /// `2026年9月18日`, and `9月18日` without the year.
        Japanese {
            has_year: bool,
            month_padded: bool,
            day_padded: bool,
        },
        Weekday(WeekdayStyle),
    }

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub(super) enum WeekdayStyle {
        /// `日`
        JapaneseShort,
        /// `日曜日`
        JapaneseLong,
        /// `Sun`, in whichever case the cell wrote it.
        EnglishShort(LetterCase),
        /// `Sunday`.
        EnglishLong(LetterCase),
    }

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub(super) enum LetterCase {
        Lower,
        Upper,
        Capitalized,
    }

    impl DateFormat {
        /// How long the cycle is that this format's values run round, for the
        /// formats with no year to carry them past its end.
        pub(super) fn cycle(self) -> Option<i64> {
            match self {
                DateFormat::Ordered { .. } | DateFormat::Japanese { has_year: true, .. } => None,
                DateFormat::Japanese {
                    has_year: false, ..
                } => Some(DAYS_IN_YEARLESS_REFERENCE_YEAR),
                DateFormat::Weekday(_) => Some(DAYS_IN_WEEK),
            }
        }

        /// Whether one cell of this format could just as easily be something
        /// else, so that only a run of them is read as a series.
        ///
        /// A weekday written as one kanji is an ordinary word as well: 金 is
        /// money, 水 is water, 土 is soil, 日 is a day or the sun. A column of
        /// those dragged down must not quietly become a calendar. `日曜日`,
        /// `Sun` and `Sunday` are nothing but weekdays, so one of them is
        /// enough.
        pub(super) fn needs_a_run(self) -> bool {
            matches!(self, DateFormat::Weekday(WeekdayStyle::JapaneseShort))
        }

        /// Writes a value of this format's own scale back out in it, or `None`
        /// when the value falls outside what the spelling can hold.
        pub(super) fn spell(self, value: i64) -> Option<String> {
            match self {
                DateFormat::Ordered {
                    separator,
                    month_padded,
                    day_padded,
                } => {
                    let date = date_from_days(value)?;
                    Some(format!(
                        "{:04}{separator}{}{separator}{}",
                        date.year(),
                        component(date.month(), month_padded),
                        component(date.day(), day_padded),
                    ))
                }
                DateFormat::Japanese {
                    has_year: true,
                    month_padded,
                    day_padded,
                } => {
                    let date = date_from_days(value)?;
                    Some(format!(
                        "{:04}年{}月{}日",
                        date.year(),
                        component(date.month(), month_padded),
                        component(date.day(), day_padded),
                    ))
                }
                DateFormat::Japanese {
                    has_year: false,
                    month_padded,
                    day_padded,
                } => {
                    let ordinal = value.rem_euclid(DAYS_IN_YEARLESS_REFERENCE_YEAR);
                    let ordinal = u32::try_from(ordinal).ok()?.checked_add(1)?;
                    let date = NaiveDate::from_yo_opt(YEARLESS_REFERENCE_YEAR, ordinal)?;
                    Some(format!(
                        "{}月{}日",
                        component(date.month(), month_padded),
                        component(date.day(), day_padded),
                    ))
                }
                DateFormat::Weekday(style) => {
                    let index = usize::try_from(value.rem_euclid(DAYS_IN_WEEK)).ok()?;
                    spell_weekday(style, index)
                }
            }
        }
    }

    /// The date `value` spells, counted on its format's own scale, together
    /// with that format.
    pub(super) fn parse(value: &str) -> Option<(DateFormat, i64)> {
        let value = value.trim();
        parse_ordered(value)
            .or_else(|| parse_japanese(value))
            .or_else(|| parse_weekday(value))
    }

    fn parse_ordered(value: &str) -> Option<(DateFormat, i64)> {
        let separator = ['-', '/'].into_iter().find(|&candidate| {
            // The first of the two that appears at all decides; a value mixing
            // them splits into too many pieces below and is refused.
            value.contains(candidate)
        })?;
        let mut parts = value.split(separator);
        let year = parse_year(parts.next()?)?;
        let (month, month_padding) = parse_component(parts.next()?)?;
        let (day, day_padding) = parse_component(parts.next()?)?;
        if parts.next().is_some() {
            return None;
        }
        let date = NaiveDate::from_ymd_opt(year, month, day)?;
        let iso = separator == '-';
        Some((
            DateFormat::Ordered {
                separator,
                month_padded: pads(month_padding, day_padding, iso),
                day_padded: pads(day_padding, month_padding, iso),
            },
            i64::from(date.num_days_from_ce()),
        ))
    }

    fn parse_japanese(value: &str) -> Option<(DateFormat, i64)> {
        // Split on the characters themselves rather than on byte offsets: every
        // one of them is three bytes long.
        let (year, rest) = match value.split_once('年') {
            Some((year, rest)) => (Some(parse_year(year)?), rest),
            None => (None, value),
        };
        let (month, rest) = rest.split_once('月')?;
        let day = rest.strip_suffix('日')?;
        let (month, month_padding) = parse_component(month)?;
        let (day, day_padding) = parse_component(day)?;
        let date = NaiveDate::from_ymd_opt(year.unwrap_or(YEARLESS_REFERENCE_YEAR), month, day)?;
        let format = DateFormat::Japanese {
            has_year: year.is_some(),
            month_padded: pads(month_padding, day_padding, false),
            day_padded: pads(day_padding, month_padding, false),
        };
        let days = match year {
            Some(_) => i64::from(date.num_days_from_ce()),
            None => i64::from(date.ordinal0()),
        };
        Some((format, days))
    }

    fn parse_weekday(value: &str) -> Option<(DateFormat, i64)> {
        if let Some(index) = JAPANESE_WEEKDAYS.iter().position(|day| *day == value) {
            return Some((
                DateFormat::Weekday(WeekdayStyle::JapaneseShort),
                index as i64,
            ));
        }
        if let Some(stem) = value.strip_suffix("曜日")
            && let Some(index) = JAPANESE_WEEKDAYS.iter().position(|day| *day == stem)
        {
            return Some((
                DateFormat::Weekday(WeekdayStyle::JapaneseLong),
                index as i64,
            ));
        }
        for (index, (short, long)) in ENGLISH_WEEKDAYS.iter().enumerate() {
            if let Some(case) = letter_case(value, long) {
                return Some((
                    DateFormat::Weekday(WeekdayStyle::EnglishLong(case)),
                    index as i64,
                ));
            }
            if let Some(case) = letter_case(value, short) {
                return Some((
                    DateFormat::Weekday(WeekdayStyle::EnglishShort(case)),
                    index as i64,
                ));
            }
        }
        None
    }

    /// Exactly four digits, so that `9/1/2026` and `26/9/1` - which could be
    /// read two ways round - are refused rather than guessed at.
    fn parse_year(value: &str) -> Option<i32> {
        if value.len() != 4 || !value.bytes().all(|byte| byte.is_ascii_digit()) {
            return None;
        }
        value.parse().ok()
    }

    /// Whether a month or a day was written with a leading zero, which is the
    /// half of the spelling a fill has to carry over.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    enum Padding {
        /// One digit, so there is plainly no zero in front of it: `9`.
        Absent,
        /// Two digits, the first of them a zero: `09`.
        Present,
        /// Two digits of its own, like `18`: written the same way whether or
        /// not the column pads, so this one value cannot say.
        Unknown,
    }

    fn parse_component(value: &str) -> Option<(u32, Padding)> {
        if value.is_empty() || value.len() > 2 || !value.bytes().all(|byte| byte.is_ascii_digit()) {
            return None;
        }
        let padding = match (value.len(), value.starts_with('0')) {
            (1, _) => Padding::Absent,
            (_, true) => Padding::Present,
            (_, false) => Padding::Unknown,
        };
        Some((value.parse().ok()?, padding))
    }

    /// Whether a component is written with a leading zero, taking the other
    /// component's word for it when its own two digits cannot say - `2026-01-31`
    /// pads its day, because the month beside it is padded - and the format's
    /// own convention when neither can: a dash is ISO 8601, which always pads,
    /// while a slash and 年月日 are the local short date, which does not.
    fn pads(padding: Padding, other: Padding, convention: bool) -> bool {
        match (padding, other) {
            (Padding::Present, _) => true,
            (Padding::Absent, _) => false,
            (Padding::Unknown, Padding::Present) => true,
            (Padding::Unknown, Padding::Absent) => false,
            (Padding::Unknown, Padding::Unknown) => convention,
        }
    }

    fn component(value: u32, padded: bool) -> String {
        if padded {
            format!("{value:02}")
        } else {
            format!("{value}")
        }
    }

    /// The date `days` days into the common era, refused unless it has the four
    /// digit year every one of these spellings writes.
    fn date_from_days(days: i64) -> Option<NaiveDate> {
        let date = NaiveDate::from_num_days_from_ce_opt(i32::try_from(days).ok()?)?;
        (1..=9999).contains(&date.year()).then_some(date)
    }

    fn spell_weekday(style: WeekdayStyle, index: usize) -> Option<String> {
        match style {
            WeekdayStyle::JapaneseShort => Some((*JAPANESE_WEEKDAYS.get(index)?).to_owned()),
            WeekdayStyle::JapaneseLong => Some(format!("{}曜日", JAPANESE_WEEKDAYS.get(index)?)),
            WeekdayStyle::EnglishShort(case) => Some(cased(ENGLISH_WEEKDAYS.get(index)?.0, case)),
            WeekdayStyle::EnglishLong(case) => Some(cased(ENGLISH_WEEKDAYS.get(index)?.1, case)),
        }
    }

    /// Which case `value` writes `canonical` in, or `None` when it is not that
    /// word at all - or writes it in a case no fill could carry over, like
    /// `sUNDAY`.
    fn letter_case(value: &str, canonical: &str) -> Option<LetterCase> {
        if value == canonical {
            Some(LetterCase::Capitalized)
        } else if value == canonical.to_lowercase() {
            Some(LetterCase::Lower)
        } else if value == canonical.to_uppercase() {
            Some(LetterCase::Upper)
        } else {
            None
        }
    }

    fn cased(name: &str, case: LetterCase) -> String {
        match case {
            LetterCase::Capitalized => name.to_owned(),
            LetterCase::Lower => name.to_lowercase(),
            LetterCase::Upper => name.to_uppercase(),
        }
    }
}

/// Which way a fill went and how far, read off the block it started on and the
/// rectangle the pointer stretched it to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct FillAxis {
    vertical: bool,
    forward: bool,
    extent: usize,
}

fn fill_axis(origin: GridRect, filled: GridRect) -> Option<FillAxis> {
    let (vertical, forward, extent) = if filled.last_row > origin.last_row {
        (true, true, filled.last_row - origin.last_row)
    } else if filled.first_row < origin.first_row {
        (true, false, origin.first_row - filled.first_row)
    } else if filled.last_col > origin.last_col {
        (false, true, filled.last_col - origin.last_col)
    } else if filled.first_col < origin.first_col {
        (false, false, origin.first_col - filled.first_col)
    } else {
        return None;
    };
    Some(FillAxis {
        vertical,
        forward,
        extent,
    })
}

/// The rectangle a fill of `origin` reaching `target` covers: the block itself
/// plus the run it was stretched by, along the one axis the pointer moved
/// furthest on, which is the axis Excel picks too.
fn stretched_rect(origin: GridRect, target: GridCell) -> GridRect {
    let row = target.row.index();
    let col = target.col.get();
    let below = row.saturating_sub(origin.last_row);
    let above = origin.first_row.saturating_sub(row);
    let right = col.saturating_sub(origin.last_col);
    let left = origin.first_col.saturating_sub(col);
    let vertical = below.max(above);
    let horizontal = right.max(left);

    if vertical == 0 && horizontal == 0 {
        return origin;
    }
    if vertical >= horizontal {
        GridRect {
            first_row: origin.first_row.min(row),
            last_row: origin.last_row.max(row),
            ..origin
        }
    } else {
        GridRect {
            first_col: origin.first_col.min(col),
            last_col: origin.last_col.max(col),
            ..origin
        }
    }
}

/// `start` moved by `shift` and held between `lowest` and `highest`, which is
/// how a dragged block stays whole inside the table.
fn shifted_start(start: usize, shift: isize, lowest: usize, highest: usize) -> usize {
    let highest = highest.max(lowest);
    (start as isize + shift).clamp(lowest as isize, highest as isize) as usize
}

impl TabularDataPreviewPane {
    ///// Gesture availability /////

    /// The cell whose corner carries the fill handle: the bottom right of a
    /// single block selection.
    ///
    /// `None` while several blocks are selected, because there is no one corner
    /// to drag, and while a cell is open for input, where the handle would sit
    /// under the overlay editor.
    pub(crate) fn fill_handle_cell(&self) -> Option<GridCell> {
        if self.grid_is_empty()
            || self.grid.editing.is_some()
            || self.has_multiple_selection_ranges()
            || self.grid.drag.is_some()
        {
            return None;
        }
        let rect = self.selection_rect();
        Some(GridCell::new(
            GridRow::from_index(rect.last_row),
            AnyColumn(rect.last_col),
        ))
    }

    /// Whether the selection's border can be grabbed to move the block.
    ///
    /// Off while the table is sorted or filtered: the rows on screen are not
    /// the rows of the file, so "put this block two rows further down" has no
    /// one meaning. The fill handle stays on, because a fill writes the rows
    /// the user can see, which is what Excel does with a filtered list too.
    pub(crate) fn block_move_is_available(&self) -> bool {
        !self.grid_is_empty()
            && self.grid.editing.is_none()
            && self.grid.drag.is_none()
            && !self.has_multiple_selection_ranges()
            && self.engine.d2d_mapping().is_identity()
    }

    ///// Pointer /////

    fn start_fill_drag(&mut self, cx: &mut Context<Self>) {
        let origin = self.selection_rect();
        self.grid.drag = Some(GridDrag::Fill { origin });
        self.grid.drag_preview = Some(origin);
        cx.notify();
    }

    fn start_move_drag(&mut self, grabbed: GridCell, cx: &mut Context<Self>) {
        let origin = self.selection_rect();
        self.grid.drag = Some(GridDrag::Move { origin, grabbed });
        self.grid.drag_preview = Some(origin);
        cx.notify();
    }

    pub(crate) fn update_fill_preview(
        &mut self,
        origin: GridRect,
        target: GridCell,
        cx: &mut Context<Self>,
    ) {
        let preview = stretched_rect(origin, self.clamp_cell(target));
        if self.grid.drag_preview != Some(preview) {
            self.grid.drag_preview = Some(preview);
            cx.notify();
        }
    }

    pub(crate) fn update_move_preview(
        &mut self,
        origin: GridRect,
        grabbed: GridCell,
        target: GridCell,
        cx: &mut Context<Self>,
    ) {
        let target = self.clamp_cell(target);
        let row_shift = target.row.index() as isize - grabbed.row.index() as isize;
        let col_shift = target.col.get() as isize - grabbed.col.get() as isize;
        let preview = self.shifted_rect(origin, row_shift, col_shift);
        if self.grid.drag_preview != Some(preview) {
            self.grid.drag_preview = Some(preview);
            cx.notify();
        }
    }

    fn shifted_rect(&self, rect: GridRect, row_shift: isize, col_shift: isize) -> GridRect {
        let first_row = shifted_start(
            rect.first_row,
            row_shift,
            self.first_row_index(),
            self.last_row_index()
                .saturating_sub(rect.row_count().saturating_sub(1)),
        );
        let first_col = shifted_start(
            rect.first_col,
            col_shift,
            0,
            self.last_col_index()
                .saturating_sub(rect.col_count().saturating_sub(1)),
        );
        GridRect {
            first_row,
            last_row: first_row + rect.row_count() - 1,
            first_col,
            last_col: first_col + rect.col_count() - 1,
        }
    }

    ///// Fill /////

    /// Writes the stretched part of a fill, reading the source values in the
    /// order the grid shows them so that a filtered table is filled down the
    /// rows the user can see.
    pub(crate) fn commit_fill(
        &mut self,
        origin: GridRect,
        filled: GridRect,
        invert: bool,
        cx: &mut Context<Self>,
    ) {
        let rule = if invert {
            FillRule::Invert
        } else {
            FillRule::Extend
        };
        self.commit_fill_with(origin, filled, rule, cx);
    }

    fn commit_fill_with(
        &mut self,
        origin: GridRect,
        filled: GridRect,
        rule: FillRule,
        cx: &mut Context<Self>,
    ) {
        let Some(axis) = fill_axis(origin, filled) else {
            return;
        };
        let lines: Vec<usize> = if axis.vertical {
            origin.cols().collect()
        } else {
            origin.rows().collect()
        };
        if !self.within_cell_budget(lines.len().saturating_mul(axis.extent), cx) {
            return;
        }

        let mut cells = Vec::with_capacity(lines.len() * axis.extent);
        for line in lines {
            let source: Vec<String> = if axis.vertical {
                origin
                    .rows()
                    .map(|row| {
                        self.cell_value(GridRow::from_index(row), AnyColumn(line))
                            .to_string()
                    })
                    .collect()
            } else {
                origin
                    .cols()
                    .map(|col| {
                        self.cell_value(GridRow::from_index(line), AnyColumn(col))
                            .to_string()
                    })
                    .collect()
            };
            for (offset, value) in fill_values(&source, axis.extent, axis.forward, rule)
                .into_iter()
                .enumerate()
            {
                let distance = offset + 1;
                let (row_index, col_index) = if axis.vertical {
                    let Some(row) = step_from(origin, axis.forward, true, distance) else {
                        continue;
                    };
                    (row, line)
                } else {
                    let Some(col) = step_from(origin, axis.forward, false, distance) else {
                        continue;
                    };
                    (line, col)
                };
                let Some(row_ref) = self.row_ref(GridRow::from_index(row_index)) else {
                    continue;
                };
                cells.push((row_ref, AnyColumn(col_index), value));
            }
        }
        if cells.is_empty() {
            return;
        }

        let edits = self
            .with_edit_engine(cx, |engine| engine.set_cells(&cells))
            .unwrap_or_default();
        for (row, col, value) in &cells {
            self.write_cached_cell(*row, *col, value);
        }
        self.clear_status_message();
        self.apply_buffer_edits(edits, cx);
        self.select_rect(filled, cx);
        cx.notify();
    }

    /// Ctrl+D and Ctrl+R: the selection's first row or first column copied over
    /// the rest of it, which is a fill whose source is that one line.
    pub(crate) fn fill_within_selection(&mut self, vertical: bool, cx: &mut Context<Self>) {
        // Ctrl+D and Ctrl+R are the editor's own keys too, and the grid only
        // takes them over while no cell is open for input.
        if self.grid_is_empty() || self.grid.editing.is_some() {
            return;
        }
        if self.refuse_on_multiple_ranges(cx) {
            return;
        }
        let mut filled = self.selection_rect();
        // Selecting a column selects the heading with it, and a fill down from
        // the heading would stamp the column's name over every value in it. The
        // user asking for a whole column cannot mean that, so the fill starts at
        // the first row of the body instead. A cell range that reaches into the
        // heading is left alone: that one the user drew by hand.
        if vertical
            && self.has_header_row()
            && matches!(
                self.grid.selection_kind,
                GridSelectionKind::Columns | GridSelectionKind::All
            )
            && filled.first_row < filled.last_row
        {
            filled.first_row += 1;
        }
        let origin = if vertical {
            GridRect {
                last_row: filled.first_row,
                ..filled
            }
        } else {
            GridRect {
                last_col: filled.first_col,
                ..filled
            }
        };
        self.commit_fill_with(origin, filled, FillRule::Copy, cx);
    }

    ///// Move /////

    /// Moves the block to where the border was dragged, or copies it when the
    /// secondary modifier was held as the button came up.
    ///
    /// The cleared source cells and the written destination cells go in as one
    /// list, the clears first: the engine keeps one value per cell and takes the
    /// last one given, so the overlap between the two rectangles is written
    /// rather than emptied.
    pub(crate) fn commit_block_move(
        &mut self,
        origin: GridRect,
        destination: GridRect,
        copy: bool,
        cx: &mut Context<Self>,
    ) {
        if destination == origin {
            return;
        }
        let moved_cells = origin.cell_count();
        if !self.within_cell_budget(moved_cells.saturating_mul(2), cx) {
            return;
        }

        let mut cells = Vec::with_capacity(moved_cells * 2);
        let mut values = Vec::with_capacity(moved_cells);
        for (row_offset, row_index) in origin.rows().enumerate() {
            for (col_offset, col_index) in origin.cols().enumerate() {
                let row = GridRow::from_index(row_index);
                values.push((
                    row_offset,
                    col_offset,
                    self.cell_value(row, AnyColumn(col_index)).to_string(),
                ));
                if !copy && let Some(row_ref) = self.row_ref(row) {
                    cells.push((row_ref, AnyColumn(col_index), String::new()));
                }
            }
        }
        for (row_offset, col_offset, value) in values {
            let row = GridRow::from_index(destination.first_row + row_offset);
            let Some(row_ref) = self.row_ref(row) else {
                continue;
            };
            cells.push((
                row_ref,
                AnyColumn(destination.first_col + col_offset),
                value,
            ));
        }
        if cells.is_empty() {
            return;
        }

        let edits = self
            .with_edit_engine(cx, |engine| engine.set_cells(&cells))
            .unwrap_or_default();
        for (row, col, value) in &cells {
            self.write_cached_cell(*row, *col, value);
        }
        self.clear_status_message();
        self.apply_buffer_edits(edits, cx);
        self.select_rect(destination, cx);
        cx.notify();
    }

    /// Leaves `rect` as the one selected block, the way a spreadsheet selects
    /// what a gesture just wrote. A gesture that wrote inside the selection it
    /// already had leaves it alone, so that Ctrl+D on selected rows does not
    /// quietly turn them into a cell range - nor a whole column, whose fill
    /// deliberately starts one row below the block the user has selected.
    fn select_rect(&mut self, rect: GridRect, cx: &mut Context<Self>) {
        if !self.has_multiple_selection_ranges() && self.selection_rect().contains_rect(rect) {
            return;
        }
        self.grid.previous_ranges.clear();
        self.grid.anchor = Some(GridCell::new(
            GridRow::from_index(rect.first_row),
            AnyColumn(rect.first_col),
        ));
        self.grid.cursor =
            GridCell::new(GridRow::from_index(rect.last_row), AnyColumn(rect.last_col));
        self.grid.selection_kind = self.kind_of(rect);
        self.notify_search_of_caret(cx);
    }

    /// What the selection is anchored to once a gesture has moved or stretched
    /// it. Whole rows stay whole rows and whole columns whole columns, so that
    /// filling a selected row downwards leaves rows selected rather than
    /// quietly turning them into a cell range the next Delete would treat
    /// differently.
    ///
    /// A kind is only kept while the rectangle it landed on really is what that
    /// kind means, because rows and columns are stored as two corners and drawn
    /// back out across the whole table: keeping `Rows` on a rectangle that is
    /// not the full width would widen the selection behind the user's back.
    fn kind_of(&self, rect: GridRect) -> GridSelectionKind {
        let spans_every_col = rect.first_col == 0 && rect.last_col == self.last_col_index();
        let spans_every_row =
            rect.first_row == self.first_row_index() && rect.last_row == self.last_row_index();
        let keeps_its_shape = match self.grid.selection_kind {
            GridSelectionKind::Cells => false,
            GridSelectionKind::Rows => spans_every_col,
            GridSelectionKind::Columns => spans_every_row,
            GridSelectionKind::All => spans_every_col && spans_every_row,
        };
        if keeps_its_shape {
            self.grid.selection_kind
        } else {
            GridSelectionKind::Cells
        }
    }

    ///// Rendering /////

    /// The parts of the selection and of these two gestures that hang off one
    /// body cell: the line around each selected block, the invisible bands that
    /// grab that line to move the block, the fill handle at its corner, and the
    /// outline of where a drag in flight would land.
    ///
    /// All of them are absolutely positioned, so the cell's wrapper has to be a
    /// flex container, and they are ordered so that the fill handle is painted
    /// last: at the bottom right corner it shares a cell with two border bands,
    /// and the later hitbox is the one the pointer finds. The bands come after
    /// the line they grab for the same reason - they are transparent, so the
    /// line still shows through them.
    pub(crate) fn grid_drag_overlays(&self, cell: GridCell, cx: &Context<Self>) -> Vec<AnyElement> {
        let mut overlays = self.render_selection_outline(cell, cx);
        overlays.extend(self.render_move_border_bands(cell, cx));
        overlays.extend(self.render_drag_preview_edges(cell, cx));
        overlays.extend(self.render_fill_handle(cell, cx));
        overlays
    }

    /// Whether `cell` is drawn with the caret's own border, which is the border
    /// a single cell block's outline would only thicken.
    pub(crate) fn draws_active_cell_border(&self, cell: GridCell) -> bool {
        self.grid.cursor == cell && self.grid.context_menu.is_none()
    }

    /// The line around every selected block, drawn one cell at a time.
    ///
    /// The faint background on each selected cell says which cells are in the
    /// selection but not where one block ends and the next begins, which is
    /// what a spreadsheet's solid border around the block says - and what the
    /// band that grabs the block to move it needs the user to be able to aim at.
    fn render_selection_outline(&self, cell: GridCell, cx: &Context<Self>) -> Vec<AnyElement> {
        if self.grid_is_empty() {
            return Vec::new();
        }
        let color = cx.theme().colors().border_focused;
        // This runs once per drawn cell, and a single block is nearly every
        // frame, so that case must not build the vector of blocks to answer.
        if !self.has_multiple_selection_ranges() {
            return self.block_outline_edges(0, self.selection_rect(), cell, color);
        }
        self.selection_rects()
            .into_iter()
            .enumerate()
            .flat_map(|(block, rect)| self.block_outline_edges(block, rect, cell, color))
            .collect()
    }

    fn block_outline_edges(
        &self,
        block: usize,
        rect: GridRect,
        cell: GridCell,
        color: Hsla,
    ) -> Vec<AnyElement> {
        if !rect.contains(cell.row, cell.col) {
            return Vec::new();
        }
        // A block of one cell that the caret is on is already outlined by the
        // caret's own border; a second line on top of it would only make that
        // border look thicker than it is anywhere else.
        if rect.row_count() == 1 && rect.col_count() == 1 && self.draws_active_cell_border(cell) {
            return Vec::new();
        }
        sides_of(rect, cell)
            .into_iter()
            .map(|side| {
                side.place(div(), SELECTION_OUTLINE_WIDTH)
                    .bg(color)
                    .when(side.is_named_segment(rect, cell), |this| {
                        this.debug_selector(move || selection_outline_debug_selector(block, side))
                    })
                    .into_any_element()
            })
            .collect()
    }

    fn render_fill_handle(&self, cell: GridCell, cx: &Context<Self>) -> Option<AnyElement> {
        if self.fill_handle_cell() != Some(cell) {
            return None;
        }
        Some(
            div()
                .absolute()
                .right_0()
                .bottom_0()
                .size(FILL_HANDLE_SIZE)
                .bg(cx.theme().colors().border_focused)
                .cursor(CursorStyle::Crosshair)
                .debug_selector(|| FILL_HANDLE_DEBUG_SELECTOR.to_owned())
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|this, _event: &MouseDownEvent, window, cx| {
                        // Stopping here keeps the press off the cell underneath,
                        // which also keeps it from the grid's own root, where a
                        // click is what puts the caret back in the cell editor
                        // and makes this pane the active one.
                        this.focus_cell_editor(window, cx);
                        this.start_fill_drag(cx);
                        cx.stop_propagation();
                    }),
                )
                .into_any_element(),
        )
    }

    fn render_move_border_bands(&self, cell: GridCell, cx: &Context<Self>) -> Vec<AnyElement> {
        if !self.block_move_is_available() {
            return Vec::new();
        }
        let rect = self.selection_rect();
        if !rect.contains(cell.row, cell.col) {
            return Vec::new();
        }
        sides_of(rect, cell)
            .into_iter()
            .map(|side| {
                side.place(div(), MOVE_BORDER_WIDTH)
                    .cursor(CursorStyle::OpenHand)
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, _event: &MouseDownEvent, window, cx| {
                            this.focus_cell_editor(window, cx);
                            this.start_move_drag(cell, cx);
                            cx.stop_propagation();
                        }),
                    )
                    .into_any_element()
            })
            .collect()
    }

    fn render_drag_preview_edges(&self, cell: GridCell, cx: &Context<Self>) -> Vec<AnyElement> {
        let Some(rect) = self.grid.drag_preview else {
            return Vec::new();
        };
        if !rect.contains(cell.row, cell.col) {
            return Vec::new();
        }
        let color = cx.theme().colors().border_focused;
        sides_of(rect, cell)
            .into_iter()
            .map(|side| {
                side.place(div(), DRAG_PREVIEW_WIDTH)
                    .bg(color)
                    .into_any_element()
            })
            .collect()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum BorderSide {
    Top,
    Bottom,
    Left,
    Right,
}

impl BorderSide {
    /// Lays `element` along this side of the cell it hangs in, `width` thick.
    fn place<E: Styled>(self, element: E, width: Pixels) -> E {
        let element = element.absolute();
        match self {
            BorderSide::Top => element.top_0().left_0().right_0().h(width),
            BorderSide::Bottom => element.bottom_0().left_0().right_0().h(width),
            BorderSide::Left => element.left_0().top_0().bottom_0().w(width),
            BorderSide::Right => element.right_0().top_0().bottom_0().w(width),
        }
    }

    fn index(self) -> usize {
        match self {
            BorderSide::Top => 0,
            BorderSide::Bottom => 1,
            BorderSide::Left => 2,
            BorderSide::Right => 3,
        }
    }

    /// Whether the piece of this side that runs along `cell` is the one a test
    /// names.
    ///
    /// A side is drawn one cell at a time, so naming every piece would leave
    /// several of them sharing a name. The piece in the block's own corner is
    /// the one named: the top and the left are taken from the block's first
    /// cell, the bottom and the right from its last, which between them pin all
    /// four corners of the block.
    fn is_named_segment(self, rect: GridRect, cell: GridCell) -> bool {
        match self {
            BorderSide::Top => cell.col.get() == rect.first_col,
            BorderSide::Bottom => cell.col.get() == rect.last_col,
            BorderSide::Left => cell.row.index() == rect.first_row,
            BorderSide::Right => cell.row.index() == rect.last_row,
        }
    }
}

/// Which of `rect`'s outer sides run along `cell`, which is how a rectangle
/// spanning many cells is outlined one cell at a time.
fn sides_of(rect: GridRect, cell: GridCell) -> Vec<BorderSide> {
    let row = cell.row.index();
    let col = cell.col.get();
    let mut sides = Vec::with_capacity(4);
    if row == rect.first_row {
        sides.push(BorderSide::Top);
    }
    if row == rect.last_row {
        sides.push(BorderSide::Bottom);
    }
    if col == rect.first_col {
        sides.push(BorderSide::Left);
    }
    if col == rect.last_col {
        sides.push(BorderSide::Right);
    }
    sides
}

/// The row or column `distance` cells past `origin` in the direction a fill
/// went, or `None` when that falls off the top or the left of the grid.
fn step_from(origin: GridRect, forward: bool, vertical: bool, distance: usize) -> Option<usize> {
    let (first, last) = if vertical {
        (origin.first_row, origin.last_row)
    } else {
        (origin.first_col, origin.last_col)
    };
    if forward {
        last.checked_add(distance)
    } else {
        first.checked_sub(distance)
    }
}

/// Marks the fill handle so that a test can check it is drawn where the
/// selection ends. `debug_selector` compiles away outside of tests.
pub(crate) const FILL_HANDLE_DEBUG_SELECTOR: &str = "tabular-grid-fill-handle";

/// The same for the four sides of a selected block's outline, by block and then
/// by [`BorderSide::index`].
///
/// Only the first few blocks are named, because a test naming more than that
/// would be unreadable anyway; the outline itself is drawn around every one.
pub(crate) const SELECTION_OUTLINE_DEBUG_SELECTORS: [[&str; 4]; 3] = [
    [
        "tabular-grid-selection-outline-0-top",
        "tabular-grid-selection-outline-0-bottom",
        "tabular-grid-selection-outline-0-left",
        "tabular-grid-selection-outline-0-right",
    ],
    [
        "tabular-grid-selection-outline-1-top",
        "tabular-grid-selection-outline-1-bottom",
        "tabular-grid-selection-outline-1-left",
        "tabular-grid-selection-outline-1-right",
    ],
    [
        "tabular-grid-selection-outline-2-top",
        "tabular-grid-selection-outline-2-bottom",
        "tabular-grid-selection-outline-2-left",
        "tabular-grid-selection-outline-2-right",
    ],
];

/// A name from [`SELECTION_OUTLINE_DEBUG_SELECTORS`], or an unused one for a
/// block past the end of it.
fn selection_outline_debug_selector(block: usize, side: BorderSide) -> String {
    SELECTION_OUTLINE_DEBUG_SELECTORS
        .get(block)
        .and_then(|sides| sides.get(side.index()))
        .copied()
        .unwrap_or("tabular-grid-selection-outline-unnamed")
        .to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn values(source: &[&str]) -> Vec<String> {
        source.iter().map(|value| (*value).to_owned()).collect()
    }

    #[test]
    fn test_a_single_value_is_copied() {
        assert_eq!(
            fill_values(&values(&["x"]), 3, true, FillRule::Extend),
            values(&["x", "x", "x"])
        );
        assert_eq!(
            fill_values(&values(&["7"]), 2, true, FillRule::Extend),
            values(&["7", "7"])
        );
        assert_eq!(
            fill_values(&values(&["7"]), 2, false, FillRule::Extend),
            values(&["7", "7"])
        );
    }

    #[test]
    fn test_ctrl_counts_a_single_number_up() {
        assert_eq!(
            fill_values(&values(&["7"]), 3, true, FillRule::Invert),
            values(&["8", "9", "10"])
        );
        // Upwards the same gesture counts back, because the series is read over
        // the grid's own order.
        assert_eq!(
            fill_values(&values(&["7"]), 3, false, FillRule::Invert),
            values(&["6", "5", "4"])
        );
        // A word has no number to count, so Ctrl changes nothing.
        assert_eq!(
            fill_values(&values(&["x"]), 2, true, FillRule::Invert),
            values(&["x", "x"])
        );
    }

    #[test]
    fn test_an_even_series_is_extended() {
        assert_eq!(
            fill_values(&values(&["1", "2", "3"]), 3, true, FillRule::Extend),
            values(&["4", "5", "6"])
        );
        assert_eq!(
            fill_values(&values(&["1", "2", "3"]), 3, false, FillRule::Extend),
            values(&["0", "-1", "-2"])
        );
        assert_eq!(
            fill_values(&values(&["10", "20"]), 2, true, FillRule::Extend),
            values(&["30", "40"])
        );
        assert_eq!(
            fill_values(&values(&["3", "2", "1"]), 2, true, FillRule::Extend),
            values(&["0", "-1"])
        );
    }

    /// Decimals are stepped as scaled integers, so the values come back spelled
    /// the way the source was rather than as the nearest float.
    #[test]
    fn test_a_decimal_series_keeps_its_decimals() {
        assert_eq!(
            fill_values(&values(&["1.10", "1.20"]), 2, true, FillRule::Extend),
            values(&["1.30", "1.40"])
        );
        assert_eq!(
            fill_values(&values(&["1", "1.5"]), 2, true, FillRule::Extend),
            values(&["2.0", "2.5"])
        );
        assert_eq!(
            fill_values(&values(&["0.1", "0.2", "0.3"]), 1, true, FillRule::Extend),
            values(&["0.4"])
        );
    }

    #[test]
    fn test_an_uneven_or_non_numeric_block_repeats() {
        assert_eq!(
            fill_values(&values(&["1", "2", "4"]), 4, true, FillRule::Extend),
            values(&["1", "2", "4", "1"])
        );
        assert_eq!(
            fill_values(&values(&["a", "b"]), 3, true, FillRule::Extend),
            values(&["a", "b", "a"])
        );
        // Going backwards the block is stamped bottom up.
        assert_eq!(
            fill_values(&values(&["a", "b", "c"]), 4, false, FillRule::Extend),
            values(&["c", "b", "a", "c"])
        );
        assert_eq!(
            fill_values(&values(&["1", "", "3"]), 2, true, FillRule::Extend),
            values(&["1", ""])
        );
    }

    /// Ctrl flips a series back to a plain copy, which is the other half of
    /// Excel's modifier.
    #[test]
    fn test_ctrl_copies_a_series_instead_of_extending_it() {
        assert_eq!(
            fill_values(&values(&["1", "2"]), 4, true, FillRule::Invert),
            values(&["1", "2", "1", "2"])
        );
    }

    #[test]
    fn test_values_too_long_to_scale_are_copied() {
        let account = "1234567890123456789012345678901";
        assert_eq!(
            fill_values(&values(&[account]), 2, true, FillRule::Invert),
            values(&[account, account])
        );
    }

    #[test]
    fn test_a_negative_series_crosses_zero() {
        assert_eq!(
            fill_values(&values(&["-2", "-1"]), 4, true, FillRule::Extend),
            values(&["0", "1", "2", "3"])
        );
    }

    #[test]
    fn test_nothing_to_write() {
        assert!(fill_values(&values(&["a"]), 0, true, FillRule::Extend).is_empty());
        assert!(fill_values(&[], 3, true, FillRule::Extend).is_empty());
    }

    ///// Dates /////

    /// One date is a series of its own, which is the one place a drag extends
    /// without being asked - and where Ctrl therefore means the copy.
    #[test]
    fn test_a_single_date_steps_by_a_day() {
        assert_eq!(
            fill_values(&values(&["2026-09-18"]), 3, true, FillRule::Extend),
            values(&["2026-09-19", "2026-09-20", "2026-09-21"])
        );
        assert_eq!(
            fill_values(&values(&["2026-09-18"]), 2, false, FillRule::Extend),
            values(&["2026-09-17", "2026-09-16"])
        );
    }

    #[test]
    fn test_ctrl_copies_a_date_instead_of_stepping_it() {
        assert_eq!(
            fill_values(&values(&["2026-09-18"]), 2, true, FillRule::Invert),
            values(&["2026-09-18", "2026-09-18"])
        );
        assert_eq!(
            fill_values(
                &values(&["2026-09-18", "2026-09-20"]),
                3,
                true,
                FillRule::Invert
            ),
            values(&["2026-09-18", "2026-09-20", "2026-09-18"])
        );
    }

    /// Ctrl+D and Ctrl+R put the first line over the rest of the block and say
    /// nothing about series, so a column of dates filled down is that date
    /// again rather than the week after it.
    #[test]
    fn test_filling_within_a_selection_copies_a_date() {
        assert_eq!(
            fill_values(&values(&["2026-09-18"]), 2, true, FillRule::Copy),
            values(&["2026-09-18", "2026-09-18"])
        );
        assert_eq!(
            fill_values(&values(&["1", "2"]), 3, true, FillRule::Copy),
            values(&["1", "2", "1"])
        );
    }

    /// The separator and the zero padding are the source's, not this code's.
    #[test]
    fn test_a_date_is_written_back_the_way_it_was_spelled() {
        assert_eq!(
            fill_values(&values(&["2026/9/1"]), 2, true, FillRule::Extend),
            values(&["2026/9/2", "2026/9/3"])
        );
        assert_eq!(
            fill_values(&values(&["2026/09/01"]), 2, true, FillRule::Extend),
            values(&["2026/09/02", "2026/09/03"])
        );
        // Two digits of their own are not padding, so a day that grows into
        // them stays in the unpadded spelling.
        assert_eq!(
            fill_values(&values(&["2026/9/9"]), 1, true, FillRule::Extend),
            values(&["2026/9/10"])
        );
        assert_eq!(
            fill_values(&values(&["2026-1-1"]), 1, true, FillRule::Extend),
            values(&["2026-1-2"])
        );
        // The month says the column pads even where the day alone could not.
        assert_eq!(
            fill_values(&values(&["2026-01-31"]), 1, true, FillRule::Extend),
            values(&["2026-02-01"])
        );
        assert_eq!(
            fill_values(&values(&["2026/09/1"]), 1, true, FillRule::Extend),
            values(&["2026/09/2"])
        );
        // Where neither can, the separator does: a dash is ISO 8601 and pads,
        // a slash is the local short date and does not.
        assert_eq!(
            fill_values(&values(&["2026-10-31"]), 1, true, FillRule::Extend),
            values(&["2026-11-01"])
        );
        assert_eq!(
            fill_values(&values(&["2026/10/31"]), 1, true, FillRule::Extend),
            values(&["2026/11/1"])
        );
        assert_eq!(
            fill_values(&values(&["2026年10月31日"]), 1, true, FillRule::Extend),
            values(&["2026年11月1日"])
        );
    }

    #[test]
    fn test_a_date_crosses_months_leap_days_and_years() {
        assert_eq!(
            fill_values(&values(&["2026-01-31"]), 1, true, FillRule::Extend),
            values(&["2026-02-01"])
        );
        assert_eq!(
            fill_values(&values(&["2024-02-28"]), 2, true, FillRule::Extend),
            values(&["2024-02-29", "2024-03-01"])
        );
        // 2026 is not a leap year, so the same drag skips the 29th.
        assert_eq!(
            fill_values(&values(&["2026-02-28"]), 1, true, FillRule::Extend),
            values(&["2026-03-01"])
        );
        assert_eq!(
            fill_values(&values(&["2026-12-31"]), 1, true, FillRule::Extend),
            values(&["2027-01-01"])
        );
        assert_eq!(
            fill_values(&values(&["2026-01-01"]), 1, false, FillRule::Extend),
            values(&["2025-12-31"])
        );
    }

    #[test]
    fn test_an_even_run_of_dates_sets_the_step() {
        assert_eq!(
            fill_values(
                &values(&["2026-09-01", "2026-09-08"]),
                2,
                true,
                FillRule::Extend
            ),
            values(&["2026-09-15", "2026-09-22"])
        );
        assert_eq!(
            fill_values(
                &values(&["2026-09-15", "2026-09-22"]),
                2,
                false,
                FillRule::Extend
            ),
            values(&["2026-09-08", "2026-09-01"])
        );
        // An uneven run is not a series, so the block is stamped again instead.
        assert_eq!(
            fill_values(
                &values(&["2026-09-01", "2026-09-08", "2026-09-10"]),
                2,
                true,
                FillRule::Extend
            ),
            values(&["2026-09-01", "2026-09-08"])
        );
    }

    #[test]
    fn test_japanese_dates_keep_their_shape() {
        assert_eq!(
            fill_values(&values(&["2026年9月30日"]), 1, true, FillRule::Extend),
            values(&["2026年10月1日"])
        );
        assert_eq!(
            fill_values(&values(&["2026年09月30日"]), 1, true, FillRule::Extend),
            values(&["2026年10月01日"])
        );
    }

    /// A date written without a year has no year to move into, so it cycles
    /// inside one: the last day of the year steps to the first.
    #[test]
    fn test_a_date_without_a_year_cycles() {
        assert_eq!(
            fill_values(&values(&["12月31日"]), 2, true, FillRule::Extend),
            values(&["1月1日", "1月2日"])
        );
        assert_eq!(
            fill_values(&values(&["1月1日"]), 1, false, FillRule::Extend),
            values(&["12月31日"])
        );
        // A run that crosses the end of the year is an even one for the same
        // reason a run that crosses the end of the week is.
        assert_eq!(
            fill_values(
                &values(&["12月30日", "12月31日", "1月1日"]),
                2,
                true,
                FillRule::Extend
            ),
            values(&["1月2日", "1月3日"])
        );
        // Whether February has a 29th is a question about a year, so the one
        // day that depends on it is left to the copying rule.
        assert_eq!(
            fill_values(&values(&["2月29日"]), 1, true, FillRule::Extend),
            values(&["2月29日"])
        );
        assert_eq!(
            fill_values(&values(&["2月28日"]), 1, true, FillRule::Extend),
            values(&["3月1日"])
        );
    }

    #[test]
    fn test_weekdays_cycle_in_the_words_they_were_written_in() {
        assert_eq!(
            fill_values(&values(&["木", "金"]), 3, true, FillRule::Extend),
            values(&["土", "日", "月"])
        );
        assert_eq!(
            fill_values(&values(&["日曜日"]), 1, false, FillRule::Extend),
            values(&["土曜日"])
        );
        assert_eq!(
            fill_values(&values(&["Fri"]), 2, true, FillRule::Extend),
            values(&["Sat", "Sun"])
        );
        assert_eq!(
            fill_values(&values(&["friday"]), 1, true, FillRule::Extend),
            values(&["saturday"])
        );
        assert_eq!(
            fill_values(&values(&["FRI"]), 1, true, FillRule::Extend),
            values(&["SAT"])
        );
        // Two weekdays set the step the same way two dates do, counting the
        // short way round the week.
        assert_eq!(
            fill_values(&values(&["月", "水"]), 3, true, FillRule::Extend),
            values(&["金", "日", "火"])
        );
        // A run that crosses the end of the week is still an even one: the step
        // from Saturday to Sunday is a day forwards, not six backwards.
        assert_eq!(
            fill_values(&values(&["金", "土", "日"]), 2, true, FillRule::Extend),
            values(&["月", "火"])
        );
        // A case no fill could carry over is not a weekday at all.
        assert_eq!(
            fill_values(&values(&["fRIday"]), 1, true, FillRule::Extend),
            values(&["fRIday"])
        );
    }

    #[test]
    fn test_what_is_not_a_date_is_left_to_the_other_rules() {
        // A day the calendar does not have is neither a date nor a number.
        assert_eq!(
            fill_values(&values(&["2026/9/31"]), 2, true, FillRule::Extend),
            values(&["2026/9/31", "2026/9/31"])
        );
        // Without a separator it is a plain number, and Ctrl still counts it.
        assert_eq!(
            fill_values(&values(&["20260918"]), 1, true, FillRule::Extend),
            values(&["20260918"])
        );
        assert_eq!(
            fill_values(&values(&["20260918"]), 1, true, FillRule::Invert),
            values(&["20260919"])
        );
        // A time of day is out of scope.
        assert_eq!(
            fill_values(&values(&["2026-09-18 10:00"]), 1, true, FillRule::Extend),
            values(&["2026-09-18 10:00"])
        );
        // Two digits could be a year or a day, so they are refused rather than
        // guessed at.
        assert_eq!(
            fill_values(&values(&["26/9/1"]), 1, true, FillRule::Extend),
            values(&["26/9/1"])
        );
        // One series cannot be written back in two spellings.
        assert_eq!(
            fill_values(
                &values(&["2026-09-01", "2026/09/02"]),
                1,
                true,
                FillRule::Extend
            ),
            values(&["2026-09-01"])
        );
        assert_eq!(
            fill_values(
                &values(&["2026-09-01", "2026-9-2"]),
                1,
                true,
                FillRule::Extend
            ),
            values(&["2026-09-01"])
        );
        // A negative number starts with the character a date is split on.
        assert_eq!(
            fill_values(&values(&["-3", "-2"]), 2, true, FillRule::Extend),
            values(&["-1", "0"])
        );
    }

    /// A spelling that could just as well be something else is not taken for a
    /// date: a fill that quietly read one as a date would count the wrong thing
    /// up, and there is no way for the user to see that it had.
    #[test]
    fn test_a_spelling_that_could_be_read_two_ways_is_not_a_date() {
        // A year and a month with no day, which is also how a subtraction and a
        // range are written.
        assert_eq!(
            fill_values(&values(&["2026-09"]), 2, true, FillRule::Extend),
            values(&["2026-09", "2026-09"])
        );
        // A month and a day with no year, which is also a fraction and a ratio.
        assert_eq!(
            fill_values(&values(&["9/18"]), 2, true, FillRule::Extend),
            values(&["9/18", "9/18"])
        );
        assert_eq!(
            fill_values(&values(&["1/2"]), 2, true, FillRule::Extend),
            values(&["1/2", "1/2"])
        );
        // Two of them in a row do not make a series either.
        assert_eq!(
            fill_values(&values(&["1/2", "1/3"]), 2, true, FillRule::Extend),
            values(&["1/2", "1/3"])
        );
    }

    /// Two dates set a step in days and nothing more. Excel does not pull the
    /// series onto the end of each month - a month is not a fixed number of
    /// days - and neither does this: the month end is where the count lands, not
    /// where it is dragged to.
    #[test]
    fn test_two_dates_step_by_days_rather_than_by_months() {
        assert_eq!(
            fill_values(
                &values(&["2026/1/31", "2026/2/28"]),
                2,
                true,
                FillRule::Extend
            ),
            values(&["2026/3/28", "2026/4/25"])
        );
        assert_eq!(
            fill_values(
                &values(&["2026-01-31", "2026-02-28"]),
                1,
                false,
                FillRule::Extend
            ),
            values(&["2026-01-03"])
        );
    }

    /// A date the spelling cannot hold takes the whole fill back to copying,
    /// rather than leaving some cells stepped and the rest of the column in a
    /// shape it never used.
    #[test]
    fn test_a_date_past_the_end_of_the_spelling_is_copied() {
        assert_eq!(
            fill_values(&values(&["9999-12-31"]), 2, true, FillRule::Extend),
            values(&["9999-12-31", "9999-12-31"])
        );
        // The same date one step in is still a date.
        assert_eq!(
            fill_values(&values(&["9999-12-30"]), 1, true, FillRule::Extend),
            values(&["9999-12-31"])
        );
        assert_eq!(
            fill_values(&values(&["0001-01-01"]), 1, false, FillRule::Extend),
            values(&["0001-01-01"])
        );
    }

    /// The spellings this code cannot read come back byte for byte. Every one of
    /// them is multibyte where a date is not, which is where a reader written in
    /// byte offsets would either cut a character in half or take the wrong piece
    /// for the month.
    #[test]
    fn test_a_spelling_this_code_cannot_read_is_copied_as_it_stands() {
        for value in [
            "２０２６年９月１８日",
            "２０２６/９/１８",
            "１月２日",
            "18日9月",
            "月日",
            "年",
            "0月0日",
            "2026年0月1日",
            "ＳＵＮ",
            "Ｓun",
            "日曜日曜日",
            "😀",
            "",
            "   ",
        ] {
            assert_eq!(
                fill_values(&values(&[value]), 2, true, FillRule::Extend),
                values(&[value, value]),
                "{value:?} is not a date this code can write back"
            );
            assert_eq!(
                fill_values(&values(&[value]), 2, false, FillRule::Extend),
                values(&[value, value]),
                "{value:?} is not a date this code can write back, either way"
            );
        }
    }

    /// Ctrl is the copy for a date whichever way the drag went, and backwards a
    /// copy is stamped bottom up the way any other block is.
    #[test]
    fn test_ctrl_backwards_stamps_a_date_block_bottom_up() {
        assert_eq!(
            fill_values(&values(&["2026-09-18"]), 2, false, FillRule::Invert),
            values(&["2026-09-18", "2026-09-18"])
        );
        assert_eq!(
            fill_values(
                &values(&["2026-09-18", "2026-09-19"]),
                3,
                false,
                FillRule::Invert
            ),
            values(&["2026-09-19", "2026-09-18", "2026-09-19"])
        );
        assert_eq!(
            fill_values(&values(&["金", "土"]), 3, false, FillRule::Invert),
            values(&["土", "金", "土"])
        );
    }

    /// A week of cells comes back to the day it started on, in the word the cell
    /// used, and a drag upwards runs the week backwards.
    #[test]
    fn test_a_weekday_comes_back_round_the_week() {
        assert_eq!(
            fill_values(&values(&["Sunday"]), 7, true, FillRule::Extend),
            values(&[
                "Monday",
                "Tuesday",
                "Wednesday",
                "Thursday",
                "Friday",
                "Saturday",
                "Sunday"
            ])
        );
        assert_eq!(
            fill_values(&values(&["Fri"]), 3, false, FillRule::Extend),
            values(&["Thu", "Wed", "Tue"])
        );
        assert_eq!(
            fill_values(&values(&["日曜日"]), 7, false, FillRule::Extend),
            values(&[
                "土曜日",
                "金曜日",
                "木曜日",
                "水曜日",
                "火曜日",
                "月曜日",
                "日曜日"
            ])
        );
    }

    /// A weekday written as one kanji is an ordinary word as well - 金 is money,
    /// 水 is water - so one cell of it is not a series. A run of them is: no
    /// column of money reads 月, 火, 水 down the page by accident.
    ///
    /// The spellings that are nothing but a weekday are a series on their own,
    /// the way a date is.
    #[test]
    fn test_one_kanji_alone_is_a_word_rather_than_a_weekday() {
        for word in ["金", "水", "土", "日", "月", "火", "木"] {
            assert_eq!(
                fill_values(&values(&[word]), 2, true, FillRule::Extend),
                values(&[word, word]),
                "{word} on its own is a word, not the start of a week"
            );
            assert_eq!(
                fill_values(&values(&[word]), 2, false, FillRule::Extend),
                values(&[word, word]),
                "{word} on its own is a word upwards too"
            );
        }
        // Two of them in a row are a week, and the second one is what says so.
        assert_eq!(
            fill_values(&values(&["月", "火"]), 3, true, FillRule::Extend),
            values(&["水", "木", "金"])
        );
        assert_eq!(
            fill_values(&values(&["金", "金"]), 2, true, FillRule::Extend),
            values(&["金", "金"])
        );
        // A spelling that can only be a weekday needs no second cell.
        assert_eq!(
            fill_values(&values(&["日曜日"]), 1, true, FillRule::Extend),
            values(&["月曜日"])
        );
        assert_eq!(
            fill_values(&values(&["Sun"]), 1, true, FillRule::Extend),
            values(&["Mon"])
        );
        assert_eq!(
            fill_values(&values(&["Sunday"]), 1, true, FillRule::Extend),
            values(&["Monday"])
        );
        // Neither does a date, which has a separator to tell it apart.
        assert_eq!(
            fill_values(&values(&["2026-09-18"]), 1, true, FillRule::Extend),
            values(&["2026-09-19"])
        );
    }

    fn rect(first_row: usize, last_row: usize, first_col: usize, last_col: usize) -> GridRect {
        GridRect {
            first_row,
            last_row,
            first_col,
            last_col,
        }
    }

    /// The pointer decides the axis, and only one axis grows: dragging down and
    /// a little to the right fills down, not into a bigger rectangle.
    #[test]
    fn test_the_longer_drag_picks_the_axis() {
        let origin = rect(1, 1, 0, 0);
        assert_eq!(
            stretched_rect(origin, GridCell::new(GridRow::from_index(5), AnyColumn(1))),
            rect(1, 5, 0, 0)
        );
        assert_eq!(
            stretched_rect(origin, GridCell::new(GridRow::from_index(2), AnyColumn(4))),
            rect(1, 1, 0, 4)
        );
        // A tie goes to the vertical axis, the direction a spreadsheet grows in.
        assert_eq!(
            stretched_rect(origin, GridCell::new(GridRow::from_index(3), AnyColumn(2))),
            rect(1, 3, 0, 0)
        );
    }

    /// Dragging the handle back inside the block is Excel's "clear", which this
    /// grid deliberately does not do: nothing is written at all.
    #[test]
    fn test_dragging_inside_the_block_fills_nothing() {
        let origin = rect(1, 4, 0, 2);
        let inside = stretched_rect(origin, GridCell::new(GridRow::from_index(2), AnyColumn(1)));
        assert_eq!(inside, origin);
        assert_eq!(fill_axis(origin, inside), None);
    }

    #[test]
    fn test_fill_axis_reads_the_stretched_rectangle() {
        let origin = rect(2, 3, 1, 1);
        assert_eq!(
            fill_axis(origin, rect(2, 6, 1, 1)),
            Some(FillAxis {
                vertical: true,
                forward: true,
                extent: 3
            })
        );
        assert_eq!(
            fill_axis(origin, rect(0, 3, 1, 1)),
            Some(FillAxis {
                vertical: true,
                forward: false,
                extent: 2
            })
        );
        assert_eq!(
            fill_axis(origin, rect(2, 3, 1, 4)),
            Some(FillAxis {
                vertical: false,
                forward: true,
                extent: 3
            })
        );
        assert_eq!(
            fill_axis(origin, rect(2, 3, 0, 1)),
            Some(FillAxis {
                vertical: false,
                forward: false,
                extent: 1
            })
        );
    }

    #[test]
    fn test_a_shifted_block_keeps_its_shape() {
        assert_eq!(shifted_start(3, 2, 1, 10), 5);
        assert_eq!(shifted_start(3, -5, 1, 10), 1);
        assert_eq!(shifted_start(3, 20, 1, 10), 10);
        // A block taller than what is left below it cannot move at all.
        assert_eq!(shifted_start(1, 3, 1, 0), 1);
    }

    #[test]
    fn test_step_from_stops_at_the_edges() {
        let origin = rect(1, 2, 0, 1);
        assert_eq!(step_from(origin, true, true, 1), Some(3));
        assert_eq!(step_from(origin, false, true, 1), Some(0));
        assert_eq!(step_from(origin, false, true, 2), None);
        assert_eq!(step_from(origin, true, false, 2), Some(3));
        assert_eq!(step_from(origin, false, false, 1), None);
    }

    ///// The gestures, end to end /////

    use crate::{
        grid::{GridSelectionKind, MULTI_RANGE_REFUSAL, SelectionChange},
        grid_editing::tests::open_grid,
        types::DisplayRow,
    };
    use gpui::{Entity, Focusable as _, Modifiers, TestAppContext, VisualTestContext};

    fn body_cell(row: usize, col: usize) -> GridCell {
        GridCell::new(GridRow::Body(DisplayRow(row)), AnyColumn(col))
    }

    fn buffer_text(editor: &Entity<editor::Editor>, cx: &mut VisualTestContext) -> String {
        editor.read_with(cx, |editor, cx| editor.text(cx))
    }

    fn select(
        pane: &Entity<TabularDataPreviewPane>,
        from: GridCell,
        to: GridCell,
        cx: &mut VisualTestContext,
    ) {
        pane.update(cx, |pane, cx| {
            pane.change_cell_selection(from, SelectionChange::Replace, cx);
            pane.change_cell_selection(to, SelectionChange::Extend, cx);
        });
    }

    /// Drags the fill handle of the current selection out to `to` and lets go,
    /// which is the path the pointer takes: the handle starts the drag, the
    /// cells under the pointer move the preview, the button up commits it.
    fn drag_fill_handle(
        pane: &Entity<TabularDataPreviewPane>,
        to: GridCell,
        modifiers: Modifiers,
        cx: &mut VisualTestContext,
    ) {
        pane.update(cx, |pane, cx| {
            let origin = pane.selection_rect();
            pane.grid.drag = Some(GridDrag::Fill { origin });
            pane.grid.drag_preview = Some(origin);
            pane.on_cell_drag(to, cx);
            pane.end_drag(modifiers, cx);
        });
        cx.run_until_parked();
    }

    /// The same for the selection's border: grabbed on `grabbed` and dropped
    /// with `grabbed` over `to`.
    fn drag_block_border(
        pane: &Entity<TabularDataPreviewPane>,
        grabbed: GridCell,
        to: GridCell,
        modifiers: Modifiers,
        cx: &mut VisualTestContext,
    ) {
        pane.update(cx, |pane, cx| {
            let origin = pane.selection_rect();
            pane.grid.drag = Some(GridDrag::Move { origin, grabbed });
            pane.grid.drag_preview = Some(origin);
            pane.on_cell_drag(to, cx);
            pane.end_drag(modifiers, cx);
        });
        cx.run_until_parked();
    }

    #[gpui::test]
    async fn test_dragging_the_handle_copies_one_cell_down(cx: &mut TestAppContext) {
        let (pane, editor, mut cx) = open_grid("h,v\nx,1\n,2\n,3", cx).await;

        select(&pane, body_cell(0, 0), body_cell(0, 0), &mut cx);
        drag_fill_handle(&pane, body_cell(2, 0), Modifiers::none(), &mut cx);

        assert_eq!(buffer_text(&editor, &mut cx), "h,v\nx,1\nx,2\nx,3");
        pane.read_with(&cx, |pane, _| {
            let rect = pane.selection_rect();
            assert_eq!((rect.first_row, rect.last_row), (1, 3));
        });

        // The whole fill is one batch of edits.
        cx.dispatch_action(editor::actions::Undo);
        cx.run_until_parked();
        assert_eq!(buffer_text(&editor, &mut cx), "h,v\nx,1\n,2\n,3");
    }

    #[gpui::test]
    async fn test_dragging_the_handle_extends_a_series(cx: &mut TestAppContext) {
        let (pane, editor, mut cx) = open_grid("h\n1\n2\nx\nx\nx", cx).await;

        select(&pane, body_cell(0, 0), body_cell(1, 0), &mut cx);
        drag_fill_handle(&pane, body_cell(4, 0), Modifiers::none(), &mut cx);

        assert_eq!(buffer_text(&editor, &mut cx), "h\n1\n2\n3\n4\n5");
    }

    /// Ctrl on the way up turns one number into a counted series. It is read at
    /// the end of the gesture, which is when Excel reads it.
    #[gpui::test]
    async fn test_ctrl_on_release_counts_a_single_number_up(cx: &mut TestAppContext) {
        let (pane, editor, mut cx) = open_grid("h\n7\nx\nx", cx).await;

        select(&pane, body_cell(0, 0), body_cell(0, 0), &mut cx);
        drag_fill_handle(&pane, body_cell(2, 0), Modifiers::secondary_key(), &mut cx);

        assert_eq!(buffer_text(&editor, &mut cx), "h\n7\n8\n9");
    }

    #[gpui::test]
    async fn test_dragging_the_handle_sideways_fills_the_row(cx: &mut TestAppContext) {
        let (pane, editor, mut cx) = open_grid("a,b,c\n5,,", cx).await;

        select(&pane, body_cell(0, 0), body_cell(0, 0), &mut cx);
        drag_fill_handle(&pane, body_cell(0, 2), Modifiers::none(), &mut cx);

        assert_eq!(buffer_text(&editor, &mut cx), "a,b,c\n5,5,5");
    }

    #[gpui::test]
    async fn test_dragging_the_handle_upwards_extrapolates_backwards(cx: &mut TestAppContext) {
        let (pane, editor, mut cx) = open_grid("h\nx\nx\n3\n4", cx).await;

        select(&pane, body_cell(2, 0), body_cell(3, 0), &mut cx);
        drag_fill_handle(&pane, body_cell(0, 0), Modifiers::none(), &mut cx);

        assert_eq!(buffer_text(&editor, &mut cx), "h\n1\n2\n3\n4");
    }

    /// A fill writes the rows the user can see, so under a filter it skips the
    /// hidden row in between - which is what Excel does with a filtered list.
    #[gpui::test]
    async fn test_filling_a_filtered_table_writes_the_visible_rows(cx: &mut TestAppContext) {
        let (pane, editor, mut cx) = open_grid("k,v\na,1\nb,2\na,3\na,4", cx).await;

        pane.update(&mut cx, |pane, cx| {
            pane.toggle_filter(AnyColumn(0), Some("a".into()), cx);
        });
        cx.run_until_parked();
        pane.read_with(&cx, |pane, _| assert_eq!(pane.body_row_count(), 3));

        select(&pane, body_cell(0, 1), body_cell(0, 1), &mut cx);
        drag_fill_handle(&pane, body_cell(2, 1), Modifiers::none(), &mut cx);

        assert_eq!(buffer_text(&editor, &mut cx), "k,v\na,1\nb,2\na,1\na,1");

        cx.dispatch_action(editor::actions::Undo);
        cx.run_until_parked();
        assert_eq!(buffer_text(&editor, &mut cx), "k,v\na,1\nb,2\na,3\na,4");
    }

    #[gpui::test]
    async fn test_dragging_the_handle_back_inside_writes_nothing(cx: &mut TestAppContext) {
        let (pane, editor, mut cx) = open_grid("h\n1\n2\n3", cx).await;

        select(&pane, body_cell(0, 0), body_cell(2, 0), &mut cx);
        drag_fill_handle(&pane, body_cell(1, 0), Modifiers::none(), &mut cx);

        assert_eq!(buffer_text(&editor, &mut cx), "h\n1\n2\n3");
    }

    #[gpui::test]
    async fn test_ctrl_d_copies_the_first_row_over_the_selection(cx: &mut TestAppContext) {
        let (pane, editor, mut cx) = open_grid("a,b\n1,2\n3,4\n5,6", cx).await;

        select(&pane, body_cell(0, 0), body_cell(2, 1), &mut cx);
        cx.simulate_keystrokes("ctrl-d");
        cx.run_until_parked();

        assert_eq!(buffer_text(&editor, &mut cx), "a,b\n1,2\n1,2\n1,2");
        // The fill stayed inside the selection, so the selection is unchanged.
        pane.read_with(&cx, |pane, _| {
            let rect = pane.selection_rect();
            assert_eq!((rect.first_row, rect.last_row), (1, 3));
        });
    }

    /// A fill that wrote inside the selection leaves it alone, so selected rows
    /// stay selected rows: turning them into a cell range would quietly change
    /// what the next Delete or the next color covers.
    #[gpui::test]
    async fn test_ctrl_d_on_selected_rows_leaves_them_selected_as_rows(cx: &mut TestAppContext) {
        let (pane, editor, mut cx) = open_grid("a,b\n1,2\n3,4", cx).await;

        pane.update(&mut cx, |pane, cx| {
            pane.change_row_selection(GridRow::Body(DisplayRow(0)), SelectionChange::Replace, cx);
            pane.change_row_selection(GridRow::Body(DisplayRow(1)), SelectionChange::Extend, cx);
        });
        cx.simulate_keystrokes("ctrl-d");
        cx.run_until_parked();

        assert_eq!(buffer_text(&editor, &mut cx), "a,b\n1,2\n1,2");
        pane.read_with(&cx, |pane, _| {
            assert_eq!(
                pane.grid.selection_kind,
                GridSelectionKind::Rows,
                "the rows the fill covered are still selected as rows"
            );
        });
    }

    #[gpui::test]
    async fn test_ctrl_r_copies_the_first_column_over_the_selection(cx: &mut TestAppContext) {
        let (pane, editor, mut cx) = open_grid("a,b,c\n1,2,3", cx).await;

        select(&pane, body_cell(0, 0), body_cell(0, 2), &mut cx);
        cx.simulate_keystrokes("ctrl-r");
        cx.run_until_parked();

        assert_eq!(buffer_text(&editor, &mut cx), "a,b,c\n1,1,1");
    }

    /// Ctrl+D is the editor's "select next occurrence" everywhere else, so the
    /// grid has to reach it at all - and has to leave it alone while a cell is
    /// open for input, where the keys belong to the text being typed.
    #[gpui::test]
    async fn test_ctrl_d_does_nothing_while_a_cell_is_open(cx: &mut TestAppContext) {
        let (_pane, editor, mut cx) = open_grid("a,b\n1,2\n3,4", cx).await;

        cx.simulate_keystrokes("down");
        cx.simulate_input("z");
        cx.simulate_keystrokes("ctrl-d");
        cx.run_until_parked();
        cx.dispatch_action(crate::CommitEditing);
        cx.run_until_parked();

        assert_eq!(buffer_text(&editor, &mut cx), "a,b\nz,2\n3,4");
    }

    #[gpui::test]
    async fn test_a_multiple_selection_refuses_ctrl_d(cx: &mut TestAppContext) {
        let (pane, editor, mut cx) = open_grid("a,b\n1,2\n3,4", cx).await;

        pane.update(&mut cx, |pane, cx| {
            pane.change_cell_selection(body_cell(0, 0), SelectionChange::Replace, cx);
            pane.change_cell_selection(body_cell(1, 0), SelectionChange::Extend, cx);
            pane.change_cell_selection(body_cell(0, 1), SelectionChange::Add, cx);
            pane.change_cell_selection(body_cell(1, 1), SelectionChange::Extend, cx);
        });
        cx.simulate_keystrokes("ctrl-d");
        cx.run_until_parked();

        assert_eq!(buffer_text(&editor, &mut cx), "a,b\n1,2\n3,4");
        pane.read_with(&cx, |pane, _| {
            assert_eq!(
                pane.grid.status_message.as_deref(),
                Some(MULTI_RANGE_REFUSAL)
            );
        });
    }

    #[gpui::test]
    async fn test_dragging_the_border_moves_the_block(cx: &mut TestAppContext) {
        let (pane, editor, mut cx) = open_grid("a,b,c\n1,2,\n3,4,\n,,", cx).await;

        select(&pane, body_cell(0, 0), body_cell(1, 1), &mut cx);
        drag_block_border(
            &pane,
            body_cell(0, 0),
            body_cell(1, 1),
            Modifiers::none(),
            &mut cx,
        );

        assert_eq!(buffer_text(&editor, &mut cx), "a,b,c\n,,\n,1,2\n,3,4");
        pane.read_with(&cx, |pane, _| {
            let rect = pane.selection_rect();
            assert_eq!((rect.first_row, rect.first_col), (2, 1));
        });

        // The clear and the write are one batch, so one undo puts it all back.
        cx.dispatch_action(editor::actions::Undo);
        cx.run_until_parked();
        assert_eq!(buffer_text(&editor, &mut cx), "a,b,c\n1,2,\n3,4,\n,,");
    }

    #[gpui::test]
    async fn test_ctrl_on_release_copies_the_block_instead(cx: &mut TestAppContext) {
        let (pane, editor, mut cx) = open_grid("a,b,c\n1,2,\n,,\n,,", cx).await;

        select(&pane, body_cell(0, 0), body_cell(0, 1), &mut cx);
        drag_block_border(
            &pane,
            body_cell(0, 0),
            body_cell(2, 0),
            Modifiers::secondary_key(),
            &mut cx,
        );

        assert_eq!(buffer_text(&editor, &mut cx), "a,b,c\n1,2,\n,,\n1,2,");
    }

    /// A block dragged past the last row stops at it whole rather than losing
    /// the rows that would fall off.
    #[gpui::test]
    async fn test_a_moved_block_stays_inside_the_table(cx: &mut TestAppContext) {
        let (pane, editor, mut cx) = open_grid("a,b\n1,2\n3,4\n,", cx).await;

        select(&pane, body_cell(0, 0), body_cell(1, 1), &mut cx);
        drag_block_border(
            &pane,
            body_cell(0, 0),
            body_cell(2, 1),
            Modifiers::none(),
            &mut cx,
        );

        // Two rows starting on the last one would need a fourth row; the block
        // lands on the last two instead.
        assert_eq!(buffer_text(&editor, &mut cx), "a,b\n,\n1,2\n3,4");
    }

    /// A row that came out of the file shorter than the table is wide has no
    /// cell to write the block's second column into; the engine grows the row
    /// rather than dropping the value.
    #[gpui::test]
    async fn test_a_block_moved_onto_a_short_row_grows_it(cx: &mut TestAppContext) {
        let (pane, editor, mut cx) = open_grid("a,b,c\n1,2,3\nx", cx).await;

        select(&pane, body_cell(0, 0), body_cell(0, 1), &mut cx);
        drag_block_border(
            &pane,
            body_cell(0, 0),
            body_cell(1, 0),
            Modifiers::none(),
            &mut cx,
        );

        assert_eq!(buffer_text(&editor, &mut cx), "a,b,c\n,,3\n1,2");
    }

    /// Sorted or filtered, the rows on screen are not the rows of the file, so
    /// the border stops being grabbable at all.
    #[gpui::test]
    async fn test_the_border_cannot_be_grabbed_while_filtered(cx: &mut TestAppContext) {
        let (pane, _editor, mut cx) = open_grid("k,v\na,1\nb,2\na,3", cx).await;

        pane.read_with(&cx, |pane, _| assert!(pane.block_move_is_available()));

        pane.update(&mut cx, |pane, cx| {
            pane.toggle_filter(AnyColumn(0), Some("a".into()), cx);
        });
        cx.run_until_parked();

        pane.read_with(&cx, |pane, _| {
            assert!(!pane.block_move_is_available());
            // The fill handle stays: a fill writes what the user can see.
            assert!(pane.fill_handle_cell().is_some());
        });
    }

    #[gpui::test]
    async fn test_the_handle_is_drawn_at_the_end_of_a_single_block(cx: &mut TestAppContext) {
        let (pane, _editor, mut cx) = open_grid("a,b\n1,2\n3,4", cx).await;

        select(&pane, body_cell(0, 0), body_cell(1, 1), &mut cx);
        cx.run_until_parked();

        pane.read_with(&cx, |pane, _| {
            assert_eq!(pane.fill_handle_cell(), Some(body_cell(1, 1)));
        });
        let handle = cx
            .debug_bounds(FILL_HANDLE_DEBUG_SELECTOR)
            .expect("the fill handle should be drawn at the corner of the selection");
        assert!(
            handle.size.width > px(0.) && handle.size.height > px(0.),
            "the fill handle was drawn with no size ({handle:?})"
        );

        // Two blocks have no one corner to drag, so the handle goes away.
        pane.update(&mut cx, |pane, cx| {
            pane.change_cell_selection(body_cell(0, 1), SelectionChange::Add, cx);
        });
        cx.run_until_parked();
        pane.read_with(&cx, |pane, _| assert_eq!(pane.fill_handle_cell(), None));
        assert_eq!(cx.debug_bounds(FILL_HANDLE_DEBUG_SELECTOR), None);
    }

    /// The handle sits on top of a cell that takes presses of its own, so the
    /// press has to reach the handle and stop there - and, because stopping it
    /// also keeps it from the grid's root, the handle has to put the caret back
    /// in the grid itself, the way any other press in the grid does.
    #[gpui::test]
    async fn test_pressing_the_handle_starts_a_fill_rather_than_a_selection(
        cx: &mut TestAppContext,
    ) {
        let (pane, _editor, mut cx) = open_grid("h\n1\n2\n3", cx).await;

        select(&pane, body_cell(0, 0), body_cell(0, 0), &mut cx);
        cx.run_until_parked();
        let handle = cx
            .debug_bounds(FILL_HANDLE_DEBUG_SELECTOR)
            .expect("the fill handle should be drawn at the corner of the selection");

        let grid_focus = pane.read_with(&cx, |pane, _| pane.focus_handle.clone());
        cx.update(|window, cx| window.focus(&grid_focus, cx));
        cx.run_until_parked();
        cx.update(|window, cx| {
            assert!(
                !pane
                    .read(cx)
                    .grid
                    .cell_editor
                    .focus_handle(cx)
                    .is_focused(window),
                "the fixture has to start with the caret away from the cell editor"
            );
        });

        cx.simulate_mouse_down(handle.center(), MouseButton::Left, Modifiers::none());
        cx.run_until_parked();

        pane.read_with(&cx, |pane, _| {
            assert!(
                matches!(pane.grid.drag, Some(GridDrag::Fill { .. })),
                "the press on the handle should start a fill, not a cell drag \
                 ({:?})",
                pane.grid.drag
            );
            assert_eq!(
                pane.selection_rect(),
                rect(1, 1, 0, 0),
                "the press must not move the selection it is about to stretch"
            );
        });
        cx.update(|window, cx| {
            assert!(
                pane.read(cx)
                    .grid
                    .cell_editor
                    .focus_handle(cx)
                    .is_focused(window),
                "the press has to take the caret back, since it never reaches the root"
            );
        });
    }

    /// The colors are painted on every block, and a block that is a whole row
    /// stays one stored entry rather than one per cell.
    #[gpui::test]
    async fn test_colors_cover_every_selected_block(cx: &mut TestAppContext) {
        use crate::grid_styles::{CellColor, StyleField};

        let (pane, _editor, mut cx) = open_grid("a,b,c\n1,2,3\n4,5,6", cx).await;

        pane.update(&mut cx, |pane, cx| {
            pane.change_cell_selection(body_cell(0, 0), SelectionChange::Replace, cx);
            pane.change_row_selection(GridRow::Body(DisplayRow(1)), SelectionChange::Add, cx);
            pane.set_selection_color(StyleField::Fill, CellColor::Red, cx);
        });
        cx.run_until_parked();

        pane.read_with(&cx, |pane, cx| {
            let styles = pane.styles.read(cx).styles();
            assert_eq!(styles.style_for(1, 0).fill, Some(CellColor::Red));
            assert_eq!(styles.style_for(2, 0).fill, Some(CellColor::Red));
            assert_eq!(styles.style_for(2, 2).fill, Some(CellColor::Red));
            // The cell block did not spill onto the rest of its row.
            assert_eq!(styles.style_for(1, 1).fill, None);
            assert_eq!(
                pane.selection_style_scopes().len(),
                2,
                "each block paints its own scope"
            );
        });
    }

    /// Pinning the search to a multiple selection can only name one rectangle,
    /// so it names the one around all of the blocks.
    #[gpui::test]
    async fn test_the_search_range_is_the_rectangle_around_every_block(cx: &mut TestAppContext) {
        let (pane, _editor, mut cx) = open_grid("a,b,c\n1,2,3\n4,5,6\n7,8,9", cx).await;

        pane.update(&mut cx, |pane, cx| {
            pane.change_cell_selection(body_cell(0, 0), SelectionChange::Replace, cx);
            pane.change_cell_selection(body_cell(2, 2), SelectionChange::Add, cx);
        });

        pane.read_with(&cx, |pane, _| {
            assert_eq!(
                pane.selection_bounding_rect(),
                GridRect {
                    first_row: 1,
                    last_row: 3,
                    first_col: 0,
                    last_col: 2,
                }
            );
            assert_eq!(pane.grid.selection_kind, GridSelectionKind::Cells);
        });
    }

    /// Selecting a whole column selects its heading with it, and a fill down
    /// from the heading would stamp the column's name over every value in it.
    /// The fill starts at the first row of the body instead - and leaves the
    /// column selected as a column.
    #[gpui::test]
    async fn test_ctrl_d_on_a_whole_column_starts_below_the_heading(cx: &mut TestAppContext) {
        let (pane, editor, mut cx) = open_grid("h,k\n1,a\n2,b\n3,c", cx).await;

        pane.update(&mut cx, |pane, cx| {
            pane.change_column_selection(AnyColumn(0), SelectionChange::Replace, cx);
        });
        cx.simulate_keystrokes("ctrl-d");
        cx.run_until_parked();

        assert_eq!(buffer_text(&editor, &mut cx), "h,k\n1,a\n1,b\n1,c");
        pane.read_with(&cx, |pane, _| {
            assert_eq!(
                pane.grid.selection_kind,
                GridSelectionKind::Columns,
                "the column the fill covered is still selected as a column"
            );
        });
    }

    /// A cell range the user drew by hand that happens to reach into the heading
    /// is left alone: that one they asked for.
    #[gpui::test]
    async fn test_ctrl_d_on_a_hand_drawn_range_still_starts_at_its_top(cx: &mut TestAppContext) {
        let (pane, editor, mut cx) = open_grid("h,k\n1,a\n2,b", cx).await;

        select(
            &pane,
            GridCell::new(GridRow::Header, AnyColumn(0)),
            body_cell(1, 0),
            &mut cx,
        );
        cx.simulate_keystrokes("ctrl-d");
        cx.run_until_parked();

        assert_eq!(buffer_text(&editor, &mut cx), "h,k\nh,a\nh,b");
    }

    /// Without a heading row there is nothing to skip, so a whole column fills
    /// from its own first row.
    #[gpui::test]
    async fn test_ctrl_d_on_a_column_of_a_headerless_file(cx: &mut TestAppContext) {
        let (pane, editor, mut cx) = open_grid("1,a\n2,b\n3,c", cx).await;

        pane.update(&mut cx, |pane, cx| {
            pane.toggle_header_row(cx);
        });
        cx.run_until_parked();
        pane.read_with(&cx, |pane, _| assert!(!pane.has_header_row()));

        pane.update(&mut cx, |pane, cx| {
            pane.change_column_selection(AnyColumn(0), SelectionChange::Replace, cx);
        });
        cx.simulate_keystrokes("ctrl-d");
        cx.run_until_parked();

        assert_eq!(buffer_text(&editor, &mut cx), "1,a\n1,b\n1,c");
    }

    ///// Dates, end to end /////

    #[gpui::test]
    async fn test_dragging_the_handle_steps_a_date_by_a_day(cx: &mut TestAppContext) {
        let (pane, editor, mut cx) = open_grid("d\n2026-09-18\nx\nx", cx).await;

        select(&pane, body_cell(0, 0), body_cell(0, 0), &mut cx);
        drag_fill_handle(&pane, body_cell(2, 0), Modifiers::none(), &mut cx);

        assert_eq!(
            buffer_text(&editor, &mut cx),
            "d\n2026-09-18\n2026-09-19\n2026-09-20"
        );
    }

    #[gpui::test]
    async fn test_ctrl_on_release_copies_a_date_instead_of_stepping_it(cx: &mut TestAppContext) {
        let (pane, editor, mut cx) = open_grid("d\n2026-09-18\nx\nx", cx).await;

        select(&pane, body_cell(0, 0), body_cell(0, 0), &mut cx);
        drag_fill_handle(&pane, body_cell(2, 0), Modifiers::secondary_key(), &mut cx);

        assert_eq!(
            buffer_text(&editor, &mut cx),
            "d\n2026-09-18\n2026-09-18\n2026-09-18"
        );
    }

    /// Ctrl+D copies the first line over the rest of the block, and a column of
    /// dates is no exception: the key the user reached for says "the same
    /// value", not "the days after it".
    #[gpui::test]
    async fn test_ctrl_d_copies_a_date_rather_than_counting_it(cx: &mut TestAppContext) {
        let (pane, editor, mut cx) = open_grid("d\n2026-09-18\nx\nx", cx).await;

        select(&pane, body_cell(0, 0), body_cell(2, 0), &mut cx);
        cx.simulate_keystrokes("ctrl-d");
        cx.run_until_parked();

        assert_eq!(
            buffer_text(&editor, &mut cx),
            "d\n2026-09-18\n2026-09-18\n2026-09-18"
        );
    }

    /// A weekday runs through the week and round again, which is a multibyte
    /// spelling read and written back whole. Two cells are what say the column
    /// holds weekdays at all, since one kanji on its own is an ordinary word.
    #[gpui::test]
    async fn test_dragging_the_handle_runs_through_the_week(cx: &mut TestAppContext) {
        let (pane, editor, mut cx) = open_grid("曜日\n木\n金\nx\nx", cx).await;

        select(&pane, body_cell(0, 0), body_cell(1, 0), &mut cx);
        drag_fill_handle(&pane, body_cell(3, 0), Modifiers::none(), &mut cx);

        assert_eq!(buffer_text(&editor, &mut cx), "曜日\n木\n金\n土\n日");
    }

    /// The same column with only one kanji in it is left alone: a cell holding
    /// 金 is as likely to be money as it is to be Friday.
    #[gpui::test]
    async fn test_dragging_one_kanji_copies_it(cx: &mut TestAppContext) {
        let (pane, editor, mut cx) = open_grid("金額\n金\nx\nx", cx).await;

        select(&pane, body_cell(0, 0), body_cell(0, 0), &mut cx);
        drag_fill_handle(&pane, body_cell(2, 0), Modifiers::none(), &mut cx);

        assert_eq!(buffer_text(&editor, &mut cx), "金額\n金\n金\n金");
    }

    ///// What a gesture leaves selected /////

    /// A fill dragged out of selected rows leaves rows selected. Turning them
    /// into a cell range would quietly change what the next Delete, or the next
    /// color, covers.
    #[gpui::test]
    async fn test_a_fill_dragged_out_of_selected_rows_keeps_them_rows(cx: &mut TestAppContext) {
        let (pane, editor, mut cx) = open_grid("a,b\n1,2\n,\n,", cx).await;

        pane.update(&mut cx, |pane, cx| {
            pane.change_row_selection(GridRow::Body(DisplayRow(0)), SelectionChange::Replace, cx);
        });
        drag_fill_handle(&pane, body_cell(2, 1), Modifiers::none(), &mut cx);

        assert_eq!(buffer_text(&editor, &mut cx), "a,b\n1,2\n1,2\n1,2");
        pane.read_with(&cx, |pane, _| {
            assert_eq!(
                pane.grid.selection_kind,
                GridSelectionKind::Rows,
                "the rows the fill grew into are still selected as rows"
            );
            let rect = pane.selection_rect();
            assert_eq!((rect.first_row, rect.last_row), (1, 3));
        });
    }

    #[gpui::test]
    async fn test_a_fill_dragged_out_of_a_selected_column_keeps_it_a_column(
        cx: &mut TestAppContext,
    ) {
        let (pane, editor, mut cx) = open_grid("a,b\n1,\n2,", cx).await;

        pane.update(&mut cx, |pane, cx| {
            pane.change_column_selection(AnyColumn(0), SelectionChange::Replace, cx);
        });
        drag_fill_handle(
            &pane,
            GridCell::new(GridRow::Body(DisplayRow(1)), AnyColumn(1)),
            Modifiers::none(),
            &mut cx,
        );

        // A whole column carries its heading, and a sideways drag has no rule
        // about headings the way Ctrl+D does, so the heading is copied too.
        assert_eq!(buffer_text(&editor, &mut cx), "a,a\n1,1\n2,2");
        pane.read_with(&cx, |pane, _| {
            assert_eq!(
                pane.grid.selection_kind,
                GridSelectionKind::Columns,
                "the columns the fill grew into are still selected as columns"
            );
        });
    }

    /// A gesture that lands on a rectangle its selection's kind cannot mean
    /// leaves a plain cell range behind: rows and columns are stored as two
    /// corners and drawn back out across the whole table, so keeping the kind
    /// would widen the selection behind the user's back.
    #[gpui::test]
    async fn test_a_kind_that_no_longer_fits_the_rectangle_falls_back_to_cells(
        cx: &mut TestAppContext,
    ) {
        let (pane, _editor, mut cx) = open_grid("a,b,c\n1,2,3\n4,5,6", cx).await;

        pane.update(&mut cx, |pane, cx| {
            pane.change_row_selection(GridRow::Body(DisplayRow(0)), SelectionChange::Replace, cx);
            assert_eq!(pane.grid.selection_kind, GridSelectionKind::Rows);

            pane.select_rect(rect(1, 2, 0, 1), cx);
            assert_eq!(
                pane.grid.selection_kind,
                GridSelectionKind::Cells,
                "two of the three columns are not whole rows"
            );
            assert_eq!(pane.selection_rect(), rect(1, 2, 0, 1));
        });
    }
}
