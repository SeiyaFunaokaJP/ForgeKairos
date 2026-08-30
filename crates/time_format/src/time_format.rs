use time::{Date, OffsetDateTime};

/// The formatting style for a timestamp.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TimestampFormat {
    /// Formats the timestamp as an absolute time, e.g. "2026年8月28日 15:00".
    Absolute,
    /// Formats the timestamp as an absolute time.
    /// If the message is from today or yesterday the date will be replaced with "今日 x" or "昨日 x" respectively.
    /// E.g. "今日 12:00", "昨日 11:00", "2026年8月28日 15:00".
    EnhancedAbsolute,
    /// Formats the timestamp as an absolute time, using year, month, day. e.g. "2026年8月28日".
    MediumAbsolute,
    /// Formats the timestamp as a relative time, e.g. "たった今", "1 分前", "2 時間前", "2 か月前".
    Relative,
}

/// Formats a timestamp, always producing a fixed, locale-independent Japanese string
/// regardless of the user's OS locale settings.
pub fn format_localized_timestamp(
    timestamp: OffsetDateTime,
    reference: OffsetDateTime,
    timezone: time::UtcOffset,
    format: TimestampFormat,
) -> String {
    let timestamp_local = timestamp.to_offset(timezone);
    let reference_local = reference.to_offset(timezone);
    format_local_timestamp(timestamp_local, reference_local, format)
}

/// Formats a timestamp, always producing a fixed, locale-independent Japanese string
/// regardless of the user's OS locale settings.
pub fn format_local_timestamp(
    timestamp: OffsetDateTime,
    reference: OffsetDateTime,
    format: TimestampFormat,
) -> String {
    match format {
        TimestampFormat::Absolute => format_absolute_timestamp(timestamp, reference, false),
        TimestampFormat::EnhancedAbsolute => format_absolute_timestamp(timestamp, reference, true),
        TimestampFormat::MediumAbsolute => format_absolute_timestamp_medium(timestamp, reference),
        TimestampFormat::Relative => format_relative_time(timestamp, reference)
            .unwrap_or_else(|| format_relative_date(timestamp, reference)),
    }
}

/// Formats the date component of a timestamp
pub fn format_date(
    timestamp: OffsetDateTime,
    reference: OffsetDateTime,
    enhanced_formatting: bool,
) -> String {
    format_absolute_date(timestamp, reference, enhanced_formatting)
}

/// Formats the time component of a timestamp
pub fn format_time(timestamp: OffsetDateTime) -> String {
    format_absolute_time(timestamp)
}

/// Formats the date component of a timestamp in medium style
pub fn format_date_medium(
    timestamp: OffsetDateTime,
    reference: OffsetDateTime,
    enhanced_formatting: bool,
) -> String {
    format_absolute_date_medium(timestamp, reference, enhanced_formatting)
}

/// Formats a date as a fixed-locale Japanese date, e.g. "2026年8月28日".
fn format_japanese_date(date: Date) -> String {
    format!(
        "{}年{}月{}日",
        date.year(),
        u8::from(date.month()),
        date.day()
    )
}

fn format_absolute_date(
    timestamp: OffsetDateTime,
    reference: OffsetDateTime,
    enhanced_date_formatting: bool,
) -> String {
    if enhanced_date_formatting {
        let timestamp_date = timestamp.date();
        let reference_date = reference.date();
        if timestamp_date == reference_date {
            return "今日".to_string();
        } else if reference_date.previous_day() == Some(timestamp_date) {
            return "昨日".to_string();
        }
    }
    format_japanese_date(timestamp.date())
}

fn format_absolute_time(timestamp: OffsetDateTime) -> String {
    format_timestamp_naive_time(timestamp, false)
}

fn format_absolute_timestamp(
    timestamp: OffsetDateTime,
    reference: OffsetDateTime,
    enhanced_date_formatting: bool,
) -> String {
    if enhanced_date_formatting {
        let timestamp_date = timestamp.date();
        let reference_date = reference.date();
        if timestamp_date == reference_date {
            return format!("今日 {}", format_absolute_time(timestamp));
        } else if reference_date.previous_day() == Some(timestamp_date) {
            return format!("昨日 {}", format_absolute_time(timestamp));
        }
    }
    format!(
        "{} {}",
        format_absolute_date(timestamp, reference, enhanced_date_formatting),
        format_absolute_time(timestamp)
    )
}

fn format_absolute_date_medium(
    timestamp: OffsetDateTime,
    reference: OffsetDateTime,
    enhanced_formatting: bool,
) -> String {
    format_absolute_date(timestamp, reference, enhanced_formatting)
}

fn format_absolute_timestamp_medium(
    timestamp: OffsetDateTime,
    reference: OffsetDateTime,
) -> String {
    format_absolute_date_medium(timestamp, reference, false)
}

fn format_relative_time(timestamp: OffsetDateTime, reference: OffsetDateTime) -> Option<String> {
    let difference = reference - timestamp;
    let minutes = difference.whole_minutes();
    match minutes {
        0 => Some("たった今".to_string()),
        1..=59 => Some(format!("{minutes} 分前")),
        _ => {
            let hours = difference.whole_hours();
            match hours {
                1..=23 => Some(format!("{hours} 時間前")),
                _ => None,
            }
        }
    }
}

fn format_relative_date(timestamp: OffsetDateTime, reference: OffsetDateTime) -> String {
    let timestamp_date = timestamp.date();
    let reference_date = reference.date();
    let difference = reference_date - timestamp_date;
    let days = difference.whole_days();
    match days {
        0 => "今日".to_string(),
        1 => "昨日".to_string(),
        2..=6 => format!("{days} 日前"),
        _ => {
            let weeks = difference.whole_weeks();
            match weeks {
                1..=4 => format!("{weeks} 週間前"),
                _ => {
                    let month_diff = calculate_month_difference(timestamp, reference);
                    match month_diff {
                        0..=1 => "1 か月前".to_string(),
                        2..=11 => format!("{month_diff} か月前"),
                        // Match git's `show_date_relative` behavior: for dates under 5 years old,
                        // include both years and months so, for example, 22 months is shown as
                        // "1 年 10 か月前" instead of being collapsed to "1 年前".
                        12..60 => format_compound_year_month(month_diff),
                        // Beyond 5 years, round to the nearest year.
                        months => {
                            let years = (months + 6) / 12;
                            format!("{years} 年前")
                        }
                    }
                }
            }
        }
    }
}

fn format_compound_year_month(month_diff: usize) -> String {
    let years = month_diff / 12;
    let months = month_diff % 12;
    if months == 0 {
        format!("{years} 年前")
    } else {
        format!("{years} 年 {months} か月前")
    }
}

/// Calculates the difference in months between two timestamps.
/// The reference timestamp should always be greater than the timestamp.
fn calculate_month_difference(timestamp: OffsetDateTime, reference: OffsetDateTime) -> usize {
    let timestamp_year = timestamp.year();
    let reference_year = reference.year();
    let timestamp_month: u8 = timestamp.month().into();
    let reference_month: u8 = reference.month().into();

    let month_diff = if reference_month >= timestamp_month {
        reference_month as usize - timestamp_month as usize
    } else {
        12 - timestamp_month as usize + reference_month as usize
    };

    let year_diff = (reference_year - timestamp_year) as usize;
    if year_diff == 0 {
        reference_month as usize - timestamp_month as usize
    } else if month_diff == 0 {
        year_diff * 12
    } else if timestamp_month > reference_month {
        (year_diff - 1) * 12 + month_diff
    } else {
        year_diff * 12 + month_diff
    }
}

/// Formats a timestamp, which is either in 12-hour or 24-hour time format.
fn format_timestamp_naive_time(timestamp_local: OffsetDateTime, is_12_hour_time: bool) -> String {
    let timestamp_local_hour = timestamp_local.hour();
    let timestamp_local_minute = timestamp_local.minute();

    let (hour, meridiem) = if is_12_hour_time {
        let meridiem = if timestamp_local_hour >= 12 {
            "PM"
        } else {
            "AM"
        };

        let hour_12 = match timestamp_local_hour {
            0 => 12,                              // Midnight
            13..=23 => timestamp_local_hour - 12, // PM hours
            _ => timestamp_local_hour,            // AM hours
        };

        (hour_12, Some(meridiem))
    } else {
        (timestamp_local_hour, None)
    };

    match meridiem {
        Some(meridiem) => format!("{}:{:02} {}", hour, timestamp_local_minute, meridiem),
        None => format!("{:02}:{:02}", hour, timestamp_local_minute),
    }
}

/// Formats a timestamp as a fixed-locale Japanese date and time, e.g. "今日 15:30",
/// "昨日 9:00 AM", or "1990年4月10日 8:20 PM".
pub fn format_timestamp_naive(
    timestamp_local: OffsetDateTime,
    reference_local: OffsetDateTime,
    is_12_hour_time: bool,
) -> String {
    let formatted_time = format_timestamp_naive_time(timestamp_local, is_12_hour_time);
    let reference_local_date = reference_local.date();
    let timestamp_local_date = timestamp_local.date();

    if timestamp_local_date == reference_local_date {
        format!("今日 {}", formatted_time)
    } else if reference_local_date.previous_day() == Some(timestamp_local_date) {
        format!("昨日 {}", formatted_time)
    } else {
        format!(
            "{} {}",
            format_japanese_date(timestamp_local_date),
            formatted_time
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_format_date() {
        let reference = create_offset_datetime(1990, 4, 12, 10, 30, 0);

        // Test with same date (today)
        let timestamp_today = create_offset_datetime(1990, 4, 12, 9, 30, 0);
        assert_eq!(format_date(timestamp_today, reference, true), "今日");

        // Test with previous day (yesterday)
        let timestamp_yesterday = create_offset_datetime(1990, 4, 11, 9, 30, 0);
        assert_eq!(format_date(timestamp_yesterday, reference, true), "昨日");

        // Test with other date
        let timestamp_other = create_offset_datetime(1990, 4, 10, 9, 30, 0);
        let result = format_date(timestamp_other, reference, true);
        assert!(!result.is_empty());
        assert_ne!(result, "今日");
        assert_ne!(result, "昨日");
    }

    #[test]
    fn test_format_time() {
        let timestamp = create_offset_datetime(1990, 4, 12, 9, 30, 0);

        let result = format_time(timestamp);
        assert!(!result.is_empty());
    }

    #[test]
    fn test_format_date_medium() {
        let reference = create_offset_datetime(1990, 4, 12, 10, 30, 0);
        let timestamp = create_offset_datetime(1990, 4, 12, 9, 30, 0);

        // Test with enhanced formatting (today)
        let result_enhanced = format_date_medium(timestamp, reference, true);
        assert_eq!(result_enhanced, "今日");

        // Test with standard formatting
        let result_standard = format_date_medium(timestamp, reference, false);
        assert!(!result_standard.is_empty());

        // Test yesterday with enhanced formatting
        let timestamp_yesterday = create_offset_datetime(1990, 4, 11, 9, 30, 0);
        let result_yesterday = format_date_medium(timestamp_yesterday, reference, true);
        assert_eq!(result_yesterday, "昨日");

        // Test other date with enhanced formatting
        let timestamp_other = create_offset_datetime(1990, 4, 10, 9, 30, 0);
        let result_other = format_date_medium(timestamp_other, reference, true);
        assert!(!result_other.is_empty());
        assert_ne!(result_other, "今日");
        assert_ne!(result_other, "昨日");
    }

    #[test]
    fn test_format_absolute_time() {
        let timestamp = create_offset_datetime(1990, 4, 12, 9, 30, 0);

        let result = format_absolute_time(timestamp);
        assert!(!result.is_empty());
    }

    #[test]
    fn test_format_absolute_date() {
        let reference = create_offset_datetime(1990, 4, 12, 10, 30, 0);

        // Test with same date (today)
        let timestamp_today = create_offset_datetime(1990, 4, 12, 9, 30, 0);
        assert_eq!(
            format_absolute_date(timestamp_today, reference, true),
            "今日"
        );

        // Test with previous day (yesterday)
        let timestamp_yesterday = create_offset_datetime(1990, 4, 11, 9, 30, 0);
        assert_eq!(
            format_absolute_date(timestamp_yesterday, reference, true),
            "昨日"
        );

        // Test with other date
        let timestamp_other = create_offset_datetime(1990, 4, 10, 9, 30, 0);
        let result = format_absolute_date(timestamp_other, reference, true);
        assert!(!result.is_empty());
        assert_ne!(result, "今日");
        assert_ne!(result, "昨日");
    }

    #[test]
    fn test_format_absolute_date_medium() {
        let reference = create_offset_datetime(1990, 4, 12, 10, 30, 0);
        let timestamp = create_offset_datetime(1990, 4, 12, 9, 30, 0);

        // Test with enhanced formatting (today)
        let result_enhanced = format_absolute_date_medium(timestamp, reference, true);
        assert_eq!(result_enhanced, "今日");

        // Test with standard formatting
        let result_standard = format_absolute_date_medium(timestamp, reference, false);
        assert!(!result_standard.is_empty());

        // Test yesterday with enhanced formatting
        let timestamp_yesterday = create_offset_datetime(1990, 4, 11, 9, 30, 0);
        let result_yesterday = format_absolute_date_medium(timestamp_yesterday, reference, true);
        assert_eq!(result_yesterday, "昨日");
    }

    #[test]
    fn test_format_timestamp_naive_time() {
        let timestamp = create_offset_datetime(1990, 4, 12, 9, 30, 0);
        assert_eq!(format_timestamp_naive_time(timestamp, true), "9:30 AM");
        assert_eq!(format_timestamp_naive_time(timestamp, false), "09:30");

        let timestamp_pm = create_offset_datetime(1990, 4, 12, 15, 45, 0);
        assert_eq!(format_timestamp_naive_time(timestamp_pm, true), "3:45 PM");
        assert_eq!(format_timestamp_naive_time(timestamp_pm, false), "15:45");
    }

    #[test]
    fn test_format_24_hour_time() {
        let reference = create_offset_datetime(1990, 4, 12, 16, 45, 0);
        let timestamp = create_offset_datetime(1990, 4, 12, 15, 30, 0);

        assert_eq!(
            format_timestamp_naive(timestamp, reference, false),
            "今日 15:30"
        );
    }

    #[test]
    fn test_format_today() {
        let reference = create_offset_datetime(1990, 4, 12, 16, 45, 0);
        let timestamp = create_offset_datetime(1990, 4, 12, 15, 30, 0);

        assert_eq!(
            format_timestamp_naive(timestamp, reference, true),
            "今日 3:30 PM"
        );
    }

    #[test]
    fn test_format_yesterday() {
        let reference = create_offset_datetime(1990, 4, 12, 10, 30, 0);
        let timestamp = create_offset_datetime(1990, 4, 11, 9, 0, 0);

        assert_eq!(
            format_timestamp_naive(timestamp, reference, true),
            "昨日 9:00 AM"
        );
    }

    #[test]
    fn test_format_yesterday_less_than_24_hours_ago() {
        let reference = create_offset_datetime(1990, 4, 12, 19, 59, 0);
        let timestamp = create_offset_datetime(1990, 4, 11, 20, 0, 0);

        assert_eq!(
            format_timestamp_naive(timestamp, reference, true),
            "昨日 8:00 PM"
        );
    }

    #[test]
    fn test_format_yesterday_more_than_24_hours_ago() {
        let reference = create_offset_datetime(1990, 4, 12, 19, 59, 0);
        let timestamp = create_offset_datetime(1990, 4, 11, 18, 0, 0);

        assert_eq!(
            format_timestamp_naive(timestamp, reference, true),
            "昨日 6:00 PM"
        );
    }

    #[test]
    fn test_format_yesterday_over_midnight() {
        let reference = create_offset_datetime(1990, 4, 12, 0, 5, 0);
        let timestamp = create_offset_datetime(1990, 4, 11, 23, 55, 0);

        assert_eq!(
            format_timestamp_naive(timestamp, reference, true),
            "昨日 11:55 PM"
        );
    }

    #[test]
    fn test_format_yesterday_over_month() {
        let reference = create_offset_datetime(1990, 4, 2, 9, 0, 0);
        let timestamp = create_offset_datetime(1990, 4, 1, 20, 0, 0);

        assert_eq!(
            format_timestamp_naive(timestamp, reference, true),
            "昨日 8:00 PM"
        );
    }

    #[test]
    fn test_format_before_yesterday() {
        let reference = create_offset_datetime(1990, 4, 12, 10, 30, 0);
        let timestamp = create_offset_datetime(1990, 4, 10, 20, 20, 0);

        assert_eq!(
            format_timestamp_naive(timestamp, reference, true),
            "1990年4月10日 8:20 PM"
        );
    }

    #[test]
    fn test_relative_format_minutes() {
        let reference = create_offset_datetime(1990, 4, 12, 23, 0, 0);
        let mut current_timestamp = reference;

        let mut next_minute = || {
            current_timestamp = if current_timestamp.minute() == 0 {
                current_timestamp
                    .replace_hour(current_timestamp.hour() - 1)
                    .unwrap()
                    .replace_minute(59)
                    .unwrap()
            } else {
                current_timestamp
                    .replace_minute(current_timestamp.minute() - 1)
                    .unwrap()
            };
            current_timestamp
        };

        assert_eq!(
            format_relative_time(reference, reference),
            Some("たった今".to_string())
        );

        assert_eq!(
            format_relative_time(next_minute(), reference),
            Some("1 分前".to_string())
        );

        for i in 2..=59 {
            assert_eq!(
                format_relative_time(next_minute(), reference),
                Some(format!("{} 分前", i))
            );
        }

        assert_eq!(
            format_relative_time(next_minute(), reference),
            Some("1 時間前".to_string())
        );
    }

    #[test]
    fn test_relative_format_hours() {
        let reference = create_offset_datetime(1990, 4, 12, 23, 0, 0);
        let mut current_timestamp = reference;

        let mut next_hour = || {
            current_timestamp = if current_timestamp.hour() == 0 {
                let date = current_timestamp.date().previous_day().unwrap();
                current_timestamp.replace_date(date)
            } else {
                current_timestamp
                    .replace_hour(current_timestamp.hour() - 1)
                    .unwrap()
            };
            current_timestamp
        };

        assert_eq!(
            format_relative_time(next_hour(), reference),
            Some("1 時間前".to_string())
        );

        for i in 2..=23 {
            assert_eq!(
                format_relative_time(next_hour(), reference),
                Some(format!("{} 時間前", i))
            );
        }

        assert_eq!(format_relative_time(next_hour(), reference), None);
    }

    #[test]
    fn test_relative_format_days() {
        let reference = create_offset_datetime(1990, 4, 12, 23, 0, 0);
        let mut current_timestamp = reference;

        let mut next_day = || {
            let date = current_timestamp.date().previous_day().unwrap();
            current_timestamp = current_timestamp.replace_date(date);
            current_timestamp
        };

        assert_eq!(
            format_relative_date(reference, reference),
            "今日".to_string()
        );

        assert_eq!(
            format_relative_date(next_day(), reference),
            "昨日".to_string()
        );

        for i in 2..=6 {
            assert_eq!(
                format_relative_date(next_day(), reference),
                format!("{} 日前", i)
            );
        }

        assert_eq!(format_relative_date(next_day(), reference), "1 週間前");
    }

    #[test]
    fn test_relative_format_weeks() {
        let reference = create_offset_datetime(1990, 4, 12, 23, 0, 0);
        let mut current_timestamp = reference;

        let mut next_week = || {
            for _ in 0..7 {
                let date = current_timestamp.date().previous_day().unwrap();
                current_timestamp = current_timestamp.replace_date(date);
            }
            current_timestamp
        };

        assert_eq!(
            format_relative_date(next_week(), reference),
            "1 週間前".to_string()
        );

        for i in 2..=4 {
            assert_eq!(
                format_relative_date(next_week(), reference),
                format!("{} 週間前", i)
            );
        }

        assert_eq!(format_relative_date(next_week(), reference), "1 か月前");
    }

    #[test]
    fn test_relative_format_months() {
        let reference = create_offset_datetime(1990, 4, 12, 23, 0, 0);
        let mut current_timestamp = reference;

        let mut next_month = || {
            if current_timestamp.month() == time::Month::January {
                current_timestamp = current_timestamp
                    .replace_month(time::Month::December)
                    .unwrap()
                    .replace_year(current_timestamp.year() - 1)
                    .unwrap();
            } else {
                current_timestamp = current_timestamp
                    .replace_month(current_timestamp.month().previous())
                    .unwrap();
            }
            current_timestamp
        };

        assert_eq!(
            format_relative_date(next_month(), reference),
            "4 週間前".to_string()
        );

        for i in 2..=11 {
            assert_eq!(
                format_relative_date(next_month(), reference),
                format!("{} か月前", i)
            );
        }

        assert_eq!(format_relative_date(next_month(), reference), "1 年前");
    }

    #[test]
    fn test_relative_format_years() {
        let reference = create_offset_datetime(1990, 4, 12, 23, 0, 0);

        // 12 months (exactly 1 year, no remainder)
        assert_eq!(
            format_relative_date(create_offset_datetime(1989, 4, 12, 23, 0, 0), reference),
            "1 年前"
        );

        // 13 months
        assert_eq!(
            format_relative_date(create_offset_datetime(1989, 3, 12, 23, 0, 0), reference),
            "1 年 1 か月前"
        );

        // 22 months (regression test for issue #57907)
        assert_eq!(
            format_relative_date(create_offset_datetime(1988, 6, 12, 23, 0, 0), reference),
            "1 年 10 か月前"
        );

        // 23 months
        assert_eq!(
            format_relative_date(create_offset_datetime(1988, 5, 12, 23, 0, 0), reference),
            "1 年 11 か月前"
        );

        // 24 months (exactly 2 years, no remainder)
        assert_eq!(
            format_relative_date(create_offset_datetime(1988, 4, 12, 23, 0, 0), reference),
            "2 年前"
        );

        // 25 months
        assert_eq!(
            format_relative_date(create_offset_datetime(1988, 3, 12, 23, 0, 0), reference),
            "2 年 1 か月前"
        );

        // 35 months
        assert_eq!(
            format_relative_date(create_offset_datetime(1987, 5, 12, 23, 0, 0), reference),
            "2 年 11 か月前"
        );

        // 36 months (exactly 3 years, no remainder)
        assert_eq!(
            format_relative_date(create_offset_datetime(1987, 4, 12, 23, 0, 0), reference),
            "3 年前"
        );

        // 37 months
        assert_eq!(
            format_relative_date(create_offset_datetime(1987, 3, 12, 23, 0, 0), reference),
            "3 年 1 か月前"
        );

        // 59 months (just under 5-year compound cutoff)
        assert_eq!(
            format_relative_date(create_offset_datetime(1985, 5, 12, 23, 0, 0), reference),
            "4 年 11 か月前"
        );

        // 60 months (5 years exactly; switches to year-only)
        assert_eq!(
            format_relative_date(create_offset_datetime(1985, 4, 12, 23, 0, 0), reference),
            "5 年前"
        );

        // 65 months (5 years + 5 months → rounds down to 5 years)
        assert_eq!(
            format_relative_date(create_offset_datetime(1984, 11, 12, 23, 0, 0), reference),
            "5 年前"
        );

        // 66 months (5 years + 6 months → rounds up to 6 years)
        assert_eq!(
            format_relative_date(create_offset_datetime(1984, 10, 12, 23, 0, 0), reference),
            "6 年前"
        );

        // 120 months
        assert_eq!(
            format_relative_date(create_offset_datetime(1980, 4, 12, 23, 0, 0), reference),
            "10 年前"
        );
    }

    #[test]
    fn test_calculate_month_difference() {
        let reference = create_offset_datetime(1990, 4, 12, 23, 0, 0);

        assert_eq!(calculate_month_difference(reference, reference), 0);

        assert_eq!(
            calculate_month_difference(create_offset_datetime(1990, 1, 12, 23, 0, 0), reference),
            3
        );

        assert_eq!(
            calculate_month_difference(create_offset_datetime(1989, 11, 12, 23, 0, 0), reference),
            5
        );

        assert_eq!(
            calculate_month_difference(create_offset_datetime(1989, 4, 12, 23, 0, 0), reference),
            12
        );

        assert_eq!(
            calculate_month_difference(create_offset_datetime(1989, 3, 12, 23, 0, 0), reference),
            13
        );

        assert_eq!(
            calculate_month_difference(create_offset_datetime(1987, 5, 12, 23, 0, 0), reference),
            35
        );

        assert_eq!(
            calculate_month_difference(create_offset_datetime(1987, 4, 12, 23, 0, 0), reference),
            36
        );

        assert_eq!(
            calculate_month_difference(create_offset_datetime(1987, 3, 12, 23, 0, 0), reference),
            37
        );
    }

    fn create_offset_datetime(
        year: i32,
        month: u8,
        day: u8,
        hour: u8,
        minute: u8,
        second: u8,
    ) -> OffsetDateTime {
        let date = time::Date::from_calendar_date(year, time::Month::try_from(month).unwrap(), day)
            .unwrap();
        let time = time::Time::from_hms(hour, minute, second).unwrap();
        let date = date.with_time(time).assume_utc(); // Assume UTC for simplicity
        date
    }
}
