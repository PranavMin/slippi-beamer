# HTTP API

All responses are JSON. There is no authentication: anyone who can reach the station over HTTP can drive this API.

| Method | Path             | What it does                                                                             |
| ------ | ---------------- | ---------------------------------------------------------------------------------------- |
| `GET`  | `/status`        | The last status report, straight off the two fragments. Runs nothing, so poll it freely. |
| `GET`  | `/SLIPPI/`       | Index of the replays this station is currently serving.                                  |
| `GET`  | `/SLIPPI/<file>` | The replay itself.                                                                       |
| `POST` | `/reset-beamer`  | Wipes the replay drive. Requires`X-Beamer-Confirm: reset`. Refused (`403`) in LazyTO mode. |

Setting `DEBUG` can sometimes add endpoints under `/debug/` - they're intentionally undocumented and unsupported. If you want to read the code and use them تَفَضَّلِي, but don't rely on them keeping the same shape or even existing on a new release.

## Schema

`GET /status`, `GET /SLIPPI/`, and every multicast carry `"schema": 1`. The number will go up if there's ever a breaking change: a key removed, renamed or retyped, or an existing key whose meaning changes.

## `GET /status`

Everything here is cached by the scan tick so this `GET` is very cheap - **it's the pollable endpoint**.

```json
{
  "schema": 1,
  "arch": "esp32",
  "firmware_version": "v0.2.2",
  "uptime": 8145, # seconds since boot
  "station_id": "3f2a...", # beamer uuid against factory mac address
  "station_name": "Station 2",
  "ssid": "nycmelee",
  "rssi": -58,
  "phy_mode": "HT20",
  "channel": 6,
  "replay_count": 47, # how many replays are stored - NOT how many are being served!
  "replay_cap": 512,
  "serving": 0, # replays in flight - always 0 or 1
  "ssh": false, # always false
  "game": {
    "live": false, # whether this game is still in progress
    "ports": [
      {
        "port": 1,
        "char": "Puff", # human readable
        "char_id": 15, # matches replay-manager-for-slippi numbering
        "color": null, # human readable, null for default
        "costume": 0, # matches replay-manager-for-slippi numbering
        "nametag": null
      },
      {
        "port": 4,
        "char": "Falco",
        "char_id": 20,
        "color": null,
        "costume": 0,
        "nametag": null
      }
    ]
  },  # game is null until a game has been started
  "secs_since_port_change": null, # how long have just these ports been in use
  "secs_since_character_change": null, # how long has this ports+characters combo been in use
  "secs_since_game_start": null, # how many seconds since the last game start
  # only with LAZYTO=true (this fork, see LAZYTO.md); absent otherwise.
  # "secret" says whether LAZYTO-SECRET is set: the secret itself is never shown.
  "lazyto": {"fw_build": 2, "station_set": true, "secret": true, "wifi": "up", "storage": "ok",
    "relay": "192.168.1.20:29470", "beacon_age_s": 1, "requests_served": 12, "last_result": "ok", "telemetry_dropped": 0,
    "mailbox": {"requests": 12, "repeats": 0, "malformed": 0, "telemetry": 40, "telemetry_bad": 0, "refused": 0, "stale": 0},
    "inventory": {"on_card": 9, "to_collect": 2, "to_erase": 7, "empty": 0, "incomplete": 0, "acks": 7}, "free_mb": 3790, "card_mb": 4093, "used_mb": 61,
    "erase": {"erased": 31, "erased_empty": 1, "erase_ms": 2140, "erase_left": 0, "cold": true, "failed": false, "acks_dropped": false},
    "sync": {"attempts": 40, "ok": 40, "acked": 7, "last": "ok", "last_age_s": 12}},
  # wifi: up, joining, no_ssid, cant_join, no_address, radio
  # storage: ok, no_card, unreadable, write_failed, wrong_format, filling, full
  # inventory is null until the first walk of the card; free_mb is null until the FAT is counted
  "health": "ok", # ok, starting, warn, or errror
  "warnings": []
  # note the lack of "errors" array - "health": "error" says you gotta walk up to the beamer anyway!
}
```

## `GET /SLIPPI/`

Not re-rendered on request - this is also a cheap and acceptable endpoint to poll. Doesn't include the game that's still being written.

```json
{
  "schema": 1,
  "station_id": "3f2a...",
  "served_replay_count": 2,
  "files": [
    {
      "size": 412393,
      "url": "/SLIPPI/Game_20260814T181203.slp"
    }
  ]
}
```

A few notes:

- `served_replay_count` is how many replays are **being served**; contrast`replay_count`in `GET /status` which is how many are on the drive
- Only `*.slp` are listed or served and filenames can't have spaces or certain special characters - this is meant for reading off of a Wii. Details are at `publish.rs::is_replay_name`.

## `GET /SLIPPI/<file>`

A few notes:

- In LazyTO mode any replay on the card is served by name, not only those in `GET /SLIPPI/` (which stays empty in that mode). The file being recorded right now is refused with `503` and a `Retry-After`; a 0-byte entry is `404`.

- Accepts `Accept-Encoding: gzip` (body comes back `Content-Encoding: gzip` with no `Content-Length`)
- `X-Replay-From: <n>` is the gzipped resume path. `n` counts uncompressed bytes. Answers `200` with `X-Replay-From` in the return header.
- `Range` answers `206` and is the uncompressed resume path. Gzip and `Range`are mutually exclusive - `Range`wins when both are present.
- This endpoint does not keep-alive, so sequential replay downloads should each be fetched on a fresh connection. The connection won't be closed Beamer side until you close it application side (to improve download speeds in poor network conditions) so make sure to **close the connection application-side upon receipt.**

## Odds and ends

`GET /` returns `403`. This is expected and intentional.

Posts can be refused with `409` - this is expected, handle it smoothly in application code. The beamer won't reset the drive while a game is live, so backoffs for that endpoint should be LONG. In LazyTO mode `POST /reset-beamer` is always refused (`403`): it would pull the drive out from under the Wii, and the beamer erases the replays the LazyTO laptop has collected itself, at its next power-on.

In LazyTO mode the HTTP server keeps one connection open at a time instead of two.

Sometimes an transfer will come back `503` - this usually means another application is already pulling from the beamer. Sometimes it's because of memory pressure for some other reason. Application code should respect the `Retry-After`, as it's meaningfully calculated rather than guessed wildly.

# UDP API

UDP is send-only - Beamers never accept UDP packets.

## Discovery

Every station advertises `_beamer._tcp` on port 80 over mDNS with the instance name as its hostname.

## Multicast announce

Not sent in LazyTO mode: the LazyTO relay learns each beamer from its sync (LAZYTO.md).

Whenever a game starts or finsihes a beamer will send a single UDP datagram to `239.255.42.1:34700` with multicast TTL 1. Joining that group lets an application poll `/status` and `/SLIPPI/` whenever game state changes. The datagram is best-effort and unacknowledged, so a missing one is normal - the source of truth is `/status` and `/SLIPPI/`.

`game_finished`:

```json
{
  "schema": 1,
  "event": "game_finished",
  "station_id": "3f2a...",
  "station_name": "Station 2",
  "seq": 7,
  "replay": {
    "name": "Game_20260814T181203.slp",
    "size": 412393, # final size on the card
    "url": "/SLIPPI/Game_20260814T181203.slp" #accessible right now
  },
  "game": { ... } # the same object as /status "game" - here "live": false
}
```

`game_started`:

```json
{
  "schema": 1,
  "event": "game_started",
  "station_id": "3f2a...",
  "station_name": "Station 2",
  "seq": 8,
  "replay": {
    "name": "Game_20260814T181203.slp",
    "size": 2134, # this is nonsense - don't worry about it!
    "url": "/SLIPPI/Game_20260814T181203.slp" # not valid until the game is finished!
  },
  "game": { ... } # the same object as /status "game" - here "live": true
}
```
