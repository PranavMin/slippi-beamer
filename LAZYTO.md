# LazyTO over the Beamer (experimental, undecided)

This branch (`LazyTO`) explores one idea for [LazyTO](https://github.com/PranavMin/LazyTO): the
Beamer carries LazyTO's relay traffic, so the Wii never uses its own network. The upstream
firmware on `esp-32` is untouched.

Study written 2026-10-01 against upstream `8b655fa` (this fork's base), the LazyTO relay
(PranavMin/LazyTO), and LazyTO Nintendont (PranavMin/Nintendont, branch `LazyTO`).

**Status, 2026-10-07: mailbox v2 is built** (see [LazyTO mode as built](#lazyto-mode-as-built)
below) for LazyTO's redesign: the Beamer holds the station number (in flash) and the relay
secret (`LAZYTO-SECRET`), puts `relay_auth` in front of what the Wii sends, and collects, acks and
erases replays for the laptop. It compiles; nothing of it has run on hardware yet. Where the
study below (2026-10-01) and the built sections disagree, the built sections win.

## The idea

Today each Wii talks to the LazyTO relay over its own Wi-Fi or LAN adapter. The LazyTO Nintendont
kernel does this, using IOS sockets:

- one TCP connection per request to the relay (port 29470),
- a UDP beacon to find the relay (29471, with a request leg to 29472),
- UDP telemetry (29472).

The Wii's network is the least reliable part of a station:

- one boot in four never joined the Wi-Fi in the first hardware test;
- it is 802.11g only;
- some routers need a mode change before it connects at all (error 51330);
- some access points drop broadcasts to it.

A Beamer is already plugged into the Wii and already on Wi-Fi with a better radio. The idea: the
Wii hands each request to the Beamer over USB, and the Beamer does the network work.

## Verdict

**Feasible.** Every piece that has to exist already exists on both sides. None of it depends on
an undocumented behaviour of the Wii or the ESP32. The open questions are about performance
under load, and only hardware answers them.

| Question | Answer | Confidence |
|----------|--------|------------|
| Can the ESP hand the Wii arbitrary bytes, and see what the Wii writes? | Yes. Every host READ(10) and WRITE(10) goes through `transfer()` in `components/beamer_msc/beamer_msc.c`. That is one place where a sector range can be served from RAM instead of the card | Certain (code) |
| Can the Wii kernel read and write sectors without a filesystem? | Yes. `USBStorage_ReadSectors` and `USBStorage_WriteSectors` in Nintendont `kernel/usbstorage.c` are raw SCSI READ(10) and WRITE(10) at any LBA | Certain (code) |
| Can the transport be swapped without touching the relay or the game? | Yes. The kernel's network use is three functions in `kernel/RelayEXI.c`: `doRoundTrip`, `serviceBeacon` and `serviceTelemetry`. The relay does not tie a station to an address, and the game only talks EXI to the kernel | Certain (code) |
| Can the ESP do the network side? | Yes. It already runs Wi-Fi with power save off, lwIP, Rust `std::net`, and mDNS. A beacon listener and one TCP client are small additions | Very likely |
| Is it fast enough? | Probably 80-120 ms per request, against a 3 s budget. The Wii's own path is about 55 ms | Estimate, unmeasured |
| Does it fit in the ESP's memory? | Probably. It needs about 5 KB of static buffers plus one short TCP connection. Heap is tight while a replay is being served (largest free block about 7.5 KiB) | Unknown until tested under load |

**The ESP cannot write a real file for the Wii to read.** While the Wii has the drive mounted,
its FAT cache and the ESP's changes would disagree and corrupt the card. This firmware already
forbids writing the FAT while a host holds the drive (`FIRMWARE_DETAILS.md`, section "FAT
cache"). The channel has to be sectors that no filesystem owns.

## Design: a mailbox past the partition end

The firmware shows the host only up to the end of the first partition
(`beamer_msc_set_visible(end)`, called from `check_partition` in `src/boot.rs`). This design
raises that limit to `end + 16` and serves those 16 sectors from RAM:

- No host mounts them, because they lie outside every partition.
- The SD card and the write-back cache never see them.
- FatFS on the Wii never addresses them, because it only reads sector 0 and the sectors of its
  own volume.

### Sector layout

(The study's sketch. The built layout is LazyTO's `protocol.yaml`; see
[the mailbox as built](#the-mailbox-c).)

| Sector | Direction | Contents |
|--------|-----------|----------|
| `end+0` | ESP to Wii | HELLO and status: magic `LAZYTOMB`, layout version, flags (Wi-Fi joined, relay found, secret set), relay address and port, station number (from the button), stream flag, firmware version |
| `end+1` | Wii to ESP | REQUEST: `seq`, `len`, then the game's `relay_hdr` and payload (56 bytes at most today) |
| `end+2` to `end+9` | ESP to Wii | RESPONSE: `seq`, `len` and the transport result in the first sector, then the relay's reply (up to 4084 bytes, the game's poll buffer) |
| `end+10` | Wii to ESP | TELEMETRY: `seq` and one kernel log line, status record or crash report. The ESP forwards it to the relay's UDP 29472 |
| `end+11` to `end+15` | none | spare (for example, a "who is who" label for `/status`) |

### Rules

- **No write before HELLO.** The Wii reads HELLO and writes nothing until the magic and version
  match. On an ordinary USB stick, a read past its capacity fails, and an unpartitioned tail has
  no magic. Either way the Wii never writes there, and the kiosk shows NO BEAMER.
- **Deduplicate by `seq`.** Nintendont retries a failed USB transfer
  (`USBSTORAGE_CYCLE_RETRIES`), so the same request can arrive twice. The ESP sends each `seq`
  to the relay once, because END_SET and START_SET must not run twice.
- **The USB callback only copies.** It runs in the TinyUSB task (priority 22, core 1), so it
  only does a `memcpy` and notifies the relay task. All network work happens in that task on
  core 0.
- **The ESP owns the secret.** Mailbox v1 had the Wii write `relay_auth` from its card; v2
  (as built) is back to this: `LAZYTO-SECRET` in the Beamer's `config.txt`, and the Wii holds no
  secret. The secret itself never leaves the Beamer. `relay_auth`, in front of each request,
  telemetry datagram and sync, carries a key derived from it: the first 16 bytes of
  HMAC-SHA256 keyed with the NUL-padded secret over `LazyTO relay_auth` (`hmac::auth_key`). The
  Beamer sends `relay_auth` to whichever host sent the last beacon, so one forged beacon
  collects that key; the secret, which signs the sync reply, stays here.
- **One request at a time.** This matches the kernel today: one buffer and one state machine.

### Round trip

1. The game sends its request over EXI. The kernel copies it, as today.
2. The kernel writes the REQUEST sector with `seq = n`: one WRITE(10) of one sector.
3. The ESP sees the write and wakes its relay task. The task connects to the relay, sends
   `relay_auth` plus the bytes, reads to EOF, and fills the RESPONSE sectors with `seq = n`.
4. The kernel reads the first RESPONSE sector every 10 ms, inside its existing 3 s budget. Once
   `seq` matches, it reads all 8 sectors in one READ(10) and hands the reply to the game's next
   poll, as today.
5. While idle, the kernel reads HELLO about once a second and maps it to the existing
   `exi_poll_hdr` flags ("looking for the relay" and so on).

### Changes needed

**This fork (ESP firmware):**

1. **Mailbox in C** (about 60 lines). In `beamer_msc.c` `transfer()`, check the LBA against the
   window before the visible-size check, and serve or capture RAM. Raise the visible size to
   `end + 16` in `check_partition`.
2. **A Rust `relay` task** (about 400-600 lines):
   - listen for the beacon on UDP 29471, and send the request leg to 29472 while no beacon has
     been heard;
   - open one TCP connection per request;
   - forward telemetry.

   All buffers are static, under the firmware's rule of no allocation over 512 B while running.
3. **Config keys** `LAZYTO` and, in v2, `LAZYTO-SECRET` (there is no `LAZYTO-STREAM`). The
   station number comes from the button that already exists, kept in flash in LazyTO mode.
4. **Optional:** the LCD shows the relay state and the current set. (As built: the relay state
   only.)

**LazyTO Nintendont (a feature branch, when started):**

1. **A USB lock** around `USBStorage_ReadSectors` and `USBStorage_WriteSectors`. Today only the
   Slippi file writer uses USB, and nothing guards the shared CBW and transfer buffers. The game
   reports a score on the first CSS frame after a game, which is exactly when the writer is
   closing the replay. A one-token mqueue can serve as the mutex.
2. **Transport.** `doRoundTrip`, `serviceBeacon` and `serviceTelemetry` become mailbox
   operations. Wii networking can then stay off in the loader.
3. **Size:** about 300 lines.

**LazyTO relay and the protocol:** a `PF_NO_BEAMER` flag in `protocol.yaml`, plus its text on the
kiosk. The relay itself is unchanged.

## Constraints

- **The game must boot from SD.** Slippi Nintendont writes replays to USB only when the game is
  not on USB, and the Beamer takes the USB port. Beamer stations already work this way.
- **LazyTO Nintendont is version 1.13.1.** The Beamer needs 1.13.0 or later.
- **Each Beamer's `config.txt` needs `LAZYTO=true` and `LAZYTO-SECRET`,** as well as the SSID
  and password, and each Beamer a station number from its button.
- **The Wi-Fi problem moves to the ESP.** Upstream recommends the TO's own router within about
  20 ft. The ESP is still better placed than the Wii: 802.11n, power save off, and a screen that
  shows when it is not connected.

## Risks, worst first

1. **ESP heap at game end.** Beamer Manager pulls the new replay at the same moment the game
   reports the score. Test with `tools/stress_station.py` while the relay task is busy.
2. **The kernel USB lock touches the replay path.** A bug there costs replays.
3. **A stalled SD card.** It can hold a host write for up to 30 s (write-back cache). A relay
   request queued behind that write times out on the kiosk at 5 s. The kiosk shows an error,
   and A retries.
4. **Upkeep.** Upstream releases have to be merged into this fork, unless the feature goes
   upstream behind config keys.

## Alternatives considered

- **A USB vendor interface beside mass storage** (a composite device). It is cleaner on the wire,
  but the kernel would need its own bulk-transfer and hotplug code. The existing driver assumes
  one interface per device. The mailbox reuses everything that exists.
- **Emulating a USB Ethernet adapter.** IOS only has a driver for the ASIX AX88772, a high-speed
  part, and the ESP32-S3 is full-speed. It also keeps IOS's network bring-up, which is part of
  the problem.
- **The ESP keeping a set-list file up to date.** The game also starts, reports and ends sets,
  so the channel has to go both ways anyway. LIST_SETS takes 55 ms, so caching the list in
  advance gains nothing.

## Test plan

| Step | Needs | Go if |
|------|-------|-------|
| 0. Coexistence | A stock Beamer, the LazyTO loader, the game on SD | Replays record and reach Beamer Manager, and the kiosk loads its set list over the Wii's own network as today |
| 1. Mailbox, with a PC as the host | This firmware with the mailbox and relay task. A Linux box (or WSL with usbipd) runs a script that plays the Wii with `O_DIRECT` reads and writes on `/dev/sdX` | LIST_SETS comes back byte-exact from a dev relay (LazyTO `test/fake-startgg.ts`) in under 200 ms |
| 2. On a Wii | Step 1 firmware and a LazyTO Nintendont build with the transport and the lock, with Network off in the loader | A full set lifecycle over 20 games with no replay lost, including the report at game end while the Beamer serves the previous replay |
| 3. Load | Several Beamers | 12 stations through a bracket, or `tools/stress_station.py` as a stand-in |

## LazyTO mode as built

Mailbox v1 was built 2026-10-01. Mailbox v2 and the replay work (firmware build 2) were built
2026-10-07, for LazyTO's redesign: the station number and the secret live on the Beamer, and the
Beamer collects, acks and erases replays for the laptop (LazyTO `docs/redesign.md`,
`docs/protocol-v2.md`). It compiles with no warnings (`cargo build --release`), and the pure
helpers' tests pass on a PC (`tools/lazyto_host_tests.sh`, including the sync signature vector
from `protocol-v2.md`). Nothing of v2 has run on a Beamer yet.

### Turning it on

`CONFIG/config.txt`:

```
LAZYTO=true
LAZYTO-SECRET=<the laptop's secret>
```

- `LAZYTO` is a flag like `FLIP-SCREEN` and `DEBUG` (`true`/`false`, `yes`/`no`, `1`/`0`;
  default `false`), and like `DEBUG` it is read at boot only, because it changes the size of the
  drive the host sees. An edit while running is logged and takes effect at the next boot. A
  config that is rejected turns it off, as it resets every other key.
- `LAZYTO-SECRET` is the relay's shared secret: 8 to 16 letters, digits, `-` or `_`, the relay's
  own rule. It follows edits live, like `LED-BRIGHTNESS`. It is never served (no route serves
  `CONFIG/`), never logged, and `error.txt` names only its length when it is rejected. Without
  it the Beamer refuses the Wii's requests (`BR_NO_SECRET`) and drops its telemetry.
- `SSID` and `PASSWORD` as for any Beamer. The relay is found by its beacon; there is no relay
  address to configure.

With `LAZYTO=false` the firmware is upstream's: the host sees exactly the replay partition, the
mailbox check in `transfer()` returns at its first test, there is no task, no socket and no
buffer, the station number lives in RAM and starts at 1, and nothing is written to flash or
erased. Its fixed state is about 560 B of static RAM.

### The station number

- **The button** is upstream's in every mode: a click adds 1, holding it takes 1 off, never
  below 1.
- **Kept in flash** in LazyTO mode only: key `station` in the `lazyto` namespace of the 64 KB
  `jrnl` NVS partition (`src/lazyto/store.rs`), written 2.5 s after the last press and only if
  it changed, and right away before a restart or an eject (`src/name.rs`). Not the default `nvs`
  at 0x9000, which the merged `beamer.bin` written at 0x0 probably pads over; `jrnl` lies past
  the image's end.
- **Unset** until the first click: a new or wiped Beamer is never "Station 1", or every fresh
  one would collide. The screen says "No station", the hello has no `BF_STATION_SET`, requests
  are refused with `BR_NO_STATION`, and telemetry is dropped. The first click sets 1.

### The protocol header

`components/beamer_lazyto/include/relay_proto.h` is a verbatim copy of LazyTO's generated header
(`kiosk/include/relay_proto.h` on LazyTO branch `redesign-v2`, generated from `protocol.yaml` at
`5dfc5fb`), MIT-licensed (`SPDX-License-Identifier: MIT`). **Never hand-edit it**: change
`protocol.yaml` in LazyTO, regenerate with `tools/gen_protocol.py`, and copy the file over again.
Rust sees it through bindgen as `esp_idf_sys::lazyto` (the `bindings_module` in `Cargo.toml`),
so its names never mix with ESP-IDF's. All multi-byte integers in it are big-endian on the wire;
the ESP32 is little-endian, so every field is read and written byte by byte.

What the Beamer parses of it, and what is frozen because dongles have no over-the-air update:
the beacon (length, magic, `tcp_port`; never `version`), `relay_auth`, and the sync
(`CMD_BEAMER_SYNC`, `BEAMER_SYNC_VERSION` 1 and its four structs). The mailbox itself is
versioned (`BEAMER_MB_VERSION` 2) and changes together with the kernel.

### The mailbox (C)

`components/beamer_lazyto/beamer_lazyto.c`, hooked into `transfer()` in
`components/beamer_msc/beamer_msc.c` ahead of the visible-size check and the write-back cache.
The window starts at the replay partition's end (`check_partition` in `src/boot.rs`), and the
host's visible size becomes `end + BEAMER_MB_SECTORS`.

| Offset | Constant | Direction | Contents | Host write |
|--------|----------|-----------|----------|------------|
| 0 | `BEAMER_MB_HELLO` | Beamer to Wii | `beamer_hello` v2, 32 bytes (below) | refused |
| 1 | `BEAMER_MB_REQ` | Wii to Beamer | `beamer_req_hdr` (`'M','Q'`, `seq`, `len` up to 500), then `relay_hdr` + payload, with no `relay_auth` | taken; reads return what was last written |
| 2-9 | `BEAMER_MB_RESP` | Beamer to Wii | `beamer_resp_hdr` (`'M','R'`, `result`, `seq`, `len`), then up to 4,084 reply bytes | refused |
| 10-11 | `BEAMER_MB_TELE` | Wii to Beamer | `beamer_tele_hdr` (`'M','E'`, `seq`, `len` up to 1,012), then `telemetry_hdr` + payload, with no `relay_auth` | taken; reads return what was last written |
| 12-15 | | | spare, read as zeros | refused |

The hello, rewritten by the relay task every 250 ms and after each request:

| Field | Value |
|-------|-------|
| `magic`, `version` | `LAZYTOMB`, `BEAMER_MB_VERSION` (2) |
| `flags` | `BF_WIFI` exactly when `wifi` is `WIFI_UP`; `BF_RELAY` once a beacon has been heard; `BF_STATION_SET` with a number; `BF_SECRET` with `LAZYTO-SECRET` |
| `station` | the number on the screen, with `BF_STATION_SET`; 0 otherwise |
| `relay_ip`, `relay_port` | the latest beacon's source address and `tcp_port` |
| `wifi` | below |
| `storage` | below |
| `fw_build` | `BEAMER_LAZYTO_FW_BUILD`, 2 (at least `BEAMER_FW_MIN`, checked at compile time) |
| `last_result` | the last request's `beamer_result`, 0 before the first |
| `beacon_age_s` | seconds since the last beacon, saturating at 0xFFFF, with `BF_RELAY` |

Before the relay task's first update (the first moment after the bind) the hello says: no flags,
station 0, `WIFI_JOINING`.

Rules, as built:

- **Never the card.** Mailbox transfers never reach the SD card or the write-back cache, so the
  FAT rule in `FIRMWARE_DETAILS.md` holds. They are not counted as reads or writes (the `BUSY`
  blink), not timed into the transfer ring, and a mailbox write is not a config edit. They do
  update the last-command time, which the inventory reads as "the Wii is talking".
- **Edges.** A transfer that overlaps the window but starts outside it, a READ(10)/WRITE(10)
  that began on the card and runs into it, or one that runs past its end, fails (-1).
- **Requests.** Taken on a write of sector 1. `seq` 0 is ignored. The same `seq` as the last
  one taken is ignored (a USB retry; counted as `repeats`), so the relay sees each `seq` once.
  Bad magic or `len` over 500: answered `BR_BAD_REQ` for that `seq`, without the relay. A
  request not yet taken when a newer one lands is replaced: the Wii gave up on it.
- **Telemetry.** Taken when the write that completes the datagram lands; the header's `len`
  says whether it needs one sector or both. One WRITE(10) of both sectors, a single sector for a
  short datagram, or the header sector first and the second after, all work. New `seq` only.
- **Re-enumeration.** When the host configures or drops the device (`tud_mount_cb`,
  `tud_umount_cb`: a Wii reboot), the last `seq`s are forgotten, a request not yet taken is
  dropped, the response goes back to `seq` 0, and an answer still in flight for the old session
  is discarded (`stale`). A rebooted kernel that starts again at `seq` 1 is therefore neither
  ignored nor handed the previous session's reply.
- **The USB callback stays fast.** It copies under a spinlock and gives a binary semaphore;
  nothing else. All network work happens in the relay task on core 0.

C API (`components/beamer_lazyto/include/beamer_lazyto.h`; Rust wrapper
`src/storage/mailbox.rs`):

| Function | Who | What |
|----------|-----|------|
| `beamer_lazyto_install(first)` | boot, before the USB bind | takes the buffers, turns the window on |
| `beamer_lazyto_set_hello(&hello)` | relay task | HELLO's live fields |
| `beamer_lazyto_wait(ms)` | relay task | sleeps until the host hands something over; returns `BEAMER_LAZYTO_EV_*` bits |
| `beamer_lazyto_request_pending()` | relay task | whether a request waits, so the sync gives way |
| `beamer_lazyto_take_request(out, cap, &seq, &gen, &bad)` | relay task | copies the pending request out |
| `beamer_lazyto_resp_begin()` / `beamer_lazyto_resp_commit(gen, seq, result, len)` | relay task | withdraws the old response and hands out the body buffer / publishes the new one |
| `beamer_lazyto_take_telemetry(out, cap)` | relay task | copies the pending datagram out |
| `beamer_lazyto_stats(&s)` | `/status` | the counters |
| `beamer_lazyto_sha_begin/update/finish(slot, ...)` | transfer task (slot `SERVE`), relay task (slot `SYNC`) | SHA-256 on the SHA accelerator (mbedtls, `CONFIG_MBEDTLS_HARDWARE_SHA`); the two contexts live in the mailbox's heap block |
| `beamer_lazyto_random(out, len)` | relay task | the sync's nonce, from the hardware RNG |
| `beamer_lazyto_transfer(...)`, `beamer_lazyto_host_reset()` | `beamer_msc.c` only | the USB side |

### Memory ordering of the response

The response is 4 KB and is read off the relay's socket straight into the mailbox, so it is not
copied under the spinlock. It is a seqlock, with `seq` 0 as the "being written" marker:

- The relay task (core 0) stores `seq = 0`, then a release fence, then writes the body; then,
  under the spinlock, `result` and `len`, and last a release store of the new `seq`.
- The USB task (core 1) loads `seq` with acquire, copies `result`, `len` and the body, then an
  acquire fence and loads `seq` again. If the two loads differ, the header it hands the host
  says `seq` 0.

So a host that reads `seq = n` has read the whole body that was complete when `n` was published,
and a read that overlapped a write says "no response yet", which the Wii polls through. The
host's 8-sector READ(10) reaches `transfer()` as two 2 KB chunks (`CFG_TUD_MSC_EP_BUFSIZE`); if
`seq` changes between the chunks of one command, the later chunk fails and the host retries the
command, so the 8 sectors are never half one answer and half the next. By protocol that cannot
happen anyway (the Wii sends its next request only after it has read the answer); it is a
guard. Writes to `seq` and the session counter happen under the same spinlock, so a reset
between a commit's check and its store cannot publish a stale answer.

### The relay task (Rust)

`src/net/relay.rs`: FreeRTOS task `relay`, priority 4, core 0, 6 KB stack, started after `net`
and only when the mailbox is installed. It wakes as soon as the host writes, and every 250 ms
otherwise.

- **Discovery.** One UDP socket on `BEACON_PORT` (29471), with `SO_BROADCAST` (lwIP hands
  broadcasts only to such sockets), non-blocking. A valid beacon is exactly 12 bytes, `'M','T'`
  and a nonzero `tcp_port`; its `version` is never compared, so a protocol bump never strands a
  Beamer. The relay is the datagram's source address plus `tcp_port`, and the latest valid
  beacon wins. While no beacon has been heard for `BEACON_STALE_S` (10 s), and the station has
  an address, it broadcasts a beacon request (a `relay_beacon` with `tcp_port` and `event_id` 0)
  to `255.255.255.255:TELEMETRY_PORT` every `BEACON_INTERVAL_MS`; the relay answers unicast to
  port 29471.
- **Requests.** Refused locally, in this order, without the relay: `BR_BAD_REQ` for a malformed
  sector, `BR_NO_STATION` without a number, `BR_NO_SECRET` without `LAZYTO-SECRET`,
  `BR_NO_WIFI` without an address, `BR_NO_RELAY` before a beacon. Otherwise one TCP connection:
  connect, send `relay_auth` (the key from the secret) + the Wii's bytes, read until the relay
  closes, all inside 2,500 ms (the Wii's own budget is 3,000 ms), close. A failed connect, send
  or read is `BR_CONNECT`, a spent budget `BR_TIMEOUT`, more than 4,084 bytes `BR_TOO_LARGE`.
  One attempt, no retries; the kiosk shows the error and A retries. The Beamer never parses
  `relay_hdr` or the reply: the kernel stamps the station from the hello.
- **Telemetry.** One UDP datagram, `relay_auth` + the kernel's bytes, to the relay's
  `TELEMETRY_PORT`, best effort. Dropped without a number (the relay would take an unnumbered
  station for Dolphin's station 0), a secret, an address or a relay; counted on `/status`.
- **Between requests**, the sync (below). It runs only while no request waits, and gives way
  within 100 ms when one arrives.
- **Stopping.** It stands down when the station is ejected or restarts.

### Wi-Fi and storage in the hello

`wifi` follows the firmware's own Wi-Fi states (`src/net/wifi.rs`):

| `wifi` | When |
|--------|------|
| `WIFI_UP` | the station has an address (the one on the screen) |
| `WIFI_JOINING` | associating or waiting for DHCP; also after a lost link, until the next attempt fails |
| `WIFI_NO_SSID` | no `SSID` in `config.txt` (or the config was rejected) |
| `WIFI_CANT_JOIN` | `WIFI ISSUE`: the network cannot be reached or refused the password |
| `WIFI_NO_ADDRESS` | `WIFI TOO FULL`: joined, no address |
| `WIFI_RADIO` | `RADIO FAILURE` |

`storage` is the worst standing state, in this order: `STORE_NO_CARD` (`NO SD CARD`),
`STORE_WRONG_FORMAT` (`WRONG FORMAT`), `STORE_UNREADABLE` (`SD UNREADABLE`, or the
`DRIVE FAILING` warning), `STORE_WRITE_FAILED` (`WRITE FAILED`, `CARD STUCK`), `STORE_FULL`
(under 64 MB free), `STORE_FILLING` (384 files or more, or under 1 GB free), `STORE_OK`. The
screen shows one error at a time, so the errors raised and not resolved are tracked apart
(`errors::standing`). `NO SD CARD` and `WRONG FORMAT` stop the Beamer before the mailbox exists,
so a Wii sees them as no LazyTO Beamer; they are mapped for completeness.

### The inventory: the scan, fixed

`src/lazyto/inventory.rs`, run by the `scan` task in place of upstream's walk. Upstream's walk
stops counting at `REPLAY-CAP`, serves only this boot's newest replays, and treats a file whose
header says raw length 0 as live until a reboot, which freezes the scan. Since Nintendont stopped
syncing files (upstream PR #66) a game being recorded is a 0-byte entry, and a file closed with
raw length 0 (an interrupted game, once the kernel syncs each file early) is common. In LazyTO
mode:

- **The walk** is uncapped and reads `SLIPPI/` with FatFs itself, so size and FAT time come
  with each entry. Only `Game_*.slp` names count.
- **Classes.** A file is *empty* (0 bytes), *finished* (its size covers the raw length in its
  header: size >= 15 + raw length, raw length not 0), *live* (raw length 0 or empty, the newest
  file by FAT time, and a USB command from the Wii in the last 5 s: the kernel reads the hello
  every second, even while a game is paused), or *incomplete* (anything else: an interrupted
  recording, a header that is not a replay's). Incomplete files are served and collected.
- **Tracked by key**: name, size and FAT modified time (`wire::key`), in a table of 1,024 keys
  (`src/lazyto/known.rs`). A file's header is read once per key; a file first seen incomplete
  (the end-of-game race, where the header's raw length lands before the final size) is a new
  key once it is finished. Keys no longer on the card are forgotten, unless acked.
- **No freeze.** While a file is live, a tick only checks that file (one `f_stat`); any change,
  or the Wii going quiet, triggers a full walk, and there is a full walk every 60 s regardless.
  When nothing is live, a host write triggers a walk on the next tick.
- **Free space** is counted from the FAT itself, not FSInfo, which overstates it after
  interrupted games: the first FAT is read through the write-back cache in 4 KB reads (about
  4 MB on a 4 GB card with 4 KB clusters), on the first walk and then at most every 2 minutes,
  never while a game is live, and only when the files changed. Between counts the free space is
  the count less what the files grew by.
- **States.** `STORE_FULL` under 64 MB free (the next game would fail); `STORE_FILLING` at 384
  files or under 1 GB free. The upstream warnings follow them in LazyTO mode (`DRIVE FULL`,
  `DRIVE FILLING`, with the reason "replug to erase collected replays"), and the screen's
  "% full" is space, not files.
- **`/status`'s game** and the BUSY readout follow the live file, as upstream's scan did when
  Nintendont still synced.
- It keeps the 16 files the next sync asks about, and asks for a sync when it finds a new file.

### Serving any file by name

In LazyTO mode `GET /SLIPPI/<name>` serves any replay on the card, not only this boot's newest
`NUM-REPLAYS-SERVED`; `Range` and `X-Replay-From` are checked against the file itself. The live
file is refused with `503` and `Retry-After`; an empty entry is `404`. HTTP keeps one socket
(`max_open_sockets` 1): lwIP has two active TCP connections, and the relay link needs the other
one when the kiosk reports a score while a replay downloads. Replays go out raw, never gzipped,
whatever `Accept-Encoding` asks: the gzip arena (15 KB) does not fit beside the relay link's
buffers, and a download that ran the heap out stalled in send for good. `POST /reset-beamer` is refused
(`403`): its wipe withdraws the medium under a mounted Wii, whose FatFs never notices.

While serving, the transfer task hashes the file (`src/lazyto/served.rs`): SHA-256 on the SHA
accelerator over the bytes sent, and a CRC32 of the first 1 KB. A resumed request
reads and hashes the skipped prefix first, without sending it. A file sent whole goes into a
RAM table of the last 32 (acked ones leave first), and a sync follows. Nothing is hashed at boot
or when idle: a file served before a reboot is simply served again.

### The sync

`src/lazyto/sync.rs`: the one request the Beamer makes on its own, on the relay link
(`relay_auth` + `relay_hdr` with version `BEAMER_SYNC_VERSION` 1, cmd `CMD_BEAMER_SYNC` 8 +
`beamer_sync_req`), after each served file, when the inventory finds a new file, every 30 s, and
2 s after a sync that acked something or left files out (`SF_MORE`). A connect gets 500 ms and
the whole exchange 1.5 s.

- **It carries** the `station_id`, the ack table's `archive_id`, a fresh random nonce,
  `fw_build`, uptime, free, card and used MB, the station (with `SF_STATION_SET`), the HTTP port,
  the counts (on the card, to collect, to erase, empty, incomplete, acks), this boot's erase
  report (with `SF_COLD_BOOT`, `SF_ERASE_FAILED`, `SF_ACKS_DROPPED`), the storage state, the last
  request's result and the RSSI; then up to 16 files without an ack, served-and-hashed ones
  first, then finished, incomplete, live, oldest first in each group. A name over 40 bytes is
  never listed.
- **The reply is accepted** only with magic, version 1 and cmd 8, `ST_OK`, exactly 52 + 36 n
  payload bytes with `answer_count` n, a nonzero `archive_id`, and an HMAC-SHA256 that verifies
  (constant-time compare), keyed with the 16 NUL-padded secret bytes over the nonce, the
  `station_id` and the payload after the HMAC. Without the signature, anyone on the Wi-Fi could
  download a file, answer "held" with its hash, and make the Beamer erase a replay nobody kept.
  The key is the secret itself, never `relay_auth`'s key: a host that sent a beacon receives that
  key with the sync request, and must not be able to sign the reply.
- **Then** a different `archive_id` drops every ack and is adopted (another laptop, or a deleted
  archive folder: the files are collected again while the Beamer is still powered), and each
  `SA_HELD` whose hash equals the Beamer's own for that file is acked. A "held" for a file not
  hashed this boot acks nothing. `SA_WANTED` and `SA_NOTED` need nothing: the relay downloads.

### The acks

`src/lazyto/acks.rs`, in the `lazyto` namespace of `jrnl`: a header blob `ackhdr` (format, the
card's FAT volume serial and a hash of its SD CID, the `archive_id`, the count) and the records
in pages `ack00`..`ack31` of 32, so one ack rewrites one small blob. A record is the file's key,
size, FAT modified time and the CRC32 of its first 1 KB. At most 1,024; when full, acking stops
and the files stay (FILLING shows). A table stored for another card, or in an unknown format, is
dropped at boot (`SF_ACKS_DROPPED`).

### The cold-boot erase

`src/lazyto/erase.rs`, inside the boot's write window, after the config is read and before the
USB bind, so no host holds the FAT:

- **Cold boots only**: a power-on reset (`ESP_RST_POWERON`) that is also the first boot since
  power was applied (`beamer_boot_count()` 1). Panic, watchdog, brownout, software, reload and
  post-flash resets never erase, and if the erase itself crashes, the next boot is warm and binds
  normally.
- **0-byte `Game_*.slp` entries first**, without an ack: at a cold boot no file can be open, and
  such an entry holds no data. **Then the acked files**: the key (name, size, FAT modified time)
  must match a record, and the CRC32 of the file's first 1 KB is read and compared again, so a
  name reused by a Wii whose clock went back never matches. A mismatch keeps the file and drops
  the record.
- **4 s budget** from the start: a file is not started if the time so far plus the average per
  file would pass it. What is left waits for the next cold boot. Files go in batches of 8, and
  the screen shows ERASING n (with DO NOT UNPLUG) and counts down.
- **A failed delete** stops the erase, raises `WRITE FAILED`, and the Beamer binds anyway.
- **Afterwards** the table keeps only records for files still on the card; it is written once,
  and only when it changed. The report (erased, empty entries, milliseconds, left over, failed)
  goes in every sync of this boot.

### What shows it

- `GET /status` carries a `"lazyto"` object in LazyTO mode only (`API.md`): firmware build,
  whether there is a number and a secret (never the secret), the Wi-Fi and storage states, the
  relay and the beacon's age, request and telemetry counters, the inventory, the erase report and
  the syncs.
- The screen: "No station" without a number; the grey line under the station name ends in
  `RELAY` once the relay has been heard; ERASING n during the cold-boot erase.
- The log has one line per request (`seq`, sizes, result, milliseconds), one per sync and per
  ack, and the inventory's and the erase's summaries. With `DEBUG`, the relay task's unused stack
  follows each request.

### Memory

| | Bytes | When |
|-|------:|------|
| static RAM: HELLO, seqs, counters, the wake semaphore, the secret, the sync, ack and erase state | ~560 | always |
| Mailbox buffers (request 512, telemetry 1,024, response 4,084) and two SHA-256 contexts, one `heap_caps_calloc` | ~5,850 | once at boot, `LAZYTO=true` only |
| Relay task stack | 6,144 | once at boot, `LAZYTO=true` only |
| Relay task buffers: one request, datagram or sync out (1,472), one sync reply (668) | 2,140 | once at boot, `LAZYTO=true` only |
| The known-files table, 1,024 keys | 8,192 | once at boot, `LAZYTO=true` only |
| The served-hash table, 32 entries | ~1,800 | once at boot, `LAZYTO=true` only |
| The inventory's snapshot (16 files to sync) | ~1,200 | once at boot, `LAZYTO=true` only |
| The UDP socket | ~500 | once, `LAZYTO=true` only |
| One request's or sync's TCP socket, pcb and segments | ~5,000 | per request, freed when the relay closes |
| The ack table read at a cold boot (up to 20 KB), and the erase's name lists | up to ~30,000 | at boot, before the bind, freed before it |

LazyTO mode leaves out upstream's multicast announce socket (~1,500), and HTTP keeps one socket
instead of two. Nothing is allocated per request by the firmware itself; lwIP's own allocations
fail softly (`BR_CONNECT`).

Against mailbox v1 (`41c16d2`) built with the same toolchain: `.dram0.bss` +176 B, `.dram0.data`
+144 B, IRAM unchanged, `.flash.text` +54 KB, `.flash.rodata` +5.9 KB.

### Hardware test

`tools/lazyto_host.py` plays the Wii from a Linux PC (step 1 of the test plan). It reads the
MBR, finds the first FAT32 partition's end as the firmware does, and prints the hello (mailbox
v2: flags, station, Wi-Fi and storage state, last result, beacon age). With `--list-sets` it
writes a `CMD_LIST_SETS` request, stamped with the hello's station as the kernel does and without
`relay_auth`, and polls the response sector every 10 ms, then prints the transport result, the
relay's status and message, and the sets. All I/O is `O_DIRECT`. It takes every constant and
offset from the generated header.

```
sudo tools/lazyto_host.py /dev/sdX
sudo tools/lazyto_host.py /dev/sdX --list-sets
```

`tools/lazyto_host_tests.sh` runs the unit tests of the pure modules (`src/lazyto/hmac.rs`,
`src/lazyto/wire.rs`) on a PC with rustup's stable toolchain, including LazyTO's two vectors:
the sync signature and `relay_auth`'s key.

### Known limits and open questions

1. **Unmeasured**, and the hardware test decides them: the boot-to-bind time with an erase, the
   erase time per file (how many fit in 4 s), the SHA-256 rate, the FAT count's time, the relay
   task's stack (6 KB), and the heap with every LazyTO allocation (about 13 KB more than v1).
2. **The relay side of the sync** is not built yet: LazyTO's relay answers `CMD_BEAMER_SYNC` with
   `ST_INTERNAL`, so until it does, no Beamer acks or erases anything but 0-byte entries.
3. **Live** relies on the Wii's clock moving forward within a session (newest by FAT time). A
   Beamer moved from a Wii whose clock runs ahead may see a game in progress as incomplete; it is
   then served as a partial, never acked as the finished file (the key differs), and collected
   again once finished.
4. **One HTTP socket with LRU purge**: a second client (a browser on `/status`) closes the least
   recently used session, which can be a download in progress; the relay resumes it.
5. **A storage fault** (`NO SD CARD`, `SD UNREADABLE`, `NO USB`) makes the drive
   write-protected, which refuses mailbox writes too; after an eject the medium is gone. The Wii
   then times out.
6. **The relay is never forgotten**, only replaced by a newer beacon; the hello's `beacon_age_s`
   says how old it is.
7. **The kernel must stamp the station** from the hello: the Beamer never reads `relay_hdr`.

## Related, but not part of this branch

Labelling replays with the players' tags does not need the Beamer. Wii replays already carry
empty display-name fields in Game Start (offset 0x1A5, 31 bytes per port), and Replay Reporter
shows those fields first. The LazyTO kernel writes those bytes itself, so it can fill them in
from the kiosk's port claim. That work belongs in LazyTO, not here. Upstream could show those
names in `/status` with a small change to `src/slp.rs`.
