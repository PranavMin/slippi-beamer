//! The cold-boot erase (LAZYTO.md, "Self-erase"). The firmware writes the
//! card only before the USB bind or after an eject, and a Wii never ejects,
//! so replays the laptop holds go at the next cold boot, before the bind:
//!
//! - only at a power-on reset that is the first boot since power was applied
//!   (`lazyto::cold_boot`); if this code crashes, the next boot is warm and
//!   binds without erasing;
//! - 0-byte `Game_*.slp` entries first, without an ack: at a cold boot no
//!   file can be open, and such an entry holds no data;
//! - then files with a matching ack: name, size and FAT modified time (the
//!   key) and the CRC32 of the first 1 KB are all checked again;
//! - within `BUDGET` of the start, so a beamer bumped mid-set misses as
//!   little as possible: what does not fit waits for the next cold boot;
//! - the screen shows ERASING n, and the next sync carries the report.

use std::io::Read as _;
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant};

use super::acks::{CardId, Record, Table};
use super::wire::{self, HEAD_BYTES};
use crate::errors::{self, Target};
use crate::status::{self, ErrorLabel};
use crate::storage::fat::{self, WriteWindow, BASE_PATH};
use crate::storage::SdCard;

/// Decided 2026-10-07: 3 to 5 s, 4 s.
const BUDGET: Duration = Duration::from_secs(4);
/// Names gathered per kind; more than a budget's worth either way.
const GATHER: usize = 256;
/// The screen's count is redrawn after each batch.
const BATCH: usize = 8;
/// Longer names are never synced, so never acked, and never erased.
const NAME_MAX: usize = super::inventory::NAME_MAX;

/// What this boot's erase did, for the sync (`beamer_sync_req`).
#[derive(Debug, Clone, Copy, Default)]
pub struct Report {
    /// acked files deleted
    pub erased: u16,
    /// 0-byte entries deleted
    pub erased_empty: u16,
    pub erase_ms: u16,
    /// acked files the budget left for the next cold boot
    pub erase_left: u16,
    /// a delete failed, and the erase stopped there
    pub failed: bool,
    /// the ack table was dropped at this boot: another card
    pub acks_dropped: bool,
}

static REPORT: Mutex<Report> = Mutex::new(Report {
    erased: 0,
    erased_empty: 0,
    erase_ms: 0,
    erase_left: 0,
    failed: false,
    acks_dropped: false,
});

fn report_lock() -> MutexGuard<'static, Report> {
    REPORT.lock().unwrap_or_else(|e| e.into_inner())
}

pub fn report() -> Report {
    *report_lock()
}

pub fn note_dropped(dropped: bool) {
    report_lock().acks_dropped = dropped;
}

/// The card's identity: the FAT volume serial from the replay partition's
/// boot sector, and a hash of the SD card's CID. Before the bind only.
pub fn card_id(window: &WriteWindow, sd: &SdCard) -> CardId {
    use esp_idf_svc::sys::{sdmmc_read_sectors, ESP_OK};

    let mut vbr = vec![0u8; 512];
    let err = unsafe {
        sdmmc_read_sectors(
            sd.raw(),
            vbr.as_mut_ptr().cast(),
            window.volume_base() as usize,
            1,
        )
    };
    let vsn = if err != ESP_OK {
        log::warn!("LazyTO: could not read the boot sector: 0x{err:x}");
        0
    } else {
        // BS_VolID: 67 on FAT32, 39 on FAT12/16
        let at = if &vbr[82..87] == b"FAT32" { 67 } else { 39 };
        u32::from_le_bytes([vbr[at], vbr[at + 1], vbr[at + 2], vbr[at + 3]])
    };

    // SAFETY: the card was probed at boot; its CID is decoded and fixed
    let c = unsafe { (*sd.raw()).__bindgen_anon_1.cid };
    // SAFETY: c_char and u8 have one size
    let name = unsafe { core::slice::from_raw_parts(c.name.as_ptr().cast::<u8>(), c.name.len()) };
    let cid = wire::fnv64(&[
        &c.mfg_id.to_le_bytes(),
        &c.oem_id.to_le_bytes(),
        name,
        &c.revision.to_le_bytes(),
        &c.serial.to_le_bytes(),
        &c.date.to_le_bytes(),
    ]);
    CardId { vsn, cid }
}

/// The erase, inside the boot's write window. Returns the ack records to
/// keep, and whether they differ from `table`'s.
pub fn run(window: &WriteWindow, table: &Table) -> (Vec<Record>, bool) {
    let t0 = Instant::now();

    let mut records = table.records.clone();
    records.sort_unstable_by_key(|r| r.key);
    let mut present = vec![false; records.len()];
    let mut empties: Vec<heapless::String<NAME_MAX>> = Vec::new();
    let mut acked: Vec<(heapless::String<NAME_MAX>, usize)> = Vec::new();
    let mut acked_over = 0usize;

    let dir = window.fat_path("SLIPPI");
    let walk = fat::for_each_entry(&dir, |e| {
        if e.dir || !wire::is_game_name(e.name) {
            return;
        }
        let Ok(name) = heapless::String::<NAME_MAX>::try_from(e.name) else {
            return;
        };
        if e.size == 0 {
            if empties.len() < GATHER {
                empties.push(name);
            }
            return;
        }
        let key = wire::key(e.name, e.size, wire::fat_mtime(e.fdate, e.ftime));
        if let Ok(i) = records.binary_search_by_key(&key, |r| r.key) {
            present[i] = true;
            if acked.len() < GATHER {
                acked.push((name, i));
            } else {
                acked_over += 1;
            }
        }
    });
    if let Err(res) = walk {
        log::warn!("erase: could not list SLIPPI/ (FRESULT {res}); nothing erased");
        return (table.records.clone(), false);
    }

    let gone = present.iter().filter(|p| !**p).count();
    let total = empties.len() + acked.len();
    if total == 0 {
        log::info!("erase: nothing to erase; {gone} ack(s) for files no longer here dropped");
        let kept: Vec<Record> = records
            .iter()
            .zip(present.iter())
            .filter(|(_, p)| **p)
            .map(|(r, _)| *r)
            .collect();
        report_lock().erase_ms = t0.elapsed().as_millis().min(u16::MAX as u128) as u16;
        return (kept, gone > 0);
    }

    status::set_erasing(Some(total as u32));
    log::info!(
        "erase: {} empty entr(ies), {} acked file(s) to erase{}",
        empties.len(),
        acked.len(),
        if acked_over > 0 {
            format!(", {acked_over} more next time")
        } else {
            String::new()
        },
    );

    let mut erased = vec![false; records.len()];
    let mut mismatched = vec![false; records.len()];
    let (mut n_empty, mut n_acked, mut done, mut failed) = (0usize, 0usize, 0usize, false);
    let started = Instant::now();

    let fits = |done: usize| -> bool {
        let elapsed = t0.elapsed();
        let average = if done == 0 {
            Duration::ZERO
        } else {
            started.elapsed() / done as u32
        };
        elapsed + average <= BUDGET
    };
    let path = |name: &str| format!("{BASE_PATH}/SLIPPI/{name}");

    'empties: for batch in empties.chunks(BATCH) {
        for name in batch {
            if !fits(done) {
                break 'empties;
            }
            match std::fs::remove_file(path(name)) {
                Ok(()) => n_empty += 1,
                Err(e) => {
                    fail(name, &e);
                    failed = true;
                    break 'empties;
                }
            }
            done += 1;
        }
        status::set_erasing(Some((total - done) as u32));
    }

    let mut processed = 0usize;
    if !failed {
        'acked: for batch in acked.chunks(BATCH) {
            for (name, i) in batch {
                if !fits(done) {
                    break 'acked;
                }
                processed += 1;
                let r = records[*i];
                match head_crc(&path(name)) {
                    Some(crc) if crc == r.crc => {}
                    other => {
                        // not the file the laptop holds: never erased, and the ack goes
                        log::warn!(
                            "erase: {name}: first 1 KB {} the ack; kept, ack dropped",
                            if other.is_some() {
                                "differs from"
                            } else {
                                "unreadable for"
                            }
                        );
                        mismatched[*i] = true;
                        done += 1;
                        continue;
                    }
                }
                match std::fs::remove_file(path(name)) {
                    Ok(()) => {
                        erased[*i] = true;
                        n_acked += 1;
                    }
                    Err(e) => {
                        fail(name, &e);
                        failed = true;
                        break 'acked;
                    }
                }
                done += 1;
            }
            status::set_erasing(Some((total.saturating_sub(done)) as u32));
        }
    }

    let left = acked.len() - processed + acked_over;
    let ms = t0.elapsed().as_millis().min(u16::MAX as u128) as u16;
    status::set_erasing(None);
    {
        let mut r = report_lock();
        r.erased = n_acked.min(u16::MAX as usize) as u16;
        r.erased_empty = n_empty.min(u16::MAX as usize) as u16;
        r.erase_ms = ms;
        r.erase_left = left.min(u16::MAX as usize) as u16;
        r.failed = failed;
    }
    log::info!(
        "erase: {n_acked} acked file(s) and {n_empty} empty entr(ies) in {ms} ms; {left} left for the next cold boot{}",
        if failed { "; stopped by a failed delete" } else { "" }
    );

    let kept: Vec<Record> = (0..records.len())
        .filter(|&i| present[i] && !erased[i] && !mismatched[i])
        .map(|i| records[i])
        .collect();
    let changed = kept.len() != table.records.len();
    (kept, changed)
}

fn head_crc(path: &str) -> Option<u32> {
    let mut f = std::fs::File::open(path).ok()?;
    let mut head = [0u8; HEAD_BYTES];
    let mut n = 0;
    while n < head.len() {
        match f.read(&mut head[n..]) {
            Ok(0) => break,
            Ok(k) => n += k,
            Err(_) => return None,
        }
    }
    Some(wire::crc32(&head[..n]))
}

fn fail(name: &str, e: &std::io::Error) {
    let detail = format!("{name}: {e}");
    errors::error(
        Target::Late,
        ErrorLabel::WriteFailed,
        "lazyto",
        &[
            "the power-on erase could not delete a collected replay",
            &detail,
            "it stopped there; the rest waits for the next power-on",
        ],
    );
}
