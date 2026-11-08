use std::collections::HashSet;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use gpui_kit::component::button::{Button, ButtonVariants};
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::menu::{DropdownMenu, PopupMenuItem};
use gpui_kit::component::{
    ActiveTheme, Disableable, Icon, IconName, Selectable, Sizable, StyledExt, h_flex, v_flex,
};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use surge_core::{AddRequest, Download, Engine, Kind, Status, util};

#[derive(Clone, Copy, PartialEq, Eq)]
enum Filter {
    All,
    Active,
    Waiting,
    Done,
}

impl Filter {
    const ALL: [Filter; 4] = [Filter::All, Filter::Active, Filter::Waiting, Filter::Done];

    fn label(self) -> &'static str {
        match self {
            Filter::All => "All",
            Filter::Active => "Active",
            Filter::Waiting => "Paused / failed",
            Filter::Done => "Completed",
        }
    }

    fn matches(self, d: &Download) -> bool {
        match self {
            Filter::All => true,
            Filter::Active => d.status.is_active(),
            Filter::Waiting => matches!(
                d.status,
                Status::Queued | Status::Paused | Status::Failed(_)
            ),
            Filter::Done => d.status == Status::Completed,
        }
    }
}

pub struct SurgeView {
    engine: Arc<Engine>,
    url_input: Entity<InputState>,
    search_input: Entity<InputState>,
    /// Lower-cased search text.
    query: String,
    filter: Filter,
    /// Rows whose connection/detail panel is open.
    expanded: HashSet<u64>,
    /// SURGE_VIEW=expanded opens every row's details (screenshots).
    expanded_all: bool,
    connections: usize,
    downloads: Vec<Download>,
    /// Cheap fingerprint of `downloads`; we only re-render when it changes.
    fingerprint: u64,
    error: Option<SharedString>,
    /// The "Download folders" panel is open instead of the list.
    show_folders: bool,
    /// Folders as last rendered (they can change from the extension).
    folders_fp: u64,
    _poll: Task<()>,
}

impl SurgeView {
    pub fn new(engine: Arc<Engine>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let url_input = cx.new(|cx| {
            InputState::new(window, cx).placeholder("Paste a download link and press Enter…")
        });
        cx.subscribe_in(
            &url_input,
            window,
            |this, _, ev: &InputEvent, window, cx| {
                if let InputEvent::PressEnter { .. } = ev {
                    this.submit(window, cx);
                }
            },
        )
        .detach();

        let search_input =
            cx.new(|cx| InputState::new(window, cx).placeholder("Search downloads…"));
        cx.subscribe_in(
            &search_input,
            window,
            |this, input, ev: &InputEvent, _, cx| {
                if let InputEvent::Change = ev {
                    this.query = input.read(cx).value().trim().to_lowercase();
                    cx.notify();
                }
            },
        )
        .detach();

        // Follow the desktop's light/dark preference, including later changes
        // (on Wayland the portal answers after the window opens).
        cx.observe_window_appearance(window, |this, window, cx| {
            crate::theme::apply(&this.engine.settings().theme, Some(window), cx);
        })
        .detach();

        // The engine runs on its own tokio runtime; poll its snapshot.
        let poll = cx.spawn_in(window, async move |this, cx| {
            loop {
                cx.background_executor()
                    .timer(Duration::from_millis(250))
                    .await;
                let alive = this.update(cx, |this, cx| this.refresh(cx));
                if alive.is_err() {
                    break;
                }
            }
        });

        let connections = engine.settings().connections;
        let mut this = Self {
            engine,
            url_input,
            search_input,
            query: String::new(),
            filter: Filter::All,
            expanded: HashSet::new(),
            expanded_all: std::env::var("SURGE_VIEW").is_ok_and(|v| v == "expanded"),
            connections,
            downloads: vec![],
            fingerprint: 0,
            error: None,
            // SURGE_VIEW=folders opens on the folders panel (screenshots, tests).
            show_folders: std::env::var("SURGE_VIEW").is_ok_and(|v| v == "folders"),
            folders_fp: 0,
            _poll: poll,
        };
        this.refresh(cx);
        this
    }

    fn refresh(&mut self, cx: &mut Context<Self>) {
        let downloads = self.engine.snapshot();
        let fp = downloads.iter().fold(downloads.len() as u64, |acc, d| {
            let status = match &d.status {
                Status::Failed(e) => 100 + e.len() as u64,
                s => s.label().len() as u64,
            };
            acc.wrapping_mul(31)
                .wrapping_add(d.id ^ d.downloaded ^ (d.speed as u64) << 1 ^ status << 7)
                .wrapping_add(d.active_connections as u64 + d.segments.len() as u64 * 131)
                .wrapping_add(d.total.unwrap_or(0))
                .wrapping_add(d.speed_limit.rotate_left(17) ^ (d.connections as u64) << 40)
                .wrapping_add(
                    d.note.as_ref().map_or(0, |n| {
                        n.bytes()
                            .fold(7u64, |a, b| a.wrapping_mul(31).wrapping_add(b as u64))
                    }) << 50,
                )
                .wrapping_add(d.filename.len() as u64)
                .wrapping_add(d.segments.iter().fold(0u64, |a, s| {
                    // Per-connection speed (to 4 KiB/s) and stalls change the detail panel.
                    let stalled = s.last_data.is_some_and(|t| t.elapsed().as_secs() >= 3);
                    a.wrapping_mul(7).wrapping_add(
                        (s.speed >> 12) ^ (s.owned as u64) << 3 ^ (stalled as u64) << 4,
                    )
                }))
        });
        if fp != self.fingerprint {
            self.fingerprint = fp;
            self.downloads = downloads;
            cx.notify();
        }
        let settings = self.engine.settings();
        let folders_fp = {
            use std::hash::{Hash, Hasher};
            let mut h = std::collections::hash_map::DefaultHasher::new();
            settings.download_dir.hash(&mut h);
            settings.type_dirs.hash(&mut h);
            settings.connections.hash(&mut h);
            h.finish()
        };
        if folders_fp != self.folders_fp {
            self.folders_fp = folders_fp;
            self.connections = settings.connections;
            cx.notify();
        }
    }

    /// Choose a folder with the system picker: the main one (`None`) or a
    /// file type's.
    fn pick_folder(&mut self, category: Option<&'static str>, cx: &mut Context<Self>) {
        let rx = cx.prompt_for_paths(PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: Some("Use this folder".into()),
        });
        let engine = self.engine.clone();
        cx.spawn(async move |this, cx| {
            let Ok(Ok(Some(paths))) = rx.await else {
                return;
            };
            let Some(dir) = paths.into_iter().next() else {
                return;
            };
            let mut s = engine.settings();
            match category {
                None => s.download_dir = dir,
                Some(c) => {
                    s.type_dirs.insert(c.into(), dir);
                }
            }
            engine.set_settings(s);
            let _ = this.update(cx, |this, cx| this.refresh(cx));
        })
        .detach();
    }

    fn reset_folder(&mut self, category: &str, cx: &mut Context<Self>) {
        let mut s = self.engine.settings();
        s.type_dirs.remove(category);
        self.engine.set_settings(s);
        self.refresh(cx);
    }

    /// Palette button: System (follows the desktop's palette), Light, Dark and presets.
    fn theme_menu(&self, cx: &App) -> AnyElement {
        let engine = self.engine.clone();
        let current = crate::theme::Choice::parse(&engine.settings().theme);
        let now = crate::theme::status(cx);
        let system = crate::theme::system_label(cx);
        Button::new("theme")
            .ghost()
            .xsmall()
            .icon(IconName::Palette)
            .tooltip(format!("Theme: {now}"))
            .dropdown_menu(move |mut menu, _, _| {
                use crate::theme::{Choice, PRESETS};
                let entry = |menu: gpui_kit::component::menu::PopupMenu,
                             label: String,
                             id: &'static str,
                             on: bool| {
                    let engine = engine.clone();
                    menu.item(PopupMenuItem::new(label).checked(on).on_click(
                        move |_, window, cx| {
                            let mut s = engine.settings();
                            s.theme = id.to_string();
                            engine.set_settings(s);
                            crate::theme::apply(id, Some(window), cx);
                            window.refresh();
                        },
                    ))
                };
                menu = menu.label("Theme");
                menu = entry(
                    menu,
                    format!("System (auto) · {system}"),
                    "system",
                    current == Choice::System,
                );
                menu = entry(menu, "Light".into(), "light", current == Choice::Light);
                menu = entry(menu, "Dark".into(), "dark", current == Choice::Dark);
                menu = menu.separator().label("Presets");
                for p in PRESETS {
                    let on = matches!(&current, Choice::Preset(c) if c.id == p.id);
                    menu = entry(menu, p.label.to_string(), p.id, on);
                }
                menu
            })
            .into_any_element()
    }

    /// Where downloads go: one main folder, and optionally one per file type.
    fn render_folders(&self, cx: &mut Context<Self>) -> AnyElement {
        let s = self.engine.settings();
        let theme = cx.theme();
        let row = |id: &'static str,
                   label: &str,
                   path: Option<String>,
                   hint: &str,
                   category: Option<&'static str>,
                   cx: &mut Context<Self>| {
            let theme = cx.theme();
            h_flex()
                .gap_3()
                .px_4()
                .py_2()
                .items_center()
                .border_b_1()
                .border_color(theme.border)
                .child(
                    Icon::new(if category.is_none() {
                        IconName::HardDrive
                    } else {
                        IconName::Folder
                    })
                    .text_color(theme.muted_foreground),
                )
                .child(
                    v_flex()
                        .flex_1()
                        .min_w_0()
                        .child(div().text_sm().font_semibold().child(label.to_string()))
                        .child(match &path {
                            Some(p) => div().text_xs().truncate().child(p.clone()),
                            None => div()
                                .text_xs()
                                .text_color(theme.muted_foreground)
                                .child(hint.to_string()),
                        }),
                )
                .when(category.is_some() && path.is_some(), |el| {
                    let c = category.unwrap();
                    el.child(
                        Button::new(SharedString::from(format!("reset-{id}")))
                            .ghost()
                            .xsmall()
                            .icon(IconName::Undo2)
                            .tooltip("Use the main folder again")
                            .on_click(cx.listener(move |this, _, _, cx| this.reset_folder(c, cx))),
                    )
                })
                .child(
                    Button::new(SharedString::from(format!("pick-{id}")))
                        .outline()
                        .xsmall()
                        .icon(IconName::FolderOpen)
                        .label("Change…")
                        .on_click(
                            cx.listener(move |this, _, _, cx| this.pick_folder(category, cx)),
                        ),
                )
        };
        let main = short_path(&s.download_dir);
        let mut panel = v_flex()
            .id("folders")
            .flex_1()
            .overflow_y_scroll()
            .child(
                v_flex()
                    .px_4()
                    .pt_3()
                    .pb_2()
                    .gap_1()
                    .child(div().text_base().font_semibold().child("Download folders"))
                    .child(div().text_xs().text_color(theme.muted_foreground).child(
                        "Each file goes to its type's folder. Types without one use the main folder. \
                         The browser extension uses these too.",
                    )),
            )
            .child(row("main", "Main folder (everything else)", Some(main.clone()), "", None, cx));
        for (id, label) in util::CATEGORIES {
            let path = s.type_dirs.get(id).map(|p| short_path(p));
            panel = panel.child(row(
                id,
                label,
                path,
                &format!("Main folder ({main})"),
                Some(id),
                cx,
            ));
        }
        panel.into_any_element()
    }

    fn submit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let text = self.url_input.read(cx).value().to_string();
        let urls: Vec<&str> = text.split_whitespace().collect();
        if urls.is_empty() {
            return;
        }
        let mut errors = vec![];
        for url in urls {
            let mut req = AddRequest::new(url);
            req.connections = Some(self.connections);
            if let Err(e) = self.engine.add(req) {
                errors.push(e.to_string());
            }
        }
        self.error = (!errors.is_empty()).then(|| errors.join("\n").into());
        if errors.is_empty() {
            self.url_input
                .update(cx, |input, cx| input.set_value("", window, cx));
        }
        self.refresh(cx);
        cx.notify();
    }

    fn set_connections(&mut self, n: usize, cx: &mut Context<Self>) {
        self.connections = n.clamp(1, 32);
        let mut s = self.engine.settings();
        s.connections = self.connections;
        self.engine.set_settings(s);
        cx.notify();
    }

    fn render_add_bar(&self, cx: &mut Context<Self>) -> AnyElement {
        let n = self.connections;
        h_flex()
            .gap_2()
            .child(
                div().flex_1().child(
                    Input::new(&self.url_input)
                        .cleanable(true)
                        .prefix(Icon::new(IconName::Globe).text_color(cx.theme().muted_foreground)),
                ),
            )
            .child(
                h_flex()
                    .gap_1()
                    .items_center()
                    .px_2()
                    .h_8()
                    .rounded(cx.theme().radius)
                    .border_1()
                    .border_color(cx.theme().border)
                    .child(
                        Button::new("conn-dec")
                            .ghost()
                            .xsmall()
                            .icon(IconName::Minus)
                            .disabled(n <= 1)
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.set_connections(n.saturating_sub(1), cx)
                            })),
                    )
                    .child(
                        div()
                            .min_w(px(64.))
                            .text_center()
                            .text_sm()
                            .child(format!("{n} conn{}", if n == 1 { "" } else { "s" })),
                    )
                    .child(
                        Button::new("conn-inc")
                            .ghost()
                            .xsmall()
                            .icon(IconName::Plus)
                            .disabled(n >= 32)
                            .on_click(
                                cx.listener(move |this, _, _, cx| this.set_connections(n + 1, cx)),
                            ),
                    ),
            )
            .child(
                Button::new("add")
                    .primary()
                    .icon(IconName::ArrowDown)
                    .label("Download")
                    .on_click(cx.listener(|this, _, window, cx| this.submit(window, cx))),
            )
            .child(
                Button::new("folders")
                    .ghost()
                    .icon(IconName::Folder)
                    .selected(self.show_folders)
                    .tooltip("Download folders")
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.show_folders = !this.show_folders;
                        cx.notify();
                    })),
            )
            .into_any_element()
    }

    fn pause_all(&mut self, cx: &mut Context<Self>) {
        for d in &self.downloads {
            if d.status.is_active() || d.status == Status::Queued {
                self.engine.pause(d.id);
            }
        }
        self.refresh(cx);
    }

    fn resume_all(&mut self, cx: &mut Context<Self>) {
        for d in &self.downloads {
            if matches!(d.status, Status::Paused | Status::Failed(_)) {
                self.engine.resume(d.id);
            }
        }
        self.refresh(cx);
    }

    fn toggle_expanded(&mut self, id: u64, cx: &mut Context<Self>) {
        if !self.expanded.remove(&id) {
            self.expanded.insert(id);
        }
        cx.notify();
    }

    /// Second toolbar row: status filter, search, pause/resume all.
    fn render_filter_bar(&self, cx: &mut Context<Self>) -> AnyElement {
        let any_running = self
            .downloads
            .iter()
            .any(|d| d.status.is_active() || d.status == Status::Queued);
        let any_paused = self
            .downloads
            .iter()
            .any(|d| matches!(d.status, Status::Paused | Status::Failed(_)));
        h_flex()
            .gap_1()
            .items_center()
            .children(Filter::ALL.into_iter().map(|f| {
                let n = self.downloads.iter().filter(|d| f.matches(d)).count();
                Button::new(SharedString::from(format!("filter-{}", f.label())))
                    .ghost()
                    .small()
                    .selected(self.filter == f)
                    .label(format!("{} {n}", f.label()))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.filter = f;
                        cx.notify();
                    }))
            }))
            .child(div().flex_1())
            .child(
                div().w(px(220.)).child(
                    Input::new(&self.search_input)
                        .small()
                        .cleanable(true)
                        .prefix(
                            Icon::new(IconName::Search).text_color(cx.theme().muted_foreground),
                        ),
                ),
            )
            .child(
                Button::new("pause-all")
                    .ghost()
                    .small()
                    .icon(IconName::Pause)
                    .label("Pause all")
                    .disabled(!any_running)
                    .on_click(cx.listener(|this, _, _, cx| this.pause_all(cx))),
            )
            .child(
                Button::new("resume-all")
                    .ghost()
                    .small()
                    .icon(IconName::Play)
                    .label("Resume all")
                    .disabled(!any_paused)
                    .on_click(cx.listener(|this, _, _, cx| this.resume_all(cx))),
            )
            .into_any_element()
    }

    /// Expanded row: URL, location and one line per live connection.
    fn render_detail(&self, d: &Download, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme();
        let muted = theme.muted_foreground;
        let url = d.final_url.clone().unwrap_or_else(|| d.url.clone());
        let copy_url = url.clone();
        let id = d.id;
        let field = |label: &'static str, value: String| {
            h_flex()
                .gap_2()
                .text_xs()
                .child(div().w(px(56.)).text_color(muted).child(label))
                .child(div().flex_1().min_w_0().truncate().child(value))
        };
        let location = short_path(d.path.as_deref().unwrap_or(&d.dir));
        let mut panel = v_flex()
            .gap_1()
            .pl_8()
            .pt_1()
            .child(
                h_flex()
                    .gap_1()
                    .items_center()
                    .child(div().flex_1().min_w_0().child(field("Link", url)))
                    .child(
                        Button::new(("copy", id))
                            .ghost()
                            .xsmall()
                            .icon(IconName::Copy)
                            .label("Copy link")
                            .on_click(move |_, _, cx| {
                                cx.write_to_clipboard(ClipboardItem::new_string(copy_url.clone()))
                            }),
                    ),
            )
            .child(field("Saved to", location));

        let live: Vec<(usize, &surge_core::Segment)> = d
            .segments
            .iter()
            .enumerate()
            .filter(|(_, s)| s.owned && s.pos < s.end)
            .collect();
        if d.kind == Kind::File && d.status.is_active() && !d.segments.is_empty() {
            let done = d.segments.iter().filter(|s| s.pos >= s.end).count();
            panel = panel.child(div().pt_1().text_xs().font_semibold().child(format!(
                "{} connection{} receiving (max {})  ·  {} segment{}, {done} finished",
                live.len(),
                if live.len() == 1 { "" } else { "s" },
                d.connections,
                d.segments.len(),
                if d.segments.len() == 1 { "" } else { "s" },
            )));
            for (i, s) in live.iter().take(32) {
                let len = (s.end - s.start).max(1);
                let frac = ((s.pos - s.start) as f32 / len as f32).clamp(0.0, 1.0);
                let stalled = s.last_data.is_some_and(|t| t.elapsed().as_secs() >= 3);
                let (speed, color) = if stalled {
                    ("stalled".to_string(), theme.warning)
                } else if s.speed > 0 {
                    (util::fmt_speed(s.speed as f64), theme.foreground)
                } else {
                    ("…".to_string(), muted)
                };
                panel = panel.child(
                    h_flex()
                        .gap_2()
                        .items_center()
                        .text_xs()
                        .child(
                            div()
                                .w(px(28.))
                                .text_color(muted)
                                .child(format!("#{}", i + 1)),
                        )
                        .child(
                            div()
                                .w(px(150.))
                                .truncate()
                                .text_color(muted)
                                .child(format!(
                                    "{} – {}",
                                    util::fmt_bytes(s.start),
                                    util::fmt_bytes(s.end)
                                )),
                        )
                        .child(
                            div()
                                .flex_1()
                                .h(px(4.))
                                .rounded_full()
                                .overflow_hidden()
                                .bg(theme.muted)
                                .child(div().h_full().w(relative(frac)).bg(theme.primary)),
                        )
                        .child(
                            div()
                                .w(px(40.))
                                .text_right()
                                .text_color(muted)
                                .child(format!("{:.0}%", frac * 100.0)),
                        )
                        .child(div().w(px(76.)).text_right().text_color(color).child(speed)),
                );
            }
            if live.len() > 32 {
                panel = panel.child(
                    div()
                        .text_xs()
                        .text_color(muted)
                        .child(format!("+ {} more", live.len() - 32)),
                );
            }
        }
        panel.into_any_element()
    }

    fn render_row(&self, d: &Download, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme();
        let id = d.id;
        let (status_icon, status_color) = match &d.status {
            Status::Completed => (IconName::CircleCheck, theme.success),
            Status::Failed(_) => (IconName::CircleX, theme.danger),
            Status::Paused => (IconName::Pause, theme.warning),
            Status::Queued => (IconName::Loader, theme.muted_foreground),
            Status::Probing | Status::Downloading => (IconName::ArrowDown, theme.primary),
        };

        let media = d.kind == Kind::Media;
        let detail: SharedString = match &d.status {
            // yt-dlp: progress plus what it's working on ("Video (stream 1) ·
            // fragment 12/40"); post-processing steps have no byte progress.
            Status::Downloading if media && d.downloaded > 0 && d.active_connections > 0 => {
                let mut s = format!(
                    "{} of {}{}  ·  {}",
                    util::fmt_bytes(d.downloaded),
                    if d.total.is_some() { "~" } else { "" },
                    d.total.map(util::fmt_bytes).unwrap_or_else(|| "?".into()),
                    util::fmt_speed(d.speed),
                );
                if let Some(eta) = d.eta_secs() {
                    s.push_str(&format!("  ·  {} left", util::fmt_duration(eta)));
                }
                if let Some(note) = &d.note {
                    s.push_str(&format!("  ·  {note}"));
                }
                s.push_str("  ·  yt-dlp");
                s.into()
            }
            Status::Probing | Status::Downloading if d.note.is_some() => {
                format!("{}…", d.note.as_deref().unwrap_or_default()).into()
            }
            Status::Downloading if media => format!(
                "{}  ·  {}  ·  yt-dlp",
                util::fmt_bytes(d.downloaded),
                util::fmt_speed(d.speed)
            )
            .into(),
            Status::Downloading => {
                let mut s = format!(
                    "{} of {}  ·  {}/{} connections",
                    util::fmt_bytes(d.downloaded),
                    d.total.map(util::fmt_bytes).unwrap_or_else(|| "?".into()),
                    d.active_connections,
                    d.connections,
                );
                if !d.resumable {
                    s.push_str("  ·  single stream (server has no range support)");
                }
                s.into()
            }
            Status::Completed => format!(
                "{}  ·  {}",
                util::fmt_bytes(d.total.unwrap_or(d.downloaded)),
                short_path(&d.dir)
            )
            .into(),
            Status::Failed(e) => e.clone().into(),
            Status::Paused => match d.total {
                Some(t) => format!(
                    "Paused at {} of {}",
                    util::fmt_bytes(d.downloaded),
                    util::fmt_bytes(t)
                )
                .into(),
                None => "Paused".into(),
            },
            Status::Probing => "Connecting…".into(),
            Status::Queued => "Waiting for a free slot".into(),
        };

        let pct = d
            .progress()
            .map(|p| format!("{:.0}%", p * 100.0))
            .unwrap_or_default();
        // Right-hand column: percent, then live speed and time left.
        let live_line = (d.status == Status::Downloading && !media && d.speed > 0.0).then(|| {
            let mut t = util::fmt_speed(d.speed);
            if let Some(eta) = d.eta_secs() {
                t.push_str(&format!("  ·  {}", util::fmt_duration(eta)));
            }
            t
        });
        let expanded = self.expanded_all || self.expanded.contains(&id);

        let can_resume = matches!(d.status, Status::Paused | Status::Failed(_));
        let can_pause = d.status.is_active() || d.status == Status::Queued;
        let done = d.status == Status::Completed;
        let path = d.path.clone();
        let dir = d.dir.clone();

        let engine = self.engine.clone();
        let limit_engine = self.engine.clone();
        let limit = d.speed_limit;
        let conns = d.connections;
        let mut actions = h_flex().gap_1().when(!done, |el| {
            el.child(limit_menu(
                ("limit", id),
                limit,
                "Speed limit and connections for this file",
                Rc::new(move |bps| limit_engine.set_speed_limit(id, bps)),
                Some((conns, Rc::new(move |n| engine.set_connections(id, n)))),
            ))
        });
        if can_pause {
            actions = actions.child(
                Button::new(("pause", id))
                    .ghost()
                    .small()
                    .icon(IconName::Pause)
                    .tooltip("Pause")
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.engine.pause(id);
                        this.refresh(cx);
                    })),
            );
        }
        if can_resume {
            actions = actions.child(
                Button::new(("resume", id))
                    .ghost()
                    .small()
                    .icon(IconName::Play)
                    .tooltip(if d.resumable || d.downloaded == 0 {
                        "Resume"
                    } else {
                        "Restart (server can't resume)"
                    })
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.engine.resume(id);
                        this.refresh(cx);
                    })),
            );
        }
        if done {
            let file = path.clone();
            actions = actions.child(
                Button::new(("open", id))
                    .ghost()
                    .small()
                    .icon(IconName::ExternalLink)
                    .tooltip("Open file")
                    .on_click(move |_, _, cx| {
                        if let Some(p) = &file {
                            cx.open_with_system(p);
                        }
                    }),
            );
        }
        let copy = d.url.clone();
        actions = actions
            .child(
                Button::new(("copyrow", id))
                    .ghost()
                    .small()
                    .icon(IconName::Copy)
                    .tooltip("Copy link")
                    .on_click(move |_, _, cx| {
                        cx.write_to_clipboard(ClipboardItem::new_string(copy.clone()))
                    }),
            )
            .child(
                Button::new(("folder", id))
                    .ghost()
                    .small()
                    .icon(IconName::FolderOpen)
                    .tooltip("Show in folder")
                    .on_click(move |_, _, cx| match &path {
                        Some(p) if p.exists() => cx.reveal_path(p),
                        _ => cx.open_with_system(&dir),
                    }),
            )
            .child(
                Button::new(("remove", id))
                    .ghost()
                    .small()
                    .icon(IconName::Close)
                    .tooltip(if done {
                        "Remove from list (keeps the file)"
                    } else {
                        "Cancel and remove"
                    })
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.engine.remove(id, false);
                        this.refresh(cx);
                    })),
            );

        let detail_panel = expanded.then(|| self.render_detail(d, cx));
        let theme = cx.theme();
        v_flex()
            .id(("row", id))
            .gap_2()
            .px_4()
            .py_3()
            .border_b_1()
            .border_color(theme.border)
            .hover(|s| s.bg(theme.list_hover))
            .child(
                h_flex()
                    .gap_3()
                    .items_center()
                    .child(
                        Button::new(("expand", id))
                            .ghost()
                            .xsmall()
                            .icon(if expanded {
                                IconName::ChevronDown
                            } else {
                                IconName::ChevronRight
                            })
                            .tooltip(if expanded {
                                "Hide details"
                            } else {
                                "Show details"
                            })
                            .on_click(
                                cx.listener(move |this, _, _, cx| this.toggle_expanded(id, cx)),
                            ),
                    )
                    .child(Icon::new(status_icon).text_color(status_color))
                    .child(
                        v_flex()
                            .flex_1()
                            .min_w_0()
                            .child(
                                div()
                                    .text_sm()
                                    .font_semibold()
                                    .truncate()
                                    .child(SharedString::from(d.display_name().to_string())),
                            )
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(if matches!(d.status, Status::Failed(_)) {
                                        theme.danger
                                    } else {
                                        theme.muted_foreground
                                    })
                                    .truncate()
                                    .child(detail),
                            ),
                    )
                    .child(
                        v_flex()
                            .w(px(150.))
                            .items_end()
                            .child(
                                div()
                                    .text_sm()
                                    .font_semibold()
                                    .text_color(status_color)
                                    .child(pct),
                            )
                            .when_some(live_line, |el, t| {
                                el.child(
                                    div().text_xs().text_color(theme.muted_foreground).child(t),
                                )
                            }),
                    )
                    .child(actions),
            )
            .when(!done, |row| row.child(segment_bar(d, cx)))
            .when_some(detail_panel, |row, p| row.child(p))
            .into_any_element()
    }
}

type SetConnections = Rc<dyn Fn(usize)>;

const LIMIT_PRESETS: [u64; 9] = [
    0,
    128 << 10,
    256 << 10,
    512 << 10,
    1 << 20,
    2 << 20,
    5 << 20,
    10 << 20,
    20 << 20,
];

fn limit_label(bps: u64) -> String {
    if bps == 0 {
        "Unlimited".into()
    } else {
        util::fmt_speed(bps as f64)
    }
}

/// A small button showing the current limit that opens a preset menu.
/// With `connections`, the menu also lets you pick the connection count.
fn limit_menu(
    id: impl Into<ElementId>,
    current: u64,
    tooltip: &'static str,
    set_limit: Rc<dyn Fn(u64)>,
    connections: Option<(usize, SetConnections)>,
) -> impl IntoElement {
    let label = if current == 0 {
        "∞".to_string()
    } else {
        limit_label(current)
    };
    Button::new(id)
        .ghost()
        .small()
        .label(label)
        .tooltip(tooltip)
        .dropdown_menu(move |mut menu, _, _| {
            menu = menu.label("Speed limit");
            for bps in LIMIT_PRESETS {
                let set = set_limit.clone();
                menu = menu.item(
                    PopupMenuItem::new(limit_label(bps))
                        .checked(bps == current)
                        .on_click(move |_, window, _| {
                            set(bps);
                            window.refresh();
                        }),
                );
            }
            if let Some((n, set_conns)) = &connections {
                menu = menu
                    .separator()
                    .label("Connections (from next start/resume)");
                for c in [1usize, 2, 4, 8, 16, 32] {
                    let set = set_conns.clone();
                    menu = menu.item(
                        PopupMenuItem::new(format!("{c}"))
                            .checked(c == *n)
                            .on_click(move |_, window, _| {
                                set(c);
                                window.refresh();
                            }),
                    );
                }
            }
            menu
        })
}

/// IDM-style bar: one block per segment, filled as far as it has been written,
/// with a tick where each connection started.
fn segment_bar(d: &Download, cx: &App) -> AnyElement {
    let theme = cx.theme();
    let bar = div()
        .relative()
        .h(px(6.))
        .w_full()
        .rounded_full()
        .overflow_hidden()
        .bg(theme.muted);
    let Some(total) = d.total.filter(|t| *t > 0) else {
        // Unknown size: a plain indeterminate strip.
        return bar
            .child(
                div()
                    .absolute()
                    .top_0()
                    .bottom_0()
                    .left_0()
                    .w(relative(0.25))
                    .bg(theme.primary.opacity(0.5)),
            )
            .into_any_element();
    };
    let fill = match d.status {
        Status::Paused => theme.warning,
        Status::Failed(_) => theme.danger,
        _ => theme.primary,
    };
    let t = total as f32;
    if d.segments.is_empty() {
        // yt-dlp / single stream: one plain fill.
        return bar
            .child(
                div()
                    .absolute()
                    .top_0()
                    .bottom_0()
                    .left_0()
                    .w(relative((d.downloaded as f32 / t).clamp(0.0, 1.0)))
                    .bg(fill),
            )
            .into_any_element();
    }
    bar.children(d.segments.iter().flat_map(|s| {
        let left = s.start as f32 / t;
        let width = (s.pos - s.start) as f32 / t;
        [
            div()
                .absolute()
                .top_0()
                .bottom_0()
                .left(relative(left))
                .w(relative(width))
                .bg(if s.owned { fill } else { fill.opacity(0.75) }),
            div()
                .absolute()
                .top_0()
                .bottom_0()
                .left(relative(left))
                .w(px(1.))
                .bg(theme.background.opacity(0.8)),
        ]
    }))
    .into_any_element()
}

impl Render for SurgeView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let active: Vec<&Download> = self
            .downloads
            .iter()
            .filter(|d| d.status.is_active())
            .collect();
        let total_speed: f64 = active.iter().map(|d| d.speed).sum();
        let waiting = self
            .downloads
            .iter()
            .filter(|d| Filter::Waiting.matches(d))
            .count();
        let done_count = self
            .downloads
            .iter()
            .filter(|d| d.status == Status::Completed)
            .count();
        let has_completed = self.downloads.iter().any(|d| d.status == Status::Completed);
        let download_dir = self.engine.settings().download_dir;

        // Newest first.
        let mut list: Vec<Download> = self.downloads.clone();
        list.sort_by_key(|d| std::cmp::Reverse(d.id));
        let any = !list.is_empty();
        list.retain(|d| {
            self.filter.matches(d)
                && (self.query.is_empty()
                    || d.display_name().to_lowercase().contains(&self.query)
                    || d.url.to_lowercase().contains(&self.query))
        });
        let rows: Vec<_> = list.iter().map(|d| self.render_row(d, cx)).collect();

        let folders = self.show_folders.then(|| self.render_folders(cx));
        let theme = cx.theme();
        let body = if let Some(folders) = folders {
            folders
        } else if rows.is_empty() {
            v_flex()
                .flex_1()
                .items_center()
                .justify_center()
                .gap_2()
                .text_color(theme.muted_foreground)
                .child(
                    Icon::new(if any {
                        IconName::Search
                    } else {
                        IconName::Inbox
                    })
                    .size_8(),
                )
                .child(div().text_sm().child(if any {
                    "No downloads match"
                } else {
                    "No downloads yet"
                }))
                .child(div().text_xs().child(if any {
                    "Change the filter or clear the search."
                } else {
                    "Paste a link above, or install the browser extension to catch downloads."
                }))
                .into_any_element()
        } else {
            div()
                .id("downloads")
                .flex_1()
                .overflow_y_scroll()
                .children(rows)
                .into_any_element()
        };

        v_flex()
            .size_full()
            .bg(theme.background)
            .text_color(theme.foreground)
            .child(
                v_flex()
                    .gap_2()
                    .p_3()
                    .border_b_1()
                    .border_color(theme.border)
                    .child(self.render_add_bar(cx))
                    .when(
                        !self.show_folders && (any || !self.query.is_empty()),
                        |el| el.child(self.render_filter_bar(cx)),
                    )
                    .when_some(self.error.clone(), |el, e| {
                        el.child(div().text_xs().text_color(cx.theme().danger).child(e))
                    }),
            )
            .child(body)
            .child(
                h_flex()
                    .px_4()
                    .py_2()
                    .gap_4()
                    .items_center()
                    .border_t_1()
                    .border_color(cx.theme().border)
                    .text_xs()
                    .text_color(cx.theme().muted_foreground)
                    .child(format!(
                        "{} active  ·  {} waiting  ·  {} done  ·  {}",
                        active.len(),
                        waiting,
                        done_count,
                        util::fmt_speed(total_speed)
                    ))
                    .child(
                        div()
                            .id("saving-to")
                            .flex_1()
                            .truncate()
                            .cursor_pointer()
                            .hover(|s| s.underline())
                            .child(format!("Saving to {}", short_path(&download_dir)))
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.show_folders = true;
                                cx.notify();
                            })),
                    )
                    .child({
                        let engine = self.engine.clone();
                        let current = self.engine.settings().speed_limit;
                        limit_menu(
                            "global-limit",
                            current,
                            "Global speed limit (all downloads)",
                            Rc::new(move |bps| {
                                let mut s = engine.settings();
                                s.speed_limit = bps;
                                engine.set_settings(s);
                            }),
                            None,
                        )
                    })
                    .child(self.theme_menu(cx))
                    .when(has_completed, |el| {
                        el.child(
                            Button::new("clear")
                                .ghost()
                                .xsmall()
                                .label("Clear completed")
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.engine.clear_completed();
                                    this.refresh(cx);
                                })),
                        )
                    }),
            )
    }
}

/// `~/Downloads` instead of `/home/me/Downloads`.
fn short_path(p: &std::path::Path) -> String {
    if let Some(home) = dirs::home_dir()
        && let Ok(rest) = p.strip_prefix(&home)
    {
        return if rest.as_os_str().is_empty() {
            "~".into()
        } else {
            format!("~/{}", rest.display())
        };
    }
    p.display().to_string()
}
