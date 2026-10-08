use core::fmt::Write as _;

use crate::slp::escape_json_into;

pub const STATUS_CAP: usize = 4096;
pub const INDEX_CAP: usize = 2560;

#[derive(Debug, Clone, Default)]
pub struct Buf<const N: usize> {
    s: heapless::String<N>,
    full: bool,
}

impl<const N: usize> Buf<N> {
    pub const fn new() -> Buf<N> {
        Buf {
            s: heapless::String::new(),
            full: false,
        }
    }

    fn reset(&mut self) {
        self.s.clear();
        self.full = false;
    }

    fn push_str(&mut self, v: &str) {
        if self.s.push_str(v).is_err() {
            self.full = true;
        }
    }

    fn push(&mut self, c: char) {
        if self.s.push(c).is_err() {
            self.full = true;
        }
    }

    pub fn as_bytes(&self) -> &[u8] {
        self.s.as_bytes()
    }

    pub fn as_str(&self) -> &str {
        self.s.as_str()
    }

    pub fn set_str(&mut self, v: &str) {
        self.reset();
        self.push_str(v);
        self.finish("a copied body");
    }

    fn finish(&mut self, what: &str) {
        if !self.full {
            return;
        }
        log::error!("{what} did not fit in {N} B -- serving an error body instead");
        self.s.clear();
        self.full = false;
        self.push_str("{\n  \"schema\": ");
        let _ = write!(
            self,
            "{SCHEMA},\n  \"error\": \"the body did not fit\"\n}}\n"
        );
    }
}

impl<const N: usize> core::fmt::Write for Buf<N> {
    fn write_str(&mut self, v: &str) -> core::fmt::Result {
        self.push_str(v);
        Ok(())
    }
}

pub const EMPTY_INDEX: &str =
    "{\n  \"schema\": 1,\n  \"station_id\": \"\",\n  \"served_replay_count\": 0,\n  \"files\": []\n}\n";
pub const ARCH: &str = "esp32";
pub const SCHEMA: u32 = 1;
pub const VERSION: &str = env!("BEAMER_VERSION");

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Health {
    Ok,
    Starting,
    Warn,
    Error,
}

impl Health {
    pub fn as_str(self) -> &'static str {
        match self {
            Health::Ok => "ok",
            Health::Starting => "starting",
            Health::Warn => "warn",
            Health::Error => "error",
        }
    }
}
pub const GAME_CAP: usize = 1024;

pub type GameJson = heapless::String<GAME_CAP>;

#[derive(Debug, Clone, Default)]
pub struct Fast {
    pub replay_count: u32,
    pub game: Option<GameJson>,
    pub port_change_at: Option<u64>,
    pub character_change_at: Option<u64>,
    pub game_start_at: Option<u64>,
}

impl Fast {
    pub const fn new() -> Fast {
        Fast {
            replay_count: 0,
            game: None,
            port_change_at: None,
            character_change_at: None,
            game_start_at: None,
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct LinkInfo {
    pub rssi: i32,
    pub phy: &'static str,
    pub channel: u8,
}

/// LazyTO mode's object in `GET /status` (LAZYTO.md, API.md). The secret
/// itself is never in it: only whether there is one.
#[derive(Debug, Clone, Copy)]
pub struct LazytoInfo {
    pub fw_build: u32,
    pub station_set: bool,
    pub secret: bool,
    pub wifi: &'static str,
    pub storage: &'static str,
    pub relay: Option<std::net::SocketAddrV4>,
    pub beacon_age_s: Option<u16>,
    pub requests_served: u32,
    pub last_result: Option<&'static str>,
    pub telemetry_dropped: u32,
    /// the mailbox's own counters, by name
    pub mailbox: [(&'static str, u32); 7],
    /// the inventory's counts, by name (`None` before its first walk)
    pub inventory: Option<[(&'static str, u32); 6]>,
    pub free_mb: Option<u32>,
    pub card_mb: u32,
    pub used_mb: u32,
    /// this boot's cold-boot erase, by name
    pub erase_counts: [(&'static str, u32); 4],
    pub erase_flags: [(&'static str, bool); 3],
    /// the syncs, by name
    pub sync_counts: [(&'static str, u32); 3],
    pub sync_last: Option<&'static str>,
    pub sync_last_age_s: Option<u64>,
}

#[allow(clippy::too_many_arguments)]
pub fn status_json(
    station_id: &str,
    station_name: &str,
    ssid: Option<&str>,
    link: Option<LinkInfo>,
    fast: &Fast,
    replay_cap: u32,
    serving: u32,
    now_s: u64,
    has_errors: bool,
    warnings: &[&str],
    net: Health,
    lazyto: Option<LazytoInfo>,
    s: &mut Buf<STATUS_CAP>,
) {
    let health = match (has_errors, net) {
        (true, _) | (_, Health::Error) => Health::Error,
        (_, Health::Starting) => Health::Starting,
        _ if !warnings.is_empty() => Health::Warn,
        (_, v) => v,
    };

    s.reset();
    s.push_str("{\n");
    let _ = writeln!(s, "  \"schema\": {SCHEMA},");
    let _ = writeln!(s, "  \"arch\": \"{ARCH}\",");
    line_str(s, "firmware_version", Some(VERSION));
    let _ = writeln!(s, "  \"uptime\": {now_s},");

    line_str(s, "station_id", Some(station_id));
    line_str(s, "station_name", Some(station_name));
    line_str(s, "ssid", ssid);

    match link {
        Some(l) => {
            let _ = writeln!(s, "  \"rssi\": {},", l.rssi);
            line_str(s, "phy_mode", Some(l.phy));
            let _ = writeln!(s, "  \"channel\": {},", l.channel);
        }
        None => {
            s.push_str("  \"rssi\": null,\n");
            s.push_str("  \"phy_mode\": null,\n");
            s.push_str("  \"channel\": null,\n");
        }
    }

    let _ = writeln!(s, "  \"replay_count\": {},", fast.replay_count);
    let _ = writeln!(s, "  \"replay_cap\": {replay_cap},");
    let _ = writeln!(s, "  \"serving\": {serving},");
    s.push_str("  \"ssh\": false,\n"); // always false - esp32 beamers have no ssh!

    match &fast.game {
        Some(g) => {
            let _ = writeln!(s, "  \"game\": {},", g.as_str());
        }
        None => s.push_str("  \"game\": null,\n"),
    }
    line_secs_since(s, "secs_since_port_change", fast.port_change_at, now_s);
    line_secs_since(
        s,
        "secs_since_character_change",
        fast.character_change_at,
        now_s,
    );
    line_secs_since(s, "secs_since_game_start", fast.game_start_at, now_s);
    if let Some(l) = lazyto {
        lazyto_json(s, &l);
    }
    line_str(s, "health", Some(health.as_str()));

    s.push_str("  \"warnings\": ");
    json_array(s, warnings);
    s.push('\n');
    s.push('}');
    s.push('\n');
    s.finish("GET /status");
}

fn opt_json<const N: usize, T: core::fmt::Display>(s: &mut Buf<N>, v: Option<T>, quoted: bool) {
    match v {
        Some(v) if quoted => {
            let _ = write!(s, "\"{v}\"");
        }
        Some(v) => {
            let _ = write!(s, "{v}");
        }
        None => s.push_str("null"),
    }
}

fn fields_json<const N: usize, T: core::fmt::Display>(s: &mut Buf<N>, items: &[(&str, T)]) {
    for (i, (name, v)) in items.iter().enumerate() {
        let sep = if i == 0 { "" } else { ", " };
        let _ = write!(s, "{sep}\"{name}\": {v}");
    }
}

fn lazyto_json<const N: usize>(s: &mut Buf<N>, l: &LazytoInfo) {
    let _ = write!(
        s,
        "  \"lazyto\": {{\"fw_build\": {}, \"station_set\": {}, \"secret\": {}, \"wifi\": \"{}\", \"storage\": \"{}\",\n    \"relay\": ",
        l.fw_build, l.station_set, l.secret, l.wifi, l.storage
    );
    opt_json(s, l.relay, true);
    s.push_str(", \"beacon_age_s\": ");
    opt_json(s, l.beacon_age_s, false);
    let _ = write!(s, ", \"requests_served\": {}", l.requests_served);
    s.push_str(", \"last_result\": ");
    opt_json(s, l.last_result, true);
    let _ = write!(s, ", \"telemetry_dropped\": {}", l.telemetry_dropped);
    s.push_str(",\n    \"mailbox\": {");
    fields_json(s, &l.mailbox);
    s.push_str("},\n    \"inventory\": ");
    match &l.inventory {
        Some(c) => {
            s.push('{');
            fields_json(s, c);
            s.push('}');
        }
        None => s.push_str("null"),
    }
    s.push_str(", \"free_mb\": ");
    opt_json(s, l.free_mb, false);
    let _ = write!(
        s,
        ", \"card_mb\": {}, \"used_mb\": {}",
        l.card_mb, l.used_mb
    );
    s.push_str(",\n    \"erase\": {");
    fields_json(s, &l.erase_counts);
    s.push_str(", ");
    fields_json(s, &l.erase_flags);
    s.push_str("},\n    \"sync\": {");
    fields_json(s, &l.sync_counts);
    s.push_str(", \"last\": ");
    opt_json(s, l.sync_last, true);
    s.push_str(", \"last_age_s\": ");
    opt_json(s, l.sync_last_age_s, false);
    s.push_str("}},\n");
}

pub fn index_json(station_id: &str, files: &[(&str, u64)], s: &mut Buf<INDEX_CAP>) {
    s.reset();
    s.push_str("{\n");
    let _ = writeln!(s, "  \"schema\": {SCHEMA},");
    line_str(s, "station_id", Some(station_id));
    let _ = writeln!(s, "  \"served_replay_count\": {},", files.len());
    s.push_str("  \"files\": [");
    for (i, (name, size)) in files.iter().enumerate() {
        if i > 0 {
            s.push(',');
        }
        s.push_str("\n    {\"size\": ");
        let _ = write!(s, "{size}, \"url\": \"/SLIPPI/");
        escape_json_into(name, s);
        s.push_str("\"}");
    }
    if !files.is_empty() {
        s.push('\n');
        s.push_str("  ");
    }
    s.push_str("]\n}\n");
    s.finish("the replay index");
}

fn line_secs_since<const N: usize>(out: &mut Buf<N>, key: &str, at: Option<u64>, now_s: u64) {
    match at {
        Some(at) => {
            let _ = writeln!(out, "  \"{key}\": {},", now_s.saturating_sub(at));
        }
        None => {
            let _ = writeln!(out, "  \"{key}\": null,");
        }
    }
}

fn line_str<const N: usize>(out: &mut Buf<N>, key: &str, value: Option<&str>) {
    let _ = write!(out, "  \"{key}\": ");
    match value {
        Some(v) => {
            out.push('"');
            escape_json_into(v, out);
            out.push('"');
        }
        None => out.push_str("null"),
    }
    out.push_str(",\n");
}

fn json_array<S: AsRef<str>, const N: usize>(out: &mut Buf<N>, items: &[S]) {
    if items.is_empty() {
        out.push_str("[]");
        return;
    }
    out.push('[');
    for (i, item) in items.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        out.push_str("\n    \"");
        escape_json_into(item.as_ref(), out);
        out.push('"');
    }
    out.push_str("\n  ]");
}
