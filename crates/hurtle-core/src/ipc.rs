//! Local IPC between the running app and short-lived clients
//! (the browser's native-messaging host, the CLI, a second app launch).
//!
//! Transport: a Unix socket at `$XDG_RUNTIME_DIR/hurtle.sock` (a named pipe
//! `\\.\pipe\hurtle-<user>` on Windows), one JSON request per line, one JSON
//! reply per line. `HURTLE_SOCKET` overrides either.

use std::collections::BTreeMap;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::path::PathBuf;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::AddRequest;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum IpcRequest {
    /// Queue a download.
    Add(AddRequest),
    /// List the qualities a video page/stream offers (via yt-dlp). Uses
    /// `url` and the header fields of the request; nothing is queued.
    Probe(AddRequest),
    /// Liveness check.
    Ping,
    /// Bring the window to the front.
    Show,
    /// Read the global settings, optionally changing some first.
    Settings(SettingsChange),
    /// Show the app's folder picker; the reply's `folder` is the choice
    /// (none when cancelled).
    PickFolder {
        #[serde(default)]
        title: Option<String>,
        #[serde(default)]
        start: Option<String>,
    },
    /// Append lines (from the extension) to the shared log, then return its
    /// last `tail` bytes. Handled by the host itself.
    #[serde(rename_all = "camelCase")]
    Log {
        #[serde(default)]
        append: Vec<String>,
        #[serde(default)]
        tail: Option<usize>,
    },
}

/// Fields of [`IpcRequest::Settings`]; each one left out stays as it is.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct SettingsChange {
    pub connections: Option<usize>,
    /// Bytes per second, 0 = unlimited.
    pub speed_limit: Option<u64>,
    /// The main download folder (`~` allowed).
    pub download_dir: Option<String>,
    /// Folders per file type; an empty path goes back to the main folder.
    pub type_dirs: Option<BTreeMap<String, String>>,
}

impl SettingsChange {
    pub fn is_empty(&self) -> bool {
        self.connections.is_none()
            && self.speed_limit.is_none()
            && self.download_dir.is_none()
            && self.type_dirs.is_none()
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct IpcReply {
    pub ok: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub settings: Option<crate::Settings>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub media: Option<crate::media::MediaInfo>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub log: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub folder: Option<String>,
    /// `add` gave an existing paused or failed download the new link.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resumed: Option<bool>,
    /// `probe` found a list page (channel, playlist, home page), not a video.
    #[serde(
        default,
        rename = "noVideo",
        skip_serializing_if = "std::ops::Not::not"
    )]
    pub no_video: bool,
}

impl IpcReply {
    /// Answer a [`IpcRequest::Probe`] in-process (the native host does this
    /// itself, so filling the dialog doesn't start the app).
    pub fn probe(req: &AddRequest) -> Self {
        match crate::media::probe(&req.url, &req.header_list()) {
            Ok(info) => Self {
                media: Some(info),
                ..Self::ok(None)
            },
            Err(e) => Self {
                no_video: e.no_video(),
                ..Self::err(e)
            },
        }
    }

    /// Answer a [`IpcRequest::Log`]: append the extension's lines, return the tail.
    pub fn log(append: &[String], tail: Option<usize>) -> Self {
        for line in append {
            crate::log::log("ext", line);
        }
        Self {
            // Native-messaging replies are capped at 1 MB.
            log: tail.map(|n| crate::log::tail(n.min(512 * 1024))),
            ..Self::ok(None)
        }
    }

    pub fn ok(id: Option<u64>) -> Self {
        Self {
            ok: true,
            id,
            version: Some(crate::VERSION.into()),
            ..Default::default()
        }
    }
    pub fn err(e: impl ToString) -> Self {
        Self {
            ok: false,
            error: Some(e.to_string()),
            ..Default::default()
        }
    }
}

pub fn socket_path() -> PathBuf {
    if let Some(p) = std::env::var_os("HURTLE_SOCKET") {
        return p.into();
    }
    if let Some(dir) = std::env::var_os("XDG_RUNTIME_DIR") {
        return PathBuf::from(dir).join("hurtle.sock");
    }
    let user = std::env::var("USER").unwrap_or_else(|_| "user".into());
    std::env::temp_dir().join(format!("hurtle-{user}.sock"))
}

/// `\\.\pipe\hurtle-<user>`, or `HURTLE_SOCKET` (a bare name or a full pipe path).
#[cfg(windows)]
pub fn pipe_name() -> String {
    let name = std::env::var("HURTLE_SOCKET").unwrap_or_else(|_| {
        let user = std::env::var("USERNAME").unwrap_or_else(|_| "user".into());
        format!("hurtle-{user}")
    });
    if name.starts_with(r"\\") {
        name
    } else {
        format!(r"\\.\pipe\{}", name.replace(['\\', '/', ':'], "-"))
    }
}

/// How long to wait for a reply: a probe runs yt-dlp, which can take a
/// while on slow sites.
fn reply_timeout(req: &IpcRequest) -> Duration {
    Duration::from_secs(match req {
        IpcRequest::Probe(_) => 100,
        // The user is choosing a folder.
        IpcRequest::PickFolder { .. } => 610,
        _ => 10,
    })
}

fn exchange(mut stream: impl Read + Write, req: &IpcRequest) -> io::Result<IpcReply> {
    let mut line = serde_json::to_string(req)?;
    line.push('\n');
    stream.write_all(line.as_bytes())?;
    stream.flush()?;
    let mut reply = String::new();
    BufReader::new(stream).read_line(&mut reply)?;
    if reply.is_empty() {
        return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "no reply"));
    }
    serde_json::from_str(&reply).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}

/// Send one request to the running app. Fails fast if nothing is listening.
#[cfg(unix)]
pub fn send(req: &IpcRequest) -> io::Result<IpcReply> {
    use std::os::unix::net::UnixStream;
    let stream = UnixStream::connect(socket_path())?;
    stream.set_read_timeout(Some(reply_timeout(req)))?;
    stream.set_write_timeout(Some(Duration::from_secs(5)))?;
    exchange(stream, req)
}

/// Send one request to the running app. Fails fast if nothing is listening.
#[cfg(windows)]
pub fn send(req: &IpcRequest) -> io::Result<IpcReply> {
    // A named pipe opens like a file. All instances busy → retry briefly.
    const ERROR_PIPE_BUSY: i32 = 231;
    let name = pipe_name();
    let mut tries = 0;
    let pipe = loop {
        match std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&name)
        {
            Ok(p) => break p,
            Err(e) if e.raw_os_error() == Some(ERROR_PIPE_BUSY) && tries < 50 => {
                tries += 1;
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(e) => return Err(e),
        }
    };
    // Pipe handles have no read timeout; wait on a thread instead.
    let (tx, rx) = std::sync::mpsc::channel();
    let req2 = req.clone();
    std::thread::spawn(move || {
        let _ = tx.send(exchange(pipe, &req2));
    });
    rx.recv_timeout(reply_timeout(req)).unwrap_or_else(|_| {
        Err(io::Error::new(
            io::ErrorKind::TimedOut,
            "Hurtle did not reply",
        ))
    })
}
