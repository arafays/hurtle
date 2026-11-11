//! BitTorrent downloads: magnet links and `.torrent` files, through librqbit.
//!
//! - One librqbit [`Session`] per [`crate::Engine`], started on first use
//!   (the CLI and the native host never start one unless they add a torrent).
//!   Its DHT state lives in `$HURTLE_DATA/torrent/`.
//! - Hurtle's own `downloads.json` is the source of truth. librqbit keeps no
//!   session file; a paused torrent stays in the session, and after a restart
//!   it is added again (`overwrite`, so it re-checks the pieces on disk).
//! - Every resolved `.torrent` is cached as `$HURTLE_DATA/torrent/<hash>.torrent`,
//!   so a magnet's metadata is fetched from peers only once (the dialog's file
//!   list fetches it, the download reuses it).
//! - Files are written in place under the download's folder. A single-file
//!   torrent lands at `dir/name`; a multi-file one in `dir/<torrent name>/`.
//! - After finishing, a torrent seeds until its share ratio is reached
//!   ([`crate::Settings::seed_ratio`], 0 = not at all).
//! - Any file can be streamed while it downloads: see [`crate::stream`].

use std::collections::HashSet;
use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr};
use std::num::NonZeroU32;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use base64::Engine as _;
use librqbit::api::TorrentIdOrHash;
use librqbit::dht::{DhtPersistenceConfig, Id20};
use librqbit::limits::LimitsConfig;
use librqbit::{
    AddTorrent, AddTorrentOptions, AddTorrentResponse, DhtSessionConfig, ListenerMode,
    ListenerOptions, Magnet, ManagedTorrent, Session, SessionOptions, TorrentStats,
    TorrentStatsState, ValidatedTorrentMetaV1Info,
};
use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;

use crate::{Inner, Job, Status, util};

/// The torrent side of a [`crate::Download`].
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct TorrentState {
    /// v1 info hash, 40 hex digits.
    pub info_hash: String,
    /// Every file in the torrent, in torrent order (indices are librqbit's
    /// file ids). Empty until the metadata is known.
    pub files: Vec<TorrentFile>,
    /// All of it is one file (written as `dir/name`, not `dir/name/...`).
    pub single: bool,
    /// File ids to download once the file list is known (from the dialog,
    /// before a magnet's metadata arrived). Applied once, then cleared.
    pub wanted: Option<Vec<usize>>,
    /// Bytes uploaded to peers, all sessions together.
    pub uploaded: u64,
    /// Seed until uploaded ÷ size reaches this; None = the global setting.
    pub seed_ratio: Option<f32>,
    /// Connected peers, and how many of them have everything.
    #[serde(skip)]
    pub peers: u32,
    #[serde(skip)]
    pub seeds: u32,
    /// Upload speed, bytes/s.
    #[serde(skip)]
    pub upload_speed: u64,
    /// Finished and still uploading to others.
    #[serde(skip)]
    pub seeding: bool,
}

impl TorrentState {
    pub fn ratio(&self, size: u64) -> f32 {
        if size == 0 {
            0.0
        } else {
            self.uploaded as f32 / size as f32
        }
    }

    /// The ids of the files that are downloaded.
    pub fn selected(&self) -> Vec<usize> {
        self.files
            .iter()
            .enumerate()
            .filter(|(_, f)| f.selected)
            .map(|(i, _)| i)
            .collect()
    }

    /// The biggest playable file that is (or will be) downloaded: what "Play" opens.
    pub fn main_playable(&self) -> Option<usize> {
        self.files
            .iter()
            .enumerate()
            .filter(|(_, f)| !f.padding && crate::player::is_playable(&f.path))
            .max_by_key(|(_, f)| (f.selected, f.size))
            .map(|(i, _)| i)
    }
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct TorrentFile {
    /// Path inside the torrent, `/`-separated.
    pub path: String,
    pub size: u64,
    pub selected: bool,
    /// BEP 47 padding file: not shown, never selected.
    pub padding: bool,
    /// Bytes of it verified on disk.
    #[serde(skip)]
    pub done: u64,
}

/// What the "add torrent" dialog shows: the files to choose from.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TorrentProbe {
    pub name: String,
    pub info_hash: String,
    pub total: u64,
    pub files: Vec<TorrentFile>,
}

/// Engine-wide BitTorrent state.
#[derive(Default)]
pub(crate) struct Torrents {
    session: tokio::sync::OnceCell<Arc<Session>>,
    /// Cancelled on shutdown (synchronously), which stops the whole session.
    pub(crate) cancel: CancellationToken,
}

/// A torrent's identity, parsed from whatever the user gave us.
#[derive(Debug, PartialEq)]
pub struct Source {
    pub info_hash: String,
    pub name: Option<String>,
}

/// A link Hurtle should treat as a torrent: a magnet, or a `.torrent` file URL.
pub fn is_torrent_url(url: &str) -> bool {
    let url = url.trim();
    url.get(..7)
        .is_some_and(|s| s.eq_ignore_ascii_case("magnet:"))
        || crate::link_path(url)
            .to_ascii_lowercase()
            .ends_with(".torrent")
}

/// An add request that is a torrent: torrent data, a magnet, or a `.torrent` URL.
pub fn is_torrent_request(req: &crate::AddRequest) -> bool {
    req.torrent_data.as_deref().is_some_and(|d| !d.is_empty()) || is_torrent_url(&req.url)
}

pub fn is_magnet(url: &str) -> bool {
    url.trim()
        .get(..7)
        .is_some_and(|s| s.eq_ignore_ascii_case("magnet:"))
}

/// Info hash and display name of a magnet link.
pub fn parse_magnet(url: &str) -> Result<Source, String> {
    let m = Magnet::parse(url.trim()).map_err(|e| format!("not a usable magnet link: {e}"))?;
    let hash = m
        .as_id20()
        .ok_or("only BitTorrent v1 (btih) magnet links are supported")?;
    Ok(Source {
        info_hash: hash.as_string(),
        name: m.name.clone().filter(|n| !n.trim().is_empty()),
    })
}

/// Peers named in a magnet link (`x.pe=host:port`, BEP 9): a direct way in
/// when there is no tracker and no DHT (LAN shares, private swarms).
pub fn magnet_peers(url: &str) -> Vec<SocketAddr> {
    use std::net::ToSocketAddrs;
    let Some((_, query)) = url.trim().split_once('?') else {
        return vec![];
    };
    query
        .split('&')
        .filter_map(|kv| kv.split_once('='))
        .filter(|(k, _)| k.eq_ignore_ascii_case("x.pe"))
        .filter_map(|(_, v)| {
            let v = percent_encoding::percent_decode_str(v).decode_utf8().ok()?;
            v.parse::<SocketAddr>()
                .ok()
                .or_else(|| v.to_socket_addrs().ok()?.next())
        })
        .take(50)
        .collect()
}

/// Decode `AddRequest.torrent_data` (base64 `.torrent` contents).
pub fn decode_data(b64: &str) -> Result<Vec<u8>, String> {
    let clean: String = b64.chars().filter(|c| !c.is_whitespace()).collect();
    // A `data:` URL from the browser works too.
    let clean = clean
        .rsplit_once("base64,")
        .map_or(clean.as_str(), |(_, d)| d);
    base64::engine::general_purpose::STANDARD
        .decode(clean)
        .map_err(|e| format!("bad torrent data: {e}"))
}

pub fn encode_data(bytes: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

/// Parse `.torrent` bytes: identity, name, files.
pub fn parse(bytes: &[u8]) -> Result<TorrentProbe, String> {
    let t = librqbit::torrent_from_bytes(bytes).map_err(|e| format!("not a .torrent file: {e}"))?;
    let hash = t.info_hash;
    let info = t
        .info
        .data
        .validate()
        .map_err(|e| format!("bad .torrent file: {e}"))?;
    Ok(probe_of(hash, &info))
}

fn probe_of<B: AsRef<[u8]>>(hash: Id20, info: &ValidatedTorrentMetaV1Info<B>) -> TorrentProbe {
    let files: Vec<TorrentFile> = info
        .iter_file_details()
        .map(|d| {
            let padding = d.attrs().padding;
            TorrentFile {
                path: d.filename.to_vec().join("/"),
                size: d.len,
                selected: !padding,
                padding,
                done: 0,
            }
        })
        .collect();
    TorrentProbe {
        name: info
            .name()
            .map(|n| n.into_owned())
            .unwrap_or_else(|| hash.as_string()),
        info_hash: hash.as_string(),
        total: files.iter().filter(|f| !f.padding).map(|f| f.size).sum(),
        files,
    }
}

fn single_file<B: AsRef<[u8]>>(info: &ValidatedTorrentMetaV1Info<B>) -> bool {
    info.info().length.is_some()
}

fn cache_dir(inner: &Inner) -> PathBuf {
    inner.data_dir.join("torrent")
}

pub(crate) fn cache_path(inner: &Inner, info_hash: &str) -> PathBuf {
    cache_dir(inner).join(format!("{}.torrent", info_hash.to_ascii_lowercase()))
}

/// Keep `.torrent` bytes for later (resume, a dialog's add after its probe).
pub(crate) fn remember(inner: &Inner, bytes: &[u8]) -> Result<TorrentProbe, String> {
    let probe = parse(bytes)?;
    let dir = cache_dir(inner);
    util::create_private_dir(&dir).map_err(|e| format!("creating {}: {e}", dir.display()))?;
    util::write_private(&cache_path(inner, &probe.info_hash), bytes)
        .map_err(|e| format!("saving the .torrent: {e}"))?;
    Ok(probe)
}

fn cached(inner: &Inner, info_hash: &str) -> Option<Vec<u8>> {
    std::fs::read(cache_path(inner, info_hash))
        .ok()
        .filter(|b| parse(b).is_ok_and(|p| p.info_hash.eq_ignore_ascii_case(info_hash)))
}

fn nz(bps: u64) -> Option<NonZeroU32> {
    NonZeroU32::new(bps.min(u32::MAX as u64) as u32)
}

/// The session, started on first use with the current settings. Port, DHT
/// and UPnP changes apply after a restart of Hurtle.
pub(crate) async fn session(inner: &Arc<Inner>) -> Result<Arc<Session>, String> {
    inner
        .torrents
        .session
        .get_or_try_init(|| start_session(inner))
        .await
        .cloned()
}

async fn start_session(inner: &Arc<Inner>) -> Result<Arc<Session>, String> {
    let s = inner.settings.lock().unwrap().clone();
    let dir = cache_dir(inner);
    util::create_private_dir(&dir).map_err(|e| format!("creating {}: {e}", dir.display()))?;
    let port = std::env::var("HURTLE_TORRENT_PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(s.torrent_port);
    let options = |port: u16, v6: bool| SessionOptions {
        dht: s.torrent_dht.then(|| DhtSessionConfig {
            persistence: Some(DhtPersistenceConfig {
                dump_interval: None,
                config_filename: Some(dir.join("dht.json")),
            }),
            ..Default::default()
        }),
        disable_local_service_discovery: !s.torrent_dht,
        listen: Some(ListenerOptions {
            mode: ListenerMode::TcpAndUtp,
            listen_addr: if v6 {
                (Ipv6Addr::UNSPECIFIED, port).into()
            } else {
                (Ipv4Addr::UNSPECIFIED, port).into()
            },
            enable_upnp_port_forwarding: s.torrent_upnp,
            ipv4_only: !v6,
            ..Default::default()
        }),
        ipv4_only: !v6,
        cancellation_token: Some(inner.torrents.cancel.child_token()),
        ratelimits: LimitsConfig {
            download_bps: nz(s.speed_limit),
            upload_bps: nz(s.upload_limit),
        },
        peer_limit: Some(s.torrent_peers.max(1)),
        ..Default::default()
    };
    // The chosen port may be taken (another client), and some systems have
    // no IPv6: fall back to any port, then to IPv4 only.
    let mut last = String::new();
    for (port, v6) in [(port, true), (0, true), (port, false), (0, false)] {
        match Session::new_with_opts(dir.join("downloads"), options(port, v6)).await {
            Ok(session) => {
                crate::log::log(
                    "torrent",
                    format!(
                        "session started, listening on {:?}, DHT {}",
                        session.listen_addr(),
                        if s.torrent_dht { "on" } else { "off" }
                    ),
                );
                return Ok(session);
            }
            Err(e) => last = format!("{e:#}"),
        }
    }
    Err(format!("starting BitTorrent: {last}"))
}

/// The `.torrent` for a download or a dialog: given bytes, the cache, the
/// `.torrent` URL (with the browser's cookies), or the magnet's peers.
async fn obtain(
    inner: &Arc<Inner>,
    url: &str,
    headers: &[(String, String)],
    known_hash: Option<&str>,
    on_wait: impl Fn(&str),
) -> Result<(Vec<u8>, Vec<SocketAddr>), String> {
    if let Some(bytes) = known_hash.and_then(|h| cached(inner, h)) {
        return Ok((bytes, vec![]));
    }
    if is_magnet(url) {
        let src = parse_magnet(url)?;
        if let Some(bytes) = cached(inner, &src.info_hash) {
            return Ok((bytes, vec![]));
        }
        on_wait("Getting the file list from peers");
        let session = session(inner).await?;
        let added = session
            .add_torrent(
                AddTorrent::from_url(url.trim().to_string()),
                Some(AddTorrentOptions {
                    list_only: true,
                    initial_peers: Some(magnet_peers(url)).filter(|p| !p.is_empty()),
                    ..Default::default()
                }),
            )
            .await
            .map_err(|e| format!("reading the magnet link: {e:#}"))?;
        let (bytes, seen) = match added {
            AddTorrentResponse::ListOnly(r) => (r.torrent_bytes.to_vec(), r.seen_peers),
            AddTorrentResponse::AlreadyManaged(_, h) | AddTorrentResponse::Added(_, h) => (
                h.with_metadata(|m| m.torrent_bytes.to_vec())
                    .map_err(|e| format!("{e:#}"))?,
                vec![],
            ),
        };
        remember(inner, &bytes)?;
        return Ok((bytes, seen));
    }
    let parsed = util::require_http(url).map_err(|e| e.to_string())?;
    on_wait("Fetching the .torrent file");
    let mut req = inner.client.get(parsed);
    for (k, v) in headers {
        req = req.header(k, v);
    }
    let resp = req
        .timeout(Duration::from_secs(60))
        .send()
        .await
        .map_err(|e| e.to_string())?;
    if !resp.status().is_success() {
        return Err(format!("server answered {}", resp.status()));
    }
    if resp.content_length().is_some_and(|l| l > MAX_TORRENT) {
        return Err("the .torrent file is too big".into());
    }
    let bytes = resp.bytes().await.map_err(|e| e.to_string())?;
    if bytes.len() as u64 > MAX_TORRENT {
        return Err("the .torrent file is too big".into());
    }
    remember(inner, &bytes)?;
    Ok((bytes.to_vec(), vec![]))
}

/// Bigger than any sane `.torrent` (they are usually well under 1 MB).
const MAX_TORRENT: u64 = 32 << 20;

/// The file list for the dialog (magnets: from peers, up to a few minutes).
pub(crate) async fn probe(
    inner: &Arc<Inner>,
    req: &crate::AddRequest,
) -> Result<TorrentProbe, String> {
    if let Some(data) = req.torrent_data.as_deref().filter(|d| !d.is_empty()) {
        return remember(inner, &decode_data(data)?);
    }
    let url = req.url.trim();
    if !is_torrent_url(url) && util::require_http(url).is_err() {
        return Err("not a magnet link or .torrent URL".into());
    }
    let headers = req.header_list();
    let work = obtain(inner, url, &headers, None, |_| {});
    let (bytes, _) = tokio::time::timeout(Duration::from_secs(180), work)
        .await
        .map_err(|_| "no peer sent the torrent's file list in 3 minutes".to_string())??;
    parse(&bytes)
}

fn handle(session: &Session, info_hash: &str) -> Option<Arc<ManagedTorrent>> {
    let id: Id20 = info_hash.parse().ok()?;
    session.get(TorrentIdOrHash::Hash(id))
}

/// The running torrent of a download, if it is in the session.
pub(crate) fn live_handle(inner: &Inner, info_hash: &str) -> Option<Arc<ManagedTorrent>> {
    handle(inner.torrents.session.get()?, info_hash)
}

pub(crate) async fn drive(
    inner: &Arc<Inner>,
    job: &Arc<Job>,
    token: &CancellationToken,
) -> Result<(), String> {
    let settings = inner.settings.lock().unwrap().clone();
    let (url, headers, known) = {
        let mut rec = job.rec.lock().unwrap();
        rec.status = Status::Probing;
        rec.note = Some("Starting BitTorrent".into());
        rec.resumable = true;
        rec.segments.clear();
        (
            rec.url.clone(),
            rec.headers.clone(),
            rec.torrent
                .as_ref()
                .map(|t| t.info_hash.clone())
                .filter(|h| !h.is_empty()),
        )
    };
    let note = |s: &str| job.rec.lock().unwrap().note = Some(s.to_string());
    let session = tokio::select! {
        _ = token.cancelled() => return Err("cancelled".into()),
        s = session(inner) => s?,
    };
    let (bytes, seen) = tokio::select! {
        _ = token.cancelled() => return Err("cancelled".into()),
        r = obtain(inner, &url, &headers, known.as_deref(), note) => r?,
    };
    let meta = parse(&bytes)?;
    let mut seen = seen;
    seen.extend(magnet_peers(&url));
    seen.dedup();
    let single = librqbit::torrent_from_bytes(&bytes)
        .ok()
        .and_then(|t| t.info.data.validate().ok())
        .is_some_and(|i| single_file(&i));

    // Name, folder and file choice, under the path claim like other downloads.
    let (output, only_files, limit) = {
        let _claim = inner.path_claim.lock().unwrap();
        let my_id = job.rec.lock().unwrap().id;
        let others: Vec<PathBuf> = inner
            .jobs
            .lock()
            .unwrap()
            .iter()
            .filter_map(|j| {
                let r = j.rec.lock().unwrap();
                (r.id != my_id).then(|| r.path.clone()).flatten()
            })
            .collect();
        let mut rec = job.rec.lock().unwrap();
        let first = rec.path.is_none();
        let t = rec.torrent.get_or_insert_with(TorrentState::default);
        t.info_hash = meta.info_hash.clone();
        t.single = single;
        if t.files.len() != meta.files.len() {
            let wanted: Option<HashSet<usize>> = t.wanted.take().map(|w| w.into_iter().collect());
            t.files = meta.files.clone();
            if let Some(w) = wanted.filter(|w| w.iter().any(|i| *i < t.files.len())) {
                for (i, f) in t.files.iter_mut().enumerate() {
                    f.selected = !f.padding && w.contains(&i);
                }
            }
        }
        let selected = t.selected();
        let biggest = t
            .files
            .iter()
            .filter(|f| f.selected)
            .max_by_key(|f| f.size)
            .map(|f| f.path.clone());
        let total = t.files.iter().filter(|f| f.selected).map(|f| f.size).sum();
        rec.total = Some(total);
        if first {
            let name = util::sanitize_filename(&meta.name);
            if rec.auto_dir {
                let cat = biggest.as_deref().and_then(util::category);
                rec.dir = settings.dir_for(cat);
            }
            let dir = rec.dir.clone();
            let taken = |p: &Path| others.iter().any(|o| o == p);
            let path = if single {
                let direct = dir.join(&name);
                if !direct.exists() && !taken(&direct) {
                    direct
                } else {
                    // Can't rename the torrent's file: give it a folder of its own.
                    let stem = Path::new(&name)
                        .file_stem()
                        .map(|s| s.to_string_lossy().into_owned())
                        .unwrap_or_else(|| name.clone());
                    util::unique_path(&dir, &stem, |p| taken(p) || taken(&p.join(&name)))
                        .join(&name)
                }
            } else {
                util::unique_path(&dir, &name, taken)
            };
            rec.filename = path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or(name);
            rec.path = Some(path);
        }
        let path = rec.path.clone().unwrap_or_default();
        let output = if single {
            path.parent().map(Path::to_path_buf).unwrap_or_default()
        } else {
            path
        };
        (output, selected, rec.speed_limit)
    };
    inner.mark_dirty();
    if only_files.is_empty() {
        return Err("no files are selected".into());
    }

    let handle = match handle(&session, &meta.info_hash) {
        // Paused earlier in this run: pick up where it stopped.
        Some(h) if h.output_folder() == output => {
            let want: HashSet<usize> = only_files.iter().copied().collect();
            if h.only_files()
                .map(|o| o.into_iter().collect::<HashSet<_>>())
                != Some(want.clone())
            {
                session
                    .update_only_files(&h, &want)
                    .await
                    .map_err(|e| format!("{e:#}"))?;
            }
            if h.is_paused() {
                session.unpause(&h).await.map_err(|e| format!("{e:#}"))?;
            }
            h
        }
        stale => {
            if let Some(h) = stale {
                // Left over from a file-list probe or another folder.
                let _ = session.delete(TorrentIdOrHash::Id(h.id()), false).await;
            }
            note("Checking files on disk");
            std::fs::create_dir_all(&output)
                .map_err(|e| format!("creating {}: {e}", output.display()))?;
            let added = session
                .add_torrent(
                    AddTorrent::from_bytes(bytes),
                    Some(AddTorrentOptions {
                        only_files: Some(only_files.clone()),
                        // Our own folder (chosen unique above), resumed in place.
                        overwrite: true,
                        output_folder: Some(output.to_string_lossy().into_owned()),
                        ratelimits: LimitsConfig {
                            download_bps: nz(limit),
                            upload_bps: None,
                        },
                        initial_peers: (!seen.is_empty()).then_some(seen),
                        peer_limit: Some(settings.torrent_peers.max(1)),
                        ..Default::default()
                    }),
                )
                .await
                .map_err(|e| format!("{e:#}"))?;
            added
                .into_handle()
                .ok_or("the torrent was only listed, not added")?
        }
    };
    crate::log::log(
        "torrent",
        format!(
            "#{} {} started in {} ({} of {} files)",
            job.rec.lock().unwrap().id,
            meta.info_hash,
            output.display(),
            only_files.len(),
            meta.files.len()
        ),
    );

    let mut uploaded_seen = 0u64;
    loop {
        let stats = handle.stats();
        update(job, &stats, &mut uploaded_seen);
        if let Some(e) = &stats.error {
            let _ = session
                .delete(TorrentIdOrHash::Id(handle.id()), false)
                .await;
            return Err(e.clone());
        }
        if stats.finished && !matches!(stats.state, TorrentStatsState::Initializing { .. }) {
            break;
        }
        tokio::select! {
            _ = token.cancelled() => {
                stop(&session, &handle, job).await;
                return Err("cancelled".into());
            }
            _ = tokio::time::sleep(Duration::from_millis(500)) => {}
        }
    }

    // Finished: seed a while, or leave the swarm.
    let ratio = {
        let rec = job.rec.lock().unwrap();
        rec.torrent
            .as_ref()
            .and_then(|t| t.seed_ratio)
            .unwrap_or(settings.seed_ratio)
    };
    if ratio > 0.0 {
        spawn_seeding(inner.clone(), job.clone(), handle, token.clone(), ratio);
    } else {
        let _ = session
            .delete(TorrentIdOrHash::Id(handle.id()), false)
            .await;
    }
    Ok(())
}

/// Paused: keep it in the session (resuming needs no re-check). Restarted
/// for new limits: drop it, so the next start adds it with them.
async fn stop(session: &Arc<Session>, handle: &Arc<ManagedTorrent>, job: &Job) {
    if job.restart.load(std::sync::atomic::Ordering::Relaxed) {
        let _ = session
            .delete(TorrentIdOrHash::Id(handle.id()), false)
            .await;
    } else if !handle.is_paused() {
        let _ = session.pause(handle).await;
    }
}

/// Copy librqbit's numbers into the record.
fn update(job: &Job, stats: &TorrentStats, uploaded_seen: &mut u64) {
    let mut rec = job.rec.lock().unwrap();
    let initializing = matches!(stats.state, TorrentStatsState::Initializing { .. });
    let (peers, seeds, up) = stats.live.as_ref().map_or((0, 0, 0), |l| {
        (
            l.snapshot.peer_stats.live,
            l.snapshot.peer_stats.live_seeders,
            l.upload_speed.as_bytes(),
        )
    });
    // `uploaded_bytes` counts from this torrent's start in the session.
    let up_total = stats.uploaded_bytes;
    let delta = up_total.saturating_sub(*uploaded_seen);
    *uploaded_seen = up_total;
    if !initializing {
        rec.downloaded = stats.progress_bytes;
        rec.total = Some(stats.total_bytes);
    }
    rec.active_connections = peers as usize;
    if let Some(t) = rec.torrent.as_mut() {
        t.uploaded += delta;
        t.peers = peers;
        t.seeds = seeds;
        t.upload_speed = up;
        for (f, done) in t.files.iter_mut().zip(&stats.file_progress) {
            f.done = *done;
        }
    }
    rec.note = Some(if initializing {
        format!(
            "Checking files on disk ({})",
            util::fmt_bytes(stats.progress_bytes)
        )
    } else if peers == 0 {
        "Looking for peers".into()
    } else {
        format!(
            "{peers} peer{} ({seeds} seed{}) · ↑ {}",
            if peers == 1 { "" } else { "s" },
            if seeds == 1 { "" } else { "s" },
            util::fmt_speed(up as f64)
        )
    });
    rec.status = if initializing {
        Status::Probing
    } else {
        Status::Downloading
    };
}

/// Upload after finishing until the ratio is reached, or the download is
/// paused/removed (its token is cancelled), or Hurtle quits.
pub(crate) fn spawn_seeding(
    inner: Arc<Inner>,
    job: Arc<Job>,
    handle: Arc<ManagedTorrent>,
    token: CancellationToken,
    ratio: f32,
) {
    tokio::spawn(async move {
        let size = job.rec.lock().unwrap().total.unwrap_or(0);
        let mut uploaded_seen = handle.stats().uploaded_bytes;
        loop {
            let stats = handle.stats();
            let done = {
                let mut rec = job.rec.lock().unwrap();
                let up_total = stats.uploaded_bytes;
                let delta = up_total.saturating_sub(uploaded_seen);
                uploaded_seen = up_total;
                let (peers, up) = stats.live.as_ref().map_or((0, 0), |l| {
                    (l.snapshot.peer_stats.live, l.upload_speed.as_bytes())
                });
                let Some(t) = rec.torrent.as_mut() else { break };
                t.uploaded += delta;
                t.peers = peers;
                t.upload_speed = up;
                t.seeding = true;
                let r = t.ratio(size);
                let done = r >= ratio || stats.error.is_some();
                rec.note = (!done).then(|| {
                    format!(
                        "Seeding · ratio {r:.2} of {ratio:.2} · {peers} peer{} · ↑ {}",
                        if peers == 1 { "" } else { "s" },
                        util::fmt_speed(up as f64)
                    )
                });
                done
            };
            if done {
                break;
            }
            tokio::select! {
                _ = token.cancelled() => break,
                _ = inner.torrents.cancel.cancelled() => return,
                _ = tokio::time::sleep(Duration::from_secs(1)) => {}
            }
        }
        if let Some(session) = inner.torrents.session.get() {
            let _ = session
                .delete(TorrentIdOrHash::Id(handle.id()), false)
                .await;
        }
        {
            let mut rec = job.rec.lock().unwrap();
            if let Some(t) = rec.torrent.as_mut() {
                t.seeding = false;
                t.peers = 0;
                t.upload_speed = 0;
            }
            rec.note = None;
            rec.active_connections = 0;
        }
        inner.mark_dirty();
    });
}

/// Start seeding a finished torrent again (after a restart, or more ratio).
pub(crate) async fn seed_again(inner: &Arc<Inner>, job: &Arc<Job>) -> Result<(), String> {
    let (hash, path, single, only, ratio) = {
        let rec = job.rec.lock().unwrap();
        let t = rec.torrent.as_ref().ok_or("not a torrent")?;
        if t.seeding {
            return Ok(());
        }
        let s = inner.settings.lock().unwrap().seed_ratio;
        let goal = t.seed_ratio.unwrap_or(s);
        // Asked for explicitly: at least one more round.
        let size = rec.total.unwrap_or(0);
        let ratio = if goal > t.ratio(size) {
            goal
        } else {
            t.ratio(size) + 1.0
        };
        (
            t.info_hash.clone(),
            rec.path.clone().ok_or("no files yet")?,
            t.single,
            t.selected(),
            ratio,
        )
    };
    let bytes = cached(inner, &hash).ok_or("the .torrent is no longer cached")?;
    let session = session(inner).await?;
    let output = if single {
        path.parent().map(Path::to_path_buf).unwrap_or_default()
    } else {
        path
    };
    let handle = match handle(&session, &hash) {
        Some(h) => {
            if h.is_paused() {
                session.unpause(&h).await.map_err(|e| format!("{e:#}"))?;
            }
            h
        }
        None => session
            .add_torrent(
                AddTorrent::from_bytes(bytes),
                Some(AddTorrentOptions {
                    only_files: Some(only),
                    overwrite: true,
                    output_folder: Some(output.to_string_lossy().into_owned()),
                    ..Default::default()
                }),
            )
            .await
            .map_err(|e| format!("{e:#}"))?
            .into_handle()
            .ok_or("not added")?,
    };
    let token = CancellationToken::new();
    *job.cancel.lock().unwrap() = token.clone();
    job.rec.lock().unwrap().note = Some("Checking files before seeding".into());
    spawn_seeding(inner.clone(), job.clone(), handle, token, ratio);
    Ok(())
}

/// Change which files are downloaded. A running torrent follows at once.
pub(crate) async fn set_files(
    inner: &Arc<Inner>,
    job: &Arc<Job>,
    files: &[usize],
) -> Result<(), String> {
    let (hash, want) = {
        let mut rec = job.rec.lock().unwrap();
        let t = rec.torrent.as_mut().ok_or("not a torrent")?;
        if t.files.is_empty() {
            t.wanted = Some(files.to_vec());
            return Ok(());
        }
        let want: HashSet<usize> = files
            .iter()
            .copied()
            .filter(|i| t.files.get(*i).is_some_and(|f| !f.padding))
            .collect();
        if want.is_empty() {
            return Err("select at least one file".into());
        }
        for (i, f) in t.files.iter_mut().enumerate() {
            f.selected = want.contains(&i);
        }
        let total = t.files.iter().filter(|f| f.selected).map(|f| f.size).sum();
        let hash = t.info_hash.clone();
        rec.total = Some(total);
        // More to fetch: a finished torrent becomes unfinished again.
        if rec.status == Status::Completed && rec.downloaded < total {
            rec.status = Status::Paused;
            rec.finished_at = None;
        }
        (hash, want)
    };
    inner.mark_dirty();
    if let Some(h) = live_handle(inner, &hash)
        && let Some(session) = inner.torrents.session.get()
    {
        session
            .update_only_files(&h, &want)
            .await
            .map_err(|e| format!("{e:#}"))?;
    }
    Ok(())
}

/// Take a torrent out of the session; with `delete_files`, its data too.
/// Files of a torrent that isn't in the session are removed by path.
pub(crate) async fn forget(inner: Arc<Inner>, rec: crate::Download, delete_files: bool) {
    let Some(t) = rec.torrent.as_ref() else {
        return;
    };
    let in_session = live_handle(&inner, &t.info_hash);
    if let (Some(h), Some(session)) = (in_session, inner.torrents.session.get()) {
        let _ = session
            .delete(TorrentIdOrHash::Id(h.id()), delete_files)
            .await;
    } else if delete_files && let Some(path) = &rec.path {
        let root = if t.single {
            path.parent().map(Path::to_path_buf).unwrap_or_default()
        } else {
            path.clone()
        };
        for f in &t.files {
            let p = root.join(Path::new(&f.path));
            // Only files inside the download's folder.
            if p.starts_with(&root) {
                let _ = tokio::fs::remove_file(&p).await;
            }
        }
        if !t.single {
            remove_empty_dirs(&root);
        } else if root.read_dir().is_ok_and(|mut d| d.next().is_none()) && root != rec.dir {
            let _ = std::fs::remove_dir(&root);
        }
    }
    let _ = std::fs::remove_file(cache_path(&inner, &t.info_hash));
}

fn remove_empty_dirs(dir: &Path) {
    if let Ok(entries) = std::fs::read_dir(dir) {
        for e in entries.flatten() {
            if e.file_type().is_ok_and(|t| t.is_dir()) {
                remove_empty_dirs(&e.path());
            }
        }
    }
    let _ = std::fs::remove_dir(dir);
}

/// Global limits changed: apply them to the running session.
pub(crate) fn apply_limits(inner: &Inner, download: u64, upload: u64) {
    if let Some(s) = inner.torrents.session.get() {
        s.ratelimits.set_download_bps(nz(download));
        s.ratelimits.set_upload_bps(nz(upload));
    }
}

/// On-disk path of file `idx` of a torrent download.
pub(crate) fn file_path(rec: &crate::Download, idx: usize) -> Option<PathBuf> {
    let t = rec.torrent.as_ref()?;
    let f = t.files.get(idx)?;
    let path = rec.path.as_ref()?;
    Some(if t.single {
        path.clone()
    } else {
        path.join(Path::new(&f.path))
    })
}

/// A magnet link for this torrent (to copy or share).
pub fn magnet_link(rec: &crate::Download) -> Option<String> {
    let t = rec.torrent.as_ref()?;
    if is_magnet(&rec.url) {
        return Some(rec.url.clone());
    }
    let name: String =
        percent_encoding::utf8_percent_encode(&rec.filename, percent_encoding::NON_ALPHANUMERIC)
            .to_string();
    Some(format!("magnet:?xt=urn:btih:{}&dn={name}", t.info_hash))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn torrent_urls() {
        assert!(is_torrent_url("magnet:?xt=urn:btih:abc"));
        assert!(is_torrent_url("MAGNET:?xt=urn:btih:abc"));
        assert!(is_torrent_url("https://x.org/a/ubuntu.iso.torrent?key=1"));
        assert!(!is_torrent_url("https://x.org/a/ubuntu.iso"));
        assert!(!is_torrent_url("https://x.org/torrent"));
    }

    #[test]
    fn magnets() {
        let m = parse_magnet(
            "magnet:?xt=urn:btih:dd8255ecdc7ca55fb0bbf81323d87062db1f6d1c&dn=Big+Buck+Bunny&tr=udp%3A%2F%2Fexplodie.org%3A6969",
        )
        .unwrap();
        assert_eq!(m.info_hash, "dd8255ecdc7ca55fb0bbf81323d87062db1f6d1c");
        assert_eq!(m.name.as_deref(), Some("Big Buck Bunny"));
        assert!(parse_magnet("magnet:?dn=nothing").is_err());
    }

    #[test]
    fn magnet_peer_addresses() {
        let p = magnet_peers(
            "magnet:?xt=urn:btih:dd82&x.pe=127.0.0.1:6881&x.pe=%5B%3A%3A1%5D%3A7000&x.pe=junk",
        );
        assert_eq!(
            p,
            vec![
                "127.0.0.1:6881".parse::<SocketAddr>().unwrap(),
                "[::1]:7000".parse().unwrap()
            ]
        );
    }

    #[test]
    fn data_urls() {
        let b = encode_data(b"d4:infoe");
        assert_eq!(decode_data(&b).unwrap(), b"d4:infoe");
        assert_eq!(
            decode_data(&format!("data:application/x-bittorrent;base64,{b}")).unwrap(),
            b"d4:infoe"
        );
    }

    #[test]
    fn main_playable_prefers_selected_big_video() {
        let f = |path: &str, size, selected| TorrentFile {
            path: path.into(),
            size,
            selected,
            ..Default::default()
        };
        let t = TorrentState {
            files: vec![
                f("Movie/sample.mkv", 10, true),
                f("Movie/movie.mkv", 1000, true),
                f("Movie/extras.mkv", 5000, false),
                f("Movie/movie.nfo", 1, true),
            ],
            ..Default::default()
        };
        assert_eq!(t.main_playable(), Some(1));
        assert_eq!(t.selected(), vec![0, 1, 3]);
    }
}
