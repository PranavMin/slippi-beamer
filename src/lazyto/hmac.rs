//! HMAC-SHA256 (RFC 2104) over any SHA-256 engine, and a constant-time
//! compare. The beamer checks the signature on a sync reply with it
//! (`sync.rs`), and derives `relay_auth`'s key from the secret
//! ([`auth_key`]), on the ESP32-S3's SHA accelerator (`sha.rs`).
//!
//! Pure Rust with no crate imports, so it also builds on a PC:
//! `tools/lazyto_host_tests.sh` runs the tests below, including the sync
//! signature and `relay_auth` key vectors from LazyTO's docs/protocol-v2.md.

/// SHA-256's block size, and so HMAC's key block.
pub const BLOCK: usize = 64;
/// SHA-256's digest size.
pub const DIGEST: usize = 32;
/// `relay_auth`'s key is HMAC-SHA256 of these 17 ASCII bytes, keyed with
/// the NUL-padded secret.
pub const AUTH_LABEL: &[u8] = b"LazyTO relay_auth";
/// How much of that HMAC `relay_auth` carries (`SECRET_LEN`).
pub const AUTH_KEY: usize = 16;

/// `relay_auth`'s key for a NUL-padded secret: the first [`AUTH_KEY`] bytes
/// of HMAC-SHA256(secret, [`AUTH_LABEL`]). The beamer sends this, never the
/// secret: `relay_auth` goes to whichever host sent the last beacon, and the
/// secret itself keys the sync reply's signature, which lets the beamer
/// erase.
pub fn auth_key<H: Sha256>(new: impl FnMut() -> H, secret: &[u8]) -> [u8; AUTH_KEY] {
    let mac = hmac(new, secret, &[AUTH_LABEL]);
    let mut key = [0u8; AUTH_KEY];
    key.copy_from_slice(&mac[..AUTH_KEY]);
    key
}

/// One SHA-256 computation: feed it, then take the digest.
pub trait Sha256 {
    fn update(&mut self, data: &[u8]);
    fn finish(self) -> [u8; DIGEST];
}

/// HMAC-SHA256 of the concatenation of `parts`, keyed with `key`. `new`
/// starts a fresh hash each time it is called.
pub fn hmac<H: Sha256>(mut new: impl FnMut() -> H, key: &[u8], parts: &[&[u8]]) -> [u8; DIGEST] {
    let mut k = [0u8; BLOCK];
    if key.len() > BLOCK {
        let mut h = new();
        h.update(key);
        k[..DIGEST].copy_from_slice(&h.finish());
    } else {
        k[..key.len()].copy_from_slice(key);
    }

    let mut pad = [0u8; BLOCK];
    for (p, k) in pad.iter_mut().zip(k.iter()) {
        *p = k ^ 0x36;
    }
    let mut inner = new();
    inner.update(&pad);
    for part in parts {
        inner.update(part);
    }
    let inner = inner.finish();

    for (p, k) in pad.iter_mut().zip(k.iter()) {
        *p = k ^ 0x5c;
    }
    let mut outer = new();
    outer.update(&pad);
    outer.update(&inner);
    outer.finish()
}

/// `a == b`, in time that depends only on the lengths.
pub fn equal(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A plain software SHA-256 (FIPS 180-4), for the tests only: the
    /// firmware hashes on the SHA accelerator.
    struct Soft {
        h: [u32; 8],
        block: [u8; 64],
        used: usize,
        len: u64,
    }

    const K: [u32; 64] = [
        0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4,
        0xab1c5ed5, 0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe,
        0x9bdc06a7, 0xc19bf174, 0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f,
        0x4a7484aa, 0x5cb0a9dc, 0x76f988da, 0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7,
        0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967, 0x27b70a85, 0x2e1b2138, 0x4d2c6dfc,
        0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85, 0xa2bfe8a1, 0xa81a664b,
        0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070, 0x19a4c116,
        0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
        0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7,
        0xc67178f2,
    ];

    impl Soft {
        fn new() -> Soft {
            Soft {
                h: [
                    0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c,
                    0x1f83d9ab, 0x5be0cd19,
                ],
                block: [0; 64],
                used: 0,
                len: 0,
            }
        }

        fn compress(&mut self) {
            let mut w = [0u32; 64];
            for (i, c) in self.block.chunks_exact(4).enumerate() {
                w[i] = u32::from_be_bytes([c[0], c[1], c[2], c[3]]);
            }
            for i in 16..64 {
                let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
                let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
                w[i] = w[i - 16]
                    .wrapping_add(s0)
                    .wrapping_add(w[i - 7])
                    .wrapping_add(s1);
            }
            let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut h] = self.h;
            for i in 0..64 {
                let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
                let ch = (e & f) ^ (!e & g);
                let t1 = h
                    .wrapping_add(s1)
                    .wrapping_add(ch)
                    .wrapping_add(K[i])
                    .wrapping_add(w[i]);
                let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
                let maj = (a & b) ^ (a & c) ^ (b & c);
                let t2 = s0.wrapping_add(maj);
                h = g;
                g = f;
                f = e;
                e = d.wrapping_add(t1);
                d = c;
                c = b;
                b = a;
                a = t1.wrapping_add(t2);
            }
            for (s, v) in self.h.iter_mut().zip([a, b, c, d, e, f, g, h]) {
                *s = s.wrapping_add(v);
            }
        }
    }

    impl Sha256 for Soft {
        fn update(&mut self, mut data: &[u8]) {
            self.len = self.len.wrapping_add(data.len() as u64);
            while !data.is_empty() {
                let take = (64 - self.used).min(data.len());
                self.block[self.used..self.used + take].copy_from_slice(&data[..take]);
                self.used += take;
                data = &data[take..];
                if self.used == 64 {
                    self.compress();
                    self.used = 0;
                }
            }
        }

        fn finish(mut self) -> [u8; DIGEST] {
            let bits = self.len.wrapping_mul(8);
            self.update(&[0x80]);
            while self.used != 56 {
                self.update(&[0]);
            }
            self.block[56..].copy_from_slice(&bits.to_be_bytes());
            self.compress();
            let mut out = [0u8; DIGEST];
            for (c, w) in out.chunks_exact_mut(4).zip(self.h) {
                c.copy_from_slice(&w.to_be_bytes());
            }
            out
        }
    }

    fn hex(s: &str) -> Vec<u8> {
        (0..s.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
            .collect()
    }

    fn sha(data: &[u8]) -> [u8; DIGEST] {
        let mut h = Soft::new();
        h.update(data);
        h.finish()
    }

    #[test]
    fn soft_sha256_known_answers() {
        assert_eq!(
            sha(b"abc").to_vec(),
            hex("ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad")
        );
        // docs/protocol-v2.md: SHA-256("replay")
        assert_eq!(
            sha(b"replay").to_vec(),
            hex("ac203c9843b5bd8c883e07039ff82820c94422010be6108bb82403ca25376a22")
        );
    }

    #[test]
    fn rfc4231_case_1() {
        let key = [0x0bu8; 20];
        let mac = hmac(Soft::new, &key, &[b"Hi ", b"There"]);
        assert_eq!(
            mac.to_vec(),
            hex("b0344c61d8db38535ca8afceaf0bf12b881dc200c9833da726e9376c2e32cff7")
        );
    }

    #[test]
    fn rfc4231_case_6_long_key() {
        let key = [0xaau8; 131];
        let mac = hmac(
            Soft::new,
            &key,
            &[b"Test Using Larger Than Block-Size Key - Hash Key First"],
        );
        assert_eq!(
            mac.to_vec(),
            hex("60e431591ee0b67f0d8a26aacbf5b77f8e0bc6213728c5140546040f0ee37f54")
        );
    }

    /// docs/protocol-v2.md "Test vector: the sync signature", byte for byte.
    #[test]
    fn protocol_v2_sync_signature() {
        let mut key = [0u8; 16];
        key[..12].copy_from_slice(b"venue-secret");
        let nonce: Vec<u8> = (0x00..=0x0f).collect();
        let station_id: Vec<u8> = (0x10..=0x1f).collect();

        // beamer_sync_resp after hmac: archive_id, answer_count, _pad[3],
        // then three sync_answer (answer, _pad[3], sha256[32])
        let mut body = Vec::new();
        body.extend(0xa0..=0xafu8);
        body.extend([3, 0, 0, 0]);
        body.extend([1, 0, 0, 0]);
        body.extend(sha(b"replay"));
        body.extend([2, 0, 0, 0]);
        body.extend([0u8; 32]);
        body.extend([0, 0, 0, 0]);
        body.extend([0u8; 32]);
        assert_eq!(32 + body.len(), 160, "the whole payload is 160 bytes");

        let mac = hmac(Soft::new, &key, &[&nonce, &station_id, &body]);
        assert_eq!(
            mac.to_vec(),
            hex("dff09c43e230fa9913e545f4df881b9f78dbc9046530bfe3654b6583e988e48b")
        );
    }

    /// docs/protocol-v2.md "Test vector: relay_auth's key": the key, not the
    /// secret, and not what signs a sync reply.
    #[test]
    fn protocol_v2_relay_auth_key() {
        let mut secret = [0u8; 16];
        secret[..12].copy_from_slice(b"venue-secret");
        let key = auth_key(Soft::new, &secret);
        assert_eq!(key.to_vec(), hex("f5a1b91cb53cb4b14fd06c5ea683c221"));
        assert_ne!(key, secret);
        let mut other = [0u8; 16];
        other[..12].copy_from_slice(b"other-secret");
        assert_ne!(auth_key(Soft::new, &other), key);
    }

    #[test]
    fn equal_is_exact() {
        assert!(equal(b"abc", b"abc"));
        assert!(!equal(b"abc", b"abd"));
        assert!(!equal(b"abc", b"ab"));
    }
}
