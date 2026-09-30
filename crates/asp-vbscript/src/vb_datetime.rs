//! VBScript-flavoured date parsing and rendering.
//!
//! Classic ASP pages lean on implicit date strings ("9/30/2026",
//! "2026-09-30 14:00", "2:30 PM"). Supporting every locale format is
//! explicitly out of scope; this module implements a documented,
//! invariant subset: ISO `YYYY-M-D`, the US-style `M/D/YYYY` (also
//! with `-` separators), an optional `HH:MM[:SS]` part with 12-hour
//! `AM`/`PM`, two-digit years (0–29 → 20xx, 30–99 → 19xx), and
//! time-only strings. Rendering is always ISO (`YYYY-MM-DD` dates,
//! `YYYY-MM-DD HH:MM:SS` datetimes), independent of host locale.

use chrono::{Duration, NaiveDate, NaiveDateTime, NaiveTime};

/// VBScript's zero date (`Dec 30, 1899`), used for numeric conversion.
const VB_EPOCH: NaiveDate = match NaiveDate::from_ymd_opt(1899, 12, 30) {
    Some(d) => d,
    None => unreachable!(),
};

/// Parse a date/time string the way the parser-free subset of VBScript
/// commonly does. Returns `None` when the text cannot be understood.
pub fn parse(s: &str) -> Option<NaiveDateTime> {
    let trimmed = s.trim();
    if trimmed.is_empty() {
        return None;
    }
    let (date_part, time_part) = split_datetime(trimmed);
    let date: NaiveDate = match date_part {
        Some(text) => parse_date(&text)?,
        None => chrono::Local::now().date_naive(),
    };
    let time = match time_part {
        Some(text) => parse_time(&text)?,
        None => NaiveTime::MIN,
    };
    Some(date.and_time(time))
}

/// Split `date time` into its parts: exactly one space (or `T`) with a
/// time-looking remainder, or a time-only string overall.
fn split_datetime(s: &str) -> (Option<String>, Option<String>) {
    let looks_like_time = |text: &str| text.contains(':') || text.contains(' ') && text.len() > 4;
    for sep in ['T', ' '] {
        if let Some((a, b)) = s.split_once(sep)
            && !a.is_empty()
            && looks_like_time(b)
        {
            return (Some(a.to_string()), Some(b.trim().to_string()));
        }
    }
    // No separator: either a date, or a bare time.
    if contains_colon(s) {
        (None, Some(s.to_string()))
    } else {
        (Some(s.to_string()), None)
    }
}

fn contains_colon(s: &str) -> bool {
    s.contains(':')
}

/// Parse the date half: `YYYY-M-D` or `M/D/YYYY` (either separator),
/// with two-digit years mapped per the 0–29 / 30–99 windowing rule.
fn parse_date(text: &str) -> Option<NaiveDate> {
    let text = text.trim();
    let (first, second, third) = match text.split_once('-') {
        Some((a, rest)) => match rest.split_once('-') {
            Some((b, c)) => (a, b, c),
            None => (a, rest, ""),
        },
        None => match text.split_once('/') {
            Some((a, rest)) => match rest.split_once('/') {
                Some((b, c)) => (a, b, c),
                None => return None,
            },
            None => return None,
        },
    };
    if third.is_empty() {
        return None;
    }
    let a: i32 = first.trim().parse().ok()?;
    let b: u32 = second.trim().parse().ok()?;
    let c: i32 = third.trim().parse().ok()?;
    // ISO form: the first field is the year (4+ digits).
    if first.trim().len() >= 4 {
        NaiveDate::from_ymd_opt(a, b, c.try_into().ok()?)
    } else {
        // US form M/D/YYYY with two-digit-year windowing.
        let year = expand_year(c);
        let month: u32 = a as u32;
        let day: u32 = b;
        NaiveDate::from_ymd_opt(year, month, day)
    }
}

/// VBScript maps two-digit years 0–29 to 2000–2029 and 30–99 to 1930–1999.
fn expand_year(y: i32) -> i32 {
    if (0..=29).contains(&y) {
        2000 + y
    } else if (30..=99).contains(&y) {
        1900 + y
    } else {
        y
    }
}

/// Parse the time half: `HH:MM[:SS]`, optional ` AM`/` PM`. An explicit
/// meridiem folds `12 AM`→0 and `12 PM`→12; a bare `12:00` is 24-hour
/// noon.
fn parse_time(text: &str) -> Option<NaiveTime> {
    let text = text.trim().to_ascii_lowercase();
    let (meridiem, body) = if let Some(rest) = text.strip_suffix(" am") {
        (Some(0u32), rest.trim().to_string())
    } else if let Some(rest) = text.strip_suffix(" pm") {
        (Some(12u32), rest.trim().to_string())
    } else {
        (None, text.clone())
    };
    let mut fields = body.split(':');
    let hour: u32 = fields.next()?.trim().parse().ok()?;
    let minute: u32 = fields.next()?.trim().parse().ok()?;
    let second: u32 = match fields.next() {
        Some(v) => v.trim().parse().ok()?,
        None => 0,
    };
    if fields.next().is_some() {
        return None;
    }
    let hour = match meridiem {
        None => hour,
        Some(sfx) => (hour % 12) + sfx,
    };
    NaiveTime::from_hms_opt(hour, minute, second)
}

/// Render a datetime the invariant way: ISO date, and the time only
/// when it is not exactly midnight.
pub fn render(d: NaiveDateTime) -> String {
    if d.time() == NaiveTime::MIN {
        d.format("%Y-%m-%d").to_string()
    } else {
        d.format("%Y-%m-%d %H:%M:%S").to_string()
    }
}

/// Numeric value of a date (days since VBScript's epoch, fractional).
pub fn to_number(d: NaiveDateTime) -> f64 {
    let days = (d.date() - VB_EPOCH).num_days() as f64;
    let midnight = d.date().and_time(NaiveTime::MIN);
    let secs = (d - midnight).num_seconds() as f64;
    days + secs / 86_400.0
}

/// `DateSerial(y, m, d)` with VBScript's month/day overflow arithmetic.
pub fn serial_date(y: i32, m: i32, d: i32) -> Option<NaiveDate> {
    // Normalise the month into 1..12, carrying into the year.
    let total = (m - 1).rem_euclid(12) + 1;
    let years = (m - 1).div_euclid(12);
    let year = y.checked_add(years)?;
    let first = NaiveDate::from_ymd_opt(year, total as u32, 1)?;
    first.checked_add_signed(Duration::days(i64::from(d) - 1))
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{Datelike, Timelike};

    fn dt(text: &str) -> NaiveDateTime {
        parse(text).unwrap()
    }

    fn date_of(text: &str) -> (i32, u32, u32) {
        let d = dt(text).date();
        (d.year(), d.month(), d.day())
    }

    #[test]
    fn iso_and_us_dates() {
        assert_eq!(date_of("2026-09-30"), (2026, 9, 30));
        assert_eq!(date_of("9/30/2026"), (2026, 9, 30));
        assert_eq!(date_of("9-30-2026"), (2026, 9, 30));
        assert_eq!(date_of("2/1/27"), (2027, 2, 1));
        assert_eq!(date_of("3/15/85"), (1985, 3, 15));
    }

    #[test]
    fn times_and_ampm() {
        let d = dt("2026-09-30 2:30 PM");
        assert_eq!((d.hour(), d.minute()), (14, 30));
        let d = dt("2026-09-30 12:00 AM");
        assert_eq!(d.hour(), 0);
        let d = dt("14:30:05");
        assert_eq!((d.hour(), d.second()), (14, 5));
    }

    #[test]
    fn rejects_garbage() {
        assert!(parse("hello").is_none());
        assert!(parse("13/40/2026").is_none());
        assert!(parse("2026-02-30").is_none());
        assert!(parse("").is_none());
    }

    #[test]
    fn renders_invariantly() {
        assert_eq!(render(dt("9/30/2026")), "2026-09-30");
        assert_eq!(render(dt("2026-9-30 15:04:05")), "2026-09-30 15:04:05");
    }

    #[test]
    fn serial_overflow_arithmetic() {
        let d = serial_date(2000, 13, 1).unwrap();
        assert_eq!((d.year(), d.month(), d.day()), (2001, 1, 1));
        let d = serial_date(2026, 1, 0).unwrap();
        assert_eq!((d.year(), d.month(), d.day()), (2025, 12, 31));
        let d = serial_date(2001, 2, 29).unwrap();
        assert_eq!((d.year(), d.month(), d.day()), (2001, 3, 1));
    }

    #[test]
    fn numeric_value_matches() {
        let d = dt("1899-12-30");
        assert!((to_number(d) - 0.0).abs() < 1e-9);
        let d = dt("1899-12-31 12:00:00");
        assert!((to_number(d) - 1.5).abs() < 1e-9);
    }
}
