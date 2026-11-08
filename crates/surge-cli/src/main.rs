//! Headless front end: `surge-cli <url> [-c N] [-o DIR] [--name NAME]`.
//!
//! If the Surge app is running, `--send` hands the URL to it instead.

use std::io::Write;
use std::time::{Duration, Instant};

use anyhow::{Result, bail};
use surge_core::{AddRequest, Engine, Status, ipc, util};

fn main() -> Result<()> {
    let mut args = std::env::args().skip(1);
    let mut req = AddRequest::default();
    let mut send = false;
    while let Some(a) = args.next() {
        match a.as_str() {
            "-c" | "--connections" => req.connections = args.next().and_then(|v| v.parse().ok()),
            // Relative to here; Surge itself only takes full paths.
            "-o" | "--dir" => req.dir = args.next().and_then(|d| std::path::absolute(d).ok()),
            "-n" | "--name" => req.filename = args.next(),
            "-H" | "--header" => {
                if let Some((k, v)) = args.next().as_deref().and_then(|h| h.split_once(':')) {
                    req.headers.push((k.trim().into(), v.trim().into()));
                }
            }
            "-l" | "--limit" => {
                req.speed_limit = args
                    .next()
                    .and_then(|v| v.parse::<u64>().ok())
                    .map(|k| k * 1024)
            }
            "--send" => send = true,
            "-h" | "--help" => {
                println!(
                    "surge-cli {}\n\nusage: surge-cli <url> [-c connections] [-o dir] [-n name] [-l KiB/s] [-H 'K: V']... [--send]",
                    surge_core::VERSION
                );
                return Ok(());
            }
            url if !url.starts_with('-') => req.url = url.into(),
            other => bail!("unknown flag {other}"),
        }
    }
    if req.url.is_empty() {
        bail!("missing URL (see --help)");
    }

    if send {
        let reply = ipc::send(&ipc::IpcRequest::Add(req))?;
        println!("{}", serde_json::to_string(&reply)?);
        return Ok(());
    }

    // Separate state dir so CLI runs don't show up in (or fight with) the app's list.
    let engine = Engine::with_data_dir(&surge_core::data_dir().join("cli"))?;
    let id = engine.add(req)?;
    let started = Instant::now();
    let mut stderr = std::io::stderr();
    loop {
        std::thread::sleep(Duration::from_millis(250));
        let Some(d) = engine.get(id) else {
            bail!("download vanished")
        };
        let pct = d
            .progress()
            .map(|p| format!("{:5.1}%", p * 100.0))
            .unwrap_or_else(|| "  ?  ".into());
        let total = d.total.map(util::fmt_bytes).unwrap_or_else(|| "?".into());
        let eta = d.eta_secs().map(util::fmt_duration).unwrap_or_default();
        let _ = write!(
            stderr,
            "\r\x1b[2K{pct}  {} / {total}  {}  conns {}/{}  segs {}  {eta}  {}",
            util::fmt_bytes(d.downloaded),
            util::fmt_speed(d.speed),
            d.active_connections,
            d.connections,
            d.segments.len(),
            d.status.label(),
        );
        match &d.status {
            Status::Completed => {
                let secs = started.elapsed().as_secs_f64();
                eprintln!(
                    "\n{} in {secs:.1}s (avg {})",
                    d.path
                        .as_ref()
                        .map(|p| p.display().to_string())
                        .unwrap_or_default(),
                    util::fmt_speed(d.downloaded as f64 / secs.max(0.001))
                );
                engine.remove(id, false);
                return Ok(());
            }
            Status::Failed(e) => {
                eprintln!();
                engine.remove(id, false);
                bail!("{e}");
            }
            _ => {}
        }
    }
}
