//! Video sites and HLS/DASH streams, through yt-dlp.
//!
//! Pages like YouTube don't serve one file: the player stitches together
//! many small requests, often separate audio and video, behind signed URLs.
//! yt-dlp knows how to resolve those, so Surge runs it as a child process,
//! parses machine-readable progress lines, and treats it like any other
//! download (pause = kill, resume = rerun with `--continue`).
//!
//! Limits: yt-dlp traffic doesn't pass through Surge's token buckets, so the
//! effective limit (per-file, capped by the global one) is handed to
//! `--limit-rate`, and the process is restarted when either changes.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio_util::sync::CancellationToken;

use crate::{Inner, Job, Segment, Status};

/// What a page offers, for the quality picker.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MediaInfo {
    pub title: String,
    pub duration: Option<f64>,
    pub thumbnail: Option<String>,
    /// Extractor name, e.g. "youtube".
    pub site: Option<String>,
    pub choices: Vec<MediaChoice>,
    /// Audio languages, original first; empty when there's only one.
    #[serde(default)]
    pub audio: Vec<AudioTrack>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AudioTrack {
    /// yt-dlp `language`, e.g. "en-US".
    pub lang: String,
    pub label: String,
    /// The language the video was made in (YouTube marks auto-dubs).
    pub original: bool,
    /// Machine-translated (YouTube auto-dubbing).
    pub dubbed: bool,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MediaChoice {
    /// yt-dlp format selector.
    pub format: String,
    pub label: String,
    /// Approximate size in bytes.
    pub size: Option<u64>,
    #[serde(default)]
    pub audio_only: bool,
    /// Frame size of the largest video in this choice.
    #[serde(default)]
    pub width: Option<u64>,
    #[serde(default)]
    pub height: Option<u64>,
}

/// Default selector: best video + best audio, else the best single file.
pub const BEST: &str = "bv*+ba/b";
/// Audio only, preferring AAC (M4A).
pub const AUDIO: &str = "ba[ext=m4a]/ba";
/// The original audio language first (not a YouTube auto-dub), then
/// resolution, then widely playable codecs (H.264/AAC in MP4).
const SORT: &str = "lang,res,vcodec:h264,acodec:aac";

/// Directories to look for helper tools in, besides `PATH`. The browser
/// launches the host (and so Surge) without the user's shell PATH additions
/// (mise shims, Homebrew on macOS), so also look in the usual install dirs
/// and next to our own executable (where Windows users can drop yt-dlp.exe).
fn tool_dirs() -> Vec<PathBuf> {
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
    dirs
}

fn find_tool(name: &str) -> Option<PathBuf> {
    let file = format!("{name}{}", std::env::consts::EXE_SUFFIX);
    tool_dirs()
        .into_iter()
        .map(|d| d.join(&file))
        .find(|p| p.is_file())
}

/// Locate yt-dlp (`SURGE_YTDLP` overrides).
pub fn ytdlp() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os("SURGE_YTDLP") {
        return Some(p.into());
    }
    find_tool("yt-dlp")
}

/// `PATH` for yt-dlp: its own dir and the usual install dirs first, so it
/// finds ffmpeg and a JS runtime (deno) even when the browser started us.
fn child_path(bin: &Path) -> std::ffi::OsString {
    let mut dirs: Vec<PathBuf> = bin.parent().map(PathBuf::from).into_iter().collect();
    dirs.extend(tool_dirs());
    dirs.dedup();
    std::env::join_paths(dirs).unwrap_or_default()
}

/// Settings shared by every yt-dlp we start.
fn prepare(cmd: &mut std::process::Command, bin: &Path) {
    cmd.env("PATH", child_path(bin));
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // CREATE_NO_WINDOW: no console flashing up for each probe/download.
        cmd.creation_flags(0x0800_0000);
    }
    #[cfg(unix)]
    {
        // Own process group, so pausing also stops the ffmpeg it starts.
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
}

#[cfg(windows)]
const MISSING: &str = "yt-dlp is not installed (needed for video sites and HLS/DASH streams). \
     Install it with `winget install yt-dlp.yt-dlp Gyan.FFmpeg DenoLand.Deno`, \
     or put yt-dlp.exe next to surge.exe";
#[cfg(target_os = "macos")]
const MISSING: &str = "yt-dlp is not installed (needed for video sites and HLS/DASH streams). \
     Install it with `brew install yt-dlp ffmpeg deno`";
#[cfg(not(any(windows, target_os = "macos")))]
const MISSING: &str = "yt-dlp is not installed (needed for video sites and HLS/DASH streams). \
     Install yt-dlp, ffmpeg and deno from your package manager";

/// ffmpeg merges separate video and audio. Without it, only formats that
/// already contain both can be downloaded.
fn has_ffmpeg() -> bool {
    find_tool("ffmpeg").is_some()
}

/// Rewrite a selector for a machine without ffmpeg: drop the video+audio
/// pairs and keep the single-file alternatives.
fn without_merge(format: &str) -> String {
    let alts: Vec<&str> = format.split('/').filter(|a| !a.contains('+')).collect();
    if alts.is_empty() {
        "b".into()
    } else {
        alts.join("/")
    }
}

/// A raw HLS/DASH manifest rather than a page yt-dlp has an extractor for.
pub fn is_stream_url(url: &str) -> bool {
    let path = reqwest::Url::parse(url)
        .map(|u| u.path().to_ascii_lowercase())
        .unwrap_or_default();
    path.ends_with(".m3u8") || path.ends_with(".mpd")
}

/// `--add-header` arguments. Extractor sites pick their own client
/// User-Agent (overriding it breaks extraction), so they only get cookies
/// and the referrer; raw manifests on a CDN often need all of them.
fn header_args(url: &str, headers: &[(String, String)]) -> Vec<String> {
    let raw = is_stream_url(url);
    let mut args = vec![];
    for (k, v) in headers {
        let keep = if raw {
            !k.eq_ignore_ascii_case("accept-encoding")
        } else {
            k.eq_ignore_ascii_case("cookie") || k.eq_ignore_ascii_case("referer")
        };
        if keep {
            args.push("--add-header".into());
            args.push(format!("{k}:{v}"));
        }
    }
    args
}

fn base_args() -> Vec<String> {
    [
        "--ignore-config",
        "--no-playlist",
        "--no-colors",
        "--no-warnings",
    ]
    .map(String::from)
    .to_vec()
}

/// A page that lists several videos instead of playing one.
fn playlist_error(v: &Value) -> String {
    match v["entries"].as_array().map_or(0, Vec::len) {
        0 => "no downloadable video found on this page".into(),
        n => format!("this page lists {n} videos; open the one you want"),
    }
}

/// Turn yt-dlp's stderr into one readable message.
fn error_from(lines: &[String]) -> Option<String> {
    let line = lines
        .iter()
        .rev()
        .find(|l| l.starts_with("ERROR:"))
        .or(lines.last())?;
    let msg = line.trim_start_matches("ERROR:").trim();
    // "[generic] Unsupported URL: …" → say what it means.
    if msg.contains("Unsupported URL") {
        return Some("no downloadable video found on this page".into());
    }
    Some(msg.chars().take(300).collect())
}

/// List the formats a page offers (blocking; runs `yt-dlp -J`).
pub fn probe(url: &str, headers: &[(String, String)]) -> Result<MediaInfo, String> {
    let bin = ytdlp().ok_or(MISSING)?;
    let mut cmd = std::process::Command::new(&bin);
    prepare(&mut cmd, &bin);
    cmd.args(base_args())
        .args(header_args(url, headers))
        // A list page (channel, playlist, creator) would otherwise have every
        // video in it read, which takes minutes: list it flat and say so.
        .args(["-J", "--flat-playlist", "--", url])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let started = Instant::now();
    crate::log::log(
        "yt-dlp",
        format!(
            "probe {} with {} (cookies: {})",
            crate::log::url(url),
            bin.display(),
            if headers
                .iter()
                .any(|(k, _)| k.eq_ignore_ascii_case("cookie"))
            {
                "yes"
            } else {
                "no"
            }
        ),
    );
    let mut child = cmd.spawn().map_err(|e| {
        crate::log::log("yt-dlp", format!("could not start: {e}"));
        format!("running yt-dlp: {e}")
    })?;

    // Read both pipes on threads (so neither fills up) while we watch the clock.
    let mut out = child.stdout.take().unwrap();
    let mut err = child.stderr.take().unwrap();
    let out_t = std::thread::spawn(move || {
        let mut b = vec![];
        let _ = std::io::Read::read_to_end(&mut out, &mut b);
        b
    });
    let err_t = std::thread::spawn(move || {
        let mut s = String::new();
        let _ = std::io::Read::read_to_string(&mut err, &mut s);
        s
    });
    let deadline = Instant::now() + Duration::from_secs(90);
    let status = loop {
        if let Some(s) = child.try_wait().map_err(|e| e.to_string())? {
            break s;
        }
        if Instant::now() > deadline {
            let _ = child.kill();
            let _ = child.wait();
            crate::log::log("yt-dlp", "probe timed out after 90s, killed");
            return Err("yt-dlp took too long to read the page".into());
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    let stdout = out_t.join().unwrap_or_default();
    let stderr = err_t.join().unwrap_or_default();
    let tail: Vec<&str> = stderr.lines().rev().take(8).collect();
    crate::log::log(
        "yt-dlp",
        format!(
            "probe exited {status} after {:.1}s, {} bytes of JSON{}",
            started.elapsed().as_secs_f64(),
            stdout.len(),
            if tail.is_empty() {
                String::new()
            } else {
                format!(
                    "\nstderr:\n{}",
                    tail.into_iter().rev().collect::<Vec<_>>().join("\n")
                )
            }
        ),
    );
    if !status.success() {
        let lines: Vec<String> = stderr.lines().map(String::from).collect();
        return Err(error_from(&lines).unwrap_or_else(|| format!("yt-dlp failed ({status})")));
    }
    let v: Value = serde_json::from_slice(&stdout).map_err(|e| format!("yt-dlp output: {e}"))?;
    if v["_type"] == "playlist" {
        return Err(playlist_error(&v));
    }
    Ok(info_from(&v))
}

/// "1080p" for both 1920×1080 and a vertical 1080×1920 (Shorts, Reels).
fn quality_class(f: &Value) -> Option<u64> {
    let note = f["format_note"].as_str().unwrap_or_default();
    if let Some(n) = note
        .split(['p', ' '])
        .next()
        .filter(|n| note.len() > n.len() && note.as_bytes()[n.len()] == b'p')
        .and_then(|n| n.parse::<u64>().ok())
    {
        return Some(n);
    }
    let h = f["height"].as_u64()?;
    Some(f["width"].as_u64().map_or(h, |w| w.min(h)))
}

/// Human name of an audio track: "English (US)" from yt-dlp's format note
/// ("English (US) original (default), medium" / "Korean, low").
fn track_name(f: &Value, lang: &str) -> String {
    let note = f["format_note"].as_str().unwrap_or_default();
    let mut name = note
        .split(',')
        .next()
        .unwrap_or_default()
        .trim()
        .to_string();
    for junk in [
        " original (default)",
        " (default)",
        " - original (original)",
        " - dubbed-auto",
        " original",
    ] {
        if let Some(stripped) = name.strip_suffix(junk) {
            name = stripped.trim().to_string();
        }
    }
    if name.is_empty() || name.ends_with('p') && name[..name.len() - 1].parse::<u32>().is_ok() {
        lang.to_string()
    } else {
        name
    }
}

fn info_from(v: &Value) -> MediaInfo {
    let duration = v["duration"].as_f64();
    let formats: Vec<&Value> = match v["formats"].as_array() {
        Some(a) if !a.is_empty() => a.iter().collect(),
        _ => vec![v],
    };
    let size = |f: &Value| -> Option<u64> {
        f["filesize"]
            .as_u64()
            .or_else(|| f["filesize_approx"].as_u64())
            .or_else(|| {
                let tbr = f["tbr"].as_f64()?;
                Some((tbr * duration? * 125.0) as u64)
            })
    };
    let has = |f: &Value, k: &str| f[k].as_str().is_some_and(|c| c != "none");
    let audio_only = |f: &&&Value| has(f, "acodec") && !has(f, "vcodec");
    let original = |f: &Value| {
        f["language_preference"].as_i64().is_some_and(|p| p >= 10)
            || f["format_note"]
                .as_str()
                .is_some_and(|n| n.contains("original"))
    };

    // Audio tracks by language. Direct (https) formats have the nicer
    // names, so they win over HLS ones.
    let mut tracks: BTreeMap<String, AudioTrack> = BTreeMap::new();
    for f in formats.iter().filter(audio_only) {
        let Some(lang) = f["language"].as_str().filter(|l| !l.is_empty()) else {
            continue;
        };
        let note = f["format_note"].as_str().unwrap_or_default();
        let direct = f["protocol"].as_str().is_none_or(|p| p.starts_with("http"));
        let t = tracks
            .entry(lang.to_string())
            .or_insert_with(|| AudioTrack {
                lang: lang.to_string(),
                label: track_name(f, lang),
                original: false,
                dubbed: false,
            });
        if direct {
            t.label = track_name(f, lang);
        }
        t.original |= original(f);
        t.dubbed |= note.contains("dubbed");
    }
    let mut audio: Vec<AudioTrack> = tracks.into_values().collect();
    audio.sort_by(|a, b| b.original.cmp(&a.original).then(a.label.cmp(&b.label)));
    if audio.len() < 2 {
        audio.clear();
    }

    let best_audio = formats
        .iter()
        .filter(audio_only)
        .filter(|f| audio.is_empty() || original(f))
        .filter_map(|f| size(f))
        .max();

    // quality class → (max height, max fps, size of the largest video incl. audio,
    // frame size of the largest video)
    type Class = (u64, u64, Option<u64>, Option<(u64, u64)>);
    let mut classes: BTreeMap<u64, Class> = BTreeMap::new();
    for f in &formats {
        // HLS masters without a CODECS attribute leave vcodec null; a height
        // still makes it a video.
        let unknown_codec = f["vcodec"].is_null() && f["height"].as_u64().is_some();
        if !has(f, "vcodec") && !unknown_codec {
            continue;
        }
        let (Some(class), Some(h)) = (quality_class(f), f["height"].as_u64()) else {
            continue;
        };
        let fps = f["fps"].as_f64().unwrap_or(0.0).round() as u64;
        let total = size(f).map(|s| {
            if has(f, "acodec") {
                s
            } else {
                s + best_audio.unwrap_or(0)
            }
        });
        let e = classes.entry(class).or_insert((0, 0, None, None));
        e.0 = e.0.max(h);
        e.1 = e.1.max(fps);
        e.2 = e.2.max(total);
        if let Some(w) = f["width"].as_u64()
            && e.3.is_none_or(|(ow, oh)| w * h > ow * oh)
        {
            e.3 = Some((w, h));
        }
    }

    let top = classes.values().next_back();
    let mut choices = vec![MediaChoice {
        format: BEST.into(),
        label: "Best quality".into(),
        size: top.and_then(|e| e.2),
        audio_only: false,
        width: top.and_then(|e| e.3).map(|d| d.0),
        height: top.and_then(|e| e.3.map(|d| d.1).or(Some(e.0))),
    }];
    for (class, (h, fps, sz, dims)) in classes.iter().rev() {
        let label = if *fps > 30 {
            format!("{class}p{fps}")
        } else {
            format!("{class}p")
        };
        choices.push(MediaChoice {
            format: format!("bv*[height<={h}]+ba/b[height<={h}]/b"),
            label,
            size: *sz,
            audio_only: false,
            width: dims.map(|d| d.0),
            height: Some(dims.map_or(*h, |d| d.1)),
        });
    }
    if best_audio.is_some() {
        choices.push(MediaChoice {
            format: AUDIO.into(),
            label: "Audio only".into(),
            size: best_audio,
            audio_only: true,
            ..Default::default()
        });
    }

    MediaInfo {
        title: v["title"].as_str().unwrap_or_default().to_string(),
        duration,
        thumbnail: v["thumbnail"].as_str().map(String::from),
        site: v["extractor_key"]
            .as_str()
            .or(v["extractor"].as_str())
            .map(String::from),
        choices,
        audio,
    }
}

/// Prefer `lang`'s audio track wherever the selector picks audio, falling
/// back to the original selector when that track doesn't exist.
fn with_lang(format: &str, lang: &str) -> String {
    let lang = lang.replace(['[', ']', '"', '\'', '/', '+', ','], "");
    let preferred: Vec<String> = format
        .split('/')
        .filter(|alt| alt.split('+').any(|p| p.starts_with("ba")))
        .map(|alt| {
            alt.split('+')
                .map(|p| match p.strip_prefix("ba") {
                    Some(rest) => format!("ba[language={lang}]{rest}"),
                    None => p.to_string(),
                })
                .collect::<Vec<_>>()
                .join("+")
        })
        .collect();
    if preferred.is_empty() {
        format.to_string()
    } else {
        format!("{}/{format}", preferred.join("/"))
    }
}

/// The file type a media download ends up as: "audio" for an audio
/// container or an audio-only format, else "video".
pub fn category(format: Option<&str>, container: Option<&str>) -> &'static str {
    let format = format.unwrap_or(BEST);
    if container.is_some_and(is_audio_type) || (!format.contains("bv") && format.starts_with("ba"))
    {
        "audio"
    } else {
        "video"
    }
}

fn is_audio_type(c: &str) -> bool {
    matches!(c, "mp3" | "m4a" | "opus" | "flac" | "wav")
}

/// yt-dlp arguments for the output container.
fn container_args(container: Option<&str>, audio_only: bool) -> Vec<String> {
    let c = container.unwrap_or(if audio_only { "m4a" } else { "mp4" });
    let args: &[&str] = match c {
        c if is_audio_type(c) => &["-x", "--audio-format", c],
        _ if audio_only => &["-x", "--audio-format", "m4a"],
        "mkv" => &["--merge-output-format", "mkv", "--remux-video", "mkv"],
        "original" => &["--merge-output-format", "mp4/mkv"],
        // MP4 plays everywhere; anything that can't go in MP4 ends up in MKV.
        _ => &["--merge-output-format", "mp4/mkv", "--remux-video", "mp4"],
    };
    args.iter().map(|s| s.to_string()).collect()
}

/// Where yt-dlp keeps fragments and `.part` files for one download.
pub(crate) fn temp_dir(inner: &Inner, id: u64) -> PathBuf {
    inner.data_dir.join("media").join(id.to_string())
}

/// Smallest non-zero of the two limits (0 = unlimited).
pub(crate) fn effective_limit(per_file: u64, global: u64) -> u64 {
    match (per_file, global) {
        (0, g) => g,
        (p, 0) => p,
        (p, g) => p.min(g),
    }
}

/// A filename the user chose, as a yt-dlp output template (`stem.%(ext)s`):
/// the container is only known once the formats are picked.
fn output_template(filename: &str) -> String {
    if filename.is_empty() {
        return "%(title).180B.%(ext)s".into();
    }
    const EXTS: [&str; 16] = [
        ".mp4", ".mkv", ".webm", ".mov", ".m4a", ".mp3", ".opus", ".ogg", ".ts", ".flv", ".m3u8",
        ".mpd", ".m4v", ".avi", ".3gp", ".flac",
    ];
    // Strip repeatedly: "clip.m3u8.mp4" → "clip".
    let mut stem = filename;
    while let Some(e) = EXTS
        .iter()
        .find(|e| stem.len() > e.len() && stem.to_ascii_lowercase().ends_with(*e))
    {
        stem = &stem[..stem.len() - e.len()];
    }
    format!("{}.%(ext)s", stem.replace('%', "%%"))
}

enum Line {
    Out(String),
    Err(String),
}

pub(crate) async fn drive(
    inner: &Arc<Inner>,
    job: &Arc<Job>,
    token: &CancellationToken,
) -> Result<(), String> {
    let bin = ytdlp().ok_or(MISSING)?;
    let settings = inner.settings.lock().unwrap().clone();
    let (id, url, headers, filename, dir, conns, format, lang, container, limit) = {
        let mut rec = job.rec.lock().unwrap();
        if rec.auto_dir && rec.path.is_none() {
            // Video or audio folder, by what this download turns into.
            let cat = category(rec.format.as_deref(), rec.container.as_deref());
            rec.dir = settings.dir_for(Some(cat));
        }
        rec.status = Status::Probing;
        rec.note = Some("Reading video info".into());
        rec.resumable = true;
        // A file job that turned out to be a page: its byte segments mean nothing here.
        rec.segments.clear();
        (
            rec.id,
            rec.url.clone(),
            rec.headers.clone(),
            rec.filename.clone(),
            rec.dir.clone(),
            rec.connections,
            rec.format.clone().unwrap_or_else(|| BEST.into()),
            rec.audio_lang.clone(),
            rec.container.clone(),
            effective_limit(rec.speed_limit, inner.global_limit.rate()),
        )
    };
    inner.mark_dirty();
    let tmp = temp_dir(inner, id);
    std::fs::create_dir_all(&tmp).map_err(|e| format!("{}: {e}", tmp.display()))?;
    std::fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;

    let mut args = base_args();
    args.extend(header_args(&url, &headers));
    args.extend(
        [
            "--newline",
            "--progress",
            "--continue",
            "--no-mtime",
            "-S",
            SORT,
            "--print",
            "before_dl:SURGEINFO %(format_id)s|%(filesize,filesize_approx)s|%(filename)s",
            "--print",
            "after_move:SURGEFILE %(filepath)s",
            "--progress-template",
            "download:SURGE %(info.format_id)s|%(progress.downloaded_bytes)s|%(progress.total_bytes,progress.total_bytes_estimate)s|%(progress.fragment_index)s|%(progress.fragment_count)s|%(info.vcodec)s",
            "--progress-template",
            "postprocess:SURGEPP %(progress.postprocessor)s|%(progress.status)s",
        ]
        .map(String::from),
    );
    // An audio type on a video quality: just fetch the audio track.
    let audio_type = container.as_deref().is_some_and(is_audio_type);
    let format = if audio_type {
        AUDIO.to_string()
    } else {
        format
    };
    let audio_only = audio_type || (!format.contains("bv") && format.starts_with("ba"));
    let merge = has_ffmpeg();
    let format = if merge {
        format
    } else {
        without_merge(&format)
    };
    if merge {
        args.extend(container_args(container.as_deref(), audio_only));
    }
    args.push("-f".into());
    args.push(match &lang {
        Some(l) => with_lang(&format, l),
        None => format,
    });
    args.push("-N".into());
    args.push(conns.clamp(1, 32).to_string());
    if limit > 0 {
        args.push("--limit-rate".into());
        args.push(limit.to_string());
    }
    args.push("-P".into());
    args.push(format!("home:{}", dir.display()));
    args.push("-P".into());
    args.push(format!("temp:{}", tmp.display()));
    args.push("-o".into());
    args.push(output_template(&filename));
    args.push("--".into());
    args.push(url.clone());

    let mut std_cmd = std::process::Command::new(&bin);
    prepare(&mut std_cmd, &bin);
    let mut cmd = tokio::process::Command::from(std_cmd);
    cmd.args(&args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let mut child = cmd.spawn().map_err(|e| format!("running yt-dlp: {e}"))?;
    *job.pid.lock().unwrap() = child.id();

    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let out = child.stdout.take().unwrap();
    let err = child.stderr.take().unwrap();
    let tx2 = tx.clone();
    tokio::spawn(async move {
        let mut lines = BufReader::new(out).lines();
        while let Ok(Some(l)) = lines.next_line().await {
            let _ = tx.send(Line::Out(l));
        }
    });
    tokio::spawn(async move {
        let mut lines = BufReader::new(err).lines();
        while let Ok(Some(l)) = lines.next_line().await {
            let _ = tx2.send(Line::Err(l));
        }
    });

    // One entry per stream yt-dlp fetches (video, then audio), in order.
    let mut streams: Vec<Stream> = vec![];
    let mut expected: Option<u64> = None;
    let mut final_path: Option<PathBuf> = None;
    let mut errors: Vec<String> = vec![];

    loop {
        let line = tokio::select! {
            _ = token.cancelled() => {
                job.kill_child();
                let _ = child.wait().await;
                return Err("cancelled".into());
            }
            l = rx.recv() => l,
        };
        let Some(line) = line else { break };
        let text = match line {
            Line::Out(t) | Line::Err(t) => t,
        };
        let mut rec = job.rec.lock().unwrap();
        if let Some(rest) = text.strip_prefix("SURGEINFO ") {
            let mut it = rest.splitn(3, '|');
            let _fid = it.next();
            expected = num(it.next());
            if let Some(p) = it.next().filter(|p| !p.is_empty()) {
                set_path(&mut rec, Path::new(p));
            }
            rec.total = expected.or(rec.total);
            rec.status = Status::Downloading;
            rec.note = None;
            rec.active_connections = 0;
        } else if let Some(rest) = text.strip_prefix("SURGE ") {
            let mut it = rest.split('|');
            let fid = it.next().unwrap_or_default().to_string();
            let done = num(it.next()).unwrap_or(0);
            let total = num(it.next());
            let frag = num(it.next());
            let frags = num(it.next());
            let video = it
                .next()
                .is_some_and(|c| !c.is_empty() && c != "none" && c != "NA");
            let i = match streams.iter().position(|s| s.id == fid) {
                Some(i) => i,
                None => {
                    streams.push(Stream {
                        id: fid,
                        video,
                        ..Default::default()
                    });
                    streams.len() - 1
                }
            };
            let s = &mut streams[i];
            s.done = done;
            // Fragmented streams only estimate their size; never let it shrink
            // below what has already arrived, so progress doesn't run backwards.
            s.total = total.map(|t| t.max(done)).or(s.total);
            s.frags = frags.map(|n| (frag.unwrap_or(0), n));

            rec.downloaded = streams.iter().map(|s| s.done).sum();
            let known: u64 = streams.iter().map(|s| s.total.unwrap_or(s.done)).sum();
            // The total only grows while downloading (estimates wobble).
            rec.total = Some(expected.unwrap_or(0).max(known).max(rec.total.unwrap_or(0)));
            rec.segments = stream_segments(&streams, i, rec.total.unwrap_or(0));
            rec.status = Status::Downloading;
            rec.note = Some(stream_note(&streams, i, conns));
            rec.active_connections = match streams[i].frags {
                Some(_) => conns,
                None => 1,
            };
        } else if let Some(rest) = text.strip_prefix("SURGEPP ") {
            let (pp, state) = rest.split_once('|').unwrap_or((rest, ""));
            rec.note = (state == "started").then(|| match pp {
                "Merger" => "Merging audio and video".into(),
                "MoveFiles" => "Moving into place".into(),
                "VideoRemuxer" => "Converting the container".into(),
                "ExtractAudio" => "Converting the audio".into(),
                "FixupM3u8" | "FixupM4a" | "FixupDuplicateMoov" => "Fixing up the container".into(),
                other => format!("Processing ({other})"),
            });
            rec.active_connections = 0;
        } else if let Some(p) = text.strip_prefix("SURGEFILE ") {
            final_path = Some(PathBuf::from(p));
        } else if !text.trim().is_empty() && !text.starts_with('[') {
            errors.push(text);
            if errors.len() > 40 {
                errors.remove(0);
            }
        }
    }

    let status = child.wait().await.map_err(|e| e.to_string())?;
    job.pid.lock().unwrap().take();
    if token.is_cancelled() {
        return Err("cancelled".into());
    }
    if !status.success() {
        return Err(error_from(&errors).unwrap_or_else(|| format!("yt-dlp failed ({status})")));
    }
    let path = final_path.ok_or("yt-dlp finished without producing a file")?;
    let size = std::fs::metadata(&path).map(|m| m.len()).ok();
    {
        let mut rec = job.rec.lock().unwrap();
        set_path(&mut rec, &path);
        if let Some(size) = size {
            rec.total = Some(size);
            rec.downloaded = size;
        }
        rec.note = None;
    }
    let _ = std::fs::remove_dir_all(&tmp);
    Ok(())
}

#[derive(Default)]
struct Stream {
    id: String,
    video: bool,
    done: u64,
    total: Option<u64>,
    /// (current fragment, fragment count) for HLS/DASH.
    frags: Option<(u64, u64)>,
}

/// yt-dlp prints numbers as "123", "123.0" or "NA".
fn num(s: Option<&str>) -> Option<u64> {
    s?.parse::<f64>()
        .ok()
        .filter(|n| *n >= 0.0)
        .map(|n| n as u64)
}

/// The bar shows each stream as one block (video, then audio).
fn stream_segments(streams: &[Stream], current: usize, total: u64) -> Vec<Segment> {
    let mut start = 0;
    let mut out = vec![];
    for (i, s) in streams.iter().enumerate() {
        let len = s.total.unwrap_or(s.done).max(s.done);
        let end = (start + len).min(total.max(start));
        out.push(Segment {
            start,
            end,
            pos: (start + s.done).min(end),
            owned: i == current,
            ..Default::default()
        });
        start = end;
    }
    out
}

/// "Video (1 of 2) · fragment 12/40, 8 at a time".
fn stream_note(streams: &[Stream], current: usize, conns: usize) -> String {
    let s = &streams[current];
    let what = if s.video { "Video" } else { "Audio" };
    let mut note = what.to_string();
    if streams.len() > 1 || current > 0 {
        note = format!("{what} (stream {})", current + 1);
    }
    match s.frags {
        Some((i, n)) if n > 0 => note.push_str(&format!(
            " · fragment {}/{n}, {} at a time",
            i.min(n),
            conns.min(n as usize)
        )),
        _ => note.push_str(" · 1 connection"),
    }
    note
}

fn set_path(rec: &mut crate::Download, path: &Path) {
    if let Some(name) = path.file_name() {
        rec.filename = name.to_string_lossy().into_owned();
    }
    rec.path = Some(path.to_path_buf());
}

/// Kill yt-dlp and the ffmpeg it may have started. Synchronous: the app
/// may exit right after.
pub(crate) fn kill_group(pid: u32) {
    #[cfg(unix)]
    unsafe {
        libc::killpg(pid as libc::pid_t, libc::SIGKILL);
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        let _ = std::process::Command::new("taskkill")
            .args(["/F", "/T", "/PID", &pid.to_string()])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .creation_flags(0x0800_0000)
            .status();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn templates() {
        assert_eq!(output_template("My Video.mp4"), "My Video.%(ext)s");
        assert_eq!(output_template("100% real"), "100%% real.%(ext)s");
        assert_eq!(output_template(""), "%(title).180B.%(ext)s");
        assert_eq!(
            output_template("Clip - YouTube.m3u8.mkv"),
            "Clip - YouTube.%(ext)s"
        );
        assert_eq!(output_template(".mp4"), ".mp4.%(ext)s");
    }

    #[test]
    fn audio_language() {
        assert_eq!(with_lang("bv*+ba/b", "ko"), "bv*+ba[language=ko]/bv*+ba/b");
        assert_eq!(
            with_lang(AUDIO, "ja"),
            "ba[language=ja][ext=m4a]/ba[language=ja]/ba[ext=m4a]/ba"
        );
        assert_eq!(with_lang("b", "ko"), "b");
        assert_eq!(
            container_args(None, false)[..2],
            ["--merge-output-format", "mp4/mkv"]
        );
        assert_eq!(
            container_args(Some("mp3"), false),
            ["-x", "--audio-format", "mp3"]
        );
    }

    #[test]
    fn no_ffmpeg() {
        assert_eq!(
            without_merge("bv*[height<=720]+ba/b[height<=720]/b"),
            "b[height<=720]/b"
        );
        assert_eq!(without_merge(BEST), "b");
        assert_eq!(without_merge("bv*+ba"), "b");
    }

    #[test]
    fn limits() {
        assert_eq!(effective_limit(0, 0), 0);
        assert_eq!(effective_limit(5, 0), 5);
        assert_eq!(effective_limit(0, 7), 7);
        assert_eq!(effective_limit(9, 7), 7);
    }

    #[test]
    fn streams_and_headers() {
        assert!(is_stream_url("https://cdn/x/master.M3U8?tok=1"));
        assert!(!is_stream_url("https://www.youtube.com/watch?v=x"));
        let h = vec![
            ("User-Agent".to_string(), "UA".to_string()),
            ("Cookie".to_string(), "a=1".to_string()),
        ];
        assert_eq!(
            header_args("https://youtube.com/watch?v=x", &h),
            ["--add-header", "Cookie:a=1"]
        );
        assert_eq!(header_args("https://cdn/a.m3u8", &h).len(), 4);
    }

    #[test]
    fn list_pages() {
        let v = serde_json::json!({"_type": "playlist", "entries": [{"url": "a"}, {"url": "b"}]});
        assert_eq!(
            playlist_error(&v),
            "this page lists 2 videos; open the one you want"
        );
    }

    #[test]
    fn hls_without_codecs() {
        // A master playlist with RESOLUTION but no CODECS: yt-dlp leaves vcodec null.
        let v: Value = serde_json::json!({
            "title": "master", "duration": 6.0, "extractor_key": "Generic",
            "formats": [
                {"format_id": "900", "vcodec": null, "acodec": null, "height": 360, "width": 640, "tbr": 900.0},
                {"format_id": "2000", "vcodec": null, "acodec": null, "height": 720, "width": 1280, "tbr": 2000.0}
            ]
        });
        let choices = info_from(&v).choices;
        let labels: Vec<_> = choices.iter().map(|c| c.label.as_str()).collect();
        assert_eq!(labels, ["Best quality", "720p", "360p"]);
        // The frame size goes along, for "720p · 1280×720" in the dialog.
        assert_eq!(
            (choices[0].width, choices[0].height),
            (Some(1280), Some(720))
        );
        assert_eq!(
            (choices[2].width, choices[2].height),
            (Some(640), Some(360))
        );
    }

    #[test]
    fn choices_from_formats() {
        let v: Value = serde_json::json!({
            "title": "T", "duration": 10.0, "extractor_key": "Youtube",
            "formats": [
                {"format_id": "140", "vcodec": "none", "acodec": "mp4a", "filesize": 100},
                {"format_id": "137", "vcodec": "avc1", "acodec": "none", "height": 1080, "fps": 30, "filesize": 1000},
                {"format_id": "22", "vcodec": "avc1", "acodec": "mp4a", "height": 720, "fps": 60, "filesize": 500}
            ]
        });
        let info = info_from(&v);
        let labels: Vec<_> = info.choices.iter().map(|c| c.label.as_str()).collect();
        assert_eq!(labels, ["Best quality", "1080p", "720p60", "Audio only"]);
        assert!(info.audio.is_empty());
        assert_eq!(info.choices[1].size, Some(1100));
        assert_eq!(info.choices[2].size, Some(500));

        // A vertical Short with an original track and an auto-dub.
        let v: Value = serde_json::json!({
            "title": "S", "duration": 10.0,
            "formats": [
                {"vcodec": "none", "acodec": "mp4a", "language": "ko", "language_preference": -1,
                 "format_note": "Korean, medium", "filesize": 90},
                {"vcodec": "none", "acodec": "mp4a", "language": "en-US", "language_preference": 10,
                 "format_note": "English (US) original (default), medium", "filesize": 100},
                {"vcodec": "avc1", "acodec": "none", "width": 1080, "height": 1920, "filesize": 1000}
            ]
        });
        let info = info_from(&v);
        assert_eq!(info.choices[1].label, "1080p");
        assert_eq!(
            info.choices[1].format,
            "bv*[height<=1920]+ba/b[height<=1920]/b"
        );
        let langs: Vec<_> = info
            .audio
            .iter()
            .map(|a| (a.lang.as_str(), a.label.as_str(), a.original))
            .collect();
        assert_eq!(
            langs,
            [("en-US", "English (US)", true), ("ko", "Korean", false)]
        );
    }
}
