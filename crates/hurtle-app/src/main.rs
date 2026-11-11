//! Hurtle desktop app (GPUI + gpui-kit).
//!
//! `hurtle [URL…]` opens the window, or, if Hurtle is already running, hands
//! the URLs to that instance and exits. Magnet links and `.torrent` files
//! (paths or `file://` URLs, as a desktop's file/URL handler passes them)
//! work too.

// No console window behind the app on Windows (release builds).
#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

mod notify;
mod theme;
#[cfg_attr(not(target_os = "linux"), path = "tray_stub.rs")]
mod tray;
mod view;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use gpui_kit::*;
use hurtle_core::{AddRequest, Engine, ipc};

// Icons beyond gpui-kit's default set.
gpui_kit::assets::icon_assets!(ExtraIcons, [TvMinimalPlay, Magnet, Upload, ListVideo, Link]);

struct AppAssets;

impl AssetSource for AppAssets {
    fn load(&self, path: &str) -> Result<Option<std::borrow::Cow<'static, [u8]>>> {
        if let Some(bytes) = ExtraIcons.load(path)? {
            return Ok(Some(bytes));
        }
        gpui_kit::assets::Assets.load(path)
    }
    fn list(&self, path: &str) -> Result<Vec<SharedString>> {
        let mut paths = gpui_kit::assets::Assets.list(path)?;
        paths.extend(ExtraIcons.list(path)?);
        paths.sort();
        paths.dedup();
        Ok(paths)
    }
}

/// A command-line argument as a download: a web link, a magnet link, or a
/// `.torrent` file (sent as data, since the running app may be another user
/// of the socket's view of the disk).
fn request_for(arg: &str) -> Option<AddRequest> {
    if arg.starts_with("http://") || arg.starts_with("https://") || arg.starts_with("magnet:") {
        return Some(AddRequest::new(arg));
    }
    let path = match arg.strip_prefix("file://") {
        Some(p) => std::path::PathBuf::from(hurtle_core::util::percent_decode(p)),
        None => std::path::PathBuf::from(arg),
    };
    if !path
        .extension()
        .is_some_and(|e| e.eq_ignore_ascii_case("torrent"))
    {
        return None;
    }
    let bytes = std::fs::read(&path).ok()?;
    Some(AddRequest {
        torrent_data: Some(hurtle_core::torrent::encode_data(&bytes)),
        ..AddRequest::default()
    })
}

fn main() -> anyhow::Result<()> {
    if std::env::args()
        .nth(1)
        .is_some_and(|a| a == "--version" || a == "-V")
    {
        println!("hurtle {}", hurtle_core::build_info());
        return Ok(());
    }
    let urls: Vec<AddRequest> = std::env::args()
        .skip(1)
        .filter_map(|a| request_for(&a))
        .collect();

    // Single instance: forward to a running Hurtle if there is one.
    if ipc::send(&ipc::IpcRequest::Ping).is_ok() {
        for req in &urls {
            ipc::send(&ipc::IpcRequest::Add(req.clone()))?;
        }
        ipc::send(&ipc::IpcRequest::Show)?;
        return Ok(());
    }

    // Let the browser extension find us (no-op when already registered).
    hurtle_core::nmh::ensure_registered();
    // Offer Hurtle for magnet links and .torrent files (never takes over the default).
    hurtle_core::assoc::ensure_registered();

    let engine = Arc::new(Engine::new()?);
    let flags = Arc::new(tray::Flags {
        show: AtomicBool::new(false),
        quit: AtomicBool::new(false),
    });
    {
        let flag = flags.clone();
        if let Err(e) = engine.serve_ipc(move |req| {
            if matches!(req, ipc::IpcRequest::Show | ipc::IpcRequest::Add(_)) {
                flag.show.store(true, Ordering::Relaxed);
            }
        }) {
            eprintln!("hurtle: IPC disabled: {e:#}");
        }
    }
    // SIGTERM/SIGINT/SIGHUP (logout, `kill`) skip GPUI's quit hook: stop
    // downloads (including yt-dlp children) and save before exiting.
    #[cfg(not(unix))]
    {
        let engine = engine.clone();
        engine.clone().runtime().spawn(async move {
            if tokio::signal::ctrl_c().await.is_ok() {
                engine.shutdown();
                std::process::exit(0);
            }
        });
    }
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        let engine = engine.clone();
        engine.clone().runtime().spawn(async move {
            let (Ok(mut term), Ok(mut int), Ok(mut hup)) = (
                signal(SignalKind::terminate()),
                signal(SignalKind::interrupt()),
                signal(SignalKind::hangup()),
            ) else {
                return;
            };
            tokio::select! {
                _ = term.recv() => {}
                _ = int.recv() => {}
                _ = hup.recv() => {}
            }
            engine.shutdown();
            std::process::exit(0);
        });
    }
    for req in urls {
        if let Err(e) = engine.add(req) {
            eprintln!("hurtle: {e:#}");
        }
    }

    // With a tray icon, closing the window leaves Hurtle running there (as
    // IDM does); "Quit" in the tray menu exits.
    let has_tray = tray::start(&engine, flags.clone());
    notify::start(&engine);

    let app = gpui_kit::application().with_assets(AppAssets);
    // macOS hands over magnet links and opened .torrent files as Apple
    // Events (not arguments), at launch and while running.
    {
        let engine = engine.clone();
        let flags = flags.clone();
        app.on_open_urls(move |urls| {
            for req in urls.iter().filter_map(|u| request_for(u)) {
                if let Err(e) = engine.add(req) {
                    eprintln!("hurtle: {e:#}");
                }
            }
            flags.show.store(true, Ordering::Relaxed);
        });
    }
    app.run(move |cx| {
        gpui_kit::init(cx);
        view::bind_keys(cx);
        // GPUI may exit the process without dropping the engine; save on quit.
        let quit_engine = engine.clone();
        cx.on_app_quit(move |_| {
            quit_engine.shutdown();
            async {}
        })
        .detach();
        if has_tray {
            cx.set_quit_mode(QuitMode::Explicit);
        }
        open_main_window(&engine, !has_tray, cx);
        cx.activate(true);

        // Tray / IPC requests: raise or reopen the window, or quit.
        let engine = engine.clone();
        let flags = flags.clone();
        cx.spawn(async move |cx| {
            let mut tick = 0u32;
            loop {
                cx.background_executor()
                    .timer(Duration::from_millis(200))
                    .await;
                // Follow the desktop's palette when its generator rewrites it.
                tick = tick.wrapping_add(1);
                if tick.is_multiple_of(5) {
                    let setting = engine.settings().theme;
                    cx.update(|cx| theme::poll(&setting, cx));
                }
                if flags.quit.swap(false, Ordering::Relaxed) {
                    cx.update(|cx| cx.quit());
                    break;
                }
                // "Choose a folder" from the browser extension: show the
                // system picker and hand the answer back.
                for pick in engine.take_folder_picks() {
                    let rx = cx.update(|cx| {
                        cx.prompt_for_paths(PathPromptOptions {
                            files: false,
                            directories: true,
                            multiple: false,
                            prompt: Some(pick.title.clone().into()),
                        })
                    });
                    cx.background_executor()
                        .spawn(async move {
                            let dir = rx
                                .await
                                .ok()
                                .and_then(Result::ok)
                                .flatten()
                                .and_then(|v| v.into_iter().next());
                            let _ = pick.reply.send(dir);
                        })
                        .detach();
                }
                if flags.show.swap(false, Ordering::Relaxed) {
                    cx.update(|cx| match cx.windows().first() {
                        Some(w) => {
                            let _ = w.update(cx, |_, window, _| window.activate_window());
                        }
                        None => open_main_window(&engine, !has_tray, cx),
                    });
                }
            }
        })
        .detach();
    });
    Ok(())
}

fn open_main_window(engine: &Arc<Engine>, ask_before_close: bool, cx: &mut App) {
    let bounds = Bounds::centered(None, size(px(860.), px(640.)), cx);
    let options = WindowOptions {
        window_bounds: Some(WindowBounds::Windowed(bounds)),
        window_min_size: Some(size(px(560.), px(360.))),
        titlebar: Some(TitlebarOptions {
            title: Some("Hurtle".into()),
            ..Default::default()
        }),
        // Tests and dev runs set HURTLE_APP_ID so the window manager can keep
        // their windows away from the user's (e.g. on another monitor).
        app_id: Some(std::env::var("HURTLE_APP_ID").unwrap_or_else(|_| "hurtle".into())),
        ..Default::default()
    };
    let engine = engine.clone();
    if let Err(e) = gpui_kit::open_window(options, cx, move |window, cx| {
        theme::apply(&engine.settings().theme, Some(window), cx);
        if ask_before_close {
            // No tray: closing the window quits and pauses everything.
            let engine = engine.clone();
            window.on_window_should_close(cx, move |window, cx| {
                let active = engine
                    .snapshot()
                    .iter()
                    .filter(|d| d.status.is_active())
                    .count();
                if active == 0 {
                    return true;
                }
                let rx = window.prompt(
                    PromptLevel::Warning,
                    &format!(
                        "{active} download{} in progress",
                        if active == 1 { "" } else { "s" }
                    ),
                    Some("Quitting pauses them. You can resume them next time."),
                    &["Quit", "Keep open"],
                    cx,
                );
                cx.spawn(async move |cx| {
                    if rx.await == Ok(0) {
                        cx.update(|cx| cx.quit());
                    }
                })
                .detach();
                false
            });
        }
        cx.new(|cx| view::HurtleView::new(engine, window, cx))
    }) {
        eprintln!("hurtle: could not open window: {e:#}");
    }
}
