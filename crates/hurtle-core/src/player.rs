//! Finding a video player (VLC first) and handing it a stream URL.
//!
//! The browser starts Hurtle without the user's shell PATH, so this looks in
//! the usual install places too ([`crate::tools::search_dirs`]), plus Flatpak
//! and the macOS/Windows application folders.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use serde::{Deserialize, Serialize};

/// A player that can open an `http://` stream URL.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Player {
    /// Stable id for settings ("vlc", "mpv", "vlc-flatpak"…).
    pub id: String,
    /// Shown in the UI ("VLC", "mpv (Flatpak)").
    pub name: String,
    pub program: PathBuf,
    /// Arguments before the URL.
    pub args: Vec<String>,
}

/// Players on PATH (or the usual bin dirs): (id, name, executable).
const ON_PATH: &[(&str, &str, &str)] = &[
    ("vlc", "VLC", "vlc"),
    ("mpv", "mpv", "mpv"),
    ("celluloid", "Celluloid", "celluloid"),
    ("haruna", "Haruna", "haruna"),
    ("smplayer", "SMPlayer", "smplayer"),
    ("mplayer", "MPlayer", "mplayer"),
    ("totem", "GNOME Videos", "totem"),
    ("potplayer", "PotPlayer", "PotPlayerMini64"),
];

/// Flatpak apps: (id, name, app id).
#[cfg(not(any(windows, target_os = "macos")))]
const FLATPAKS: &[(&str, &str, &str)] = &[
    ("vlc-flatpak", "VLC (Flatpak)", "org.videolan.VLC"),
    ("mpv-flatpak", "mpv (Flatpak)", "io.mpv.Mpv"),
    (
        "celluloid-flatpak",
        "Celluloid (Flatpak)",
        "io.github.celluloid_player.Celluloid",
    ),
    ("haruna-flatpak", "Haruna (Flatpak)", "org.kde.haruna"),
];

fn exe(name: &str) -> String {
    format!("{name}{}", std::env::consts::EXE_SUFFIX)
}

/// Every player found, VLC first.
pub fn detect() -> Vec<Player> {
    let mut found: Vec<Player> = Vec::new();
    let mut push = |p: Player| {
        if !found.iter().any(|f| f.id == p.id) {
            found.push(p);
        }
    };
    let dirs = crate::tools::search_dirs();
    for (id, name, bin) in ON_PATH {
        if let Some(program) = dirs.iter().map(|d| d.join(exe(bin))).find(|p| p.is_file()) {
            push(Player {
                id: (*id).into(),
                name: (*name).into(),
                program,
                args: vec![],
            });
        }
    }
    for p in platform_players() {
        push(p);
    }
    found.sort_by_key(|p| !p.id.starts_with("vlc"));
    found
}

#[cfg(target_os = "macos")]
fn platform_players() -> Vec<Player> {
    let mut out = vec![];
    let apps: Vec<PathBuf> = std::iter::once(PathBuf::from("/Applications"))
        .chain(dirs::home_dir().map(|h| h.join("Applications")))
        .collect();
    for (id, name, bundle, bin) in [
        ("vlc", "VLC", "VLC.app", "Contents/MacOS/VLC"),
        ("iina", "IINA", "IINA.app", "Contents/MacOS/iina-cli"),
        ("mpv", "mpv", "mpv.app", "Contents/MacOS/mpv"),
    ] {
        if let Some(program) = apps
            .iter()
            .map(|a| a.join(bundle).join(bin))
            .find(|p| p.is_file())
        {
            out.push(Player {
                id: id.into(),
                name: name.into(),
                program,
                args: vec![],
            });
        }
    }
    out
}

#[cfg(windows)]
fn platform_players() -> Vec<Player> {
    let mut out = vec![];
    let roots: Vec<PathBuf> = ["ProgramFiles", "ProgramFiles(x86)", "LOCALAPPDATA"]
        .iter()
        .filter_map(std::env::var_os)
        .map(PathBuf::from)
        .collect();
    for (id, name, rel) in [
        ("vlc", "VLC", "VideoLAN\\VLC\\vlc.exe"),
        ("mpc-hc", "MPC-HC", "MPC-HC\\mpc-hc64.exe"),
        (
            "potplayer",
            "PotPlayer",
            "DAUM\\PotPlayer\\PotPlayerMini64.exe",
        ),
        ("mpv", "mpv", "mpv\\mpv.exe"),
    ] {
        if let Some(program) = roots.iter().map(|r| r.join(rel)).find(|p| p.is_file()) {
            out.push(Player {
                id: id.into(),
                name: name.into(),
                program,
                args: vec![],
            });
        }
    }
    out
}

#[cfg(not(any(windows, target_os = "macos")))]
fn platform_players() -> Vec<Player> {
    let mut roots = vec![PathBuf::from("/var/lib/flatpak/app")];
    if let Some(d) = dirs::data_dir() {
        roots.push(d.join("flatpak/app"));
    }
    let Some(flatpak) = crate::tools::search_dirs()
        .into_iter()
        .map(|d| d.join("flatpak"))
        .find(|p| p.is_file())
    else {
        return vec![];
    };
    FLATPAKS
        .iter()
        .filter(|(_, _, app)| roots.iter().any(|r| r.join(app).is_dir()))
        .map(|(id, name, app)| Player {
            id: (*id).into(),
            name: (*name).into(),
            program: flatpak.clone(),
            args: vec!["run".into(), (*app).into()],
        })
        .collect()
}

/// The player to use: the one named in settings (an id from [`detect`] or
/// a path to a program), else the first found.
pub fn choose(preferred: &str) -> Option<Player> {
    let preferred = preferred.trim();
    let found = detect();
    if !preferred.is_empty() {
        if let Some(p) = found.iter().find(|p| p.id == preferred) {
            return Some(p.clone());
        }
        let path = Path::new(preferred);
        if path.is_absolute() && path.is_file() {
            return Some(Player {
                id: "custom".into(),
                name: path
                    .file_stem()
                    .map(|s| s.to_string_lossy().into_owned())
                    .unwrap_or_else(|| "Player".into()),
                program: path.to_path_buf(),
                args: vec![],
            });
        }
    }
    found.into_iter().next()
}

/// Open `url` in the chosen player, or with the system's default handler
/// when no player is installed. Returns what it was opened with.
pub fn open(url: &str, preferred: &str, title: Option<&str>) -> Result<String, String> {
    if !url.starts_with("http://") {
        return Err("only http stream URLs can be played".into());
    }
    match choose(preferred) {
        Some(p) => {
            let mut cmd = Command::new(&p.program);
            cmd.args(&p.args);
            if let Some(t) = title.filter(|_| p.id.starts_with("vlc")) {
                cmd.arg(format!("--meta-title={t}"));
            }
            if let Some(t) = title.filter(|_| p.id.starts_with("mpv")) {
                cmd.arg(format!("--force-media-title={t}"));
            }
            cmd.arg(url);
            spawn_detached(cmd).map_err(|e| format!("starting {}: {e}", p.name))?;
            Ok(p.name)
        }
        None => {
            open_with_system(url)?;
            Ok("the default app".into())
        }
    }
}

/// `xdg-open` / `open` / `start`.
pub fn open_with_system(target: &str) -> Result<(), String> {
    let cmd = if cfg!(windows) {
        let mut c = Command::new("cmd");
        c.args(["/C", "start", ""]).arg(target);
        c
    } else if cfg!(target_os = "macos") {
        let mut c = Command::new("open");
        c.arg(target);
        c
    } else {
        let mut c = Command::new("xdg-open");
        c.arg(target);
        c
    };
    spawn_detached(cmd).map_err(|e| format!("opening {target}: {e}"))
}

/// Start a program that outlives us: no stdio, its own process group (so a
/// Ctrl-C to Hurtle's terminal doesn't reach it), reaped on a thread.
fn spawn_detached(mut cmd: Command) -> std::io::Result<()> {
    cmd.stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP
        cmd.creation_flags(0x0000_0008 | 0x0000_0200);
    }
    let mut child = cmd.spawn()?;
    std::thread::spawn(move || {
        let _ = child.wait();
    });
    Ok(())
}

/// Whether a file name looks like something a player opens.
pub fn is_playable(name: &str) -> bool {
    matches!(crate::util::category(name), Some("video") | Some("audio"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn playable() {
        assert!(is_playable("Movie.2024.1080p.mkv"));
        assert!(is_playable("song.flac"));
        assert!(!is_playable("readme.txt"));
        assert!(!is_playable("sample.nfo"));
    }

    #[test]
    fn refuses_non_http() {
        assert!(open("file:///etc/passwd", "", None).is_err());
    }
}
