//! LazyTO mode's flash: the `lazyto` namespace of the 64 KB `jrnl` NVS
//! partition, which `journal.rs` already holds open. Not the default `nvs`
//! at 0x9000: a merged `beamer.bin` flashed at 0x0 pads over it (probably),
//! while `jrnl` lies past the image's end, so the number and the acks survive
//! a reflash that does not erase the whole chip.
//!
//! Keys: `station` (u16; absent = no number yet), `ackhdr` and `ack00` to
//! `ack31` (`acks.rs`).

use std::sync::{Mutex, MutexGuard};

use esp_idf_svc::nvs::{EspNvs, NvsCustom};

const NAMESPACE: &str = "lazyto";
const KEY_STATION: &str = "station";

static NVS: Mutex<Option<EspNvs<NvsCustom>>> = Mutex::new(None);

fn lock() -> MutexGuard<'static, Option<EspNvs<NvsCustom>>> {
    NVS.lock().unwrap_or_else(|e| e.into_inner())
}

/// Opens the namespace. False when `jrnl` or NVS is unavailable; LazyTO mode
/// then runs without a saved number and without acks.
pub fn open() -> bool {
    let Some(part) = crate::journal::partition() else {
        return false;
    };
    match EspNvs::new(part, NAMESPACE, true) {
        Ok(nvs) => {
            *lock() = Some(nvs);
            true
        }
        Err(e) => {
            log::error!("LazyTO: NVS namespace {NAMESPACE}: {e}");
            false
        }
    }
}

/// Runs `f` on the open namespace; `None` if it is not open.
pub fn with<R>(f: impl FnOnce(&mut EspNvs<NvsCustom>) -> R) -> Option<R> {
    lock().as_mut().map(f)
}

/// The saved station number; `None` when the beamer has none yet.
pub fn station() -> Option<u16> {
    with(|nvs| match nvs.get_u16(KEY_STATION) {
        Ok(v) => v.filter(|n| *n != 0),
        Err(e) => {
            log::warn!("LazyTO: could not read the station number: {e}");
            None
        }
    })
    .flatten()
}

pub fn set_station(n: u16) -> bool {
    with(|nvs| match nvs.set_u16(KEY_STATION, n) {
        Ok(()) => true,
        Err(e) => {
            log::error!("LazyTO: could not save station {n}: {e}");
            false
        }
    })
    .unwrap_or(false)
}
