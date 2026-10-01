# LazyTO over the Beamer (experimental, undecided)

This branch (`LazyTO`) explores one idea for [LazyTO](https://github.com/PranavMin/LazyTO): the
Beamer carries LazyTO's relay traffic, so the Wii never uses its own network. The upstream
firmware on `esp-32` is untouched.

Study written 2026-10-01 against upstream `8b655fa` (this fork's base), the LazyTO relay
(PranavMin/LazyTO), and LazyTO Nintendont (PranavMin/Nintendont, branch `LazyTO`).

**Status, 2026-10-01: the firmware side is built** (see [LazyTO mode as built](#lazyto-mode-as-built)
below), and compiles; it has not run on hardware yet. The Nintendont side does not exist yet.
Where the study and the built firmware disagree, the built section wins: the Wii, not the
Beamer, holds the relay secret, and the only config key is `LAZYTO`.

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
- ~~**The ESP owns the secret.**~~ Superseded: the Wii writes `relay_auth` (and so the
  secret from its `tournament.cfg`) into the request itself, exactly the bytes it would send
  on its own TCP connection, and the Beamer is a pure pipe that never parses them.
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
3. ~~**Config keys** `LAZYTO-SECRET` and `LAZYTO-STREAM`.~~ As built: one key, `LAZYTO`; the
   secret and the stream flag stay in the Wii's `tournament.cfg`. The station number comes from
   the button that already exists.
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
- **Each Beamer's `config.txt` needs `LAZYTO=true`,** as well as the SSID and password. (The
  study had the secret here too; as built, the Wii keeps it.)
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

Built 2026-10-01. It compiles (`cargo build --release`); the C mailbox has been exercised on a
PC with stubbed FreeRTOS, and `tools/lazyto_host.py` against a disk image, but nothing has run on
a Beamer yet.

### Turning it on

`CONFIG/config.txt`:

```
LAZYTO=true
```

It is a flag like `FLIP-SCREEN` and `DEBUG` (`true`/`false`, `yes`/`no`, `1`/`0`; default
`false`), and like `DEBUG` it is read at boot only, because it changes the size of the drive the
host sees. An edit while running is logged and takes effect at the next boot. A config that is
rejected turns it off, as it resets every other key.

With `LAZYTO=false` the firmware is upstream's: the host sees exactly the replay partition, the
mailbox check in `transfer()` returns at its first test, and there is no task, no socket and no
buffer. Its fixed state is about 240 B of `.bss`.

With `LAZYTO=true` the Beamer still needs `SSID` and `PASSWORD`. It finds the relay by itself
(its beacon); there is no relay address to configure. The secret stays on the Wii.

### The protocol header

`components/beamer_lazyto/include/relay_proto.h` is a verbatim copy of LazyTO's
`generated/relay_proto.h` (as of LazyTO branch `beamer`, commit `d639861`), which LazyTO's
`tools/gen_protocol.py` generates from its `protocol.yaml`. **Never hand-edit it**: change
`protocol.yaml` in LazyTO, regenerate, and copy the file over again. Rust sees it through bindgen as `esp_idf_sys::lazyto` (the
`bindings_module` in `Cargo.toml`), so its names never mix with ESP-IDF's. All multi-byte
integers in it are big-endian on the wire; the ESP32 is little-endian, so every field is read
and written byte by byte.

### The mailbox (C)

`components/beamer_lazyto/beamer_lazyto.c`, hooked into `transfer()` in
`components/beamer_msc/beamer_msc.c` ahead of the visible-size check and the write-back cache.
The window starts at the replay partition's end (`check_partition` in `src/boot.rs`), and the
host's visible size becomes `end + BEAMER_MB_SECTORS`.

| Offset | Constant | Direction | Contents | Host write |
|--------|----------|-----------|----------|------------|
| 0 | `BEAMER_MB_HELLO` | Beamer to Wii | `beamer_hello`: `LAZYTOMB`, `BEAMER_MB_VERSION`, flags (`BF_WIFI` when the station has an address, `BF_RELAY` once a beacon has been heard), the station number on the screen, the relay's address and port, `fw_build` (`BEAMER_LAZYTO_FW_BUILD`, now 1) | refused |
| 1 | `BEAMER_MB_REQ` | Wii to Beamer | `beamer_req_hdr` (`'M','Q'`, `seq`, `len` up to 500), then the bytes for the relay: `relay_auth` + `relay_hdr` + payload | taken; reads return what was last written |
| 2-9 | `BEAMER_MB_RESP` | Beamer to Wii | `beamer_resp_hdr` (`'M','R'`, `result`, `seq`, `len`), then up to 4,084 reply bytes | refused |
| 10-11 | `BEAMER_MB_TELE` | Wii to Beamer | `beamer_tele_hdr` (`'M','E'`, `seq`, `len` up to 1,012), then one telemetry datagram | taken; reads return what was last written |
| 12-15 | | | spare, read as zeros | refused |

Rules, as built:

- **Never the card.** Mailbox transfers never reach the SD card or the write-back cache, so the
  FAT rule in `FIRMWARE_DETAILS.md` holds. They are not counted as reads or writes (the `BUSY`
  blink), not timed into the transfer ring, and a mailbox write is not a config edit.
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
| `beamer_lazyto_set_hello(flags, station, ip, port)` | relay task | HELLO's live fields |
| `beamer_lazyto_wait(ms)` | relay task | sleeps until the host hands something over; returns `BEAMER_LAZYTO_EV_*` bits |
| `beamer_lazyto_take_request(out, cap, &seq, &gen, &bad)` | relay task | copies the pending request out |
| `beamer_lazyto_resp_begin()` / `beamer_lazyto_resp_commit(gen, seq, result, len)` | relay task | withdraws the old response and hands out the body buffer / publishes the new one |
| `beamer_lazyto_take_telemetry(out, cap)` | relay task | copies the pending datagram out |
| `beamer_lazyto_stats(&s)` | `/status` | the counters |
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

`src/net/relay.rs`: FreeRTOS task `relay`, priority 4, core 0, 4 KB stack, started after `net`
and only when the mailbox is installed. It wakes as soon as the host writes, and every 250 ms
otherwise.

- **Discovery.** One UDP socket on `BEACON_PORT` (29471), with `SO_BROADCAST` (lwIP hands
  broadcasts only to such sockets), non-blocking. A valid beacon is exactly 12 bytes, `'M','T'`,
  `RELAY_PROTO_VERSION`, and a nonzero `tcp_port`; the relay is the datagram's source address
  plus `tcp_port`, and the latest valid beacon wins. Until one is heard, and while the station
  has an address, it broadcasts a beacon request (a `relay_beacon` with `tcp_port` and
  `event_id` 0) to `255.255.255.255:TELEMETRY_PORT` every `BEACON_INTERVAL_MS`; the relay answers
  unicast to port 29471.
- **Requests.** `BR_BAD_REQ` for a malformed sector; `BR_NO_WIFI` when the station has no
  address (the one the screen shows); `BR_NO_RELAY` when no beacon has been heard. Otherwise one
  TCP connection: connect, send the bytes, read until the relay closes, all inside 2,500 ms (the
  Wii's own budget is 3,000 ms), close. A failed connect, send or read is `BR_CONNECT`, a spent
  budget `BR_TIMEOUT`, more than 4,084 bytes `BR_TOO_LARGE`. One attempt, no retries; the kiosk
  shows the error and A retries.
- **Telemetry.** One UDP datagram to the relay's `TELEMETRY_PORT`, best effort; dropped while
  there is no address or no relay.
- **Stopping.** It stands down when the station is ejected or restarts.

### What shows it

- `GET /status` carries a `"lazyto"` object in LazyTO mode only: the relay's address (or
  `null`), requests served, the last result, and the mailbox's counters (`API.md`).
- The screen's grey line under the station name ends in `RELAY` once the relay has been heard
  (`42% full RELAY`).
- The log has one line per request: `seq`, sizes, result, milliseconds. With `DEBUG`, the
  relay task's unused stack follows each one.

### Memory

| | Bytes | When |
|-|------:|------|
| `.bss`: HELLO, seqs, counters, the wake semaphore | ~240 | always |
| Mailbox buffers (request 512, telemetry 1,024, response 4,084), one `heap_caps_calloc` | 5,620 | once at boot, `LAZYTO=true` only |
| Relay task stack | 4,096 | once at boot, `LAZYTO=true` only |
| Relay task scratch (one request or one datagram on its way out) | 1,012 | once at boot, `LAZYTO=true` only |
| The UDP socket | ~500 | once, `LAZYTO=true` only |
| One request's TCP socket, pcb and reply segments | ~5,000 | per request, freed when the relay closes |

Against this fork's base built with the same toolchain: `.dram0.bss` +240 B, IRAM unchanged,
`.flash.text` +17 KB, `.flash.rodata` +2.5 KB. Nothing is allocated per request by the firmware
itself; lwIP's own allocations fail softly (`BR_CONNECT`).

### Hardware test

`tools/lazyto_host.py` plays the Wii from a Linux PC (step 1 of the test plan). It reads the
MBR, finds the first FAT32 partition's end as the firmware does, and prints HELLO; with
`--list-sets --secret S --station N` it writes a `CMD_LIST_SETS` request and polls the response
sector every 10 ms, then prints the transport result, the relay's status and message, and the
sets. All I/O is `O_DIRECT`. It takes every constant and offset from the generated header.

```
sudo tools/lazyto_host.py /dev/sdX
sudo tools/lazyto_host.py /dev/sdX --list-sets --secret S --station 1
```

### Known limits and open questions

1. **TCP connections.** `LWIP_MAX_ACTIVE_TCP` is 2 and httpd may hold 2 sockets. While Beamer
   Manager has both open, the relay connect fails (`BR_CONNECT`): exactly the game-end report.
   Raising the limit to 3 would also raise `HEAP_FLOOR` (it is computed from it) and so change
   when upstream's `LOW MEMORY` shows; capping httpd at one socket in LazyTO mode is the other
   option. Measure on hardware first.
2. **The relay task's stack** (4 KB) is unmeasured; `DEBUG` logs what is left after each request.
3. **A storage fault** (`NO SD CARD`, `SD UNREADABLE`, `NO USB`) makes the drive write-protected,
   which refuses mailbox writes too; after an eject the medium is gone. The Wii then times out.
4. **The relay is never forgotten**, only replaced by a newer beacon.
5. **HELLO's station** is the number on the Beamer's screen, for display. The station the relay
   acts on is the one the Wii puts in `relay_hdr`; the Beamer never reads it.

## Related, but not part of this branch

Labelling replays with the players' tags does not need the Beamer. Wii replays already carry
empty display-name fields in Game Start (offset 0x1A5, 31 bytes per port), and Replay Reporter
shows those fields first. The LazyTO kernel writes those bytes itself, so it can fill them in
from the kiosk's port claim. That work belongs in LazyTO, not here. Upstream could show those
names in `/status` with a small change to `src/slp.rs`.
