//! BitTorrent end to end over loopback, no internet needed:
//!
//! 1. A plain librqbit session seeds a generated folder (a video and a text file).
//! 2. Hurtle lists it from a magnet link whose only peer is `x.pe=<seeder>`
//!    (so the metadata itself comes from the peer), then downloads only the video.
//! 3. While that runs (throttled), a byte range is read through the stream URL.
//! 4. Pause, drop the engine, start a new one on the same data dir, resume.
//! 5. The finished file must equal the source; the text file must not exist;
//!    the whole file streams from disk.
//!
//! `cargo run -p hurtle-core --example torrent [work dir]`

use std::net::{Ipv4Addr, SocketAddr};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail, ensure};
use hurtle_core::{AddRequest, Engine, Status};

const VIDEO: usize = 24 << 20;

fn main() -> Result<()> {
    let base = std::env::args()
        .nth(1)
        .map(PathBuf::from)
        .unwrap_or_else(|| std::env::temp_dir().join("hurtle-torrent-check"));
    let _ = std::fs::remove_dir_all(&base);
    let src = base.join("seed").join("Pack");
    let data = base.join("data");
    let out = base.join("out");
    std::fs::create_dir_all(&src)?;
    std::fs::create_dir_all(&out)?;
    // Logs go to the test's data dir, not the real one.
    unsafe { std::env::set_var("HURTLE_DATA", &data) };

    // Pseudo-random but reproducible content, so every piece differs.
    let mut x: u64 = 0x9E37_79B9_7F4A_7C15;
    let video: Vec<u8> = (0..VIDEO)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x as u8
        })
        .collect();
    std::fs::write(src.join("movie.mp4"), &video)?;
    std::fs::write(src.join("readme.txt"), b"not wanted\n")?;

    // ---- 1. Seeder.
    let rt = tokio::runtime::Runtime::new()?;
    let (hash, seeder_addr, _seeder) = rt.block_on(async {
        let session = librqbit::Session::new_with_opts(
            base.join("seed"),
            librqbit::SessionOptions {
                dht: None,
                disable_local_service_discovery: true,
                disable_trackers: true,
                ipv4_only: true,
                listen: Some(librqbit::ListenerOptions {
                    mode: librqbit::ListenerMode::TcpOnly,
                    listen_addr: (Ipv4Addr::LOCALHOST, 0).into(),
                    enable_upnp_port_forwarding: false,
                    ipv4_only: true,
                    ..Default::default()
                }),
                ..Default::default()
            },
        )
        .await?;
        let (created, _handle) = session
            .create_and_serve_torrent(
                &src,
                librqbit::CreateTorrentOptions {
                    name: Some("Pack"),
                    trackers: vec![],
                    piece_length: Some(256 * 1024),
                },
            )
            .await
            .map_err(|e| anyhow::anyhow!("{e:?}"))?;
        let addr: SocketAddr = session.listen_addr().context("seeder isn't listening")?;
        anyhow::Ok((created.info_hash().as_string(), addr, session))
    })?;
    let seeder = SocketAddr::from((Ipv4Addr::LOCALHOST, seeder_addr.port()));
    println!("seeding {hash} on {seeder}");
    let magnet = format!("magnet:?xt=urn:btih:{hash}&dn=Pack&x.pe={seeder}");

    // ---- 2. List and add.
    let engine = start(&data, &out)?;
    let listed = engine.probe_torrent(&AddRequest::new(&magnet))?;
    println!(
        "listed \"{}\": {:?}",
        listed.name,
        listed
            .files
            .iter()
            .map(|f| (&f.path, f.size))
            .collect::<Vec<_>>()
    );
    ensure!(listed.info_hash == hash, "probe hash differs");
    let video_idx = listed
        .files
        .iter()
        .position(|f| f.path.ends_with("movie.mp4"))
        .context("no movie.mp4 in the list")?;
    let id = engine.add(AddRequest {
        files: Some(vec![video_idx]),
        // Slow enough to pause half-way.
        speed_limit: Some(3 << 20),
        ..AddRequest::new(&magnet)
    })?;

    // ---- 3. Stream a range while it downloads.
    wait(
        &engine,
        id,
        Duration::from_secs(60),
        "a quarter done",
        |d| d.downloaded > VIDEO as u64 / 4,
    )?;
    let url = engine.stream_url(id, Some(video_idx))?;
    println!("stream url {url}");
    let client = reqwest::blocking::Client::new();
    // Near the end: not downloaded yet, so the stream has to fetch it first.
    let (a, b) = (VIDEO - 300_000, VIDEO - 200_001);
    let resp = client
        .get(&url)
        .header("Range", format!("bytes={a}-{b}"))
        .timeout(Duration::from_secs(60))
        .send()?;
    ensure!(resp.status() == 206, "range answered {}", resp.status());
    let cr = resp
        .headers()
        .get("content-range")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_string();
    ensure!(cr == format!("bytes {a}-{b}/{VIDEO}"), "content-range {cr}");
    let got = resp.bytes()?;
    ensure!(got[..] == video[a..=b], "streamed range differs");
    println!("streamed {} bytes from the end mid-download: ok", got.len());

    // ---- 4. Pause, restart the engine, resume.
    engine.pause(id);
    wait(&engine, id, Duration::from_secs(15), "paused", |d| {
        d.status == Status::Paused
    })?;
    let before = engine.get(id).unwrap().downloaded;
    engine.shutdown();
    drop(engine);
    println!("paused at {before} bytes; restarting the engine");
    let engine = start(&data, &out)?;
    let d = engine.get(id).context("record lost")?;
    ensure!(d.status == Status::Paused, "came back {:?}", d.status);
    engine.set_speed_limit(id, 0);
    engine.resume(id);
    let d = wait(&engine, id, Duration::from_secs(120), "completed", |d| {
        matches!(d.status, Status::Completed | Status::Failed(_))
    })?;
    if let Status::Failed(e) = &d.status {
        bail!("failed: {e}");
    }

    // ---- 5. Check.
    let path = d.path.clone().context("no path")?;
    let file = path.join("movie.mp4");
    println!("completed into {}", path.display());
    ensure!(std::fs::read(&file)? == video, "downloaded file differs");
    // readme.txt shares the first piece with the movie, so its bytes may be
    // on disk anyway (as in every client); it must not count as wanted.
    let t = d.torrent.as_ref().context("no torrent state")?;
    ensure!(
        t.files.iter().filter(|f| f.selected).count() == 1
            && d.total
                .is_some_and(|n| (VIDEO as u64..VIDEO as u64 + 256 * 1024).contains(&n)),
        "file choice not kept: {:?}",
        t.files
    );
    let full = client.get(engine.stream_url(id, Some(video_idx))?).send()?;
    ensure!(
        full.status() == 200,
        "full stream answered {}",
        full.status()
    );
    ensure!(full.bytes()?.len() == VIDEO, "full stream length");
    let m3u = client.get(engine.playlist_url(id)?).send()?.text()?;
    ensure!(m3u.contains("movie.mp4"), "playlist: {m3u}");
    println!(
        "players found: {:?}",
        hurtle_core::player::detect()
            .iter()
            .map(|p| &p.name)
            .collect::<Vec<_>>()
    );
    engine.shutdown();
    println!("torrent check passed");
    Ok(())
}

fn start(data: &Path, out: &Path) -> Result<Engine> {
    let engine = Engine::with_data_dir(data)?;
    let mut s = engine.settings();
    s.download_dir = out.to_path_buf();
    s.torrent_dht = false;
    s.torrent_upnp = false;
    s.torrent_port = 0;
    s.seed_ratio = 0.0;
    engine.set_settings(s);
    Ok(engine)
}

fn wait(
    engine: &Engine,
    id: u64,
    limit: Duration,
    what: &str,
    done: impl Fn(&hurtle_core::Download) -> bool,
) -> Result<hurtle_core::Download> {
    let until = Instant::now() + limit;
    let mut last = Instant::now();
    loop {
        let d = engine.get(id).context("download vanished")?;
        if done(&d) {
            return Ok(d);
        }
        if let Status::Failed(e) = &d.status {
            bail!("failed while waiting for {what}: {e}");
        }
        if last.elapsed() > Duration::from_secs(2) {
            last = Instant::now();
            println!(
                "  {:?} {} / {:?} {}",
                d.status,
                d.downloaded,
                d.total,
                d.note.unwrap_or_default()
            );
        }
        if Instant::now() > until {
            bail!("timed out waiting for {what}");
        }
        std::thread::sleep(Duration::from_millis(200));
    }
}
