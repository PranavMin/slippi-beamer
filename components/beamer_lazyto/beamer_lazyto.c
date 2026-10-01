/*
 * See include/beamer_lazyto.h for documentation, and LAZYTO.md for the design.
 *
 * Two sides touch this file's state:
 *
 *  - the USB side: beamer_lazyto_transfer and beamer_lazyto_host_reset, from
 *    the TinyUSB callbacks in the beamer_msc task (priority 22, core 1). They
 *    only memcpy and give s_wake - never block, never log on the hot path.
 *  - the relay task (src/net/relay.rs, core 0): everything else.
 *
 * HELLO, REQUEST and TELEMETRY are small and copied whole under s_mux. The
 * RESPONSE is 4 KB and is filled straight off the relay's socket, so it is a
 * seqlock instead, with seq 0 as the "being written" marker:
 *
 *   writer (relay task)                 reader (USB task)
 *   seq = 0; fence(release)             s1 = load_acquire(seq)
 *   body = ...                          copy result, len, body
 *   result, len = ...; store_release    fence(acquire); s2 = load(seq)
 *     (seq = n)                         header seq = (s1 == s2) ? s1 : 0
 *
 * A host that reads seq n therefore reads the body that was complete when n
 * was published; a read that overlapped a write reports seq 0, which the Wii
 * treats as "no response yet" and polls again.
 */

#include "beamer_lazyto.h"

#include <stdatomic.h>
#include <string.h>

#include "esp_heap_caps.h"
#include "esp_log.h"
#include "freertos/FreeRTOS.h"
#include "freertos/semphr.h"
#include "freertos/task.h"

static const char *TAG = "beamer_lazyto";

#define SECTOR BEAMER_SECTOR_SIZE
#define RESP_BYTES (BEAMER_MB_RESP_SECTORS * SECTOR)
#define TELE_BYTES (BEAMER_MB_TELE_SECTORS * SECTOR)

_Static_assert(BEAMER_LAZYTO_REQ_MAX == SECTOR - sizeof(struct beamer_req_hdr), "request size");
_Static_assert(BEAMER_LAZYTO_RESP_MAX == RESP_BYTES - sizeof(struct beamer_resp_hdr),
               "response size");
_Static_assert(BEAMER_LAZYTO_TELE_MAX == TELE_BYTES - sizeof(struct beamer_tele_hdr),
               "telemetry size");
_Static_assert(BEAMER_MB_RESP + BEAMER_MB_RESP_SECTORS <= BEAMER_MB_TELE, "layout overlaps");
_Static_assert(BEAMER_MB_TELE + BEAMER_MB_TELE_SECTORS <= BEAMER_MB_SECTORS, "layout overflows");
_Static_assert(sizeof(struct beamer_hello) <= SECTOR, "hello fits a sector");

static atomic_bool s_on;
static uint32_t s_first; // set once by beamer_lazyto_install, before the USB bind

static portMUX_TYPE s_mux = portMUX_INITIALIZER_UNLOCKED;
static SemaphoreHandle_t s_wake;
static StaticSemaphore_t s_wake_buf;

// The sector buffers come off the heap once, at boot and only in LazyTO
// mode, so a station without LAZYTO keeps upstream's heap to the byte.
typedef struct
{
    uint8_t req[SECTOR];                       // under s_mux: as last written
    uint8_t tele[TELE_BYTES];                  // under s_mux: as last written
    uint8_t resp_body[BEAMER_LAZYTO_RESP_MAX]; // the response seqlock
} mailbox_t;

static mailbox_t *s_mb;

// --- under s_mux ----------------------------------------------------------
static uint8_t s_hello[sizeof(struct beamer_hello)];

static bool s_req_pending;
static bool s_req_bad;
static uint32_t s_req_seq;
static uint32_t s_req_gen;
static uint16_t s_req_len;

static bool s_tele_pending;
static uint16_t s_tele_len;

static uint32_t s_gen; // host sessions: bumped on every (re)configure or drop

// --- USB task only --------------------------------------------------------
static uint32_t s_req_last;  // last accepted request seq, 0 = none
static uint32_t s_tele_last; // last accepted telemetry seq
static uint32_t s_cmd_seq;   // the response seq the current READ(10) went out with...
static bool s_cmd_seq_set;   // ...once one of its chunks has carried a response sector

// --- the response seqlock (see the top of this file), with s_mb->resp_body
static uint8_t s_resp_result;
static uint16_t s_resp_len;
static atomic_uint s_resp_seq;

static atomic_uint s_requests;
static atomic_uint s_malformed;
static atomic_uint s_repeats;
static atomic_uint s_telemetry;
static atomic_uint s_tele_bad;
static atomic_uint s_refused;
static atomic_uint s_stale;

static inline uint16_t rd16(const uint8_t *p)
{
    return (uint16_t)((p[0] << 8) | p[1]);
}

static inline uint32_t rd32(const uint8_t *p)
{
    return ((uint32_t)p[0] << 24) | ((uint32_t)p[1] << 16) | ((uint32_t)p[2] << 8) | p[3];
}

static inline void wr16(uint8_t *p, uint16_t v)
{
    p[0] = (uint8_t)(v >> 8);
    p[1] = (uint8_t)v;
}

static inline void wr32(uint8_t *p, uint32_t v)
{
    p[0] = (uint8_t)(v >> 24);
    p[1] = (uint8_t)(v >> 16);
    p[2] = (uint8_t)(v >> 8);
    p[3] = (uint8_t)v;
}

static inline void count(atomic_uint *c)
{
    atomic_fetch_add_explicit(c, 1, memory_order_relaxed);
}

bool beamer_lazyto_install(uint32_t first)
{
    s_mb = heap_caps_calloc(1, sizeof(mailbox_t), MALLOC_CAP_INTERNAL | MALLOC_CAP_8BIT);
    if (s_mb == NULL)
    {
        ESP_LOGE(TAG, "no %u B for the mailbox", (unsigned)sizeof(mailbox_t));
        return false;
    }
    s_first = first;
    s_wake = xSemaphoreCreateBinaryStatic(&s_wake_buf);

    memset(s_hello, 0, sizeof(s_hello));
    memcpy(s_hello + offsetof(struct beamer_hello, magic), "LAZYTOMB", 8);
    s_hello[offsetof(struct beamer_hello, version)] = BEAMER_MB_VERSION;
    wr16(s_hello + offsetof(struct beamer_hello, station), 1);
    wr32(s_hello + offsetof(struct beamer_hello, fw_build), BEAMER_LAZYTO_FW_BUILD);

    atomic_store(&s_on, true);
    ESP_LOGI(TAG, "mailbox at lba %u..%u", (unsigned)first,
             (unsigned)(first + BEAMER_MB_SECTORS - 1));
    return true;
}

bool beamer_lazyto_installed(void)
{
    return atomic_load(&s_on);
}

void beamer_lazyto_set_hello(uint8_t flags, uint16_t station, uint32_t relay_ip,
                             uint16_t relay_port)
{
    portENTER_CRITICAL(&s_mux);
    s_hello[offsetof(struct beamer_hello, flags)] = flags;
    wr16(s_hello + offsetof(struct beamer_hello, station), station);
    wr32(s_hello + offsetof(struct beamer_hello, relay_ip), relay_ip);
    wr16(s_hello + offsetof(struct beamer_hello, relay_port), relay_port);
    portEXIT_CRITICAL(&s_mux);
}

uint32_t beamer_lazyto_wait(uint32_t timeout_ms)
{
    if (s_wake == NULL)
    {
        vTaskDelay(pdMS_TO_TICKS(timeout_ms));
        return 0;
    }
    xSemaphoreTake(s_wake, pdMS_TO_TICKS(timeout_ms));

    portENTER_CRITICAL(&s_mux);
    const uint32_t bits = (s_req_pending ? BEAMER_LAZYTO_EV_REQUEST : 0) |
                          (s_tele_pending ? BEAMER_LAZYTO_EV_TELEMETRY : 0);
    portEXIT_CRITICAL(&s_mux);
    return bits;
}

int32_t beamer_lazyto_take_request(uint8_t *out, size_t cap, uint32_t *seq, uint32_t *gen,
                                   bool *bad)
{
    int32_t n = -1;
    portENTER_CRITICAL(&s_mux);
    if (s_req_pending)
    {
        const size_t len = s_req_len < cap ? s_req_len : cap;
        memcpy(out, s_mb->req + sizeof(struct beamer_req_hdr), len);
        *seq = s_req_seq;
        *gen = s_req_gen;
        *bad = s_req_bad;
        s_req_pending = false;
        n = (int32_t)len;
    }
    portEXIT_CRITICAL(&s_mux);
    return n;
}

uint8_t *beamer_lazyto_resp_begin(void)
{
    atomic_store_explicit(&s_resp_seq, 0, memory_order_relaxed);
    atomic_thread_fence(memory_order_release);
    return s_mb->resp_body;
}

void beamer_lazyto_resp_commit(uint32_t gen, uint32_t seq, uint8_t result, size_t len)
{
    if (len > BEAMER_LAZYTO_RESP_MAX)
    {
        len = 0;
        result = BR_TOO_LARGE;
    }

    portENTER_CRITICAL(&s_mux);
    const bool current = gen == s_gen;
    if (current)
    {
        s_resp_result = result;
        s_resp_len = (uint16_t)len;
        atomic_store_explicit(&s_resp_seq, seq, memory_order_release);
    }
    portEXIT_CRITICAL(&s_mux);

    if (!current)
    {
        count(&s_stale);
    }
}

int32_t beamer_lazyto_take_telemetry(uint8_t *out, size_t cap)
{
    int32_t n = -1;
    portENTER_CRITICAL(&s_mux);
    if (s_tele_pending)
    {
        const size_t len = s_tele_len < cap ? s_tele_len : cap;
        memcpy(out, s_mb->tele + sizeof(struct beamer_tele_hdr), len);
        s_tele_pending = false;
        n = (int32_t)len;
    }
    portEXIT_CRITICAL(&s_mux);
    return n;
}

void beamer_lazyto_stats(beamer_lazyto_stats_t *out)
{
    if (out == NULL)
    {
        return;
    }
    out->requests = atomic_load_explicit(&s_requests, memory_order_relaxed);
    out->malformed = atomic_load_explicit(&s_malformed, memory_order_relaxed);
    out->repeats = atomic_load_explicit(&s_repeats, memory_order_relaxed);
    out->telemetry = atomic_load_explicit(&s_telemetry, memory_order_relaxed);
    out->tele_bad = atomic_load_explicit(&s_tele_bad, memory_order_relaxed);
    out->refused = atomic_load_explicit(&s_refused, memory_order_relaxed);
    out->stale = atomic_load_explicit(&s_stale, memory_order_relaxed);
}

void beamer_lazyto_host_reset(void)
{
    if (!atomic_load(&s_on))
    {
        return;
    }
    s_req_last = 0;
    s_tele_last = 0;

    portENTER_CRITICAL(&s_mux);
    s_gen++;
    s_req_pending = false; // a request from the old session is answered to nobody
    atomic_store_explicit(&s_resp_seq, 0, memory_order_release);
    portEXIT_CRITICAL(&s_mux);
}

// --- the USB side ---------------------------------------------------------

static void wake(void)
{
    xSemaphoreGive(s_wake);
}

// `off` is the byte offset into the 4096-byte RESPONSE image, `n` a multiple
// of SECTOR.
static void read_response(uint8_t *dst, uint32_t off, uint32_t n, int32_t *ret)
{
    const uint32_t s1 = atomic_load_explicit(&s_resp_seq, memory_order_acquire);
    if (s_cmd_seq_set && s1 != s_cmd_seq)
    {
        // a later chunk of a READ(10) whose header already went out with
        // another seq: fail the command so the host reads it again
        count(&s_refused);
        *ret = -1;
        return;
    }

    const uint8_t result = s_resp_result;
    const uint16_t len = s_resp_len;

    const uint32_t hdr = sizeof(struct beamer_resp_hdr);
    uint32_t body_from = off;
    uint8_t *body_dst = dst;
    uint32_t body_n = n;
    if (off < hdr)
    {
        body_from = 0;
        body_dst = dst + (hdr - off);
        body_n = n - (hdr - off);
    }
    else
    {
        body_from = off - hdr;
    }
    memcpy(body_dst, s_mb->resp_body + body_from, body_n);

    atomic_thread_fence(memory_order_acquire);
    const uint32_t s2 = atomic_load_explicit(&s_resp_seq, memory_order_relaxed);
    const uint32_t seq = s1 == s2 ? s1 : 0;

    if (off == 0)
    {
        memset(dst, 0, hdr);
        dst[offsetof(struct beamer_resp_hdr, magic)] = 'M';
        dst[offsetof(struct beamer_resp_hdr, magic) + 1] = 'R';
        dst[offsetof(struct beamer_resp_hdr, result)] = result;
        wr32(dst + offsetof(struct beamer_resp_hdr, seq), seq);
        wr16(dst + offsetof(struct beamer_resp_hdr, len), len);
    }
    if (!s_cmd_seq_set)
    {
        s_cmd_seq = seq;
        s_cmd_seq_set = true;
    }
}

static int32_t mb_read(uint32_t off, uint32_t count_, uint8_t *dst)
{
    int32_t ret = (int32_t)(count_ * SECTOR);
    for (uint32_t i = 0; i < count_;)
    {
        const uint32_t s = off + i;
        uint8_t *const d = dst + i * SECTOR;

        if (s >= BEAMER_MB_RESP && s < BEAMER_MB_RESP + BEAMER_MB_RESP_SECTORS)
        {
            // the whole run of response sectors in this chunk, in one snapshot
            uint32_t run = BEAMER_MB_RESP + BEAMER_MB_RESP_SECTORS - s;
            if (run > count_ - i)
            {
                run = count_ - i;
            }
            read_response(d, (s - BEAMER_MB_RESP) * SECTOR, run * SECTOR, &ret);
            if (ret < 0)
            {
                return ret;
            }
            i += run;
            continue;
        }

        if (s == BEAMER_MB_HELLO)
        {
            memset(d, 0, SECTOR);
            portENTER_CRITICAL(&s_mux);
            memcpy(d, s_hello, sizeof(s_hello));
            portEXIT_CRITICAL(&s_mux);
        }
        else if (s == BEAMER_MB_REQ)
        {
            portENTER_CRITICAL(&s_mux);
            memcpy(d, s_mb->req, SECTOR);
            portEXIT_CRITICAL(&s_mux);
        }
        else if (s >= BEAMER_MB_TELE && s < BEAMER_MB_TELE + BEAMER_MB_TELE_SECTORS)
        {
            portENTER_CRITICAL(&s_mux);
            memcpy(d, s_mb->tele + (s - BEAMER_MB_TELE) * SECTOR, SECTOR);
            portEXIT_CRITICAL(&s_mux);
        }
        else
        {
            memset(d, 0, SECTOR); // spare
        }
        i++;
    }
    return ret;
}

static void accept_request(void)
{
    // s_mb->req holds the sector just written; s_mux is held
    const uint32_t seq = rd32(s_mb->req + offsetof(struct beamer_req_hdr, seq));
    const uint16_t len = rd16(s_mb->req + offsetof(struct beamer_req_hdr, len));
    const bool magic = s_mb->req[0] == 'M' && s_mb->req[1] == 'Q';

    if (seq == 0)
    {
        return; // nothing to answer: seq 0 means "no response" on the way back
    }
    if (seq == s_req_last)
    {
        count(&s_repeats); // a USB retry of a request already handed over
        return;
    }
    s_req_last = seq;

    const bool bad = !magic || len > BEAMER_LAZYTO_REQ_MAX;
    s_req_pending = true; // a request not yet taken is replaced: the host gave up on it
    s_req_bad = bad;
    s_req_seq = seq;
    s_req_gen = s_gen;
    s_req_len = bad ? 0 : len;

    count(&s_requests);
    if (bad)
    {
        count(&s_malformed);
    }
}

static void accept_telemetry(uint32_t last_written)
{
    // s_mb->tele holds the sectors just written; s_mux is held
    const uint32_t seq = rd32(s_mb->tele + offsetof(struct beamer_tele_hdr, seq));
    const uint16_t len = rd16(s_mb->tele + offsetof(struct beamer_tele_hdr, len));
    const bool magic = s_mb->tele[0] == 'M' && s_mb->tele[1] == 'E';

    if (!magic || len > BEAMER_LAZYTO_TELE_MAX)
    {
        count(&s_tele_bad);
        return;
    }
    const uint32_t needed = (sizeof(struct beamer_tele_hdr) + len + SECTOR - 1) / SECTOR;
    if (last_written < BEAMER_MB_TELE + needed - 1)
    {
        return; // the rest of the datagram is still to come
    }
    if (seq == 0 || seq == s_tele_last)
    {
        return;
    }
    s_tele_last = seq;

    s_tele_pending = true;
    s_tele_len = len;
    count(&s_telemetry);
}

static int32_t mb_write(uint32_t off, uint32_t count_, const uint8_t *src)
{
    if (off == BEAMER_MB_REQ && count_ == 1)
    {
        portENTER_CRITICAL(&s_mux);
        memcpy(s_mb->req, src, SECTOR);
        accept_request();
        portEXIT_CRITICAL(&s_mux);
        wake();
        return SECTOR;
    }

    if (off >= BEAMER_MB_TELE && off + count_ <= BEAMER_MB_TELE + BEAMER_MB_TELE_SECTORS)
    {
        portENTER_CRITICAL(&s_mux);
        memcpy(s_mb->tele + (off - BEAMER_MB_TELE) * SECTOR, src, count_ * SECTOR);
        accept_telemetry(off + count_ - 1);
        portEXIT_CRITICAL(&s_mux);
        wake();
        return (int32_t)(count_ * SECTOR);
    }

    count(&s_refused); // HELLO, RESPONSE and the spare sectors are the beamer's
    return -1;
}

bool beamer_lazyto_transfer(bool write, uint32_t cmd_lba, uint32_t start, uint32_t count_,
                            void *buf, int32_t *ret)
{
    if (!atomic_load_explicit(&s_on, memory_order_relaxed))
    {
        return false;
    }

    const uint32_t first = s_first;
    const uint32_t end = first + BEAMER_MB_SECTORS;
    if (start >= end || start + count_ <= first)
    {
        return false;
    }

    if (cmd_lba < first || start < first || start + count_ > end)
    {
        count(&s_refused); // straddles the window's edge
        *ret = -1;
        return true;
    }

    const uint32_t off = start - first;
    if (write)
    {
        *ret = mb_write(off, count_, (const uint8_t *)buf);
    }
    else
    {
        if (start == cmd_lba)
        {
            s_cmd_seq_set = false; // a new READ(10)
        }
        *ret = mb_read(off, count_, (uint8_t *)buf);
    }
    return true;
}
