//! Desktop notifications when a download finishes or fails.
//!
//! A watcher on the engine's runtime compares status changes between
//! snapshots, so it works while the window is closed. Downloads already in
//! the list at start-up are only noted, not announced.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use hurtle_core::{Engine, Status};

#[derive(Clone, Copy, PartialEq)]
enum Seen {
    Other,
    Completed,
    Failed,
}

fn seen(s: &Status) -> Seen {
    match s {
        Status::Completed => Seen::Completed,
        Status::Failed(_) => Seen::Failed,
        _ => Seen::Other,
    }
}

pub fn start(engine: &Arc<Engine>) {
    let weak = Arc::downgrade(engine);
    engine.runtime().spawn(async move {
        let mut known: HashMap<u64, Seen> = HashMap::new();
        let mut first = true;
        loop {
            tokio::time::sleep(Duration::from_secs(1)).await;
            let Some(engine) = weak.upgrade() else {
                break;
            };
            let list = engine.snapshot();
            known.retain(|id, _| list.iter().any(|d| d.id == *id));
            for d in &list {
                let now = seen(&d.status);
                let before = known.insert(d.id, now);
                if first || before.unwrap_or(Seen::Other) == now {
                    continue;
                }
                if !engine.settings().notify {
                    continue;
                }
                let (title, body) = match &d.status {
                    Status::Completed => ("Download complete", d.display_name().to_string()),
                    Status::Failed(e) => ("Download failed", format!("{}\n{e}", d.display_name())),
                    _ => continue,
                };
                tokio::task::spawn_blocking(move || {
                    let _ = notify_rust::Notification::new()
                        .appname("Hurtle")
                        .summary(title)
                        .body(&body)
                        .show();
                });
            }
            first = false;
        }
    });
}
