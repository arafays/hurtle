//! Hurtle as the system's app for `magnet:` links and `.torrent` files.
//!
//! Two levels, as every OS has them:
//! - **Known** (offered in "Open with" and the default-apps settings):
//!   done on app start by [`ensure_registered`], without taking anything over.
//!   - Linux: `hurtle.desktop` with `MimeType=x-scheme-handler/magnet;application/x-bittorrent;`.
//!   - Windows: `Hurtle.Magnet`/`Hurtle.Torrent` ProgIDs, `.torrent\OpenWithProgids`,
//!     and `Capabilities` under `RegisteredApplications` (HKCU only).
//!   - macOS: the bundle's Info.plist (`CFBundleURLTypes`, `CFBundleDocumentTypes`).
//! - **Default**: only when the user asks ([`make_default`]). Windows doesn't
//!   let apps set it, so that opens the Settings page for Hurtle instead.

#[cfg(not(any(windows, target_os = "macos")))]
use std::path::{Path, PathBuf};

pub const MAGNET_MIME: &str = "x-scheme-handler/magnet";
pub const TORRENT_MIME: &str = "application/x-bittorrent";

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Status {
    /// The system knows Hurtle opens these.
    pub known: bool,
    /// Hurtle opens magnet links by default.
    pub magnet: bool,
    /// Hurtle opens `.torrent` files by default.
    pub torrent: bool,
}

impl Status {
    pub fn is_default(&self) -> bool {
        self.magnet && self.torrent
    }
}

/// App start (release builds only, so a dev build never registers its
/// `target/` binary): make the system aware of Hurtle as a handler, where it
/// isn't already. Never changes the default.
pub fn ensure_registered() {
    if cfg!(debug_assertions) {
        return;
    }
    if let Err(e) = imp::ensure_registered(false) {
        crate::log::log("app", format!("registering for magnet links: {e}"));
    }
}

pub fn status() -> Status {
    imp::status()
}

/// Make Hurtle the default for both. Returns what happened, for the UI.
pub fn make_default() -> Result<String, String> {
    imp::make_default()
}

// ---------------------------------------------------------------- Linux & BSD

#[cfg(not(any(windows, target_os = "macos")))]
mod imp {
    use super::*;
    use std::process::{Command, Stdio};

    const DESKTOP: &str = "hurtle.desktop";

    fn data_home() -> Option<PathBuf> {
        std::env::var_os("XDG_DATA_HOME")
            .map(PathBuf::from)
            .filter(|p| p.is_absolute())
            .or_else(|| dirs::home_dir().map(|h| h.join(".local/share")))
    }

    fn data_dirs() -> Vec<PathBuf> {
        let mut out: Vec<PathBuf> = data_home().into_iter().collect();
        let sys = std::env::var("XDG_DATA_DIRS")
            .ok()
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "/usr/local/share:/usr/share".into());
        out.extend(
            sys.split(':')
                .map(PathBuf::from)
                .filter(|p| p.is_absolute()),
        );
        out
    }

    /// The `hurtle.desktop` the system uses: the first in the XDG data dirs.
    fn installed_entry() -> Option<PathBuf> {
        data_dirs()
            .into_iter()
            .map(|d| d.join("applications").join(DESKTOP))
            .find(|p| p.is_file())
    }

    fn declares_types(text: &str) -> bool {
        text.lines().any(|l| {
            l.starts_with("MimeType=") && l.contains(MAGNET_MIME) && l.contains(TORRENT_MIME)
        })
    }

    pub(super) fn entry_text(exe: &Path) -> String {
        format!(
            "[Desktop Entry]\nType=Application\nName=Hurtle\nGenericName=Download Manager\n\
             Comment=Multi-connection download accelerator\nExec={} %U\nIcon=hurtle\n\
             Categories=Network;FileTransfer;P2P;\nMimeType={MAGNET_MIME};{TORRENT_MIME};\n\
             StartupWMClass=hurtle\nTerminal=false\n",
            desktop_quote(&exe.to_string_lossy())
        )
    }

    /// Quote an Exec argument when it needs it (desktop entry spec).
    fn desktop_quote(s: &str) -> String {
        if s.chars().any(|c| " \t\"'\\$`<>~|&;*?#()".contains(c)) {
            let inner = s
                .replace('\\', "\\\\\\\\")
                .replace('"', "\\\\\"")
                .replace('`', "\\\\`")
                .replace('$', "\\\\$");
            format!("\"{inner}\"")
        } else {
            s.to_string()
        }
    }

    /// An entry that predates torrent support gets the MimeType line.
    pub(super) fn with_types(text: &str) -> String {
        let line = format!("MimeType={MAGNET_MIME};{TORRENT_MIME};");
        let mut out = String::new();
        let mut done = false;
        let mut in_main = false;
        for l in text.lines() {
            if l.starts_with('[') {
                if in_main && !done {
                    out.push_str(&line);
                    out.push('\n');
                    done = true;
                }
                in_main = l.trim() == "[Desktop Entry]";
            }
            if in_main && l.starts_with("MimeType=") {
                if !done {
                    out.push_str(&line);
                    out.push('\n');
                    done = true;
                }
                continue;
            }
            out.push_str(l);
            out.push('\n');
        }
        if !done {
            out.push_str(&line);
            out.push('\n');
        }
        out
    }

    /// `force`: write a user entry even when one can't be found (for
    /// "Make default"); otherwise only fill in what's missing.
    pub(super) fn ensure_registered(force: bool) -> Result<(), String> {
        let user = data_home()
            .ok_or("no home folder")?
            .join("applications")
            .join(DESKTOP);
        let text = match installed_entry() {
            Some(p) => std::fs::read_to_string(&p).map_err(|e| e.to_string())?,
            None => {
                let exe = std::env::current_exe().map_err(|e| e.to_string())?;
                if !force && cfg!(debug_assertions) {
                    return Ok(());
                }
                entry_text(&exe)
            }
        };
        if declares_types(&text) && installed_entry().is_some() {
            return Ok(());
        }
        // A system entry (a package) without the types is shadowed by a
        // user copy that has them; a user entry is updated in place.
        let new = if declares_types(&text) {
            text
        } else {
            with_types(&text)
        };
        if let Some(dir) = user.parent() {
            std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
        }
        std::fs::write(&user, new).map_err(|e| format!("writing {}: {e}", user.display()))?;
        // The cache "Open with" menus read.
        let _ = Command::new("update-desktop-database")
            .arg(user.parent().unwrap_or(Path::new(".")))
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
        Ok(())
    }

    fn xdg_mime(args: &[&str]) -> Option<String> {
        let out = Command::new("xdg-mime")
            .args(args)
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .output()
            .ok()?;
        out.status
            .success()
            .then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
    }

    fn config_home() -> Option<PathBuf> {
        std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .filter(|p| p.is_absolute())
            .or_else(dirs::config_dir)
    }

    /// The default for `mime` per `mimeapps.list`, when xdg-mime is missing.
    fn mimeapps_default(mime: &str) -> Option<String> {
        let text = std::fs::read_to_string(config_home()?.join("mimeapps.list")).ok()?;
        mimeapps_lookup(&text, mime)
    }

    pub(super) fn mimeapps_lookup(text: &str, mime: &str) -> Option<String> {
        let mut in_defaults = false;
        for l in text.lines() {
            let l = l.trim();
            if l.starts_with('[') {
                in_defaults = l == "[Default Applications]";
            } else if in_defaults
                && let Some((k, v)) = l.split_once('=')
                && k.trim() == mime
            {
                return v
                    .split(';')
                    .map(str::trim)
                    .find(|s| !s.is_empty())
                    .map(String::from);
            }
        }
        None
    }

    /// Set `mime`'s default in a `mimeapps.list` text.
    pub(super) fn mimeapps_set(text: &str, mime: &str, app: &str) -> String {
        let entry = format!("{mime}={app};");
        let mut out = Vec::new();
        let mut in_defaults = false;
        let mut seen_section = false;
        let mut done = false;
        for l in text.lines() {
            let t = l.trim();
            if t.starts_with('[') {
                if in_defaults && !done {
                    out.push(entry.clone());
                    done = true;
                }
                in_defaults = t == "[Default Applications]";
                seen_section |= in_defaults;
            } else if in_defaults && t.split_once('=').is_some_and(|(k, _)| k.trim() == mime) {
                if !done {
                    out.push(entry.clone());
                    done = true;
                }
                continue;
            }
            out.push(l.to_string());
        }
        if !done {
            if !seen_section {
                if out.last().is_some_and(|l| !l.is_empty()) {
                    out.push(String::new());
                }
                out.push("[Default Applications]".into());
            }
            out.push(entry);
        }
        out.join("\n") + "\n"
    }

    fn default_for(mime: &str) -> Option<String> {
        xdg_mime(&["query", "default", mime]).or_else(|| mimeapps_default(mime))
    }

    pub(super) fn status() -> Status {
        let known = installed_entry()
            .and_then(|p| std::fs::read_to_string(p).ok())
            .is_some_and(|t| declares_types(&t));
        let is_us = |m| default_for(m).is_some_and(|d| d == DESKTOP);
        Status {
            known,
            magnet: is_us(MAGNET_MIME),
            torrent: is_us(TORRENT_MIME),
        }
    }

    pub(super) fn make_default() -> Result<String, String> {
        ensure_registered(true)?;
        let via_xdg = xdg_mime(&["default", DESKTOP, MAGNET_MIME, TORRENT_MIME]).is_some();
        if !via_xdg {
            let path = config_home()
                .ok_or("no config folder")?
                .join("mimeapps.list");
            let mut text = std::fs::read_to_string(&path).unwrap_or_default();
            for m in [MAGNET_MIME, TORRENT_MIME] {
                text = mimeapps_set(&text, m, DESKTOP);
            }
            if let Some(dir) = path.parent() {
                std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
            }
            std::fs::write(&path, text).map_err(|e| format!("writing {}: {e}", path.display()))?;
        }
        if status().is_default() {
            Ok("Hurtle now opens magnet links and .torrent files".into())
        } else {
            Err("the desktop kept another default; choose Hurtle in your system's default-apps settings".into())
        }
    }
}

// -------------------------------------------------------------------- Windows

#[cfg(windows)]
mod imp {
    use super::*;
    use std::os::windows::process::CommandExt;
    use std::process::{Command, Stdio};

    const CLASSES: &str = r"HKCU\Software\Classes";
    const CAPS: &str = r"Software\Hurtle\Capabilities";
    const MAGNET_PROGID: &str = "Hurtle.Magnet";
    const TORRENT_PROGID: &str = "Hurtle.Torrent";

    fn reg(args: &[&str]) -> Option<String> {
        let out = Command::new("reg")
            .args(args)
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .creation_flags(0x0800_0000) // CREATE_NO_WINDOW
            .output()
            .ok()?;
        out.status
            .success()
            .then(|| String::from_utf8_lossy(&out.stdout).into_owned())
    }

    fn set(key: &str, name: Option<&str>, value: &str) -> Result<(), String> {
        let mut args = vec!["add", key];
        match name {
            Some(n) => args.extend(["/v", n]),
            None => args.push("/ve"),
        }
        args.extend(["/t", "REG_SZ", "/d", value, "/f"]);
        reg(&args)
            .map(|_| ())
            .ok_or_else(|| format!("couldn't write {key}"))
    }

    /// A value from `reg query` output (`    Name    REG_SZ    data`).
    pub(super) fn query_value(out: &str) -> Option<String> {
        out.lines()
            .find_map(|l| l.split_once("REG_SZ").map(|(_, v)| v.trim().to_string()))
    }

    fn get(key: &str, name: Option<&str>) -> Option<String> {
        let mut args = vec!["query", key];
        match name {
            Some(n) => args.extend(["/v", n]),
            None => args.push("/ve"),
        }
        query_value(&reg(&args)?)
    }

    fn command_for(exe: &str) -> String {
        format!("\"{exe}\" \"%1\"")
    }

    fn exe() -> Result<String, String> {
        let p = std::env::current_exe().map_err(|e| e.to_string())?;
        let s = p.to_string_lossy();
        Ok(s.strip_prefix(r"\\?\").unwrap_or(&s).to_string())
    }

    fn register(exe: &str) -> Result<(), String> {
        let cmd = command_for(exe);
        let icon = format!("\"{exe}\",0");
        let magnet = format!(r"{CLASSES}\{MAGNET_PROGID}");
        set(&magnet, None, "URL:Magnet link")?;
        set(&magnet, Some("URL Protocol"), "")?;
        set(&format!(r"{magnet}\DefaultIcon"), None, &icon)?;
        set(&format!(r"{magnet}\shell\open\command"), None, &cmd)?;
        let torrent = format!(r"{CLASSES}\{TORRENT_PROGID}");
        set(&torrent, None, "BitTorrent file")?;
        set(&format!(r"{torrent}\DefaultIcon"), None, &icon)?;
        set(&format!(r"{torrent}\shell\open\command"), None, &cmd)?;
        // Offered in "Open with" for .torrent; with no other handler it's used.
        set(
            &format!(r"{CLASSES}\.torrent\OpenWithProgids"),
            Some(TORRENT_PROGID),
            "",
        )?;
        // A protocol nobody handles yet: Hurtle, without overriding anyone.
        if get(&format!(r"{CLASSES}\magnet\shell\open\command"), None).is_none()
            && get(r"HKLM\Software\Classes\magnet\shell\open\command", None).is_none()
        {
            let m = format!(r"{CLASSES}\magnet");
            set(&m, None, "URL:Magnet link")?;
            set(&m, Some("URL Protocol"), "")?;
            set(&format!(r"{m}\shell\open\command"), None, &cmd)?;
        }
        // Settings → Default apps → Hurtle.
        let caps = format!(r"HKCU\{CAPS}");
        set(&caps, Some("ApplicationName"), "Hurtle")?;
        set(
            &caps,
            Some("ApplicationDescription"),
            "Download accelerator with BitTorrent support",
        )?;
        set(
            &format!(r"{caps}\FileAssociations"),
            Some(".torrent"),
            TORRENT_PROGID,
        )?;
        set(
            &format!(r"{caps}\URLAssociations"),
            Some("magnet"),
            MAGNET_PROGID,
        )?;
        set(
            r"HKCU\Software\RegisteredApplications",
            Some("Hurtle"),
            CAPS,
        )?;
        Ok(())
    }

    pub(super) fn ensure_registered(force: bool) -> Result<(), String> {
        let exe = exe()?;
        let current = get(
            &format!(r"{CLASSES}\{MAGNET_PROGID}\shell\open\command"),
            None,
        );
        // Re-register when missing or pointing at a program that's gone
        // (moved install); a working one from another copy is left alone.
        let stale = current.as_deref().is_none_or(|c| {
            let path = c.trim_start_matches('"').split('"').next().unwrap_or("");
            !std::path::Path::new(path).is_file()
        });
        if force || stale {
            register(&exe)?;
        }
        Ok(())
    }

    fn user_choice(key: &str) -> Option<String> {
        get(
            &format!(r"HKCU\Software\Microsoft\Windows\{key}\UserChoice"),
            Some("ProgId"),
        )
    }

    pub(super) fn status() -> Status {
        let known = get(
            &format!(r"{CLASSES}\{MAGNET_PROGID}\shell\open\command"),
            None,
        )
        .is_some();
        let magnet = match user_choice(r"Shell\Associations\UrlAssociations\magnet") {
            Some(p) => p == MAGNET_PROGID,
            // No choice made: whatever `magnet` itself runs.
            None => get(&format!(r"{CLASSES}\magnet\shell\open\command"), None)
                .is_some_and(|c| c.to_ascii_lowercase().contains("hurtle")),
        };
        let torrent = match user_choice(r"CurrentVersion\Explorer\FileExts\.torrent") {
            Some(p) => p == TORRENT_PROGID,
            None => get(&format!(r"{CLASSES}\.torrent"), None).is_some_and(|p| p == TORRENT_PROGID),
        };
        Status {
            known,
            magnet,
            torrent,
        }
    }

    pub(super) fn make_default() -> Result<String, String> {
        ensure_registered(true)?;
        // Windows only lets the user pick defaults: open Hurtle's page there.
        crate::player::open_with_system("ms-settings:defaultapps?registeredAppUser=Hurtle")?;
        Ok("In Settings, choose Hurtle for .torrent and MAGNET".into())
    }
}

// ---------------------------------------------------------------------- macOS

#[cfg(target_os = "macos")]
mod imp {
    use super::*;
    use core_foundation::base::TCFType;
    use core_foundation::string::{CFString, CFStringRef};

    const BUNDLE_ID: &str = "io.github.arafays.hurtle";
    /// The UTI the bundle's Info.plist imports for `.torrent`.
    const TORRENT_UTI: &str = "org.bittorrent.torrent";
    const ROLES_ALL: u32 = 0xFFFF_FFFF;

    #[link(name = "CoreServices", kind = "framework")]
    unsafe extern "C" {
        fn LSSetDefaultHandlerForURLScheme(scheme: CFStringRef, bundle: CFStringRef) -> i32;
        fn LSSetDefaultRoleHandlerForContentType(
            uti: CFStringRef,
            role: u32,
            bundle: CFStringRef,
        ) -> i32;
        fn LSCopyDefaultHandlerForURLScheme(scheme: CFStringRef) -> CFStringRef;
        fn LSCopyDefaultRoleHandlerForContentType(uti: CFStringRef, role: u32) -> CFStringRef;
    }

    fn owned(r: CFStringRef) -> Option<String> {
        // SAFETY: the LSCopy* functions return a +1 reference or null.
        (!r.is_null()).then(|| unsafe { CFString::wrap_under_create_rule(r) }.to_string())
    }

    fn in_bundle() -> bool {
        std::env::current_exe().is_ok_and(|p| p.to_string_lossy().contains(".app/Contents/MacOS/"))
    }

    pub(super) fn ensure_registered(_force: bool) -> Result<(), String> {
        // The Info.plist does it; Launch Services reads it when the app is opened.
        Ok(())
    }

    pub(super) fn status() -> Status {
        let scheme = CFString::new("magnet");
        let uti = CFString::new(TORRENT_UTI);
        // SAFETY: valid CFStrings for the duration of the calls.
        let magnet =
            owned(unsafe { LSCopyDefaultHandlerForURLScheme(scheme.as_concrete_TypeRef()) });
        let torrent = owned(unsafe {
            LSCopyDefaultRoleHandlerForContentType(uti.as_concrete_TypeRef(), ROLES_ALL)
        });
        let us = |h: Option<String>| h.is_some_and(|h| h.eq_ignore_ascii_case(BUNDLE_ID));
        Status {
            known: in_bundle(),
            magnet: us(magnet),
            torrent: us(torrent),
        }
    }

    pub(super) fn make_default() -> Result<String, String> {
        if !in_bundle() {
            return Err(
                "only Hurtle.app can be the default (this copy isn't in an app bundle)".into(),
            );
        }
        let id = CFString::new(BUNDLE_ID);
        let scheme = CFString::new("magnet");
        let uti = CFString::new(TORRENT_UTI);
        // SAFETY: valid CFStrings for the duration of the calls.
        let a = unsafe {
            LSSetDefaultHandlerForURLScheme(scheme.as_concrete_TypeRef(), id.as_concrete_TypeRef())
        };
        let b = unsafe {
            LSSetDefaultRoleHandlerForContentType(
                uti.as_concrete_TypeRef(),
                ROLES_ALL,
                id.as_concrete_TypeRef(),
            )
        };
        if a == 0 && b == 0 {
            Ok("Hurtle now opens magnet links and .torrent files".into())
        } else {
            Err(format!("Launch Services refused (errors {a}, {b})"))
        }
    }
}

#[cfg(all(test, not(any(windows, target_os = "macos"))))]
mod tests {
    use super::imp::*;
    use super::*;

    #[test]
    fn adds_mime_types_to_old_entries() {
        let old = "[Desktop Entry]\nName=Hurtle\nExec=hurtle %U\n\n[Desktop Action x]\nName=X\n";
        let new = with_types(old);
        let at = new.find("MimeType=").unwrap();
        assert!(
            at < new.find("[Desktop Action x]").unwrap()
                && new.contains(&format!("MimeType={MAGNET_MIME};{TORRENT_MIME};\n"))
        );
        let replaced = with_types("[Desktop Entry]\nMimeType=text/plain;\nName=H\n");
        assert_eq!(replaced.matches("MimeType=").count(), 1);
        assert!(!replaced.contains("text/plain"));
    }

    #[test]
    fn mimeapps() {
        let t = "[Added Associations]\nx-scheme-handler/magnet=qbittorrent.desktop;\n\n[Default Applications]\ntext/html=firefox.desktop\nx-scheme-handler/magnet=qbittorrent.desktop;\n";
        assert_eq!(
            mimeapps_lookup(t, MAGNET_MIME).as_deref(),
            Some("qbittorrent.desktop")
        );
        let t2 = mimeapps_set(t, MAGNET_MIME, "hurtle.desktop");
        assert_eq!(
            mimeapps_lookup(&t2, MAGNET_MIME).as_deref(),
            Some("hurtle.desktop")
        );
        assert!(t2.contains("[Added Associations]\nx-scheme-handler/magnet=qbittorrent.desktop;"));
        assert!(t2.contains("text/html=firefox.desktop"));
        let t3 = mimeapps_set("", TORRENT_MIME, "hurtle.desktop");
        assert_eq!(
            t3,
            "[Default Applications]\napplication/x-bittorrent=hurtle.desktop;\n"
        );
        let t4 = mimeapps_set(
            "[Default Applications]\na=b\n[Other]\n",
            TORRENT_MIME,
            "hurtle.desktop",
        );
        assert!(t4.starts_with(
            "[Default Applications]\na=b\napplication/x-bittorrent=hurtle.desktop;\n[Other]"
        ));
    }

    #[test]
    fn entry_quotes_exec() {
        let e = entry_text(Path::new("/opt/My Apps/hurtle"));
        assert!(e.contains("Exec=\"/opt/My Apps/hurtle\" %U"));
        assert!(e.contains("MimeType=x-scheme-handler/magnet;application/x-bittorrent;"));
    }
}
