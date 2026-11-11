//! Seed a file or folder on this machine and print a magnet link for it
//! (with `x.pe=` pointing here, so no tracker or DHT is needed), for trying
//! Hurtle's torrent support without the internet.
//!
//! `cargo run -p hurtle-core --example seed -- <path> [KiB/s upload limit]`

use std::net::{Ipv4Addr, SocketAddr};
use std::num::NonZeroU32;
use std::path::PathBuf;

use anyhow::{Context, Result};

fn main() -> Result<()> {
    let mut args = std::env::args().skip(1);
    let path = PathBuf::from(args.next().context("usage: seed <path> [KiB/s]")?);
    let path = std::path::absolute(path)?;
    let limit = args
        .next()
        .and_then(|k| k.parse::<u32>().ok())
        .and_then(|k| NonZeroU32::new(k * 1024));
    let rt = tokio::runtime::Runtime::new()?;
    rt.block_on(async {
        let session = librqbit::Session::new_with_opts(
            path.parent().context("no parent folder")?.to_path_buf(),
            librqbit::SessionOptions {
                dht: None,
                disable_local_service_discovery: true,
                ipv4_only: true,
                listen: Some(librqbit::ListenerOptions {
                    mode: librqbit::ListenerMode::TcpOnly,
                    listen_addr: (Ipv4Addr::LOCALHOST, 0).into(),
                    enable_upnp_port_forwarding: false,
                    ipv4_only: true,
                    ..Default::default()
                }),
                ratelimits: librqbit::limits::LimitsConfig {
                    upload_bps: limit,
                    download_bps: None,
                },
                ..Default::default()
            },
        )
        .await?;
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        let (created, _h) = session
            .create_and_serve_torrent(
                &path,
                librqbit::CreateTorrentOptions {
                    name: Some(&name),
                    trackers: vec![],
                    piece_length: None,
                },
            )
            .await
            .map_err(|e| anyhow::anyhow!("{e:?}"))?;
        let port = session.listen_addr().context("not listening")?.port();
        let peer = SocketAddr::from((Ipv4Addr::LOCALHOST, port));
        let torrent = std::env::temp_dir().join(format!("{name}.torrent"));
        std::fs::write(&torrent, created.as_bytes()?)?;
        println!(
            "magnet:?xt=urn:btih:{}&dn={}&x.pe={peer}",
            created.info_hash().as_string(),
            percent_encoding::utf8_percent_encode(&name, percent_encoding::NON_ALPHANUMERIC)
        );
        eprintln!(
            "seeding {} (also {}); Ctrl-C to stop",
            path.display(),
            torrent.display()
        );
        tokio::signal::ctrl_c().await?;
        anyhow::Ok(())
    })
}
