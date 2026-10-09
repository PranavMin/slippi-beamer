//! LazyTO mode (LAZYTO.md), behind `LAZYTO=true` in CONFIG/config.txt: what a
//! beamer does for a LazyTO station beyond carrying the Wii's traffic
//! (`net::relay`, `storage::mailbox`).
//!
//! - `store`: the `lazyto` namespace of the `jrnl` NVS partition, where the
//!   station number (`crate::name`) and the acks live;
//! - `known` and `inventory`: every replay on the card, classed and counted,
//!   with the card's free space counted from the FAT;
//! - `served`: the SHA-256 of each replay as it is served;
//! - `sync`: the beamer's own request on the relay link, answered with acks
//!   for the replays the laptop holds, and signed with the secret;
//! - `acks`: those acks, in flash;
//! - `erase`: at a cold boot, before the USB bind, the acked replays go.
//!
//! With `LAZYTO=false` none of it runs: the station number resets to 1 at
//! every boot, nothing is written to flash, and the scan is upstream's.

pub mod acks;
pub mod erase;
pub mod hmac;
pub mod inventory;
pub mod known;
pub mod served;
pub mod store;
pub mod sync;
pub mod wire;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};

use esp_idf_svc::sys::lazyto::{
    beamer_storage_STORE_FILLING, beamer_storage_STORE_FULL, beamer_storage_STORE_NO_CARD,
    beamer_storage_STORE_OK, beamer_storage_STORE_UNREADABLE, beamer_storage_STORE_WRITE_FAILED,
    beamer_storage_STORE_WRONG_FORMAT, beamer_wifi_WIFI_CANT_JOIN, beamer_wifi_WIFI_JOINING,
    beamer_wifi_WIFI_NO_ADDRESS, beamer_wifi_WIFI_NO_SSID, beamer_wifi_WIFI_RADIO,
    beamer_wifi_WIFI_UP,
};

use crate::config::{Secret, Settings};
use crate::station::StationId;
use crate::status::{ErrorLabel, WarningLabel};
use crate::storage::fat::WriteWindow;
use crate::storage::SdCard;

static ENABLED: AtomicBool = AtomicBool::new(false);
static COLD: AtomicBool = AtomicBool::new(false);
static SECRET: Mutex<Option<Secret>> = Mutex::new(None);
static STATION_ID: OnceLock<[u8; 16]> = OnceLock::new();

/// Whether this boot runs LazyTO mode. Read at boot only, like `LAZYTO`.
pub fn enabled() -> bool {
    ENABLED.load(Ordering::Relaxed)
}

/// A power-on reset, and the first boot since power was applied: the only
/// boot that erases (`erase.rs`). Panic, watchdog, brownout, software and
/// post-flash resets are warm.
pub fn cold_boot() -> bool {
    COLD.load(Ordering::Relaxed)
}

/// LAZYTO-SECRET, which the beamer puts in `relay_auth`.
pub fn secret() -> Option<Secret> {
    *SECRET.lock().unwrap_or_else(|e| e.into_inner())
}

/// A config edit changed LAZYTO-SECRET: it applies from the next request.
pub fn set_secret(secret: Option<Secret>) {
    *SECRET.lock().unwrap_or_else(|e| e.into_inner()) = secret;
}

/// The beamer's StationId, as the sync carries it.
pub fn station_id() -> [u8; 16] {
    STATION_ID.get().copied().unwrap_or([0; 16])
}

/// LazyTO mode's part of the boot, inside the write window and before the
/// USB bind: the flash store, the cold-boot erase, the ack table, the saved
/// station number and the secret. Only with `LAZYTO=true`.
pub fn boot(window: &WriteWindow, sd: &SdCard, id: &StationId, settings: &Settings) {
    ENABLED.store(true, Ordering::Relaxed);
    let _ = STATION_ID.set(*id.as_bytes());
    set_secret(settings.secret);

    let reset = unsafe { esp_idf_svc::sys::esp_reset_reason() };
    let boots = unsafe { esp_idf_svc::sys::beamer_boot_count() };
    let cold = reset == esp_idf_svc::sys::esp_reset_reason_t_ESP_RST_POWERON && boots == 1;
    COLD.store(cold, Ordering::Relaxed);

    // replays go out raw in LazyTO mode (net::transfer): the gzip arena's
    // 15 KB go to the heap, which the relay link, the tables below and a
    // download all draw on (hardware, 2026-10-08: about 28 KB free at idle
    // without it, and a download ran the heap out)
    if !unsafe { esp_idf_svc::sys::beamer_gz_donate_arena() } {
        log::error!("LazyTO: the gzip arena stayed out of the heap");
    }

    known::init();
    served::init();
    inventory::init();
    if !store::open() {
        log::error!("LazyTO: no flash store; the station number and acks are off this boot");
    }

    let card = erase::card_id(window, sd);
    let table = acks::read(card);
    erase::note_dropped(table.dropped);
    let archive_id = if table.dropped {
        [0; 16]
    } else {
        table.archive_id
    };
    if cold {
        let (kept, changed) = erase::run(window, &table);
        if changed || table.dropped || table.torn {
            acks::write(card, archive_id, &kept);
        } else {
            acks::install(card, archive_id, &kept);
        }
    } else {
        log::info!("LazyTO: warm boot ({boots} since power-on): nothing is erased");
        if table.dropped || table.torn {
            acks::write(card, archive_id, &table.records);
        } else {
            acks::install(card, archive_id, &table.records);
        }
    }
    drop(table);
    crate::name::load();

    log::info!(
        "LazyTO: station {}, secret {}, {} ack(s)",
        crate::name::current(),
        if settings.secret.is_some() {
            "set"
        } else {
            "missing"
        },
        acks::count(),
    );
}

/// `beamer_hello.wifi`: the Wi-Fi state, `WIFI_UP` exactly when the station
/// has an address.
pub fn wifi_state() -> u8 {
    use crate::net::wifi::{self, State};
    if crate::status::ip().is_some() {
        return beamer_wifi_WIFI_UP as u8;
    }
    (match wifi::state() {
        State::Joining | State::Up => beamer_wifi_WIFI_JOINING,
        State::NoSsid => beamer_wifi_WIFI_NO_SSID,
        State::CantJoin => beamer_wifi_WIFI_CANT_JOIN,
        State::NoAddress => beamer_wifi_WIFI_NO_ADDRESS,
        State::Radio => beamer_wifi_WIFI_RADIO,
    }) as u8
}

/// `beamer_hello.storage`: the card's state, from the error and warning
/// labels and the inventory. The worst one wins: a card that cannot be used
/// before one that cannot hold the next game, before one filling up.
pub fn storage_state() -> u8 {
    use crate::errors::standing;
    let worst = if standing(ErrorLabel::NoSdCard) {
        beamer_storage_STORE_NO_CARD
    } else if standing(ErrorLabel::WrongFormat) {
        beamer_storage_STORE_WRONG_FORMAT
    } else if standing(ErrorLabel::SdUnreadable)
        || crate::warnings::active(WarningLabel::DriveFailing)
    {
        beamer_storage_STORE_UNREADABLE
    } else if standing(ErrorLabel::WriteFailed) || standing(ErrorLabel::CardStuck) {
        beamer_storage_STORE_WRITE_FAILED
    } else {
        match inventory::space() {
            inventory::Space::Full => beamer_storage_STORE_FULL,
            inventory::Space::Filling => beamer_storage_STORE_FILLING,
            inventory::Space::Ok | inventory::Space::Unknown => beamer_storage_STORE_OK,
        }
    };
    worst as u8
}

#[allow(non_upper_case_globals)]
pub fn wifi_name(w: u8) -> &'static str {
    match w as u32 {
        beamer_wifi_WIFI_UP => "up",
        beamer_wifi_WIFI_JOINING => "joining",
        beamer_wifi_WIFI_NO_SSID => "no_ssid",
        beamer_wifi_WIFI_CANT_JOIN => "cant_join",
        beamer_wifi_WIFI_NO_ADDRESS => "no_address",
        beamer_wifi_WIFI_RADIO => "radio",
        _ => "unknown",
    }
}

#[allow(non_upper_case_globals)]
pub fn storage_name(s: u8) -> &'static str {
    match s as u32 {
        beamer_storage_STORE_OK => "ok",
        beamer_storage_STORE_NO_CARD => "no_card",
        beamer_storage_STORE_UNREADABLE => "unreadable",
        beamer_storage_STORE_WRITE_FAILED => "write_failed",
        beamer_storage_STORE_WRONG_FORMAT => "wrong_format",
        beamer_storage_STORE_FILLING => "filling",
        beamer_storage_STORE_FULL => "full",
        _ => "unknown",
    }
}
