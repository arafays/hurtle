//! A plain-text log shared by the app, the native host and the browser
//! extension: `$HURTLE_DATA/logs/hurtle.log`, one line per event, rotated to
//! `hurtle.log.1` at 2 MB. It never records cookie values or URL queries.

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

const MAX_BYTES: u64 = 2 * 1024 * 1024;

pub fn path() -> PathBuf {
    crate::data_dir().join("logs").join("hurtle.log")
}

/// Append one line: `2026-11-08 05:23:10.123Z host[4510] message`.
pub fn log(component: &str, msg: impl AsRef<str>) {
    let p = path();
    if let Some(dir) = p.parent() {
        let _ = fs::create_dir_all(dir);
    }
    if fs::metadata(&p).is_ok_and(|m| m.len() > MAX_BYTES) {
        let _ = fs::rename(&p, p.with_extension("log.1"));
    }
    let line = format!(
        "{} {component}[{}] {}\n",
        timestamp(),
        std::process::id(),
        msg.as_ref().replace('\n', "\n    ")
    );
    // One write per line with O_APPEND, so lines from several processes don't interleave.
    if let Ok(mut f) = OpenOptions::new().create(true).append(true).open(&p) {
        let _ = f.write_all(line.as_bytes());
    }
}

/// The last `max_bytes` of the log (from the rotated file too if needed).
pub fn tail(max_bytes: usize) -> String {
    let p = path();
    let mut text = fs::read_to_string(p.with_extension("log.1")).unwrap_or_default();
    text.push_str(&fs::read_to_string(&p).unwrap_or_default());
    if text.len() > max_bytes {
        let mut start = text.len() - max_bytes;
        while !text.is_char_boundary(start) {
            start += 1;
        }
        let cut = text[start..].find('\n').map_or(start, |i| start + i + 1);
        text.drain(..cut);
    }
    text
}

/// A URL without its query and fragment (they often carry tokens).
pub fn url(u: &str) -> String {
    let end = u.find(['?', '#']).unwrap_or(u.len());
    if end < u.len() {
        format!("{}?…", &u[..end])
    } else {
        u.to_string()
    }
}

/// UTC time with milliseconds, without pulling in a date crate.
fn timestamp() -> String {
    let d = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    let secs = d.as_secs();
    let (days, rem) = (secs / 86_400, secs % 86_400);
    // Civil date from days since 1970-01-01 (Howard Hinnant's algorithm).
    let z = days as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02} {:02}:{:02}:{:02}.{:03}Z",
        rem / 3600,
        rem % 3600 / 60,
        rem % 60,
        d.subsec_millis()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn urls_and_time() {
        assert_eq!(url("https://a.b/c?token=1#x"), "https://a.b/c?…");
        assert_eq!(url("https://a.b/c"), "https://a.b/c");
        let t = timestamp();
        assert_eq!(t.len(), "2026-11-08 05:23:10.123Z".len());
        assert!(t.starts_with("20"));
    }
}
