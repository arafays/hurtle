//! Hurtle desktop app (GPUI + gpui-kit).
//!
//! `hurtle [URL…]` opens the window, or, if Hurtle is already running, hands
//! the URLs to that instance and exits.

// No console window behind the app on Windows (release builds).
#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

mod theme;
#[cfg_attr(not(target_os = "linux"), path = "tray_stub.rs")]
mod tray;
mod view;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use gpui_kit::*;
use hurtle_core::{AddRequest, Engine, ipc};

fn main() -> anyhow::Result<()> {
    let urls: Vec<String> = std::env::args()
        .skip(1)
        .filter(|a| a.starts_with("http://") || a.starts_with("https://"))
        .collect();

    // Single instance: forward to a running Hurtle if there is one.
    if ipc::send(&ipc::IpcRequest::Ping).is_ok() {
        for url in &urls {
            ipc::send(&ipc::IpcRequest::Add(AddRequest::new(url.clone())))?;
        }
        ipc::send(&ipc::IpcRequest::Show)?;
        return Ok(());
    }

    // Let the browser extension find us (no-op when already registered).
    hurtle_core::nmh::ensure_registered();

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
    for url in urls {
        if let Err(e) = engine.add(AddRequest::new(url)) {
            eprintln!("hurtle: {e:#}");
        }
    }

    // With a tray icon, closing the window leaves Hurtle running there (as
    // IDM does); "Quit" in the tray menu exits.
    let has_tray = tray::start(&engine, flags.clone());

    gpui_kit::application()
        .with_assets(gpui_kit::assets::Assets)
        .run(move |cx| {
            gpui_kit::init(cx);
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
            open_main_window(&engine, cx);
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
                            None => open_main_window(&engine, cx),
                        });
                    }
                }
            })
            .detach();
        });
    Ok(())
}

fn open_main_window(engine: &Arc<Engine>, cx: &mut App) {
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
        cx.new(|cx| view::HurtleView::new(engine, window, cx))
    }) {
        eprintln!("hurtle: could not open window: {e:#}");
    }
}
