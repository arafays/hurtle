//! Hurtle download engine.
//!
//! A download is split into byte-range *segments*, each fetched over its own
//! HTTP/1.1 connection. When a connection finishes its segment it steals the
//! back half of the largest remaining one (dynamic segmentation), so every
//! connection stays busy until the very end. Servers that throttle per
//! connection therefore deliver roughly `connections ×` the single-stream speed.

pub mod assoc;
pub mod ipc;
mod job;
pub mod log;
pub mod media;
pub mod nmh;
pub mod player;
pub mod rate;
mod sched;
pub mod stream;
pub mod tools;
pub mod torrent;
pub mod util;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context as _, Result};
use serde::{Deserialize, Serialize};
use tokio::runtime::Runtime;
use tokio_util::sync::CancellationToken;

pub const VERSION: &str = env!("CARGO_PKG_VERSION");
/// The git commit this was built from ("c1e7181"); none for builds outside
/// a git checkout. Uncommitted changes aren't marked.
pub const COMMIT: Option<&str> = option_env!("HURTLE_COMMIT");

/// "0.3.0 (c1e7181)", "0.3.0 (c1e7181, debug build)".
pub fn build_info() -> String {
    let mut parts: Vec<&str> = COMMIT.into_iter().collect();
    if cfg!(debug_assertions) {
        parts.push("debug build");
    }
    if parts.is_empty() {
        VERSION.to_string()
    } else {
        format!("{VERSION} ({})", parts.join(", "))
    }
}

/// A request to download something. This is also the wire format the browser
/// extension sends (camelCase JSON).
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct AddRequest {
    pub url: String,
    pub filename: Option<String>,
    pub dir: Option<PathBuf>,
    /// Extra request headers, as `[name, value]` pairs.
    pub headers: Vec<(String, String)>,
    pub referrer: Option<String>,
    /// `Cookie` header value (`a=1; b=2`).
    pub cookies: Option<String>,
    pub user_agent: Option<String>,
    /// Override the default connection count for this download.
    pub connections: Option<usize>,
    /// Per-download speed limit in bytes/s (None or 0 = unlimited).
    pub speed_limit: Option<u64>,
    /// Start paused instead of immediately.
    pub paused: bool,
    /// A page or HLS/DASH stream to fetch through yt-dlp, not a plain file.
    pub media: bool,
    /// yt-dlp format selector (media only), e.g. from [`media::MediaChoice`].
    pub format: Option<String>,
    /// Audio track language (media only), e.g. "en-US". None = the original.
    pub audio_lang: Option<String>,
    /// Output container (media only): mp4, mkv, webm, m4a, mp3, opus.
    /// None = MP4 when possible.
    pub container: Option<String>,
    /// The page the download was found on (the tab, or the link's referrer):
    /// where to go back to for a fresh link once this one expires.
    pub page_url: Option<String>,
    /// Size the browser reported, in bytes. A paused or failed download of
    /// the same size picks up the new link and resumes instead of starting over.
    pub size: Option<u64>,
    /// Contents of a `.torrent` file, base64 (the browser fetched it, or a
    /// second app launch read it from disk). `url` may then be empty.
    pub torrent_data: Option<String>,
    /// Torrents: the ids of the files to download (None = all).
    pub files: Option<Vec<usize>>,
    /// Torrents: seed until this share ratio (None = the global setting, 0 = don't).
    pub seed_ratio: Option<f32>,
    /// Torrents: open the main video in a player as soon as it can stream.
    pub stream: bool,
}

impl AddRequest {
    pub fn new(url: impl Into<String>) -> Self {
        Self {
            url: url.into(),
            ..Default::default()
        }
    }

    /// `headers` plus the cookie/referrer/user-agent fields as headers.
    pub fn header_list(&self) -> Vec<(String, String)> {
        let mut headers = self.headers.clone();
        for (k, v) in [
            ("Cookie", &self.cookies),
            ("Referer", &self.referrer),
            ("User-Agent", &self.user_agent),
        ] {
            if let Some(v) = v.as_ref().filter(|v| !v.is_empty())
                && !headers.iter().any(|(h, _)| h.eq_ignore_ascii_case(k))
            {
                headers.push((k.to_string(), v.clone()));
            }
        }
        headers
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    /// Where downloads go unless their type has its own folder.
    pub download_dir: PathBuf,
    /// Per-type folders, keyed by [`util::CATEGORIES`] id ("video", "image"…).
    /// A type without an entry uses `download_dir`.
    pub type_dirs: BTreeMap<String, PathBuf>,
    /// Connections per download.
    pub connections: usize,
    /// Never split a segment into pieces smaller than this.
    pub min_segment: u64,
    /// Retries per connection before it gives its segment back.
    pub max_retries: u32,
    /// Retry a download a few times, with growing pauses, after the network
    /// drops, instead of failing it.
    pub auto_retry: bool,
    /// Desktop notifications when a download finishes or fails.
    pub notify: bool,
    /// Downloads allowed to run at once; the rest wait as `Queued`.
    pub max_active: usize,
    /// Global speed limit across all downloads, bytes/s (0 = unlimited).
    pub speed_limit: u64,
    /// App theme: "system" (follow the desktop's palette), "light", "dark",
    /// or the id of a built-in preset.
    pub theme: String,
    /// BitTorrent listening port (TCP and uTP); taken → any free port.
    pub torrent_port: u16,
    /// Forward the port on the router (UPnP).
    pub torrent_upnp: bool,
    /// Find peers through the DHT and local discovery (magnets need it).
    pub torrent_dht: bool,
    /// Keep uploading a finished torrent until uploaded ÷ size reaches this (0 = never seed).
    pub seed_ratio: f32,
    /// BitTorrent upload limit, bytes/s (0 = unlimited).
    pub upload_limit: u64,
    /// Peers per torrent.
    pub torrent_peers: usize,
    /// Player for streams: an id from `player::detect` ("vlc", "mpv"…) or a
    /// program path. Empty = the first one found, VLC preferred.
    pub player: String,
    /// Let other devices on the network open stream URLs (VLC on a TV…).
    pub stream_lan: bool,
    /// Stream server port (0 = any free port).
    pub stream_port: u16,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            download_dir: dirs::download_dir()
                .or_else(|| dirs::home_dir().map(|h| h.join("Downloads")))
                .unwrap_or_else(|| PathBuf::from(".")),
            type_dirs: BTreeMap::new(),
            connections: 8,
            min_segment: 512 * 1024,
            max_retries: 6,
            auto_retry: true,
            notify: true,
            max_active: 4,
            speed_limit: 0,
            theme: "system".into(),
            torrent_port: 42069,
            torrent_upnp: true,
            torrent_dht: true,
            seed_ratio: 1.0,
            upload_limit: 0,
            torrent_peers: 128,
            player: String::new(),
            stream_lan: false,
            stream_port: 0,
        }
    }
}

impl Settings {
    /// The folder for a file of this type ([`util::category`]).
    pub fn dir_for(&self, category: Option<&str>) -> PathBuf {
        category
            .and_then(|c| self.type_dirs.get(c))
            .filter(|d| !d.as_os_str().is_empty())
            .unwrap_or(&self.download_dir)
            .clone()
    }

    /// `settings.json` in the data dir, read without starting an engine
    /// (the native host answers the extension with it when the app is closed).
    pub fn load() -> Self {
        read_settings(&data_dir().join("settings.json"))
    }

    pub fn save(&self) -> std::io::Result<()> {
        let dir = data_dir();
        util::create_private_dir(&dir)?;
        util::write_private(
            &dir.join("settings.json"),
            &serde_json::to_vec_pretty(self)?,
        )
    }

    /// Apply a settings change from IPC. Returns whether anything changed.
    pub fn apply(&mut self, change: &ipc::SettingsChange) -> Result<bool, String> {
        let mut changed = false;
        if let Some(c) = change.connections {
            self.connections = c.clamp(1, 32);
            changed = true;
        }
        if let Some(l) = change.speed_limit {
            self.speed_limit = l;
            changed = true;
        }
        if let Some(b) = change.auto_retry {
            self.auto_retry = b;
            changed = true;
        }
        if let Some(b) = change.notify {
            self.notify = b;
            changed = true;
        }
        if let Some(r) = change.seed_ratio {
            self.seed_ratio = r.clamp(0.0, 100.0);
            changed = true;
        }
        if let Some(l) = change.upload_limit {
            self.upload_limit = l;
            changed = true;
        }
        if let Some(p) = change.torrent_port {
            self.torrent_port = p;
            changed = true;
        }
        if let Some(b) = change.torrent_dht {
            self.torrent_dht = b;
            changed = true;
        }
        if let Some(b) = change.torrent_upnp {
            self.torrent_upnp = b;
            changed = true;
        }
        if let Some(p) = &change.player {
            // An id, or a program path; nothing else from IPC.
            let p = p.trim();
            if !p.is_empty() && !player::detect().iter().any(|d| d.id == p) {
                return Err(format!("no player \"{p}\" found"));
            }
            self.player = p.to_string();
            changed = true;
        }
        if let Some(d) = &change.download_dir {
            self.download_dir = folder(d)?;
            changed = true;
        }
        for (cat, d) in change.type_dirs.iter().flatten() {
            if !util::CATEGORIES.iter().any(|(id, _)| id == cat) {
                return Err(format!("unknown file type \"{cat}\""));
            }
            // An empty path means "use the main folder".
            if d.trim().is_empty() {
                self.type_dirs.remove(cat);
            } else {
                self.type_dirs.insert(cat.clone(), folder(d)?);
            }
            changed = true;
        }
        Ok(changed)
    }
}

/// A folder typed or picked by the user: absolute, `~` expanded.
fn folder(input: &str) -> Result<PathBuf, String> {
    let input = input.trim();
    let path = match input.strip_prefix('~') {
        Some(rest) if rest.is_empty() || rest.starts_with(['/', '\\']) => dirs::home_dir()
            .ok_or("no home folder")?
            .join(rest.trim_start_matches(['/', '\\'])),
        _ => PathBuf::from(input),
    };
    if !path.is_absolute() {
        return Err(format!("\"{input}\" is not a full path"));
    }
    if path.exists() && !path.is_dir() {
        return Err(format!("{} is a file, not a folder", path.display()));
    }
    Ok(path)
}

/// Resolve a path that may not exist yet: canonicalize its deepest existing
/// ancestor (following symlinks) and re-append the rest.
fn resolve_loose(path: &Path) -> PathBuf {
    let mut rest = vec![];
    let mut cur = path;
    loop {
        if let Ok(real) = cur.canonicalize() {
            return rest.iter().rev().fold(real, |p, c| p.join(c));
        }
        match (cur.parent(), cur.file_name()) {
            (Some(parent), Some(name)) => {
                rest.push(name.to_owned());
                cur = parent;
            }
            _ => return path.to_path_buf(),
        }
    }
}

/// The folder blocklist applied to a settings change that came over IPC
/// (also used by hurtle-host when it edits settings with the app closed).
pub fn check_ipc_settings(change: &ipc::SettingsChange) -> Result<(), String> {
    change
        .download_dir
        .iter()
        .chain(change.type_dirs.iter().flat_map(|m| m.values()))
        .filter(|d| !d.trim().is_empty())
        .try_for_each(|d| check_ipc_dir(d))
}

/// Folders an IPC caller (the extension, or whoever reaches the socket) may
/// not point downloads at: places where a dropped file runs by itself later.
/// The app's own folder picker and the CLI are not restricted.
fn check_ipc_dir(input: &str) -> Result<(), String> {
    let path = folder(input)?;
    if path
        .components()
        .any(|c| matches!(c, std::path::Component::ParentDir))
    {
        return Err(format!("\"{input}\" must not contain \"..\""));
    }
    let real = resolve_loose(&path);
    let mut bad: Vec<PathBuf> = vec![];
    if let Some(home) = dirs::home_dir() {
        for p in [
            ".ssh",
            ".gnupg",
            ".config/autostart",
            ".config/systemd",
            ".config/fish/conf.d",
            ".config/fish/functions",
            ".config/environment.d",
            ".config/plasma-workspace",
            ".config/autostart-scripts",
            ".bashrc.d",
            ".profile.d",
            ".local/bin",
            ".mozilla/native-messaging-hosts",
            ".config/google-chrome/NativeMessagingHosts",
            ".config/chromium/NativeMessagingHosts",
            ".config/BraveSoftware/Brave-Browser/NativeMessagingHosts",
            ".config/microsoft-edge/NativeMessagingHosts",
            ".config/vivaldi/NativeMessagingHosts",
            ".config/opera/NativeMessagingHosts",
            "Library/Application Support/Google/Chrome/NativeMessagingHosts",
            "Library/Application Support/Mozilla/NativeMessagingHosts",
            ".local/share/applications",
            "Library/LaunchAgents",
        ] {
            bad.push(home.join(p));
        }
    }
    if cfg!(windows)
        && let Some(cfg) = dirs::config_dir()
    {
        bad.push(cfg.join("Microsoft/Windows/Start Menu"));
    }
    if bad
        .iter()
        .any(|b| real.starts_with(b) || real.starts_with(resolve_loose(b)))
    {
        return Err(format!(
            "{} is a protected folder; choose it with Hurtle's folder picker",
            path.display()
        ));
    }
    Ok(())
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", content = "message", rename_all = "camelCase")]
pub enum Status {
    Queued,
    Probing,
    Downloading,
    Paused,
    Completed,
    Failed(String),
}

impl Status {
    pub fn is_active(&self) -> bool {
        matches!(self, Status::Probing | Status::Downloading)
    }
    pub fn label(&self) -> &str {
        match self {
            Status::Queued => "Queued",
            Status::Probing => "Connecting",
            Status::Downloading => "Downloading",
            Status::Paused => "Paused",
            Status::Completed => "Completed",
            Status::Failed(_) => "Failed",
        }
    }
}

/// How a download is fetched.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Kind {
    /// A plain file over HTTP, split into segments.
    #[default]
    File,
    /// A video page or HLS/DASH stream, handed to yt-dlp.
    Media,
    /// A magnet link or `.torrent`, fetched over BitTorrent.
    Torrent,
}

/// Byte range `[start, end)`; `pos` is how far it has been written.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Segment {
    pub start: u64,
    pub end: u64,
    pub pos: u64,
    /// A connection is currently working on this segment.
    #[serde(skip)]
    pub owned: bool,
    /// Smoothed speed of the connection working on it, bytes/s (0 = unknown).
    #[serde(skip)]
    pub speed: u64,
    /// When its owner last delivered data (stall detection).
    #[serde(skip)]
    pub last_data: Option<std::time::Instant>,
}

impl Segment {
    pub fn remaining(&self) -> u64 {
        self.end.saturating_sub(self.pos)
    }
}

/// Everything known about one download. `Engine::snapshot` hands out clones.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Download {
    pub id: u64,
    pub url: String,
    /// URL after redirects; every segment requests this.
    pub final_url: Option<String>,
    pub headers: Vec<(String, String)>,
    pub filename: String,
    pub dir: PathBuf,
    /// No folder was asked for: `dir` follows the file's type
    /// ([`Settings::dir_for`]) once its name is known.
    #[serde(default)]
    pub auto_dir: bool,
    /// Final path once the name is resolved.
    pub path: Option<PathBuf>,
    pub total: Option<u64>,
    /// Server honours `Range`, so multi-connection and resume work.
    pub resumable: bool,
    pub etag: Option<String>,
    pub last_modified: Option<String>,
    pub segments: Vec<Segment>,
    pub downloaded: u64,
    pub status: Status,
    pub connections: usize,
    /// Per-download speed limit, bytes/s (0 = unlimited).
    #[serde(default)]
    pub speed_limit: u64,
    pub added_at: u64,
    pub finished_at: Option<u64>,
    #[serde(default)]
    pub kind: Kind,
    /// yt-dlp format selector (media downloads).
    #[serde(default)]
    pub format: Option<String>,
    /// Audio track language (media downloads); None = the original.
    #[serde(default)]
    pub audio_lang: Option<String>,
    /// Output container (media downloads); None = MP4 when possible.
    #[serde(default)]
    pub container: Option<String>,
    /// The page this was found on; go there for a fresh link.
    #[serde(default)]
    pub page_url: Option<String>,
    /// Torrent downloads: info hash, files, seeding.
    #[serde(default)]
    pub torrent: Option<torrent::TorrentState>,
    /// What's happening besides transferring bytes ("Merging audio and video").
    #[serde(skip)]
    pub note: Option<String>,
    /// Bytes per second (smoothed).
    #[serde(skip)]
    pub speed: f64,
    /// Connections currently receiving data.
    #[serde(skip)]
    pub active_connections: usize,
}

impl Download {
    pub fn progress(&self) -> Option<f32> {
        match self.total {
            Some(0) => Some(1.0),
            Some(t) => Some((self.downloaded as f64 / t as f64).clamp(0.0, 1.0) as f32),
            None => None,
        }
    }

    pub fn eta_secs(&self) -> Option<u64> {
        let total = self.total?;
        (self.speed > 1.0)
            .then(|| (total.saturating_sub(self.downloaded) as f64 / self.speed) as u64)
    }

    pub fn display_name(&self) -> &str {
        if self.filename.is_empty() {
            &self.url
        } else {
            &self.filename
        }
    }
}

/// A `.torrent` path (or `file://` URL) on this machine.
fn local_torrent(url: &str) -> Option<PathBuf> {
    let u = url.trim();
    let path = match u.strip_prefix("file://") {
        Some(rest) => PathBuf::from(
            percent_encoding::percent_decode_str(rest)
                .decode_utf8_lossy()
                .into_owned(),
        ),
        None if !u.contains("://") && !torrent::is_magnet(u) => PathBuf::from(u),
        None => return None,
    };
    (path
        .extension()
        .is_some_and(|e| e.eq_ignore_ascii_case("torrent"))
        && path.is_file())
    .then_some(path)
}

/// Resolve what to stream (the main video by default), make sure it will be
/// downloaded and the download runs, and start the server.
async fn prepare_stream(
    inner: &Arc<Inner>,
    id: u64,
    file: Option<usize>,
) -> Result<(String, usize), String> {
    let job = inner.job(id).ok_or("no such download")?;
    let (file, name, need_select, start) = {
        let rec = job.rec.lock().unwrap();
        match (&rec.kind, &rec.torrent) {
            (Kind::Torrent, Some(t)) => {
                let file = match file.or_else(|| t.main_playable()) {
                    Some(f) => f,
                    None if t.files.is_empty() => {
                        return Err("the file list isn't known yet; try again in a moment".into());
                    }
                    None => return Err("this torrent has no video or audio file".into()),
                };
                let f = t.files.get(file).ok_or("no such file")?;
                (
                    file,
                    f.path.clone(),
                    !f.selected,
                    matches!(rec.status, Status::Paused | Status::Failed(_)),
                )
            }
            _ if rec.status == Status::Completed => (0, rec.filename.clone(), false, false),
            _ => return Err("only finished downloads and torrents can be streamed".into()),
        }
    };
    if need_select {
        let mut sel = job
            .rec
            .lock()
            .unwrap()
            .torrent
            .as_ref()
            .map(|t| t.selected())
            .unwrap_or_default();
        sel.push(file);
        torrent::set_files(inner, &job, &sel).await?;
    }
    let restart = start || (need_select && job.rec.lock().unwrap().status == Status::Paused);
    if restart {
        let mut rec = job.rec.lock().unwrap();
        if matches!(rec.status, Status::Paused | Status::Failed(_)) {
            rec.status = Status::Queued;
        }
        drop(rec);
        inner.mark_dirty();
        inner.schedule();
    }
    let server = stream::server(inner).await?;
    Ok((stream::file_url(server, id, file, &name), file))
}

/// Wait until the file can be read (metadata known, pieces checked), then
/// open the player. Gives up after 10 minutes.
async fn launch_when_ready(inner: Arc<Inner>, id: u64, file: usize, url: String) {
    let deadline = std::time::Instant::now() + Duration::from_secs(600);
    loop {
        let Some(job) = inner.job(id) else { return };
        let (ready, title, gone) = {
            let rec = job.rec.lock().unwrap();
            let title = rec
                .torrent
                .as_ref()
                .and_then(|t| t.files.get(file))
                .map(|f| f.path.rsplit('/').next().unwrap_or(&f.path).to_string())
                .unwrap_or_else(|| rec.filename.clone());
            let ready = match &rec.torrent {
                Some(t) => {
                    rec.status == Status::Completed
                        || (rec.status == Status::Downloading
                            && torrent::live_handle(&inner, &t.info_hash).is_some())
                }
                None => rec.status == Status::Completed,
            };
            let gone = matches!(rec.status, Status::Paused | Status::Failed(_));
            (ready, title, gone)
        };
        if ready {
            let preferred = inner.settings.lock().unwrap().player.clone();
            let msg = match tokio::task::spawn_blocking(move || {
                player::open(&url, &preferred, Some(&title))
            })
            .await
            {
                Ok(Ok(name)) => format!("#{id} streaming file {file} in {name}"),
                Ok(Err(e)) => {
                    job.rec.lock().unwrap().note = Some(format!("Couldn't open a player: {e}"));
                    format!("#{id} player: {e}")
                }
                Err(e) => e.to_string(),
            };
            log::log("stream", msg);
            return;
        }
        if gone || std::time::Instant::now() > deadline {
            return;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

/// The page a request came from: `page_url`, else its referrer (http(s) only).
fn source_page(req: &AddRequest) -> Option<String> {
    [&req.page_url, &req.referrer]
        .into_iter()
        .flatten()
        .map(|u| u.trim())
        .find(|u| u.starts_with("http://") || u.starts_with("https://"))
        .map(String::from)
}

/// A download being added, as [`same_download`] compares it.
struct Want<'a> {
    url: &'a str,
    kind: Kind,
    page: Option<String>,
    filename: Option<String>,
    size: Option<u64>,
    format: Option<&'a str>,
    audio_lang: Option<&'a str>,
    container: Option<&'a str>,
}

/// A link without its query string: signed and expiring links usually
/// change only there (`?token=…&expires=…`).
pub(crate) fn link_path(url: &str) -> &str {
    url.split(['?', '#']).next().unwrap_or(url)
}

/// Is `want` a new link to `d`, an unfinished download in the list?
///
/// - The same link, or the same link with another query string.
/// - A file of exactly the same size (at least 1 MiB) with the same name or
///   found on the same page: an expired CDN link captured again.
/// - A video (yt-dlp) with the same name and options from the same page:
///   a stream whose manifest link expired.
fn same_download(d: &Download, want: &Want) -> bool {
    if !matches!(
        d.status,
        Status::Paused | Status::Failed(_) | Status::Queued
    ) || d.kind != want.kind
    {
        return false;
    }
    if d.url == want.url {
        return true;
    }
    let same_page = want.page.is_some() && d.page_url == want.page;
    let same_name = want.filename.as_deref().is_some_and(|n| n == d.filename);
    match d.kind {
        Kind::File => {
            let same_size = want
                .size
                .is_some_and(|s| s >= 1 << 20 && d.total == Some(s));
            same_size && (same_name || same_page || link_path(&d.url) == link_path(want.url))
        }
        Kind::Media => {
            let same_options = d.format.as_deref() == want.format
                && d.audio_lang.as_deref() == want.audio_lang
                && d.container.as_deref() == want.container;
            same_options && (link_path(&d.url) == link_path(want.url) || (same_page && same_name))
        }
        // Torrents match by info hash, in `add_torrent`.
        Kind::Torrent => false,
    }
}

pub(crate) struct Job {
    pub(crate) rec: Mutex<Download>,
    pub(crate) cancel: Mutex<CancellationToken>,
    pub(crate) limiter: rate::RateLimiter,
    /// yt-dlp's process group, while one runs for this job.
    pub(crate) pid: Mutex<Option<u32>>,
    /// Cancelled only to start again with new settings (yt-dlp limits).
    pub(crate) restart: AtomicBool,
    /// (bytes at last sample) for the speed meter.
    last_sample: Mutex<u64>,
}

pub(crate) struct Inner {
    pub(crate) client: reqwest::Client,
    pub(crate) jobs: Mutex<Vec<Arc<Job>>>,
    pub(crate) settings: Mutex<Settings>,
    pub(crate) global_limit: Arc<rate::RateLimiter>,
    next_id: AtomicU64,
    pub(crate) data_dir: PathBuf,
    store_path: PathBuf,
    settings_path: PathBuf,
    pub(crate) dirty: AtomicBool,
    /// Socket we bound in `serve_ipc`, removed again on shutdown.
    ipc_path: Mutex<Option<PathBuf>>,
    /// Folder pickers asked for over IPC, waiting for the UI to show them.
    folder_picks: Mutex<Vec<FolderPick>>,
    /// Held while a download picks its file name and records the path, so
    /// two downloads probing at once can't claim the same one.
    pub(crate) path_claim: Mutex<()>,
    /// The BitTorrent session, started with the first torrent.
    pub(crate) torrents: torrent::Torrents,
    /// The stream server for players, started on first use.
    pub(crate) stream: tokio::sync::OnceCell<stream::Server>,
}

/// "Choose a folder" asked by a client (the extension). The UI shows its
/// native picker and answers with the folder, or None when cancelled.
pub struct FolderPick {
    pub title: String,
    pub start: Option<PathBuf>,
    pub reply: tokio::sync::oneshot::Sender<Option<PathBuf>>,
}

/// Owns the tokio runtime and every download. Cheap to share behind an `Arc`.
pub struct Engine {
    inner: Arc<Inner>,
    rt: Runtime,
}

pub fn data_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("HURTLE_DATA") {
        return PathBuf::from(dir);
    }
    let Some(base) = dirs::data_dir() else {
        return PathBuf::from(".hurtle");
    };
    let dir = base.join("hurtle");
    static MIGRATE: std::sync::Once = std::sync::Once::new();
    MIGRATE.call_once(|| migrate_legacy_dir(&base.join("surge"), &dir));
    dir
}

/// Hurtle was called Surge until 0.2. Move the old data dir (downloads list,
/// settings, partial media, fetched tools) to the new name once. Partial media
/// paths are derived from the data dir, so paused downloads still resume.
fn migrate_legacy_dir(old: &Path, new: &Path) {
    if new.exists() || !old.join("settings.json").is_file() && !old.join("downloads.json").is_file()
    {
        return;
    }
    if let Err(e) = std::fs::rename(old, new) {
        eprintln!("hurtle: moving {} to {}: {e}", old.display(), new.display());
    }
}

/// Keep the unreadable file as `<name>.bak` before it gets overwritten.
fn back_up(path: &Path, why: &str) {
    let bak = path.with_extension("json.bak");
    eprintln!(
        "hurtle: {} {why}; keeping a copy as {}",
        path.display(),
        bak.display()
    );
    let _ = std::fs::copy(path, bak);
}

/// `settings.json`; a file that exists but doesn't parse is backed up first.
fn read_settings(path: &Path) -> Settings {
    let Ok(bytes) = std::fs::read(path) else {
        return Settings::default();
    };
    serde_json::from_slice(&bytes).unwrap_or_else(|e| {
        back_up(path, &format!("is unreadable ({e})"));
        Settings::default()
    })
}

/// `downloads.json`, one record at a time so a single bad one doesn't take
/// the rest with it. Anything skipped is kept in `downloads.json.bak`.
fn read_records(path: &Path) -> Vec<Download> {
    let Ok(bytes) = std::fs::read(path) else {
        return Vec::new();
    };
    let values: Vec<serde_json::Value> = match serde_json::from_slice(&bytes) {
        Ok(v) => v,
        Err(e) => {
            back_up(path, &format!("is unreadable ({e})"));
            return Vec::new();
        }
    };
    let total = values.len();
    let records: Vec<Download> = values
        .into_iter()
        .filter_map(|v| serde_json::from_value(v).ok())
        .collect();
    if records.len() < total {
        back_up(path, &format!("has {} bad records", total - records.len()));
    }
    records
}

impl Engine {
    /// Load persisted downloads/settings from [`data_dir`] and start the runtime.
    pub fn new() -> Result<Self> {
        Self::with_data_dir(&data_dir())
    }

    pub fn with_data_dir(dir: &Path) -> Result<Self> {
        util::create_private_dir(dir).with_context(|| format!("creating {}", dir.display()))?;
        let store_path = dir.join("downloads.json");
        let settings_path = dir.join("settings.json");

        let settings = read_settings(&settings_path);
        let mut records = read_records(&store_path);
        // Anything interrupted by a quit comes back paused.
        for r in &mut records {
            if r.status.is_active() || r.status == Status::Queued {
                r.status = Status::Paused;
            }
            // Stores from before `page_url`: the Referer it was sent with.
            if r.page_url.is_none() {
                r.page_url = r
                    .headers
                    .iter()
                    .find(|(k, v)| k.eq_ignore_ascii_case("referer") && v.starts_with("http"))
                    .map(|(_, v)| v.clone());
            }
        }
        let next_id = records.iter().map(|r| r.id).max().unwrap_or(0) + 1;

        let client = reqwest::Client::builder()
            // One TCP connection per segment. HTTP/2 would multiplex every
            // segment over a single connection and defeat the whole point.
            .http1_only()
            .connect_timeout(Duration::from_secs(20))
            .read_timeout(Duration::from_secs(30))
            .pool_max_idle_per_host(32)
            .redirect(reqwest::redirect::Policy::limited(15))
            .build()?;

        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(4)
            .thread_name("hurtle-io")
            .enable_all()
            .build()?;

        let inner = Arc::new(Inner {
            client,
            jobs: Mutex::new(records.into_iter().map(|r| Arc::new(Job::new(r))).collect()),
            global_limit: Arc::new(rate::RateLimiter::new(settings.speed_limit)),
            settings: Mutex::new(settings),
            next_id: AtomicU64::new(next_id),
            data_dir: dir.to_path_buf(),
            store_path,
            settings_path,
            dirty: AtomicBool::new(false),
            ipc_path: Mutex::new(None),
            folder_picks: Mutex::new(Vec::new()),
            path_claim: Mutex::new(()),
            torrents: torrent::Torrents::default(),
            stream: tokio::sync::OnceCell::new(),
        });

        rt.spawn(ticker(inner.clone()));
        Ok(Self { inner, rt })
    }

    pub fn runtime(&self) -> &tokio::runtime::Handle {
        self.rt.handle()
    }

    /// Folder pickers the UI should show now (see [`FolderPick`]).
    pub fn take_folder_picks(&self) -> Vec<FolderPick> {
        std::mem::take(&mut *self.inner.folder_picks.lock().unwrap())
    }

    pub fn settings(&self) -> Settings {
        self.inner.settings.lock().unwrap().clone()
    }

    pub fn set_settings(&self, s: Settings) {
        torrent::apply_limits(&self.inner, s.speed_limit, s.upload_limit);
        if s.speed_limit != self.inner.global_limit.rate() {
            self.inner.global_limit.set_rate(s.speed_limit);
            // yt-dlp can't share our token bucket: restart it with the new cap.
            for job in self.inner.jobs.lock().unwrap().iter() {
                let rec = job.rec.lock().unwrap();
                if rec.kind == Kind::Media && rec.status.is_active() {
                    job.restart();
                }
            }
        }
        *self.inner.settings.lock().unwrap() = s;
        self.inner.save_settings();
        self.inner.schedule();
    }

    /// Queue a download and return its id. Unlike [`Engine::add_or_relink`]
    /// (what IPC uses), `url` may also be a local `.torrent` file (the CLI,
    /// the app's add bar).
    pub fn add(&self, req: AddRequest) -> Result<u64> {
        if torrent::is_torrent_request(&req) || local_torrent(&req.url).is_some() {
            return self.add_torrent(req, true).map(|(id, _)| id);
        }
        self.add_or_relink(req).map(|(id, _)| id)
    }

    /// Queue a download, or, when it is a fresh link to a paused or failed
    /// download already in the list (see [`same_download`]), give that one
    /// the new link and resume it. The flag says which happened.
    pub fn add_or_relink(&self, req: AddRequest) -> Result<(u64, bool)> {
        if torrent::is_torrent_request(&req) {
            return self.add_torrent(req, false);
        }
        let url = req.url.trim().to_string();
        util::require_http(&url)?;
        if let Some(id) = self.relink(&req, &url) {
            return Ok((id, true));
        }
        let settings = self.settings();
        // A folder for this download only ("~/Videos" from the extension too).
        let dir = match &req.dir {
            Some(d) => Some(folder(&d.to_string_lossy()).map_err(anyhow::Error::msg)?),
            None => None,
        };

        let headers = req.header_list();

        let id = self.inner.next_id.fetch_add(1, Ordering::Relaxed);
        let kind = if req.media || media::is_stream_url(&req.url) {
            Kind::Media
        } else {
            Kind::File
        };
        // The type decides the folder; a name we don't have yet is decided
        // again when the server tells us (see `Settings::dir_for`).
        let category = match kind {
            Kind::Media => Some(media::category(
                req.format.as_deref(),
                req.container.as_deref(),
            )),
            Kind::File | Kind::Torrent => req.filename.as_deref().and_then(util::category),
        };
        let rec = Download {
            id,
            url,
            final_url: None,
            headers,
            filename: req
                .filename
                .as_deref()
                .map(util::sanitize_filename)
                .unwrap_or_default(),
            auto_dir: dir.is_none(),
            dir: dir.unwrap_or_else(|| settings.dir_for(category)),
            path: None,
            total: None,
            resumable: false,
            etag: None,
            last_modified: None,
            segments: vec![],
            downloaded: 0,
            status: if req.paused {
                Status::Paused
            } else {
                Status::Queued
            },
            connections: req.connections.unwrap_or(settings.connections).clamp(1, 32),
            speed_limit: req.speed_limit.unwrap_or(0),
            added_at: util::now_secs(),
            finished_at: None,
            kind,
            format: req.format.clone().filter(|f| !f.is_empty()),
            audio_lang: req.audio_lang.clone().filter(|l| !l.is_empty()),
            container: req.container.clone().filter(|c| !c.is_empty()),
            page_url: source_page(&req),
            torrent: None,
            note: None,
            speed: 0.0,
            active_connections: 0,
        };
        log::log(
            "app",
            format!(
                "#{id} added {} {}: \"{}\" to {} format={:?} audio={:?} container={:?} connections={}{}",
                if rec.kind == Kind::Media {
                    "media"
                } else {
                    "file"
                },
                log::url(&rec.url),
                rec.filename,
                rec.dir.display(),
                rec.format,
                rec.audio_lang,
                rec.container,
                rec.connections,
                if req.paused { " (paused)" } else { "" }
            ),
        );
        self.inner
            .jobs
            .lock()
            .unwrap()
            .push(Arc::new(Job::new(rec)));
        self.inner.mark_dirty();
        let _guard = self.rt.enter();
        self.inner.schedule();
        Ok((id, false))
    }

    /// Queue a magnet link or `.torrent` (URL, data, or with `allow_local` a
    /// file path). The same torrent already in the list is resumed instead.
    fn add_torrent(&self, req: AddRequest, allow_local: bool) -> Result<(u64, bool)> {
        let url = req.url.trim().to_string();
        let bytes = match req.torrent_data.as_deref().filter(|d| !d.is_empty()) {
            Some(d) => Some(torrent::decode_data(d).map_err(anyhow::Error::msg)?),
            None => match local_torrent(&url).filter(|_| allow_local) {
                Some(path) => Some(
                    std::fs::read(&path).with_context(|| format!("reading {}", path.display()))?,
                ),
                None => None,
            },
        };
        let (hash, name, url) = if let Some(b) = &bytes {
            let p = torrent::remember(&self.inner, b).map_err(anyhow::Error::msg)?;
            let shown = if url.starts_with("http") {
                url.clone()
            } else {
                format!("magnet:?xt=urn:btih:{}", p.info_hash)
            };
            (p.info_hash, Some(p.name), shown)
        } else if torrent::is_magnet(&url) {
            let m = torrent::parse_magnet(&url).map_err(anyhow::Error::msg)?;
            (m.info_hash, m.name, url)
        } else {
            util::require_http(&url)?;
            (String::new(), None, url)
        };

        // Already in the list: resume it (and take the new file choice).
        if !hash.is_empty()
            && let Some(d) = self.snapshot().into_iter().find(|d| {
                d.torrent
                    .as_ref()
                    .is_some_and(|t| t.info_hash.eq_ignore_ascii_case(&hash))
            })
        {
            if let Some(files) = &req.files {
                let _ = self.set_torrent_files(d.id, files);
            }
            if !req.paused {
                self.resume(d.id);
            }
            if req.stream {
                let _ = self.play(d.id, None);
            }
            return Ok((d.id, true));
        }

        let settings = self.settings();
        let dir = match &req.dir {
            Some(d) => Some(folder(&d.to_string_lossy()).map_err(anyhow::Error::msg)?),
            None => None,
        };
        let id = self.inner.next_id.fetch_add(1, Ordering::Relaxed);
        let filename = name
            .or_else(|| req.filename.clone())
            .or_else(|| {
                util::filename_from_url(&url).map(|n| n.trim_end_matches(".torrent").to_string())
            })
            .map(|n| util::sanitize_filename(&n))
            .unwrap_or_default();
        let rec = Download {
            id,
            url,
            final_url: None,
            headers: req.header_list(),
            filename,
            auto_dir: dir.is_none(),
            dir: dir.unwrap_or_else(|| settings.download_dir.clone()),
            path: None,
            total: None,
            resumable: true,
            etag: None,
            last_modified: None,
            segments: vec![],
            downloaded: 0,
            status: if req.paused {
                Status::Paused
            } else {
                Status::Queued
            },
            connections: settings.connections,
            speed_limit: req.speed_limit.unwrap_or(0),
            added_at: util::now_secs(),
            finished_at: None,
            kind: Kind::Torrent,
            format: None,
            audio_lang: None,
            container: None,
            page_url: source_page(&req),
            torrent: Some(torrent::TorrentState {
                info_hash: hash.clone(),
                wanted: req.files.clone(),
                seed_ratio: req.seed_ratio,
                ..Default::default()
            }),
            note: None,
            speed: 0.0,
            active_connections: 0,
        };
        log::log(
            "app",
            format!(
                "#{id} added torrent {} \"{}\" to {}{}",
                if hash.is_empty() {
                    log::url(&rec.url)
                } else {
                    hash
                },
                rec.filename,
                rec.dir.display(),
                if req.paused { " (paused)" } else { "" }
            ),
        );
        self.inner
            .jobs
            .lock()
            .unwrap()
            .push(Arc::new(Job::new(rec)));
        self.inner.mark_dirty();
        let _guard = self.rt.enter();
        self.inner.schedule();
        if req.stream {
            let _ = self.play(id, None);
        }
        Ok((id, false))
    }

    /// The file list of a magnet link or `.torrent` (for a dialog). Magnets
    /// ask peers, which can take a while. Blocking.
    pub fn probe_torrent(&self, req: &AddRequest) -> Result<torrent::TorrentProbe> {
        let mut req = req.clone();
        if req.torrent_data.is_none()
            && let Some(path) = local_torrent(&req.url)
        {
            req.torrent_data = Some(torrent::encode_data(&std::fs::read(&path)?));
        }
        self.rt
            .block_on(torrent::probe(&self.inner, &req))
            .map_err(anyhow::Error::msg)
    }

    /// Choose which files of a torrent are downloaded.
    pub fn set_torrent_files(&self, id: u64, files: &[usize]) -> Result<()> {
        let job = self.inner.job(id).context("no such download")?;
        let r = match tokio::runtime::Handle::try_current() {
            Ok(_) => {
                // Inside the runtime (IPC): don't block it.
                let (inner, files) = (self.inner.clone(), files.to_vec());
                self.rt.spawn(async move {
                    let _ = torrent::set_files(&inner, &job, &files).await;
                });
                Ok(())
            }
            Err(_) => self
                .rt
                .block_on(torrent::set_files(&self.inner, &job, files)),
        };
        r.map_err(anyhow::Error::msg)
    }

    /// Seed a finished torrent again (until its ratio, or one more round).
    pub fn seed(&self, id: u64) -> Result<()> {
        let job = self.inner.job(id).context("no such download")?;
        self.rt
            .block_on(torrent::seed_again(&self.inner, &job))
            .map_err(anyhow::Error::msg)
    }

    /// The URL a player can stream file `file` of download `id` from (None =
    /// the main video). A paused torrent is resumed, and an unselected file
    /// is added to the download. Works for finished plain downloads too.
    pub fn stream_url(&self, id: u64, file: Option<usize>) -> Result<String> {
        let inner = self.inner.clone();
        self.block(async move { prepare_stream(&inner, id, file).await })
            .map(|(url, _)| url)
            .map_err(anyhow::Error::msg)
    }

    /// The `.m3u` playlist of every playable file of a download.
    pub fn playlist_url(&self, id: u64) -> Result<String> {
        let inner = self.inner.clone();
        self.block(async move {
            let s = stream::server(&inner).await?;
            Ok(stream::playlist_url(s, id))
        })
        .map_err(anyhow::Error::msg)
    }

    /// Stream a file into the player (VLC first, see [`player::choose`]) as
    /// soon as it can be read. Returns the stream URL.
    pub fn play(&self, id: u64, file: Option<usize>) -> Result<String> {
        let inner = self.inner.clone();
        let (url, file) = self
            .block(async move { prepare_stream(&inner, id, file).await })
            .map_err(anyhow::Error::msg)?;
        let inner = self.inner.clone();
        let u = url.clone();
        self.rt
            .spawn(async move { launch_when_ready(inner, id, file, u).await });
        Ok(url)
    }

    /// Run a future to completion from a non-async caller, or on the runtime
    /// when called from inside it (IPC), without blocking a worker.
    fn block<T: Send + 'static>(
        &self,
        fut: impl std::future::Future<Output = Result<T, String>> + Send + 'static,
    ) -> Result<T, String> {
        if tokio::runtime::Handle::try_current().is_ok() {
            let (tx, rx) = std::sync::mpsc::channel();
            self.rt.spawn(async move {
                let _ = tx.send(fut.await);
            });
            tokio::task::block_in_place(|| rx.recv().map_err(|e| e.to_string()))?
        } else {
            self.rt.block_on(fut)
        }
    }

    /// A magnet link for a torrent download.
    pub fn magnet_link(&self, id: u64) -> Option<String> {
        self.get(id).as_ref().and_then(torrent::magnet_link)
    }

    /// Point a matching unfinished download at `url` (with the request's
    /// fresh cookies and referrer) and queue it again. Its segments stay, so
    /// the next start resumes them if the server still has the same file.
    fn relink(&self, req: &AddRequest, url: &str) -> Option<u64> {
        let kind = if req.media || media::is_stream_url(url) {
            Kind::Media
        } else {
            Kind::File
        };
        let found = Want {
            url,
            kind,
            page: source_page(req),
            filename: req.filename.as_deref().map(util::sanitize_filename),
            size: req.size,
            format: req.format.as_deref().filter(|f| !f.is_empty()),
            audio_lang: req.audio_lang.as_deref().filter(|l| !l.is_empty()),
            container: req.container.as_deref().filter(|c| !c.is_empty()),
        };
        // The furthest along wins; `snapshot` has released every lock again.
        let id = self
            .snapshot()
            .into_iter()
            .filter(|d| same_download(d, &found))
            .max_by_key(|d| (d.downloaded, d.id))?
            .id;
        let job = self.inner.job(id)?;
        {
            let mut rec = job.rec.lock().unwrap();
            // It may have been started in the meantime.
            if !matches!(
                rec.status,
                Status::Paused | Status::Failed(_) | Status::Queued
            ) {
                return None;
            }
            crate::log::log(
                "app",
                format!(
                    "#{id} new link {} (was {}), {} of {:?} bytes kept",
                    log::url(url),
                    log::url(&rec.url),
                    rec.downloaded,
                    rec.total
                ),
            );
            rec.url = url.to_string();
            rec.final_url = None;
            rec.headers = req.header_list();
            if let Some(page) = found.page {
                rec.page_url = Some(page);
            }
            // Another CDN node or a re-signed link often sends a different
            // ETag for the same bytes; the size (and the probe) decide.
            rec.etag = None;
            rec.last_modified = None;
            rec.status = if req.paused {
                Status::Paused
            } else {
                Status::Queued
            };
        }
        self.inner.mark_dirty();
        let _guard = self.rt.enter();
        self.inner.schedule();
        Some(id)
    }

    pub fn pause(&self, id: u64) {
        if let Some(job) = self.inner.job(id) {
            let mut rec = job.rec.lock().unwrap();
            match rec.status {
                Status::Queued => rec.status = Status::Paused,
                // A finished torrent that is seeding: stop uploading.
                Status::Completed if rec.torrent.as_ref().is_some_and(|t| t.seeding) => {
                    job.cancel.lock().unwrap().cancel();
                }
                Status::Probing | Status::Downloading => {
                    job.cancel.lock().unwrap().cancel();
                    job.kill_child();
                }
                _ => {}
            }
        }
        self.inner.mark_dirty();
    }

    /// Resume a paused or failed download (re-queues it).
    pub fn resume(&self, id: u64) {
        if let Some(job) = self.inner.job(id) {
            let mut rec = job.rec.lock().unwrap();
            if matches!(rec.status, Status::Paused | Status::Failed(_)) {
                rec.status = Status::Queued;
            }
        }
        self.inner.mark_dirty();
        let _guard = self.rt.enter();
        self.inner.schedule();
    }

    /// Pause everything that is running or waiting.
    pub fn pause_all(&self) {
        for id in self.ids() {
            self.pause(id);
        }
    }

    /// Resume everything paused (failed downloads stay as they are).
    pub fn resume_all(&self) {
        for d in self.snapshot() {
            if d.status == Status::Paused {
                self.resume(d.id);
            }
        }
    }

    fn ids(&self) -> Vec<u64> {
        self.snapshot().iter().map(|d| d.id).collect()
    }

    /// Change the connection count; takes effect on the next start/resume.
    pub fn set_connections(&self, id: u64, n: usize) {
        if let Some(job) = self.inner.job(id) {
            job.rec.lock().unwrap().connections = n.clamp(1, 32);
        }
        self.inner.mark_dirty();
    }

    /// Per-download speed limit in bytes/s (0 = unlimited). Applies immediately.
    pub fn set_speed_limit(&self, id: u64, bytes_per_sec: u64) {
        if let Some(job) = self.inner.job(id) {
            let mut rec = job.rec.lock().unwrap();
            let changed = rec.speed_limit != bytes_per_sec;
            rec.speed_limit = bytes_per_sec;
            job.limiter.set_rate(bytes_per_sec);
            if changed && rec.kind != Kind::File && rec.status.is_active() {
                job.restart();
            }
        }
        self.inner.mark_dirty();
    }

    /// Forget a download. With `delete_files`, also remove what is on disk.
    pub fn remove(&self, id: u64, delete_files: bool) {
        let job = {
            let mut jobs = self.inner.jobs.lock().unwrap();
            let Some(i) = jobs.iter().position(|j| j.rec.lock().unwrap().id == id) else {
                return;
            };
            jobs.remove(i)
        };
        job.cancel.lock().unwrap().cancel();
        job.kill_child();
        let rec = job.rec.lock().unwrap().clone();
        if rec.kind == Kind::Torrent {
            // Unfinished torrent data is as useless as a .part file.
            let delete = delete_files || rec.status != Status::Completed;
            self.rt
                .spawn(torrent::forget(self.inner.clone(), rec.clone(), delete));
        } else if rec.kind == Kind::Media {
            // yt-dlp's fragments and .part files live in a per-job dir.
            let tmp = media::temp_dir(&self.inner, rec.id);
            self.rt.spawn(async move {
                tokio::time::sleep(Duration::from_millis(300)).await;
                let _ = tokio::fs::remove_dir_all(tmp).await;
            });
            if delete_files
                && rec.status == Status::Completed
                && let Some(path) = &rec.path
            {
                let _ = std::fs::remove_file(path);
            }
        } else if let Some(path) = &rec.path {
            // The .part file is useless without its record, so always drop it.
            let part = util::part_path(path);
            if rec.status.is_active() {
                // Workers are still unwinding; let them close the file first.
                self.rt.spawn(async move {
                    tokio::time::sleep(Duration::from_millis(500)).await;
                    let _ = tokio::fs::remove_file(part).await;
                });
            } else {
                let _ = std::fs::remove_file(part);
            }
            if delete_files && rec.status == Status::Completed {
                let _ = std::fs::remove_file(path);
            }
        }
        self.inner.mark_dirty();
        let _guard = self.rt.enter();
        self.inner.schedule();
    }

    /// Drop every completed entry from the list (files stay on disk).
    pub fn clear_completed(&self) {
        self.inner
            .jobs
            .lock()
            .unwrap()
            .retain(|j| j.rec.lock().unwrap().status != Status::Completed);
        self.inner.mark_dirty();
    }

    pub fn get(&self, id: u64) -> Option<Download> {
        self.inner.job(id).map(|j| j.rec.lock().unwrap().clone())
    }

    pub fn snapshot(&self) -> Vec<Download> {
        self.inner
            .jobs
            .lock()
            .unwrap()
            .iter()
            .map(|j| j.rec.lock().unwrap().clone())
            .collect()
    }

    /// Write state to disk now (also happens automatically every couple of seconds).
    pub fn save(&self) {
        self.inner.save();
    }

    /// Listen for IPC clients (a Unix socket, or a named pipe on Windows).
    /// `on_request` sees every request after it has been handled (the app
    /// uses it to raise its window).
    pub fn serve_ipc(
        self: &Arc<Self>,
        on_request: impl Fn(&ipc::IpcRequest) + Send + Sync + 'static,
    ) -> Result<()> {
        let engine = Arc::downgrade(self);
        let on_request: OnRequest = Arc::new(on_request);

        #[cfg(unix)]
        {
            let path = ipc::socket_path();
            ipc::guard_socket_dir(true)
                .with_context(|| format!("securing the folder of {}", path.display()))?;
            if path.exists() {
                if std::os::unix::net::UnixStream::connect(&path).is_ok() {
                    anyhow::bail!(
                        "another Hurtle instance is already listening on {}",
                        path.display()
                    );
                }
                let _ = std::fs::remove_file(&path);
            }
            let listener = {
                let _guard = self.rt.enter();
                tokio::net::UnixListener::bind(&path)
                    .with_context(|| format!("binding {}", path.display()))?
            };
            {
                use std::os::unix::fs::PermissionsExt;
                let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
            }
            *self.inner.ipc_path.lock().unwrap() = Some(path.clone());
            self.rt.spawn(async move {
                while let Ok((stream, _)) = listener.accept().await {
                    tokio::spawn(serve_client(stream, engine.clone(), on_request.clone()));
                }
            });
        }

        #[cfg(windows)]
        {
            use tokio::net::windows::named_pipe::ServerOptions;
            let name = ipc::pipe_name();
            // first_pipe_instance fails if another Hurtle already owns the name.
            let mut server = {
                let _guard = self.rt.enter();
                ServerOptions::new()
                    .first_pipe_instance(true)
                    .reject_remote_clients(true)
                    .create(&name)
                    .with_context(|| format!("another Hurtle instance owns {name}"))?
            };
            self.rt.spawn(async move {
                loop {
                    if server.connect().await.is_err() {
                        break;
                    }
                    // Open the next instance before handing this one off, so
                    // there's never a moment with no pipe to connect to.
                    let next = match ServerOptions::new()
                        .reject_remote_clients(true)
                        .create(&name)
                    {
                        Ok(n) => n,
                        Err(_) => break,
                    };
                    let client = std::mem::replace(&mut server, next);
                    tokio::spawn(serve_client(client, engine.clone(), on_request.clone()));
                }
            });
        }

        Ok(())
    }
}

type OnRequest = Arc<dyn Fn(&ipc::IpcRequest) + Send + Sync>;

/// One IPC client: JSON request lines in, JSON reply lines out.
async fn serve_client<S>(stream: S, engine: std::sync::Weak<Engine>, on_request: OnRequest)
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Send + 'static,
{
    use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
    /// A request is a few KB; anything bigger is a broken or hostile client.
    const MAX_LINE: u64 = 4 << 20;
    let (r, mut w) = tokio::io::split(stream);
    let mut reader = BufReader::new(r);
    let mut buf = Vec::new();
    loop {
        buf.clear();
        match (&mut reader)
            .take(MAX_LINE + 1)
            .read_until(b'\n', &mut buf)
            .await
        {
            Ok(0) | Err(_) => return,
            Ok(_) if buf.len() as u64 > MAX_LINE => {
                let mut out = serde_json::to_string(&ipc::IpcReply::err("request too large"))
                    .unwrap_or_default();
                out.push('\n');
                let _ = w.write_all(out.as_bytes()).await;
                return;
            }
            Ok(_) => {}
        }
        let line = String::from_utf8_lossy(&buf);
        let Some(engine) = engine.upgrade() else {
            return;
        };
        let reply = match serde_json::from_str::<ipc::IpcRequest>(&line) {
            Ok(req) => {
                let reply = handle_request(&engine, &req).await;
                on_request(&req);
                reply
            }
            Err(e) => ipc::IpcReply::err(format!("bad request: {e}")),
        };
        let mut out = serde_json::to_string(&reply).unwrap_or_default();
        out.push('\n');
        if w.write_all(out.as_bytes()).await.is_err() || w.flush().await.is_err() {
            return;
        }
    }
}

/// Files of a torrent listed over IPC (the rest are still downloaded).
const MAX_LISTED_FILES: usize = 4000;

async fn handle_request(engine: &Engine, req: &ipc::IpcRequest) -> ipc::IpcReply {
    match req {
        ipc::IpcRequest::Add(add) => match add
            .dir
            .as_ref()
            .map_or(Ok(()), |d| check_ipc_dir(&d.to_string_lossy()))
            .map_err(anyhow::Error::msg)
            .and_then(|_| engine.add_or_relink(add.clone()))
        {
            Ok((id, resumed)) => ipc::IpcReply {
                resumed: resumed.then_some(true),
                ..ipc::IpcReply::ok(Some(id))
            },
            Err(e) => ipc::IpcReply::err(e),
        },
        ipc::IpcRequest::Probe(p) => {
            let p = p.clone();
            tokio::task::spawn_blocking(move || ipc::IpcReply::probe(&p))
                .await
                .unwrap_or_else(ipc::IpcReply::err)
        }
        ipc::IpcRequest::TorrentInfo(r) => match torrent::probe(&engine.inner, r).await {
            Ok(mut info) => {
                // Native-messaging replies are capped at 1 MB.
                info.files.truncate(MAX_LISTED_FILES);
                ipc::IpcReply {
                    torrent: Some(info),
                    ..ipc::IpcReply::ok(None)
                }
            }
            Err(e) => ipc::IpcReply::err(e),
        },
        ipc::IpcRequest::Stream { id, file, play } => {
            let r = match prepare_stream(&engine.inner, *id, *file).await {
                Ok((url, f)) => {
                    if *play {
                        let inner = engine.inner.clone();
                        tokio::spawn(launch_when_ready(inner, *id, f, url.clone()));
                    }
                    Ok(url)
                }
                Err(e) => Err(e),
            };
            match r {
                Ok(url) => ipc::IpcReply {
                    url: Some(url),
                    ..ipc::IpcReply::ok(Some(*id))
                },
                Err(e) => ipc::IpcReply::err(e),
            }
        }
        ipc::IpcRequest::Players => ipc::IpcReply {
            players: Some(
                tokio::task::spawn_blocking(player::detect)
                    .await
                    .unwrap_or_default(),
            ),
            ..ipc::IpcReply::ok(None)
        },
        ipc::IpcRequest::Ping | ipc::IpcRequest::Show => ipc::IpcReply::ok(None),
        ipc::IpcRequest::Log { append, tail } => ipc::IpcReply::log(append, *tail),
        ipc::IpcRequest::PickFolder { title, start } => {
            let (tx, rx) = tokio::sync::oneshot::channel();
            engine.inner.folder_picks.lock().unwrap().push(FolderPick {
                title: title.clone().unwrap_or_else(|| "Choose a folder".into()),
                start: start.as_deref().map(PathBuf::from),
                reply: tx,
            });
            match tokio::time::timeout(Duration::from_secs(600), rx).await {
                Ok(Ok(Some(dir))) => ipc::IpcReply {
                    folder: Some(dir.to_string_lossy().into_owned()),
                    ..ipc::IpcReply::ok(None)
                },
                // Cancelled: ok, no folder.
                Ok(Ok(None)) => ipc::IpcReply::ok(None),
                _ => ipc::IpcReply::err("no folder picker answered"),
            }
        }
        ipc::IpcRequest::Settings(change) => {
            if let Err(e) = check_ipc_settings(change) {
                return ipc::IpcReply::err(e);
            }
            let mut s = engine.settings();
            match s.apply(change) {
                Ok(true) => engine.set_settings(s.clone()),
                Ok(false) => {}
                Err(e) => return ipc::IpcReply::err(e),
            }
            ipc::IpcReply {
                settings: Some(s),
                ..ipc::IpcReply::ok(None)
            }
        }
    }
}

impl Engine {
    /// Stop everything and persist: running downloads are saved as paused,
    /// and our IPC socket is removed. Safe to call more than once. Call it
    /// from the UI's quit hook (a GUI toolkit may exit without running `Drop`).
    pub fn shutdown(&self) {
        // Stops the BitTorrent session (listeners, DHT, every torrent).
        self.inner.torrents.cancel.cancel();
        for job in self.inner.jobs.lock().unwrap().iter() {
            job.cancel.lock().unwrap().cancel();
            job.kill_child();
            let mut rec = job.rec.lock().unwrap();
            if rec.status.is_active() || rec.status == Status::Queued {
                rec.status = Status::Paused;
            }
            rec.active_connections = 0;
            rec.speed = 0.0;
        }
        self.inner.save();
        if let Some(path) = self.inner.ipc_path.lock().unwrap().take() {
            let _ = std::fs::remove_file(path);
        }
    }
}

impl Drop for Engine {
    fn drop(&mut self) {
        self.shutdown();
    }
}

impl Job {
    fn new(rec: Download) -> Self {
        let downloaded = rec.downloaded;
        let limiter = rate::RateLimiter::new(rec.speed_limit);
        Self {
            rec: Mutex::new(rec),
            cancel: Mutex::new(CancellationToken::new()),
            limiter,
            pid: Mutex::new(None),
            restart: AtomicBool::new(false),
            last_sample: Mutex::new(downloaded),
        }
    }

    /// Stop a running yt-dlp right now (synchronously: the app may exit
    /// before any async cleanup gets to run).
    pub(crate) fn kill_child(&self) {
        if let Some(pid) = self.pid.lock().unwrap().take() {
            media::kill_group(pid);
        }
    }

    /// Stop and immediately queue again (picks up new limits).
    fn restart(&self) {
        self.restart.store(true, Ordering::Relaxed);
        self.cancel.lock().unwrap().cancel();
        self.kill_child();
    }
}

impl Inner {
    pub(crate) fn job(&self, id: u64) -> Option<Arc<Job>> {
        self.jobs
            .lock()
            .unwrap()
            .iter()
            .find(|j| j.rec.lock().unwrap().id == id)
            .cloned()
    }

    pub(crate) fn mark_dirty(&self) {
        self.dirty.store(true, Ordering::Relaxed);
    }

    /// Start queued downloads while there is room. Must run inside the runtime.
    pub(crate) fn schedule(self: &Arc<Self>) {
        let max_active = self.settings.lock().unwrap().max_active.max(1);
        let jobs = self.jobs.lock().unwrap().clone();
        let mut active = jobs
            .iter()
            .filter(|j| j.rec.lock().unwrap().status.is_active())
            .count();
        for job in jobs {
            if active >= max_active {
                break;
            }
            let token = {
                let mut rec = job.rec.lock().unwrap();
                if rec.status != Status::Queued {
                    continue;
                }
                rec.status = Status::Probing;
                let token = CancellationToken::new();
                *job.cancel.lock().unwrap() = token.clone();
                token
            };
            active += 1;
            tokio::spawn(job::run(self.clone(), job, token));
        }
    }

    pub(crate) fn save(&self) {
        let records: Vec<Download> = self
            .jobs
            .lock()
            .unwrap()
            .iter()
            .map(|j| j.rec.lock().unwrap().clone())
            .collect();
        if let Ok(json) = serde_json::to_vec_pretty(&records) {
            let tmp = self.store_path.with_extension("json.tmp");
            let written = util::create_private_file(&tmp).and_then(|mut f| {
                std::io::Write::write_all(&mut f, &json)?;
                f.sync_all()
            });
            if written.is_ok() {
                let _ = std::fs::rename(&tmp, &self.store_path);
            }
        }
    }

    fn save_settings(&self) {
        let s = self.settings.lock().unwrap().clone();
        if let Ok(json) = serde_json::to_vec_pretty(&s) {
            let _ = util::write_private(&self.settings_path, &json);
        }
    }
}

/// Speed meter + periodic persistence.
async fn ticker(inner: Arc<Inner>) {
    const TICK: Duration = Duration::from_millis(500);
    let mut since_save = Duration::ZERO;
    let mut interval = tokio::time::interval(TICK);
    loop {
        interval.tick().await;
        let jobs = inner.jobs.lock().unwrap().clone();
        let mut any_active = false;
        for job in jobs {
            let mut rec = job.rec.lock().unwrap();
            let mut last = job.last_sample.lock().unwrap();
            let delta = rec.downloaded.saturating_sub(*last) as f64 / TICK.as_secs_f64();
            *last = rec.downloaded;
            if rec.status == Status::Downloading {
                any_active = true;
                // Exponential moving average: responsive but not jumpy.
                rec.speed = if rec.speed == 0.0 {
                    delta
                } else {
                    rec.speed * 0.7 + delta * 0.3
                };
            } else {
                rec.speed = 0.0;
            }
        }
        since_save += TICK;
        if since_save >= Duration::from_secs(2)
            && (any_active || inner.dirty.swap(false, Ordering::Relaxed))
        {
            since_save = Duration::ZERO;
            let inner = inner.clone();
            let _ = tokio::task::spawn_blocking(move || inner.save()).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn legacy_data_dir_moves_once() {
        let base = std::env::temp_dir().join(format!("hurtle-mig-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let (old, new) = (base.join("surge"), base.join("hurtle"));
        std::fs::create_dir_all(old.join("media/1")).unwrap();
        std::fs::write(old.join("downloads.json"), "[]").unwrap();
        migrate_legacy_dir(&old, &new);
        assert!(!old.exists());
        assert!(new.join("downloads.json").is_file() && new.join("media/1").is_dir());

        // An existing new dir is never overwritten, and a dir without our
        // files (someone else's "surge") is left alone.
        std::fs::create_dir_all(&old).unwrap();
        std::fs::write(old.join("settings.json"), "{}").unwrap();
        migrate_legacy_dir(&old, &new);
        assert!(old.join("settings.json").is_file());
        let other = base.join("other");
        std::fs::create_dir_all(&other).unwrap();
        migrate_legacy_dir(&other, &base.join("fresh"));
        assert!(other.exists() && !base.join("fresh").exists());
        std::fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn folders_by_type() {
        // An absolute path on this OS ("/pics" has no drive on Windows).
        let abs = |p: &str| {
            if cfg!(windows) {
                format!("C:{p}")
            } else {
                p.to_string()
            }
        };
        let mut s = Settings {
            download_dir: "/dl".into(),
            ..Default::default()
        };
        let change = ipc::SettingsChange {
            type_dirs: Some(BTreeMap::from([
                ("video".to_string(), abs("/media/videos")),
                ("image".to_string(), abs("/pics")),
            ])),
            ..Default::default()
        };
        assert_eq!(s.apply(&change), Ok(true));
        assert_eq!(
            s.dir_for(util::category("clip.MP4")),
            PathBuf::from(abs("/media/videos"))
        );
        assert_eq!(
            s.dir_for(util::category("a.png")),
            PathBuf::from(abs("/pics"))
        );
        // No folder of its own, or no known type: the main folder.
        assert_eq!(s.dir_for(util::category("a.zip")), PathBuf::from("/dl"));
        assert_eq!(s.dir_for(util::category("README")), PathBuf::from("/dl"));
        assert_eq!(
            s.dir_for(Some(media::category(Some(media::AUDIO), None))),
            PathBuf::from("/dl")
        );
        assert_eq!(
            s.dir_for(Some(media::category(None, None))),
            PathBuf::from(abs("/media/videos"))
        );

        // An empty path goes back to the main folder.
        let reset = ipc::SettingsChange {
            type_dirs: Some(BTreeMap::from([("image".to_string(), String::new())])),
            ..Default::default()
        };
        s.apply(&reset).unwrap();
        assert_eq!(s.dir_for(Some("image")), PathBuf::from("/dl"));

        // Relative paths and unknown types are refused; ~ is the home folder.
        let bad = |dir: &str, cat: Option<&str>| {
            let mut c = ipc::SettingsChange::default();
            match cat {
                Some(cat) => {
                    c.type_dirs = Some(BTreeMap::from([(cat.to_string(), dir.to_string())]))
                }
                None => c.download_dir = Some(dir.to_string()),
            }
            s.clone().apply(&c)
        };
        assert!(bad("videos", None).is_err());
        assert!(bad("/x", Some("fonts")).is_err());
        let mut home = s.clone();
        home.apply(&ipc::SettingsChange {
            download_dir: Some("~/Downloads/hurtle".into()),
            ..Default::default()
        })
        .unwrap();
        assert_eq!(
            home.download_dir,
            dirs::home_dir().unwrap().join("Downloads/hurtle")
        );

        // Old settings files (no type_dirs) still load.
        let old: Settings =
            serde_json::from_str(r#"{"download_dir":"/dl","connections":4}"#).unwrap();
        assert!(old.type_dirs.is_empty());
    }

    fn paused(url: &str, kind: Kind) -> Download {
        Download {
            id: 1,
            url: url.into(),
            final_url: None,
            headers: vec![],
            filename: "clip.mp4".into(),
            dir: "/dl".into(),
            auto_dir: true,
            path: None,
            total: Some(50 << 20),
            resumable: true,
            etag: None,
            last_modified: None,
            segments: vec![],
            downloaded: 10 << 20,
            status: Status::Paused,
            connections: 8,
            speed_limit: 0,
            added_at: 0,
            finished_at: None,
            kind,
            format: None,
            audio_lang: None,
            container: None,
            page_url: Some("https://site.example/watch/1".into()),
            torrent: None,
            note: None,
            speed: 0.0,
            active_connections: 0,
        }
    }

    fn want(url: &str, kind: Kind) -> Want<'_> {
        Want {
            url,
            kind,
            page: None,
            filename: None,
            size: None,
            format: None,
            audio_lang: None,
            container: None,
        }
    }

    #[test]
    fn relink_file_by_size_and_name_or_page() {
        let d = paused("https://cdn.example/a.mp4?token=old", Kind::File);
        let fresh = "https://cdn2.example/b.mp4?token=new";
        // Same size alone isn't enough.
        let w = Want {
            size: Some(50 << 20),
            ..want(fresh, Kind::File)
        };
        assert!(!same_download(&d, &w));
        let w = Want {
            size: Some(50 << 20),
            filename: Some("clip.mp4".into()),
            ..want(fresh, Kind::File)
        };
        assert!(same_download(&d, &w));
        let w = Want {
            size: Some(50 << 20),
            page: d.page_url.clone(),
            ..want(fresh, Kind::File)
        };
        assert!(same_download(&d, &w));
        // Same link with a new token.
        let w = Want {
            size: Some(50 << 20),
            ..want("https://cdn.example/a.mp4?token=new", Kind::File)
        };
        assert!(same_download(&d, &w));
        // Another size, or no size: a different file.
        let w = Want {
            size: Some(49 << 20),
            page: d.page_url.clone(),
            ..want(fresh, Kind::File)
        };
        assert!(!same_download(&d, &w));
        let w = Want {
            page: d.page_url.clone(),
            ..want(fresh, Kind::File)
        };
        assert!(!same_download(&d, &w));
        // The very same link always matches.
        assert!(same_download(&d, &want(&d.url, Kind::File)));
    }

    #[test]
    fn relink_only_unfinished() {
        let mut d = paused("https://cdn.example/a.mp4", Kind::File);
        let w = Want {
            size: Some(50 << 20),
            filename: Some("clip.mp4".into()),
            ..want("https://x.example/a.mp4", Kind::File)
        };
        for (status, ok) in [
            (Status::Failed("server answered 403 Forbidden".into()), true),
            (Status::Queued, true),
            (Status::Completed, false),
            (Status::Downloading, false),
            (Status::Probing, false),
        ] {
            d.status = status;
            assert_eq!(same_download(&d, &w), ok, "{:?}", d.status);
        }
    }

    #[test]
    fn relink_media_by_page_name_and_options() {
        let d = paused("https://cdn.example/hls/master.m3u8?sig=old", Kind::Media);
        let fresh = "https://cdn.example/hls/v2/master.m3u8?sig=new";
        let w = Want {
            page: d.page_url.clone(),
            filename: Some("clip.mp4".into()),
            ..want(fresh, Kind::Media)
        };
        assert!(same_download(&d, &w));
        // Another quality is another download.
        let w2 = Want {
            format: Some("bv*[height<=480]+ba"),
            ..w
        };
        assert!(!same_download(&d, &w2));
        // Same manifest path with a new signature.
        assert!(same_download(
            &d,
            &want("https://cdn.example/hls/master.m3u8?sig=new", Kind::Media)
        ));
        // A file request never takes over a video download.
        assert!(!same_download(
            &d,
            &Want {
                size: Some(50 << 20),
                ..want(&d.url, Kind::File)
            }
        ));
    }

    #[test]
    fn bad_store_records_are_skipped_and_backed_up() {
        let dir = std::env::temp_dir().join(format!("hurtle-store-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("downloads.json");
        // Missing file: empty, and nothing to back up.
        assert!(read_records(&path).is_empty());
        assert!(!dir.join("downloads.json.bak").exists());

        let good = serde_json::to_value(paused("https://h/a.bin", Kind::File)).unwrap();
        let text = serde_json::to_vec(&vec![good, serde_json::json!({"id": "bad"})]).unwrap();
        std::fs::write(&path, &text).unwrap();
        assert_eq!(read_records(&path).len(), 1);
        assert_eq!(std::fs::read(dir.join("downloads.json.bak")).unwrap(), text);

        // Truncated file: nothing readable, original bytes kept.
        std::fs::remove_file(dir.join("downloads.json.bak")).unwrap();
        std::fs::write(&path, &text[..text.len() / 2]).unwrap();
        assert!(read_records(&path).is_empty());
        assert_eq!(
            std::fs::read(dir.join("downloads.json.bak")).unwrap(),
            &text[..text.len() / 2]
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
