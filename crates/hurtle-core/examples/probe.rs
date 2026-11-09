//! Print what yt-dlp offers for a page, as the extension's dialog sees it.
//!
//!   cargo run -p hurtle-core --example probe -- <url>

fn main() {
    let url = std::env::args().nth(1).expect("url");
    match hurtle_core::media::probe(&url, &[]) {
        Ok(info) => println!("{}", serde_json::to_string_pretty(&info).unwrap()),
        Err(e) => eprintln!("error: {e} (no video: {})", e.no_video()),
    }
}
