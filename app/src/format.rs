//! Number formatting for the window.

/// 1024-based sizes with one decimal, e.g. "3.4 MB".
pub fn bytes(n: u64) -> String {
    const UNITS: [&str; 4] = ["KB", "MB", "GB", "TB"];
    if n < 1024 {
        return format!("{n} B");
    }
    let mut v = n as f64 / 1024.0;
    let mut unit = 0;
    while v >= 1024.0 && unit < UNITS.len() - 1 {
        v /= 1024.0;
        unit += 1;
    }
    format!("{v:.1} {}", UNITS[unit])
}

pub fn speed(bytes_per_sec: f64) -> String {
    if bytes_per_sec.is_finite() && bytes_per_sec >= 1.0 {
        format!("{}/s", bytes(bytes_per_sec as u64))
    } else {
        "–".into()
    }
}

/// "less than a minute left", "about 5 min left", "about 1 h 20 min left", "about 2 h left".
pub fn time_left(secs: f64) -> String {
    if !secs.is_finite() || secs < 0.0 {
        return "time left unknown".into();
    }
    if secs < 60.0 {
        return "less than a minute left".into();
    }
    let mins = (secs / 60.0).ceil() as u64;
    match (mins / 60, mins % 60) {
        _ if mins <= 60 => format!("about {mins} min left"),
        (h, 0) => format!("about {h} h left"),
        (h, m) => format!("about {h} h {m} min left"),
    }
}

/// Thousands separators, e.g. "12,345".
pub fn count(n: usize) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// "1 file", "2 files", "1,234 files".
pub fn files(n: usize) -> String {
    if n == 1 {
        "1 file".into()
    } else {
        format!("{} files", count(n))
    }
}

/// `one` for 1, `many` otherwise: "is" or "are", "it" or "they".
pub fn pick<'a>(n: usize, one: &'a str, many: &'a str) -> &'a str {
    if n == 1 { one } else { many }
}

/// "1 item", "2 items", "1,234 items".
pub fn plural(n: usize, one: &str, many: &str) -> String {
    format!("{} {}", count(n), pick(n, one, many))
}

/// A sample backup folder path for the message that asks for a full path.
pub fn example_folder() -> &'static str {
    if cfg!(windows) {
        r"C:\Plug-n-Keep"
    } else {
        "/home/you/Plug-n-Keep"
    }
}

/// `2026-11-17T18:00:00Z` as `17 Nov 2026`. Anything else is shown as it is.
pub fn date(utc: &str) -> String {
    const MONTHS: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    let mut parts = utc.get(..10).unwrap_or("").split('-');
    let (Some(y), Some(m), Some(d)) = (parts.next(), parts.next(), parts.next()) else {
        return utc.to_string();
    };
    match (m.parse::<usize>(), d.parse::<u32>()) {
        (Ok(m @ 1..=12), Ok(d)) if y.len() == 4 => format!("{d} {} {y}", MONTHS[m - 1]),
        _ => utc.to_string(),
    }
}

/// A local `2026-11-18 09-30-12` (the sync stamp) as `18 Nov 2026 09:30`. Anything else is
/// shown as it is.
pub fn local_time(stamp: &str) -> String {
    let (Some(day), Some(time)) = (stamp.get(..10), stamp.get(11..16)) else {
        return stamp.to_string();
    };
    let when = format!("{day}T00:00:00Z");
    let shown = date(&when);
    if shown == when {
        return stamp.to_string();
    }
    format!("{shown} {}", time.replace('-', ":"))
}

/// How long something took: "45 s", "3 min 12 s", "1 h 05 min".
pub fn duration(secs: f64) -> String {
    if !secs.is_finite() || secs < 0.0 {
        return "–".into();
    }
    let s = secs.round() as u64;
    match (s / 3600, (s % 3600) / 60, s % 60) {
        (0, 0, s) => format!("{s} s"),
        (0, m, s) => format!("{m} min {s:02} s"),
        (h, m, _) => format!("{h} h {m:02} min"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_is_singular_and_the_rest_plural() {
        assert_eq!(plural(1, "item", "items"), "1 item");
        assert_eq!(plural(0, "item", "items"), "0 items");
        assert_eq!(plural(1_234, "item", "items"), "1,234 items");
        assert_eq!(pick(1, "is", "are"), "is");
        assert_eq!(pick(2, "is", "are"), "are");
        assert_eq!(files(1), "1 file");
    }

    #[test]
    fn local_times_and_durations() {
        assert_eq!(local_time("2026-11-18 09-30-12"), "18 Nov 2026 09:30");
        assert_eq!(local_time("junk"), "junk");
        assert_eq!(local_time(""), "");
        assert_eq!(duration(0.4), "0 s");
        assert_eq!(duration(45.0), "45 s");
        assert_eq!(duration(192.0), "3 min 12 s");
        assert_eq!(duration(3900.0), "1 h 05 min");
        assert_eq!(duration(f64::NAN), "–");
    }

    #[test]
    fn file_counts() {
        assert_eq!(files(0), "0 files");
        assert_eq!(files(1), "1 file");
        assert_eq!(files(1234), "1,234 files");
    }

    #[test]
    fn sizes() {
        assert_eq!(bytes(0), "0 B");
        assert_eq!(bytes(1023), "1023 B");
        assert_eq!(bytes(1024), "1.0 KB");
        assert_eq!(bytes(262_145), "256.0 KB");
        assert_eq!(bytes(8 * 1024 * 1024 + 3), "8.0 MB");
        assert_eq!(bytes(5 * 1024 * 1024 * 1024), "5.0 GB");
        assert_eq!(bytes(u64::MAX), "16777216.0 TB");
    }

    #[test]
    fn speeds() {
        assert_eq!(speed(0.0), "–");
        assert_eq!(speed(f64::NAN), "–");
        assert_eq!(speed(2.5 * 1024.0 * 1024.0), "2.5 MB/s");
    }

    #[test]
    fn times_left() {
        assert_eq!(time_left(0.0), "less than a minute left");
        assert_eq!(time_left(59.9), "less than a minute left");
        assert_eq!(time_left(f64::INFINITY), "time left unknown");
        assert_eq!(time_left(f64::NAN), "time left unknown");
        assert_eq!(time_left(60.0), "about 1 min left");
        assert_eq!(time_left(61.0), "about 2 min left");
        assert_eq!(time_left(3599.0), "about 60 min left");
        assert_eq!(time_left(3601.0), "about 1 h 1 min left");
        assert_eq!(time_left(4800.0), "about 1 h 20 min left");
        assert_eq!(time_left(7200.0), "about 2 h left");
    }

    #[test]
    fn counts() {
        assert_eq!(count(0), "0");
        assert_eq!(count(999), "999");
        assert_eq!(count(1000), "1,000");
        assert_eq!(count(1_234_567), "1,234,567");
    }

    #[test]
    fn dates() {
        assert_eq!(date("2026-11-17T18:00:00Z"), "17 Nov 2026");
        assert_eq!(date("2026-01-05T00:00:00Z"), "5 Jan 2026");
        assert_eq!(date("later"), "later");
        assert_eq!(date("2026-13-01T00:00:00Z"), "2026-13-01T00:00:00Z");
    }
}
