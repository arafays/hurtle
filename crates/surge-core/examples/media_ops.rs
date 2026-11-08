//! Exercise a yt-dlp (media) download: start with a speed limit, change the
//! limit mid-way (restart), pause, check no yt-dlp is left running, resume.
//!
//!   cargo run -p surge-core --example media_ops -- <url> <out-dir>

use std::time::{Duration, Instant};

use surge_core::{AddRequest, Engine, Status};

fn ytdlp_running() -> bool {
    std::fs::read_dir("/proc")
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| std::fs::read(e.path().join("cmdline")).ok())
        .any(|c| String::from_utf8_lossy(&c).contains("SURGEINFO"))
}

fn wait(engine: &Engine, id: u64, what: &str, f: impl Fn(&surge_core::Download) -> bool) {
    let end = Instant::now() + Duration::from_secs(120);
    loop {
        let d = engine.get(id).unwrap();
        if f(&d) {
            println!(
                "{what}: {:?} {}/{:?} note={:?}",
                d.status, d.downloaded, d.total, d.note
            );
            return;
        }
        if let Status::Failed(e) = &d.status {
            panic!("failed: {e}");
        }
        assert!(Instant::now() < end, "timed out: {what}");
        std::thread::sleep(Duration::from_millis(100));
    }
}

fn main() {
    let mut args = std::env::args().skip(1);
    let url = args.next().expect("url");
    let out = args.next().expect("out dir");
    let data = std::path::Path::new(&out).join(".data");
    let engine = Engine::with_data_dir(&data).unwrap();
    let id = engine
        .add(AddRequest {
            dir: Some(out.clone().into()),
            filename: Some("Test Stream.mp4".into()),
            speed_limit: Some(1024 * 1024),
            connections: Some(4),
            ..AddRequest::new(url)
        })
        .unwrap();
    wait(&engine, id, "started", |d| d.downloaded > 1_000_000);
    let t = Instant::now();
    engine.set_speed_limit(id, 512 * 1024);
    wait(&engine, id, "after limit change", |d| {
        d.status == Status::Downloading && d.downloaded > 2_000_000
    });
    println!("  restart + 1MB took {:.1}s", t.elapsed().as_secs_f32());
    engine.pause(id);
    wait(&engine, id, "paused", |d| d.status == Status::Paused);
    assert!(!ytdlp_running(), "yt-dlp still running after pause");
    println!("no yt-dlp process left after pause");
    let before = engine.get(id).unwrap().downloaded;
    engine.set_speed_limit(id, 0);
    engine.resume(id);
    wait(&engine, id, "resumed", |d| d.status == Status::Downloading);
    let first = engine.get(id).unwrap().downloaded;
    println!("  progress before pause {before}, first after resume {first}");
    wait(&engine, id, "completed", |d| d.status == Status::Completed);
    println!("file: {:?}", engine.get(id).unwrap().path);
}
