//! LazyTO mode: the relay task. It finds the LazyTO relay on the LAN by its
//! UDP beacon, and carries the Wii's requests and telemetry from the mailbox
//! (`storage::mailbox`) to the relay and the answers back. A pipe: it puts
//! `relay_auth` (the key derived from LAZYTO-SECRET, never the secret,
//! `sync::put_auth`) in front of what the Wii wrote and never
//! parses the rest; the Wii holds no secret (mailbox v2, LazyTO's
//! docs/protocol-v2.md). Between requests it runs the beamer's own sync
//! (`lazyto::sync`).
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
    beamer_flags_BF_RELAY, beamer_flags_BF_SECRET, beamer_flags_BF_STATION_SET,
    beamer_flags_BF_WIFI, beamer_result_BR_BAD_REQ, beamer_result_BR_CONNECT,
    beamer_result_BR_NO_RELAY, beamer_result_BR_NO_SECRET, beamer_result_BR_NO_STATION,
    beamer_result_BR_NO_WIFI, beamer_result_BR_OK, beamer_result_BR_TIMEOUT,
    beamer_result_BR_TOO_LARGE, beamer_wifi_WIFI_UP, relay_auth, relay_beacon, BEACON_INTERVAL_MS,
    BEACON_PORT, BEACON_STALE_S, RELAY_MAGIC_0, RELAY_MAGIC_1, RELAY_PROTO_VERSION, TELEMETRY_PORT,
};

use crate::config::Secret;
use crate::lazyto::sync;
use crate::storage::mailbox::{self, Hello, Request};

/// The whole round trip: connect, send, read to EOF. The Wii allows 3000 ms.
const BUDGET: Duration = Duration::from_millis(2500);
/// How long the task sleeps when the host hands nothing over: the HELLO
/// sector and the beacon socket are refreshed this often.
const IDLE: Duration = Duration::from_millis(250);
const BEACON_INTERVAL: Duration = Duration::from_millis(BEACON_INTERVAL_MS as u64);
const BEACON_STALE: Duration = Duration::from_secs(BEACON_STALE_S as u64);
const BEACON_LEN: usize = size_of::<relay_beacon>();
const AUTH: usize = size_of::<relay_auth>();
/// The sync (building, signing, checking) runs here too, so more than the
/// 4 KB the pipe alone had.
const STACK: usize = 6144;

const OK: u8 = beamer_result_BR_OK as u8;
const NO_RELAY: u8 = beamer_result_BR_NO_RELAY as u8;
const NO_WIFI: u8 = beamer_result_BR_NO_WIFI as u8;
const CONNECT: u8 = beamer_result_BR_CONNECT as u8;
const TIMEOUT: u8 = beamer_result_BR_TIMEOUT as u8;
const TOO_LARGE: u8 = beamer_result_BR_TOO_LARGE as u8;
const BAD_REQ: u8 = beamer_result_BR_BAD_REQ as u8;
const NO_STATION: u8 = beamer_result_BR_NO_STATION as u8;
const NO_SECRET: u8 = beamer_result_BR_NO_SECRET as u8;

const NO_RESULT: u8 = u8::MAX;

static RUNNING: AtomicBool = AtomicBool::new(false);
static RELAY_IP: AtomicU32 = AtomicU32::new(0);
static RELAY_PORT: AtomicU16 = AtomicU16::new(0);
static SERVED: AtomicU32 = AtomicU32::new(0);
static LAST: AtomicU8 = AtomicU8::new(NO_RESULT);
static BEACON_AGE: AtomicU16 = AtomicU16::new(u16::MAX);
static TELE_DROPPED: AtomicU32 = AtomicU32::new(0);

/// One request's or one datagram's bytes on their way out, with
/// `relay_auth` in front, or one sync request: whichever is largest, since
/// the task does one at a time. Taken from the heap once, at boot, so a
/// station without LAZYTO does not pay for it.
const OUT: usize = {
    let pipe = AUTH
        + if mailbox::TELE_MAX > mailbox::REQ_MAX {
            mailbox::TELE_MAX
        } else {
            mailbox::REQ_MAX
        };
    if sync::OUT_BYTES > pipe {
        sync::OUT_BYTES
    } else {
        pipe
    }
};

#[derive(Debug, Clone, Copy)]
pub struct Snapshot {
    pub relay: Option<SocketAddrV4>,
    pub beacon_age_s: Option<u16>,
    pub served: u32,
    pub last: Option<&'static str>,
    pub telemetry_dropped: u32,
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
        beacon_age_s: relay().map(|_| BEACON_AGE.load(Ordering::Relaxed)),
        served: SERVED.load(Ordering::Relaxed),
        last: match LAST.load(Ordering::Relaxed) {
            NO_RESULT => None,
            r => Some(result_name(r)),
        },
        telemetry_dropped: TELE_DROPPED.load(Ordering::Relaxed),
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

/// The last mailbox round trip's `beamer_result`; 0 before the first.
pub fn last_result() -> u8 {
    match LAST.load(Ordering::Relaxed) {
        NO_RESULT => OK,
        r => r,
    }
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
        NO_STATION => "no_station",
        NO_SECRET => "no_secret",
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

    let out = vec![0u8; OUT];
    let reply = vec![0u8; sync::IN_BYTES];
    let spawned = std::thread::Builder::new()
        .stack_size(STACK)
        .spawn(move || run(out, reply));

    ThreadSpawnConfiguration::default().set()?;
    spawned?;
    RUNNING.store(true, Ordering::Relaxed);
    Ok(())
}

/// The beacon socket and when the relay was last heard.
struct Beacon {
    socket: Option<UdpSocket>,
    next_open: Instant,
    next_ask: Instant,
    heard: Option<Instant>,
}

impl Beacon {
    fn age(&self) -> Option<Duration> {
        self.heard.map(|t| t.elapsed())
    }

    fn age_s(&self) -> u16 {
        self.age()
            .map_or(u16::MAX, |a| a.as_secs().min(u16::MAX as u64) as u16)
    }
}

fn run(mut out: Vec<u8>, mut reply: Vec<u8>) {
    let mut beacon = Beacon {
        socket: None,
        next_open: Instant::now(),
        next_ask: Instant::now(),
        heard: None,
    };
    let mut found = false;
    let mut schedule = sync::Schedule::new();

    log::info!("relay: listening for the beacon on udp {BEACON_PORT}");

    while !super::stopping() {
        let ip = crate::status::ip();
        refresh_beacon(&mut beacon, ip.is_some());

        let relay = relay();
        if relay.is_some() != found {
            found = relay.is_some();
            crate::status::set_relay(found);
        }
        publish_hello(relay, &beacon);

        let events = mailbox::wait(IDLE);
        if events.request() || events.telemetry() {
            // the wait may have been long: catch up before going out
            if let Some(socket) = beacon.socket.as_ref() {
                hear_beacons(socket, &mut beacon.heard);
            }
            let ip = crate::status::ip();
            let relay = self::relay();
            let secret = crate::lazyto::secret();

            if events.telemetry() {
                forward_telemetry(beacon.socket.as_ref(), ip, relay, secret.as_ref(), &mut out);
            }
            if events.request() {
                if let Some(req) = mailbox::take_request(&mut out[AUTH..AUTH + mailbox::REQ_MAX]) {
                    answer(&req, &mut out, ip, relay, secret.as_ref());
                }
            }
            continue;
        }

        // idle: the beamer's own sync, only while no request waits
        let secret = crate::lazyto::secret();
        if let (Some(relay), Some(_), Some(secret)) = (relay, ip, secret) {
            if schedule.due() && !mailbox::request_pending() {
                schedule.run(relay, &secret, &mut out, &mut reply);
            }
        }
    }

    log::info!("relay: standing down");
}

fn refresh_beacon(b: &mut Beacon, have_ip: bool) {
    // No socket before the station has an address: the net task brings lwIP
    // up after this task starts, and a socket call before that trips lwIP's
    // tcpip mbox assert (panic in task relay, first boot on hardware,
    // 2026-10-07).
    if b.socket.is_none() && have_ip && Instant::now() >= b.next_open {
        b.next_open = Instant::now() + BEACON_INTERVAL;
        b.socket = open_beacon();
    }
    let Some(socket) = b.socket.as_ref() else {
        return;
    };
    hear_beacons(socket, &mut b.heard);
    // ask while no beacon reaches us: some access points drop broadcasts to
    // a client, and the relay's unicast answer gets through
    let quiet = b.age().is_none_or(|a| a > BEACON_STALE);
    if quiet && have_ip && Instant::now() >= b.next_ask {
        b.next_ask = Instant::now() + BEACON_INTERVAL;
        ask_for_beacon(socket);
    }
    BEACON_AGE.store(b.age_s(), Ordering::Relaxed);
}

fn publish_hello(relay: Option<SocketAddrV4>, beacon: &Beacon) {
    let wifi = crate::lazyto::wifi_state();
    let station_set = crate::name::is_set();
    let mut flags = 0u32;
    if wifi as u32 == beamer_wifi_WIFI_UP {
        flags |= beamer_flags_BF_WIFI;
    }
    if relay.is_some() {
        flags |= beamer_flags_BF_RELAY;
    }
    if station_set {
        flags |= beamer_flags_BF_STATION_SET;
    }
    if crate::lazyto::secret().is_some() {
        flags |= beamer_flags_BF_SECRET;
    }
    mailbox::set_hello(&Hello {
        flags: flags as u8,
        station: if station_set {
            crate::name::number()
        } else {
            0
        },
        relay,
        wifi,
        storage: crate::lazyto::storage_state(),
        last_result: last_result(),
        beacon_age_s: if relay.is_some() { beacon.age_s() } else { 0 },
    });
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

fn hear_beacons(socket: &UdpSocket, heard: &mut Option<Instant>) {
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

        *heard = Some(Instant::now());
        let ip = u32::from(*from.ip());
        if RELAY_IP.load(Ordering::Relaxed) != ip || RELAY_PORT.load(Ordering::Relaxed) != port {
            log::info!("relay: found at {}:{port}", from.ip());
            RELAY_PORT.store(port, Ordering::Relaxed);
            RELAY_IP.store(ip, Ordering::Relaxed);
        }
    }
}

/// The relay's TCP port, if `b` is a valid `relay_beacon`: exactly its
/// length, its magic, and a nonzero port. Its version is never compared, so
/// a protocol bump never strands a beamer (dongles have no over-the-air
/// update).
fn beacon_port(b: &[u8]) -> Option<u16> {
    let port = offset_of!(relay_beacon, tcp_port);
    if b.len() != BEACON_LEN || b[0] != RELAY_MAGIC_0 || b[1] != RELAY_MAGIC_1 {
        return None;
    }
    let port = u16::from_be_bytes([b[port], b[port + 1]]);
    (port != 0).then_some(port)
}

/// The beacon request: a `relay_beacon` with tcp_port and event_id 0,
/// broadcast to the relay's telemetry port. The relay answers it whatever
/// its version.
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

/// One telemetry datagram: `relay_auth` + what the kernel wrote, to the
/// relay's telemetry port. Dropped without a number or a secret (the relay
/// would take an unnumbered station for Dolphin's station 0), or without
/// the Wi-Fi or a relay: best effort, as on the Wii's own network.
fn forward_telemetry(
    socket: Option<&UdpSocket>,
    ip: Option<Ipv4Addr>,
    relay: Option<SocketAddrV4>,
    secret: Option<&Secret>,
    out: &mut [u8],
) {
    let Some(n) = mailbox::take_telemetry(&mut out[AUTH..AUTH + mailbox::TELE_MAX]) else {
        return;
    };
    let (Some(socket), Some(_), Some(relay), Some(secret), true) =
        (socket, ip, relay, secret, crate::name::is_set())
    else {
        TELE_DROPPED.fetch_add(1, Ordering::Relaxed);
        return;
    };
    sync::put_auth(out, secret);
    let to = SocketAddrV4::new(*relay.ip(), TELEMETRY_PORT as u16);
    if let Err(e) = socket.send_to(&out[..AUTH + n], to) {
        log::debug!("relay: telemetry not sent: {e}");
    }
}

/// One request: `out` holds the Wii's bytes at `AUTH..`; refused locally
/// when the beamer cannot send it, else `relay_auth` goes in front.
fn answer(
    req: &Request,
    out: &mut [u8],
    ip: Option<Ipv4Addr>,
    relay: Option<SocketAddrV4>,
    secret: Option<&Secret>,
) {
    let start = Instant::now();
    let (result, len) = mailbox::respond(req, |body| {
        if req.bad {
            (BAD_REQ, 0)
        } else if !crate::name::is_set() {
            (NO_STATION, 0)
        } else if secret.is_none() {
            (NO_SECRET, 0)
        } else if ip.is_none() {
            (NO_WIFI, 0)
        } else if let (Some(relay), Some(secret)) = (relay, secret) {
            sync::put_auth(out, secret);
            round_trip(relay, &out[..AUTH + req.len], body)
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
