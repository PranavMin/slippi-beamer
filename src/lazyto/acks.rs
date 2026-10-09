//! The ack table: one record per replay the laptop has stored and the beamer
//! has checked (`sync.rs`), erased at the next cold boot (`erase.rs`). In
//! flash, in `store`'s namespace, so it survives the power-off that makes
//! the next boot cold:
//!
//! - `ackhdr`: format, the card it belongs to (FAT volume serial and a hash
//!   of the SD CID), the archive it belongs to (the laptop's `archive_id`),
//!   and the record count;
//! - `ack00`..`ack31`: the records, 32 per page, so adding one rewrites one
//!   small blob.
//!
//! A record is the file's key (name, size, FAT modified time), its size and
//! modified time again, and the CRC32 of its first 1 KB; the erase re-checks
//! all of them, so a reused name never matches.
//!
//! In RAM only the count, the ids and an ACKED flag per key in `known`.

use std::sync::{Mutex, MutexGuard};

use super::known::{self, ACKED};
use super::store;

pub const CAPACITY: usize = 1024;
const PER_PAGE: usize = 32;
const PAGES: usize = CAPACITY / PER_PAGE;
const RECORD: usize = 20;
const PAGE_BYTES: usize = PER_PAGE * RECORD;

const KEY_HEADER: &str = "ackhdr";
const FORMAT: u8 = 1;
const HEADER: usize = 36;

/// Which card the table belongs to: a swapped or reformatted card drops it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CardId {
    /// the FAT volume serial from the replay partition's boot sector
    pub vsn: u32,
    /// FNV-1a of the SD card's CID fields
    pub cid: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Record {
    pub key: u64,
    pub size: u32,
    pub mtime: u32,
    /// CRC32 of the file's first 1 KB (`wire::HEAD_BYTES`)
    pub crc: u32,
}

impl Record {
    fn encode(&self, out: &mut [u8]) {
        out[0..8].copy_from_slice(&self.key.to_le_bytes());
        out[8..12].copy_from_slice(&self.size.to_le_bytes());
        out[12..16].copy_from_slice(&self.mtime.to_le_bytes());
        out[16..20].copy_from_slice(&self.crc.to_le_bytes());
    }

    fn decode(b: &[u8]) -> Record {
        let u32_at = |o: usize| u32::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]]);
        let mut key = [0u8; 8];
        key.copy_from_slice(&b[0..8]);
        Record {
            key: u64::from_le_bytes(key),
            size: u32_at(8),
            mtime: u32_at(12),
            crc: u32_at(16),
        }
    }
}

struct State {
    card: CardId,
    archive_id: [u8; 16],
    count: usize,
}

static STATE: Mutex<State> = Mutex::new(State {
    card: CardId { vsn: 0, cid: 0 },
    archive_id: [0; 16],
    count: 0,
});

fn state() -> MutexGuard<'static, State> {
    STATE.lock().unwrap_or_else(|e| e.into_inner())
}

fn page_key(i: usize) -> heapless::String<8> {
    use core::fmt::Write as _;
    let mut s = heapless::String::new();
    let _ = write!(s, "ack{i:02}");
    s
}

/// The table as stored. `dropped` is set when one was stored for another
/// card, or in a format this firmware does not know: it is discarded, and
/// the archive id with it. `torn` when a page could not be read: the
/// records before it are kept, and the table must be written again.
pub struct Table {
    pub archive_id: [u8; 16],
    pub records: Vec<Record>,
    pub dropped: bool,
    pub torn: bool,
}

/// Boot only (the records take up to 20 KB of heap, which the boot can
/// spare): reads the stored table for `card`.
pub fn read(card: CardId) -> Table {
    let mut t = Table {
        archive_id: [0; 16],
        records: Vec::new(),
        dropped: false,
        torn: false,
    };
    let read = store::with(|nvs| {
        let mut head = [0u8; HEADER];
        let head = match nvs.get_blob(KEY_HEADER, &mut head) {
            Ok(Some(h)) => h,
            Ok(None) => return,
            Err(e) => {
                log::warn!("acks: header unreadable, dropping the table: {e}");
                t.dropped = true;
                return;
            }
        };
        if head.len() != HEADER || head[0] != FORMAT {
            log::warn!("acks: unknown format, dropping the table");
            t.dropped = true;
            return;
        }
        let stored = CardId {
            vsn: u32::from_le_bytes([head[4], head[5], head[6], head[7]]),
            cid: u64::from_le_bytes(head[8..16].try_into().unwrap_or([0; 8])),
        };
        let count = (u16::from_le_bytes([head[32], head[33]]) as usize).min(CAPACITY);
        if stored != card {
            if count > 0 {
                log::warn!("acks: the card changed; dropping {count} ack(s)");
            }
            t.dropped = count > 0;
            return;
        }
        t.archive_id.copy_from_slice(&head[16..32]);

        let mut page = [0u8; PAGE_BYTES];
        t.records.reserve_exact(count);
        for p in 0..count.div_ceil(PER_PAGE) {
            let want = (count - p * PER_PAGE).min(PER_PAGE);
            match nvs.get_blob(&page_key(p), &mut page) {
                Ok(Some(b)) if b.len() >= want * RECORD => {
                    for r in b[..want * RECORD].chunks_exact(RECORD) {
                        t.records.push(Record::decode(r));
                    }
                }
                other => {
                    // a torn table: keep what is whole, and say so
                    log::warn!(
                        "acks: page {p} unreadable ({:?}); keeping the first {} ack(s)",
                        other.err(),
                        t.records.len()
                    );
                    t.torn = true;
                    break;
                }
            }
        }
    });
    if read.is_none() {
        log::warn!("acks: no flash store this boot");
    }
    t
}

fn write_header(
    nvs: &mut esp_idf_svc::nvs::EspNvs<esp_idf_svc::nvs::NvsCustom>,
    card: CardId,
    archive_id: &[u8; 16],
    count: usize,
) -> bool {
    let mut head = [0u8; HEADER];
    head[0] = FORMAT;
    head[4..8].copy_from_slice(&card.vsn.to_le_bytes());
    head[8..16].copy_from_slice(&card.cid.to_le_bytes());
    head[16..32].copy_from_slice(archive_id);
    head[32..34].copy_from_slice(&(count as u16).to_le_bytes());
    match nvs.set_blob(KEY_HEADER, &head) {
        Ok(()) => true,
        Err(e) => {
            log::error!("acks: could not write the header: {e}");
            false
        }
    }
}

/// Boot only: stores `records` as the whole table (after the erase, or when
/// it was dropped), then [`install`]s it.
pub fn write(card: CardId, archive_id: [u8; 16], records: &[Record]) {
    let records = &records[..records.len().min(CAPACITY)];
    store::with(|nvs| {
        let mut page = [0u8; PAGE_BYTES];
        for p in 0..PAGES {
            let chunk = records
                .get(p * PER_PAGE..)
                .map(|r| &r[..r.len().min(PER_PAGE)])
                .unwrap_or(&[]);
            let key = page_key(p);
            if chunk.is_empty() {
                let _ = nvs.remove(&key);
                continue;
            }
            for (r, out) in chunk.iter().zip(page.chunks_exact_mut(RECORD)) {
                r.encode(out);
            }
            if let Err(e) = nvs.set_blob(&key, &page[..chunk.len() * RECORD]) {
                log::error!("acks: could not write page {p}: {e}");
            }
        }
        write_header(nvs, card, &archive_id, records.len());
    });
    install(card, archive_id, records);
}

/// Boot only: the RAM side of the table read with [`read`] (when it was not
/// rewritten): the count, the ids and the ACKED keys.
pub fn install(card: CardId, archive_id: [u8; 16], records: &[Record]) {
    let mut k = known::lock();
    k.clear_all(ACKED);
    let mut held = 0;
    for r in records {
        if k.set(r.key, ACKED) {
            held += 1;
        }
    }
    drop(k);
    let mut s = state();
    s.card = card;
    s.archive_id = archive_id;
    s.count = held;
}

/// Ack records stored.
pub fn count() -> usize {
    state().count
}

/// The archive the table belongs to; all zero for none yet.
pub fn archive_id() -> [u8; 16] {
    state().archive_id
}

/// A verified sync reply names another archive (another laptop, or a
/// deleted archive folder): every ack goes, so the files are collected again
/// while the beamer is still powered, and the table takes the new id.
pub fn adopt(archive_id: [u8; 16]) {
    let mut s = state();
    let dropped = s.count;
    store::with(|nvs| {
        for p in 0..PAGES {
            let _ = nvs.remove(&page_key(p));
        }
        write_header(nvs, s.card, &archive_id, 0);
    });
    s.archive_id = archive_id;
    s.count = 0;
    drop(s);
    known::lock().clear_all(ACKED);
    if dropped > 0 {
        log::warn!(
            "acks: another archive; {dropped} ack(s) dropped, so those files are collected again"
        );
    } else {
        log::info!("acks: the laptop's archive adopted");
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AddError {
    /// 1024 acks wait for a cold boot: the files stay, and FILLING shows
    Full,
    /// the flash write failed
    Flash,
}

/// Acks one file: in flash first, then in RAM.
pub fn add(r: Record) -> Result<(), AddError> {
    if known::lock().flags(r.key) & ACKED != 0 {
        return Ok(());
    }
    let room = {
        let k = known::lock();
        k.contains(r.key) || !k.is_full()
    };
    let mut s = state();
    if s.count >= CAPACITY || !room {
        return Err(AddError::Full);
    }
    let i = s.count;
    let (p, slot) = (i / PER_PAGE, i % PER_PAGE);
    let (card, archive_id) = (s.card, s.archive_id);
    let written = store::with(|nvs| {
        let mut page = [0u8; PAGE_BYTES];
        let key = page_key(p);
        if slot > 0 {
            match nvs.get_blob(&key, &mut page) {
                Ok(Some(b)) if b.len() >= slot * RECORD => {}
                other => {
                    log::error!(
                        "acks: page {p} unreadable before an append: {:?}",
                        other.err()
                    );
                    return false;
                }
            }
        }
        r.encode(&mut page[slot * RECORD..(slot + 1) * RECORD]);
        if let Err(e) = nvs.set_blob(&key, &page[..(slot + 1) * RECORD]) {
            log::error!("acks: could not write page {p}: {e}");
            return false;
        }
        write_header(nvs, card, &archive_id, i + 1)
    });
    if written != Some(true) {
        return Err(AddError::Flash);
    }
    s.count = i + 1;
    drop(s);
    known::lock().set(r.key, ACKED);
    Ok(())
}
