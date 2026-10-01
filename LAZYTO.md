# LazyTO over the Beamer (experimental, undecided)

This branch (`LazyTO`) explores one idea for [LazyTO](https://github.com/PranavMin/LazyTO): the
Beamer carries LazyTO's relay traffic, so the Wii never uses its own network. It is a
feasibility study, not a plan of record. Nothing here is built yet, and the upstream firmware
on `esp-32` is untouched.

Study written 2026-10-01 against upstream `8b655fa` (this fork's base), the LazyTO relay
(PranavMin/LazyTO), and LazyTO Nintendont (PranavMin/Nintendont, branch `LazyTO`).

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
- **The ESP owns the secret.** The ESP prepends the relay's 20-byte `relay_auth`. The secret
  lives in `CONFIG/config.txt`, so the Wii never holds it.
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
3. **Config keys** `LAZYTO-SECRET` and `LAZYTO-STREAM`. The station number comes from the button
   that already exists.
4. **Optional:** the LCD shows the relay state and the current set.

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
- **Each Beamer's `config.txt` needs the relay secret,** as well as the SSID and password.
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

## Related, but not part of this branch

Labelling replays with the players' tags does not need the Beamer. Wii replays already carry
empty display-name fields in Game Start (offset 0x1A5, 31 bytes per port), and Replay Reporter
shows those fields first. The LazyTO kernel writes those bytes itself, so it can fill them in
from the kiosk's port claim. That work belongs in LazyTO, not here. Upstream could show those
names in `/status` with a small change to `src/slp.rs`.
