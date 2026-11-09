//! An expired link captured again: pause a download, add a *different* link
//! to a file of the same size, and check that the old entry resumes.
//!   cargo run -p hurtle-core --example relink -- <base-url> <size> <dir>
//! e.g. with `mise run serve <file>` running: http://127.0.0.1:8090 <bytes of file> /tmp/x
use hurtle_core::{AddRequest, Engine, Status};
use std::time::Duration;

fn main() -> anyhow::Result<()> {
    let mut args = std::env::args().skip(1);
    let base = args.next().expect("base url");
    let size: u64 = args.next().expect("size").parse()?;
    let dir = args.next().expect("dir");
    let engine = Engine::with_data_dir(&std::path::Path::new(&dir).join(".state"))?;
    let req = |path: &str| AddRequest {
        dir: Some(dir.clone().into()),
        filename: Some("clip.bin".into()),
        page_url: Some("https://site.example/watch/1".into()),
        size: Some(size),
        ..AddRequest::new(format!("{base}{path}"))
    };

    let id = engine.add(req("/a/clip.bin?token=old"))?;
    std::thread::sleep(Duration::from_secs(3));
    engine.pause(id);
    while engine.get(id).unwrap().status != Status::Paused {
        std::thread::sleep(Duration::from_millis(100));
    }
    let before = engine.get(id).unwrap();
    println!("paused at {} of {:?}", before.downloaded, before.total);

    let (again, resumed) = engine.add_or_relink(req("/b/clip.bin?token=new"))?;
    assert_eq!(
        (again, resumed),
        (id, true),
        "a fresh link should resume #{id}"
    );
    assert_eq!(engine.snapshot().len(), 1, "no second entry");
    loop {
        std::thread::sleep(Duration::from_millis(100));
        let d = engine.get(id).unwrap();
        // The probe would reset `downloaded` to 0 if it restarted.
        if d.status == Status::Downloading && d.total.is_some() {
            assert!(
                d.downloaded >= before.downloaded,
                "restarted from {} instead of resuming at {}",
                d.downloaded,
                before.downloaded
            );
        }
        match &d.status {
            Status::Completed => {
                println!("completed from {} via {}", d.downloaded, d.url);
                break;
            }
            Status::Failed(e) => anyhow::bail!("failed: {e}"),
            _ => {}
        }
    }
    Ok(())
}
