//! The SHA-256 of each replay served whole this boot, which a sync's "held"
//! answer must equal before the beamer acks the file (`sync.rs`). Computed
//! on the SHA accelerator while the transfer task serves the file, over the
//! raw bytes before gzip; a resumed request hashes the skipped prefix first
//! (`net::transfer`). Nothing is re-read at boot or when idle: a file served
//! before a reboot is simply served again.
//!
//! The last `SLOTS` files, in a table taken from the heap once at boot, in
//! LazyTO mode only.

use std::sync::{Mutex, MutexGuard};

use super::known::{self, ACKED};
use super::wire::{Crc32, HEAD_BYTES};
use crate::lazyto::hmac::Sha256 as _;
use crate::storage::mailbox::{Sha, ShaSlot};

pub const SLOTS: usize = 32;

#[derive(Debug, Clone, Copy)]
pub struct Served {
    pub key: u64,
    pub size: u32,
    pub mtime: u32,
    /// CRC32 of the first 1 KB, for the ack
    pub crc: u32,
    pub sha: [u8; 32],
}

static TABLE: Mutex<Option<Vec<Served>>> = Mutex::new(None);

fn lock() -> MutexGuard<'static, Option<Vec<Served>>> {
    TABLE.lock().unwrap_or_else(|e| e.into_inner())
}

pub fn init() {
    *lock() = Some(Vec::with_capacity(SLOTS));
}

/// The hash of `key`, if the beamer served that file whole this boot.
pub fn get(key: u64) -> Option<Served> {
    lock().as_ref()?.iter().find(|s| s.key == key).copied()
}

fn record(entry: Served) {
    let mut guard = lock();
    let Some(t) = guard.as_mut() else { return };
    if let Some(i) = t.iter().position(|s| s.key == entry.key) {
        t.remove(i);
    } else if t.len() >= SLOTS {
        // an acked file's hash is spent; otherwise the oldest goes
        let k = known::lock();
        let spent = t.iter().position(|s| k.flags(s.key) & ACKED != 0);
        drop(k);
        t.remove(spent.unwrap_or(0));
    }
    t.push(entry);
}

/// One transfer's hash, fed every raw byte of the file from 0, in order.
pub struct Hashing {
    sha: Sha,
    crc: Crc32,
    fed: u64,
    key: u64,
    size: u32,
    mtime: u32,
}

impl Hashing {
    /// `None` when the SHA contexts are not there (no mailbox).
    pub fn begin(key: u64, size: u32, mtime: u32) -> Option<Hashing> {
        Some(Hashing {
            sha: Sha::begin(ShaSlot::Serve)?,
            crc: Crc32::new(),
            fed: 0,
            key,
            size,
            mtime,
        })
    }

    pub fn feed(&mut self, data: &[u8]) {
        let head = (HEAD_BYTES as u64)
            .saturating_sub(self.fed)
            .min(data.len() as u64) as usize;
        self.crc.update(&data[..head]);
        self.sha.update(data);
        self.fed += data.len() as u64;
    }

    /// The whole file went through: keep its hash for the next sync. False
    /// (and nothing kept) when the bytes fed do not add up to its size.
    pub fn complete(self) -> bool {
        if self.fed != self.size as u64 {
            log::warn!("served: {} of {} B hashed; not kept", self.fed, self.size);
            return false;
        }
        record(Served {
            key: self.key,
            size: self.size,
            mtime: self.mtime,
            crc: self.crc.finish(),
            sha: self.sha.finish(),
        });
        true
    }
}
