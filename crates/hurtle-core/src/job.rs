//! Running one download: probe, then N segment workers with dynamic splitting.

use std::fs::{File, OpenOptions};
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use futures_util::StreamExt;
use reqwest::header::{
    ACCEPT_ENCODING, CONTENT_DISPOSITION, CONTENT_RANGE, CONTENT_TYPE, ETAG, HeaderMap, HeaderName,
    HeaderValue, IF_RANGE, LAST_MODIFIED, RANGE, RETRY_AFTER,
};
use reqwest::{Response, StatusCode};
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;

use crate::rate::RateLimiter;
use crate::sched::{self, Plan, Ramp, SegView, Tuning};
use crate::{Inner, Job, Kind, Segment, Status, media, torrent, util};

pub(crate) async fn run(inner: Arc<Inner>, job: Arc<Job>, token: CancellationToken) {
    let result = run_with_retries(&inner, &job, &token).await;
    job.kill_child();
    {
        let mut rec = job.rec.lock().unwrap();
        rec.active_connections = 0;
        rec.speed = 0.0;
        rec.note = None;
        rec.status = match result {
            Ok(()) => {
                rec.finished_at = Some(util::now_secs());
                Status::Completed
            }
            // Stopped only to pick up new settings: straight back in the queue.
            Err(_) if token.is_cancelled() && job.restart.swap(false, Ordering::Relaxed) => {
                Status::Queued
            }
            Err(_) if token.is_cancelled() => Status::Paused,
            Err(e) => Status::Failed(e),
        };
        let what = format!("#{} {}", rec.id, crate::log::url(&rec.url));
        match &rec.status {
            Status::Completed => crate::log::log(
                "app",
                format!(
                    "{what} completed: {} ({} bytes)",
                    rec.path
                        .as_ref()
                        .map(|p| p.display().to_string())
                        .unwrap_or_default(),
                    rec.downloaded
                ),
            ),
            Status::Failed(e) => crate::log::log("app", format!("{what} FAILED: {e}")),
            s => crate::log::log("app", format!("{what} stopped: {}", s.label())),
        }
    }
    inner.mark_dirty();
    let saver = inner.clone();
    let _ = tokio::task::spawn_blocking(move || saver.save()).await;
    inner.schedule();
}

/// The remote file changed while we held part of it.
pub(crate) const FILE_CHANGED: &str = "the file changed on the server";

/// Automatic retries after a network failure: 30 s, 1 min, 2 min, 5 min, 5 min.
const AUTO_RETRY_DELAYS: [u64; 5] = [30, 60, 120, 300, 300];

/// How long to wait before retry number `attempt` (1-based); None when used up.
fn auto_retry_delay(attempt: u32) -> Option<Duration> {
    let i = (attempt as usize).checked_sub(1)?;
    AUTO_RETRY_DELAYS.get(i).map(|s| Duration::from_secs(*s))
}

/// Whether an error is the network's fault and worth trying again later:
/// dropped or refused connections, timeouts, 5xx, 408 and 429. Missing files
/// (404, 403, 410…), disk errors and protocol surprises stay failures.
fn is_transient(e: &str) -> bool {
    let e = e.to_ascii_lowercase();
    if let Some(rest) = e.strip_prefix("server answered ") {
        let code: u16 = rest
            .split_whitespace()
            .next()
            .and_then(|c| c.parse().ok())
            .unwrap_or(0);
        return code == 408 || code == 429 || (500..600).contains(&code);
    }
    if e.starts_with("write failed") || e.contains("no space left") {
        return false;
    }
    // Certificate, TLS and redirect-policy failures will not heal by waiting.
    if [
        "certificate",
        "invalid peer",
        "tls handshake",
        "handshake",
        "unknownissuer",
        "redirect",
    ]
    .iter()
    .any(|m| e.contains(m))
    {
        return false;
    }
    [
        "timed out",
        "connection",
        "error decoding response body",
        "error reading a body",
        "broken pipe",
        "dns error",
        "unreachable",
    ]
    .iter()
    .any(|m| e.contains(m))
}

/// `drive` (or yt-dlp) until it succeeds, with two kinds of second chances:
/// a changed remote file restarts from zero, and a network failure waits and
/// tries again (resuming from the segments already on disk).
async fn run_with_retries(
    inner: &Arc<Inner>,
    job: &Arc<Job>,
    token: &CancellationToken,
) -> Result<(), String> {
    let mut retries = 0u32;
    let mut restarts = 0u32;
    loop {
        let (kind, before) = {
            let rec = job.rec.lock().unwrap();
            (rec.kind, rec.downloaded)
        };
        let result = match kind {
            Kind::Media => media::drive(inner, job, token).await,
            // librqbit keeps looking for peers itself: no retry loop here.
            Kind::Torrent => return torrent::drive(inner, job, token).await,
            Kind::File => drive(inner, job, token).await,
        };
        let Err(e) = result else { return Ok(()) };
        if token.is_cancelled() {
            return Err(e);
        }
        if e == FILE_CHANGED && restarts < 2 {
            restarts += 1;
            let mut rec = job.rec.lock().unwrap();
            rec.segments.clear();
            rec.downloaded = 0;
            rec.total = None;
            rec.etag = None;
            rec.last_modified = None;
            continue;
        }
        let auto = inner.settings.lock().unwrap().auto_retry;
        let kind_media = job.rec.lock().unwrap().kind == Kind::Media;
        // yt-dlp has retries of its own.
        if !auto || kind_media || !is_transient(&e) {
            return Err(e);
        }
        if job.rec.lock().unwrap().downloaded > before {
            retries = 0;
        }
        retries += 1;
        let Some(delay) = auto_retry_delay(retries) else {
            return Err(e);
        };
        crate::log::log(
            "app",
            format!(
                "#{} {e}; retry {retries}/{} in {}s",
                job.rec.lock().unwrap().id,
                AUTO_RETRY_DELAYS.len(),
                delay.as_secs()
            ),
        );
        let until = Instant::now() + delay;
        loop {
            let left = until.saturating_duration_since(Instant::now());
            {
                let mut rec = job.rec.lock().unwrap();
                rec.speed = 0.0;
                rec.active_connections = 0;
                rec.note = Some(format!(
                    "Connection lost, retrying in {} (attempt {retries}/{})",
                    util::fmt_duration(left.as_secs() + 1),
                    AUTO_RETRY_DELAYS.len()
                ));
            }
            if left.is_zero() {
                break;
            }
            tokio::select! {
                _ = token.cancelled() => return Err("cancelled".into()),
                _ = tokio::time::sleep(left.min(Duration::from_secs(1))) => {}
            }
        }
        job.rec.lock().unwrap().note = None;
    }
}

/// The stored validators agree with the server's now. The ETag is compared
/// when both sides have it; Last-Modified only when neither side has an ETag,
/// since some servers send an unstable Last-Modified but still honour Range.
fn same_validators(
    old_etag: &Option<String>,
    etag: &Option<String>,
    old_modified: &Option<String>,
    modified: &Option<String>,
) -> bool {
    let agree = |a: &Option<String>, b: &Option<String>| a.is_none() || b.is_none() || a == b;
    if old_etag.is_some() && etag.is_some() {
        return old_etag == etag;
    }
    if old_etag.is_none() && etag.is_none() {
        return agree(old_modified, modified);
    }
    true
}

/// `Content-Range: bytes 0-0/N`: N, if the server knows it.
fn range_total(h: &HeaderMap) -> Option<u64> {
    h.get(CONTENT_RANGE)
        .and_then(|v| v.to_str().ok())
        .and_then(util::total_from_content_range)
}

/// The first request: `Range: bytes=0-0` (or, as a fallback, a plain GET).
/// Connect errors, timeouts, 429 and 502-504 are tried again a couple of times.
async fn send_probe(
    inner: &Inner,
    token: &CancellationToken,
    url: &str,
    headers: &HeaderMap,
    range: bool,
) -> Result<Response, String> {
    let mut attempt = 0u32;
    loop {
        let mut req = inner.client.get(url).headers(headers.clone());
        if range {
            req = req.header(RANGE, "bytes=0-0");
        }
        let sent = tokio::select! {
            _ = token.cancelled() => return Err("cancelled".into()),
            r = req.send() => r,
        };
        let backoff = Duration::from_secs(1 << attempt);
        let wait = match sent {
            Ok(r) if attempt < 2 && probe_retryable(r.status()) => r
                .headers()
                .get(RETRY_AFTER)
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.trim().parse::<u64>().ok())
                .map_or(backoff, |s| Duration::from_secs(s.min(30))),
            Ok(r) => return Ok(r),
            Err(e) if attempt < 2 && (e.is_connect() || e.is_timeout()) => backoff,
            Err(e) => return Err(describe(&e)),
        };
        attempt += 1;
        tokio::select! {
            _ = token.cancelled() => return Err("cancelled".into()),
            _ = tokio::time::sleep(wait) => {}
        }
    }
}

fn probe_retryable(s: StatusCode) -> bool {
    s == StatusCode::TOO_MANY_REQUESTS
        || s == StatusCode::BAD_GATEWAY
        || s == StatusCode::SERVICE_UNAVAILABLE
        || s == StatusCode::GATEWAY_TIMEOUT
}

/// Paths other downloads have claimed. Takes `jobs` and then each record,
/// the allowed lock order; call it with no record lock held.
fn sibling_paths(inner: &Inner, my_id: u64) -> Vec<std::path::PathBuf> {
    inner
        .jobs
        .lock()
        .unwrap()
        .iter()
        .filter_map(|j| {
            let r = j.rec.lock().unwrap();
            (r.id != my_id).then(|| r.path.clone()).flatten()
        })
        .collect()
}

/// Move the finished `.part` into place. If someone created the target in
/// the meantime, take the next free name instead of overwriting it.
fn place(
    inner: &Inner,
    job: &Job,
    part: &std::path::Path,
    path: std::path::PathBuf,
) -> Result<(), String> {
    let _claim = inner.path_claim.lock().unwrap();
    let mut path = path;
    if path.exists() {
        let my_id = job.rec.lock().unwrap().id;
        let siblings = sibling_paths(inner, my_id);
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "download".into());
        let dir = path.parent().unwrap_or(std::path::Path::new("."));
        path = util::unique_path(dir, &name, |p| siblings.iter().any(|s| s == p));
        let mut rec = job.rec.lock().unwrap();
        rec.filename = name_of(&path).unwrap_or_else(|| rec.filename.clone());
        rec.path = Some(path.clone());
    }
    std::fs::rename(part, &path).map_err(|e| format!("renaming to {}: {e}", path.display()))
}

fn name_of(path: &std::path::Path) -> Option<String> {
    path.file_name().map(|n| n.to_string_lossy().into_owned())
}

async fn drive(
    inner: &Arc<Inner>,
    job: &Arc<Job>,
    token: &CancellationToken,
) -> Result<(), String> {
    let (url, headers) = {
        let rec = job.rec.lock().unwrap();
        (rec.url.clone(), build_headers(&rec.headers)?)
    };

    // ---- Probe: one byte via Range tells us size, resumability and the name.
    let mut probe = send_probe(inner, token, &url, &headers, true).await?;
    // Empty files answer 416, and some servers refuse Range outright: ask
    // again without it (a plain, single-stream download).
    if matches!(probe.status().as_u16(), 400 | 405 | 416 | 501) {
        probe = send_probe(inner, token, &url, &headers, false).await?;
    }
    // "206 … bytes 0-0/*": the body is one byte of an unknown whole. Fetch it
    // all in one stream instead.
    if probe.status() == StatusCode::PARTIAL_CONTENT && range_total(probe.headers()).is_none() {
        probe = send_probe(inner, token, &url, &headers, false).await?;
    }
    let status = probe.status();
    if !(status == StatusCode::PARTIAL_CONTENT || status.is_success()) {
        return Err(format!("server answered {status}"));
    }
    let final_url = probe.url().to_string();
    // Cookies were gathered for the original site: don't replay them to a
    // different host the link redirected to.
    let seg_headers = match reqwest::Url::parse(&url) {
        Ok(orig) => util::headers_for_redirect(&orig, probe.url(), &headers),
        Err(_) => headers.clone(),
    };
    let h = probe.headers();

    // A web page or a stream manifest rather than a file: a video page link
    // (YouTube…) or HLS/DASH. yt-dlp handles those.
    let ctype = header_str(h, CONTENT_TYPE)
        .unwrap_or_default()
        .to_ascii_lowercase();
    let attachment = header_str(h, CONTENT_DISPOSITION).is_some_and(|d| {
        d.trim_start()
            .to_ascii_lowercase()
            .starts_with("attachment")
    });
    // A .torrent behind a link without the extension: fetch it over BitTorrent.
    if ctype.starts_with("application/x-bittorrent") {
        drop(probe);
        {
            let mut rec = job.rec.lock().unwrap();
            rec.kind = Kind::Torrent;
            rec.torrent.get_or_insert_with(Default::default);
        }
        return torrent::drive(inner, job, token).await;
    }
    let page = ctype.starts_with("text/html") && !attachment;
    if (page || ctype.contains("mpegurl") || ctype.contains("dash+xml")) && media::ytdlp_available()
    {
        drop(probe);
        job.rec.lock().unwrap().kind = Kind::Media;
        return media::drive(inner, job, token).await;
    }

    let ranged = status == StatusCode::PARTIAL_CONTENT;
    let total = if ranged {
        range_total(h)
    } else {
        probe.content_length()
    };
    let resumable = ranged && total.is_some();
    let etag = header_str(h, ETAG);
    let last_modified = header_str(h, LAST_MODIFIED);
    let disposition_name = header_str(h, CONTENT_DISPOSITION)
        .as_deref()
        .and_then(util::filename_from_disposition);

    // ---- Reconcile with what we had (resume), pick the path.
    let settings = inner.settings.lock().unwrap().clone();
    // Validator for `If-Range`: a changed file then answers 200, not 206.
    let if_range = if resumable {
        etag.as_deref()
            .filter(|e| !e.starts_with("W/"))
            .and_then(|v| HeaderValue::from_str(v).ok())
    } else {
        None
    };
    let (path, segments_fresh) = {
        // One download at a time chooses its name, so two of the same name
        // can't both take it. Lock order: claim, `jobs`, then a record.
        let _claim = inner.path_claim.lock().unwrap();
        let my_id = job.rec.lock().unwrap().id;
        let siblings = sibling_paths(inner, my_id);
        let mut rec = job.rec.lock().unwrap();
        let same_file = rec.total == total
            && rec.resumable
            && resumable
            && same_validators(&rec.etag, &etag, &rec.last_modified, &last_modified)
            && rec
                .path
                .as_ref()
                .is_some_and(|p| util::part_path(p).exists());
        if !same_file {
            rec.segments.clear();
            rec.downloaded = 0;
        }
        if rec.filename.is_empty() {
            let name = disposition_name
                .or_else(|| util::filename_from_url(&final_url))
                .or_else(|| util::filename_from_url(&rec.url))
                .unwrap_or_else(|| "download".into());
            rec.filename = util::sanitize_filename(&name);
        }
        if rec.auto_dir && rec.path.is_none() {
            // Now that the name is known, its type picks the folder.
            rec.dir = settings.dir_for(util::category(&rec.filename));
        }
        if rec.path.is_none() || !same_file {
            // Re-use our own previous path when restarting from scratch.
            let path = match rec.path.clone() {
                Some(p) if !p.exists() => p,
                _ => {
                    util::unique_path(&rec.dir, &rec.filename, |p| siblings.iter().any(|s| s == p))
                }
            };
            rec.filename = path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| rec.filename.clone());
            rec.path = Some(path);
        }
        rec.final_url = Some(final_url.clone());
        rec.total = total;
        rec.resumable = resumable;
        rec.etag = etag;
        rec.last_modified = last_modified;
        rec.status = Status::Downloading;
        (rec.path.clone().unwrap(), !same_file)
    };
    inner.mark_dirty();

    let part = util::part_path(&path);
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    }
    let file = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(segments_fresh || !resumable)
        .open(&part)
        .map_err(|e| format!("{}: {e}", part.display()))?;

    if resumable {
        // Read the 1-byte probe body to its end so the connection goes back
        // to the pool and the first segment can reuse it.
        let _ = tokio::time::timeout(Duration::from_secs(2), probe.bytes()).await;
        let total = total.unwrap_or(0);
        if segments_fresh && total > 0 {
            // Reserve the space up front; segments write in place.
            file.set_len(total)
                .map_err(|e| format!("allocating {total} bytes: {e}"))?;
        }
        let connections = {
            let mut rec = job.rec.lock().unwrap();
            if rec.segments.is_empty() && total > 0 {
                rec.segments.push(Segment {
                    start: 0,
                    end: total,
                    pos: 0,
                    ..Default::default()
                });
            }
            for s in &mut rec.segments {
                s.owned = false;
            }
            rec.downloaded = rec.segments.iter().map(|s| s.pos - s.start).sum();
            rec.connections
        };
        multi(
            inner,
            job,
            token,
            Arc::new(file),
            final_url,
            seg_headers,
            connections,
            if_range,
        )
        .await?;
    } else {
        // No Range support: plain single stream (the probe response *is* the body).
        {
            let mut rec = job.rec.lock().unwrap();
            rec.segments = vec![Segment {
                start: 0,
                end: total.unwrap_or(0),
                pos: 0,
                owned: true,
                ..Default::default()
            }];
            rec.downloaded = 0;
            rec.active_connections = 1;
        }
        single(job, token, &file, probe, &inner.global_limit).await?;
        file.sync_data().map_err(|e| e.to_string())?;
    }

    place(inner, job, &part, path)?;
    Ok(())
}

/// Shared by the workers of one download: how many connections are alive and
/// how many the server seems to tolerate.
struct Ctl {
    live: AtomicUsize,
    cap: AtomicUsize,
    pressured: std::sync::atomic::AtomicBool,
    last_cut: Mutex<Option<Instant>>,
}

impl Ctl {
    /// The server pushed back (429/503/refused connection): lower the cap.
    fn pressure(&self) {
        let mut last = self.last_cut.lock().unwrap();
        if last.is_some_and(|t| t.elapsed() < Duration::from_secs(3)) {
            return;
        }
        *last = Some(Instant::now());
        let live = self.live.load(Ordering::Relaxed);
        let cap = self.cap.load(Ordering::Relaxed).min(live.max(1));
        self.cap.store((cap / 2).max(1), Ordering::Relaxed);
        self.pressured.store(true, Ordering::Relaxed);
    }

    /// Give up this connection if more are alive than allowed (never the last).
    fn retire(&self) -> bool {
        let cap = self.cap.load(Ordering::Relaxed);
        self.live
            .try_update(Ordering::Relaxed, Ordering::Relaxed, |l| {
                (l > cap && l > 1).then(|| l - 1)
            })
            .is_ok()
    }
}

/// Segmented download. `connections` is the maximum: workers are added while
/// aggregate throughput keeps improving, and shed when the server pushes back.
#[allow(clippy::too_many_arguments)]
async fn multi(
    inner: &Arc<Inner>,
    job: &Arc<Job>,
    token: &CancellationToken,
    file: Arc<File>,
    url: String,
    headers: HeaderMap,
    connections: usize,
    if_range: Option<HeaderValue>,
) -> Result<(), String> {
    let (min_segment, max_retries) = {
        let s = inner.settings.lock().unwrap();
        (s.min_segment.max(64 * 1024), s.max_retries)
    };
    let max = connections.max(1);
    let ctl = Arc::new(Ctl {
        live: AtomicUsize::new(0),
        cap: AtomicUsize::new(max),
        pressured: Default::default(),
        last_cut: Mutex::new(None),
    });
    let mut set = JoinSet::new();
    let spawn = |set: &mut JoinSet<Result<(), String>>, stagger: u64| {
        let ctx = Worker {
            client: inner.client.clone(),
            job: job.clone(),
            file: file.clone(),
            url: url.clone(),
            headers: headers.clone(),
            if_range: if_range.clone(),
            token: token.clone(),
            tuning: Tuning {
                min_segment,
                min_eta: 2.0,
                stall: Duration::from_secs(10),
            },
            max_retries,
            global: inner.global_limit.clone(),
            ctl: ctl.clone(),
            speed: AtomicU64::new(0),
        };
        ctl.live.fetch_add(1, Ordering::Relaxed);
        set.spawn(async move {
            // Stagger connection setup a little so we don't hammer the server
            // with N simultaneous handshakes (some CDNs punish that).
            tokio::time::sleep(Duration::from_millis(80 * stagger)).await;
            ctx.run().await
        });
    };
    // Start with a few; the ramp below adds more while they pay off.
    for n in 0..max.min(4) {
        spawn(&mut set, n as u64);
    }
    let window = Duration::from_secs(2);
    let mut tick = tokio::time::interval_at(tokio::time::Instant::now() + window, window);
    let mut ramp = Ramp::new();
    let mut last_bytes = job.rec.lock().unwrap().downloaded;
    let mut last_err = None;
    loop {
        tokio::select! {
            res = set.join_next() => match res {
                None => break,
                Some(Ok(Err(e))) if e == FILE_CHANGED => {
                    // Bytes already written are from the old file: stop everyone.
                    set.abort_all();
                    return Err(e);
                }
                Some(Ok(Err(e))) => last_err = Some(e),
                Some(_) => {}
            },
            _ = tick.tick() => {
                let now = job.rec.lock().unwrap().downloaded;
                let tput = now.saturating_sub(last_bytes) as f64 / window.as_secs_f64();
                last_bytes = now;
                if ctl.pressured.swap(false, Ordering::Relaxed) {
                    ramp.stop();
                }
                let live = ctl.live.load(Ordering::Relaxed);
                let cap = ctl.cap.load(Ordering::Relaxed).min(max);
                for n in 0..ramp.step(tput, live, cap) {
                    spawn(&mut set, n as u64);
                }
            }
        }
    }
    if token.is_cancelled() {
        return Err("cancelled".into());
    }
    let left: u64 = job
        .rec
        .lock()
        .unwrap()
        .segments
        .iter()
        .map(Segment::remaining)
        .sum();
    if left > 0 {
        return Err(last_err.unwrap_or_else(|| format!("{left} bytes could not be downloaded")));
    }
    file.sync_data().map_err(|e| e.to_string())?;
    Ok(())
}

struct Worker {
    client: reqwest::Client,
    job: Arc<Job>,
    file: Arc<File>,
    url: String,
    headers: HeaderMap,
    /// Sent as `If-Range` with every segment request.
    if_range: Option<HeaderValue>,
    token: CancellationToken,
    tuning: Tuning,
    max_retries: u32,
    global: Arc<RateLimiter>,
    ctl: Arc<Ctl>,
    /// This connection's smoothed speed (bytes/s), carried across segments.
    speed: AtomicU64,
}

enum SegError {
    Cancelled,
    /// Worth retrying (network hiccup, 5xx, 429).
    Retry(String),
    /// Retrying won't help (403, server ignored Range, disk full).
    Fatal(String),
}

impl Worker {
    async fn run(self) -> Result<(), String> {
        let (result, retired) = self.work().await;
        if !retired {
            self.ctl.live.fetch_sub(1, Ordering::Relaxed);
        }
        result
    }

    /// Returns the outcome and whether this worker retired itself (and so
    /// already left the live count).
    async fn work(&self) -> (Result<(), String>, bool) {
        while let Some(idx) = self.claim() {
            let mut attempt = 0;
            let mut retired = false;
            let outcome = loop {
                let before = self.job.rec.lock().unwrap().segments[idx].pos;
                match self.fetch(idx).await {
                    Ok(()) => break Ok(()),
                    Err(SegError::Cancelled) => break Err(None),
                    Err(SegError::Fatal(e)) => break Err(Some(e)),
                    Err(SegError::Retry(e)) => {
                        // Only failures in a row without progress count.
                        if self.job.rec.lock().unwrap().segments[idx].pos > before {
                            attempt = 0;
                        }
                        attempt += 1;
                        // The server is pushing back and we are over the
                        // lowered cap: leave the segment to the others.
                        if self.ctl.retire() {
                            retired = true;
                            break Ok(());
                        }
                        if attempt > self.max_retries {
                            break Err(Some(e));
                        }
                        let backoff = Duration::from_millis(500 * (1 << attempt.min(5)));
                        tokio::select! {
                            _ = self.token.cancelled() => break Err(None),
                            _ = tokio::time::sleep(backoff) => {}
                        }
                    }
                }
            };
            // Hand the segment back either way; if it is unfinished another
            // worker will pick it up when it runs out of work.
            self.job.rec.lock().unwrap().segments[idx].owned = false;
            if retired {
                return (Ok(()), true);
            }
            match outcome {
                Ok(()) => continue,
                Err(None) => return (Ok(()), false),
                Err(Some(e)) => return (Err(e), false),
            }
        }
        (Ok(()), false)
    }

    /// Pick the next segment: an orphan, a stalled owner's remainder, or half
    /// (weighted by speed) of the segment that will take longest (IDM-style).
    fn claim(&self) -> Option<usize> {
        let mut rec = self.job.rec.lock().unwrap();
        let segs = &mut rec.segments;
        let now = Instant::now();
        let views: Vec<SegView> = segs
            .iter()
            .map(|s| SegView {
                remaining: s.remaining(),
                owned: s.owned,
                speed: s.speed,
                idle: s
                    .last_data
                    .map_or(Duration::ZERO, |t| now.duration_since(t)),
            })
            .collect();
        let mut helper = self.speed.load(Ordering::Relaxed);
        if helper == 0 {
            // A new connection: assume it is as fast as its peers.
            let known: Vec<u64> = views
                .iter()
                .filter(|v| v.owned && v.speed > 0)
                .map(|v| v.speed)
                .collect();
            if !known.is_empty() {
                helper = known.iter().sum::<u64>() / known.len() as u64;
            }
        }
        let plan = sched::plan(&views, helper, &self.tuning)?;
        let fresh = |start, end, pos| Segment {
            start,
            end,
            pos,
            owned: true,
            speed: helper,
            last_data: Some(now),
        };
        match plan {
            Plan::Take(i) => {
                segs[i].owned = true;
                segs[i].speed = helper;
                segs[i].last_data = Some(now);
                Some(i)
            }
            Plan::Takeover(i) => {
                let (pos, end) = (segs[i].pos, segs[i].end);
                segs[i].end = pos;
                segs.push(fresh(pos, end, pos));
                Some(segs.len() - 1)
            }
            Plan::Split { idx, at } => {
                let mid = segs[idx].pos + at;
                let end = segs[idx].end;
                segs[idx].end = mid;
                segs.push(fresh(mid, end, mid));
                Some(segs.len() - 1)
            }
        }
    }

    async fn fetch(&self, idx: usize) -> Result<(), SegError> {
        let (pos, end) = {
            let rec = self.job.rec.lock().unwrap();
            let s = rec.segments[idx];
            (s.pos, s.end)
        };
        if pos >= end {
            return Ok(());
        }
        let mut req = self
            .client
            .get(&self.url)
            .headers(self.headers.clone())
            .header(RANGE, format!("bytes={pos}-{}", end - 1));
        if let Some(v) = &self.if_range {
            req = req.header(IF_RANGE, v);
        }
        let resp = tokio::select! {
            _ = self.token.cancelled() => return Err(SegError::Cancelled),
            r = req.send() => r.map_err(|e| {
                if e.is_connect() {
                    self.ctl.pressure();
                }
                SegError::Retry(describe(&e))
            })?,
        };
        let status = resp.status();
        if status == StatusCode::TOO_MANY_REQUESTS || status == StatusCode::SERVICE_UNAVAILABLE {
            self.ctl.pressure();
        }
        if status == StatusCode::TOO_MANY_REQUESTS || status.is_server_error() {
            return Err(SegError::Retry(format!("server answered {status}")));
        }
        if status == StatusCode::OK && self.if_range.is_some() {
            return Err(SegError::Fatal(FILE_CHANGED.into()));
        }
        if status != StatusCode::PARTIAL_CONTENT {
            return Err(SegError::Fatal(if status.is_success() {
                "server stopped honouring byte ranges".into()
            } else {
                format!("server answered {status}")
            }));
        }
        let start = resp
            .headers()
            .get(CONTENT_RANGE)
            .and_then(|v| v.to_str().ok())
            .and_then(util::start_from_content_range);
        if start != Some(pos) {
            return Err(SegError::Fatal(format!(
                "server sent the wrong range (asked for {pos}, got {start:?})"
            )));
        }

        self.job.rec.lock().unwrap().active_connections += 1;
        let result = self.stream(idx, resp, end - pos).await;
        self.job.rec.lock().unwrap().active_connections -= 1;
        result
    }

    async fn stream(&self, idx: usize, resp: Response, req_len: u64) -> Result<(), SegError> {
        let mut body = resp.bytes_stream();
        let mut got = 0u64;
        // Speed sampling window: (start, bytes since).
        let mut win: Option<(Instant, u64)> = None;
        loop {
            let chunk = tokio::select! {
                _ = self.token.cancelled() => return Err(SegError::Cancelled),
                c = body.next() => c,
            };
            let chunk = match chunk {
                Some(Ok(c)) => c,
                Some(Err(e)) => return Err(SegError::Retry(describe(&e))),
                None => {
                    // Body ended; fine only if the segment is complete.
                    let rem = self.job.rec.lock().unwrap().segments[idx].remaining();
                    return if rem == 0 {
                        Ok(())
                    } else {
                        Err(SegError::Retry("connection closed early".into()))
                    };
                }
            };
            got += chunk.len() as u64;
            // Speed limits: per download first, then the global one.
            tokio::select! {
                _ = self.token.cancelled() => return Err(SegError::Cancelled),
                _ = async {
                    self.job.limiter.acquire(chunk.len()).await;
                    self.global.acquire(chunk.len()).await;
                } => {}
            }
            let done = {
                let mut rec = self.job.rec.lock().unwrap();
                let seg = &mut rec.segments[idx];
                // `end` may have shrunk since the request: another worker stole the tail.
                let n = (chunk.len() as u64).min(seg.remaining()) as usize;
                if n > 0 {
                    write_at(&self.file, &chunk[..n], seg.pos)
                        .map_err(|e| SegError::Fatal(format!("write failed: {e}")))?;
                    seg.pos += n as u64;
                    seg.last_data = Some(Instant::now());
                    rec.downloaded += n as u64;
                }
                // Smoothed speed of this connection, sampled every ~300 ms.
                match &mut win {
                    None => win = Some((Instant::now(), 0)),
                    Some((t0, bytes)) => {
                        *bytes += n as u64;
                        let dt = t0.elapsed();
                        if dt >= Duration::from_millis(300) {
                            let sample = (*bytes as f64 / dt.as_secs_f64()) as u64;
                            let v = sched::ewma(self.speed.load(Ordering::Relaxed), sample);
                            self.speed.store(v, Ordering::Relaxed);
                            rec.segments[idx].speed = v;
                            win = Some((Instant::now(), 0));
                        }
                    }
                }
                rec.segments[idx].remaining() == 0
            };
            if done {
                // Consume what is left of the response so the connection goes
                // back to the pool for the next range. If the segment was cut
                // short and a lot is still in flight, dropping it is cheaper.
                let unread = req_len.saturating_sub(got);
                if unread <= 256 * 1024 {
                    let _ = tokio::time::timeout(Duration::from_secs(2), async {
                        while let Some(Ok(_)) = body.next().await {}
                    })
                    .await;
                }
                return Ok(());
            }
        }
    }
}

/// Server without Range support: write the body sequentially.
async fn single(
    job: &Arc<Job>,
    token: &CancellationToken,
    file: &File,
    resp: Response,
    global: &RateLimiter,
) -> Result<(), String> {
    let mut body = resp.bytes_stream();
    let mut pos = 0u64;
    loop {
        let chunk = tokio::select! {
            _ = token.cancelled() => return Err("cancelled".into()),
            c = body.next() => c,
        };
        match chunk {
            Some(Ok(c)) => {
                tokio::select! {
                    _ = token.cancelled() => return Err("cancelled".into()),
                    _ = async {
                        job.limiter.acquire(c.len()).await;
                        global.acquire(c.len()).await;
                    } => {}
                }
                write_at(file, &c, pos).map_err(|e| format!("write failed: {e}"))?;
                pos += c.len() as u64;
                let mut rec = job.rec.lock().unwrap();
                rec.downloaded = pos;
                if let Some(s) = rec.segments.first_mut() {
                    s.pos = pos;
                    s.end = s.end.max(pos);
                }
            }
            Some(Err(e)) => return Err(describe(&e)),
            None => break,
        }
    }
    let mut rec = job.rec.lock().unwrap();
    if let Some(t) = rec.total
        && pos < t
    {
        return Err(format!("connection closed after {pos} of {t} bytes"));
    }
    rec.total = Some(pos);
    Ok(())
}

fn build_headers(pairs: &[(String, String)]) -> Result<HeaderMap, String> {
    let mut map = HeaderMap::new();
    for (k, v) in pairs {
        let name = HeaderName::from_bytes(k.as_bytes()).map_err(|e| format!("header {k}: {e}"))?;
        // Ranges are byte offsets into the *encoded* body; never let a
        // compressed transfer encoding sneak in.
        if name == ACCEPT_ENCODING || name == RANGE {
            continue;
        }
        let value = HeaderValue::from_str(v).map_err(|e| format!("header {k}: {e}"))?;
        map.insert(name, value);
    }
    map.insert(ACCEPT_ENCODING, HeaderValue::from_static("identity"));
    if !map.contains_key(reqwest::header::USER_AGENT) {
        map.insert(
            reqwest::header::USER_AGENT,
            HeaderValue::from_static(concat!("Hurtle/", env!("CARGO_PKG_VERSION"))),
        );
    }
    Ok(map)
}

fn header_str(h: &HeaderMap, name: HeaderName) -> Option<String> {
    h.get(name)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string)
}

fn describe(e: &reqwest::Error) -> String {
    let mut msg = e.to_string();
    let mut src = std::error::Error::source(e);
    while let Some(s) = src {
        msg.push_str(": ");
        msg.push_str(&s.to_string());
        src = s.source();
    }
    msg
}

#[cfg(unix)]
fn write_at(file: &File, buf: &[u8], offset: u64) -> std::io::Result<()> {
    std::os::unix::fs::FileExt::write_all_at(file, buf, offset)
}

#[cfg(windows)]
fn write_at(file: &File, mut buf: &[u8], mut offset: u64) -> std::io::Result<()> {
    use std::os::windows::fs::FileExt;
    while !buf.is_empty() {
        let n = file.seek_write(buf, offset)?;
        buf = &buf[n..];
        offset += n as u64;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader, Write};
    use std::net::TcpListener;

    #[test]
    fn transient_errors() {
        for e in [
            "error sending request for url (http://h/): client error (Connect): tcp connect error: Connection refused",
            "operation timed out",
            "connection closed early",
            "connection closed after 5 of 10 bytes",
            "server answered 503 Service Unavailable",
            "server answered 429 Too Many Requests",
            "server answered 502 Bad Gateway",
        ] {
            assert!(is_transient(e), "{e}");
        }
        for e in [
            "server answered 404 Not Found",
            "server answered 403 Forbidden",
            "server answered 410 Gone",
            "server stopped honouring byte ranges",
            "server sent the wrong range (asked for 5, got None)",
            "write failed: No space left on device (os error 28)",
            "/dl/x.part: Permission denied (os error 13)",
            "error sending request for url (https://h/): client error (Connect): invalid peer certificate: UnknownIssuer",
            "cancelled",
        ] {
            assert!(!is_transient(e), "{e}");
        }
    }

    #[test]
    fn retry_delays_are_bounded() {
        assert_eq!(auto_retry_delay(0), None);
        assert_eq!(auto_retry_delay(1), Some(Duration::from_secs(30)));
        assert_eq!(auto_retry_delay(5), Some(Duration::from_secs(300)));
        assert_eq!(auto_retry_delay(6), None);
    }

    #[test]
    fn same_file_validators() {
        let s = |v: &str| Some(v.to_string());
        let none = || None;
        assert!(same_validators(&s("\"a\""), &s("\"a\""), &none(), &none()));
        assert!(!same_validators(&s("\"a\""), &s("\"b\""), &none(), &none()));
        // No ETag: a different Last-Modified still means a different file.
        assert!(!same_validators(&none(), &none(), &s("Mon"), &s("Tue")));
        assert!(same_validators(&none(), &none(), &s("Mon"), &s("Mon")));
        // Unknown on one side: nothing to compare.
        assert!(same_validators(&none(), &s("\"a\""), &s("Mon"), &none()));
    }

    /// Minimal HTTP/1.1 server: one request per connection. `handler` gets the
    /// request line and `Range` header and returns the raw response.
    fn serve(handler: impl Fn(&str, Option<&str>) -> Vec<u8> + Send + 'static) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            for conn in listener.incoming() {
                let Ok(mut conn) = conn else { break };
                let mut r = BufReader::new(conn.try_clone().unwrap());
                let mut line = String::new();
                r.read_line(&mut line).unwrap_or(0);
                let mut range = None;
                loop {
                    let mut h = String::new();
                    if r.read_line(&mut h).unwrap_or(0) == 0 || h.trim().is_empty() {
                        break;
                    }
                    if let Some(v) = h.to_ascii_lowercase().strip_prefix("range:") {
                        range = Some(v.trim().to_string());
                    }
                }
                let _ = conn.write_all(&handler(line.trim(), range.as_deref()));
            }
        });
        format!("http://{addr}/file.bin")
    }

    fn response(status: &str, headers: &str, body: &[u8]) -> Vec<u8> {
        let mut out = format!(
            "HTTP/1.1 {status}\r\n{headers}Content-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        )
        .into_bytes();
        out.extend_from_slice(body);
        out
    }

    /// Download `url` into a fresh dir and return the file's bytes (or the error).
    fn fetch_all(url: &str) -> Result<Vec<u8>, String> {
        let dir = std::env::temp_dir().join(format!(
            "hurtle-job-{}-{}",
            std::process::id(),
            url.rsplit(':').next().unwrap_or("0").replace('/', "_")
        ));
        let _ = std::fs::remove_dir_all(&dir);
        let data = dir.join("data");
        let out = dir.join("out");
        let engine = crate::Engine::with_data_dir(&data).unwrap();
        engine
            .add(crate::AddRequest {
                url: url.into(),
                dir: Some(out.clone()),
                filename: Some("file.bin".into()),
                ..Default::default()
            })
            .unwrap();
        let end = Instant::now() + Duration::from_secs(20);
        let result = loop {
            let d = engine.snapshot().remove(0);
            match d.status {
                Status::Completed => break Ok(std::fs::read(out.join("file.bin")).unwrap()),
                Status::Failed(e) => break Err(e),
                _ if Instant::now() > end => {
                    break Err(format!("timeout: {:?} {:?}", d.status, d.note));
                }
                _ => std::thread::sleep(Duration::from_millis(50)),
            }
        };
        engine.shutdown();
        let _ = std::fs::remove_dir_all(&dir);
        result
    }

    #[test]
    fn unknown_total_in_206_downloads_everything() {
        let body = b"hello, whole file".to_vec();
        let url = serve(move |_, range| match range {
            Some(_) => response(
                "206 Partial Content",
                "Content-Range: bytes 0-0/*\r\n",
                &body[..1],
            ),
            None => response("200 OK", "", &body),
        });
        assert_eq!(fetch_all(&url).unwrap(), b"hello, whole file");
    }

    #[test]
    fn empty_file_after_416() {
        let url = serve(|_, range| match range {
            Some(_) => response(
                "416 Range Not Satisfiable",
                "Content-Range: bytes */0\r\n",
                b"",
            ),
            None => response("200 OK", "", b""),
        });
        assert_eq!(fetch_all(&url).unwrap(), b"");
    }

    #[test]
    fn probe_retries_503() {
        let hits = std::sync::atomic::AtomicUsize::new(0);
        let url = serve(move |_, range| {
            if hits.fetch_add(1, Ordering::Relaxed) < 2 {
                return response("503 Service Unavailable", "Retry-After: 0\r\n", b"");
            }
            match range {
                Some(r) => {
                    let (a, b) = r.trim_start_matches("bytes=").split_once('-').unwrap();
                    let (a, b): (usize, usize) = (a.parse().unwrap(), b.parse().unwrap());
                    let body = b"0123456789";
                    response(
                        "206 Partial Content",
                        &format!("Content-Range: bytes {a}-{b}/10\r\n"),
                        &body[a..=b.min(9)],
                    )
                }
                None => response("200 OK", "", b"0123456789"),
            }
        });
        assert_eq!(fetch_all(&url).unwrap(), b"0123456789");
    }
}
