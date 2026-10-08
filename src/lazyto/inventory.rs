//! The inventory of SLIPPI/ in LazyTO mode, run by the scan task in place of
//! upstream's walk (`scan.rs`). Upstream's counts stop at REPLAY-CAP, serve
//! only this boot's newest replays, and treat a file whose header says raw
//! length 0 as live until a reboot, freezing the scan. This one:
//!
//! - walks the whole folder with FatFs (sizes and FAT times come with the
//!   entries), and reads a file's header once per key (`known.rs`);
//! - classes each file: empty (0 bytes), finished (its size covers the raw
//!   length in its header), live (raw length 0, the newest file, and a USB
//!   command from the Wii in the last few seconds: the kernel reads the hello
//!   every second, even while a game is paused), or incomplete (anything
//!   else: an interrupted recording, served and collected as partial);
//! - while a file is live, checks only that file every tick, plus a full
//!   walk every minute, and never freezes;
//! - counts free space from the FAT itself, not from FSInfo, which
//!   overstates it after interrupted games;
//! - keeps the 16 files the next sync asks about (`sync.rs`).

use std::io::Read as _;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant};

use esp_idf_svc::sys::lazyto::{
    sync_kind_SK_FINISHED, sync_kind_SK_INCOMPLETE, sync_kind_SK_LIVE, SYNC_MAX_FILES,
    SYNC_NAME_LEN,
};
use esp_idf_svc::sys::{beamer_wbc_read, ESP_OK};

use super::known::{self, ACKED, BAD, FINISHED, INCOMPLETE, SEEN};
use super::wire;
use crate::slp;
use crate::storage::fat::{self, FatGeometry, ReadWindow};
use crate::storage::{msc, SdCard};
use crate::warnings::{self, WarningLabel};

/// A file is live only while the Wii has sent a USB command this recently.
const LIVE_USB: Duration = Duration::from_secs(5);
/// A full walk at least this often, live file or not.
const BACKSTOP_TICKS: u32 = 60;
/// The FAT is counted at most this often, and never while a game is live.
const FAT_COUNT_EVERY: Duration = Duration::from_secs(120);

/// FILLING: this many files, or less than `FILLING_FREE` free.
const FILLING_FILES: u32 = 384;
const FILLING_FREE: u64 = 1 << 30;
/// FULL: less than this free, so the next game would fail.
const FULL_FREE: u64 = 64 << 20;

const MIB: u64 = 1 << 20;
const SECTOR: u32 = 512;

pub const NAME_MAX: usize = SYNC_NAME_LEN as usize;
pub const LISTED: usize = SYNC_MAX_FILES as usize;
/// One more than a sync lists: the live file may move to the back.
const KEPT: usize = LISTED + 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Space {
    /// no FAT count yet
    Unknown,
    Ok,
    /// `FILLING_FILES` files or less than 1 GB free: replug to erase
    Filling,
    /// less than 64 MB free: the next replay would fail
    Full,
}

/// A file the next sync asks about: not acked, not empty (unless live), and
/// a name of at most `SYNC_NAME_LEN` bytes.
#[derive(Debug, Clone)]
pub struct Candidate {
    pub name: heapless::String<NAME_MAX>,
    pub size: u32,
    pub mtime: u32,
    /// enum sync_kind
    pub kind: u8,
    pub key: u64,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct Counts {
    /// every Game_*.slp
    pub on_card: u32,
    /// not acked, not empty: finished, incomplete or live
    pub to_collect: u32,
    /// acked, so erased at the next cold boot
    pub to_erase: u32,
    /// 0-byte entries, the live one aside
    pub empty: u32,
    /// neither finished, live nor empty
    pub incomplete: u32,
}

#[derive(Debug, Clone)]
pub struct Snapshot {
    /// full walks so far; 0 until the first
    pub passes: u32,
    pub counts: Counts,
    /// free space from the FAT; `None` before the first count
    pub free_mb: Option<u32>,
    /// the volume's data area
    pub card_mb: u32,
    /// held by Game_*.slp files, in whole clusters
    pub used_mb: u32,
    pub space: Space,
    /// group order: finished, incomplete, live; oldest first in each
    pub candidates: heapless::Vec<Candidate, KEPT>,
    /// more files qualify than a sync lists
    pub more: bool,
    /// the live file's key, which the transfer task refuses to serve
    pub live_key: Option<u64>,
}

impl Snapshot {
    fn new() -> Snapshot {
        Snapshot {
            passes: 0,
            counts: Counts::default(),
            free_mb: None,
            card_mb: 0,
            used_mb: 0,
            space: Space::Unknown,
            candidates: heapless::Vec::new(),
            more: false,
            live_key: None,
        }
    }
}

static SNAP: Mutex<Option<Box<Snapshot>>> = Mutex::new(None);
static PASS_DUE: AtomicBool = AtomicBool::new(false);

fn snap() -> MutexGuard<'static, Option<Box<Snapshot>>> {
    SNAP.lock().unwrap_or_else(|e| e.into_inner())
}

pub fn init() {
    *snap() = Some(Box::new(Snapshot::new()));
}

/// The latest full walk's results; `None` outside LazyTO mode.
pub fn snapshot() -> Option<Snapshot> {
    snap().as_deref().cloned()
}

pub fn space() -> Space {
    snap().as_ref().map_or(Space::Unknown, |s| s.space)
}

pub fn live_key() -> Option<u64> {
    snap().as_ref().and_then(|s| s.live_key)
}

/// Asks for a full walk on the next tick: the acks changed.
pub fn request_pass() {
    PASS_DUE.store(true, Ordering::Relaxed);
}

/// The scan task's own state between ticks.
#[derive(Default)]
pub struct State {
    ticks_since_full: u32,
    passes: u32,
    live: Option<Live>,
    fat: Option<FatCount>,
    known_files: u32,
}

struct Live {
    name: heapless::String<NAME_MAX>,
    size: u32,
    mtime: u32,
}

struct FatCount {
    at: Instant,
    free_clusters: u32,
    /// `used` (bytes in whole clusters) when the FAT was counted
    used_then: u64,
}

fn usb_active() -> bool {
    let now = unsafe { esp_idf_svc::sys::esp_timer_get_time() };
    let last = msc::last_cbw_us();
    last != 0 && now.saturating_sub(last) < LIVE_USB.as_micros() as i64
}

/// One scan tick (every second).
pub fn tick(sd: &SdCard, st: &mut State, still_wanted: impl Fn() -> bool) {
    let dirty = msc::take_dirty();
    st.ticks_since_full = st.ticks_since_full.saturating_add(1);
    let forced = PASS_DUE.swap(false, Ordering::Relaxed);
    let full = st.passes == 0
        || forced
        || st.ticks_since_full >= BACKSTOP_TICKS
        || (st.live.is_none() && dirty);
    if !full && st.live.is_none() {
        return;
    }

    let window = match ReadWindow::open_next(sd, still_wanted) {
        Ok(Some(w)) => w,
        Ok(None) => return,
        Err(e) => {
            log::warn!(
                "inventory: could not mount: {e} ({})",
                crate::journal::heap_note()
            );
            warnings::set(WarningLabel::DriveFailing, true);
            return;
        }
    };

    if !full && still_live(&window, st) {
        return;
    }
    pass(&window, st);
}

/// The live file, unchanged and the Wii still talking: nothing to walk.
fn still_live(window: &ReadWindow, st: &State) -> bool {
    let Some(live) = st.live.as_ref() else {
        return false;
    };
    if !usb_active() {
        return false;
    }
    let path = window.fat_path(&format!("SLIPPI/{}", live.name));
    match fat::stat(&path) {
        Some((size, fdate, ftime)) => {
            size == live.size && wire::fat_mtime(fdate, ftime) == live.mtime
        }
        None => false,
    }
}

/// Reads a file's header: FINISHED, INCOMPLETE or INCOMPLETE | BAD; 0 when
/// it could not be read (it is read again next pass).
fn class_of(window: &ReadWindow, name: &str, size: u32) -> u64 {
    let path = window.path(&format!("SLIPPI/{name}"));
    let mut head = [0u8; slp::HEADER_BYTES];
    let want = head.len().min(size as usize);
    let read = std::fs::File::open(&path).and_then(|mut f| f.read_exact(&mut head[..want]));
    if let Err(e) = read {
        log::warn!("inventory: {name}: {e}");
        return 0;
    }
    if want < head.len() {
        return if slp::could_be_replay(&head[..want]) {
            INCOMPLETE
        } else {
            INCOMPLETE | BAD
        };
    }
    match slp::raw_length(&head) {
        None => INCOMPLETE | BAD,
        Some(raw) if raw != 0 && size as u64 >= slp::HEADER_BYTES as u64 + raw as u64 => FINISHED,
        Some(_) => INCOMPLETE,
    }
}

struct Newest {
    name: heapless::String<NAME_MAX>,
    size: u32,
    mtime: u32,
    key: u64,
    raw_zero: bool,
}

/// Sort order of a candidate: files hashed this boot first (their ack is
/// one answer away), then finished, incomplete, live; oldest first.
#[allow(non_upper_case_globals)]
fn rank(c: &Candidate) -> (u8, u8, u32) {
    let hashed = super::served::get(c.key).is_some();
    let group = match c.kind as u32 {
        sync_kind_SK_FINISHED => 1,
        sync_kind_SK_INCOMPLETE => 2,
        _ => 3,
    };
    (u8::from(!hashed), group, c.mtime)
}

fn keep(cands: &mut heapless::Vec<(u8, u8, u32, Candidate), KEPT>, c: Candidate) {
    let r = rank(&c);
    let entry = (r.0, r.1, r.2, c);
    if cands.is_full() {
        let worst = cands
            .iter()
            .enumerate()
            .max_by_key(|(_, e)| (e.0, e.1, e.2))
            .map(|(i, e)| (i, (e.0, e.1, e.2)));
        match worst {
            Some((i, w)) if (entry.0, entry.1, entry.2) < w => cands[i] = entry,
            _ => {}
        }
    } else {
        let _ = cands.push(entry);
    }
}

fn pass(window: &ReadWindow, st: &mut State) {
    let geometry = window.geometry();
    let cluster = geometry
        .map_or(4096, |g| g.cluster_sectors * SECTOR)
        .max(SECTOR) as u64;
    let usb = usb_active();
    let dir = window.fat_path("SLIPPI");

    known::lock().clear_all(SEEN);
    let mut c = Counts::default();
    let mut used = 0u64;
    let mut newest: Option<Newest> = None;
    let mut cands: heapless::Vec<(u8, u8, u32, Candidate), KEPT> = heapless::Vec::new();
    let mut qualifying = 0u32;
    let mut fresh = 0u32;
    let mut bad = false;

    let walk = fat::for_each_entry(&dir, |e| {
        if e.dir || !wire::is_game_name(e.name) {
            return;
        }
        c.on_card += 1;
        let mtime = wire::fat_mtime(e.fdate, e.ftime);
        let short = heapless::String::<NAME_MAX>::try_from(e.name).ok();

        let consider = |newest: &mut Option<Newest>, key: u64, raw_zero: bool| {
            let Some(name) = short.clone() else { return };
            if newest.as_ref().is_none_or(|n| mtime >= n.mtime) {
                *newest = Some(Newest {
                    name,
                    size: e.size,
                    mtime,
                    key,
                    raw_zero,
                });
            }
        };

        if e.size == 0 {
            c.empty += 1;
            consider(&mut newest, wire::key(e.name, 0, mtime), true);
            return;
        }

        used += (e.size as u64).div_ceil(cluster) * cluster;
        let key = wire::key(e.name, e.size, mtime);
        let (seen_before, mut flags) = {
            let k = known::lock();
            (k.contains(key), k.flags(key))
        };
        if !seen_before {
            fresh += 1;
        }
        if flags & (FINISHED | INCOMPLETE) == 0 {
            flags |= class_of(window, e.name, e.size);
        }
        // cached when there is room; otherwise read again next pass
        known::lock().set(key, flags | SEEN);

        bad |= flags & BAD != 0;
        let finished = flags & FINISHED != 0;
        if !finished {
            c.incomplete += 1;
        }
        if flags & ACKED != 0 {
            c.to_erase += 1;
        } else {
            c.to_collect += 1;
            if let Some(name) = short.clone() {
                qualifying += 1;
                let kind = if finished {
                    sync_kind_SK_FINISHED
                } else {
                    sync_kind_SK_INCOMPLETE
                };
                keep(
                    &mut cands,
                    Candidate {
                        name,
                        size: e.size,
                        mtime,
                        kind: kind as u8,
                        key,
                    },
                );
            }
        }
        consider(&mut newest, key, !finished);
    });

    if let Err(res) = walk {
        log::warn!("inventory: could not list SLIPPI/: FRESULT {res}");
        warnings::set(WarningLabel::DriveFailing, true);
        return;
    }
    warnings::set(WarningLabel::DriveFailing, false);
    known::lock().retain(|f| f & (SEEN | ACKED) != 0);

    // live: raw length 0, the newest file, and the Wii talking
    let live = newest.filter(|n| n.raw_zero && usb);
    if let Some(l) = live.as_ref() {
        c.to_collect += u32::from(l.size == 0);
        if l.size == 0 {
            c.empty -= 1;
            qualifying += 1;
            keep(
                &mut cands,
                Candidate {
                    name: l.name.clone(),
                    size: 0,
                    mtime: l.mtime,
                    kind: sync_kind_SK_LIVE as u8,
                    key: l.key,
                },
            );
        } else {
            c.incomplete -= 1;
            if let Some(e) = cands.iter_mut().find(|e| e.3.key == l.key) {
                e.3.kind = sync_kind_SK_LIVE as u8;
                e.1 = 3;
            }
        }
    }
    cands.sort_unstable_by_key(|e| (e.0, e.1, e.2));
    let mut listed: heapless::Vec<Candidate, KEPT> = heapless::Vec::new();
    for e in cands.into_iter().take(LISTED) {
        let _ = listed.push(e.3);
    }

    follow_live(window, st, live.as_ref());

    // free space, from the FAT
    let due = st
        .fat
        .as_ref()
        .is_none_or(|f| f.at.elapsed() >= FAT_COUNT_EVERY && live.is_none() && used != f.used_then);
    if due {
        if let Some(free) = geometry.and_then(|g| count_free(&g)) {
            log::info!(
                "inventory: {free} free cluster(s) of {} by the FAT",
                geometry.map_or(0, |g| g.entries.saturating_sub(2))
            );
            st.fat = Some(FatCount {
                at: Instant::now(),
                free_clusters: free,
                used_then: used,
            });
        }
    }
    let free = st.fat.as_ref().map(|f| {
        (f.free_clusters as u64 * cluster).saturating_sub(used.saturating_sub(f.used_then))
    });
    let card = geometry.map_or(0, |g| g.entries.saturating_sub(2) as u64 * cluster);

    let filling = c.on_card >= FILLING_FILES || free.is_some_and(|f| f < FILLING_FREE);
    let space = match free {
        Some(f) if f < FULL_FREE => Space::Full,
        _ if filling => Space::Filling,
        Some(_) => Space::Ok,
        None => Space::Unknown,
    };
    warnings::set(WarningLabel::DriveFull, space == Space::Full);
    warnings::set(WarningLabel::DriveFilling, space == Space::Filling);
    warnings::set(WarningLabel::SlpMisformat, bad);
    if let Some(f) = free {
        let card_mb = (card / MIB) as u32;
        crate::status::set_files(card_mb.saturating_sub((f / MIB) as u32), card_mb.max(1));
    }
    crate::scan::set_replay_count(c.on_card);

    st.passes += 1;
    st.ticks_since_full = 0;
    let first = st.passes == 1;
    {
        let mut guard = snap();
        if let Some(s) = guard.as_mut() {
            s.passes = st.passes;
            s.counts = c;
            s.free_mb = free.map(|f| (f / MIB) as u32);
            s.card_mb = (card / MIB) as u32;
            s.used_mb = used.div_ceil(MIB) as u32;
            s.space = space;
            s.candidates = listed;
            s.more = qualifying as usize > LISTED;
            s.live_key = live.as_ref().map(|l| l.key);
        }
    }

    let known_files = c.on_card;
    if first {
        log::info!(
            "inventory: {} replay(s): {} to collect, {} to erase, {} empty, {} incomplete; {}",
            c.on_card,
            c.to_collect,
            c.to_erase,
            c.empty,
            c.incomplete,
            match free {
                Some(f) => format!("{} MB free", f / MIB),
                None => String::from("free space not counted"),
            },
        );
        super::sync::request_soon();
    } else if fresh > 0 || known_files != st.known_files {
        log::info!(
            "inventory: {} replay(s), {fresh} new; {} to collect, {} to erase",
            c.on_card,
            c.to_collect,
            c.to_erase,
        );
        super::sync::request_soon();
    }
    st.known_files = known_files;
}

/// Keeps `/status`'s game and the BUSY readout in step with the live file.
fn follow_live(window: &ReadWindow, st: &mut State, live: Option<&Newest>) {
    let same = match (st.live.as_ref(), live) {
        (Some(a), Some(b)) => a.name == b.name,
        (None, None) => true,
        _ => false,
    };
    if same {
        if let (Some(a), Some(b)) = (st.live.as_mut(), live) {
            a.size = b.size;
            a.mtime = b.mtime;
        }
        return;
    }

    if let Some(ended) = st.live.take() {
        log::info!("inventory: {} is no longer live", ended.name);
        if let Some(mut game) = peek_game(window, &ended.name) {
            game.live = false; // finished, or interrupted
            crate::scan::publish_game(&game, false);
        }
        crate::scan::set_game_live(false);
    }
    if let Some(l) = live {
        log::info!("inventory: {} is being written", l.name);
        crate::scan::set_game_live(true);
        if l.size > 0 {
            if let Some(mut game) = peek_game(window, &l.name) {
                game.live = true;
                crate::scan::publish_game(&game, true);
            }
        }
        st.live = Some(Live {
            name: l.name.clone(),
            size: l.size,
            mtime: l.mtime,
        });
    }
}

fn peek_game(window: &ReadWindow, name: &str) -> Option<slp::Game> {
    let file = std::fs::File::open(window.path(&format!("SLIPPI/{name}"))).ok()?;
    slp::peek_reader(file).ok()?.ok()
}

/// Free clusters, by reading the first FAT (through the write-back cache, so
/// the Wii's latest writes count). About 4 MB on a 4 GB card with 4 KB
/// clusters, in 4 KB reads into the HTTP scratch, so it waits for a time no
/// replay is being served. `None` when that is now, or a read fails.
fn count_free(g: &FatGeometry) -> Option<u32> {
    let entry = match g.fs_type {
        3 => 4usize, // FS_FAT32
        2 => 2,      // FS_FAT16
        _ => return None,
    };
    let mut scratch = crate::net::http::SCRATCH.try_lock().ok()?;
    let buf = &mut scratch.out;
    let per_read = (buf.len() as u32 / SECTOR).max(1);

    let mut free = 0u32;
    let mut index = 0u32;
    let mut sector = 0u32;
    let mut reads = 0u32;
    while sector < g.fat_sectors && index < g.entries {
        let n = per_read.min(g.fat_sectors - sector);
        let err =
            unsafe { beamer_wbc_read(g.fat_base + sector, buf.as_mut_ptr().cast(), n as usize, 0) };
        if err != ESP_OK {
            log::warn!("inventory: FAT read at +{sector} failed: 0x{err:x}");
            return None;
        }
        for e in buf[..(n * SECTOR) as usize].chunks_exact(entry) {
            if index >= g.entries {
                break;
            }
            let v = if entry == 4 {
                u32::from_le_bytes([e[0], e[1], e[2], e[3]]) & 0x0FFF_FFFF
            } else {
                u16::from_le_bytes([e[0], e[1]]) as u32
            };
            if index >= 2 && v == 0 {
                free += 1;
            }
            index += 1;
        }
        sector += n;
        reads += 1;
        if reads.is_multiple_of(64) {
            std::thread::sleep(Duration::from_millis(1)); // let the Wii's writes through
        }
    }
    Some(free)
}
