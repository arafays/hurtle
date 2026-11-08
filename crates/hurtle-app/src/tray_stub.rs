//! No tray icon on this platform yet: closing the window quits Hurtle.

use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use hurtle_core::Engine;

pub struct Flags {
    pub show: AtomicBool,
    pub quit: AtomicBool,
}

pub fn start(_engine: &Arc<Engine>, _flags: Arc<Flags>) -> bool {
    false
}
