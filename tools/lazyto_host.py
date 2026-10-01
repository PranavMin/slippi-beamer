#!/usr/bin/env python3
"""Plays the Wii against a real Beamer in LazyTO mode (LAZYTO.md), for the
first hardware test. Linux only, no dependencies; run as root (or a user who
can open the block device).

    sudo tools/lazyto_host.py /dev/sdX
    sudo tools/lazyto_host.py /dev/sdX --list-sets --secret S --station N

It reads the MBR, finds the first FAT32 partition's end (where the mailbox
starts, exactly as the firmware does), and prints the HELLO sector. With
--list-sets it writes a CMD_LIST_SETS request to the REQUEST sector and polls
the RESPONSE sector until the Beamer answers, then prints the transport
result and the relay's status, message and sets.

Every read and write is O_DIRECT, so the kernel's page cache never answers
for the Beamer: the mailbox changes under it.

Layouts and constants come from the generated header the firmware builds
with (components/beamer_lazyto/include/relay_proto.h): its #defines, enum
values and the offsetof/sizeof asserts. Only the field widths are written
out here.
"""

import argparse
import fcntl
import mmap
import os
import re
import struct
import sys
import time
from pathlib import Path

sys.dont_write_bytecode = True

HEADER = Path(__file__).resolve().parent.parent / "components/beamer_lazyto/include/relay_proto.h"

BLKSSZGET = 0x1268  # <linux/fs.h>: logical sector size
FAT32_TYPES = (0x0B, 0x0C)


class Proto:
    """Names from relay_proto.h: #defines, enum members, offsets, sizes."""

    def __init__(self, path: Path):
        text = path.read_text()
        self.const = {}
        for name, value in re.findall(r"^#define\s+(\w+)\s+(0x[0-9A-Fa-f]+|\d+|'.')", text, re.M):
            self.const[name] = ord(value[1]) if value.startswith("'") else int(value, 0)
        for name, value in re.findall(r"^\s+([A-Z][A-Z0-9_]+)\s*=\s*(\d+),", text, re.M):
            self.const[name] = int(value)
        self.offset = {}
        for s, f, n in re.findall(r"offsetof\(struct (\w+), (\w+)\) == (\d+)", text):
            self.offset[(s, f)] = int(n)
        self.size = {s: int(n) for s, n in re.findall(r"sizeof\(struct (\w+)\) == (\d+)", text)}

    def __getitem__(self, name):
        return self.const[name]

    def off(self, struct_name, field):
        return self.offset[(struct_name, field)]

    def names(self, prefix):
        """value -> name for the enum members that start with prefix"""
        return {v: k for k, v in self.const.items() if k.startswith(prefix)}


class Disk:
    def __init__(self, path: str, write: bool):
        flags = (os.O_RDWR if write else os.O_RDONLY) | os.O_DIRECT | os.O_SYNC
        self.fd = os.open(path, flags)
        buf = bytearray(4)
        try:
            fcntl.ioctl(self.fd, BLKSSZGET, buf)
            self.ssz = struct.unpack("i", buf)[0]
        except OSError:
            self.ssz = 512  # a disk image rather than a device

    def read(self, lba: int, count: int) -> bytes:
        buf = mmap.mmap(-1, count * self.ssz)  # page-aligned, as O_DIRECT needs
        n = os.preadv(self.fd, [buf], lba * self.ssz)
        if n != len(buf):
            raise OSError(f"short read at lba {lba}: {n} of {len(buf)} bytes")
        return bytes(buf)

    def write(self, lba: int, data: bytes):
        if len(data) % self.ssz:
            raise ValueError("writes are whole sectors")
        buf = mmap.mmap(-1, len(data))
        buf[:] = data
        n = os.pwritev(self.fd, [buf], lba * self.ssz)
        if n != len(data):
            raise OSError(f"short write at lba {lba}: {n} of {len(data)} bytes")


def be16(b, o):
    return struct.unpack_from(">H", b, o)[0]


def be32(b, o):
    return struct.unpack_from(">I", b, o)[0]


def text(b: bytes) -> str:
    return b.split(b"\0", 1)[0].decode("ascii", "replace")


def mailbox_start(disk: Disk) -> int:
    mbr = disk.read(0, 1)
    if mbr[510:512] != b"\x55\xaa":
        sys.exit("no MBR signature in sector 0")
    for i in range(4):
        e = mbr[446 + 16 * i : 446 + 16 * (i + 1)]
        if e[4] in FAT32_TYPES:
            start, sectors = struct.unpack_from("<II", e, 8)
            print(f"FAT32 partition {i + 1}: lba {start} + {sectors} sectors")
            return start + sectors
    sys.exit("no FAT32 partition in the MBR")


def show_hello(p: Proto, sector: bytes) -> bool:
    h = "beamer_hello"
    magic = sector[p.off(h, "magic") : p.off(h, "magic") + 8]
    version = sector[p.off(h, "version")]
    if magic != b"LAZYTOMB" or version != p["BEAMER_MB_VERSION"]:
        print(f"HELLO: no LazyTO mailbox here (magic {magic!r}, version {version})")
        return False
    flags = sector[p.off(h, "flags")]
    names = [n for v, n in sorted(p.names("BF_").items()) if flags & v]
    ip = be32(sector, p.off(h, "relay_ip"))
    port = be16(sector, p.off(h, "relay_port"))
    relay = f"{ip >> 24}.{(ip >> 16) & 255}.{(ip >> 8) & 255}.{ip & 255}:{port}" if ip else "unknown"
    print(
        f"HELLO: version {version}, flags 0x{flags:02x} [{' '.join(names) or '-'}], "
        f"station {be16(sector, p.off(h, 'station'))}, relay {relay}, "
        f"fw_build {be32(sector, p.off(h, 'fw_build'))}"
    )
    return True


def list_sets_request(p: Proto, seq: int, secret: str, station: int) -> bytes:
    auth = bytearray(p.size["relay_auth"])
    auth[0], auth[1] = p["AUTH_MAGIC_0"], p["AUTH_MAGIC_1"]
    s = secret.encode("ascii")
    if len(s) > p["SECRET_LEN"]:
        sys.exit(f"--secret is at most {p['SECRET_LEN']} characters")
    o = p.off("relay_auth", "secret")
    auth[o : o + len(s)] = s

    hdr = bytearray(p.size["relay_hdr"])
    hdr[0], hdr[1] = p["RELAY_MAGIC_0"], p["RELAY_MAGIC_1"]
    hdr[p.off("relay_hdr", "version")] = p["RELAY_PROTO_VERSION"]
    hdr[p.off("relay_hdr", "cmd")] = p["CMD_LIST_SETS"]
    struct.pack_into(">H", hdr, p.off("relay_hdr", "station"), station)
    struct.pack_into(">H", hdr, p.off("relay_hdr", "len"), 0)
    body = bytes(auth + hdr)

    req = bytearray(p["BEAMER_SECTOR_SIZE"])
    req[0], req[1] = ord("M"), ord("Q")
    struct.pack_into(">I", req, p.off("beamer_req_hdr", "seq"), seq)
    struct.pack_into(">H", req, p.off("beamer_req_hdr", "len"), len(body))
    o = p.size["beamer_req_hdr"]
    req[o : o + len(body)] = body
    return bytes(req)


def show_reply(p: Proto, reply: bytes):
    hdr, resp = p.size["relay_hdr"], p.size["relay_resp"]
    if len(reply) < hdr + resp:
        print(f"reply: {len(reply)} bytes, too short for relay_hdr + relay_resp: {reply.hex()}")
        return
    if reply[0] != p["RELAY_MAGIC_0"] or reply[1] != p["RELAY_MAGIC_1"]:
        print(f"reply: bad magic {reply[:2]!r}")
    status = reply[hdr + p.off("relay_resp", "status")]
    msg = text(reply[hdr + p.off("relay_resp", "msg") : hdr + resp])
    print(f"relay: status {status} ({p.names('ST_').get(status, '?')}), msg {msg!r}")
    if status != p["ST_OK"] or reply[p.off("relay_hdr", "cmd")] != p["CMD_LIST_SETS"]:
        return

    o = hdr + resp
    count = be16(reply, o + p.off("list_sets_resp", "count"))
    o += p.off("list_sets_resp", "sets")
    row = p.size["set_entry"]
    print(f"sets: {count}")
    for i in range(count):
        e = reply[o + i * row : o + (i + 1) * row]
        if len(e) < row:
            print("  (truncated)")
            break
        rnd = text(e[p.off("set_entry", "round") : p.off("set_entry", "p1_tag")])
        p1 = text(e[p.off("set_entry", "p1_tag") : p.off("set_entry", "p2_tag")])
        p2 = text(e[p.off("set_entry", "p2_tag") : p.off("set_entry", "best_of")])
        print(
            f"  {be32(e, p.off('set_entry', 'set_id')):>10}  {rnd:<24} {p1} vs {p2}"
            f"  bo{e[p.off('set_entry', 'best_of')]} state {e[p.off('set_entry', 'state')]}"
        )


def list_sets(p: Proto, disk: Disk, first: int, secret: str, station: int, timeout: float):
    req_lba = first + p["BEAMER_MB_REQ"]
    resp_lba = first + p["BEAMER_MB_RESP"]

    last = be32(disk.read(req_lba, 1), p.off("beamer_req_hdr", "seq"))
    seq = (last + 1) & 0xFFFFFFFF or 1  # never the last seq: the Beamer would take it for a retry

    t0 = time.monotonic()
    disk.write(req_lba, list_sets_request(p, seq, secret, station))
    print(f"request: seq {seq} written to lba {req_lba}")

    polls = 0
    while True:
        head = disk.read(resp_lba, 1)
        polls += 1
        if head[0:2] == b"MR" and be32(head, p.off("beamer_resp_hdr", "seq")) == seq:
            break
        if time.monotonic() - t0 > timeout:
            sys.exit(f"no response for seq {seq} after {timeout:.1f} s ({polls} polls)")
        time.sleep(0.01)

    full = disk.read(resp_lba, p["BEAMER_MB_RESP_SECTORS"])
    ms = (time.monotonic() - t0) * 1000
    h = "beamer_resp_hdr"
    if be32(full, p.off(h, "seq")) != seq:
        sys.exit("the response changed between the poll and the full read")
    result = full[p.off(h, "result")]
    length = be16(full, p.off(h, "len"))
    print(
        f"response: seq {seq}, result {result} ({p.names('BR_').get(result, '?')}), "
        f"{length} bytes, {ms:.0f} ms, {polls} polls"
    )
    if result == p["BR_OK"]:
        o = p.size[h]
        show_reply(p, full[o : o + length])


def main():
    ap = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    ap.add_argument("device", help="the Beamer's block device, e.g. /dev/sdb (not a partition)")
    ap.add_argument("--list-sets", action="store_true", help="send CMD_LIST_SETS through the mailbox")
    ap.add_argument("--secret", help="the relay's shared secret (RELAY_SECRET / secret=)")
    ap.add_argument("--station", type=int, help="the station number to ask for")
    ap.add_argument("--timeout", type=float, default=3.0, help="seconds to wait for the answer (the Wii's budget is 3)")
    ap.add_argument("--proto", type=Path, default=HEADER, help="relay_proto.h to take the layout from")
    args = ap.parse_args()

    if args.list_sets and (args.secret is None or args.station is None):
        ap.error("--list-sets needs --secret and --station")

    p = Proto(args.proto)
    disk = Disk(args.device, write=args.list_sets)
    if disk.ssz != p["BEAMER_SECTOR_SIZE"]:
        sys.exit(f"{args.device} has {disk.ssz}-byte sectors; the mailbox needs {p['BEAMER_SECTOR_SIZE']}")

    first = mailbox_start(disk)
    print(f"mailbox: lba {first}..{first + p['BEAMER_MB_SECTORS'] - 1}")
    if not show_hello(p, disk.read(first + p["BEAMER_MB_HELLO"], 1)):
        sys.exit(2)

    if args.list_sets:
        list_sets(p, disk, first, args.secret, args.station, args.timeout)


if __name__ == "__main__":
    main()
