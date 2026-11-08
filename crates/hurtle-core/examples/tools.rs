//! Find or fetch yt-dlp, ffmpeg and deno, then print each one's version.
//! To test fetching on a machine that has them installed:
//!
//!   HURTLE_TOOLS_ONLY=1 cargo run -p hurtle-core --example tools
//!
//! (mise points HURTLE_DATA at .dev-data; delete .dev-data/tools to refetch.)

use hurtle_core::tools::{self, Tool};

fn main() {
    println!("tools dir: {}", tools::dir().display());
    for tool in [Tool::YtDlp, Tool::Ffmpeg, Tool::Deno] {
        let started = std::time::Instant::now();
        let progress = |t: Tool, got: u64, total: Option<u64>| {
            eprint!("\r{}: {} / {:?} bytes   ", t.name(), got, total);
        };
        match tools::ensure(tool, &progress) {
            Ok(path) => {
                let version = std::process::Command::new(&path)
                    .arg(if tool == Tool::Ffmpeg {
                        "-version"
                    } else {
                        "--version"
                    })
                    .output()
                    .map(|o| {
                        String::from_utf8_lossy(&o.stdout)
                            .lines()
                            .next()
                            .unwrap_or_default()
                            .to_string()
                    })
                    .unwrap_or_else(|e| format!("does not run: {e}"));
                eprintln!();
                println!(
                    "{}: {} ({version}) in {:.1?}",
                    tool.name(),
                    path.display(),
                    started.elapsed()
                );
            }
            Err(e) => println!("{}: {e}", tool.name()),
        }
    }
}
