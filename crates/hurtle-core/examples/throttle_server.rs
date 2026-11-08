//! A tiny HTTP/1.1 file server that caps every *connection* at a fixed rate,
//! like many file hosts do. Use it to see segmented downloading pay off.
//!
//!   cargo run -p hurtle-core --example throttle_server -- <file> [port] [KiB/s per connection] [--no-range]
//!
//! Then: hurtle-cli http://127.0.0.1:8090/<anything> -c 1   vs   -c 8

use std::time::{Duration, Instant};

use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncSeekExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let file = args
        .first()
        .cloned()
        .expect("usage: throttle_server <file> [port] [KiB/s] [--no-range]");
    let port: u16 = args.get(1).and_then(|a| a.parse().ok()).unwrap_or(8090);
    let rate: u64 = args.get(2).and_then(|a| a.parse().ok()).unwrap_or(512) * 1024;
    let ranges = !args.iter().any(|a| a == "--no-range");
    let len = std::fs::metadata(&file)?.len();
    let listener = TcpListener::bind(("127.0.0.1", port)).await?;
    eprintln!(
        "serving {file} ({len} bytes) on :{port}, {} KiB/s per connection, ranges={ranges}",
        rate / 1024
    );
    let mut accepted = 0u32;
    loop {
        let (sock, _) = listener.accept().await?;
        accepted += 1;
        eprintln!("accepted connection #{accepted}");
        let file = file.clone();
        tokio::spawn(async move {
            if let Err(e) = handle(sock, &file, len, rate, ranges).await {
                eprintln!("conn: {e}");
            }
        });
    }
}

async fn handle(
    sock: TcpStream,
    path: &str,
    len: u64,
    rate: u64,
    ranges: bool,
) -> anyhow::Result<()> {
    let (r, mut w) = sock.into_split();
    let mut reader = BufReader::new(r);
    loop {
        // Keep-alive: serve requests until the client hangs up.
        let mut line = String::new();
        if reader.read_line(&mut line).await? == 0 {
            return Ok(());
        }
        let mut range = None;
        loop {
            let mut h = String::new();
            reader.read_line(&mut h).await?;
            let h = h.trim_end();
            if h.is_empty() {
                break;
            }
            if let Some((k, v)) = h.split_once(':')
                && k.eq_ignore_ascii_case("range")
            {
                range = parse_range(v.trim(), len);
            }
        }
        let (start, end, head) = match range.filter(|_| ranges) {
            Some((s, e)) => (
                s,
                e,
                format!(
                    "HTTP/1.1 206 Partial Content\r\nContent-Range: bytes {s}-{e}/{len}\r\nContent-Length: {}\r\nAccept-Ranges: bytes\r\nETag: \"v1\"\r\nContent-Disposition: attachment; filename=\"test-file.bin\"\r\n\r\n",
                    e - s + 1
                ),
            ),
            None => (
                0,
                len.saturating_sub(1),
                format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {len}\r\nContent-Disposition: attachment; filename=\"test-file.bin\"\r\n\r\n"
                ),
            ),
        };
        w.write_all(head.as_bytes()).await?;
        if len == 0 {
            continue;
        }
        let mut f = tokio::fs::File::open(path).await?;
        f.seek(std::io::SeekFrom::Start(start)).await?;
        let mut left = end - start + 1;
        let began = Instant::now();
        let mut sent = 0u64;
        let mut buf = vec![0u8; 16 * 1024];
        let cap = buf.len() as u64;
        while left > 0 {
            let n = f.read(&mut buf[..left.min(cap) as usize]).await?;
            if n == 0 {
                break;
            }
            w.write_all(&buf[..n]).await?;
            left -= n as u64;
            sent += n as u64;
            // Token-bucket-ish: sleep until we are back under `rate`.
            let due = Duration::from_secs_f64(sent as f64 / rate as f64);
            if let Some(wait) = due.checked_sub(began.elapsed()) {
                tokio::time::sleep(wait).await;
            }
        }
    }
}

fn parse_range(v: &str, len: u64) -> Option<(u64, u64)> {
    let spec = v.strip_prefix("bytes=")?;
    let (a, b) = spec.split_once('-')?;
    let start: u64 = a.parse().ok()?;
    let end: u64 = if b.is_empty() {
        len - 1
    } else {
        b.parse::<u64>().ok()?.min(len - 1)
    };
    (start <= end).then_some((start, end))
}
