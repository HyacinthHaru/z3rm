use std::time::Duration;

pub fn duration_alt_display(duration: Duration) -> String {
    let hours = duration.as_secs() / 3600;
    let minutes = (duration.as_secs() % 3600) / 60;
    let seconds = duration.as_secs() % 60;

    if hours > 0 {
        format!("{hours}h {minutes}m {seconds}s")
    } else if minutes > 0 {
        format!("{minutes}m {seconds}s")
    } else {
        format!("{seconds}s")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_duration_alt_display() {
        use duration_alt_display as f;
        assert_eq!("0s", f(Duration::from_secs(0)));
        assert_eq!("59s", f(Duration::from_secs(59)));
        assert_eq!("1m 0s", f(Duration::from_secs(60)));
        assert_eq!("10m 0s", f(Duration::from_secs(600)));
        assert_eq!("1h 0m 0s", f(Duration::from_secs(3600)));
        assert_eq!("3h 2m 1s", f(Duration::from_secs(3600 * 3 + 60 * 2 + 1)));
        assert_eq!("23h 59m 59s", f(Duration::from_secs(3600 * 24 - 1)));
        assert_eq!("100h 0m 0s", f(Duration::from_secs(3600 * 100)));
    }
}

use time::{OffsetDateTime, UtcOffset};

/// Simple replacement for time_format::format_localized_timestamp
pub fn format_localized_timestamp(
    dt: OffsetDateTime,
    now: OffsetDateTime,
    _local_offset: Option<UtcOffset>,
    format: TimestampFormat,
) -> String {
    match format {
        TimestampFormat::Relative => relative_time(&dt, &now),
        TimestampFormat::MediumAbsolute => dt
            .format(&time::format_description::well_known::Rfc3339)
            .unwrap_or_else(|_| dt.to_string()),
        TimestampFormat::EnhancedAbsolute => {
            let day = dt.day();
            let month_num = match dt.month() {
                time::Month::January => 1,
                time::Month::February => 2,
                time::Month::March => 3,
                time::Month::April => 4,
                time::Month::May => 5,
                time::Month::June => 6,
                time::Month::July => 7,
                time::Month::August => 8,
                time::Month::September => 9,
                time::Month::October => 10,
                time::Month::November => 11,
                time::Month::December => 12,
            };
            let month = month_name(month_num);
            let year = dt.year();
            let hour = dt.hour();
            let minute = dt.minute();
            format!("{month} {day}, {year} {hour:02}:{minute:02}")
        }
    }
}

fn relative_time(dt: &OffsetDateTime, now: &OffsetDateTime) -> String {
    let diff = *now - *dt;
    let secs = diff.whole_seconds();
    if secs < 0 {
        return "in the future".to_string();
    }
    if secs < 60 {
        return format!("{secs}s ago");
    }
    let mins = secs / 60;
    if mins < 60 {
        return format!("{mins}m ago");
    }
    let hours = mins / 60;
    if hours < 24 {
        return format!("{hours}h ago");
    }
    let days = hours / 24;
    if days < 30 {
        return format!("{days}d ago");
    }
    let months = days / 30;
    if months < 12 {
        return format!("{months}mo ago");
    }
    format!("{}y ago", months / 12)
}

fn month_name(month: u8) -> &'static str {
    match month {
        1 => "Jan",
        2 => "Feb",
        3 => "Mar",
        4 => "Apr",
        5 => "May",
        6 => "Jun",
        7 => "Jul",
        8 => "Aug",
        9 => "Sep",
        10 => "Oct",
        11 => "Nov",
        12 => "Dec",
        _ => "???",
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TimestampFormat {
    Relative,
    MediumAbsolute,
    EnhancedAbsolute,
}
