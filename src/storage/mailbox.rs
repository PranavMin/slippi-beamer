//! Safe wrapper over `components/beamer_lazyto`: the LazyTO mailbox, sectors
//! past the replay partition that a LazyTO Wii reads and writes instead of
//! using its own network. See LAZYTO.md.

use std::net::SocketAddrV4;
use std::time::Duration;

use esp_idf_svc::sys::lazyto::{
    beamer_lazyto_install, beamer_lazyto_installed, beamer_lazyto_resp_begin,
    beamer_lazyto_resp_commit, beamer_lazyto_set_hello, beamer_lazyto_stats, beamer_lazyto_stats_t,
    beamer_lazyto_take_request, beamer_lazyto_take_telemetry, beamer_lazyto_wait,
    BEAMER_LAZYTO_EV_REQUEST, BEAMER_LAZYTO_EV_TELEMETRY, BEAMER_LAZYTO_REQ_MAX,
    BEAMER_LAZYTO_RESP_MAX, BEAMER_LAZYTO_TELE_MAX, BEAMER_MB_SECTORS,
};

pub const SECTORS: u32 = BEAMER_MB_SECTORS;
pub const REQ_MAX: usize = BEAMER_LAZYTO_REQ_MAX as usize;
pub const RESP_MAX: usize = BEAMER_LAZYTO_RESP_MAX as usize;
pub const TELE_MAX: usize = BEAMER_LAZYTO_TELE_MAX as usize;

/// Serves the mailbox at `first`, the replay partition's end. Before the USB
/// bind only; the caller raises the visible size to `first + SECTORS`. False
/// when the heap could not spare its buffers.
pub fn install(first: u32) -> bool {
    unsafe { beamer_lazyto_install(first) }
}

pub fn installed() -> bool {
    unsafe { beamer_lazyto_installed() }
}

pub fn set_hello(flags: u8, station: u16, relay: Option<SocketAddrV4>) {
    let (ip, port) = relay.map_or((0, 0), |r| (u32::from(*r.ip()), r.port()));
    unsafe { beamer_lazyto_set_hello(flags, station, ip, port) }
}

#[derive(Debug, Clone, Copy)]
pub struct Events(u32);

impl Events {
    pub fn request(self) -> bool {
        self.0 & BEAMER_LAZYTO_EV_REQUEST != 0
    }

    pub fn telemetry(self) -> bool {
        self.0 & BEAMER_LAZYTO_EV_TELEMETRY != 0
    }
}

/// Waits for the host to hand something over, at most `timeout`.
pub fn wait(timeout: Duration) -> Events {
    Events(unsafe { beamer_lazyto_wait(timeout.as_millis() as u32) })
}

#[derive(Debug, Clone, Copy)]
pub struct Request {
    pub seq: u32,
    pub gen: u32,
    /// the sector was malformed: answer `BR_BAD_REQ`, send nothing
    pub bad: bool,
    pub len: usize,
}

pub fn take_request(out: &mut [u8]) -> Option<Request> {
    let (mut seq, mut gen, mut bad) = (0u32, 0u32, false);
    let n = unsafe {
        beamer_lazyto_take_request(out.as_mut_ptr(), out.len(), &mut seq, &mut gen, &mut bad)
    };
    (n >= 0).then_some(Request {
        seq,
        gen,
        bad,
        len: n as usize,
    })
}

pub fn take_telemetry(out: &mut [u8]) -> Option<usize> {
    let n = unsafe { beamer_lazyto_take_telemetry(out.as_mut_ptr(), out.len()) };
    (n >= 0).then_some(n as usize)
}

/// Answers `req`: `fill` writes the body straight into the mailbox and
/// returns the result code and the body length. The host sees seq 0 until
/// `fill` returns, then the whole answer at once.
///
/// Only the relay task calls this. The USB task reads the same bytes
/// meanwhile, but only ever reports them under the seq they were published
/// with (the seqlock in beamer_lazyto.c).
pub fn respond(req: &Request, fill: impl FnOnce(&mut [u8]) -> (u8, usize)) -> (u8, usize) {
    // SAFETY: the C side hands out one static RESP_MAX buffer; the borrow ends
    // before commit publishes it.
    let body = unsafe { core::slice::from_raw_parts_mut(beamer_lazyto_resp_begin(), RESP_MAX) };
    let (result, len) = fill(body);
    unsafe { beamer_lazyto_resp_commit(req.gen, req.seq, result, len) } // > RESP_MAX: BR_TOO_LARGE
    (result, len)
}

pub fn stats() -> beamer_lazyto_stats_t {
    let mut s = beamer_lazyto_stats_t::default();
    unsafe { beamer_lazyto_stats(&mut s) };
    s
}
