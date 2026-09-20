//! Zeitfenster-Parser für `schedule = "Mo-Fr 09:00-18:00"`.
//!
//! Formate: `HH:MM-HH:MM`, `Mo-Fr HH:MM-HH:MM`, `Sa,Su 10:00-14:00`,
//! `Mo-So 09:00-18:00`. Wochentage: Mo,Di,Mi,Do,Fr,Sa,So (auch lang).

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Window {
    /// 1 = Mo … 7 = So.
    pub days: Vec<u8>,
    pub start_min: u32,
    pub end_min: u32,
}

impl Window {
    /// Aktiv an Tag `weekday` (1=Mo) zur Minute `minute_of_day`?
    /// Fenster über Mitternacht (z. B. 20:00-02:00) wird unterstützt.
    pub fn contains(&self, weekday: u8, minute_of_day: u32) -> bool {
        if !self.days.contains(&weekday) {
            return false;
        }
        if self.start_min <= self.end_min {
            minute_of_day >= self.start_min && minute_of_day < self.end_min
        } else {
            minute_of_day >= self.start_min || minute_of_day < self.end_min
        }
    }

    /// Minute, ab der Prewarm gestartet werden soll (start - prewarm_min),
    /// NUR an einem Fenstertag — sonst hätte z. B. ein So-12:00-Poll den
    /// Prewarm-Arm belegt und den "Fenster zu → Stop" verschluckt (der
    /// Sunday-Stop kam bislang nur versehentlich über den Idle-Pfad).
    pub fn prewarm_start(&self, prewarm_min: u32, weekday: u8) -> Option<u32> {
        if prewarm_min == 0 || !self.days.contains(&weekday) {
            return None;
        }
        self.start_min.checked_sub(prewarm_min)
    }
}

fn day_from(s: &str) -> Option<u8> {
    let s = s.trim().to_lowercase();
    let s: String = s.chars().filter(|c| c.is_ascii_alphabetic()).collect();
    Some(match s.as_str() {
        "mo" | "mon" | "montag" | "monday" => 1,
        "di" | "tu" | "tue" | "tues" | "dienstag" | "tuesday" => 2,
        "mi" | "we" | "wed" | "mittwoch" | "wednesday" => 3,
        "do" | "th" | "thu" | "thurs" | "donnerstag" | "thursday" => 4,
        "fr" | "friday" | "freitag" | "fri" => 5,
        "sa" | "sat" | "samstag" | "saturday" | "satterday" => 6,
        "so" | "su" | "sun" | "sonntag" | "sunday" => 7,
        _ => return None,
    })
}

fn parse_hhmm(s: &str) -> Option<u32> {
    let (h, m) = s.trim().split_once(':')?;
    let h: u32 = h.trim().parse().ok()?;
    let m: u32 = m.trim().parse().ok()?;
    if h == 24 && m == 0 {
        Some(1440)
    } else if h < 24 && m < 60 {
        Some(h * 60 + m)
    } else {
        None
    }
}

fn parse_days(s: &str) -> Option<Vec<u8>> {
    let mut days = Vec::new();
    for part in s.split(',') {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }
        if let Some((a, b)) = part.split_once('-') {
            let (a, b) = (day_from(a)?, day_from(b)?);
            let (lo, hi) = if a <= b { (a, b) } else { (b, a) };
            days.extend(lo..=hi);
        } else {
            days.push(day_from(part)?);
        }
    }
    days.sort_unstable();
    days.dedup();
    if days.is_empty() {
        None
    } else {
        Some(days)
    }
}

/// `"Mo-Fr 09:00-18:00"` → Window. Ohne Tage: alle Tage.
pub fn parse_window(spec: &str) -> Option<Window> {
    let spec = spec.trim();
    let (days, time) = match spec.split_once(' ') {
        Some((d, t)) => (parse_days(d)?, t),
        None => (vec![1, 2, 3, 4, 5, 6, 7], spec),
    };
    let (start, end) = time.split_once('-')?;
    Some(Window { days, start_min: parse_hhmm(start)?, end_min: parse_hhmm(end)? })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_range() {
        let w = parse_window("Mo-Fr 09:00-18:00").unwrap();
        assert_eq!(w.days, vec![1, 2, 3, 4, 5]);
        assert_eq!(w.start_min, 540);
        assert_eq!(w.end_min, 1080);
    }

    #[test]
    fn parses_list() {
        let w = parse_window("Sa,So 10:00-14:00").unwrap();
        assert_eq!(w.days, vec![6, 7]);
    }

    #[test]
    fn everyday() {
        let w = parse_window("00:00-24:00").unwrap();
        assert!(w.contains(3, 0));
        assert!(!w.contains(3, 24 * 60 + 1));
        let w2 = parse_window("Mo-So 00:00-23:59").unwrap();
        assert!(w2.contains(7, 23 * 60 + 58));
    }

    #[test]
    fn midnight_span() {
        let w = parse_window("20:00-02:00").unwrap();
        assert!(w.contains(2, 21 * 60));
        assert!(w.contains(2, 1 * 60));
        assert!(!w.contains(2, 10 * 60));
    }

    #[test]
    fn prewarm() {
        let w = parse_window("Mo-Fr 09:00-18:00").unwrap();
        assert_eq!(w.prewarm_start(20, 1), Some(520));
        assert_eq!(w.prewarm_start(0, 1), None);
        assert_eq!(w.prewarm_start(600, 1), None); // würde Vortag betreffen → None
        // Fensterfreier Tag: kein Prewarm — der Stop-Arm muss greifen können.
        assert_eq!(w.prewarm_start(20, 7), None);
        assert_eq!(w.prewarm_start(20, 6), None);
    }

    #[test]
    fn garbage_is_none() {
        assert!(parse_window("mo-xx 9-18").is_none());
        assert!(parse_window("").is_none());
    }
}