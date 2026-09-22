//! Convert a daily wall-clock resume into a persisted-stop-relative UTC deadline.
use chrono::{DateTime, Duration, LocalResult, NaiveTime, TimeZone, Utc};
use chrono_tz::Tz;

pub fn next_resume(stopped: DateTime<Utc>, at: &str, tz: Tz) -> Option<DateTime<Utc>> {
    let time = NaiveTime::parse_from_str(at, "%H:%M").ok()?;
    let date = stopped.with_timezone(&tz).date_naive();
    for day in 0..3 {
        let local = date.checked_add_signed(Duration::days(day))?.and_time(time);
        // A missing DST wall time means the first valid time after the gap.
        // A fold uses the first occurrence strictly after this stop.
        for minute in 0..=180 {
            let candidate = local.checked_add_signed(Duration::minutes(minute))?;
            let times = match tz.from_local_datetime(&candidate) {
                LocalResult::None => continue,
                LocalResult::Single(t) => vec![t.with_timezone(&Utc)],
                LocalResult::Ambiguous(a, b) => vec![a.with_timezone(&Utc), b.with_timezone(&Utc)],
            };
            if let Some(next) = times.into_iter().filter(|t| *t > stopped).min() { return Some(next); }
            break; // Today's occurrence has passed; don't resume a minute later.
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    fn at(s: &str) -> DateTime<Utc> { DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc) }
    #[test]
    fn daily_resume_uses_local_date_and_dst() {
        let tz = chrono_tz::Europe::Berlin;
        assert_eq!(next_resume(at("2026-09-21T20:00:00Z"), "07:30", tz), Some(at("2026-09-22T05:30:00Z")));
        assert_eq!(next_resume(at("2026-03-29T00:00:00Z"), "02:30", tz), Some(at("2026-03-29T01:00:00Z")));
        assert_eq!(next_resume(at("2026-10-25T00:45:00Z"), "02:30", tz), Some(at("2026-10-25T01:30:00Z")));
        assert_eq!(next_resume(at("2026-09-21T05:30:00Z"), "07:30", tz), Some(at("2026-09-22T05:30:00Z")));
        assert!(next_resume(at("2026-09-21T00:00:00Z"), "25:00", tz).is_none());
    }
}
