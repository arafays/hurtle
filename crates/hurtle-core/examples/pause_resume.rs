//! Pause mid-download, drop the engine (simulating a quit), reopen, resume.
//!   cargo run -p hurtle-core --example pause_resume -- <url> <dir>
use hurtle_core::{AddRequest, Engine, Status};
use std::time::Duration;

fn wait(engine: &Engine, id: u64, done: impl Fn(&Status) -> bool) -> hurtle_core::Download {
    loop {
        std::thread::sleep(Duration::from_millis(100));
        let d = engine.get(id).expect("download exists");
        if done(&d.status) {
            return d;
        }
    }
}

fn main() -> anyhow::Result<()> {
    let mut args = std::env::args().skip(1);
    let (url, dir) = (args.next().expect("url"), args.next().expect("dir"));
    let data = std::path::PathBuf::from(&dir).join(".state");
    let id = {
        let engine = Engine::with_data_dir(&data)?;
        let mut req = AddRequest::new(url);
        req.dir = Some(dir.clone().into());
        req.connections = Some(6);
        let id = engine.add(req)?;
        std::thread::sleep(Duration::from_secs(3));
        engine.pause(id);
        let d = wait(&engine, id, |s| *s == Status::Paused);
        println!(
            "paused at {} / {:?} with {} segments",
            d.downloaded,
            d.total,
            d.segments.len()
        );
        id
    }; // engine dropped: state saved
    let engine = Engine::with_data_dir(&data)?;
    let d = engine.get(id).expect("restored");
    println!("restored: {} bytes, status {:?}", d.downloaded, d.status);
    engine.resume(id);
    let d = wait(&engine, id, |s| {
        matches!(s, Status::Completed | Status::Failed(_))
    });
    println!(
        "final: {:?} {} bytes -> {:?}",
        d.status, d.downloaded, d.path
    );
    Ok(())
}
