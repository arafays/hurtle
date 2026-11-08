//! The helper programs video downloads need: yt-dlp, ffmpeg (+ ffprobe) and
//! deno (yt-dlp's JavaScript runtime for YouTube).
//!
//! An installed copy is used when there is one. Otherwise the tool is fetched
//! once from its official release into `<data dir>/tools`, checked against the
//! SHA-256 sums the project publishes, and used from there. The tools dir is
//! also on the PATH yt-dlp gets, so it finds a fetched ffmpeg and deno.
//!
//! A fetched yt-dlp updates itself (`yt-dlp -U`) about once a day: sites
//! change often, and an old yt-dlp stops working on them. An installed one is
//! left to the package manager that owns it.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, SystemTime};

use sha2::{Digest, Sha256};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tool {
    YtDlp,
    Ffmpeg,
    Deno,
}

impl Tool {
    pub fn name(self) -> &'static str {
        match self {
            Tool::YtDlp => "yt-dlp",
            Tool::Ffmpeg => "ffmpeg",
            Tool::Deno => "deno",
        }
    }

    fn lock(self) -> &'static Mutex<()> {
        static LOCKS: [Mutex<()>; 3] = [Mutex::new(()), Mutex::new(()), Mutex::new(())];
        &LOCKS[self as usize]
    }
}

/// Where fetched tools live. Shared by the app, the CLI and the browser host.
pub fn dir() -> PathBuf {
    crate::data_dir().join("tools")
}

/// Directories to look for tools in, in order. The browser launches the host
/// (and so Surge) without the user's shell PATH additions (mise shims,
/// Homebrew on macOS), so this also has the usual install dirs and our own
/// executable's dir (where Windows users can drop yt-dlp.exe). Fetched tools
/// come last: an installed copy wins.
///
/// `SURGE_TOOLS_ONLY=1` skips the installed ones, to test fetching.
pub fn search_dirs() -> Vec<PathBuf> {
    if std::env::var_os("SURGE_TOOLS_ONLY").is_some_and(|v| v == "1") {
        return vec![dir()];
    }
    let mut dirs: Vec<PathBuf> = std::env::var_os("PATH")
        .map(|p| std::env::split_paths(&p).collect())
        .unwrap_or_default();
    if let Some(exe_dir) = std::env::current_exe()
        .ok()
        .and_then(|e| e.parent().map(PathBuf::from))
    {
        dirs.insert(0, exe_dir);
    }
    let home = dirs::home_dir();
    if cfg!(windows) {
        if let Some(local) = dirs::data_local_dir() {
            dirs.push(local.join("Microsoft").join("WinGet").join("Links"));
        }
        if let Some(h) = &home {
            dirs.push(h.join("scoop").join("shims"));
        }
    } else {
        dirs.extend(
            [
                "/opt/homebrew/bin",
                "/usr/local/bin",
                "/usr/bin",
                "/opt/local/bin",
            ]
            .map(PathBuf::from),
        );
        if let Some(h) = &home {
            dirs.push(h.join(".local/bin"));
        }
    }
    dirs.push(dir());
    dirs
}

fn exe(name: &str) -> String {
    format!("{name}{}", std::env::consts::EXE_SUFFIX)
}

/// An installed or already-fetched copy.
pub fn find(tool: Tool) -> Option<PathBuf> {
    let file = exe(tool.name());
    search_dirs()
        .into_iter()
        .map(|d| d.join(&file))
        .find(|p| p.is_file())
}

/// Whether this platform has a release of the tool to fetch.
pub fn fetchable(tool: Tool) -> bool {
    !sources(tool).is_empty()
}

/// Found, or fetchable on demand.
pub fn available(tool: Tool) -> bool {
    find(tool).is_some() || fetchable(tool)
}

/// Fetch progress: (tool, bytes so far, total if known).
pub type Progress<'a> = &'a (dyn Fn(Tool, u64, Option<u64>) + Sync);

/// The tool's path, fetching it first when it isn't there. Blocking: call it
/// off the async runtime (`spawn_blocking`).
pub fn ensure(tool: Tool, progress: Progress) -> Result<PathBuf, String> {
    if let Some(p) = find(tool) {
        return Ok(p);
    }
    // One fetch per tool in this process; another process fetching at the
    // same time only costs bandwidth (both install the same files).
    let _guard = tool.lock().lock().unwrap_or_else(|e| e.into_inner());
    if let Some(p) = find(tool) {
        return Ok(p);
    }
    let srcs = sources(tool);
    if srcs.is_empty() {
        return Err(format!("no {} build for this platform", tool.name()));
    }
    crate::log::log("tools", format!("fetching {}", tool.name()));
    let started = std::time::Instant::now();
    match install(tool, &srcs, progress) {
        Ok(path) => {
            crate::log::log(
                "tools",
                format!(
                    "installed {} in {:.0?} at {}",
                    tool.name(),
                    started.elapsed(),
                    path.display()
                ),
            );
            Ok(path)
        }
        Err(e) => {
            crate::log::log("tools", format!("fetching {} failed: {e}", tool.name()));
            Err(e)
        }
    }
}

/// Where the published checksum for a download is.
#[derive(Clone, Debug, PartialEq)]
enum Sums {
    /// A `sha256sum`-style list, with the asset's line found by file name.
    List { url: String, name: String },
    /// A file holding just this download's sum.
    Single(String),
    /// `<final URL after redirects>.sha256`.
    NextToFinal,
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Archive {
    /// The download is the program itself.
    Bare,
    Zip,
    TarXz,
}

#[derive(Clone, Debug, PartialEq)]
struct Source {
    url: String,
    sums: Sums,
    archive: Archive,
    /// Programs to take out of it (without the .exe suffix).
    files: &'static [&'static str],
}

const YTDLP: &str = "https://github.com/yt-dlp/yt-dlp/releases/latest/download/";
const FFMPEG_BUILDS: &str = "https://github.com/yt-dlp/FFmpeg-Builds/releases/download/latest/";
const DENO: &str = "https://github.com/denoland/deno/releases/latest/download/";
/// macOS ffmpeg (FFmpeg-Builds has no macOS builds): signed static builds.
const MAC_FFMPEG: &str = "https://ffmpeg.martin-riedl.de/redirect/latest/macos/";

fn sources(tool: Tool) -> Vec<Source> {
    sources_for(
        tool,
        std::env::consts::OS,
        std::env::consts::ARCH,
        cfg!(target_env = "musl"),
    )
}

fn sources_for(tool: Tool, os: &str, arch: &str, musl: bool) -> Vec<Source> {
    match tool {
        Tool::YtDlp => {
            let asset = match (os, arch, musl) {
                ("linux", "x86_64", false) => "yt-dlp_linux",
                ("linux", "aarch64", false) => "yt-dlp_linux_aarch64",
                ("linux", "x86_64", true) => "yt-dlp_musllinux",
                ("linux", "aarch64", true) => "yt-dlp_musllinux_aarch64",
                ("macos", _, _) => "yt-dlp_macos",
                ("windows", "x86_64", _) => "yt-dlp.exe",
                ("windows", "aarch64", _) => "yt-dlp_arm64.exe",
                _ => return vec![],
            };
            vec![Source {
                url: format!("{YTDLP}{asset}"),
                sums: Sums::List {
                    url: format!("{YTDLP}SHA2-256SUMS"),
                    name: asset.into(),
                },
                archive: Archive::Bare,
                files: &["yt-dlp"],
            }]
        }
        Tool::Ffmpeg => {
            let build = |asset: &str, archive| Source {
                url: format!("{FFMPEG_BUILDS}{asset}"),
                sums: Sums::List {
                    url: format!("{FFMPEG_BUILDS}checksums.sha256"),
                    name: asset.into(),
                },
                archive,
                files: &["ffmpeg", "ffprobe"],
            };
            let mac = |arch: &str| {
                ["ffprobe", "ffmpeg"]
                    .into_iter()
                    .map(|p| Source {
                        url: format!("{MAC_FFMPEG}{arch}/release/{p}.zip"),
                        sums: Sums::NextToFinal,
                        archive: Archive::Zip,
                        files: if p == "ffmpeg" {
                            &["ffmpeg"]
                        } else {
                            &["ffprobe"]
                        },
                    })
                    .collect()
            };
            match (os, arch) {
                ("linux", "x86_64") => {
                    vec![build(
                        "ffmpeg-master-latest-linux64-gpl.tar.xz",
                        Archive::TarXz,
                    )]
                }
                ("linux", "aarch64") => vec![build(
                    "ffmpeg-master-latest-linuxarm64-gpl.tar.xz",
                    Archive::TarXz,
                )],
                ("windows", "x86_64") => {
                    vec![build("ffmpeg-master-latest-win64-gpl.zip", Archive::Zip)]
                }
                ("windows", "aarch64") => {
                    vec![build("ffmpeg-master-latest-winarm64-gpl.zip", Archive::Zip)]
                }
                ("macos", "x86_64") => mac("amd64"),
                ("macos", "aarch64") => mac("arm64"),
                _ => vec![],
            }
        }
        Tool::Deno => {
            let triple = match (os, arch, musl) {
                ("linux", "x86_64", false) => "x86_64-unknown-linux-gnu",
                ("linux", "aarch64", false) => "aarch64-unknown-linux-gnu",
                ("macos", "x86_64", _) => "x86_64-apple-darwin",
                ("macos", "aarch64", _) => "aarch64-apple-darwin",
                ("windows", "x86_64", _) => "x86_64-pc-windows-msvc",
                ("windows", "aarch64", _) => "aarch64-pc-windows-msvc",
                _ => return vec![],
            };
            let url = format!("{DENO}deno-{triple}.zip");
            vec![Source {
                sums: Sums::Single(format!("{url}.sha256sum")),
                url,
                archive: Archive::Zip,
                files: &["deno"],
            }]
        }
    }
}

fn client() -> Result<reqwest::blocking::Client, String> {
    reqwest::blocking::Client::builder()
        .user_agent(concat!("surge/", env!("CARGO_PKG_VERSION")))
        .connect_timeout(Duration::from_secs(20))
        // The whole transfer: ffmpeg is ~100 MB, even slow links get there.
        .timeout(Duration::from_secs(30 * 60))
        .build()
        .map_err(|e| e.to_string())
}

fn install(tool: Tool, srcs: &[Source], progress: Progress) -> Result<PathBuf, String> {
    let dir = dir();
    std::fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    clean_stale(&dir);
    let staging = dir.join(format!(".{}-{}", tool.name(), std::process::id()));
    let _ = std::fs::remove_dir_all(&staging);
    std::fs::create_dir_all(&staging).map_err(|e| format!("{}: {e}", staging.display()))?;
    let result = (|| {
        let client = client()?;
        let mut done: u64 = 0;
        for src in srcs {
            let file = staging.join("download");
            // `latest` can move between reading the sums and the file
            // (FFmpeg-Builds rebuilds daily): one retry before giving up.
            let mut attempt = 0;
            loop {
                let (sum, final_url) = download(&client, &src.url, &file, &|n, total| {
                    progress(tool, done + n, total.map(|t| done + t))
                })?;
                let want = expected_sum(&client, src, &final_url)?;
                if sum == want {
                    break;
                }
                attempt += 1;
                if attempt == 2 {
                    return Err(format!(
                        "{}: checksum mismatch (got {sum}, published {want})",
                        src.url
                    ));
                }
            }
            done += std::fs::metadata(&file).map(|m| m.len()).unwrap_or(0);
            unpack(&file, src, &staging)?;
            let _ = std::fs::remove_file(&file);
        }
        // Helpers first, the tool itself last: its presence means "complete".
        let mut names: Vec<&str> = srcs.iter().flat_map(|s| s.files.iter().copied()).collect();
        names.sort_by_key(|n| *n == tool.name());
        for name in names {
            let from = staging.join(exe(name));
            let to = dir.join(exe(name));
            make_runnable(&from)?;
            if let Err(e) = std::fs::rename(&from, &to)
                && !to.is_file()
            {
                return Err(format!("{}: {e}", to.display()));
            }
        }
        Ok(dir.join(exe(tool.name())))
    })();
    let _ = std::fs::remove_dir_all(&staging);
    result
}

/// Leftovers of a fetch that was killed midway (the browser host exits with
/// the message it answered).
fn clean_stale(dir: &Path) {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    for e in rd.flatten() {
        let old = e
            .metadata()
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.elapsed().ok())
            .is_some_and(|age| age > Duration::from_secs(3600));
        if old && e.file_name().to_string_lossy().starts_with('.') && e.path().is_dir() {
            let _ = std::fs::remove_dir_all(e.path());
        }
    }
}

/// Stream `url` to `dest`, hashing as it goes. Returns the SHA-256 (hex) and
/// the URL after redirects.
fn download(
    client: &reqwest::blocking::Client,
    url: &str,
    dest: &Path,
    progress: &dyn Fn(u64, Option<u64>),
) -> Result<(String, String), String> {
    let mut resp = client
        .get(url)
        .send()
        .and_then(|r| r.error_for_status())
        .map_err(|e| format!("{url}: {e}"))?;
    let final_url = resp.url().to_string();
    let total = resp.content_length();
    let mut out = std::fs::File::create(dest).map_err(|e| format!("{}: {e}", dest.display()))?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 256 * 1024];
    let mut got: u64 = 0;
    let mut reported = 0;
    loop {
        let n = resp.read(&mut buf).map_err(|e| format!("{url}: {e}"))?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
        out.write_all(&buf[..n])
            .map_err(|e| format!("{}: {e}", dest.display()))?;
        got += n as u64;
        if got - reported >= 1 << 20 {
            reported = got;
            progress(got, total);
        }
    }
    out.flush().map_err(|e| e.to_string())?;
    if let Some(t) = total
        && got != t
    {
        return Err(format!("{url}: got {got} of {t} bytes"));
    }
    progress(got, total);
    Ok((hex(&hasher.finalize()), final_url))
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn expected_sum(
    client: &reqwest::blocking::Client,
    src: &Source,
    final_url: &str,
) -> Result<String, String> {
    let get = |url: &str| -> Result<String, String> {
        client
            .get(url)
            .send()
            .and_then(|r| r.error_for_status())
            .and_then(|r| r.text())
            .map_err(|e| format!("{url}: {e}"))
    };
    let (url, text) = match &src.sums {
        Sums::List { url, name } => {
            let text = get(url)?;
            return sum_for(&text, name).ok_or_else(|| format!("{url}: no sum for {name}"));
        }
        Sums::Single(url) => (url.clone(), get(url)?),
        Sums::NextToFinal => {
            let url = format!("{final_url}.sha256");
            let text = get(&url)?;
            (url, text)
        }
    };
    first_sum(&text).ok_or_else(|| format!("{url}: no checksum in it"))
}

/// The first SHA-256 in a text, lowercased. Covers `sha256sum` output and
/// PowerShell's `Get-FileHash` listing (deno's Windows sums).
fn first_sum(text: &str) -> Option<String> {
    text.split(|c: char| !c.is_ascii_hexdigit())
        .find(|w| w.len() == 64)
        .map(str::to_ascii_lowercase)
}

/// The sum on the line for `name` in a `sha256sum`-style list.
fn sum_for(text: &str, name: &str) -> Option<String> {
    text.lines().find_map(|l| {
        let mut parts = l.split_whitespace();
        let sum = parts.next()?;
        let file = parts.next()?.trim_start_matches('*');
        (file == name && sum.len() == 64).then(|| sum.to_ascii_lowercase())
    })
}

/// Put the wanted programs from `file` into `dest` (flat, by file name).
fn unpack(file: &Path, src: &Source, dest: &Path) -> Result<(), String> {
    let err = |e: &dyn std::fmt::Display| format!("unpacking {}: {e}", src.url);
    let wanted = |path: &Path| -> Option<String> {
        let base = path.file_name()?.to_str()?;
        src.files.iter().map(|f| exe(f)).find(|f| f == base)
    };
    let mut found = 0;
    match src.archive {
        Archive::Bare => {
            std::fs::rename(file, dest.join(exe(src.files[0]))).map_err(|e| err(&e))?;
            found = 1;
        }
        Archive::Zip => {
            let f = std::fs::File::open(file).map_err(|e| err(&e))?;
            let mut zip = zip::ZipArchive::new(std::io::BufReader::new(f)).map_err(|e| err(&e))?;
            for i in 0..zip.len() {
                let mut entry = zip.by_index(i).map_err(|e| err(&e))?;
                if !entry.is_file() {
                    continue;
                }
                let Some(name) = entry.enclosed_name().and_then(|p| wanted(&p)) else {
                    continue;
                };
                let mut out = std::fs::File::create(dest.join(&name)).map_err(|e| err(&e))?;
                std::io::copy(&mut entry, &mut out).map_err(|e| err(&e))?;
                found += 1;
            }
        }
        Archive::TarXz => {
            let f = std::fs::File::open(file).map_err(|e| err(&e))?;
            let xz = lzma_rust2::XzReader::new(std::io::BufReader::new(f), true);
            let mut tar = tar::Archive::new(xz);
            for entry in tar.entries().map_err(|e| err(&e))? {
                let mut entry = entry.map_err(|e| err(&e))?;
                if !entry.header().entry_type().is_file() {
                    continue;
                }
                let Some(name) = entry.path().ok().and_then(|p| wanted(&p)) else {
                    continue;
                };
                let mut out = std::fs::File::create(dest.join(&name)).map_err(|e| err(&e))?;
                std::io::copy(&mut entry, &mut out).map_err(|e| err(&e))?;
                found += 1;
            }
        }
    }
    if found < src.files.len() {
        return Err(format!(
            "{}: {} not found in it",
            src.url,
            src.files.join(", ")
        ));
    }
    Ok(())
}

fn make_runnable(path: &Path) -> Result<(), String> {
    #[cfg(windows)]
    let _ = path;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755))
            .map_err(|e| format!("{}: {e}", path.display()))?;
    }
    #[cfg(target_os = "macos")]
    {
        // Apple Silicon only runs signed code; an ad-hoc signature will do
        // for a binary that isn't signed already.
        let signed = std::process::Command::new("codesign")
            .arg("--verify")
            .arg(path)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok_and(|s| s.success());
        if !signed {
            let _ = std::process::Command::new("codesign")
                .args(["--force", "--sign", "-"])
                .arg(path)
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status();
        }
    }
    Ok(())
}

/// Let a fetched yt-dlp update itself, at most once a day, in the
/// background. Installed copies belong to their package manager.
pub fn update_ytdlp_in_background() {
    let bin = dir().join(exe("yt-dlp"));
    if find(Tool::YtDlp).as_deref() != Some(bin.as_path()) {
        return;
    }
    let stamp = dir().join(".yt-dlp-update");
    let fresh = std::fs::metadata(&stamp)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| SystemTime::now().duration_since(t).ok())
        .is_some_and(|age| age < Duration::from_secs(24 * 3600));
    if fresh || std::fs::write(&stamp, b"").is_err() {
        return;
    }
    std::thread::spawn(move || {
        let mut cmd = std::process::Command::new(&bin);
        crate::media::prepare(&mut cmd, &bin);
        let out = cmd
            .args(["--ignore-config", "--no-colors", "-U"])
            .stdin(std::process::Stdio::null())
            .output();
        let msg = match out {
            Ok(o) => String::from_utf8_lossy(&o.stdout)
                .lines()
                .chain(String::from_utf8_lossy(&o.stderr).lines())
                .rfind(|l| !l.trim().is_empty())
                .unwrap_or("done")
                .to_string(),
            Err(e) => e.to_string(),
        };
        crate::log::log("tools", format!("yt-dlp -U: {msg}"));
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn platforms() {
        for (os, arch) in [
            ("linux", "x86_64"),
            ("linux", "aarch64"),
            ("macos", "x86_64"),
            ("macos", "aarch64"),
            ("windows", "x86_64"),
            ("windows", "aarch64"),
        ] {
            for tool in [Tool::YtDlp, Tool::Ffmpeg, Tool::Deno] {
                let srcs = sources_for(tool, os, arch, false);
                assert!(!srcs.is_empty(), "{tool:?} on {os}/{arch}");
                let files: Vec<_> = srcs.iter().flat_map(|s| s.files.iter()).collect();
                assert!(files.contains(&&tool.name()), "{tool:?} on {os}/{arch}");
            }
            let ff = sources_for(Tool::Ffmpeg, os, arch, false);
            assert!(ff.iter().any(|s| s.files.contains(&"ffprobe")));
        }
        assert!(sources_for(Tool::YtDlp, "freebsd", "x86_64", false).is_empty());
        assert!(sources_for(Tool::Deno, "linux", "x86_64", true).is_empty());
        assert_eq!(
            sources_for(Tool::YtDlp, "linux", "x86_64", true)[0].url,
            format!("{YTDLP}yt-dlp_musllinux")
        );
    }

    #[test]
    fn sums() {
        let list = "e51e69e89489f199530f13d32a4342da3ddb2cf34842f364603a066db05afbf3  yt-dlp\n\
                    767ba9dce7052aa196f8b12652f433d76de128ba168966fa84b6394d5e6c6795  yt-dlp_linux\n\
                    f34053415136e55073dd64ba5326d161ecc23ce0e98c081d8752867b37809450  yt-dlp_linux.zip\n";
        assert_eq!(
            sum_for(list, "yt-dlp_linux").as_deref(),
            Some("767ba9dce7052aa196f8b12652f433d76de128ba168966fa84b6394d5e6c6795")
        );
        assert_eq!(sum_for(list, "yt-dlp_macos"), None);
        let ps = "\nAlgorithm : SHA256\nHash      : CBA84B26E0DF5AE7F74FC66B86A95B866872A86292EBD84FA1E959927EDD93BF\nPath      : C:\\a\\deno.zip\n";
        assert_eq!(
            first_sum(ps).as_deref(),
            Some("cba84b26e0df5ae7f74fc66b86a95b866872a86292ebd84fa1e959927edd93bf")
        );
        assert_eq!(
            first_sum(
                "c8ed4c4e6978a03c485edbfe4e0a5dc2380f8a30bba5150531b31b094492d924  ffmpeg.zip"
            )
            .as_deref(),
            Some("c8ed4c4e6978a03c485edbfe4e0a5dc2380f8a30bba5150531b31b094492d924")
        );
    }
}
