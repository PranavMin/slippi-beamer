//! The station number, set with the button: a click adds 1, holding it takes
//! 1 off, never below 1 (status/mod.rs).
//!
//! Upstream keeps it in RAM and starts every boot at 1. In LazyTO mode it is
//! the station's identity, so it is kept in flash (`lazyto::store`), written
//! a few seconds after the last press and only if it changed, and a new or
//! wiped beamer has no number until its first click: never "Station 1", or
//! every fresh beamer would collide as station 1. 0 means "no number".

use std::sync::atomic::{AtomicU16, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

static N: AtomicU16 = AtomicU16::new(1);

/// LazyTO mode: the number in flash (0 = none), and when the button last
/// changed it.
static SAVED: AtomicU16 = AtomicU16::new(0);
static CHANGED: Mutex<Option<Instant>> = Mutex::new(None);

/// The flash write waits this long after the last press.
const SETTLE: Duration = Duration::from_millis(2500);

pub fn reset() {
    set(1);
}

/// LazyTO mode, at boot: the number saved in flash, or none.
pub fn load() {
    let n = crate::lazyto::store::station().unwrap_or(0);
    SAVED.store(n, Ordering::Relaxed);
    show(n);
}

pub fn current() -> String {
    match number() {
        0 => String::from("No station"),
        n => format!("Station {n}"),
    }
}

/// The station number on the screen, without the allocation `current` makes;
/// 0 when a LazyTO beamer has none yet.
pub fn number() -> u16 {
    N.load(Ordering::Relaxed)
}

/// Whether the beamer has a number. Always, outside LazyTO mode.
pub fn is_set() -> bool {
    number() != 0
}

pub fn bump() {
    set(N.load(Ordering::Relaxed).saturating_add(1));
}

pub fn lower() {
    match N.load(Ordering::Relaxed) {
        0 => {} // nothing to take off: the first click sets 1
        n => set(n.saturating_sub(1)),
    }
}

fn set(n: u16) {
    show(n.max(1));
    if crate::lazyto::enabled() {
        *CHANGED.lock().unwrap_or_else(|e| e.into_inner()) = Some(Instant::now());
    }
}

fn show(n: u16) {
    N.store(n, Ordering::Relaxed);
    let name = current();
    crate::status::set_name(&name);
    crate::net::announce::set_name(&name);

    let mut id = crate::net::check::identity();
    if !id.station.is_empty() {
        id.station_name = name;
        crate::net::check::set(id);
    }
}

/// LazyTO mode, from the main loop: writes the number to flash once the
/// button has been left alone for a moment, and only if it changed.
pub fn persist_due() {
    let due = {
        let changed = CHANGED.lock().unwrap_or_else(|e| e.into_inner());
        changed.is_some_and(|t| t.elapsed() >= SETTLE)
    };
    if due {
        persist_now();
    }
}

/// Writes a pending change now: before a restart or an eject.
pub fn persist_now() {
    if CHANGED
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .take()
        .is_none()
    {
        return;
    }
    let n = number();
    if n == 0 || n == SAVED.load(Ordering::Relaxed) {
        return;
    }
    if crate::lazyto::store::set_station(n) {
        SAVED.store(n, Ordering::Relaxed);
        log::info!("station number {n} saved");
    }
}
