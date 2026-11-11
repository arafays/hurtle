use std::collections::HashSet;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use gpui_kit::assets::IconName as Lucide;
use gpui_kit::component::button::{Button, ButtonVariants};
use gpui_kit::component::checkbox::Checkbox;
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::menu::{DropdownMenu, PopupMenuItem};
use gpui_kit::component::{
    ActiveTheme, Disableable, Icon, IconName, Selectable, Sizable, StyledExt, h_flex, v_flex,
};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use hurtle_core::{AddRequest, Download, Engine, Kind, Status, assoc, player, util};

actions!(hurtle, [FocusSearch, Dismiss, PasteLink]);

/// Window shortcuts. The URL box and search box handle their own copy/paste
/// when focused; these cover the rest of the window.
pub fn bind_keys(cx: &mut App) {
    let (find, paste) = if cfg!(target_os = "macos") {
        ("cmd-f", "cmd-v")
    } else {
        ("ctrl-f", "ctrl-v")
    };
    cx.bind_keys([
        KeyBinding::new(find, FocusSearch, Some("HurtleView")),
        KeyBinding::new("escape", Dismiss, Some("HurtleView")),
        KeyBinding::new(paste, PasteLink, Some("HurtleView")),
    ]);
}

/// Page for people who don't have the browser extension yet.
const EXTENSION_URL: &str = "https://github.com/arafays/hurtle/releases/latest";

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

pub struct HurtleView {
    engine: Arc<Engine>,
    /// Keeps keyboard shortcuts working when focus would otherwise fall out of the view.
    focus: FocusHandle,
    url_input: Entity<InputState>,
    search_input: Entity<InputState>,
    /// Lower-cased search text.
    query: String,
    filter: Filter,
    /// Rows whose connection/detail panel is open.
    expanded: HashSet<u64>,
    /// HURTLE_VIEW=expanded opens every row's details (screenshots).
    expanded_all: bool,
    connections: usize,
    downloads: Vec<Download>,
    /// Cheap fingerprint of `downloads`; we only re-render when it changes.
    fingerprint: u64,
    error: Option<SharedString>,
    /// The "Download folders" panel is open instead of the list.
    show_folders: bool,
    /// A row asking "Remove?": (id, also delete the finished file).
    confirm: Option<(u64, bool)>,
    /// Folders as last rendered (they can change from the extension).
    folders_fp: u64,
    /// Whether Hurtle opens magnet links / .torrent files (asked when the
    /// settings panel opens, since it runs xdg-mime / reg).
    assoc: std::cell::Cell<Option<assoc::Status>>,
    /// What "Make default" did.
    assoc_note: Option<SharedString>,
    _poll: Task<()>,
}

impl HurtleView {
    pub fn new(engine: Arc<Engine>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let url_input = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("Paste a download link or magnet link and press Enter…")
        });
        cx.subscribe_in(
            &url_input,
            window,
            |this, _, ev: &InputEvent, window, cx| match ev {
                InputEvent::PressEnter { .. } => this.submit(window, cx),
                // A stale error would otherwise stay until the next submit.
                InputEvent::Change if this.error.take().is_some() => cx.notify(),
                _ => {}
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
            focus: cx.focus_handle(),
            url_input,
            search_input,
            query: String::new(),
            filter: Filter::All,
            expanded: HashSet::new(),
            expanded_all: std::env::var("HURTLE_VIEW").is_ok_and(|v| v == "expanded"),
            connections,
            downloads: vec![],
            fingerprint: 0,
            error: None,
            // HURTLE_VIEW=folders opens on the folders panel (screenshots, tests).
            show_folders: std::env::var("HURTLE_VIEW").is_ok_and(|v| v == "folders"),
            confirm: None,
            folders_fp: 0,
            assoc: std::cell::Cell::new(None),
            assoc_note: None,
            _poll: poll,
        };
        this.refresh(cx);
        let url_focus = this.url_input.read(cx).focus_handle(cx);
        window.focus(&url_focus, cx);
        this
    }

    fn refresh(&mut self, cx: &mut Context<Self>) {
        let downloads = self.engine.snapshot();
        let fp = fingerprint(&downloads);
        if fp != self.fingerprint {
            self.fingerprint = fp;
            self.downloads = downloads;
            if self
                .confirm
                .is_some_and(|(id, _)| !self.downloads.iter().any(|d| d.id == id))
            {
                self.confirm = None;
            }
            cx.notify();
        }
        let settings = self.engine.settings();
        let folders_fp = {
            use std::hash::{Hash, Hasher};
            let mut h = std::collections::hash_map::DefaultHasher::new();
            settings.download_dir.hash(&mut h);
            settings.type_dirs.hash(&mut h);
            settings.connections.hash(&mut h);
            settings.auto_retry.hash(&mut h);
            settings.notify.hash(&mut h);
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
        panel = panel.child(
            v_flex()
                .px_4()
                .pt_4()
                .pb_2()
                .child(div().text_base().font_semibold().child("Behavior")),
        );
        for (id, label, on) in [
            (
                "retry",
                "Retry failed downloads automatically",
                s.auto_retry,
            ),
            (
                "notify",
                "Notify when a download finishes or fails",
                s.notify,
            ),
        ] {
            panel = panel.child(
                h_flex()
                    .gap_3()
                    .px_4()
                    .py_2()
                    .items_center()
                    .border_b_1()
                    .border_color(cx.theme().border)
                    .child(div().flex_1().text_sm().child(label))
                    .child(
                        Button::new(SharedString::from(format!("toggle-{id}")))
                            .outline()
                            .xsmall()
                            .selected(on)
                            .label(if on { "On" } else { "Off" })
                            .on_click(cx.listener(move |this, _, _, cx| {
                                let mut s = this.engine.settings();
                                match id {
                                    "retry" => s.auto_retry = !s.auto_retry,
                                    _ => s.notify = !s.notify,
                                }
                                this.engine.set_settings(s);
                                this.refresh(cx);
                            })),
                    ),
            );
        }
        let border = cx.theme().border;
        let muted = cx.theme().muted_foreground;
        let data_dir = hurtle_core::data_dir();
        panel
            .child(self.render_torrent_settings(&s, cx))
            .child(
                v_flex()
                    .px_4()
                    .pt_4()
                    .pb_2()
                    .child(div().text_base().font_semibold().child("About")),
            )
            .child(
                h_flex()
                    .gap_3()
                    .px_4()
                    .py_2()
                    .items_center()
                    .border_b_1()
                    .border_color(border)
                    .child(
                        v_flex()
                            .flex_1()
                            .min_w_0()
                            .child(
                                div()
                                    .text_sm()
                                    .child(format!("Hurtle {}", hurtle_core::build_info())),
                            )
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(muted)
                                    .truncate()
                                    .child(format!("Data and logs in {}", short_path(&data_dir))),
                            ),
                    )
                    .child(
                        Button::new("copy-version")
                            .outline()
                            .xsmall()
                            .label("Copy")
                            .on_click(|_, _, cx| {
                                cx.write_to_clipboard(ClipboardItem::new_string(format!(
                                    "Hurtle {} on {} {}",
                                    hurtle_core::build_info(),
                                    std::env::consts::OS,
                                    std::env::consts::ARCH
                                )));
                            }),
                    ),
            )
            .into_any_element()
    }

    /// BitTorrent and streaming settings.
    fn render_torrent_settings(
        &self,
        s: &hurtle_core::Settings,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let border = cx.theme().border;
        let muted = cx.theme().muted_foreground;
        let line = |label: &str, hint: &str| {
            h_flex()
                .gap_3()
                .px_4()
                .py_2()
                .items_center()
                .border_b_1()
                .border_color(border)
                .child(
                    v_flex()
                        .flex_1()
                        .min_w_0()
                        .child(div().text_sm().child(label.to_string()))
                        .when(!hint.is_empty(), |el| {
                            el.child(div().text_xs().text_color(muted).child(hint.to_string()))
                        }),
                )
        };
        let mut panel = v_flex().child(
            v_flex().px_4().pt_4().pb_2().child(
                div()
                    .text_base()
                    .font_semibold()
                    .child("BitTorrent and streaming"),
            ),
        );

        // Seeding.
        let ratio = s.seed_ratio;
        let engine = self.engine.clone();
        panel = panel.child(
            line(
                "Seed finished torrents",
                "Keep uploading until you have shared this much of what you downloaded",
            )
            .child(
                Button::new("seed-ratio")
                    .outline()
                    .xsmall()
                    .label(if ratio <= 0.0 {
                        "Don't seed".to_string()
                    } else {
                        format!("Ratio {ratio}")
                    })
                    .dropdown_menu(move |mut menu, _, _| {
                        for r in [0.0f32, 0.5, 1.0, 2.0, 5.0] {
                            let engine = engine.clone();
                            menu = menu.item(
                                PopupMenuItem::new(if r == 0.0 {
                                    "Don't seed".to_string()
                                } else {
                                    format!("Ratio {r}")
                                })
                                .checked((r - ratio).abs() < 0.01)
                                .on_click(move |_, window, _| {
                                    let mut s = engine.settings();
                                    s.seed_ratio = r;
                                    engine.set_settings(s);
                                    window.refresh();
                                }),
                            );
                        }
                        menu
                    }),
            ),
        );
        let engine = self.engine.clone();
        panel = panel.child(
            line("Upload limit", "For all torrents together").child(limit_menu(
                "upload-limit",
                s.upload_limit,
                "Upload speed limit",
                Rc::new(move |bps| {
                    let mut s = engine.settings();
                    s.upload_limit = bps;
                    engine.set_settings(s);
                }),
                None,
            )),
        );

        // Player.
        let players = player::detect();
        let current = player::choose(&s.player);
        let engine = self.engine.clone();
        let chosen = s.player.clone();
        panel = panel.child(
            line(
                "Player for streams",
                if players.is_empty() {
                    "No player found (VLC, mpv…): streams open in the default app"
                } else {
                    "Opens while the torrent downloads; seeking fetches that part first"
                },
            )
            .child(
                Button::new("player")
                    .outline()
                    .xsmall()
                    .icon(Lucide::TvMinimalPlay)
                    .label(
                        current
                            .map(|p| p.name)
                            .unwrap_or_else(|| "Default app".into()),
                    )
                    .dropdown_menu(move |mut menu, _, _| {
                        let auto = engine.clone();
                        menu = menu.item(
                            PopupMenuItem::new("Automatic (VLC first)")
                                .checked(chosen.is_empty())
                                .on_click(move |_, window, _| {
                                    let mut s = auto.settings();
                                    s.player.clear();
                                    auto.set_settings(s);
                                    window.refresh();
                                }),
                        );
                        for p in &players {
                            let engine = engine.clone();
                            let pid = p.id.clone();
                            menu = menu.item(
                                PopupMenuItem::new(p.name.clone())
                                    .checked(chosen == p.id)
                                    .on_click(move |_, window, _| {
                                        let mut s = engine.settings();
                                        s.player = pid.clone();
                                        engine.set_settings(s);
                                        window.refresh();
                                    }),
                            );
                        }
                        menu
                    }),
            ),
        );

        for (id, label, hint, on) in [
            (
                "lan",
                "Share streams on my network",
                "Stream URLs work on a TV or phone on this network (VLC → Open Network Stream). Applies after a restart.",
                s.stream_lan,
            ),
            (
                "dht",
                "Find peers through DHT",
                "Needed for most magnet links. Applies after a restart.",
                s.torrent_dht,
            ),
            (
                "upnp",
                "Open the port on the router (UPnP)",
                "More peers can connect to you. Applies after a restart.",
                s.torrent_upnp,
            ),
        ] {
            panel = panel.child(
                line(label, hint).child(
                    Button::new(SharedString::from(format!("toggle-{id}")))
                        .outline()
                        .xsmall()
                        .selected(on)
                        .label(if on { "On" } else { "Off" })
                        .on_click(cx.listener(move |this, _, _, cx| {
                            let mut s = this.engine.settings();
                            match id {
                                "lan" => s.stream_lan = !s.stream_lan,
                                "dht" => s.torrent_dht = !s.torrent_dht,
                                _ => s.torrent_upnp = !s.torrent_upnp,
                            }
                            this.engine.set_settings(s);
                            this.refresh(cx);
                            cx.notify();
                        })),
                ),
            );
        }
        // The system's app for magnet links and .torrent files.
        let st = match self.assoc.get() {
            Some(st) => st,
            None => {
                let st = assoc::status();
                self.assoc.set(Some(st));
                st
            }
        };
        let hint: String = match (&self.assoc_note, st.is_default(), st.magnet, st.torrent) {
            (Some(note), ..) => note.to_string(),
            (None, true, ..) => {
                "Hurtle opens them: browsers and file managers hand them over".into()
            }
            (None, false, true, false) => {
                "Hurtle opens magnet links; .torrent files open in another app".into()
            }
            (None, false, false, true) => {
                "Hurtle opens .torrent files; magnet links open in another app".into()
            }
            (None, false, ..) if st.known => {
                "Hurtle is offered in \"Open with\", but another app is the default".into()
            }
            _ => "Not registered with the system yet".into(),
        };
        panel = panel.child(
            line("Default app for magnet links and .torrent files", &hint).child(
                Button::new("make-default")
                    .outline()
                    .xsmall()
                    .icon(Lucide::Magnet)
                    .selected(st.is_default())
                    .label(if st.is_default() {
                        "Default"
                    } else {
                        "Make default"
                    })
                    .on_click(cx.listener(|this, _, _, cx| {
                        let r = assoc::make_default();
                        this.assoc_note = Some(match r {
                            Ok(m) => m.into(),
                            Err(e) => format!("Couldn't: {e}").into(),
                        });
                        this.assoc.set(None);
                        cx.notify();
                    })),
            ),
        );
        panel = panel.child(line(
            "Listening port",
            &format!(
                "{} (TCP and uTP; a taken port falls back to any free one)",
                s.torrent_port
            ),
        ));
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
                errors.push(format!("{}: {e}", shorten(url, 48)));
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

    /// Add the links on the clipboard (if it holds any) as downloads.
    fn paste_link(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let text = cx
            .read_from_clipboard()
            .and_then(|c| c.text())
            .unwrap_or_default();
        let links: Vec<&str> = text
            .split_whitespace()
            .filter(|t| {
                t.starts_with("http://") || t.starts_with("https://") || t.starts_with("magnet:")
            })
            .collect();
        if links.is_empty() {
            self.error = Some("The clipboard has no link (http://, https:// or magnet:).".into());
            cx.notify();
            return;
        }
        let joined = links.join(" ");
        self.url_input
            .update(cx, |input, cx| input.set_value(joined, window, cx));
        self.submit(window, cx);
    }

    /// Pick `.torrent` files with the system picker and add them.
    fn open_torrents(&mut self, cx: &mut Context<Self>) {
        let rx = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: true,
            prompt: Some("Download".into()),
        });
        let engine = self.engine.clone();
        cx.spawn(async move |this, cx| {
            let Ok(Ok(Some(paths))) = rx.await else {
                return;
            };
            let errors: Vec<String> = paths
                .iter()
                .filter_map(|p| {
                    engine
                        .add(AddRequest::new(p.to_string_lossy()))
                        .err()
                        .map(|e| format!("{}: {e}", p.display()))
                })
                .collect();
            let _ = this.update(cx, |this, cx| {
                this.error = (!errors.is_empty()).then(|| errors.join("\n").into());
                this.refresh(cx);
                cx.notify();
            });
        })
        .detach();
    }

    /// Stream a file (None = the main video) into the player.
    fn play(&mut self, id: u64, file: Option<usize>, cx: &mut Context<Self>) {
        if let Err(e) = self.engine.play(id, file) {
            self.error = Some(format!("Can't stream: {e}").into());
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
                            .tooltip("Fewer connections per file")
                            .disabled(n <= 1)
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.set_connections(n.saturating_sub(1), cx)
                            })),
                    )
                    .child(
                        div()
                            .min_w(px(88.))
                            .text_center()
                            .text_sm()
                            .child(format!("{n} conn{} / file", if n == 1 { "" } else { "s" })),
                    )
                    .child(
                        Button::new("conn-inc")
                            .ghost()
                            .xsmall()
                            .icon(IconName::Plus)
                            .tooltip("More connections per file")
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
                Button::new("open-torrent")
                    .ghost()
                    .icon(Lucide::Magnet)
                    .tooltip("Open .torrent files")
                    .on_click(cx.listener(|this, _, _, cx| this.open_torrents(cx))),
            )
            .child(
                Button::new("folders")
                    .ghost()
                    .icon(IconName::Folder)
                    .selected(self.show_folders)
                    .tooltip("Folders and settings")
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.show_folders = !this.show_folders;
                        this.assoc.set(None);
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
        let torrent_panel = d
            .torrent
            .as_ref()
            .filter(|_| d.kind == Kind::Torrent)
            .map(|t| self.render_torrent(d, t, cx));
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
        if let Some(page) = d.page_url.clone().filter(|p| *p != d.url) {
            let open = page.clone();
            panel = panel.child(
                h_flex()
                    .gap_1()
                    .items_center()
                    .child(div().flex_1().min_w_0().child(field("Page", page)))
                    .child(
                        Button::new(("page", id))
                            .ghost()
                            .xsmall()
                            .icon(IconName::Globe)
                            .label("Open page")
                            .tooltip("Where this was found. Play the video or click the link there again: a download of the same size picks up the new link and resumes.")
                            .on_click(move |_, _, cx| cx.open_url(&open)),
                    ),
            );
        }

        if let Some(p) = torrent_panel {
            panel = panel.child(p);
        }

        let live: Vec<(usize, &hurtle_core::Segment)> = d
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

    /// A torrent's swarm numbers, stream links and file list (with choice).
    fn render_torrent(
        &self,
        d: &Download,
        t: &hurtle_core::torrent::TorrentState,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = cx.theme();
        let (muted, track, fill) = (theme.muted_foreground, theme.muted, theme.primary);
        let id = d.id;
        let size = d.total.unwrap_or(0);
        let field = |label: &'static str, value: String| {
            h_flex()
                .gap_2()
                .text_xs()
                .child(div().w(px(56.)).text_color(muted).child(label))
                .child(div().flex_1().min_w_0().truncate().child(value))
        };
        let mut panel = v_flex().gap_1();
        if !t.info_hash.is_empty() {
            panel = panel.child(field("Hash", t.info_hash.clone()));
        }
        let mut swarm = format!(
            "{} peer{} ({} seed{})  ·  uploaded {}  ·  ratio {:.2}",
            t.peers,
            if t.peers == 1 { "" } else { "s" },
            t.seeds,
            if t.seeds == 1 { "" } else { "s" },
            util::fmt_bytes(t.uploaded),
            t.ratio(size)
        );
        if t.upload_speed > 0 {
            swarm.push_str(&format!(
                "  ·  ↑ {}",
                util::fmt_speed(t.upload_speed as f64)
            ));
        }
        panel = panel.child(field("Swarm", swarm));

        // Stream links: for VLC's "Open Network Stream", or another device.
        let has_video = t.main_playable().is_some();
        if has_video {
            let player = player::choose(&self.engine.settings().player)
                .map(|p| p.name)
                .unwrap_or_else(|| "the default app".into());
            let (e1, e2) = (self.engine.clone(), self.engine.clone());
            panel = panel.child(
                h_flex()
                    .gap_1()
                    .items_center()
                    .child(div().w(px(56.)).text_xs().text_color(muted).child("Stream"))
                    .child(
                        Button::new(("play-main", id))
                            .outline()
                            .xsmall()
                            .icon(Lucide::TvMinimalPlay)
                            .label(format!("Play in {player}"))
                            .on_click(cx.listener(move |this, _, _, cx| this.play(id, None, cx))),
                    )
                    .child(
                        Button::new(("copy-stream", id))
                            .ghost()
                            .xsmall()
                            .icon(Lucide::Link)
                            .label("Copy stream URL")
                            .tooltip("For VLC's Open Network Stream (Ctrl+N), or any player")
                            .on_click(cx.listener(move |this, _, _, cx| {
                                match e1.stream_url(id, None) {
                                    Ok(u) => cx.write_to_clipboard(ClipboardItem::new_string(u)),
                                    Err(e) => this.error = Some(format!("{e}").into()),
                                }
                                cx.notify();
                            })),
                    )
                    .child(
                        Button::new(("copy-m3u", id))
                            .ghost()
                            .xsmall()
                            .icon(Lucide::ListVideo)
                            .label("Copy playlist URL")
                            .tooltip(
                                "Every video and audio file of this torrent, as an .m3u playlist",
                            )
                            .on_click(cx.listener(move |this, _, _, cx| {
                                match e2.playlist_url(id) {
                                    Ok(u) => cx.write_to_clipboard(ClipboardItem::new_string(u)),
                                    Err(e) => this.error = Some(format!("{e}").into()),
                                }
                                cx.notify();
                            })),
                    ),
            );
        }

        let shown: Vec<(usize, &hurtle_core::torrent::TorrentFile)> = t
            .files
            .iter()
            .enumerate()
            .filter(|(_, f)| !f.padding)
            .collect();
        if shown.is_empty() {
            return panel
                .child(field(
                    "Files",
                    "waiting for the file list from peers…".into(),
                ))
                .into_any_element();
        }
        let selected = shown.iter().filter(|(_, f)| f.selected).count();
        panel = panel.child(div().pt_1().text_xs().font_semibold().child(format!(
            "{} file{}, {selected} selected",
            shown.len(),
            if shown.len() == 1 { "" } else { "s" }
        )));
        const MAX_ROWS: usize = 300;
        for (i, f) in shown.iter().take(MAX_ROWS) {
            let i = *i;
            let frac = if f.size == 0 {
                1.0
            } else {
                (f.done as f32 / f.size as f32).clamp(0.0, 1.0)
            };
            let engine = self.engine.clone();
            let current: Vec<usize> = t.selected();
            let only_one = f.selected && selected == 1;
            let row = h_flex()
                .gap_2()
                .items_center()
                .text_xs()
                .child(
                    Checkbox::new(("tfile", id * 100_000 + i as u64))
                        .checked(f.selected)
                        .disabled(only_one)
                        .on_click(move |on, _, _| {
                            let mut sel = current.clone();
                            if *on {
                                sel.push(i);
                            } else {
                                sel.retain(|x| *x != i);
                            }
                            let _ = engine.set_torrent_files(id, &sel);
                        }),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .truncate()
                        .when(!f.selected, |el| el.text_color(muted))
                        .child(f.path.clone()),
                )
                .child(
                    div()
                        .w(px(64.))
                        .h(px(4.))
                        .rounded_full()
                        .overflow_hidden()
                        .bg(track)
                        .child(div().h_full().w(relative(frac)).bg(fill)),
                )
                .child(
                    div()
                        .w(px(72.))
                        .text_right()
                        .text_color(muted)
                        .child(util::fmt_bytes(f.size)),
                )
                .child(div().w(px(24.)).when(player::is_playable(&f.path), |el| {
                    el.child(
                        Button::new(("tplay", id * 100_000 + i as u64))
                            .ghost()
                            .xsmall()
                            .icon(Lucide::TvMinimalPlay)
                            .tooltip("Stream this file")
                            .on_click(
                                cx.listener(move |this, _, _, cx| this.play(id, Some(i), cx)),
                            ),
                    )
                }));
            panel = panel.child(row);
        }
        if shown.len() > MAX_ROWS {
            panel = panel.child(
                div()
                    .text_xs()
                    .text_color(muted)
                    .child(format!("+ {} more files", shown.len() - MAX_ROWS)),
            );
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
        let torrent = d.torrent.as_ref().filter(|_| d.kind == Kind::Torrent);
        let detail: SharedString = match &d.status {
            Status::Downloading if torrent.is_some() => {
                let mut s = format!(
                    "{} of {}",
                    util::fmt_bytes(d.downloaded),
                    d.total.map(util::fmt_bytes).unwrap_or_else(|| "?".into()),
                );
                if let Some(note) = &d.note {
                    s.push_str(&format!("  ·  {note}"));
                }
                s.into()
            }
            Status::Completed if torrent.is_some_and(|t| t.seeding) => format!(
                "{}  ·  {}",
                util::fmt_bytes(d.total.unwrap_or(d.downloaded)),
                d.note.as_deref().unwrap_or("Seeding")
            )
            .into(),
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
        let is_torrent = torrent.is_some();
        let mut actions = h_flex().gap_1().when(!done, |el| {
            el.child(limit_menu(
                ("limit", id),
                limit,
                if is_torrent {
                    "Download speed limit for this torrent"
                } else {
                    "Speed limit and connections for this file"
                },
                Rc::new(move |bps| limit_engine.set_speed_limit(id, bps)),
                (!is_torrent).then(|| {
                    (
                        conns,
                        Rc::new(move |n| engine.set_connections(id, n)) as SetConnections,
                    )
                }),
            ))
        });
        // Watch it while it downloads (torrents), or play the finished file.
        let playable = match torrent {
            Some(t) => t.files.is_empty() || t.main_playable().is_some(),
            None => done && player::is_playable(&d.filename),
        };
        if playable && !matches!(d.status, Status::Failed(_)) {
            actions = actions.child(
                Button::new(("play", id))
                    .ghost()
                    .small()
                    .icon(Lucide::TvMinimalPlay)
                    .tooltip(if done {
                        "Play in VLC (or your player)"
                    } else {
                        "Stream into VLC (or your player) while it downloads"
                    })
                    .on_click(cx.listener(move |this, _, _, cx| this.play(id, None, cx))),
            );
        }
        if let Some(t) = torrent.filter(|_| done) {
            let seeding = t.seeding;
            actions = actions.child(
                Button::new(("seed", id))
                    .ghost()
                    .small()
                    .icon(Lucide::Upload)
                    .selected(seeding)
                    .tooltip(if seeding {
                        "Stop seeding"
                    } else {
                        "Seed (share with other peers)"
                    })
                    .on_click(cx.listener(move |this, _, _, cx| {
                        if seeding {
                            this.engine.pause(id);
                        } else if let Err(e) = this.engine.seed(id) {
                            this.error = Some(format!("Can't seed: {e}").into());
                        }
                        this.refresh(cx);
                        cx.notify();
                    })),
            );
        }
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
                    .tooltip(if matches!(d.status, Status::Failed(_)) {
                        "Retry"
                    } else if d.resumable || d.downloaded == 0 {
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
        // An expired link: the page it came from hands out a fresh one, and
        // capturing it again resumes this download (`Engine::add_or_relink`).
        if let Some(page) = d.page_url.clone().filter(|_| can_resume) {
            actions = actions.child(
                Button::new(("pagerow", id))
                    .ghost()
                    .small()
                    .icon(IconName::Globe)
                    .tooltip("Open the source page for a fresh link (resumes here)")
                    .on_click(move |_, _, cx| cx.open_url(&page)),
            );
        }
        if done {
            let file = path.clone();
            let open_dir = dir.clone();
            actions = actions.child(
                Button::new(("open", id))
                    .ghost()
                    .small()
                    .icon(IconName::ExternalLink)
                    .tooltip("Open file")
                    .on_click(cx.listener(move |this, _, _, cx| match &file {
                        Some(p) if p.exists() => cx.open_with_system(p),
                        missing => {
                            // Moved or deleted: say so and show the folder.
                            let name = missing
                                .as_ref()
                                .and_then(|p| p.file_name())
                                .map(|n| n.to_string_lossy().into_owned())
                                .unwrap_or_default();
                            this.error = Some(format!("{name} was moved or deleted.").into());
                            cx.open_with_system(&open_dir);
                            cx.notify();
                        }
                    })),
            );
        }
        let copy = hurtle_core::torrent::magnet_link(d).unwrap_or_else(|| d.url.clone());
        actions = actions
            .child(
                Button::new(("copyrow", id))
                    .ghost()
                    .small()
                    .icon(if is_torrent {
                        Lucide::Magnet.into()
                    } else {
                        Icon::new(IconName::Copy)
                    })
                    .tooltip(if is_torrent {
                        "Copy magnet link"
                    } else {
                        "Copy link"
                    })
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
            );
        if done {
            actions = actions.child(
                Button::new(("trash", id))
                    .ghost()
                    .small()
                    .icon(IconName::Delete)
                    .tooltip("Delete the file from disk")
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.confirm = Some((id, true));
                        cx.notify();
                    })),
            );
        }
        // Partial progress is lost on remove, so ask first when there is some.
        let risky = !done && d.downloaded > 0;
        actions = actions.child(
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
                    if risky {
                        this.confirm = Some((id, false));
                    } else {
                        this.engine.remove(id, false);
                    }
                    this.refresh(cx);
                    cx.notify();
                })),
        );
        if let Some((_, delete)) = self.confirm.filter(|(cid, _)| *cid == id) {
            actions = h_flex()
                .gap_1()
                .items_center()
                .child(
                    div()
                        .text_xs()
                        .text_color(theme.muted_foreground)
                        .child(if delete {
                            "Delete the file?"
                        } else {
                            "Discard the download?"
                        }),
                )
                .child(
                    Button::new(("rm-yes", id))
                        .danger()
                        .xsmall()
                        .label(if delete { "Delete" } else { "Remove" })
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.confirm = None;
                            this.engine.remove(id, delete);
                            this.refresh(cx);
                            cx.notify();
                        })),
                )
                .child(
                    Button::new(("rm-no", id))
                        .ghost()
                        .xsmall()
                        .label("Keep")
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.confirm = None;
                            cx.notify();
                        })),
                );
        }

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
                                    // A failure is the thing to read: let it wrap.
                                    .when(!matches!(d.status, Status::Failed(_)), |el| {
                                        el.truncate()
                                    })
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

impl Render for HurtleView {
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
                    "Paste a link above, or let the browser extension hand downloads over. Video page links work too."
                }))
                .when(!any, |el| {
                    el.child(
                        h_flex()
                            .pt_2()
                            .gap_2()
                            .child(
                                Button::new("empty-paste")
                                    .primary()
                                    .small()
                                    .icon(IconName::ArrowDown)
                                    .label("Paste link")
                                    .tooltip("Download the link on the clipboard")
                                    .on_click(
                                        cx.listener(|this, _, window, cx| this.paste_link(window, cx)),
                                    ),
                            )
                            .child(
                                Button::new("empty-ext")
                                    .outline()
                                    .small()
                                    .icon(IconName::Globe)
                                    .label("Get the browser extension")
                                    .on_click(|_, _, cx| cx.open_url(EXTENSION_URL)),
                            ),
                    )
                })
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
            .key_context("HurtleView")
            .track_focus(&self.focus)
            .on_action(cx.listener(|this, _: &FocusSearch, window, cx| {
                let handle = this.search_input.read(cx).focus_handle(cx);
                window.focus(&handle, cx);
            }))
            .on_action(cx.listener(|this, _: &PasteLink, window, cx| this.paste_link(window, cx)))
            .on_action(cx.listener(|this, _: &Dismiss, window, cx| {
                if this.show_folders {
                    this.show_folders = false;
                } else if this.confirm.take().is_none() && !this.query.is_empty() {
                    this.search_input
                        .update(cx, |input, cx| input.set_value("", window, cx));
                }
                this.error = None;
                // The confirm row's button may have held focus.
                let inner = this.url_input.read(cx).focus_handle(cx).is_focused(window)
                    || this
                        .search_input
                        .read(cx)
                        .focus_handle(cx)
                        .is_focused(window);
                if !inner {
                    window.focus(&this.focus, cx);
                }
                cx.notify();
            }))
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
                                this.assoc.set(None);
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
                    .child(
                        div()
                            .id("version")
                            .flex_none()
                            .cursor_pointer()
                            .hover(|s| s.underline())
                            .child(format!("v{}", hurtle_core::VERSION))
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.show_folders = true;
                                this.assoc.set(None);
                                cx.notify();
                            })),
                    )
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

pub(crate) fn fingerprint(downloads: &[Download]) -> u64 {
    downloads.iter().fold(downloads.len() as u64, |acc, d| {
        // Queued and Paused have labels of the same length, so hash the
        // state itself (and the whole error, not just its length).
        let status = match &d.status {
            Status::Queued => 1,
            Status::Probing => 2,
            Status::Downloading => 3,
            Status::Paused => 4,
            Status::Completed => 5,
            Status::Failed(e) => e
                .bytes()
                .fold(6u64, |a, b| a.wrapping_mul(31).wrapping_add(b as u64)),
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
            // A fresh link (relink) or a newly known page changes the details.
            .wrapping_add(
                [
                    &d.url,
                    d.final_url.as_deref().unwrap_or(""),
                    d.page_url.as_deref().unwrap_or(""),
                ]
                .iter()
                .flat_map(|s| s.bytes())
                .fold(3u64, |a, b| a.wrapping_mul(31).wrapping_add(b as u64))
                .rotate_left(23),
            )
            .wrapping_add(d.torrent.as_ref().map_or(0, |t| {
                // Swarm numbers, seeding, and each file's choice and progress.
                t.files.iter().fold(
                    (t.peers as u64) << 1
                        ^ (t.seeds as u64) << 12
                        ^ t.upload_speed >> 10 << 24
                        ^ t.uploaded >> 16
                        ^ (t.seeding as u64) << 62
                        ^ t.info_hash.len() as u64,
                    |a, f| {
                        a.wrapping_mul(13)
                            .wrapping_add((f.done >> 14) ^ (f.selected as u64) << 61)
                    },
                )
            }))
            .wrapping_add(d.segments.iter().fold(0u64, |a, s| {
                // Per-connection speed (to 4 KiB/s) and stalls change the detail panel.
                let stalled = s.last_data.is_some_and(|t| t.elapsed().as_secs() >= 3);
                a.wrapping_mul(7)
                    .wrapping_add((s.speed >> 12) ^ (s.owned as u64) << 3 ^ (stalled as u64) << 4)
            }))
    })
}

/// Keep error lines short: `https://example.com/very/long…`.
fn shorten(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        format!("{}…", s.chars().take(max).collect::<String>())
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

#[cfg(test)]
mod tests {
    use super::{Download, Status, fingerprint};

    fn download(status: Status) -> Download {
        let mut d: Download = serde_json::from_str(
            r#"{"id":1,"url":"http://x/f","final_url":null,"headers":[],"filename":"f",
            "dir":"/tmp","path":null,"total":null,"resumable":true,"etag":null,
            "last_modified":null,"segments":[],"downloaded":0,"status":{"state":"queued"},
            "connections":4,"added_at":0,"finished_at":null}"#,
        )
        .expect("download json");
        d.status = status;
        d
    }

    #[test]
    fn fingerprint_tells_states_apart() {
        let fp = |s| fingerprint(&[download(s)]);
        assert_ne!(fp(Status::Queued), fp(Status::Paused));
        assert_ne!(
            fp(Status::Failed("server said no".into())),
            fp(Status::Failed("server said ok!".into()))
        );
    }
}
