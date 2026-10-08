//! What the beamer knows about each replay file this boot, by `wire::key`
//! (name, size and FAT modified time): its class once its header has been
//! read, and whether the laptop has acked it. One sorted table, so the
//! inventory peeks a file's header once per key, not on every pass.
//!
//! Taken from the heap once, at boot, in LazyTO mode only: `CAPACITY` keys of
//! 8 bytes, with the flags in each key's low bits ([`wire::KEY_FLAGS`]).

use std::sync::{Mutex, MutexGuard};

use super::wire::KEY_FLAGS;

/// As many as the ack table holds (`acks::CAPACITY`): the most 4 MB replays
/// a 4 GB partition holds.
pub const CAPACITY: usize = 1024;

/// The header was read: its size covers the raw length it declares.
pub const FINISHED: u64 = 1;
/// The header was read: raw length 0 (an interrupted or live recording), or
/// a size short of it.
pub const INCOMPLETE: u64 = 2;
/// The header was read and is not a replay's.
pub const BAD: u64 = 4;
/// The laptop holds this file and the beamer has an ack for it in flash.
pub const ACKED: u64 = 8;
/// Seen by the inventory's current pass: a pass forgets the keys it did not
/// see, unless they are acked.
pub const SEEN: u64 = 16;

const _: () = assert!(FINISHED | INCOMPLETE | BAD | ACKED | SEEN == KEY_FLAGS);

static TABLE: Mutex<Option<Vec<u64>>> = Mutex::new(None);

pub struct Known<'a>(MutexGuard<'a, Option<Vec<u64>>>);

/// The table, locked; empty (and refusing everything) outside LazyTO mode.
pub fn lock() -> Known<'static> {
    Known(TABLE.lock().unwrap_or_else(|e| e.into_inner()))
}

pub fn init() {
    *lock().0 = Some(Vec::with_capacity(CAPACITY));
}

fn bare(entry: u64) -> u64 {
    entry & !KEY_FLAGS
}

impl Known<'_> {
    fn find(&self, key: u64) -> Option<Result<usize, usize>> {
        let v = self.0.as_ref()?;
        Some(v.binary_search_by(|e| bare(*e).cmp(&bare(key))))
    }

    pub fn contains(&self, key: u64) -> bool {
        matches!(self.find(key), Some(Ok(_)))
    }

    /// The flags of `key`; 0 if it is not in the table.
    pub fn flags(&self, key: u64) -> u64 {
        match (self.find(key), self.0.as_ref()) {
            (Some(Ok(i)), Some(v)) => v[i] & KEY_FLAGS,
            _ => 0,
        }
    }

    /// Adds `flags` to `key`, inserting it. False when the table is full.
    pub fn set(&mut self, key: u64, flags: u64) -> bool {
        let found = self.find(key);
        let Some(v) = self.0.as_mut() else {
            return false;
        };
        match found {
            Some(Ok(i)) => {
                v[i] |= flags & KEY_FLAGS;
                true
            }
            Some(Err(i)) if v.len() < CAPACITY => {
                v.insert(i, bare(key) | (flags & KEY_FLAGS));
                true
            }
            _ => false,
        }
    }

    /// Takes `flags` off every key.
    pub fn clear_all(&mut self, flags: u64) {
        if let Some(v) = self.0.as_mut() {
            for e in v.iter_mut() {
                *e &= !(flags & KEY_FLAGS);
            }
        }
    }

    /// Keeps the keys for which `keep(flags)` holds.
    pub fn retain(&mut self, keep: impl Fn(u64) -> bool) {
        if let Some(v) = self.0.as_mut() {
            v.retain(|e| keep(e & KEY_FLAGS));
        }
    }

    pub fn is_full(&self) -> bool {
        self.0.as_ref().is_none_or(|v| v.len() >= CAPACITY)
    }
}
