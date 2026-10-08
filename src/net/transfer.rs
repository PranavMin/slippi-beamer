use std::ffi::{c_char, c_int, c_void, CStr};
use std::sync::atomic::{AtomicBool, AtomicPtr, AtomicU32, AtomicU8, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use esp_idf_svc::hal::cpu::Core;
use esp_idf_svc::hal::task::thread::ThreadSpawnConfiguration;
use esp_idf_svc::handle::RawHandle;
use esp_idf_svc::http::server::EspHttpServer;
use esp_idf_svc::sys::{
    esp_err_t, httpd_handle_t, httpd_register_uri_handler, httpd_req_async_handler_begin,
    httpd_req_async_handler_complete, httpd_req_get_hdr_value_len, httpd_req_get_hdr_value_str,
    httpd_req_t, httpd_req_to_sockfd, httpd_resp_send, httpd_resp_send_chunk, httpd_resp_set_hdr,
    httpd_resp_set_status, httpd_resp_set_type, httpd_sess_trigger_close, httpd_uri_t, ESP_OK,
};

use crate::scan;
use crate::storage::SdCard;

use super::gz;
use super::http;

const STACK: usize = 6144;
const MAX_NAME: usize = 96;
const MAX_HDR: usize = 128;
const WINDOW_WAIT: Duration = Duration::from_secs(2);

pub struct Job {
    req: *mut httpd_req_t,
    name: heapless::String<MAX_NAME>,
    start: u64,
    ranged: bool,
    resumed: bool,
    gzip: bool,
    level: i32,
    _transfer: super::Transfer,
}

unsafe impl Send for Job {}

static SLOT: Mutex<Option<Job>> = Mutex::new(None);
static WAKE: Condvar = Condvar::new();
static STOP: AtomicBool = AtomicBool::new(false);

static BUSY: AtomicBool = AtomicBool::new(false);

/// Where the current transfer is, for the journal's heartbeat: a transfer
/// that stops making progress shows the step it stopped in.
static STEP: AtomicU8 = AtomicU8::new(STEP_IDLE);
static STEP_BYTES: AtomicU32 = AtomicU32::new(0);
static STEP_AT_MS: AtomicU32 = AtomicU32::new(0);
const STEP_IDLE: u8 = 0;
const STEP_OPEN: u8 = 1;
const STEP_READ: u8 = 2;
const STEP_HASH: u8 = 3;
const STEP_SEND: u8 = 4;
const STEP_FINISH: u8 = 5;

fn step(s: u8, bytes: u64) {
    STEP.store(s, Ordering::Relaxed);
    STEP_BYTES.store(bytes.min(u32::MAX as u64) as u32, Ordering::Relaxed);
    STEP_AT_MS.store((http::now_us() / 1000) as u32, Ordering::Relaxed);
}

/// The current transfer's step, bytes read so far and ms since it entered
/// that step; `None` when no replay is being served.
pub fn progress_note() -> Option<String> {
    let s = STEP.load(Ordering::Relaxed);
    if s == STEP_IDLE {
        return None;
    }
    let name = match s {
        STEP_OPEN => "open",
        STEP_READ => "read",
        STEP_HASH => "hash",
        STEP_SEND => "send",
        _ => "finish",
    };
    let now = (http::now_us() / 1000) as u32;
    let idle = now.wrapping_sub(STEP_AT_MS.load(Ordering::Relaxed));
    Some(format!(
        "transfer in {name} at {} B read, {idle} ms in that step",
        STEP_BYTES.load(Ordering::Relaxed)
    ))
}
static SERVER_HANDLE: AtomicPtr<c_void> = AtomicPtr::new(std::ptr::null_mut());

fn close_async_session(fd: c_int) {
    let handle: httpd_handle_t = SERVER_HANDLE.load(Ordering::Acquire);
    if !handle.is_null() && fd >= 0 {
        unsafe { httpd_sess_trigger_close(handle, fd) };
    }
}

pub fn spawn(card: Arc<SdCard>) -> anyhow::Result<std::thread::JoinHandle<()>> {
    STOP.store(false, Ordering::SeqCst);
    ThreadSpawnConfiguration {
        name: Some(c"transfer"),
        stack_size: STACK,
        priority: 4,
        pin_to_core: Some(Core::Core0),
        ..Default::default()
    }
    .set()?;
    let h = std::thread::Builder::new()
        .stack_size(STACK)
        .spawn(move || worker(card))?;
    ThreadSpawnConfiguration::default().set()?;
    Ok(h)
}

pub fn shutdown(handle: Option<std::thread::JoinHandle<()>>) {
    STOP.store(true, Ordering::SeqCst);
    WAKE.notify_all();
    if let Some(h) = handle {
        if h.join().is_err() {
            log::error!("transfer worker panicked on the way down");
        }
    }
}

pub fn busy() -> bool {
    BUSY.load(Ordering::SeqCst)
}

fn worker(card: Arc<SdCard>) {
    log::info!("transfer worker up");
    loop {
        let job = {
            let mut slot = SLOT.lock().unwrap_or_else(|e| e.into_inner());
            while slot.is_none() && !STOP.load(Ordering::SeqCst) {
                slot = WAKE.wait(slot).unwrap_or_else(|e| e.into_inner());
            }
            match slot.take() {
                Some(j) => j,
                None => break,
            }
        };

        let raw = job.req;
        step(STEP_OPEN, 0);
        if let Err(e) = run(&card, &job) {
            log::error!("{}: transfer failed: {e}", job.name);
        }
        let fd = unsafe { httpd_req_to_sockfd(raw) };
        unsafe { httpd_req_async_handler_complete(raw) };
        close_async_session(fd);
        drop(job);
        step(STEP_IDLE, 0);
        BUSY.store(false, Ordering::SeqCst);
    }
    log::info!("transfer worker down");
}

fn enqueue(job: Job) -> Result<(), Job> {
    let mut slot = SLOT.lock().unwrap_or_else(|e| e.into_inner());
    if slot.is_some() || BUSY.swap(true, Ordering::SeqCst) {
        return Err(job);
    }
    *slot = Some(job);
    WAKE.notify_one();
    Ok(())
}

const S_200: &CStr = c"200 OK";
const S_206: &CStr = c"206 Partial Content";
const S_404: &CStr = c"404 Not Found";
const S_416: &CStr = c"416 Range Not Satisfiable";
const S_500: &CStr = c"500 Internal Server Error";
const S_503: &CStr = c"503 Service Unavailable";

const H_OCTET: &CStr = c"application/octet-stream";
const H_JSON: &CStr = c"application/json";

struct RawResponse(*mut httpd_req_t);

fn check(rc: esp_err_t) -> anyhow::Result<()> {
    if rc == ESP_OK {
        Ok(())
    } else {
        anyhow::bail!("esp_err {rc}")
    }
}

impl RawResponse {
    fn status(&self, s: &'static CStr) {
        unsafe { httpd_resp_set_status(self.0, s.as_ptr()) };
    }

    fn ctype(&self, t: &'static CStr) {
        unsafe { httpd_resp_set_type(self.0, t.as_ptr()) };
    }

    fn hdr(&self, k: &'static CStr, v: &CStr) {
        unsafe { httpd_resp_set_hdr(self.0, k.as_ptr(), v.as_ptr()) };
    }

    fn chunk(&self, b: &[u8]) -> anyhow::Result<()> {
        check(unsafe {
            httpd_resp_send_chunk(self.0, b.as_ptr() as *const c_char, b.len() as isize)
        })
    }

    fn finish(&self) -> anyhow::Result<()> {
        self.chunk(&[])
    }

    fn send(&self, status: &'static CStr, ctype: &'static CStr, body: &[u8]) -> esp_err_t {
        self.status(status);
        self.ctype(ctype);
        unsafe { httpd_resp_send(self.0, body.as_ptr() as *const c_char, body.len() as isize) }
    }
}

fn send_503(resp: &RawResponse, body: &[u8]) -> esp_err_t {
    let secs = std::ffi::CString::new(super::retry_after_secs().to_string());
    if let Ok(secs) = &secs {
        resp.hdr(c"Retry-After", secs);
    }
    resp.send(S_503, H_JSON, body)
}

fn run(card: &SdCard, job: &Job) -> anyhow::Result<()> {
    use std::io::{Read as _, Seek as _, SeekFrom};

    let resp = RawResponse(job.req);
    let t_start = http::now_us();

    let opened = match crate::storage::fat::ReadWindow::open_measured(card, WINDOW_WAIT) {
        Ok(Some(w)) => w,
        Ok(None) => {
            log::warn!(
                "{}: the read window stayed busy for {}s; refusing",
                job.name,
                WINDOW_WAIT.as_secs()
            );
            send_503(&resp, http::ERR_VOLUME);
            return Ok(());
        }
        Err(e) => {
            log::error!("{}: could not mount read-only: {e}", job.name);
            send_503(&resp, http::ERR_VOLUME);
            return Ok(());
        }
    };
    let (window, open) = opened;

    let path = window.path(&format!("SLIPPI/{}", job.name));
    let mut file = match std::fs::File::open(&path) {
        Ok(f) => f,
        Err(e) => {
            log::warn!("{path}: {e}");
            resp.send(S_404, H_JSON, http::ERR_NOT_FOUND);
            return Ok(());
        }
    };

    let len = match file.metadata() {
        Ok(m) => m.len(),
        Err(e) => {
            log::error!("{path}: could not stat: {e}");
            resp.send(S_500, H_JSON, http::ERR_STAT);
            return Ok(());
        }
    };
    let mut hashing = None;
    if crate::lazyto::enabled() {
        match lazyto_serve(&window, &job.name, len) {
            Serve::Live => {
                log::info!("{}: being recorded; refusing", job.name);
                send_503(&resp, http::ERR_LIVE);
                return Ok(());
            }
            Serve::Empty => {
                resp.send(S_404, H_JSON, http::ERR_NOT_FOUND);
                return Ok(());
            }
            Serve::Go(h) => hashing = h,
        }
        super::project(len.saturating_sub(job.start));
    }

    if job.start >= len {
        let cr = std::ffi::CString::new(format!("bytes */{len}"))?;
        resp.status(S_416);
        resp.ctype(H_JSON);
        resp.hdr(c"Content-Range", &cr);
        unsafe {
            httpd_resp_send(
                resp.0,
                http::ERR_RANGE.as_ptr() as *const c_char,
                http::ERR_RANGE.len() as isize,
            )
        };
        return Ok(());
    }

    let mut stream = if job.gzip {
        gz::Stream::begin(job.level)
    } else {
        None
    };
    let gzip = stream.is_some();

    let mut scratch = http::SCRATCH.lock().unwrap_or_else(|e| e.into_inner());
    let http::Scratch { read: buf, out } = &mut *scratch;

    if let Some(h) = hashing.as_mut() {
        // a resumed request: the skipped prefix is hashed first, never sent
        let mut left = job.start;
        while left > 0 {
            let want = (buf.len() as u64).min(left) as usize;
            let n = match file.read(&mut buf[..want]) {
                Ok(0) | Err(_) => {
                    log::error!("{path}: could not read the skipped prefix, {left} B short");
                    resp.send(S_500, H_JSON, http::ERR_STAT);
                    return Ok(());
                }
                Ok(n) => n,
            };
            h.feed(&buf[..n]);
            left -= n as u64;
        }
    } else if job.start > 0 {
        file.seek(SeekFrom::Start(job.start))?;
    }

    let content_range = std::ffi::CString::new(format!("bytes {}-{}/{len}", job.start, len - 1))?;
    let from_echo = std::ffi::CString::new(job.start.to_string())?;

    resp.status(if job.ranged { S_206 } else { S_200 });
    resp.ctype(H_OCTET);
    if job.ranged {
        resp.hdr(c"Accept-Ranges", c"bytes");
        resp.hdr(c"Content-Range", &content_range);
    } else if gzip {
        resp.hdr(c"Content-Encoding", c"gzip");
        resp.hdr(c"Vary", c"Accept-Encoding, X-Replay-From");
    } else {
        resp.hdr(c"Accept-Ranges", c"bytes");
    }
    if job.resumed && !job.ranged {
        resp.hdr(c"X-Replay-From", &from_echo);
    }

    crate::storage::msc::read_wait_reset();

    let mut sent = 0u64;
    let mut write_us = 0u32;
    let mut write_max_us = 0u32;
    let mut bytes = 0u64;
    let mut chunks = 0u32;
    let mut read_us = 0u32;
    let mut read_max_us = 0u32;
    let mut read_failed = false;

    {
        let mut sink = |block: &[u8]| -> anyhow::Result<()> {
            let t = http::now_us();
            resp.chunk(block)?;
            let took = (http::now_us() - t) as u32;
            write_us += took;
            write_max_us = write_max_us.max(took);
            sent += block.len() as u64;
            Ok(())
        };

        loop {
            step(STEP_READ, bytes);
            let t0 = http::now_us();
            let n = match file.read(buf) {
                Ok(0) => break,
                Ok(n) => n,
                Err(e) => {
                    log::error!("{path}: read failed after the header: {e}");
                    read_failed = true;
                    break;
                }
            };
            let took = (http::now_us() - t0) as u32;
            read_us += took;
            read_max_us = read_max_us.max(took);

            if let Some(h) = hashing.as_mut() {
                step(STEP_HASH, bytes);
                h.feed(&buf[..n]); // the raw bytes, before gzip
            }

            step(STEP_SEND, bytes);
            match stream.as_mut() {
                Some(gz) => gz.push(&buf[..n], out, &mut sink)?,
                None => sink(&buf[..n])?,
            }

            chunks += 1;
            bytes += n as u64;
        }

        step(STEP_FINISH, bytes);
        if let Some(gz) = stream.as_mut() {
            gz.finish(out, &mut sink)?;
        }
    }

    let (deflate_us, deflate_max_us) = stream
        .as_ref()
        .map_or((0, 0), |gz| (gz.deflate_us, gz.deflate_max_us));

    let mut stats = http::TransferStats {
        bytes,
        sent_bytes: sent,
        chunks,
        gzip,
        level: if gzip { job.level } else { 0 },
        read_us,
        read_max_us,
        write_us,
        write_max_us,
        deflate_us,
        deflate_max_us,
        lock_us: open.lock_us,
        mount_us: open.mount_us,
        stack_left: unsafe { esp_idf_svc::sys::uxTaskGetStackHighWaterMark(std::ptr::null_mut()) },
        ..http::TransferStats::default()
    };

    resp.finish()?;

    if let Some(h) = hashing.filter(|_| !read_failed) {
        if h.complete() {
            log::info!("{}: served whole and hashed", job.name);
            crate::lazyto::sync::request_soon();
        }
    }

    (stats.sd_wait_us, stats.sd_wait_max_us) = crate::storage::msc::read_wait();
    stats.total_us = (http::now_us() - t_start) as u32;
    http::publish_stats(stats);
    crate::journal::heap_checkin();
    Ok(())
}

enum Serve {
    /// the file being recorded right now: not served yet
    Live,
    /// a 0-byte entry: nothing to serve
    Empty,
    /// serve it, hashing it when the SHA accelerator's context is there
    Go(Option<crate::lazyto::served::Hashing>),
}

/// LazyTO mode: whether to serve `name` (opened, `len` bytes), and its hash.
fn lazyto_serve(window: &crate::storage::fat::ReadWindow, name: &str, len: u64) -> Serve {
    use crate::lazyto::{inventory, served, wire};

    if len == 0 {
        return Serve::Empty;
    }
    let Some((size, fdate, ftime)) =
        crate::storage::fat::stat(&window.fat_path(&format!("SLIPPI/{name}")))
    else {
        return Serve::Go(None);
    };
    let mtime = wire::fat_mtime(fdate, ftime);
    let key = wire::key(name, size, mtime);
    if inventory::live_key() == Some(key) {
        return Serve::Live;
    }
    if size as u64 != len {
        return Serve::Go(None); // changing under us: served, never acked
    }
    Serve::Go(served::Hashing::begin(key, size, mtime))
}

fn header(r: *mut httpd_req_t, key: &CStr) -> Option<heapless::String<MAX_HDR>> {
    let len = unsafe { httpd_req_get_hdr_value_len(r, key.as_ptr()) };
    if len == 0 || len >= MAX_HDR {
        return None;
    }
    let mut buf = [0u8; MAX_HDR];
    let rc = unsafe {
        httpd_req_get_hdr_value_str(r, key.as_ptr(), buf.as_mut_ptr() as *mut c_char, MAX_HDR)
    };
    if rc != ESP_OK {
        return None;
    }
    let s = CStr::from_bytes_until_nul(&buf).ok()?.to_str().ok()?;
    heapless::String::try_from(s).ok()
}

unsafe extern "C" fn handle(r: *mut httpd_req_t) -> esp_err_t {
    let resp = RawResponse(r);

    let Ok(uri) = CStr::from_bytes_until_nul(&(*r).uri)
        .map_err(|_| ())
        .and_then(|c| c.to_str().map_err(|_| ()))
    else {
        return resp.send(S_404, H_JSON, http::ERR_NOT_FOUND);
    };
    let Some(name) = http::replay_name(uri) else {
        log::warn!("refused {uri:?}: not a replay name");
        return resp.send(S_404, H_JSON, http::ERR_NOT_FOUND);
    };
    let Ok(name) = heapless::String::<MAX_NAME>::try_from(name) else {
        return resp.send(S_404, H_JSON, http::ERR_NOT_FOUND);
    };
    // LazyTO mode serves any replay by name (LAZYTO.md); its size, and
    // whether it is live, are checked against the card in the worker
    let indexed_len = if crate::lazyto::enabled() {
        None
    } else {
        let Some(len) = scan::published_size(&name) else {
            log::info!("refused {name}: not published");
            return resp.send(S_404, H_JSON, http::ERR_NOT_FOUND);
        };
        Some(len)
    };

    if let Some(short) = super::heap_too_low() {
        match short {
            super::HeapShort::Free(free) => log::error!(
                "refusing {name}: {free} B free is under the {} B floor",
                super::HEAP_FLOOR
            ),
            super::HeapShort::Fragmented(block) => log::error!(
                "refusing {name}: largest free block {block} B is under the {} B floor",
                super::BLOCK_FLOOR
            ),
        }
        return send_503(&resp, http::ERR_LOW_MEMORY);
    }

    if super::transfers_in_flight() > 0 || busy() {
        log::info!("refusing {name}: already serving a replay");
        return send_503(&resp, http::ERR_SERVING);
    }

    let range = http::parse_range(header(r, c"Range").as_deref());
    let ranged = matches!(range, http::RangeReq::From(_));
    let resume = match range {
        http::RangeReq::None => http::parse_from(header(r, c"X-Replay-From").as_deref()),
        _ => http::Resume::None,
    };
    let resumed = matches!(resume, http::Resume::At(_));

    let want = match (&range, &resume) {
        (http::RangeReq::Bad, _) | (_, http::Resume::Bad) => None,
        (http::RangeReq::From(n), _) | (http::RangeReq::None, http::Resume::At(n)) => {
            indexed_len.is_none_or(|len| *n < len).then_some(*n)
        }
        (http::RangeReq::None, http::Resume::None) => Some(0),
    };
    let Some(start) = want else {
        let Ok(cr) = std::ffi::CString::new(format!("bytes */{}", indexed_len.unwrap_or(0))) else {
            return resp.send(S_500, H_JSON, http::ERR_STAT);
        };
        resp.status(S_416);
        resp.ctype(H_JSON);
        resp.hdr(c"Accept-Ranges", c"bytes");
        resp.hdr(c"Content-Range", &cr);
        return httpd_resp_send(
            r,
            http::ERR_RANGE.as_ptr() as *const c_char,
            http::ERR_RANGE.len() as isize,
        );
    };

    let level = if http::debug_enabled() {
        gz::level(header(r, c"X-Beamer-Gz-Level").as_deref())
    } else {
        gz::LEVEL_DEFAULT
    };
    let gzip = !ranged && gz::accepted(header(r, c"Accept-Encoding").as_deref());

    let mut async_req: *mut httpd_req_t = std::ptr::null_mut();
    if httpd_req_async_handler_begin(r, &mut async_req) != ESP_OK || async_req.is_null() {
        log::error!("refusing {name}: could not take the request async");
        return send_503(&resp, http::ERR_LOW_MEMORY);
    }

    let job = Job {
        req: async_req,
        name,
        start,
        ranged,
        resumed,
        gzip,
        level,
        _transfer: super::Transfer::begin(indexed_len.map_or(0, |len| len - start)),
    };

    if let Err(job) = enqueue(job) {
        let async_req = job.req;
        drop(job);
        let late = RawResponse(async_req);
        send_503(&late, http::ERR_SERVING);
        let fd = httpd_req_to_sockfd(async_req);
        httpd_req_async_handler_complete(async_req);
        close_async_session(fd);
    }
    ESP_OK
}

pub fn register(server: &EspHttpServer<'static>) -> anyhow::Result<()> {
    SERVER_HANDLE.store(server.handle() as *mut c_void, Ordering::Release);
    let uri = c"/SLIPPI/*";
    let cfg = httpd_uri_t {
        uri: uri.as_ptr(),
        method: esp_idf_svc::sys::http_method_HTTP_GET,
        handler: Some(handle),
        user_ctx: std::ptr::null_mut::<c_void>(),
    };
    check(unsafe { httpd_register_uri_handler(server.handle(), &cfg) })
}
