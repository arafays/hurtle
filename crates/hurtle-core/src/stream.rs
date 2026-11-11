//! A small HTTP server that streams downloads to a video player while they
//! are still downloading (torrents), or from disk once they are complete.
//!
//! - `GET /<token>/<download id>/<file index>/<name>`: the file, with `Range`
//!   support so players can seek. Torrent pieces at the read position are
//!   fetched first (librqbit's `FileStream`), so playback starts long before
//!   the download ends.
//! - `GET /<token>/<download id>.m3u`: a playlist of the download's playable
//!   files (a season, an album).
//!
//! It listens on 127.0.0.1 only, unless `Settings.stream_lan` is on, so a TV
//! or phone on the same network can open the URL in VLC ("Open Network
//! Stream"). The random token in every path keeps web pages (which can reach
//! localhost) and other LAN devices from reading files they weren't given a
//! link to.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::Path;
use std::sync::Arc;

use tokio::io::{AsyncRead, AsyncReadExt, AsyncSeek, AsyncSeekExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

use crate::{Inner, Kind, Status, torrent};

pub(crate) struct Server {
    pub(crate) addr: SocketAddr,
    pub(crate) token: String,
    /// Reachable from other devices.
    pub(crate) lan: bool,
}

/// The running server, started on first use.
pub(crate) async fn server(inner: &Arc<Inner>) -> Result<&Server, String> {
    inner
        .stream
        .get_or_try_init(|| async {
            let (lan, port) = {
                let s = inner.settings.lock().unwrap();
                (s.stream_lan, s.stream_port)
            };
            let bind: SocketAddr = if lan {
                (Ipv4Addr::UNSPECIFIED, port).into()
            } else {
                (Ipv4Addr::LOCALHOST, port).into()
            };
            let listener = match TcpListener::bind(bind).await {
                Ok(l) => l,
                // The fixed port is taken: any port will do.
                Err(_) if port != 0 => TcpListener::bind(SocketAddr::new(bind.ip(), 0))
                    .await
                    .map_err(|e| format!("starting the stream server: {e}"))?,
                Err(e) => return Err(format!("starting the stream server: {e}")),
            };
            let addr = listener.local_addr().map_err(|e| e.to_string())?;
            let token = random_token();
            crate::log::log(
                "stream",
                format!("serving on {addr}{}", if lan { " (LAN)" } else { "" }),
            );
            let weak = Arc::downgrade(inner);
            let tok = token.clone();
            tokio::spawn(async move {
                loop {
                    let Ok((sock, _)) = listener.accept().await else {
                        continue;
                    };
                    let Some(inner) = weak.upgrade() else { return };
                    let tok = tok.clone();
                    tokio::spawn(async move {
                        let _ = serve(inner, sock, &tok).await;
                    });
                }
            });
            Ok(Server { addr, token, lan })
        })
        .await
}

fn random_token() -> String {
    let mut b = [0u8; 16];
    if getrandom::fill(&mut b).is_err() {
        // Never expected; still unguessable enough across a process lifetime.
        let t = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        b.copy_from_slice(&t.to_le_bytes());
    }
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// The address other devices reach us on: the LAN IP when sharing.
fn public_host(s: &Server) -> String {
    if !s.lan {
        return s.addr.ip().to_string();
    }
    // The interface the default route uses (no packet is sent).
    let ip = std::net::UdpSocket::bind("0.0.0.0:0")
        .and_then(|u| {
            u.connect("192.0.2.1:9")?;
            u.local_addr()
        })
        .map(|a| a.ip())
        .unwrap_or(IpAddr::V4(Ipv4Addr::LOCALHOST));
    ip.to_string()
}

/// `http://127.0.0.1:port/<token>/<id>/<file>/<name>`.
pub(crate) fn file_url(s: &Server, id: u64, file: usize, name: &str) -> String {
    let base = Path::new(name)
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "file".into());
    let enc = percent_encoding::utf8_percent_encode(&base, percent_encoding::NON_ALPHANUMERIC);
    format!(
        "http://{}:{}/{}/{id}/{file}/{enc}",
        public_host(s),
        s.addr.port(),
        s.token
    )
}

pub(crate) fn playlist_url(s: &Server, id: u64) -> String {
    format!(
        "http://{}:{}/{}/{id}.m3u",
        public_host(s),
        s.addr.port(),
        s.token
    )
}

struct Request {
    method: String,
    path: String,
    range: Option<String>,
}

async fn read_request(sock: &mut TcpStream) -> Option<Request> {
    let mut buf = Vec::with_capacity(1024);
    let mut chunk = [0u8; 1024];
    loop {
        let n = tokio::time::timeout(std::time::Duration::from_secs(15), sock.read(&mut chunk))
            .await
            .ok()?
            .ok()?;
        if n == 0 {
            return None;
        }
        buf.extend_from_slice(&chunk[..n]);
        if buf.windows(4).any(|w| w == b"\r\n\r\n") {
            break;
        }
        if buf.len() > 16 * 1024 {
            return None;
        }
    }
    let text = String::from_utf8_lossy(&buf);
    let mut lines = text.split("\r\n");
    let mut first = lines.next()?.split_whitespace();
    let method = first.next()?.to_string();
    let path = first.next()?.to_string();
    let range = lines
        .take_while(|l| !l.is_empty())
        .filter_map(|l| l.split_once(':'))
        .find(|(k, _)| k.trim().eq_ignore_ascii_case("range"))
        .map(|(_, v)| v.trim().to_string());
    Some(Request {
        method,
        path,
        range,
    })
}

async fn reply(sock: &mut TcpStream, status: &str, body: &str) -> std::io::Result<()> {
    let head = format!(
        "HTTP/1.1 {status}\r\nContent-Type: text/plain; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    sock.write_all(head.as_bytes()).await
}

/// `bytes=a-b`, `bytes=a-`, `bytes=-n` → `[start, end]` (inclusive).
pub(crate) fn parse_range(h: &str, len: u64) -> Option<(u64, u64)> {
    let spec = h.trim().strip_prefix("bytes=")?;
    // Only the first range of a multi-range request.
    let spec = spec.split(',').next()?.trim();
    let (a, b) = spec.split_once('-')?;
    if len == 0 {
        return None;
    }
    let (start, end) = match (a.trim(), b.trim()) {
        ("", n) => {
            let n: u64 = n.parse().ok()?;
            (len.saturating_sub(n), len - 1)
        }
        (a, "") => (a.parse().ok()?, len - 1),
        (a, b) => (a.parse().ok()?, b.parse::<u64>().ok()?.min(len - 1)),
    };
    (start <= end && start < len).then_some((start, end))
}

pub(crate) fn content_type(name: &str) -> &'static str {
    let ext = Path::new(name)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    match ext.as_str() {
        "mp4" | "m4v" => "video/mp4",
        "mkv" => "video/x-matroska",
        "webm" => "video/webm",
        "avi" => "video/x-msvideo",
        "mov" => "video/quicktime",
        "wmv" => "video/x-ms-wmv",
        "flv" => "video/x-flv",
        "ts" | "m2ts" => "video/mp2t",
        "mpg" | "mpeg" => "video/mpeg",
        "ogv" => "video/ogg",
        "3gp" => "video/3gpp",
        "mp3" => "audio/mpeg",
        "m4a" | "aac" => "audio/mp4",
        "flac" => "audio/flac",
        "wav" => "audio/wav",
        "ogg" | "oga" | "opus" => "audio/ogg",
        "srt" => "application/x-subrip",
        "vtt" => "text/vtt",
        _ => "application/octet-stream",
    }
}

async fn serve(inner: Arc<Inner>, mut sock: TcpStream, token: &str) -> std::io::Result<()> {
    let Some(req) = read_request(&mut sock).await else {
        return Ok(());
    };
    if req.method != "GET" && req.method != "HEAD" {
        return reply(&mut sock, "405 Method Not Allowed", "GET only\n").await;
    }
    let path = req.path.split('?').next().unwrap_or("");
    let mut parts = path.trim_start_matches('/').splitn(4, '/');
    // Constant-time enough: the token is compared whole, and a miss says nothing.
    if parts.next() != Some(token) {
        return reply(&mut sock, "404 Not Found", "not found\n").await;
    }
    let Some(second) = parts.next() else {
        return reply(&mut sock, "404 Not Found", "not found\n").await;
    };
    if let Some(id) = second
        .strip_suffix(".m3u")
        .and_then(|i| i.parse::<u64>().ok())
    {
        return playlist(&inner, &mut sock, id, &req).await;
    }
    let (Some(id), Some(file)) = (
        second.parse::<u64>().ok(),
        parts.next().and_then(|f| f.parse::<usize>().ok()),
    ) else {
        return reply(&mut sock, "404 Not Found", "not found\n").await;
    };
    let Some(job) = inner.job(id) else {
        return reply(&mut sock, "404 Not Found", "no such download\n").await;
    };
    let rec = job.rec.lock().unwrap().clone();
    let (name, size, complete_on_disk, disk_path, hash) = match rec.kind {
        Kind::Torrent => {
            let Some(t) = rec.torrent.as_ref() else {
                return reply(&mut sock, "404 Not Found", "no file list yet\n").await;
            };
            let Some(f) = t.files.get(file) else {
                return reply(&mut sock, "404 Not Found", "no such file\n").await;
            };
            let p = torrent::file_path(&rec, file);
            (
                f.path.clone(),
                f.size,
                f.done >= f.size && p.as_ref().is_some_and(|p| p.is_file()),
                p,
                Some(t.info_hash.clone()),
            )
        }
        _ => {
            let Some(p) = rec.path.clone().filter(|_| file == 0) else {
                return reply(&mut sock, "404 Not Found", "no such file\n").await;
            };
            let len = std::fs::metadata(&p).map(|m| m.len()).unwrap_or(0);
            (
                rec.filename.clone(),
                len,
                rec.status == Status::Completed,
                Some(p),
                None,
            )
        }
    };
    if complete_on_disk
        && let Some(p) = disk_path
        && let Ok(f) = tokio::fs::File::open(&p).await
    {
        let len = f.metadata().await.map(|m| m.len()).unwrap_or(size);
        return send(&mut sock, &req, f, len, &name).await;
    }
    let handle = hash.and_then(|h| torrent::live_handle(&inner, &h));
    let Some(handle) = handle else {
        return reply(
            &mut sock,
            "503 Service Unavailable",
            "start the download in Hurtle to stream it\n",
        )
        .await;
    };
    match handle.stream(file).await {
        Ok(stream) => {
            let len = stream.len();
            send(&mut sock, &req, stream, len, &name).await
        }
        Err(e) => {
            reply(
                &mut sock,
                "503 Service Unavailable",
                &format!("can't stream yet: {e:#}\n"),
            )
            .await
        }
    }
}

async fn send<R: AsyncRead + AsyncSeek + Unpin>(
    sock: &mut TcpStream,
    req: &Request,
    mut body: R,
    len: u64,
    name: &str,
) -> std::io::Result<()> {
    let ctype = content_type(name);
    let (status, start, end) = match req.range.as_deref() {
        Some(r) => match parse_range(r, len) {
            Some((s, e)) => ("206 Partial Content", s, e),
            None => {
                let head = format!(
                    "HTTP/1.1 416 Range Not Satisfiable\r\nContent-Range: bytes */{len}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                );
                return sock.write_all(head.as_bytes()).await;
            }
        },
        None => ("200 OK", 0, len.saturating_sub(1)),
    };
    let count = if len == 0 { 0 } else { end - start + 1 };
    let mut head = format!(
        "HTTP/1.1 {status}\r\nContent-Type: {ctype}\r\nContent-Length: {count}\r\nAccept-Ranges: bytes\r\nCache-Control: no-store\r\nConnection: close\r\n"
    );
    if status.starts_with("206") {
        head.push_str(&format!("Content-Range: bytes {start}-{end}/{len}\r\n"));
    }
    head.push_str("\r\n");
    sock.write_all(head.as_bytes()).await?;
    if req.method == "HEAD" || count == 0 {
        return Ok(());
    }
    body.seek(std::io::SeekFrom::Start(start)).await?;
    let mut limited = body.take(count);
    tokio::io::copy_buf(
        &mut tokio::io::BufReader::with_capacity(256 * 1024, &mut limited),
        sock,
    )
    .await?;
    sock.flush().await
}

async fn playlist(
    inner: &Arc<Inner>,
    sock: &mut TcpStream,
    id: u64,
    req: &Request,
) -> std::io::Result<()> {
    let Some(job) = inner.job(id) else {
        return reply(sock, "404 Not Found", "no such download\n").await;
    };
    let rec = job.rec.lock().unwrap().clone();
    let Some(server) = inner.stream.get() else {
        return Ok(());
    };
    let mut body = String::from("#EXTM3U\n");
    let files: Vec<(usize, String)> = match &rec.torrent {
        Some(t) => t
            .files
            .iter()
            .enumerate()
            .filter(|(_, f)| f.selected && !f.padding && crate::player::is_playable(&f.path))
            .map(|(i, f)| (i, f.path.clone()))
            .collect(),
        None => vec![(0, rec.filename.clone())],
    };
    let mut sorted = files;
    sorted.sort_by(|a, b| a.1.cmp(&b.1));
    for (i, name) in sorted {
        let title = Path::new(&name)
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        body.push_str(&format!(
            "#EXTINF:-1,{title}\n{}\n",
            file_url(server, id, i, &name)
        ));
    }
    let head = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: audio/x-mpegurl; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    sock.write_all(head.as_bytes()).await?;
    if req.method == "GET" {
        sock.write_all(body.as_bytes()).await?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ranges() {
        assert_eq!(parse_range("bytes=0-", 100), Some((0, 99)));
        assert_eq!(parse_range("bytes=10-19", 100), Some((10, 19)));
        assert_eq!(parse_range("bytes=90-200", 100), Some((90, 99)));
        assert_eq!(parse_range("bytes=-10", 100), Some((90, 99)));
        assert_eq!(parse_range("bytes=100-", 100), None);
        assert_eq!(parse_range("bytes=5-1", 100), None);
        assert_eq!(parse_range("bytes=0-1,5-6", 100), Some((0, 1)));
        assert_eq!(parse_range("items=0-1", 100), None);
    }

    #[test]
    fn types() {
        assert_eq!(content_type("a/Movie.MKV"), "video/x-matroska");
        assert_eq!(content_type("x.mp4"), "video/mp4");
        assert_eq!(content_type("x.bin"), "application/octet-stream");
    }
}
