//! LazyTO mode: the relay task. It finds the LazyTO relay on the LAN by its
//! UDP beacon, and carries the Wii's requests and telemetry from the mailbox
//! (`storage::mailbox`) to the relay and the answers back. A pure pipe: the
//! bytes are never parsed, and the Wii has already put the relay's shared
//! secret in them. See LAZYTO.md.
//!
//! Runs only when `CONFIG/config.txt` sets `LAZYTO=true`. Its buffers are
//! taken once, at boot; the response is read off the socket straight into
//! the mailbox, and nothing is allocated per request.

use std::io::{ErrorKind, Read as _, Write as _};
use std::mem::{offset_of, size_of};
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4, TcpStream, UdpSocket};
use std::sync::atomic::{AtomicBool, AtomicU16, AtomicU32, AtomicU8, Ordering};
use std::time::{Duration, Instant};

use esp_idf_svc::hal::cpu::Core;
use esp_idf_svc::hal::task::thread::ThreadSpawnConfiguration;
use esp_idf_svc::sys::lazyto::{
    beamer_flags_BF_RELAY, beamer_flags_BF_WIFI, beamer_result_BR_BAD_REQ,
    beamer_result_BR_CONNECT, beamer_result_BR_NO_RELAY, beamer_result_BR_NO_WIFI,
    beamer_result_BR_OK, beamer_result_BR_TIMEOUT, beamer_result_BR_TOO_LARGE, relay_beacon,
    BEACON_INTERVAL_MS, BEACON_PORT, RELAY_MAGIC_0, RELAY_MAGIC_1, RELAY_PROTO_VERSION,
    TELEMETRY_PORT,
};

use crate::storage::mailbox::{self, Request};

/// The whole round trip: connect, send, read to EOF. The Wii allows 3000 ms.
const BUDGET: Duration = Duration::from_millis(2500);
/// How long the task sleeps when the host hands nothing over: the HELLO
/// sector and the beacon socket are refreshed this often.
const IDLE: Duration = Duration::from_millis(250);
const BEACON_INTERVAL: Duration = Duration::from_millis(BEACON_INTERVAL_MS as u64);
const BEACON_LEN: usize = size_of::<relay_beacon>();
const STACK: usize = 4096;

const OK: u8 = beamer_result_BR_OK as u8;
const NO_RELAY: u8 = beamer_result_BR_NO_RELAY as u8;
const NO_WIFI: u8 = beamer_result_BR_NO_WIFI as u8;
const CONNECT: u8 = beamer_result_BR_CONNECT as u8;
const TIMEOUT: u8 = beamer_result_BR_TIMEOUT as u8;
const TOO_LARGE: u8 = beamer_result_BR_TOO_LARGE as u8;
const BAD_REQ: u8 = beamer_result_BR_BAD_REQ as u8;

const NO_RESULT: u8 = u8::MAX;

static RUNNING: AtomicBool = AtomicBool::new(false);
static RELAY_IP: AtomicU32 = AtomicU32::new(0);
static RELAY_PORT: AtomicU16 = AtomicU16::new(0);
static SERVED: AtomicU32 = AtomicU32::new(0);
static LAST: AtomicU8 = AtomicU8::new(NO_RESULT);

/// One request's or one datagram's bytes on their way out: whichever is
/// larger, since the task handles one at a time. Taken from the heap once,
/// at boot, so a station without LAZYTO does not pay for it.
const SCRATCH: usize = if mailbox::TELE_MAX > mailbox::REQ_MAX {
    mailbox::TELE_MAX
} else {
    mailbox::REQ_MAX
};

#[derive(Debug, Clone, Copy)]
pub struct Snapshot {
    pub relay: Option<SocketAddrV4>,
    pub served: u32,
    pub last: Option<&'static str>,
    pub mailbox: [(&'static str, u32); 7],
}

/// What `GET /status` shows; `None` unless LazyTO mode is running.
pub fn snapshot() -> Option<Snapshot> {
    if !RUNNING.load(Ordering::Relaxed) {
        return None;
    }
    let m = mailbox::stats();
    Some(Snapshot {
        relay: relay(),
        served: SERVED.load(Ordering::Relaxed),
        last: match LAST.load(Ordering::Relaxed) {
            NO_RESULT => None,
            r => Some(result_name(r)),
        },
        mailbox: [
            ("requests", m.requests),
            ("repeats", m.repeats),
            ("malformed", m.malformed),
            ("telemetry", m.telemetry),
            ("telemetry_bad", m.tele_bad),
            ("refused", m.refused),
            ("stale", m.stale),
        ],
    })
}

pub fn result_name(result: u8) -> &'static str {
    match result {
        OK => "ok",
        NO_RELAY => "no_relay",
        NO_WIFI => "no_wifi",
        CONNECT => "connect",
        TIMEOUT => "timeout",
        TOO_LARGE => "too_large",
        BAD_REQ => "bad_req",
        _ => "unknown",
    }
}

fn relay() -> Option<SocketAddrV4> {
    match RELAY_IP.load(Ordering::Relaxed) {
        0 => None,
        ip => Some(SocketAddrV4::new(
            Ipv4Addr::from(ip),
            RELAY_PORT.load(Ordering::Relaxed),
        )),
    }
}

pub fn spawn() -> anyhow::Result<()> {
    ThreadSpawnConfiguration {
        name: Some(c"relay"),
        stack_size: STACK,
        priority: 4,
        pin_to_core: Some(Core::Core0),
        ..Default::default()
    }
    .set()?;

    let out = vec![0u8; SCRATCH];
    let spawned = std::thread::Builder::new()
        .stack_size(STACK)
        .spawn(move || run(out));

    ThreadSpawnConfiguration::default().set()?;
    spawned?;
    RUNNING.store(true, Ordering::Relaxed);
    Ok(())
}

fn run(mut out: Vec<u8>) {
    let mut beacon: Option<UdpSocket> = None;
    let mut next_open = Instant::now();
    let mut next_ask = Instant::now();
    let mut found = false;

    log::info!("relay: listening for the beacon on udp {BEACON_PORT}");

    while !super::stopping() {
        let ip = crate::status::ip();

        // No socket before the station has an address: the net task brings
        // lwIP up after this task starts, and a socket call before that
        // trips lwIP's tcpip mbox assert (panic in task relay, first boot
        // on hardware, 2026-10-07).
        if beacon.is_none() && ip.is_some() && Instant::now() >= next_open {
            next_open = Instant::now() + BEACON_INTERVAL;
            beacon = open_beacon();
        }
        if let Some(socket) = beacon.as_ref() {
            hear_beacons(socket);
            if relay().is_none() && ip.is_some() && Instant::now() >= next_ask {
                next_ask = Instant::now() + BEACON_INTERVAL;
                ask_for_beacon(socket);
            }
        }

        let relay = relay();
        if relay.is_some() != found {
            found = relay.is_some();
            crate::status::set_relay(found);
        }
        let mut flags = 0u8;
        if ip.is_some() {
            flags |= beamer_flags_BF_WIFI as u8;
        }
        if relay.is_some() {
            flags |= beamer_flags_BF_RELAY as u8;
        }
        mailbox::set_hello(flags, crate::name::number(), relay);

        let events = mailbox::wait(IDLE);
        if !events.request() && !events.telemetry() {
            continue;
        }

        // the wait may have been long: catch up before going out
        if let Some(socket) = beacon.as_ref() {
            hear_beacons(socket);
        }
        let ip = crate::status::ip();
        let relay = self::relay();

        if events.telemetry() {
            if let Some(n) = mailbox::take_telemetry(&mut out[..]) {
                forward_telemetry(beacon.as_ref(), ip, relay, &out[..n]);
            }
        }

        if events.request() {
            if let Some(req) = mailbox::take_request(&mut out[..mailbox::REQ_MAX]) {
                answer(&req, &out[..req.len], ip, relay);
            }
        }
    }

    log::info!("relay: standing down");
}

fn open_beacon() -> Option<UdpSocket> {
    let opened = UdpSocket::bind(SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, BEACON_PORT as u16))
        .and_then(|s| {
            s.set_broadcast(true)?; // lwIP drops broadcasts to sockets without it
            s.set_nonblocking(true)?;
            Ok(s)
        });
    match opened {
        Ok(s) => Some(s),
        Err(e) => {
            log::warn!("relay: no beacon socket, retrying: {e}");
            None
        }
    }
}

fn hear_beacons(socket: &UdpSocket) {
    let mut buf = [0u8; BEACON_LEN + 1]; // one spare byte, so an oversized datagram shows
    loop {
        let (n, from) = match socket.recv_from(&mut buf) {
            Ok(got) => got,
            Err(e) if e.kind() == ErrorKind::WouldBlock => return,
            Err(e) => {
                log::warn!("relay: beacon socket: {e}");
                return;
            }
        };
        let SocketAddr::V4(from) = from else {
            continue;
        };
        let Some(port) = beacon_port(&buf[..n]) else {
            continue;
        };

        let ip = u32::from(*from.ip());
        if RELAY_IP.load(Ordering::Relaxed) != ip || RELAY_PORT.load(Ordering::Relaxed) != port {
            log::info!("relay: found at {}:{port}", from.ip());
            RELAY_PORT.store(port, Ordering::Relaxed);
            RELAY_IP.store(ip, Ordering::Relaxed);
        }
    }
}

/// The relay's TCP port, if `b` is a valid `relay_beacon`.
fn beacon_port(b: &[u8]) -> Option<u16> {
    let version = offset_of!(relay_beacon, version);
    let port = offset_of!(relay_beacon, tcp_port);
    if b.len() != BEACON_LEN
        || b[0] != RELAY_MAGIC_0
        || b[1] != RELAY_MAGIC_1
        || b[version] != RELAY_PROTO_VERSION as u8
    {
        return None;
    }
    let port = u16::from_be_bytes([b[port], b[port + 1]]);
    (port != 0).then_some(port)
}

/// The beacon request: a `relay_beacon` with tcp_port and event_id 0,
/// broadcast to the relay's telemetry port. Some access points never pass
/// the relay's broadcasts on; its unicast answer gets through.
fn ask_for_beacon(socket: &UdpSocket) {
    let mut b = [0u8; BEACON_LEN];
    b[0] = RELAY_MAGIC_0;
    b[1] = RELAY_MAGIC_1;
    b[offset_of!(relay_beacon, version)] = RELAY_PROTO_VERSION as u8;
    let to = SocketAddrV4::new(Ipv4Addr::BROADCAST, TELEMETRY_PORT as u16);
    if let Err(e) = socket.send_to(&b, to) {
        log::debug!("relay: beacon request not sent: {e}");
    }
}

fn forward_telemetry(
    socket: Option<&UdpSocket>,
    ip: Option<Ipv4Addr>,
    relay: Option<SocketAddrV4>,
    datagram: &[u8],
) {
    let (Some(socket), Some(_), Some(relay)) = (socket, ip, relay) else {
        return; // best effort, as on the Wii's own network
    };
    let to = SocketAddrV4::new(*relay.ip(), TELEMETRY_PORT as u16);
    if let Err(e) = socket.send_to(datagram, to) {
        log::debug!("relay: telemetry not sent: {e}");
    }
}

fn answer(req: &Request, bytes: &[u8], ip: Option<Ipv4Addr>, relay: Option<SocketAddrV4>) {
    let start = Instant::now();
    let (result, len) = mailbox::respond(req, |body| {
        if req.bad {
            (BAD_REQ, 0)
        } else if ip.is_none() {
            (NO_WIFI, 0)
        } else if let Some(relay) = relay {
            round_trip(relay, bytes, body)
        } else {
            (NO_RELAY, 0)
        }
    });

    SERVED.fetch_add(1, Ordering::Relaxed);
    LAST.store(result, Ordering::Relaxed);

    let ms = start.elapsed().as_millis();
    if result == OK {
        log::info!("relay: seq {} {} B -> {len} B in {ms} ms", req.seq, req.len);
    } else {
        log::warn!(
            "relay: seq {} {} B -> {} in {ms} ms",
            req.seq,
            req.len,
            result_name(result),
        );
    }
    if crate::journal::enabled() {
        let left = unsafe { esp_idf_svc::sys::uxTaskGetStackHighWaterMark(core::ptr::null_mut()) };
        log::info!("relay: {left} B of stack never used");
    }
}

/// One TCP connection to the relay: send `req`, read until the relay closes,
/// all inside BUDGET. Returns the result code and how much of `body` holds
/// the reply.
fn round_trip(relay: SocketAddrV4, req: &[u8], body: &mut [u8]) -> (u8, usize) {
    let deadline = Instant::now() + BUDGET;

    let mut stream = match TcpStream::connect_timeout(&SocketAddr::V4(relay), BUDGET) {
        Ok(s) => s,
        Err(e) => {
            log::warn!("relay: connect to {relay}: {e}");
            return (CONNECT, 0);
        }
    };

    let Some(left) = remaining(deadline) else {
        return (TIMEOUT, 0);
    };
    if let Err(e) = stream
        .set_write_timeout(Some(left))
        .and_then(|()| stream.write_all(req))
    {
        return (failed(&e, "send"), 0);
    }

    let mut n = 0;
    loop {
        let Some(left) = remaining(deadline) else {
            return (TIMEOUT, 0);
        };
        if let Err(e) = stream.set_read_timeout(Some(left)) {
            return (failed(&e, "read timeout"), 0);
        }

        let mut spare = [0u8; 1];
        let into = if n < body.len() {
            &mut body[n..]
        } else {
            &mut spare[..] // the mailbox is full: anything more is too much
        };
        match stream.read(into) {
            Ok(0) => return (OK, n),
            Ok(_) if n == body.len() => return (TOO_LARGE, 0),
            Ok(k) => n += k,
            Err(e) if e.kind() == ErrorKind::Interrupted => {}
            Err(e) => return (failed(&e, "read"), 0),
        }
    }
}

fn remaining(deadline: Instant) -> Option<Duration> {
    let left = deadline.saturating_duration_since(Instant::now());
    (!left.is_zero()).then_some(left)
}

fn failed(e: &std::io::Error, what: &str) -> u8 {
    match e.kind() {
        ErrorKind::WouldBlock | ErrorKind::TimedOut => TIMEOUT,
        _ => {
            log::warn!("relay: {what}: {e}");
            CONNECT
        }
    }
}
