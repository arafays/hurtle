//! System tray icon (StatusNotifierItem): shows that Hurtle is running and
//! what it's doing, and keeps it reachable after the window is closed.
//!
//! The tray lives on the engine's tokio runtime. It talks to the GPUI side
//! only through two flags the app polls: `show` (open/raise the window) and
//! `quit`.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use hurtle_core::{Engine, Status, util};
use ksni::TrayMethods as _;
use ksni::menu::StandardItem;

pub struct Flags {
    pub show: AtomicBool,
    pub quit: AtomicBool,
}

struct HurtleTray {
    engine: Arc<Engine>,
    flags: Arc<Flags>,
    active: usize,
    queued: usize,
    speed: f64,
    icon: Vec<ksni::Icon>,
}

impl HurtleTray {
    fn summary(&self) -> String {
        match (self.active, self.queued) {
            (0, 0) => "Idle".into(),
            (0, q) => format!("{q} waiting"),
            (a, 0) => format!("{a} downloading · {}", util::fmt_speed(self.speed)),
            (a, q) => format!(
                "{a} downloading · {} · {q} waiting",
                util::fmt_speed(self.speed)
            ),
        }
    }
}

impl ksni::Tray for HurtleTray {
    fn id(&self) -> String {
        "hurtle".into()
    }
    fn title(&self) -> String {
        format!("Hurtle: {}", self.summary())
    }
    fn category(&self) -> ksni::Category {
        ksni::Category::ApplicationStatus
    }
    fn icon_name(&self) -> String {
        "hurtle".into()
    }
    fn icon_pixmap(&self) -> Vec<ksni::Icon> {
        self.icon.clone()
    }
    fn tool_tip(&self) -> ksni::ToolTip {
        ksni::ToolTip {
            title: "Hurtle".into(),
            description: self.summary(),
            ..Default::default()
        }
    }
    fn activate(&mut self, _x: i32, _y: i32) {
        self.flags.show.store(true, Ordering::Relaxed);
    }
    fn menu(&self) -> Vec<ksni::MenuItem<Self>> {
        let busy = self.active + self.queued > 0;
        vec![
            StandardItem {
                label: self.summary(),
                enabled: false,
                ..Default::default()
            }
            .into(),
            ksni::MenuItem::Separator,
            StandardItem {
                label: "Show Hurtle".into(),
                icon_name: "window-new".into(),
                activate: Box::new(|t: &mut Self| t.flags.show.store(true, Ordering::Relaxed)),
                ..Default::default()
            }
            .into(),
            StandardItem {
                label: "Pause all".into(),
                icon_name: "media-playback-pause".into(),
                enabled: busy,
                activate: Box::new(|t: &mut Self| t.engine.pause_all()),
                ..Default::default()
            }
            .into(),
            StandardItem {
                label: "Resume all".into(),
                icon_name: "media-playback-start".into(),
                activate: Box::new(|t: &mut Self| t.engine.resume_all()),
                ..Default::default()
            }
            .into(),
            ksni::MenuItem::Separator,
            StandardItem {
                label: "Quit".into(),
                icon_name: "application-exit".into(),
                activate: Box::new(|t: &mut Self| t.flags.quit.store(true, Ordering::Relaxed)),
                ..Default::default()
            }
            .into(),
        ]
    }
}

/// The app icon as ARGB32 pixmaps, so the tray works without an installed
/// icon theme entry (e.g. `mise run dev`).
fn pixmaps() -> Vec<ksni::Icon> {
    [
        &include_bytes!("../../../extension/src/icons/icon-32.png")[..],
        &include_bytes!("../../../extension/src/icons/icon-48.png")[..],
        &include_bytes!("../../../extension/src/icons/icon-128.png")[..],
    ]
    .into_iter()
    .filter_map(|bytes| {
        let mut dec = png::Decoder::new(std::io::Cursor::new(bytes));
        dec.set_transformations(png::Transformations::EXPAND | png::Transformations::ALPHA);
        let mut reader = dec.read_info().ok()?;
        let mut buf = vec![0; reader.output_buffer_size()?];
        let info = reader.next_frame(&mut buf).ok()?;
        if info.color_type != png::ColorType::Rgba || info.bit_depth != png::BitDepth::Eight {
            return None;
        }
        let data = buf[..info.buffer_size()]
            .as_chunks::<4>()
            .0
            .iter()
            .flat_map(|&[r, g, b, a]| [a, r, g, b])
            .collect();
        Some(ksni::Icon {
            width: info.width as i32,
            height: info.height as i32,
            data,
        })
    })
    .collect()
}

/// Register the tray icon. Returns false when there's no session bus /
/// tray host to talk to; the app then quits with its window as usual.
pub fn start(engine: &Arc<Engine>, flags: Arc<Flags>) -> bool {
    let tray = HurtleTray {
        engine: engine.clone(),
        flags,
        active: 0,
        queued: 0,
        speed: 0.0,
        icon: pixmaps(),
    };
    let rt = engine.runtime().clone();
    let handle = match rt.block_on(tray.spawn()) {
        Ok(h) => h,
        Err(e) => {
            eprintln!("hurtle: no tray icon: {e}");
            return false;
        }
    };
    // Keep the tooltip/menu summary current.
    let engine = Arc::downgrade(engine);
    rt.spawn(async move {
        let mut last = (usize::MAX, usize::MAX, u64::MAX);
        loop {
            tokio::time::sleep(Duration::from_secs(1)).await;
            let Some(engine) = engine.upgrade() else {
                break;
            };
            let list = engine.snapshot();
            let active = list.iter().filter(|d| d.status.is_active()).count();
            let queued = list.iter().filter(|d| d.status == Status::Queued).count();
            let speed: f64 = list.iter().map(|d| d.speed).sum();
            // Round so the tray isn't re-sent for every byte/s of jitter.
            let key = (active, queued, (speed / 10_240.0) as u64);
            if key != last {
                last = key;
                if handle
                    .update(|t| {
                        t.active = active;
                        t.queued = queued;
                        t.speed = speed;
                    })
                    .await
                    .is_none()
                {
                    break;
                }
            }
        }
    });
    true
}
