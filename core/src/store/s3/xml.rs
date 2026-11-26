//! The few bits of XML and time text the S3 replies hold that rusty-s3 has no parser for.
#![forbid(unsafe_code)]

use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// The text inside the first `<name>...</name>` of `s`, still escaped.
pub(super) fn tag<'a>(s: &'a str, name: &str) -> Option<&'a str> {
    let open = format!("<{name}>");
    let close = format!("</{name}>");
    let from = s.find(&open)? + open.len();
    let len = s[from..].find(&close)?;
    Some(&s[from..from + len])
}

pub(super) fn unescape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(i) = rest.find('&') {
        out.push_str(&rest[..i]);
        rest = &rest[i..];
        let Some(end) = rest.find(';') else { break };
        let ent = &rest[1..end];
        let ch = match ent {
            "amp" => Some('&'),
            "lt" => Some('<'),
            "gt" => Some('>'),
            "quot" => Some('"'),
            "apos" => Some('\''),
            _ => ent
                .strip_prefix("#x")
                .and_then(|h| u32::from_str_radix(h, 16).ok())
                .or_else(|| ent.strip_prefix('#').and_then(|d| d.parse().ok()))
                .and_then(char::from_u32),
        };
        match ch {
            Some(c) => {
                out.push(c);
                rest = &rest[end + 1..];
            }
            None => {
                out.push('&');
                rest = &rest[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

/// The `<Code>` of an S3 error reply, when it looks like a plain code word.
pub(super) fn error_code(body: &str) -> Option<String> {
    let c = tag(body, "Code")?.trim();
    (!c.is_empty() && c.len() < 64 && c.bytes().all(|b| b.is_ascii_alphanumeric()))
        .then(|| c.into())
}

/// Copy and Complete can answer 200 and still carry an error in the body.
pub(super) fn is_error_body(body: &str) -> bool {
    let b = body.trim_start();
    let b = b.strip_prefix("<?xml").map_or(b, |r| {
        r.split_once("?>")
            .map_or(r, |(_, after)| after.trim_start())
    });
    b.starts_with("<Error>") || b.starts_with("<Error ")
}

#[derive(Debug, Default, PartialEq, Eq)]
pub(super) struct Uploads {
    /// key, upload id, initiated
    pub items: Vec<(String, String, SystemTime)>,
    /// Where to continue, when the list was cut short: (key marker, upload id marker).
    pub next: Option<(String, String)>,
}

/// A ListMultipartUploads reply.
pub(super) fn parse_uploads(body: &str) -> Uploads {
    let mut out = Uploads::default();
    let mut rest = body;
    while let Some(i) = rest.find("<Upload>") {
        let Some(len) = rest[i..].find("</Upload>") else {
            break;
        };
        let block = &rest[i..i + len];
        rest = &rest[i + len..];
        let (Some(key), Some(id)) = (tag(block, "Key"), tag(block, "UploadId")) else {
            continue;
        };
        // No time known means "just now": never old enough to clean up.
        let when = tag(block, "Initiated")
            .and_then(parse_time)
            .unwrap_or_else(SystemTime::now);
        out.items.push((unescape(key), unescape(id), when));
    }
    if tag(body, "IsTruncated").map(str::trim) == Some("true") {
        let k = tag(body, "NextKeyMarker").map(unescape).unwrap_or_default();
        let u = tag(body, "NextUploadIdMarker")
            .map(unescape)
            .unwrap_or_default();
        out.next = Some((k, u));
    }
    out
}

/// `2026-11-26T12:34:56.789Z` (UTC, fraction optional).
pub(super) fn parse_time(s: &str) -> Option<SystemTime> {
    let s = s.trim().strip_suffix('Z')?;
    let (date, time) = s.split_once('T')?;
    let mut d = date.split('-');
    let (y, m, day): (i64, i64, i64) = (
        d.next()?.parse().ok()?,
        d.next()?.parse().ok()?,
        d.next()?.parse().ok()?,
    );
    let (hms, frac) = time.split_once('.').unwrap_or((time, ""));
    let mut t = hms.split(':');
    let (h, mi, sec): (u64, u64, u64) = (
        t.next()?.parse().ok()?,
        t.next()?.parse().ok()?,
        t.next()?.parse().ok()?,
    );
    if !(1..=12).contains(&m) || !(1..=31).contains(&day) || h > 23 || mi > 59 || sec > 60 {
        return None;
    }
    // Days from 1970-01-01 (civil calendar, proleptic Gregorian).
    let y2 = if m <= 2 { y - 1 } else { y };
    let era = y2.div_euclid(400);
    let yoe = y2 - era * 400;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    let secs = u64::try_from(days).ok()? * 86_400 + h * 3600 + mi * 60 + sec;
    let nanos: u32 = if frac.is_empty() {
        0
    } else {
        let digits: String = frac.chars().take(9).collect();
        digits.parse::<u32>().ok()? * 10u32.pow(9 - digits.len() as u32)
    };
    Some(UNIX_EPOCH + Duration::new(secs, nanos))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn times_parse() {
        assert_eq!(parse_time("1970-01-01T00:00:00Z"), Some(UNIX_EPOCH));
        assert_eq!(
            parse_time("2026-11-26T12:34:56.5Z"),
            Some(UNIX_EPOCH + Duration::new(1_795_696_496, 500_000_000))
        );
        assert_eq!(
            parse_time("2000-03-01T00:00:00.000Z"),
            Some(UNIX_EPOCH + Duration::from_secs(951_868_800))
        );
        assert_eq!(parse_time("nonsense"), None);
    }

    #[test]
    fn uploads_parse() {
        let body = "<?xml version=\"1.0\"?><ListMultipartUploadsResult>\
            <IsTruncated>true</IsTruncated><NextKeyMarker>b&amp;c</NextKeyMarker>\
            <NextUploadIdMarker>u2</NextUploadIdMarker>\
            <Upload><Key>a/x &amp; y.bin</Key><UploadId>u1</UploadId>\
            <Initiated>2026-01-01T00:00:00.000Z</Initiated></Upload>\
            <Upload><Key>b</Key><UploadId>u2</UploadId></Upload></ListMultipartUploadsResult>";
        let u = parse_uploads(body);
        assert_eq!(u.items.len(), 2);
        assert_eq!(u.items[0].0, "a/x & y.bin");
        assert_eq!(u.items[0].1, "u1");
        assert_eq!(u.next, Some(("b&c".into(), "u2".into())));
        assert!(
            parse_uploads("<R><IsTruncated>false</IsTruncated></R>")
                .next
                .is_none()
        );
    }

    #[test]
    fn error_bodies() {
        let b =
            "<?xml version=\"1.0\"?><Error><Code>NoSuchUpload</Code><Message>m</Message></Error>";
        assert!(is_error_body(b));
        assert_eq!(error_code(b).as_deref(), Some("NoSuchUpload"));
        assert!(!is_error_body(
            "<CopyObjectResult><ETag>x</ETag></CopyObjectResult>"
        ));
        assert_eq!(unescape("a&lt;&#65;&#x42;&bogus;"), "a<AB&bogus;");
    }
}
