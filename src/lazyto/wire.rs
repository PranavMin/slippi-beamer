//! Small pure helpers LazyTO mode shares: how a replay file is identified,
//! the CRC32 an ack carries, and which names it handles.
//!
//! No crate imports, so it also builds on a PC (`tools/lazyto_host_tests.sh`).

/// The low bits of a [`key`] are always zero, so a table can keep per-file
/// flags there (`known.rs`).
pub const KEY_FLAGS: u64 = 0x1F;

/// FNV-1a, 64-bit.
pub fn fnv64(parts: &[&[u8]]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for part in parts {
        for b in part.iter() {
            h ^= *b as u64;
            h = h.wrapping_mul(0x0000_0100_0000_01b3);
        }
    }
    h
}

/// What identifies one file on the card: its name, its size and its FAT
/// modified date and time. A name reused by a Wii whose clock went back, or
/// a file that grew, is a different key. Never 0.
pub fn key(name: &str, size: u32, mtime: u32) -> u64 {
    let k = fnv64(&[name.as_bytes(), &size.to_le_bytes(), &mtime.to_le_bytes()]) & !KEY_FLAGS;
    if k == 0 {
        KEY_FLAGS + 1
    } else {
        k
    }
}

/// FAT modified date and time as one word: `date << 16 | time`, as
/// `sync_file.mtime` carries it.
pub fn fat_mtime(fdate: u16, ftime: u16) -> u32 {
    (fdate as u32) << 16 | ftime as u32
}

/// How much of a file's start an ack's CRC32 covers.
pub const HEAD_BYTES: usize = 1024;

/// CRC-32 (IEEE 802.3, reflected, as zlib's), bit by bit: the beamer runs it
/// over [`HEAD_BYTES`] per file, fed in pieces as they are read.
#[derive(Debug, Clone, Copy)]
pub struct Crc32(u32);

impl Crc32 {
    pub fn new() -> Crc32 {
        Crc32(!0)
    }

    pub fn update(&mut self, data: &[u8]) {
        for b in data {
            self.0 ^= *b as u32;
            for _ in 0..8 {
                let mask = (self.0 & 1).wrapping_neg();
                self.0 = (self.0 >> 1) ^ (0xEDB8_8320 & mask);
            }
        }
    }

    pub fn finish(self) -> u32 {
        !self.0
    }
}

impl Default for Crc32 {
    fn default() -> Self {
        Crc32::new()
    }
}

pub fn crc32(data: &[u8]) -> u32 {
    let mut c = Crc32::new();
    c.update(data);
    c.finish()
}

/// A Slippi Nintendont replay name: `Game_<anything safe>.slp`. Only these
/// are inventoried, collected and erased.
pub fn is_game_name(name: &str) -> bool {
    let b = name.as_bytes();
    b.len() > 9
        && b.len() <= 255
        && b[..5].eq_ignore_ascii_case(b"Game_")
        && b[b.len() - 4..].eq_ignore_ascii_case(b".slp")
        && b.iter()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'.' | b'_' | b'-'))
}

pub fn put_be16(b: &mut [u8], at: usize, v: u16) {
    b[at..at + 2].copy_from_slice(&v.to_be_bytes());
}

pub fn put_be32(b: &mut [u8], at: usize, v: u32) {
    b[at..at + 4].copy_from_slice(&v.to_be_bytes());
}

pub fn be16(b: &[u8], at: usize) -> u16 {
    u16::from_be_bytes([b[at], b[at + 1]])
}

/// Saturates a count into a u16 wire field.
pub fn sat16(n: u32) -> u16 {
    n.min(u16::MAX as u32) as u16
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crc32_check_value() {
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
        assert_eq!(crc32(b""), 0);
        let mut c = Crc32::new();
        c.update(b"1234");
        c.update(b"56789");
        assert_eq!(c.finish(), 0xCBF4_3926);
    }

    #[test]
    fn fnv64_known_answers() {
        assert_eq!(fnv64(&[b""]), 0xcbf2_9ce4_8422_2325);
        assert_eq!(fnv64(&[b"a"]), 0xaf63_dc4c_8601_ec8c);
        assert_eq!(fnv64(&[b"foo", b"bar"]), fnv64(&[b"foobar"]));
    }

    #[test]
    fn key_separates_size_and_time() {
        let n = "Game_0017AB12CD34_20261007T201502.slp";
        let k = key(n, 4096, 7);
        assert_eq!(k & KEY_FLAGS, 0);
        assert_ne!(k, 0);
        assert_eq!(k, key(n, 4096, 7));
        assert_ne!(k, key(n, 4097, 7));
        assert_ne!(k, key(n, 4096, 8));
        assert_ne!(k, key("Game_0017AB12CD34_20261007T201503.slp", 4096, 7));
    }

    #[test]
    fn game_names() {
        assert!(is_game_name("Game_0017AB12CD34_20261007T201502.slp"));
        assert!(is_game_name("game_x.SLP"));
        assert!(!is_game_name("Game_.slp"));
        assert!(!is_game_name("Replay_1.slp"));
        assert!(!is_game_name("Game_a b.slp"));
        assert!(!is_game_name("Game_x.txt"));
    }

    #[test]
    fn mtime_packs_date_high() {
        assert_eq!(fat_mtime(0x5947, 0xA1B2), 0x5947_A1B2);
    }
}
