//! Hurtle download engine.
//!
//! A download is split into byte-range *segments*, each fetched over its own
//! HTTP/1.1 connection. When a connection finishes its segment it steals the
//! back half of the largest remaining one (dynamic segmentation), so every
//! connection stays busy until the very end. Servers that throttle per
//! connection therefore deliver roughly `connections ×` the single-stream speed.

pub mod ipc;
mod job;
pub mod log;
pub mod media;
pub mod nmh;
pub mod rate;
mod sched;
pub mod tools;
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
    /// Downloads allowed to run at once; the rest wait as `Queued`.
    pub max_active: usize,
    /// Global speed limit across all downloads, bytes/s (0 = unlimited).
    pub speed_limit: u64,
    /// App theme: "system" (follow the desktop's palette), "light", "dark",
    /// or the id of a built-in preset.
    pub theme: String,
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
            max_active: 4,
            speed_limit: 0,
            theme: "system".into(),
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
        std::fs::read(data_dir().join("settings.json"))
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default()
    }

    pub fn save(&self) -> std::io::Result<()> {
        let dir = data_dir();
        std::fs::create_dir_all(&dir)?;
        std::fs::write(dir.join("settings.json"), serde_json::to_vec_pretty(self)?)
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
fn link_path(url: &str) -> &str {
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

impl Engine {
    /// Load persisted downloads/settings from [`data_dir`] and start the runtime.
    pub fn new() -> Result<Self> {
        Self::with_data_dir(&data_dir())
    }

    pub fn with_data_dir(dir: &Path) -> Result<Self> {
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        let store_path = dir.join("downloads.json");
        let settings_path = dir.join("settings.json");

        let settings: Settings = std::fs::read(&settings_path)
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default();
        let mut records: Vec<Download> = std::fs::read(&store_path)
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default();
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

    /// Queue a download and return its id.
    pub fn add(&self, req: AddRequest) -> Result<u64> {
        self.add_or_relink(req).map(|(id, _)| id)
    }

    /// Queue a download, or, when it is a fresh link to a paused or failed
    /// download already in the list (see [`same_download`]), give that one
    /// the new link and resume it. The flag says which happened.
    pub fn add_or_relink(&self, req: AddRequest) -> Result<(u64, bool)> {
        let url = req.url.trim().to_string();
        let parsed = reqwest::Url::parse(&url).with_context(|| format!("invalid URL: {url}"))?;
        anyhow::ensure!(
            matches!(parsed.scheme(), "http" | "https"),
            "only http(s) URLs are supported"
        );
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
            Kind::File => req.filename.as_deref().and_then(util::category),
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
            if changed && rec.kind == Kind::Media && rec.status.is_active() {
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
        if rec.kind == Kind::Media {
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
                    let next = match ServerOptions::new().create(&name) {
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
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    let (r, mut w) = tokio::io::split(stream);
    let mut lines = BufReader::new(r).lines();
    while let Ok(Some(line)) = lines.next_line().await {
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

async fn handle_request(engine: &Engine, req: &ipc::IpcRequest) -> ipc::IpcReply {
    match req {
        ipc::IpcRequest::Add(add) => match engine.add_or_relink(add.clone()) {
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
            if std::fs::write(&tmp, json).is_ok() {
                let _ = std::fs::rename(&tmp, &self.store_path);
            }
        }
    }

    fn save_settings(&self) {
        let s = self.settings.lock().unwrap().clone();
        if let Ok(json) = serde_json::to_vec_pretty(&s) {
            let _ = std::fs::write(&self.settings_path, json);
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
}
