//! The beamer's sync (`CMD_BEAMER_SYNC`, LazyTO's docs/protocol-v2.md): the
//! one request the beamer makes on its own, on the relay link, never while a
//! Wii request waits. It tells the laptop what is on the card and asks about
//! up to 16 files; the laptop answers each one "held" (with the SHA-256 of
//! its stored copy), "wanted" (it will download it) or "noted".
//!
//! The reply is signed: HMAC-SHA256 keyed with the secret, over the
//! request's nonce, the beamer's station_id and the reply. Without it,
//! anyone on the Wi-Fi could download a file, answer "held" with its hash,
//! and make the beamer erase a replay nobody kept. A file is acked only when
//! the laptop's hash equals the one the beamer computed serving it this boot
//! (`served.rs`). The layout is frozen under `BEAMER_SYNC_VERSION`: dongles
//! have no over-the-air update.
//!
//! When: after each served file, when the inventory finds a new file, and
//! every 30 s; again soon when more files wait than one sync lists.

use std::io::{ErrorKind, Read as _, Write as _};
use std::mem::{offset_of, size_of};
use std::net::{SocketAddr, SocketAddrV4, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use esp_idf_svc::sys::lazyto::{
    beamer_sync_flags_SF_ACKS_DROPPED, beamer_sync_flags_SF_COLD_BOOT,
    beamer_sync_flags_SF_ERASE_FAILED, beamer_sync_flags_SF_MORE, beamer_sync_flags_SF_STATION_SET,
    beamer_sync_req, beamer_sync_resp, relay_auth, relay_cmd_CMD_BEAMER_SYNC, relay_hdr,
    relay_resp, relay_status_ST_OK, sync_answer, sync_answer_kind_SA_HELD,
    sync_answer_kind_SA_WANTED, sync_file, sync_kind_SK_LIVE, AUTH_MAGIC_0, AUTH_MAGIC_1,
    BEAMER_LAZYTO_FW_BUILD, BEAMER_SYNC_VERSION, RELAY_MAGIC_0, RELAY_MAGIC_1, SHA256_LEN,
    SYNC_ID_LEN,
};

use super::acks::{self, Record};
use super::hmac::{self, Sha256};
use super::inventory::{self, Candidate, LISTED};
use super::known::{self, ACKED};
use super::served::{self, Served};
use super::wire::{put_be16, put_be32, sat16};
use crate::config::Secret;
use crate::storage::mailbox::{self, Sha, ShaSlot};

const AUTH: usize = size_of::<relay_auth>();
const HDR: usize = size_of::<relay_hdr>();
const RESP: usize = size_of::<relay_resp>();
const REQ: usize = size_of::<beamer_sync_req>();
const FILE: usize = size_of::<sync_file>();
const REPLY: usize = size_of::<beamer_sync_resp>();
const ANSWER: usize = size_of::<sync_answer>();
const ID: usize = SYNC_ID_LEN as usize;
const SHA: usize = SHA256_LEN as usize;

/// A whole sync request: relay_auth + relay_hdr + beamer_sync_req + files.
pub const OUT_BYTES: usize = AUTH + HDR + REQ + LISTED * FILE;
/// The largest valid reply: relay_hdr + relay_resp + beamer_sync_resp + answers.
pub const IN_BYTES: usize = HDR + RESP + REPLY + LISTED * ANSWER;

const _: () = assert!(REQ == 100 && FILE == 84 && REPLY == 52 && ANSWER == 36);

const EVERY: Duration = Duration::from_secs(30);
const AGAIN: Duration = Duration::from_secs(2);
const BUDGET: Duration = Duration::from_millis(1500);
const CONNECT: Duration = Duration::from_millis(500);
/// Reads wait this long at most, so a Wii request never waits much longer.
const POLL: Duration = Duration::from_millis(100);
const HTTP_PORT: u16 = 80;

static DUE: AtomicBool = AtomicBool::new(false);

/// A sync soon: a file was served, or the inventory found a new one.
pub fn request_soon() {
    DUE.store(true, Ordering::Relaxed);
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    Ok {
        listed: u8,
        acked: u8,
        wanted: u8,
        /// "held" for a file without a matching hash this boot: not acked
        unproven: u8,
    },
    /// no full inventory yet
    NotReady,
    /// a Wii request arrived: it goes first, the sync is tried again
    GaveWay,
    Connect,
    Timeout,
    TooLarge,
    BadReply,
    /// relay_resp.status, e.g. ST_BAD_SECRET
    Refused(u8),
    BadSignature,
}

impl Outcome {
    pub fn name(self) -> &'static str {
        match self {
            Outcome::Ok { .. } => "ok",
            Outcome::NotReady => "not_ready",
            Outcome::GaveWay => "gave_way",
            Outcome::Connect => "connect",
            Outcome::Timeout => "timeout",
            Outcome::TooLarge => "too_large",
            Outcome::BadReply => "bad_reply",
            Outcome::Refused(_) => "refused",
            Outcome::BadSignature => "bad_signature",
        }
    }
}

/// What `/status` shows about the syncs.
#[derive(Debug, Clone, Copy)]
pub struct Status {
    pub attempts: u32,
    pub ok: u32,
    pub acked: u32,
    pub last: Option<Outcome>,
    pub last_at: Option<Instant>,
}

static STATUS: Mutex<Status> = Mutex::new(Status {
    attempts: 0,
    ok: 0,
    acked: 0,
    last: None,
    last_at: None,
});

pub fn status() -> Status {
    *STATUS.lock().unwrap_or_else(|e| e.into_inner())
}

/// The relay task's schedule.
pub struct Schedule {
    next: Instant,
    again: Option<Instant>,
}

impl Schedule {
    pub fn new() -> Schedule {
        Schedule {
            next: Instant::now() + EVERY,
            again: None,
        }
    }

    pub fn due(&self) -> bool {
        let now = Instant::now();
        DUE.load(Ordering::Relaxed) || now >= self.next || self.again.is_some_and(|t| now >= t)
    }

    /// One sync, on the relay task. `out` holds [`OUT_BYTES`], `reply`
    /// [`IN_BYTES`].
    pub fn run(&mut self, relay: SocketAddrV4, secret: &Secret, out: &mut [u8], reply: &mut [u8]) {
        DUE.store(false, Ordering::Relaxed);
        self.next = Instant::now() + EVERY;
        self.again = None;

        let started = Instant::now();
        let (outcome, more) = once(relay, secret, out, reply);
        let ms = started.elapsed().as_millis();

        match outcome {
            Outcome::GaveWay => {
                DUE.store(true, Ordering::Relaxed);
                return; // not an attempt: the Wii's request comes first
            }
            Outcome::NotReady => {
                self.again = Some(Instant::now() + AGAIN);
                return;
            }
            Outcome::Ok {
                listed,
                acked,
                wanted,
                unproven,
            } => {
                log::info!(
                    "sync: {listed} listed, {acked} acked, {wanted} wanted, {unproven} held unproven, in {ms} ms"
                );
                if more || acked > 0 {
                    self.again = Some(Instant::now() + AGAIN);
                }
            }
            Outcome::Refused(st) => {
                log::warn!("sync: the relay refused it (status {st}) in {ms} ms")
            }
            other => log::warn!("sync: {} after {ms} ms", other.name()),
        }

        let mut s = STATUS.lock().unwrap_or_else(|e| e.into_inner());
        s.attempts += 1;
        if let Outcome::Ok { acked, .. } = outcome {
            s.ok += 1;
            s.acked += acked as u32;
        }
        s.last = Some(outcome);
        s.last_at = Some(Instant::now());
    }
}

impl Default for Schedule {
    fn default() -> Self {
        Schedule::new()
    }
}

/// The SHA accelerator's sync slot. Without the mailbox (which holds the
/// contexts) it hashes nothing, and no signature verifies.
struct Engine(Option<Sha>);

impl Sha256 for Engine {
    fn update(&mut self, data: &[u8]) {
        if let Some(s) = self.0.as_mut() {
            s.update(data);
        }
    }

    fn finish(self) -> [u8; 32] {
        self.0.map_or([0; 32], Sha::finish)
    }
}

/// One file in this sync, and its hash if the beamer served it whole.
struct Listed {
    c: Candidate,
    served: Option<Served>,
}

fn once(relay: SocketAddrV4, secret: &Secret, out: &mut [u8], reply: &mut [u8]) -> (Outcome, bool) {
    let Some(snap) = inventory::snapshot() else {
        return (Outcome::NotReady, false);
    };
    if snap.passes == 0 {
        return (Outcome::NotReady, false);
    }

    // the inventory's files, minus any acked since; hashed ones first
    let mut files: heapless::Vec<Listed, LISTED> = heapless::Vec::new();
    for c in snap.candidates.iter().take(LISTED) {
        if known::lock().flags(c.key) & ACKED != 0 {
            continue;
        }
        let served = if c.kind as u32 == sync_kind_SK_LIVE {
            None
        } else {
            served::get(c.key).filter(|s| s.size == c.size && s.mtime == c.mtime)
        };
        let _ = files.push(Listed {
            c: c.clone(),
            served,
        });
    }
    files.sort_by_key(|f| f.served.is_none()); // stable: keeps the inventory's order
    let n = files.len();

    let mut nonce = [0u8; ID];
    mailbox::random(&mut nonce);
    let station_id = super::station_id();

    let len = encode(out, secret, &nonce, &station_id, &files, &snap);
    let got = match exchange(relay, &out[..len], reply) {
        Ok(got) => got,
        Err(o) => return (o, false),
    };

    let payload = match verify(&reply[..got], n, &nonce, &station_id, secret) {
        Ok(p) => p,
        Err(o) => return (o, false),
    };

    let mut archive_id = [0u8; ID];
    let at = offset_of!(beamer_sync_resp, archive_id);
    archive_id.copy_from_slice(&payload[at..at + ID]);
    if archive_id != acks::archive_id() {
        acks::adopt(archive_id);
    }

    let (mut acked, mut wanted, mut unproven) = (0u8, 0u8, 0u8);
    let answers = offset_of!(beamer_sync_resp, answers);
    for (i, f) in files.iter().enumerate() {
        let a = &payload[answers + i * ANSWER..answers + (i + 1) * ANSWER];
        let kind = a[offset_of!(sync_answer, answer)] as u32;
        if kind == sync_answer_kind_SA_WANTED {
            wanted += 1;
            continue;
        }
        if kind != sync_answer_kind_SA_HELD {
            continue;
        }
        let at = offset_of!(sync_answer, sha256);
        let theirs = &a[at..at + SHA];
        let Some(s) = f.served.filter(|s| s.sha[..] == *theirs) else {
            unproven += 1;
            continue;
        };
        match acks::add(Record {
            key: s.key,
            size: s.size,
            mtime: s.mtime,
            crc: s.crc,
        }) {
            Ok(()) => {
                acked += 1;
                log::info!("sync: {} acked; erased at the next power-on", f.c.name);
            }
            Err(e) => log::warn!("sync: {} held, but not acked: {e:?}", f.c.name),
        }
    }
    if acked > 0 {
        inventory::request_pass();
    }

    (
        Outcome::Ok {
            listed: n as u8,
            acked,
            wanted,
            unproven,
        },
        snap.more,
    )
}

/// Writes relay_auth + relay_hdr + beamer_sync_req + files into `out`;
/// returns the length.
fn encode(
    out: &mut [u8],
    secret: &Secret,
    nonce: &[u8; ID],
    station_id: &[u8; ID],
    files: &[Listed],
    snap: &inventory::Snapshot,
) -> usize {
    let body = REQ + files.len() * FILE;
    let len = AUTH + HDR + body;
    let b = &mut out[..len];
    b.fill(0);

    b[offset_of!(relay_auth, magic)] = AUTH_MAGIC_0 as u8;
    b[offset_of!(relay_auth, magic) + 1] = AUTH_MAGIC_1 as u8;
    let at = offset_of!(relay_auth, secret);
    b[at..at + secret.padded().len()].copy_from_slice(secret.padded());

    let station = crate::name::number();
    let set = crate::name::is_set();
    let h = &mut b[AUTH..AUTH + HDR];
    h[offset_of!(relay_hdr, magic)] = RELAY_MAGIC_0;
    h[offset_of!(relay_hdr, magic) + 1] = RELAY_MAGIC_1;
    h[offset_of!(relay_hdr, version)] = BEAMER_SYNC_VERSION as u8;
    h[offset_of!(relay_hdr, cmd)] = relay_cmd_CMD_BEAMER_SYNC as u8;
    put_be16(
        h,
        offset_of!(relay_hdr, station),
        if set { station } else { 0 },
    );
    put_be16(h, offset_of!(relay_hdr, len), body as u16);

    let counts = snap.counts;
    let report = super::erase::report();
    let mut flags = 0u32;
    if set {
        flags |= beamer_sync_flags_SF_STATION_SET;
    }
    if super::cold_boot() {
        flags |= beamer_sync_flags_SF_COLD_BOOT;
    }
    if report.failed {
        flags |= beamer_sync_flags_SF_ERASE_FAILED;
    }
    if report.acks_dropped {
        flags |= beamer_sync_flags_SF_ACKS_DROPPED;
    }
    if snap.more {
        flags |= beamer_sync_flags_SF_MORE;
    }
    let rssi = crate::net::wifi::link().map_or(0, |l| (-l.rssi).clamp(0, 255) as u8);

    let r = &mut b[AUTH + HDR..];
    macro_rules! at {
        ($f:ident) => {
            offset_of!(beamer_sync_req, $f)
        };
    }
    r[at!(station_id)..at!(station_id) + ID].copy_from_slice(station_id);
    r[at!(archive_id)..at!(archive_id) + ID].copy_from_slice(&acks::archive_id());
    r[at!(nonce)..at!(nonce) + ID].copy_from_slice(nonce);
    put_be32(r, at!(fw_build), BEAMER_LAZYTO_FW_BUILD);
    put_be32(r, at!(uptime_s), crate::scan::uptime_s() as u32);
    put_be32(r, at!(free_mb), snap.free_mb.unwrap_or(0));
    put_be32(r, at!(card_mb), snap.card_mb);
    put_be32(r, at!(used_mb), snap.used_mb);
    put_be16(r, at!(station), if set { station } else { 0 });
    put_be16(r, at!(http_port), HTTP_PORT);
    put_be16(r, at!(on_card), sat16(counts.on_card));
    put_be16(r, at!(to_collect), sat16(counts.to_collect));
    put_be16(r, at!(to_erase), sat16(counts.to_erase));
    put_be16(r, at!(empty), sat16(counts.empty));
    put_be16(r, at!(incomplete), sat16(counts.incomplete));
    put_be16(r, at!(acks), sat16(acks::count() as u32));
    if super::cold_boot() {
        put_be16(r, at!(erased), report.erased);
        put_be16(r, at!(erased_empty), report.erased_empty);
        put_be16(r, at!(erase_ms), report.erase_ms);
        put_be16(r, at!(erase_left), report.erase_left);
    }
    r[at!(flags)] = flags as u8;
    r[at!(storage)] = super::storage_state();
    r[at!(last_result)] = crate::net::relay::last_result();
    r[at!(rssi)] = rssi;
    r[at!(file_count)] = files.len() as u8;

    for (i, f) in files.iter().enumerate() {
        let e = &mut r[REQ + i * FILE..REQ + (i + 1) * FILE];
        let name = f.c.name.as_bytes();
        let at = offset_of!(sync_file, name);
        e[at..at + name.len()].copy_from_slice(name);
        put_be32(e, offset_of!(sync_file, bytes), f.c.size);
        put_be32(e, offset_of!(sync_file, mtime), f.c.mtime);
        e[offset_of!(sync_file, kind)] = f.c.kind;
        if let Some(s) = f.served {
            e[offset_of!(sync_file, hashed)] = 1;
            let at = offset_of!(sync_file, sha256);
            e[at..at + SHA].copy_from_slice(&s.sha);
        }
    }
    len
}

/// One TCP connection: send, read to EOF, inside BUDGET; gives way when a
/// Wii request arrives.
fn exchange(relay: SocketAddrV4, req: &[u8], reply: &mut [u8]) -> Result<usize, Outcome> {
    let deadline = Instant::now() + BUDGET;
    let left = || {
        let l = deadline.saturating_duration_since(Instant::now());
        (!l.is_zero()).then_some(l)
    };
    let mut s = TcpStream::connect_timeout(&SocketAddr::V4(relay), CONNECT).map_err(|e| {
        log::debug!("sync: connect to {relay}: {e}");
        Outcome::Connect
    })?;
    let l = left().ok_or(Outcome::Timeout)?;
    s.set_write_timeout(Some(l))
        .and_then(|()| s.write_all(req))
        .map_err(|e| failed(&e))?;

    let mut n = 0;
    loop {
        if mailbox::request_pending() {
            return Err(Outcome::GaveWay);
        }
        let l = left().ok_or(Outcome::Timeout)?;
        s.set_read_timeout(Some(l.min(POLL)))
            .map_err(|e| failed(&e))?;
        let mut spare = [0u8; 1];
        let into = if n < reply.len() {
            &mut reply[n..]
        } else {
            &mut spare[..]
        };
        match s.read(into) {
            Ok(0) => return Ok(n),
            Ok(_) if n == reply.len() => return Err(Outcome::TooLarge),
            Ok(k) => n += k,
            Err(e)
                if matches!(
                    e.kind(),
                    ErrorKind::WouldBlock | ErrorKind::TimedOut | ErrorKind::Interrupted
                ) => {}
            Err(e) => return Err(failed(&e)),
        }
    }
}

fn failed(e: &std::io::Error) -> Outcome {
    match e.kind() {
        ErrorKind::WouldBlock | ErrorKind::TimedOut => Outcome::Timeout,
        _ => Outcome::Connect,
    }
}

/// Checks the reply; returns its beamer_sync_resp payload.
fn verify<'a>(
    reply: &'a [u8],
    count: usize,
    nonce: &[u8; ID],
    station_id: &[u8; ID],
    secret: &Secret,
) -> Result<&'a [u8], Outcome> {
    if reply.len() < HDR + RESP {
        return Err(Outcome::BadReply);
    }
    let ok_header = reply[offset_of!(relay_hdr, magic)] == RELAY_MAGIC_0
        && reply[offset_of!(relay_hdr, magic) + 1] == RELAY_MAGIC_1
        && reply[offset_of!(relay_hdr, version)] == BEAMER_SYNC_VERSION as u8
        && reply[offset_of!(relay_hdr, cmd)] == relay_cmd_CMD_BEAMER_SYNC as u8
        && super::wire::be16(reply, offset_of!(relay_hdr, len)) as usize == reply.len() - HDR;
    if !ok_header {
        return Err(Outcome::BadReply);
    }
    let status = reply[HDR + offset_of!(relay_resp, status)];
    if status as u32 != relay_status_ST_OK {
        return Err(Outcome::Refused(status));
    }
    let payload = &reply[HDR + RESP..];
    if payload.len() != REPLY + count * ANSWER
        || payload[offset_of!(beamer_sync_resp, answer_count)] as usize != count
    {
        return Err(Outcome::BadReply);
    }
    let at = offset_of!(beamer_sync_resp, archive_id);
    if payload[at..at + ID].iter().all(|b| *b == 0) {
        return Err(Outcome::BadReply);
    }

    let signed_from = offset_of!(beamer_sync_resp, archive_id);
    let mac = hmac::hmac(
        || Engine(Sha::begin(ShaSlot::Sync)),
        secret.padded(),
        &[&nonce[..], &station_id[..], &payload[signed_from..]],
    );
    let at = offset_of!(beamer_sync_resp, hmac);
    if !hmac::equal(&mac, &payload[at..at + SHA]) {
        return Err(Outcome::BadSignature);
    }
    Ok(payload)
}
