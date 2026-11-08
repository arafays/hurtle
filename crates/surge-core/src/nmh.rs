//! Registering `surge-host` as a native-messaging host, so the browser
//! extension can reach Surge.
//!
//! - Linux: a manifest in each Chrome-family config dir that exists, and in
//!   `~/.mozilla` (plus Firefox forks that exist).
//! - macOS: the same under `~/Library/Application Support`.
//! - Windows: manifests in Surge's data dir, pointed to by registry keys under
//!   `HKCU\Software\<browser>\NativeMessagingHosts`.
//!
//! Chrome-family manifests list the extension's origin (`allowed_origins`),
//! Firefox ones its add-on id (`allowed_extensions`).

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde_json::json;

pub const HOST_NAME: &str = "com.surge.downloader";
/// Pinned by the `key` in the Chrome manifest (extension/chrome-id.txt).
pub const CHROME_ID: &str = "llabdeoinnakkcncicakgafkjkkjfdgh";
pub const FIREFOX_ID: &str = "surge@surge-downloader";

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Family {
    Chrome,
    Firefox,
}

fn manifest(family: Family, host: &Path) -> String {
    let mut m = json!({
        "name": HOST_NAME,
        "description": "Surge download manager",
        "path": host,
        "type": "stdio",
    });
    match family {
        Family::Chrome => {
            m["allowed_origins"] = json!([format!("chrome-extension://{CHROME_ID}/")])
        }
        Family::Firefox => m["allowed_extensions"] = json!([FIREFOX_ID]),
    }
    serde_json::to_string_pretty(&m).unwrap() + "\n"
}

/// Manifest files to write (Linux/macOS): (family, path, write even if the
/// browser's config dir doesn't exist yet).
#[cfg(not(windows))]
fn targets() -> Vec<(Family, PathBuf, bool)> {
    let Some(home) = dirs::home_dir() else {
        return vec![];
    };
    let file = format!("{HOST_NAME}.json");
    let mut out = vec![];
    if cfg!(target_os = "macos") {
        let base = home.join("Library/Application Support");
        for d in [
            "Google/Chrome",
            "Google/Chrome Beta",
            "Google/Chrome Canary",
            "Chromium",
            "BraveSoftware/Brave-Browser",
            "Microsoft Edge",
            "Vivaldi",
            "Arc/User Data",
        ] {
            out.push((
                Family::Chrome,
                base.join(d).join("NativeMessagingHosts").join(&file),
                false,
            ));
        }
        for (d, always) in [
            ("Mozilla", true),
            ("LibreWolf", false),
            ("Waterfox", false),
            ("zen", false),
        ] {
            out.push((
                Family::Firefox,
                base.join(d).join("NativeMessagingHosts").join(&file),
                always,
            ));
        }
    } else {
        let config = home.join(".config");
        for d in [
            "google-chrome",
            "google-chrome-beta",
            "google-chrome-unstable",
            "chromium",
            "BraveSoftware/Brave-Browser",
            "microsoft-edge",
            "vivaldi",
            "opera",
        ] {
            out.push((
                Family::Chrome,
                config.join(d).join("NativeMessagingHosts").join(&file),
                false,
            ));
        }
        // Zen and most forks read ~/.mozilla too.
        for (d, always) in [
            (".mozilla", true),
            (".librewolf", false),
            (".waterfox", false),
            (".floorp", false),
        ] {
            out.push((
                Family::Firefox,
                home.join(d).join("native-messaging-hosts").join(&file),
                always,
            ));
        }
    }
    out
}

/// Registry keys (under HKCU) that point browsers at a manifest.
#[cfg(windows)]
const REG_KEYS: [(Family, &str); 7] = [
    (Family::Chrome, r"Software\Google\Chrome"),
    (Family::Chrome, r"Software\Chromium"),
    (Family::Chrome, r"Software\BraveSoftware\Brave-Browser"),
    (Family::Chrome, r"Software\Microsoft\Edge"),
    (Family::Chrome, r"Software\Vivaldi"),
    (Family::Firefox, r"Software\Mozilla"),
    (Family::Firefox, r"Software\LibreWolf"),
];

#[cfg(windows)]
fn reg(args: &[&str]) -> bool {
    use std::os::windows::process::CommandExt;
    std::process::Command::new("reg")
        .args(args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .creation_flags(0x0800_0000)
        .status()
        .is_ok_and(|s| s.success())
}

#[cfg(windows)]
fn manifest_dir() -> PathBuf {
    crate::data_dir().join("native-messaging")
}

/// The `surge-host` next to the running executable.
pub fn sibling_host() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let host = exe
        .parent()?
        .join(format!("surge-host{}", std::env::consts::EXE_SUFFIX));
    host.is_file().then_some(host)
}

/// Write the manifests for `host`. Returns where they went.
pub fn register(host: &Path) -> Result<Vec<String>> {
    let host = host
        .canonicalize()
        .with_context(|| format!("{} not found", host.display()))?;
    let host = strip_verbatim(host);
    let mut done = vec![];

    #[cfg(not(windows))]
    for (family, path, always) in targets() {
        // …/<browser>/NativeMessagingHosts/<file>: only for installed browsers.
        let browser_dir = path.parent().and_then(Path::parent);
        if !always && !browser_dir.is_some_and(Path::is_dir) {
            continue;
        }
        write_if_changed(&path, &manifest(family, &host))?;
        done.push(path.display().to_string());
    }

    #[cfg(windows)]
    {
        let dir = manifest_dir();
        for family in [Family::Chrome, Family::Firefox] {
            let name = if family == Family::Chrome {
                "chrome.json"
            } else {
                "firefox.json"
            };
            let path = dir.join(name);
            write_if_changed(&path, &manifest(family, &host))?;
            for (_, key) in REG_KEYS.iter().filter(|(f, _)| *f == family) {
                let key = format!(r"HKCU\{key}\NativeMessagingHosts\{HOST_NAME}");
                if reg(&[
                    "add",
                    &key,
                    "/ve",
                    "/t",
                    "REG_SZ",
                    "/d",
                    &path.display().to_string(),
                    "/f",
                ]) {
                    done.push(key);
                }
            }
        }
    }
    Ok(done)
}

/// Remove what [`register`] wrote.
pub fn unregister() -> Vec<String> {
    let mut done = vec![];
    #[cfg(not(windows))]
    for (_, path, _) in targets() {
        if std::fs::remove_file(&path).is_ok() {
            done.push(path.display().to_string());
        }
    }
    #[cfg(windows)]
    {
        for (_, key) in REG_KEYS {
            let key = format!(r"HKCU\{key}\NativeMessagingHosts\{HOST_NAME}");
            if reg(&["delete", &key, "/f"]) {
                done.push(key);
            }
        }
        let _ = std::fs::remove_dir_all(manifest_dir());
    }
    done
}

/// Where the host is currently registered, if anywhere (the first manifest
/// found whose `path` still exists).
pub fn registered_host() -> Option<PathBuf> {
    #[cfg(not(windows))]
    let manifests: Vec<PathBuf> = targets().into_iter().map(|(_, p, _)| p).collect();
    #[cfg(windows)]
    let manifests: Vec<PathBuf> = ["chrome.json", "firefox.json"]
        .iter()
        .map(|f| manifest_dir().join(f))
        .collect();
    manifests.iter().find_map(|m| {
        let v: serde_json::Value = serde_json::from_slice(&std::fs::read(m).ok()?).ok()?;
        let p = PathBuf::from(v["path"].as_str()?);
        p.is_file().then_some(p)
    })
}

/// On app start: register the host shipped next to the app, unless a
/// working registration already exists (e.g. `install.sh`, or a dev build
/// shouldn't take over an installed one).
pub fn ensure_registered() {
    if registered_host().is_some() {
        return;
    }
    if let Some(host) = sibling_host()
        && let Err(e) = register(&host)
    {
        eprintln!("surge: registering the browser host: {e:#}");
    }
}

fn write_if_changed(path: &Path, text: &str) -> Result<()> {
    if std::fs::read_to_string(path).is_ok_and(|t| t == text) {
        return Ok(());
    }
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    }
    std::fs::write(path, text).with_context(|| format!("writing {}", path.display()))
}

/// `canonicalize` on Windows yields `\\?\C:\…`, which browsers don't accept.
fn strip_verbatim(p: PathBuf) -> PathBuf {
    match p.to_str().and_then(|s| s.strip_prefix(r"\\?\")) {
        Some(s) if !s.starts_with("UNC") => PathBuf::from(s),
        _ => p,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manifests() {
        let c: serde_json::Value =
            serde_json::from_str(&manifest(Family::Chrome, Path::new("/x/surge-host"))).unwrap();
        assert_eq!(
            c["allowed_origins"][0],
            format!("chrome-extension://{CHROME_ID}/")
        );
        assert_eq!(c["path"], "/x/surge-host");
        let f: serde_json::Value =
            serde_json::from_str(&manifest(Family::Firefox, Path::new("/x/surge-host"))).unwrap();
        assert_eq!(f["allowed_extensions"][0], FIREFOX_ID);
        assert!(f.get("allowed_origins").is_none());
        assert_eq!(
            strip_verbatim(PathBuf::from(r"\\?\C:\a\b.exe")),
            PathBuf::from(r"C:\a\b.exe")
        );
    }
}
