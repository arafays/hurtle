//! Native-messaging host. The browser starts this process, writes
//! length-prefixed JSON messages to stdin and reads replies from stdout.
//! Each message is relayed to the running Hurtle app over its IPC socket;
//! if Hurtle isn't running it is launched first.
//!
//! stdout is the protocol channel: nothing else may ever be printed there.

use std::io::{self, Read, Write};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use hurtle_core::ipc::{self, IpcReply, IpcRequest};

fn main() {
    // Setup commands (run by a person or install.sh, never by a browser).
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("--register") => return register(args.get(1)),
        Some("--unregister") => {
            for place in hurtle_core::nmh::unregister() {
                println!("removed {place}");
            }
            return;
        }
        Some("--version") => return println!("hurtle-host {}", hurtle_core::VERSION),
        _ => {}
    }

    let mut stdin = io::stdin().lock();
    let mut stdout = io::stdout().lock();
    // Chrome passes the caller origin (and on Windows a window handle);
    // Firefox the manifest path and the add-on id. Only logged.
    let caller = args.last().cloned().unwrap_or_default();
    while let Some(msg) = read_message(&mut stdin) {
        let started = Instant::now();
        let (reply, what) = match serde_json::from_slice::<IpcRequest>(&msg) {
            Ok(req) => {
                let what = describe(&req);
                (relay(&req), what)
            }
            Err(e) => (
                IpcReply::err(format!("bad message: {e}")),
                "unreadable message".into(),
            ),
        };
        let bytes = serde_json::to_vec(&reply).unwrap_or_else(|_| b"{\"ok\":false}".to_vec());
        let written = write_message(&mut stdout, &bytes);
        // Reading the log is noise in the log itself.
        if what != "log" {
            hurtle_core::log::log(
                "host",
                format!(
                    "{what} from {caller}: {} in {:.1}s, reply {} bytes, {}",
                    match (&reply.ok, &reply.error) {
                        (true, None) => "ok".to_string(),
                        (_, Some(e)) =>
                            format!("{} \"{e}\"", if reply.ok { "ok" } else { "error" }),
                        (false, None) => "error".to_string(),
                    },
                    started.elapsed().as_secs_f64(),
                    bytes.len(),
                    match &written {
                        Ok(()) => "delivered".to_string(),
                        Err(e) => format!("NOT delivered: {e}"),
                    }
                ),
            );
        }
        if written.is_err() {
            break;
        }
    }
}

/// "probe https://site/path", "add https://…", "ping" — never cookies.
fn describe(req: &IpcRequest) -> String {
    match req {
        IpcRequest::Add(a) => format!("add {}", hurtle_core::log::url(&a.url)),
        IpcRequest::Probe(a) => format!("probe {}", hurtle_core::log::url(&a.url)),
        IpcRequest::Ping => "ping".into(),
        IpcRequest::Show => "show".into(),
        IpcRequest::Settings(c) => {
            if c.is_empty() {
                "settings".into()
            } else {
                format!(
                    "settings connections={:?} speed_limit={:?} auto_retry={:?} notify={:?} download_dir={:?} type_dirs={:?}",
                    c.connections,
                    c.speed_limit,
                    c.auto_retry,
                    c.notify,
                    c.download_dir,
                    c.type_dirs
                )
            }
        }
        IpcRequest::PickFolder { .. } => "pick folder".into(),
        IpcRequest::Log { .. } => "log".into(),
    }
}

fn register(path: Option<&String>) {
    let host = match path {
        Some(p) => PathBuf::from(p),
        None => std::env::current_exe().expect("own path"),
    };
    match hurtle_core::nmh::register(&host) {
        Ok(places) => {
            for place in &places {
                println!("registered {place}");
            }
            if places.is_empty() {
                println!("no supported browser found");
            }
        }
        Err(e) => {
            eprintln!("hurtle-host: {e:#}");
            std::process::exit(1);
        }
    }
}

fn relay(req: &IpcRequest) -> IpcReply {
    // Listing a page's qualities needs only yt-dlp, not the app.
    if let IpcRequest::Probe(p) = req {
        return IpcReply::probe(p);
    }
    if let IpcRequest::Log { append, tail } = req {
        return IpcReply::log(append, *tail);
    }
    match ipc::send(req) {
        Ok(r) => return r,
        // A ping only asks whether the host itself is installed; don't launch the app for it.
        Err(_) if matches!(req, IpcRequest::Ping) => {
            return IpcReply {
                ok: true,
                version: Some(hurtle_core::VERSION.into()),
                error: Some("Hurtle app is not running (it starts on the first download)".into()),
                ..Default::default()
            };
        }
        // Nor for settings: with the app closed, its settings file is ours to
        // read and change (it is read again when the app starts).
        Err(_) if matches!(req, IpcRequest::Settings(_)) => {
            let IpcRequest::Settings(change) = req else {
                unreachable!()
            };
            if let Err(e) = hurtle_core::check_ipc_settings(change) {
                return IpcReply::err(e);
            }
            let mut s = hurtle_core::Settings::load();
            return match s.apply(change) {
                Ok(changed) => {
                    if changed && let Err(e) = s.save() {
                        return IpcReply::err(format!("could not save settings: {e}"));
                    }
                    IpcReply {
                        settings: Some(s),
                        ..IpcReply::ok(None)
                    }
                }
                Err(e) => IpcReply::err(e),
            };
        }
        Err(_) => {}
    }
    if let Err(e) = launch_app() {
        return IpcReply::err(format!("could not start Hurtle: {e}"));
    }
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        std::thread::sleep(Duration::from_millis(150));
        match ipc::send(req) {
            Ok(r) => return r,
            Err(e) if Instant::now() > deadline => {
                return IpcReply::err(format!("Hurtle did not come up: {e}"));
            }
            Err(_) => {}
        }
    }
}

fn app_binary() -> PathBuf {
    if let Some(p) = std::env::var_os("HURTLE_APP") {
        return p.into();
    }
    // Installed side by side (same dir as this host), else rely on PATH.
    std::env::current_exe()
        .ok()
        .and_then(|exe| {
            exe.parent()
                .map(|d| d.join(format!("hurtle{}", std::env::consts::EXE_SUFFIX)))
        })
        .filter(|p| p.exists())
        .unwrap_or_else(|| PathBuf::from("hurtle"))
}

fn launch_app() -> io::Result<()> {
    let mut cmd = Command::new(app_binary());
    cmd.stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    #[cfg(unix)]
    {
        // Own process group: closing the browser must not take Hurtle down with it.
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // Detached, and out of the browser's job object (Firefox kills the
        // host's children with it otherwise).
        const DETACHED_PROCESS: u32 = 0x0000_0008;
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
        const CREATE_BREAKAWAY_FROM_JOB: u32 = 0x0100_0000;
        cmd.creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP | CREATE_BREAKAWAY_FROM_JOB);
        match cmd.spawn() {
            Ok(_) => return Ok(()),
            // The job doesn't allow breakaway: start it inside, still detached.
            Err(e) if e.kind() == io::ErrorKind::PermissionDenied => {
                cmd.creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP);
            }
            Err(e) => return Err(e),
        }
    }
    cmd.spawn().map(drop)
}

fn read_message(r: &mut impl Read) -> Option<Vec<u8>> {
    let mut len = [0u8; 4];
    r.read_exact(&mut len).ok()?;
    let len = u32::from_ne_bytes(len) as usize;
    if len > 4 * 1024 * 1024 {
        return None;
    }
    let mut buf = vec![0u8; len];
    r.read_exact(&mut buf).ok()?;
    Some(buf)
}

fn write_message(w: &mut impl Write, bytes: &[u8]) -> io::Result<()> {
    w.write_all(&(bytes.len() as u32).to_ne_bytes())?;
    w.write_all(bytes)?;
    w.flush()
}
