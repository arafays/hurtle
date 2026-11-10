//! Small helpers: filenames, header parsing and human-readable formatting.

use std::path::{Path, PathBuf};

use percent_encoding::percent_decode_str;

/// Pull a filename out of a `Content-Disposition` header.
/// `filename*=UTF-8''…` (RFC 5987) wins over plain `filename=`.
pub fn filename_from_disposition(value: &str) -> Option<String> {
    let mut plain = None;
    for part in value.split(';').map(str::trim) {
        let Some((key, val)) = part.split_once('=') else {
            continue;
        };
        let key = key.trim().to_ascii_lowercase();
        let val = val.trim();
        if key == "filename*" {
            // charset'lang'percent-encoded
            let encoded = val.splitn(3, '\'').nth(2).unwrap_or(val);
            let decoded = percent_decode_str(encoded.trim_matches('"')).decode_utf8_lossy();
            if !decoded.is_empty() {
                return Some(decoded.into_owned());
            }
        } else if key == "filename" {
            let v = val.trim_matches('"');
            if !v.is_empty() {
                plain = Some(percent_decode_str(v).decode_utf8_lossy().into_owned());
            }
        }
    }
    plain
}

/// Last non-empty path segment of a URL, percent-decoded.
pub fn filename_from_url(url: &str) -> Option<String> {
    let parsed = reqwest::Url::parse(url).ok()?;
    let seg = parsed
        .path_segments()?
        .rfind(|s| !s.is_empty())?
        .to_string();
    let decoded = percent_decode_str(&seg).decode_utf8_lossy().into_owned();
    (!decoded.is_empty()).then_some(decoded)
}

/// Only plain web links are downloaded or probed.
pub fn require_http(url: &str) -> anyhow::Result<reqwest::Url> {
    let parsed = reqwest::Url::parse(url.trim())
        .map_err(|e| anyhow::anyhow!("invalid URL: {}: {e}", url.trim()))?;
    anyhow::ensure!(
        matches!(parsed.scheme(), "http" | "https"),
        "only http(s) URLs are supported"
    );
    Ok(parsed)
}

/// Cookie and Authorization belong to the site they were gathered for: keep
/// them only when a redirect landed on the same origin (scheme and port, no
/// https to http downgrade) and the same host or a parent/child of it.
pub fn headers_for_redirect(
    from: &reqwest::Url,
    to: &reqwest::Url,
    headers: &reqwest::header::HeaderMap,
) -> reqwest::header::HeaderMap {
    let mut out = headers.clone();
    if !same_site(from, to) {
        out.remove(reqwest::header::COOKIE);
        out.remove(reqwest::header::AUTHORIZATION);
    }
    out
}

fn same_site(a: &reqwest::Url, b: &reqwest::Url) -> bool {
    // Same scheme (so no https to http downgrade) and same port.
    if a.scheme() != b.scheme() || a.port_or_known_default() != b.port_or_known_default() {
        return false;
    }
    let (Some(a), Some(b)) = (a.host_str(), b.host_str()) else {
        return false;
    };
    let (a, b) = (a.to_ascii_lowercase(), b.to_ascii_lowercase());
    if a == b {
        return true;
    }
    // No public-suffix list: only an exact host or a strict parent/child
    // relationship counts, so siblings under a shared suffix (a.github.io and
    // b.github.io) are different sites.
    let is_ip = |h: &str| h.parse::<std::net::IpAddr>().is_ok() || h.starts_with('[');
    if is_ip(&a) || is_ip(&b) {
        return false;
    }
    let child_of = |child: &str, parent: &str| {
        parent.contains('.') && child.strip_suffix(parent).is_some_and(|r| r.ends_with('.'))
    };
    child_of(&a, &b) || child_of(&b, &a)
}

/// Create a directory (and parents) that only the current user can enter.
pub fn create_private_dir(dir: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

/// Create (truncating) a file readable only by the current user.
pub fn create_private_file(path: &Path) -> std::io::Result<std::fs::File> {
    let mut o = std::fs::OpenOptions::new();
    o.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        o.mode(0o600);
    }
    let f = o.open(path)?;
    #[cfg(unix)]
    {
        // An older, looser file keeps its mode through `open`.
        use std::os::unix::fs::PermissionsExt;
        f.set_permissions(std::fs::Permissions::from_mode(0o600))?;
    }
    Ok(f)
}

/// `std::fs::write` for files that may hold cookies.
pub fn write_private(path: &Path, data: &[u8]) -> std::io::Result<()> {
    std::io::Write::write_all(&mut create_private_file(path)?, data)
}

/// File types that can have their own download folder: (id, label).
pub const CATEGORIES: [(&str, &str); 6] = [
    ("video", "Videos"),
    ("audio", "Music"),
    ("image", "Pictures"),
    ("document", "Documents"),
    ("archive", "Archives"),
    ("program", "Programs"),
];

/// Which [`CATEGORIES`] entry a file name belongs to, by its extension.
pub fn category(filename: &str) -> Option<&'static str> {
    let ext = Path::new(filename)
        .extension()?
        .to_str()?
        .to_ascii_lowercase();
    Some(match ext.as_str() {
        "mp4" | "m4v" | "mkv" | "webm" | "mov" | "avi" | "wmv" | "flv" | "mpg" | "mpeg" | "3gp"
        | "ts" | "m2ts" | "ogv" => "video",
        "mp3" | "m4a" | "aac" | "flac" | "wav" | "ogg" | "oga" | "opus" | "wma" | "alac"
        | "aiff" => "audio",
        "jpg" | "jpeg" | "png" | "gif" | "webp" | "avif" | "heic" | "heif" | "bmp" | "tif"
        | "tiff" | "svg" | "ico" | "raw" | "cr2" | "nef" | "dng" => "image",
        "pdf" | "doc" | "docx" | "odt" | "rtf" | "txt" | "md" | "epub" | "mobi" | "xls"
        | "xlsx" | "ods" | "csv" | "ppt" | "pptx" | "odp" => "document",
        "zip" | "rar" | "7z" | "tar" | "gz" | "tgz" | "bz2" | "xz" | "zst" | "iso" => "archive",
        "exe" | "msi" | "dmg" | "pkg" | "deb" | "rpm" | "appimage" | "apk" | "flatpak" | "snap"
        | "sh" | "run" | "bin" => "program",
        _ => return None,
    })
}

/// Make a name safe to use as a single path component.
pub fn sanitize_filename(name: &str) -> String {
    let cleaned: String = name
        .chars()
        .map(|c| match c {
            '/' | '\\' | '\0' | ':' | '*' | '?' | '"' | '<' | '>' | '|' => '_',
            c if c.is_control() => '_',
            c => c,
        })
        .collect();
    let cleaned = cleaned.trim().trim_start_matches('.').trim();
    // Keep well under the 255-byte filename limit (".part" is appended
    // later), cutting the stem so the extension survives.
    let (stem, ext) = split_ext(cleaned);
    let (stem, ext) = if ext.len() > 16 {
        (cleaned, "")
    } else {
        (stem, ext)
    };
    let mut stem = stem;
    let room = 200usize.saturating_sub(ext.len());
    if stem.len() > room {
        let mut cut = room;
        while !stem.is_char_boundary(cut) {
            cut -= 1;
        }
        stem = &stem[..cut];
    }
    // Windows silently drops trailing dots and spaces.
    let mut out = format!("{stem}{ext}")
        .trim_end_matches(['.', ' '])
        .to_string();
    if out.is_empty() {
        return "download".into();
    }
    // Device names can't be files on Windows, with or without an extension.
    let base = out.split('.').next().unwrap_or_default().trim_end();
    const DEVICES: [&str; 4] = ["CON", "PRN", "AUX", "NUL"];
    let reserved = DEVICES.iter().any(|d| base.eq_ignore_ascii_case(d))
        || base.len() == 4
            && (base.as_bytes()[..3].eq_ignore_ascii_case(b"COM")
                || base.as_bytes()[..3].eq_ignore_ascii_case(b"LPT"))
            && base.as_bytes()[3].is_ascii_digit();
    if reserved {
        out.insert(0, '_');
    }
    out
}

/// `dir/name`, or `dir/name (1).ext`, `dir/name (2).ext`… if taken.
/// `taken` lets the caller reserve names used by other in-flight downloads.
pub fn unique_path(dir: &Path, name: &str, taken: impl Fn(&Path) -> bool) -> PathBuf {
    let candidate = dir.join(name);
    let free = |p: &Path| !p.exists() && !part_path(p).exists() && !taken(p);
    if free(&candidate) {
        return candidate;
    }
    let (stem, ext) = split_ext(name);
    for n in 1.. {
        let p = dir.join(format!("{stem} ({n}){ext}"));
        if free(&p) {
            return p;
        }
    }
    unreachable!()
}

/// Split `archive.tar.gz` into (`archive`, `.tar.gz`) and `movie.mkv` into (`movie`, `.mkv`).
fn split_ext(name: &str) -> (&str, &str) {
    for double in [".tar.gz", ".tar.xz", ".tar.bz2", ".tar.zst"] {
        if name.len() > double.len() && name.to_ascii_lowercase().ends_with(double) {
            let i = name.len() - double.len();
            return (&name[..i], &name[i..]);
        }
    }
    match name.rfind('.') {
        Some(i) if i > 0 => (&name[..i], &name[i..]),
        _ => (name, ""),
    }
}

/// Where bytes land while a download is in progress.
pub fn part_path(path: &Path) -> PathBuf {
    let mut s = path.as_os_str().to_owned();
    s.push(".part");
    PathBuf::from(s)
}

/// Total size from `Content-Range: bytes 0-0/12345`.
pub fn total_from_content_range(value: &str) -> Option<u64> {
    value.rsplit_once('/')?.1.trim().parse().ok()
}

/// Start offset from `Content-Range: bytes 100-199/12345`.
pub fn start_from_content_range(value: &str) -> Option<u64> {
    let range = value.trim().strip_prefix("bytes")?.trim();
    range.split_once('-')?.0.trim().parse().ok()
}

pub fn fmt_bytes(n: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut v = n as f64;
    let mut u = 0;
    while v >= 1024.0 && u < UNITS.len() - 1 {
        v /= 1024.0;
        u += 1;
    }
    if u == 0 {
        format!("{n} B")
    } else if v >= 100.0 {
        format!("{v:.0} {}", UNITS[u])
    } else {
        format!("{v:.1} {}", UNITS[u])
    }
}

pub fn fmt_speed(bytes_per_sec: f64) -> String {
    format!("{}/s", fmt_bytes(bytes_per_sec.max(0.0) as u64))
}

pub fn fmt_duration(secs: u64) -> String {
    let (h, m, s) = (secs / 3600, (secs / 60) % 60, secs % 60);
    if h > 0 {
        format!("{h}h {m:02}m")
    } else if m > 0 {
        format!("{m}m {s:02}s")
    } else {
        format!("{s}s")
    }
}

pub fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redirect_headers() {
        use reqwest::header::{COOKIE, HeaderMap, HeaderValue, REFERER};
        let mut h = HeaderMap::new();
        h.insert(COOKIE, HeaderValue::from_static("a=b"));
        h.insert(REFERER, HeaderValue::from_static("https://x/"));
        let u = |s: &str| reqwest::Url::parse(s).unwrap();
        let keep = headers_for_redirect(
            &u("https://a.example.com/f"),
            &u("https://a.example.com/g"),
            &h,
        );
        assert!(keep.contains_key(COOKIE));
        let sub = headers_for_redirect(
            &u("https://example.com/f"),
            &u("https://cdn.example.com/g"),
            &h,
        );
        assert!(sub.contains_key(COOKIE));
        let sib = headers_for_redirect(
            &u("https://www.example.com/f"),
            &u("https://cdn.example.com/g"),
            &h,
        );
        assert!(!sib.contains_key(COOKIE));
        let down = headers_for_redirect(
            &u("https://a.example.com/f"),
            &u("http://a.example.com/g"),
            &h,
        );
        assert!(!down.contains_key(COOKIE));
        let port = headers_for_redirect(
            &u("https://a.example.com/f"),
            &u("https://a.example.com:8443/g"),
            &h,
        );
        assert!(!port.contains_key(COOKIE));
        let gh = headers_for_redirect(&u("https://a.github.io/f"), &u("https://b.github.io/g"), &h);
        assert!(!gh.contains_key(COOKIE));
        let drop = headers_for_redirect(&u("https://example.com/f"), &u("https://evil.net/g"), &h);
        assert!(!drop.contains_key(COOKIE) && drop.contains_key(REFERER));
        let uk = headers_for_redirect(&u("https://a.co.uk/f"), &u("https://b.co.uk/g"), &h);
        assert!(!uk.contains_key(COOKIE));
    }

    #[test]
    fn http_only() {
        assert!(require_http("https://h/x").is_ok());
        assert!(require_http("ftp://h/x").is_err());
        assert!(require_http("ytsearchall:foo").is_err());
    }

    /// folders.js mirrors CATEGORIES and `category`; keep them in sync.
    #[test]
    fn folders_js_matches() {
        let js = include_str!("../../../extension/src/folders.js");
        let cats = js
            .split("const EXT = {")
            .nth(1)
            .unwrap()
            .split("};")
            .next()
            .unwrap();
        let mut seen = std::collections::HashSet::new();
        for line in cats.lines().filter_map(|l| l.trim().split_once(": ")) {
            let (id, list) = (
                line.0,
                line.1.trim().trim_end_matches(',').trim_matches('"'),
            );
            assert!(
                CATEGORIES.iter().any(|(c, _)| *c == id),
                "unknown {id} in folders.js"
            );
            for ext in list.split(' ') {
                assert_eq!(
                    category(&format!("x.{ext}")),
                    CATEGORIES.iter().map(|c| c.0).find(|c| *c == id),
                    ".{ext}"
                );
                seen.insert(ext.to_string());
            }
        }
        for (id, label) in CATEGORIES {
            assert!(
                js.contains(&format!("[\"{id}\", \"{label}\"]")),
                "{id} missing"
            );
        }
        // Reverse: every short extension util knows must be in folders.js.
        let alpha: Vec<char> = ('a'..='z').chain('0'..='9').collect();
        let mut names = vec![];
        for a in &alpha {
            names.push(a.to_string());
            for b in &alpha {
                names.push(format!("{a}{b}"));
                for c in &alpha {
                    names.push(format!("{a}{b}{c}"));
                }
            }
        }
        for e in names {
            if category(&format!("x.{e}")).is_some() {
                assert!(
                    seen.contains(&e),
                    ".{e} is in util::category but not folders.js"
                );
            }
        }
        for e in [
            "jpeg", "tiff", "webm", "mpeg", "docx", "xlsx", "pptx", "appimage", "flatpak", "heic",
            "heif", "avif", "webp", "m2ts", "epub", "mobi", "alac", "aiff", "opus",
        ] {
            assert!(
                category(&format!("x.{e}")).is_none() || seen.contains(e),
                ".{e}"
            );
        }
    }

    #[test]
    fn disposition() {
        assert_eq!(
            filename_from_disposition(r#"attachment; filename="a b.zip""#).as_deref(),
            Some("a b.zip")
        );
        assert_eq!(
            filename_from_disposition("attachment; filename=x.bin; filename*=UTF-8''%E2%9C%93.iso")
                .as_deref(),
            Some("✓.iso")
        );
        assert_eq!(filename_from_disposition("inline"), None);
    }

    #[test]
    fn url_names() {
        assert_eq!(
            filename_from_url("https://h/dir/My%20File.tar.gz?x=1").as_deref(),
            Some("My File.tar.gz")
        );
        assert_eq!(filename_from_url("https://h/"), None);
    }

    #[test]
    fn content_range() {
        assert_eq!(total_from_content_range("bytes 0-0/12345"), Some(12345));
        assert_eq!(total_from_content_range("bytes 0-0/*"), None);
        assert_eq!(start_from_content_range("bytes 100-199/12345"), Some(100));
    }

    #[test]
    fn ext_split() {
        assert_eq!(split_ext("a.tar.gz"), ("a", ".tar.gz"));
        assert_eq!(split_ext("movie.mkv"), ("movie", ".mkv"));
        assert_eq!(split_ext(".bashrc"), (".bashrc", ""));
        assert_eq!(sanitize_filename("../etc/passwd"), "_etc_passwd");
    }

    #[test]
    fn sanitize() {
        let long = sanitize_filename(&format!("{}.mp4", "a".repeat(300)));
        assert!(long.ends_with(".mp4") && long.len() <= 200);
        let multi = sanitize_filename(&format!("{}.mp4", "é".repeat(150)));
        assert!(multi.ends_with(".mp4") && multi.len() <= 200);
        assert_eq!(sanitize_filename("NUL.txt"), "_NUL.txt");
        assert_eq!(sanitize_filename("aux"), "_aux");
        assert_eq!(sanitize_filename("COM1.zip"), "_COM1.zip");
        assert_eq!(sanitize_filename("lpt9"), "_lpt9");
        assert_eq!(sanitize_filename("console.txt"), "console.txt");
        assert_eq!(sanitize_filename("COM10"), "COM10");
        assert_eq!(sanitize_filename("éé"), "éé");
        assert_eq!(sanitize_filename("a."), "a");
        assert_eq!(sanitize_filename("a. "), "a");
        assert_eq!(sanitize_filename("..."), "download");
        assert_eq!(sanitize_filename("x.tar.gz"), "x.tar.gz");
    }
}
