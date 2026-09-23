//! Best-effort provider hints; local configured windows always remain enforced.
use crate::db::free_router_store::{Feedback, RemoteBudget};
use axum::http::{header, HeaderMap, StatusCode};

fn value<'a>(h: &'a HeaderMap, names: &[&str]) -> Option<&'a str> {
    names.iter().find_map(|name| h.get(*name)?.to_str().ok())
}

/// Retry-After: seconds or HTTP-date. Reset headers additionally use Unix
/// timestamps, RFC3339, or compound durations such as Groq's "1m2.5s".
fn reset(value: &str, now: i64, timestamp: bool) -> Option<i64> {
    let value = value.trim();
    if value.is_empty() {
        return None;
    }
    if let Ok(n) = value.parse::<f64>() {
        if !n.is_finite() || n < 0.0 || n > 1e15 {
            return None;
        }
        let ms = if timestamp && n >= 1e12 {
            n
        } else if timestamp && n >= 1e9 {
            n * 1000.0
        } else {
            now as f64 + n * 1000.0
        };
        return Some((ms.ceil() as i64).max(now + 1000));
    }
    if let Ok(date) = chrono::DateTime::parse_from_rfc2822(value) {
        return Some(date.timestamp_millis().max(now + 1000));
    }
    if let Ok(date) = chrono::DateTime::parse_from_rfc3339(value) {
        return Some(date.timestamp_millis().max(now + 1000));
    }
    let mut rest = value;
    let mut ms = 0.0;
    while !rest.is_empty() {
        let end = rest.find(|c: char| !c.is_ascii_digit() && c != '.')?;
        let n: f64 = rest[..end].parse().ok()?;
        rest = &rest[end..];
        let (unit, multiplier) = [
            ("ms", 1.0),
            ("s", 1000.0),
            ("m", 60_000.0),
            ("h", 3_600_000.0),
            ("d", 86_400_000.0),
        ]
        .into_iter()
        .find(|(u, _)| rest.starts_with(u))?;
        rest = &rest[unit.len()..];
        ms += n * multiplier;
    }
    (ms.is_finite() && ms <= 31.0 * 86_400_000.0).then(|| now + (ms.ceil() as i64).max(1000))
}

pub fn feedback(status: StatusCode, h: &HeaderMap, now: i64) -> Feedback {
    let mut out = Feedback::default();
    let budget = |remaining: &[&str], resets: &[&str]| -> Option<RemoteBudget> {
        let remaining = value(h, remaining)?.parse().ok()?;
        let reset_ms = value(h, resets)
            .and_then(|v| reset(v, now, true))
            .unwrap_or(now + 60_000);
        Some(RemoteBudget {
            remaining,
            reset_ms,
        })
    };
    out.requests = budget(
        &[
            "x-ratelimit-remaining-requests",
            "ratelimit-remaining",
            "x-ratelimit-remaining",
        ],
        &[
            "x-ratelimit-reset-requests",
            "ratelimit-reset",
            "x-ratelimit-reset",
        ],
    );
    out.tokens = budget(
        &["x-ratelimit-remaining-tokens"],
        &["x-ratelimit-reset-tokens"],
    );
    if status == StatusCode::TOO_MANY_REQUESTS || status == StatusCode::SERVICE_UNAVAILABLE {
        let hint = h
            .get(header::RETRY_AFTER)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| reset(v, now, false));
        // A 429 without Retry-After may be a daily quota. Honor the longer
        // supplied reset rather than hammering the provider once a minute.
        let reset_hint = [&out.requests, &out.tokens]
            .into_iter()
            .flatten()
            .map(|b| b.reset_ms)
            .max();
        out.cooldown_until = hint.or(reset_hint).unwrap_or(now + 60_000);
        out.reason = "rate limited / retry-after";
    } else if matches!(status.as_u16(), 401..=404) {
        out.cooldown_until = now + 300_000;
        out.reason = "credentials, credit or model unavailable";
    } else if status.is_server_error() {
        out.cooldown_until = now + 30_000;
        out.reason = "provider unavailable";
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn reset_formats_and_retry_after() {
        let now = 1_700_000_000_000;
        assert_eq!(reset("1m2.5s", now, true), Some(now + 62_500));
        assert_eq!(reset("250ms", now, true), Some(now + 1000));
        assert_eq!(reset("1700000100", now, true), Some(now + 100_000));
        assert_eq!(
            reset("Tue, 14 Nov 2023 22:15:00 GMT", now, false),
            Some(now + 100_000)
        );
        assert_eq!(reset("NaN", now, true), None);
        assert_eq!(reset("bad", now, true), None);
        let mut h = HeaderMap::new();
        h.insert("retry-after", "120".parse().unwrap());
        assert_eq!(
            feedback(StatusCode::TOO_MANY_REQUESTS, &h, now).cooldown_until,
            now + 120_000
        );
    }
}
