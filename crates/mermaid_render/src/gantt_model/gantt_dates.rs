//! Writing down a moment `merman` resolved a task to, as text the chart's own
//! `dateFormat` reads back to the same moment.
//!
//! Deleting a task that another one starts `after` leaves that other task
//! needing a date of its own, and `merman` only hands out milliseconds. Those
//! are turned into the local wall clock time `merman` reads a date as by asking
//! `merman` itself, and then spelled with the `dayjs` tokens a gantt chart's
//! `dateFormat` is written in. Only the tokens that spell a number are
//! supported; anything else is an error rather than a guess, and the caller
//! checks the result by drawing the chart again in any case.

use anyhow::{Result, anyhow, bail};
use merman::diagram::RenderSemanticModel;
use merman::{Engine, ParseOptions};

const DAY: i64 = 86_400_000;
const HOUR: i64 = 3_600_000;
const MINUTE: i64 = 60_000;
const SECOND: i64 = 1_000;

/// Where one task starts and ends, in milliseconds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct TaskTime {
    pub(super) start: i64,
    /// The end `after` and `until` read.
    pub(super) end: i64,
    /// Where the bar stops. `excludes` pushes `end` past the days it skips,
    /// and when the last of those days are skipped ones the bar is drawn
    /// shorter than `end`; which of the two a task gets depends on how its end
    /// field is spelled, not only on the moment it resolves to.
    pub(super) drawn_end: i64,
}

/// Where every task of `source` starts and ends, in the order the fence
/// writes them.
pub(super) fn task_times(source: &str) -> Result<Vec<TaskTime>> {
    let parsed = Engine::new()
        .parse_diagram_for_render_model_sync(source, ParseOptions::strict())
        .map_err(|error| anyhow!("{error}"))?
        .ok_or_else(|| anyhow!("Mermaid の図として解釈できませんでした"))?;
    let RenderSemanticModel::Gantt(model) = parsed.into_parts().1 else {
        bail!("ガントチャートではありません");
    };
    let mut tasks = model
        .tasks
        .iter()
        .map(|task| {
            (
                task.order,
                TaskTime {
                    start: task.start_ms,
                    end: task.end_ms,
                    drawn_end: task.render_end_ms.unwrap_or(task.end_ms),
                },
            )
        })
        .collect::<Vec<_>>();
    tasks.sort_by_key(|(order, _)| *order);
    Ok(tasks.into_iter().map(|(_, time)| time).collect())
}

/// How a task's start field has to be spelled to start at `moment`.
pub(super) fn start_text(date_format: Option<&str>, moment: i64) -> Result<String> {
    match date_format.map(str::trim).unwrap_or_default() {
        // With no format Mermaid falls back to JavaScript's `Date`, which reads
        // a bare `YYYY-MM-DD` as midnight UTC and a date with a time in it as
        // local time.
        "" => {
            if moment.rem_euclid(DAY) == 0 {
                format_civil(moment, "YYYY-MM-DD")
            } else {
                let local = local_civil(moment)?;
                let format = if local.rem_euclid(SECOND) == 0 {
                    "YYYY-MM-DD HH:mm:ss"
                } else {
                    "YYYY-MM-DD HH:mm:ss.SSS"
                };
                format_civil(local, format)
            }
        }
        // A start made of digits is read as milliseconds under `X` as well as
        // under `x`; only a negative one goes through the format's own unit.
        "x" => Ok(moment.to_string()),
        "X" if moment >= 0 => Ok(moment.to_string()),
        "X" => seconds_text(moment),
        format => format_civil(local_civil(moment)?, format),
    }
}

/// How a task's end field has to be spelled to end at `moment`, for a task that
/// starts at `start`.
///
/// A date is preferred, because that is what an `until` meant: an end that
/// stays put when the start is edited. When the format cannot spell one, the
/// length from `start` is the fallback.
pub(super) fn end_text(
    date_format: Option<&str>,
    inclusive: bool,
    moment: i64,
    start: i64,
) -> Result<String> {
    let as_date = || -> Result<String> {
        // `inclusiveEndDates` adds a day to an end that is read as a date.
        let shift = if inclusive { DAY } else { 0 };
        match date_format.map(str::trim).unwrap_or_default() {
            "" => bail!("`dateFormat` が無いと終了を日付で書けません"),
            "x" => Ok((moment - shift).to_string()),
            "X" => seconds_text(moment - shift),
            format => format_civil(local_civil(moment)? - shift, format),
        }
    };
    as_date().or_else(|date_error| duration_text(moment - start).map_err(|_| date_error))
}

fn seconds_text(moment: i64) -> Result<String> {
    if moment % SECOND != 0 {
        bail!("`X` (秒) では 1 秒未満を書けません");
    }
    Ok((moment / SECOND).to_string())
}

fn duration_text(length: i64) -> Result<String> {
    if length < 0 {
        bail!("終了が開始より前です");
    }
    for (unit, size) in [("d", DAY), ("h", HOUR), ("m", MINUTE), ("s", SECOND)] {
        if length % size == 0 {
            return Ok(format!("{}{unit}", length / size));
        }
    }
    Ok(format!("{length}ms"))
}

/// The wall clock time, as milliseconds since 1970-01-01T00:00 with no time
/// zone, that `merman` reads as the instant `moment`.
///
/// `merman` reads a date in the local time zone, and this crate has no time
/// zone database of its own, so `merman` is asked: a guess is read back and
/// corrected by how far off it came out, which settles in one step outside a
/// daylight saving change.
fn local_civil(moment: i64) -> Result<i64> {
    let mut guess = moment;
    for _ in 0..3 {
        let read = read_as_local(guess)?;
        if read == moment {
            return Ok(guess);
        }
        guess += moment - read;
    }
    bail!("この日時は現地時刻で書けません")
}

fn read_as_local(civil: i64) -> Result<i64> {
    const PROBE_FORMAT: &str = "YYYY-MM-DD HH:mm:ss.SSS";
    let text = format_civil(civil, PROBE_FORMAT)?;
    let source = format!("gantt\n    dateFormat {PROBE_FORMAT}\n    probe :probe, {text}, 1d");
    task_times(&source)?
        .first()
        .map(|time| time.start)
        .ok_or_else(|| anyhow!("日時を読み取れませんでした"))
}

/// The `dayjs` tokens, in the order `merman` tries them, so that a format is
/// cut into the same pieces here as where it is read.
const TOKENS: [&str; 28] = [
    "YYYY", "MMMM", "MMM", "MM", "M", "Do", "DD", "D", "HH", "H", "hh", "h", "mm", "m", "ss", "s",
    "SSS", "SS", "S", "ZZ", "Z", "A", "a", "x", "X", "dddd", "ddd", "YY",
];

/// Spells the wall clock time `civil` with a `dayjs` format.
pub(super) fn format_civil(civil: i64, format: &str) -> Result<String> {
    let time = Civil::from_millis(civil);
    let mut out = String::new();
    let mut rest = format;
    while let Some(character) = rest.chars().next() {
        if let Some(inner) = rest.strip_prefix('[')
            && let Some(end) = inner.find(']')
        {
            out.push_str(inner.get(..end).unwrap_or_default());
            rest = inner.get(end + 1..).unwrap_or_default();
            continue;
        }
        if let Some(token) = TOKENS.iter().find(|token| rest.starts_with(**token)) {
            out.push_str(&time.token(token)?);
            rest = rest.get(token.len()..).unwrap_or_default();
            continue;
        }
        out.push(character);
        rest = rest.get(character.len_utf8()..).unwrap_or_default();
    }
    Ok(out)
}

struct Civil {
    year: i64,
    month: i64,
    day: i64,
    hour: i64,
    minute: i64,
    second: i64,
    millis: i64,
}

impl Civil {
    fn from_millis(millis: i64) -> Self {
        let (year, month, day) = civil_from_days(millis.div_euclid(DAY));
        let rest = millis.rem_euclid(DAY);
        Self {
            year,
            month,
            day,
            hour: rest / HOUR,
            minute: rest / MINUTE % 60,
            second: rest / SECOND % 60,
            millis: rest % SECOND,
        }
    }

    fn token(&self, token: &str) -> Result<String> {
        Ok(match token {
            "YYYY" => {
                if !(0..=9999).contains(&self.year) {
                    bail!("{} 年は 4 桁で書けません", self.year);
                }
                format!("{:04}", self.year)
            }
            // `merman` reads two digits up to 68 as 20xx and the rest as 19xx.
            "YY" => {
                if !(1969..=2068).contains(&self.year) {
                    bail!("{} 年は 2 桁で書けません", self.year);
                }
                format!("{:02}", self.year % 100)
            }
            "MM" => format!("{:02}", self.month),
            "M" => self.month.to_string(),
            "DD" => format!("{:02}", self.day),
            "D" => self.day.to_string(),
            "HH" => format!("{:02}", self.hour),
            "H" => self.hour.to_string(),
            "mm" => format!("{:02}", self.minute),
            "m" => self.minute.to_string(),
            "ss" => format!("{:02}", self.second),
            "s" => self.second.to_string(),
            "SSS" => format!("{:03}", self.millis),
            "SS" if self.millis % 10 == 0 => format!("{:02}", self.millis / 10),
            "S" if self.millis % 100 == 0 => (self.millis / 100).to_string(),
            "SS" | "S" => bail!("`{token}` ではミリ秒を書き切れません"),
            other => bail!("`dateFormat` の `{other}` には日付を書き込めません"),
        })
    }
}

/// The proleptic Gregorian date `days` after 1970-01-01.
fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let shifted = days + 719_468;
    let era = shifted.div_euclid(146_097);
    let day_of_era = shifted - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_index = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_index + 2) / 5 + 1;
    let month = if month_index < 10 {
        month_index + 3
    } else {
        month_index - 9
    };
    let year = year_of_era + era * 400 + i64::from(month <= 2);
    (year, month, day)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn civil(year: i64, month: i64, day: i64) -> i64 {
        // Counted forwards from the epoch so that the test does not lean on
        // the conversion it is testing.
        let mut days = 0;
        for y in 1970..year {
            days += if (y % 4 == 0 && y % 100 != 0) || y % 400 == 0 {
                366
            } else {
                365
            };
        }
        let leap = (year % 4 == 0 && year % 100 != 0) || year % 400 == 0;
        let lengths = [
            31,
            if leap { 29 } else { 28 },
            31,
            30,
            31,
            30,
            31,
            31,
            30,
            31,
            30,
            31,
        ];
        for length in lengths.iter().take((month - 1) as usize) {
            days += length;
        }
        (days + day - 1) * DAY
    }

    #[test]
    fn a_wall_clock_time_is_spelled_with_the_numeric_tokens() {
        let moment = civil(2026, 1, 6) + 9 * HOUR + 5 * MINUTE + 7 * SECOND + 30;
        for (format, expected) in [
            ("YYYY-MM-DD", "2026-01-06"),
            ("YYYY-MM-DD HH:mm", "2026-01-06 09:05"),
            ("DD-MM-YYYY", "06-01-2026"),
            ("D/M/YY", "6/1/26"),
            ("YYYY-MM-DD HH:mm:ss.SSS", "2026-01-06 09:05:07.030"),
            ("SS", "03"),
            ("[Day] D", "Day 6"),
            ("YYYY年M月D日", "2026年1月6日"),
        ] {
            assert_eq!(
                format_civil(moment, format).expect("spellable"),
                expected,
                "{format}"
            );
        }
        for format in ["MMM D", "Do", "hh:mm A", "YYYY-MM-DDZ", "S", "x", "dddd"] {
            assert!(
                format_civil(moment, format).is_err(),
                "`{format}` should not be guessed at"
            );
        }
        assert_eq!(
            format_civil(civil(2024, 2, 29), "YYYY-MM-DD").expect("spellable"),
            "2024-02-29"
        );
        assert_eq!(
            format_civil(-DAY, "YYYY-MM-DD").expect("spellable"),
            "1969-12-31"
        );
    }

    /// `merman` reads a date as local time, so reading one back through it
    /// has to land on the same wall clock time wherever the tests run.
    #[test]
    fn the_local_time_of_a_moment_is_the_one_merman_reads_back() {
        let times =
            task_times("gantt\n    dateFormat YYYY-MM-DD HH:mm\n    A :a, 2026-01-06 09:30, 1d")
                .expect("the chart should resolve");
        let TaskTime { start, end, .. } = times.first().copied().expect("one task");
        assert_eq!(
            format_civil(
                local_civil(start).expect("a local time"),
                "YYYY-MM-DD HH:mm"
            )
            .expect("spellable"),
            "2026-01-06 09:30"
        );
        assert_eq!(
            start_text(Some("YYYY-MM-DD HH:mm"), start).expect("spellable"),
            "2026-01-06 09:30"
        );
        assert_eq!(
            end_text(Some("YYYY-MM-DD HH:mm"), false, end, start).expect("spellable"),
            "2026-01-07 09:30"
        );
        assert_eq!(
            end_text(Some("YYYY-MM-DD HH:mm"), true, end, start).expect("spellable"),
            "2026-01-06 09:30"
        );
        // A format that cannot spell the moment falls back to a length.
        assert_eq!(
            end_text(Some("MMM D"), false, end, start).expect("a length"),
            "1d"
        );
        assert_eq!(end_text(None, false, end, start).expect("a length"), "1d");
        assert!(start_text(Some("MMM D"), start).is_err());

        assert_eq!(
            start_text(Some("x"), 1_700_000_000_123).expect("x"),
            "1700000000123"
        );
        assert_eq!(
            start_text(Some("X"), 1_700_000_000_123).expect("X"),
            "1700000000123"
        );
        assert_eq!(
            end_text(Some("X"), false, 1_700_000_000_000, 0).expect("X"),
            "1700000000"
        );
        assert_eq!(duration_text(90 * MINUTE).expect("a length"), "90m");
        assert_eq!(duration_text(1_500).expect("a length"), "1500ms");
        assert!(duration_text(-1).is_err());
    }
}
