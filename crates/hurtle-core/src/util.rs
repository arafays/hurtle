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
    let mut out: String = if cleaned.is_empty() {
        "download".into()
    } else {
        cleaned.into()
    };
    // Keep well under the 255-byte filename limit (".part" is appended later).
    while out.len() > 200 {
        out.pop();
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
}
