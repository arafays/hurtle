//! Theming: follow the desktop's palette, not just its light/dark switch.
//!
//! Theme generators (Omarchy, Noctalia, pywal, wallust, Stylix, matugen via
//! GTK, KDE, …) all write their colours to files. We read those files, turn
//! them into one [`Palette`], derive every role the UI needs (hover, muted,
//! border, readable text on the accent…) and paint gpui-kit's global `Theme`.
//! A poll of the files' mtimes re-applies it when the generator rewrites them.
//!
//! Everything up to [`apply_scheme`] is pure (strings and colours in, colours
//! out) so it can be unit tested. The file layout knowledge lives in
//! [`detect`]; all of it is Linux/BSD paths and simply finds nothing elsewhere,
//! where the OS light/dark preference is used instead.

use std::collections::{HashMap, hash_map::DefaultHasher};
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};

use gpui_kit::component::{ActiveTheme, Theme, ThemeColor, ThemeMode};
use gpui_kit::{App, Global, Hsla, Rgba, Window};

// ---------------------------------------------------------------- colours --

/// sRGB, each channel 0..=1.
pub type Rgb = [f32; 3];

const BLACK: Rgb = [0.0, 0.0, 0.0];
const WHITE: Rgb = [1.0, 1.0, 1.0];

fn hex(v: u32) -> Rgb {
    [
        ((v >> 16) & 0xff) as f32 / 255.0,
        ((v >> 8) & 0xff) as f32 / 255.0,
        (v & 0xff) as f32 / 255.0,
    ]
}

/// `#rrggbb`, `rrggbb`, `0xrrggbb`, `#rgb`, `#rrggbbaa` (alpha dropped),
/// `rgb(r, g, b)` and KDE's `r,g,b`.
pub fn parse_color(s: &str) -> Option<Rgb> {
    let s = s.trim().trim_end_matches(';').trim();
    let s = s.trim_matches(|c| c == '"' || c == '\'');
    if s.is_empty() {
        return None;
    }
    let lower = s.to_ascii_lowercase();
    let inner = lower
        .strip_prefix("rgba(")
        .or_else(|| lower.strip_prefix("rgb("))
        .map(|r| r.trim_end_matches(')'));
    if inner.is_some() || lower.contains(',') {
        let parts: Vec<f32> = inner
            .unwrap_or(&lower)
            .split(',')
            .take(3)
            .filter_map(|p| p.trim().parse::<f32>().ok())
            .collect();
        return (parts.len() == 3).then(|| [parts[0] / 255.0, parts[1] / 255.0, parts[2] / 255.0]);
    }
    let h = lower
        .strip_prefix('#')
        .or_else(|| lower.strip_prefix("0x"))
        .unwrap_or(&lower);
    if !h.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    match h.len() {
        3 => {
            let v = u32::from_str_radix(h, 16).ok()?;
            let (r, g, b) = ((v >> 8) & 0xf, (v >> 4) & 0xf, v & 0xf);
            Some(hex((r * 0x11) << 16 | (g * 0x11) << 8 | (b * 0x11)))
        }
        6 => Some(hex(u32::from_str_radix(h, 16).ok()?)),
        8 => Some(hex(u32::from_str_radix(&h[..6], 16).ok()?)),
        _ => None,
    }
}

fn lin(c: f32) -> f32 {
    if c <= 0.04045 {
        c / 12.92
    } else {
        ((c + 0.055) / 1.055).powf(2.4)
    }
}

/// WCAG relative luminance.
pub fn luminance(c: Rgb) -> f32 {
    0.2126 * lin(c[0]) + 0.7152 * lin(c[1]) + 0.0722 * lin(c[2])
}

/// WCAG contrast ratio, 1..=21.
pub fn contrast(a: Rgb, b: Rgb) -> f32 {
    let (la, lb) = (luminance(a), luminance(b));
    (la.max(lb) + 0.05) / (la.min(lb) + 0.05)
}

/// `a` moved `t` of the way to `b`.
pub fn mix(a: Rgb, b: Rgb, t: f32) -> Rgb {
    [
        a[0] + (b[0] - a[0]) * t,
        a[1] + (b[1] - a[1]) * t,
        a[2] + (b[2] - a[2]) * t,
    ]
}

fn saturation(c: Rgb) -> f32 {
    let max = c[0].max(c[1]).max(c[2]);
    let min = c[0].min(c[1]).min(c[2]);
    if max <= 0.0 { 0.0 } else { (max - min) / max }
}

fn is_dark(bg: Rgb) -> bool {
    contrast(bg, WHITE) > contrast(bg, BLACK)
}

/// `c`, pushed away from `bg` (towards white on dark, black on light) until it
/// reaches `min` contrast.
pub fn ensure_contrast(c: Rgb, bg: Rgb, min: f32) -> Rgb {
    if contrast(c, bg) >= min {
        return c;
    }
    let target = if is_dark(bg) { WHITE } else { BLACK };
    for i in 1..=20 {
        let m = mix(c, target, i as f32 * 0.05);
        if contrast(m, bg) >= min {
            return m;
        }
    }
    target
}

/// Readable text on `bg`: the first candidate with enough contrast, else the
/// best one.
fn text_on(bg: Rgb, candidates: &[Rgb]) -> Rgb {
    candidates
        .iter()
        .copied()
        .find(|&c| contrast(c, bg) >= 4.5)
        .unwrap_or_else(|| {
            candidates
                .iter()
                .copied()
                .max_by(|&a, &b| contrast(a, bg).total_cmp(&contrast(b, bg)))
                .unwrap_or(WHITE)
        })
}

/// A hover/pressed variant: lighter on dark colours, darker on light ones.
fn shade(c: Rgb, t: f32) -> Rgb {
    if luminance(c) > 0.5 {
        mix(c, BLACK, t)
    } else {
        mix(c, WHITE, t)
    }
}

// ---------------------------------------------------------------- palette --

/// What a theme generator tells us. Only background, foreground and accent are
/// required; anything missing is derived.
#[derive(Clone, Debug, PartialEq)]
pub struct Palette {
    pub bg: Rgb,
    pub fg: Rgb,
    pub accent: Rgb,
    pub red: Option<Rgb>,
    pub green: Option<Rgb>,
    pub yellow: Option<Rgb>,
    pub blue: Option<Rgb>,
    /// A slightly raised surface (cards, popovers).
    pub surface: Option<Rgb>,
    /// Secondary text.
    pub muted: Option<Rgb>,
    pub border: Option<Rgb>,
}

impl Palette {
    fn new(bg: Rgb, fg: Rgb, accent: Rgb) -> Self {
        Self {
            bg,
            fg,
            accent,
            red: None,
            green: None,
            yellow: None,
            blue: None,
            surface: None,
            muted: None,
            border: None,
        }
    }
}

/// Every colour role the UI paints with, already contrast-checked.
#[derive(Clone, Debug, PartialEq)]
pub struct Scheme {
    pub dark: bool,
    pub bg: Rgb,
    pub fg: Rgb,
    pub muted_fg: Rgb,
    pub surface: Rgb,
    pub muted: Rgb,
    pub hover: Rgb,
    pub active: Rgb,
    pub border: Rgb,
    pub primary: Rgb,
    pub primary_fg: Rgb,
    pub success: Rgb,
    pub warning: Rgb,
    pub danger: Rgb,
    pub info: Rgb,
}

impl Scheme {
    fn on(&self, c: Rgb) -> Rgb {
        text_on(c, &[self.bg, self.fg, BLACK, WHITE])
    }
}

/// Fill in the roles a palette doesn't give and make text readable.
pub fn derive(p: &Palette) -> Scheme {
    let dark = is_dark(p.bg);
    let bg = p.bg;
    let fg = ensure_contrast(p.fg, bg, 4.5);
    let muted_fg = ensure_contrast(p.muted.unwrap_or_else(|| mix(bg, fg, 0.62)), bg, 4.5);
    let surface = p
        .surface
        .filter(|&s| contrast(s, bg) > 1.04)
        .unwrap_or_else(|| mix(bg, fg, 0.06));
    let border = p
        .border
        .filter(|&b| contrast(b, bg) > 1.15)
        .unwrap_or_else(|| mix(bg, fg, 0.18));
    let primary = ensure_contrast(p.accent, bg, 3.0);
    let (d_green, d_yellow, d_red, d_blue) = if dark {
        (0x4ade80, 0xfacc15, 0xf87171, 0x60a5fa)
    } else {
        (0x16a34a, 0xca8a04, 0xdc2626, 0x2563eb)
    };
    let status = |c: Option<Rgb>, d: u32| ensure_contrast(c.unwrap_or_else(|| hex(d)), bg, 3.0);
    let s = Scheme {
        dark,
        bg,
        fg,
        muted_fg,
        surface,
        muted: mix(bg, fg, 0.12),
        hover: mix(bg, fg, 0.08),
        active: mix(bg, fg, 0.14),
        border,
        primary,
        primary_fg: [0.0; 3],
        success: status(p.green, d_green),
        warning: status(p.yellow, d_yellow),
        danger: status(p.red, d_red),
        info: status(p.blue, d_blue),
    };
    Scheme {
        primary_fg: s.on(primary),
        ..s
    }
}

/// The accent among the 16 terminal colours: vivid, and visible on the
/// background. Prefers blue/magenta/cyan on ties.
fn best_accent(ansi: &[Option<Rgb>; 16], bg: Rgb) -> Option<Rgb> {
    const ORDER: [usize; 12] = [4, 5, 6, 2, 3, 1, 12, 13, 14, 10, 11, 9];
    let mut best: Option<(f32, Rgb)> = None;
    for (rank, &i) in ORDER.iter().enumerate() {
        let Some(c) = ansi[i] else { continue };
        let score = saturation(c) * (contrast(c, bg) / 3.0).min(1.0) - rank as f32 * 0.01;
        if best.is_none_or(|(b, _)| score > b) {
            best = Some((score, c));
        }
    }
    best.map(|(_, c)| c)
}

// ----------------------------------------------------------------- parsing --

/// Flatten a theme file into `key -> raw value`. Handles JSON (nested; leaf
/// names), TOML/INI (`section.key`), and `key: value` YAML.
fn scan(text: &str) -> HashMap<String, String> {
    let mut out = HashMap::new();
    let t = text.trim_start();
    if t.starts_with('{')
        && let Ok(v) = serde_json::from_str::<serde_json::Value>(t)
    {
        fn walk(v: &serde_json::Value, key: &str, out: &mut HashMap<String, String>) {
            match v {
                serde_json::Value::Object(m) => m.iter().for_each(|(k, v)| walk(v, k, out)),
                serde_json::Value::String(s) => {
                    out.entry(key.to_string()).or_insert_with(|| s.clone());
                }
                _ => {}
            }
        }
        walk(&v, "", &mut out);
        return out;
    }
    let mut section = String::new();
    for line in text.lines() {
        let l = line.trim();
        if l.is_empty() || l.starts_with('#') || l.starts_with(';') || l.starts_with("//") {
            continue;
        }
        if l.starts_with('[') && l.ends_with(']') && !l.contains('"') && !l.contains(',') {
            section = l[1..l.len() - 1].trim().to_string();
            continue;
        }
        let Some(i) = l.find(['=', ':']) else {
            continue;
        };
        let key = l[..i].trim().trim_matches(|c| c == '"' || c == '\'');
        let rest = l[i + 1..].trim();
        let value = match rest.chars().next() {
            Some(q @ ('"' | '\'')) => rest[1..].split(q).next().unwrap_or("").to_string(),
            _ => rest
                .split(" #")
                .next()
                .unwrap_or("")
                .trim()
                .trim_end_matches(',')
                .to_string(),
        };
        let key = if section.is_empty() {
            key.to_string()
        } else {
            format!("{section}.{key}")
        };
        out.insert(key, value);
    }
    out
}

fn get(kv: &HashMap<String, String>, key: &str) -> Option<Rgb> {
    kv.get(key).and_then(|v| parse_color(v))
}

/// The ANSI palette in `color0..color15` form, or from alacritty's
/// `colors.normal.red`… tables.
fn ansi_from(kv: &HashMap<String, String>) -> [Option<Rgb>; 16] {
    let mut ansi = [None; 16];
    for (i, slot) in ansi.iter_mut().enumerate() {
        *slot = get(kv, &format!("color{i}"));
    }
    if ansi.iter().all(Option::is_none) {
        const NAMES: [&str; 8] = [
            "black", "red", "green", "yellow", "blue", "magenta", "cyan", "white",
        ];
        for (i, n) in NAMES.iter().enumerate() {
            ansi[i] = get(kv, &format!("colors.normal.{n}"));
            ansi[i + 8] = get(kv, &format!("colors.bright.{n}"));
        }
    }
    if ansi.iter().all(Option::is_none) {
        // Omarchy's colors.toml: bare `red = …`, `bright_red = …` keys.
        const NAMES: [&str; 8] = [
            "black", "red", "green", "yellow", "blue", "magenta", "cyan", "white",
        ];
        for (i, n) in NAMES.iter().enumerate() {
            ansi[i] = get(kv, n);
            ansi[i + 8] = get(kv, &format!("bright_{n}"));
        }
    }
    ansi
}

fn from_ansi(
    bg: Rgb,
    fg: Rgb,
    ansi: [Option<Rgb>; 16],
    accent: Option<Rgb>,
    muted: Option<Rgb>,
) -> Palette {
    let accent = accent
        .or_else(|| best_accent(&ansi, bg))
        .unwrap_or_else(|| hex(0x60a5fa));
    Palette {
        red: ansi[1],
        green: ansi[2],
        yellow: ansi[3],
        blue: ansi[4],
        muted,
        ..Palette::new(bg, fg, accent)
    }
}

/// `@define-color name value;` lines, with `@other` references resolved.
fn parse_gtk(text: &str) -> HashMap<String, Rgb> {
    let mut raw: HashMap<String, String> = HashMap::new();
    for line in text.lines() {
        if let Some(rest) = line.trim().strip_prefix("@define-color")
            && let Some((name, value)) = rest.trim().split_once(char::is_whitespace)
        {
            raw.insert(name.to_string(), value.trim().trim_end_matches(';').into());
        }
    }
    let mut out = HashMap::new();
    for name in raw.keys() {
        let mut v = raw[name].clone();
        for _ in 0..6 {
            match v.strip_prefix('@').and_then(|r| raw.get(r.trim())) {
                Some(next) => v = next.clone(),
                None => break,
            }
        }
        if let Some(c) = parse_color(&v) {
            out.insert(name.clone(), c);
        }
    }
    out
}

fn palette_from_gtk(text: &str) -> Option<Palette> {
    let c = parse_gtk(text);
    let first = |names: &[&str]| names.iter().find_map(|n| c.get(*n).copied());
    let bg = first(&["window_bg_color", "theme_bg_color", "view_bg_color"])?;
    let fg = first(&["window_fg_color", "theme_fg_color", "view_fg_color"])?;
    let accent = first(&["accent_bg_color", "accent_color", "theme_selected_bg_color"])?;
    let surface = ["card_bg_color", "headerbar_bg_color", "sidebar_bg_color"]
        .iter()
        .filter_map(|n| c.get(*n).copied())
        .find(|&s| contrast(s, bg) > 1.04);
    Some(Palette {
        red: first(&["error_bg_color", "destructive_bg_color", "error_color"]),
        green: first(&["success_bg_color", "success_color"]),
        yellow: first(&["warning_bg_color", "warning_color"]),
        surface,
        ..Palette::new(bg, fg, accent)
    })
}

/// Recognise a palette file by its content. Used for every source and for
/// `SURGE_THEME=<path>`.
pub fn palette_from_text(text: &str) -> Option<Palette> {
    if text.contains("@define-color") {
        return palette_from_gtk(text);
    }
    let kv = scan(text);
    // Noctalia (Material-style roles).
    if let (Some(bg), Some(fg)) = (get(&kv, "mSurface"), get(&kv, "mOnSurface")) {
        let accent = get(&kv, "mPrimary").unwrap_or_else(|| hex(0x60a5fa));
        return Some(Palette {
            red: get(&kv, "mError"),
            surface: get(&kv, "mSurfaceVariant"),
            muted: get(&kv, "mOnSurfaceVariant"),
            border: get(&kv, "mOutline"),
            ..Palette::new(bg, fg, accent)
        });
    }
    // KDE colour scheme.
    if let (Some(bg), Some(fg)) = (
        get(&kv, "Colors:Window.BackgroundNormal"),
        get(&kv, "Colors:Window.ForegroundNormal"),
    ) {
        let accent = get(&kv, "Colors:Window.DecorationFocus")
            .or_else(|| get(&kv, "Colors:Selection.BackgroundNormal"))
            .unwrap_or_else(|| hex(0x60a5fa));
        return Some(Palette {
            red: get(&kv, "Colors:Window.ForegroundNegative"),
            green: get(&kv, "Colors:Window.ForegroundPositive"),
            yellow: get(&kv, "Colors:Window.ForegroundNeutral"),
            surface: get(&kv, "Colors:Button.BackgroundNormal"),
            muted: get(&kv, "Colors:Window.ForegroundInactive"),
            ..Palette::new(bg, fg, accent)
        });
    }
    // Base16 / tinted-theming / Stylix.
    let b = |n: u8| get(&kv, &format!("base{n:02X}")).or_else(|| get(&kv, &format!("base{n:02x}")));
    if let (Some(bg), Some(fg)) = (b(0), b(5)) {
        return Some(Palette {
            red: b(8),
            yellow: b(0xA),
            green: b(0xB),
            blue: b(0xD),
            surface: b(1),
            muted: b(4),
            border: b(2),
            ..Palette::new(bg, fg, b(0xD).or(b(0xC)).unwrap_or_else(|| hex(0x60a5fa)))
        });
    }
    // Terminal-style: Omarchy colors.toml, pywal/wallust colors.json, alacritty.toml.
    let (bg, fg) = match (get(&kv, "background"), get(&kv, "foreground")) {
        (Some(b), Some(f)) => (b, f),
        _ => (
            get(&kv, "colors.primary.background")?,
            get(&kv, "colors.primary.foreground")?,
        ),
    };
    Some(from_ansi(
        bg,
        fg,
        ansi_from(&kv),
        get(&kv, "accent"),
        get(&kv, "muted"),
    ))
}

// ------------------------------------------------------------- detection --

/// The directories theme generators write to.
#[derive(Clone, Debug)]
pub struct Dirs {
    pub config: PathBuf,
    pub cache: PathBuf,
    pub kde: bool,
}

impl Dirs {
    pub fn from_env() -> Self {
        let var = |k: &str| {
            std::env::var_os(k)
                .map(PathBuf::from)
                .filter(|p| p.is_absolute())
        };
        let home = var("HOME")
            .or_else(dirs::home_dir)
            .unwrap_or_else(|| PathBuf::from("/"));
        Self {
            config: var("XDG_CONFIG_HOME").unwrap_or_else(|| home.join(".config")),
            cache: var("XDG_CACHE_HOME").unwrap_or_else(|| home.join(".cache")),
            kde: std::env::var("XDG_CURRENT_DESKTOP")
                .is_ok_and(|d| d.to_uppercase().contains("KDE")),
        }
    }

    fn omarchy(&self) -> PathBuf {
        self.config.join("omarchy/current")
    }

    /// `pywal`-format files from any generator that writes one.
    fn wal_files(&self) -> Vec<(&'static str, PathBuf)> {
        vec![
            ("Pywal", self.cache.join("wal/colors.json")),
            ("Wallust", self.cache.join("wallust/colors.json")),
            ("Hellwal", self.cache.join("hellwal/colors.json")),
        ]
    }

    fn gtk_dirs(&self) -> [PathBuf; 2] {
        [self.config.join("gtk-4.0"), self.config.join("gtk-3.0")]
    }
}

/// A palette found on the system, and where it came from.
#[derive(Clone, Debug, PartialEq)]
pub struct Detected {
    pub source: &'static str,
    pub name: Option<String>,
    pub palette: Palette,
}

impl Detected {
    pub fn label(&self) -> String {
        match &self.name {
            Some(n) => format!("{} ({n})", self.source),
            None => self.source.to_string(),
        }
    }
}

fn read(p: &Path) -> Option<String> {
    std::fs::read_to_string(p).ok()
}

fn mtime(p: &Path) -> std::time::SystemTime {
    std::fs::metadata(p)
        .and_then(|m| m.modified())
        .unwrap_or(std::time::UNIX_EPOCH)
}

fn omarchy(d: &Dirs) -> Option<Detected> {
    let cur = d.omarchy();
    let theme = cur.join("theme");
    let palette = read(&theme.join("colors.toml"))
        .and_then(|t| palette_from_text(&t))
        .or_else(|| read(&theme.join("alacritty.toml")).and_then(|t| palette_from_text(&t)))?;
    let name = read(&cur.join("theme.name"))
        .map(|n| n.trim().to_string())
        .filter(|n| !n.is_empty())
        .or_else(|| {
            let p = std::fs::read_link(&theme)
                .ok()
                .or_else(|| std::fs::canonicalize(&theme).ok())?;
            Some(p.file_name()?.to_string_lossy().into_owned())
        });
    Some(Detected {
        source: "Omarchy",
        name,
        palette,
    })
}

fn noctalia_json(d: &Dirs) -> Option<Detected> {
    [
        d.config.join("noctalia/colors.json"),
        d.cache.join("noctalia/colors.json"),
    ]
    .iter()
    .find_map(|p| read(p).and_then(|t| palette_from_text(&t)))
    .map(|palette| Detected {
        source: "Noctalia",
        name: None,
        palette,
    })
}

/// pywal and friends: the most recently written file wins.
fn wal(d: &Dirs) -> Option<Detected> {
    let mut files = d.wal_files();
    files.retain(|(_, p)| p.is_file());
    files.sort_by_key(|(_, p)| std::cmp::Reverse(mtime(p)));
    files.iter().find_map(|(source, p)| {
        read(p)
            .and_then(|t| palette_from_text(&t))
            .map(|palette| Detected {
                source,
                name: None,
                palette,
            })
    })
}

fn stylix(d: &Dirs) -> Option<Detected> {
    let p = d.config.join("stylix/palette.json");
    palette_from_text(&read(&p)?).map(|palette| Detected {
        source: "Stylix",
        name: None,
        palette,
    })
}

/// `@import`ed file names in a GTK stylesheet.
fn css_imports(css: &str) -> Vec<String> {
    css.lines()
        .filter_map(|l| l.trim().strip_prefix("@import"))
        .map(|r| {
            let r = r.trim().trim_end_matches(';').trim();
            let r = r.strip_prefix("url(").unwrap_or(r).trim_end_matches(')');
            r.trim_matches(|c| c == '"' || c == '\'').to_string()
        })
        .filter(|f| !f.is_empty() && !f.starts_with("resource:") && !f.contains("://"))
        .collect()
}

fn gtk(d: &Dirs) -> Option<Detected> {
    for dir in d.gtk_dirs() {
        let Some(mut css) = read(&dir.join("gtk.css")) else {
            continue;
        };
        let mut generator = None;
        for f in css_imports(&css.clone()) {
            let lower = f.to_lowercase();
            for (needle, name) in [
                ("noctalia", "Noctalia"),
                ("matugen", "Matugen"),
                ("wallust", "Wallust"),
                ("pywal", "Pywal"),
                ("stylix", "Stylix"),
                ("caelestia", "Caelestia"),
                ("dank", "DankMaterialShell"),
            ] {
                if lower.contains(needle) {
                    generator = Some(name);
                }
            }
            if let Some(extra) = read(&dir.join(&f)) {
                css.push('\n');
                css.push_str(&extra);
            }
        }
        if let Some(palette) = palette_from_gtk(&css) {
            return Some(Detected {
                source: generator.unwrap_or("GTK"),
                name: None,
                palette,
            });
        }
    }
    None
}

fn kde(d: &Dirs) -> Option<Detected> {
    let text = read(&d.config.join("kdeglobals"))?;
    let name = scan(&text).get("General.ColorScheme").cloned();
    palette_from_text(&text).map(|palette| Detected {
        source: "KDE",
        name,
        palette,
    })
}

/// The first palette found, in priority order: dedicated theme managers, then
/// palette generators, then the desktop toolkits' colours.
pub fn detect(d: &Dirs) -> Option<Detected> {
    omarchy(d)
        .or_else(|| noctalia_json(d))
        .or_else(|| wal(d))
        .or_else(|| stylix(d))
        .or_else(|| {
            if d.kde {
                kde(d).or_else(|| gtk(d))
            } else {
                gtk(d).or_else(|| kde(d))
            }
        })
}

/// Every file whose change should re-run detection.
fn watched(d: &Dirs) -> Vec<PathBuf> {
    let cur = d.omarchy();
    let mut v = vec![
        cur.join("theme"),
        cur.join("theme/colors.toml"),
        cur.join("theme/alacritty.toml"),
        cur.join("theme.name"),
        d.config.join("noctalia/colors.json"),
        d.cache.join("noctalia/colors.json"),
        d.config.join("stylix/palette.json"),
        d.config.join("kdeglobals"),
    ];
    v.extend(d.wal_files().into_iter().map(|(_, p)| p));
    for dir in d.gtk_dirs() {
        v.push(dir.join("gtk.css"));
        if let Ok(rd) = std::fs::read_dir(&dir) {
            v.extend(
                rd.flatten()
                    .map(|e| e.path())
                    .filter(|p| p.extension().is_some_and(|e| e == "css")),
            );
        }
    }
    v
}

/// A cheap fingerprint of the watched files: where each resolves to (an
/// Omarchy theme switch swaps a symlink or directory), when it was written
/// and how big it is.
fn signature(d: &Dirs, extra: &[PathBuf]) -> u64 {
    let mut h = DefaultHasher::new();
    for p in watched(d).iter().chain(extra) {
        p.hash(&mut h);
        std::fs::read_link(p).ok().hash(&mut h);
        match std::fs::metadata(p) {
            Ok(m) => {
                std::fs::canonicalize(p).ok().hash(&mut h);
                m.modified().ok().hash(&mut h);
                m.len().hash(&mut h);
            }
            Err(_) => 0u8.hash(&mut h),
        }
    }
    h.finish()
}

// ---------------------------------------------------------------- presets --

/// A built-in colour scheme.
#[derive(Debug, PartialEq)]
pub struct Preset {
    pub id: &'static str,
    pub label: &'static str,
    bg: u32,
    fg: u32,
    accent: u32,
    red: u32,
    green: u32,
    yellow: u32,
    blue: u32,
}

impl Preset {
    pub fn palette(&self) -> Palette {
        Palette {
            red: Some(hex(self.red)),
            green: Some(hex(self.green)),
            yellow: Some(hex(self.yellow)),
            blue: Some(hex(self.blue)),
            ..Palette::new(hex(self.bg), hex(self.fg), hex(self.accent))
        }
    }
}

pub const PRESETS: &[Preset] = &[
    Preset {
        id: "tokyo-night",
        label: "Tokyo Night",
        bg: 0x1a1b26,
        fg: 0xc0caf5,
        accent: 0x7aa2f7,
        red: 0xf7768e,
        green: 0x9ece6a,
        yellow: 0xe0af68,
        blue: 0x7aa2f7,
    },
    Preset {
        id: "catppuccin-mocha",
        label: "Catppuccin Mocha",
        bg: 0x1e1e2e,
        fg: 0xcdd6f4,
        accent: 0xcba6f7,
        red: 0xf38ba8,
        green: 0xa6e3a1,
        yellow: 0xf9e2af,
        blue: 0x89b4fa,
    },
    Preset {
        id: "nord",
        label: "Nord",
        bg: 0x2e3440,
        fg: 0xeceff4,
        accent: 0x88c0d0,
        red: 0xbf616a,
        green: 0xa3be8c,
        yellow: 0xebcb8b,
        blue: 0x81a1c1,
    },
    Preset {
        id: "gruvbox-dark",
        label: "Gruvbox Dark",
        bg: 0x282828,
        fg: 0xebdbb2,
        accent: 0xfe8019,
        red: 0xfb4934,
        green: 0xb8bb26,
        yellow: 0xfabd2f,
        blue: 0x83a598,
    },
    Preset {
        id: "rose-pine-dawn",
        label: "Rosé Pine Dawn",
        bg: 0xfaf4ed,
        fg: 0x575279,
        accent: 0x907aa9,
        red: 0xb4637a,
        green: 0x56949f,
        yellow: 0xea9d34,
        blue: 0x286983,
    },
    Preset {
        id: "solarized-light",
        label: "Solarized Light",
        bg: 0xfdf6e3,
        fg: 0x586e75,
        accent: 0x268bd2,
        red: 0xdc322f,
        green: 0x859900,
        yellow: 0xb58900,
        blue: 0x268bd2,
    },
];

// ----------------------------------------------------------------- choice --

/// What the user picked.
#[derive(Clone, Debug, PartialEq)]
pub enum Choice {
    /// Follow the desktop: its palette if a generator wrote one, else light/dark.
    System,
    Light,
    Dark,
    Preset(&'static Preset),
    /// A palette file (`SURGE_THEME=/path/to/colors.json`).
    File(PathBuf),
}

impl Choice {
    /// From the stored setting or `SURGE_THEME`.
    pub fn parse(s: &str) -> Self {
        let s = s.trim();
        match s.to_lowercase().as_str() {
            "" | "system" | "auto" => Self::System,
            "light" => Self::Light,
            "dark" => Self::Dark,
            id => PRESETS
                .iter()
                .find(|p| p.id == id)
                .map(Self::Preset)
                .or_else(|| {
                    (s.contains('/') || Path::new(s).is_file()).then(|| Self::File(s.into()))
                })
                .unwrap_or(Self::System),
        }
    }
}

/// The setting, unless `SURGE_THEME` overrides it (screenshots, testing).
fn effective(setting: &str) -> Choice {
    match std::env::var("SURGE_THEME") {
        Ok(v) if !v.trim().is_empty() => Choice::parse(&v),
        _ => Choice::parse(setting),
    }
}

// -------------------------------------------------------------- applying --

fn h(c: Rgb) -> Hsla {
    Rgba {
        r: c[0],
        g: c[1],
        b: c[2],
        a: 1.0,
    }
    .into()
}

/// Paint a scheme over a base theme's colours.
fn paint(t: &mut ThemeColor, s: &Scheme) {
    let hov2 = mix(s.bg, s.fg, 0.11);
    let solid = |c: Rgb| (h(c), h(shade(c, 0.12)), h(shade(c, 0.22)), h(s.on(c)));
    t.background = h(s.bg);
    t.foreground = h(s.fg);
    t.border = h(s.border);
    t.input = h(s.border);
    t.caret = h(s.fg);
    t.ring = h(s.primary);
    t.selection = h(s.primary).opacity(0.3);
    t.muted = h(s.muted);
    t.muted_foreground = h(s.muted_fg);
    t.accent = h(s.hover);
    t.accent_foreground = h(s.fg);
    t.popover = h(s.surface);
    t.popover_foreground = h(s.fg);
    t.secondary = h(s.surface);
    t.secondary_hover = h(hov2);
    t.secondary_active = h(s.active);
    t.secondary_foreground = h(s.fg);
    t.button = h(s.surface);
    t.button_foreground = h(s.fg);
    t.button_hover = h(hov2);
    t.button_active = h(s.active);
    t.button_secondary = h(s.surface);
    t.button_secondary_hover = h(hov2);
    t.button_secondary_active = h(s.active);
    t.button_secondary_foreground = h(s.fg);

    (
        t.primary,
        t.primary_hover,
        t.primary_active,
        t.primary_foreground,
    ) = solid(s.primary);
    (
        t.button_primary,
        t.button_primary_hover,
        t.button_primary_active,
        t.button_primary_foreground,
    ) = solid(s.primary);
    (
        t.danger,
        t.danger_hover,
        t.danger_active,
        t.danger_foreground,
    ) = solid(s.danger);
    (
        t.button_danger,
        t.button_danger_hover,
        t.button_danger_active,
        t.button_danger_foreground,
    ) = solid(s.danger);
    (
        t.success,
        t.success_hover,
        t.success_active,
        t.success_foreground,
    ) = solid(s.success);
    (
        t.button_success,
        t.button_success_hover,
        t.button_success_active,
        t.button_success_foreground,
    ) = solid(s.success);
    (
        t.warning,
        t.warning_hover,
        t.warning_active,
        t.warning_foreground,
    ) = solid(s.warning);
    (
        t.button_warning,
        t.button_warning_hover,
        t.button_warning_active,
        t.button_warning_foreground,
    ) = solid(s.warning);
    (t.info, t.info_hover, t.info_active, t.info_foreground) = solid(s.info);
    (
        t.button_info,
        t.button_info_hover,
        t.button_info_active,
        t.button_info_foreground,
    ) = solid(s.info);

    t.link = h(s.primary);
    t.link_hover = h(shade(s.primary, 0.15));
    t.link_active = h(shade(s.primary, 0.25));
    t.progress_bar = h(s.primary);
    t.slider_bar = h(s.primary);
    t.slider_thumb = h(s.primary);
    t.switch = h(mix(s.bg, s.fg, 0.25));
    t.switch_thumb = h(s.fg);
    t.skeleton = h(s.muted);
    t.scrollbar_thumb = h(mix(s.bg, s.fg, 0.3));
    t.scrollbar_thumb_hover = h(mix(s.bg, s.fg, 0.5));
    t.drag_border = h(s.primary);
    t.drop_target = h(s.primary).opacity(0.2);
    t.window_border = h(s.border);
    t.group_box = h(s.surface);
    t.group_box_foreground = h(s.fg);

    let sel = h(s.primary).opacity(0.16);
    t.list = h(s.bg);
    t.list_even = h(s.bg);
    t.list_head = h(s.surface);
    t.list_hover = h(s.hover);
    t.list_active = sel;
    t.list_active_border = h(s.primary);
    t.table = h(s.bg);
    t.table_even = h(s.bg);
    t.table_head = h(s.surface);
    t.table_head_foreground = h(s.muted_fg);
    t.table_foot = h(s.surface);
    t.table_foot_foreground = h(s.muted_fg);
    t.table_hover = h(s.hover);
    t.table_active = sel;
    t.table_active_border = h(s.primary);
    t.table_row_border = h(s.border);
    t.tab_bar = h(s.surface);
    t.tab_bar_segmented = h(s.muted);
    t.tab_active = h(s.bg);
    t.tab_active_foreground = h(s.fg);
    t.tab_foreground = h(s.muted_fg);
    t.sidebar = h(s.surface);
    t.sidebar_foreground = h(s.fg);
    t.sidebar_border = h(s.border);
    t.sidebar_accent = h(s.hover);
    t.sidebar_accent_foreground = h(s.fg);
    t.sidebar_primary = h(s.primary);
    t.sidebar_primary_foreground = h(s.primary_fg);
    t.title_bar = h(s.bg);
    t.title_bar_border = h(s.border);
    t.status_bar = h(s.bg);
    t.status_bar_border = h(s.border);

    t.red = h(s.danger);
    t.green = h(s.success);
    t.yellow = h(s.warning);
    t.blue = h(s.info);
    t.chart_1 = h(s.primary);
    t.chart_2 = h(s.success);
    t.chart_3 = h(s.warning);
    t.chart_4 = h(s.danger);
    t.chart_5 = h(s.info);
}

fn apply_scheme(s: &Scheme, cx: &mut App) {
    Theme::change(
        if s.dark {
            ThemeMode::Dark
        } else {
            ThemeMode::Light
        },
        None,
        cx,
    );
    Theme::update(cx, |theme| paint(&mut theme.colors, s));
}

/// What is applied now, kept so the poll can tell when something changed.
struct State {
    setting: String,
    sig: u64,
    label: String,
    system_label: String,
    extra: Vec<PathBuf>,
}

impl Global for State {}

fn os_label(dark: bool) -> String {
    format!("OS {}", if dark { "dark" } else { "light" })
}

/// Resolve the setting to a palette (if any) and paint the theme.
/// `window` only supplies the OS appearance when nothing else decides.
pub fn apply(setting: &str, window: Option<&Window>, cx: &mut App) {
    let dirs = Dirs::from_env();
    let choice = effective(setting);
    let detected = detect(&dirs);
    let appearance = window
        .map(|w| w.appearance())
        .unwrap_or_else(|| cx.window_appearance());
    let os_dark = ThemeMode::from(appearance).is_dark();

    let system_label = detected
        .as_ref()
        .map(Detected::label)
        .unwrap_or_else(|| os_label(os_dark));
    let mut extra = vec![];
    let label = match &choice {
        Choice::System => {
            match &detected {
                Some(d) => apply_scheme(&derive(&d.palette), cx),
                None => Theme::change(appearance, None, cx),
            }
            format!("System · {system_label}")
        }
        Choice::Light => {
            Theme::change(ThemeMode::Light, None, cx);
            "Light".into()
        }
        Choice::Dark => {
            Theme::change(ThemeMode::Dark, None, cx);
            "Dark".into()
        }
        Choice::Preset(p) => {
            apply_scheme(&derive(&p.palette()), cx);
            p.label.into()
        }
        Choice::File(path) => {
            extra.push(path.clone());
            match read(path).and_then(|t| palette_from_text(&t)) {
                Some(p) => {
                    apply_scheme(&derive(&p), cx);
                    format!("File · {}", path.display())
                }
                None => {
                    Theme::change(appearance, None, cx);
                    format!("File · {} (unreadable)", path.display())
                }
            }
        }
    };
    let sig = signature(&dirs, &extra);
    cx.set_global(State {
        setting: setting.to_string(),
        sig,
        label,
        system_label,
        extra,
    });
}

/// Re-apply when the setting changed or a generator rewrote its files.
/// Cheap enough to call every second.
pub fn poll(setting: &str, cx: &mut App) {
    let dirs = Dirs::from_env();
    let stale = match cx.try_global::<State>() {
        Some(s) => s.setting != setting || s.sig != signature(&dirs, &s.extra),
        None => true,
    };
    if stale {
        apply(setting, None, cx);
    }
}

/// "System · Omarchy (tokyo-night)", "Nord", …
pub fn status(cx: &App) -> String {
    cx.try_global::<State>()
        .map(|s| s.label.clone())
        .unwrap_or_else(|| {
            if cx.theme().mode.is_dark() {
                "Dark"
            } else {
                "Light"
            }
            .into()
        })
}

/// What "System (auto)" currently resolves to, e.g. "Omarchy (tokyo-night)".
pub fn system_label(cx: &App) -> String {
    cx.try_global::<State>()
        .map(|s| s.system_label.clone())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn close(a: Rgb, b: Rgb) -> bool {
        a.iter().zip(b).all(|(x, y)| (x - y).abs() < 0.005)
    }

    #[test]
    fn colors() {
        assert!(close(parse_color("#ff8000").unwrap(), [1.0, 0.502, 0.0]));
        assert!(close(parse_color("0x1a1b26").unwrap(), hex(0x1a1b26)));
        assert!(close(parse_color("'#fff'").unwrap(), WHITE));
        assert!(close(parse_color("28,28,28").unwrap(), hex(0x1c1c1c)));
        assert!(close(
            parse_color("rgb(12, 12, 12)").unwrap(),
            hex(0x0c0c0c)
        ));
        assert!(close(parse_color("1d2021").unwrap(), hex(0x1d2021)));
        assert!(close(parse_color("#11223344").unwrap(), hex(0x112233)));
        assert!(parse_color("banana").is_none());
        assert!(parse_color("").is_none());
        assert!((contrast(BLACK, WHITE) - 21.0).abs() < 0.01);
    }

    #[test]
    fn omarchy_colors_toml() {
        let t = r##"
accent = "#7aa2f7"
cursor = "#c0caf5"
foreground = "#a9b1d6"
background = "#1a1b26"
selection_foreground = "#c0caf5"
selection_background = "#7aa2f7"

color0 = "#32344a"
color1 = "#f7768e"
color2 = "#9ece6a"
color3 = "#e0af68"
color4 = "#7aa2f7"
color5 = "#ad8ee6"
color6 = "#449dab"
color7 = "#787c99"
"##;
        let p = palette_from_text(t).unwrap();
        assert!(close(p.bg, hex(0x1a1b26)));
        assert!(close(p.accent, hex(0x7aa2f7)));
        assert!(close(p.red.unwrap(), hex(0xf7768e)));
        assert!(close(p.green.unwrap(), hex(0x9ece6a)));
        assert!(derive(&p).dark);
    }

    /// Upstream basecamp/omarchy themes/tokyo-night/colors.toml (named keys).
    #[test]
    fn omarchy_named_colors_toml() {
        let t = r##"
mode = "dark"
accent = "#7aa2f7"
selection = "#292e42"
muted = "#414868"
background = "#1a1b26"
lighter_background = "#24283b"
foreground = "#a9b1d6"
red = "#f7768e"
yellow = "#e0af68"
green = "#9ece6a"
blue = "#7aa2f7"
bright_red = "#ff7a93"
"##;
        let p = palette_from_text(t).unwrap();
        assert!(close(p.bg, hex(0x1a1b26)));
        assert!(close(p.accent, hex(0x7aa2f7)));
        assert!(close(p.red.unwrap(), hex(0xf7768e)));
        assert!(close(p.green.unwrap(), hex(0x9ece6a)));
        assert!(close(p.muted.unwrap(), hex(0x414868)));
        assert!(derive(&p).dark);
    }

    #[test]
    fn alacritty_toml() {
        let t = r##"
[colors.primary]
background = "0x1a1b26"
foreground = '0xc0caf5'

[colors.normal]
black = "0x15161e"
red = "0xf7768e"
green = "0x9ece6a"
yellow = "0xe0af68"
blue = "0x7aa2f7"
magenta = "0xbb9af7"
cyan = "0x7dcfff"
white = "0xa9b1d6"
"##;
        let p = palette_from_text(t).unwrap();
        assert!(close(p.bg, hex(0x1a1b26)));
        assert!(close(p.fg, hex(0xc0caf5)));
        assert!(close(p.blue.unwrap(), hex(0x7aa2f7)));
        // blue is vivid and wins the accent
        assert!(close(p.accent, hex(0x7aa2f7)));
    }

    #[test]
    fn pywal_json_pretty_and_minified() {
        let pretty = r##"{
    "wallpaper": "/x.png",
    "alpha": "100",
    "special": {
        "background": "#10141c",
        "foreground": "#e6e0d4",
        "cursor": "#e6e0d4"
    },
    "colors": {
        "color0": "#10141c",
        "color1": "#c4635f",
        "color2": "#6fa06a",
        "color3": "#c9a45c",
        "color4": "#5f87c4",
        "color5": "#a56fc4",
        "color6": "#5fb5b0",
        "color7": "#e6e0d4"
    }
}"##;
        let a = palette_from_text(pretty).unwrap();
        let min: String = pretty.split_whitespace().collect();
        let b = palette_from_text(&min).unwrap();
        assert_eq!(a, b);
        assert!(close(a.bg, hex(0x10141c)));
        assert!(close(a.red.unwrap(), hex(0xc4635f)));
    }

    #[test]
    fn noctalia_json() {
        let t = r##"{
  "mPrimary": "#aaaaff",
  "mOnPrimary": "#000000",
  "mError": "#ff6666",
  "mSurface": "#101018",
  "mOnSurface": "#eeeeee",
  "mSurfaceVariant": "#1c1c28",
  "mOnSurfaceVariant": "#9a9aa8",
  "mOutline": "#33334a"
}"##;
        let p = palette_from_text(t).unwrap();
        assert!(close(p.accent, hex(0xaaaaff)));
        assert!(close(p.surface.unwrap(), hex(0x1c1c28)));
        assert!(close(p.red.unwrap(), hex(0xff6666)));
    }

    #[test]
    fn base16() {
        let yaml = "scheme: \"Gruvbox\"\nbase00: \"282828\"\nbase01: \"3c3836\"\nbase02: \"504945\"\nbase03: \"665c54\"\nbase04: \"bdae93\"\nbase05: \"d5c4a1\"\nbase08: \"fb4934\"\nbase0A: \"fabd2f\"\nbase0B: \"b8bb26\"\nbase0D: \"83a598\"\n";
        let p = palette_from_text(yaml).unwrap();
        assert!(close(p.bg, hex(0x282828)));
        assert!(close(p.accent, hex(0x83a598)));
        assert!(close(p.yellow.unwrap(), hex(0xfabd2f)));
        let json = r##"{"base00": "1d2021", "base05": "#d5c4a1", "base0D": "83a598"}"##;
        assert!(palette_from_text(json).is_some());
    }

    #[test]
    fn kde() {
        let t = "[Colors:Header][Inactive]\nBackgroundNormal=1,2,3\n\n[Colors:Window]\nBackgroundNormal=28,28,28\nForegroundNormal=255,255,255\nDecorationFocus=255,199,153\nForegroundNegative=255,128,128\n\n[General]\nColorScheme=noctalia\n";
        let p = palette_from_text(t).unwrap();
        assert!(close(p.bg, hex(0x1c1c1c)));
        assert!(close(p.accent, hex(0xffc799)));
        assert_eq!(scan(t).get("General.ColorScheme").unwrap(), "noctalia");
    }

    #[test]
    fn gtk_css_with_references() {
        let t = "@define-color accent_bg_color #ffc799;\n@define-color window_bg_color #0c0c0c;\n@define-color window_fg_color #ffffff;\n@define-color card_bg_color #1c1c1c;\n@define-color error_bg_color @destructive;\n@define-color destructive #ff8080;\n";
        let p = palette_from_text(t).unwrap();
        assert!(close(p.bg, hex(0x0c0c0c)));
        assert!(close(p.surface.unwrap(), hex(0x1c1c1c)));
        assert!(close(p.red.unwrap(), hex(0xff8080)));
        assert_eq!(
            css_imports("@import url(\"noctalia.css\");\n@import 'x.css';"),
            vec!["noctalia.css", "x.css"]
        );
    }

    #[test]
    fn derived_text_is_readable() {
        let mut all: Vec<Palette> = PRESETS.iter().map(Preset::palette).collect();
        // Awkward inputs: low-contrast text and an accent close to the background.
        all.push(Palette::new(hex(0x222222), hex(0x2a2a2a), hex(0x242424)));
        all.push(Palette::new(hex(0xf0f0f0), hex(0xe8e8e8), hex(0xf4f4f4)));
        for p in all {
            let s = derive(&p);
            assert!(contrast(s.fg, s.bg) >= 4.5, "fg");
            assert!(contrast(s.muted_fg, s.bg) >= 4.5, "muted fg");
            assert!(contrast(s.primary_fg, s.primary) >= 4.5, "text on primary");
            assert!(contrast(s.primary, s.bg) >= 3.0, "primary");
            assert!(contrast(s.danger, s.bg) >= 3.0, "danger");
            assert!(contrast(s.muted, s.bg) > 1.05, "progress track visible");
            assert!(contrast(s.border, s.bg) > 1.1, "border visible");
        }
        assert!(derive(&PRESETS[0].palette()).dark);
        assert!(!derive(&PRESETS[4].palette()).dark);
    }

    #[test]
    fn choices() {
        assert_eq!(Choice::parse("system"), Choice::System);
        assert_eq!(Choice::parse(""), Choice::System);
        assert_eq!(Choice::parse("Dark"), Choice::Dark);
        assert!(matches!(Choice::parse("nord"), Choice::Preset(p) if p.id == "nord"));
        assert!(matches!(Choice::parse("/tmp/x.json"), Choice::File(_)));
        assert_eq!(Choice::parse("nonsense"), Choice::System);
    }

    fn temp(tag: &str) -> Dirs {
        let root = std::env::temp_dir().join(format!("surge-theme-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("config")).unwrap();
        std::fs::create_dir_all(root.join("cache")).unwrap();
        Dirs {
            config: root.join("config"),
            cache: root.join("cache"),
            kde: false,
        }
    }

    fn put(p: PathBuf, text: &str) {
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, text).unwrap();
    }

    #[test]
    fn detection_priority_and_reload() {
        let d = temp("detect");
        assert!(detect(&d).is_none());
        let sig0 = signature(&d, &[]);

        // pywal alone
        let wal = d.cache.join("wal/colors.json");
        put(
            wal.clone(),
            r##"{"special":{"background":"#101010","foreground":"#eeeeee"},"colors":{"color4":"#5f87c4"}}"##,
        );
        let got = detect(&d).unwrap();
        assert_eq!(got.source, "Pywal");
        assert_ne!(sig0, signature(&d, &[]));

        // Omarchy beats it; its name comes from theme.name
        let cur = d.config.join("omarchy/current");
        put(
            cur.join("theme/colors.toml"),
            "background = \"#fafafa\"\nforeground = \"#222222\"\naccent = \"#3366cc\"\n",
        );
        put(cur.join("theme.name"), "paper\n");
        let got = detect(&d).unwrap();
        assert_eq!(got.label(), "Omarchy (paper)");
        assert!(!derive(&got.palette).dark);

        // Theme switch: the symlink moves to another directory.
        #[cfg(unix)]
        {
            let _ = std::fs::remove_dir_all(cur.join("theme"));
            let themes = d.config.join("omarchy/themes");
            put(
                themes.join("night/colors.toml"),
                "background = \"#0a0a0a\"\nforeground = \"#dddddd\"\naccent = \"#ff9900\"\n",
            );
            std::os::unix::fs::symlink(themes.join("night"), cur.join("theme")).unwrap();
            std::fs::remove_file(cur.join("theme.name")).unwrap();
            let sig1 = signature(&d, &[]);
            let got = detect(&d).unwrap();
            assert_eq!(got.label(), "Omarchy (night)");
            assert!(derive(&got.palette).dark);
            // ... and back
            std::fs::remove_file(cur.join("theme")).unwrap();
            std::os::unix::fs::symlink(d.config.join("omarchy/themes/night"), cur.join("theme"))
                .unwrap();
            put(
                themes.join("night/colors.toml"),
                "background = \"#0b0b0b\"\nforeground = \"#dddddd\"\naccent = \"#ff9900\"\n",
            );
            assert_ne!(sig1, signature(&d, &[]));
        }
        let _ = std::fs::remove_dir_all(d.config.parent().unwrap());
    }

    #[test]
    fn gtk_import_names_the_generator() {
        let d = temp("gtk");
        let gtk3 = d.config.join("gtk-3.0");
        put(gtk3.join("gtk.css"), "@import url(\"noctalia.css\");\n");
        put(
            gtk3.join("noctalia.css"),
            "@define-color window_bg_color #0c0c0c;\n@define-color window_fg_color #ffffff;\n@define-color accent_bg_color #ffc799;\n",
        );
        assert_eq!(detect(&d).unwrap().label(), "Noctalia");
        let _ = std::fs::remove_dir_all(d.config.parent().unwrap());
    }
}
