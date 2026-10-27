//! MD5 + name-based (version 3) UUID + SHA-256 hex helpers.
//!
//! Java's `io.agentscope.core.rag.model.Document` derives its id with
//! `UUID.nameUUIDFromBytes(jsonKey.getBytes(UTF_8))`, i.e. a **version 3 (MD5)**
//! name-based UUID over the serialized `{doc_id, chunk_id, content}` key. In order to
//! reproduce that id faithfully without pulling a new third-party crate (the root
//! `Cargo.toml` may not be modified by this module), MD5 is implemented here from
//! RFC 1321.

/// Per-round left-rotation amounts (MD5 `s` table).
const S: [u32; 64] = [
    7, 12, 17, 22, 7, 12, 17, 22, 7, 12, 17, 22, 7, 12, 17, 22, 5, 9, 14, 20, 5, 9, 14, 20, 5, 9,
    14, 20, 5, 9, 14, 20, 4, 11, 16, 23, 4, 11, 16, 23, 4, 11, 16, 23, 4, 11, 16, 23, 6, 10, 15,
    21, 6, 10, 15, 21, 6, 10, 15, 21, 6, 10, 15, 21,
];

/// MD5 additive constants `K[i] = floor(2^32 * abs(sin(i + 1)))`.
const K: [u32; 64] = [
    0xd76aa478, 0xe8c7b756, 0x242070db, 0xc1bdceee, 0xf57c0faf, 0x4787c62a, 0xa8304613, 0xfd469501,
    0x698098d8, 0x8b44f7af, 0xffff5bb1, 0x895cd7be, 0x6b901122, 0xfd987193, 0xa679438e, 0x49b40821,
    0xf61e2562, 0xc040b340, 0x265e5a51, 0xe9b6c7aa, 0xd62f105d, 0x02441453, 0xd8a1e681, 0xe7d3fbc8,
    0x21e1cde6, 0xc33707d6, 0xf4d50d87, 0x455a14ed, 0xa9e3e905, 0xfcefa3f8, 0x676f02d9, 0x8d2a4c8a,
    0xfffa3942, 0x8771f681, 0x6d9d6122, 0xfde5380c, 0xa4beea44, 0x4bdecfa9, 0xf6bb4b60, 0xbebfbc70,
    0x289b7ec6, 0xeaa127fa, 0xd4ef3085, 0x04881d05, 0xd9d4d039, 0xe6db99e5, 0x1fa27cf8, 0xc4ac5665,
    0xf4292244, 0x432aff97, 0xab9423a7, 0xfc93a039, 0x655b59c3, 0x8f0ccc92, 0xffeff47d, 0x85845dd1,
    0x6fa87e4f, 0xfe2ce6e0, 0xa3014314, 0x4e0811a1, 0xf7537e82, 0xbd3af235, 0x2ad7d2bb, 0xeb86d391,
];

/// Computes the raw 16-byte MD5 digest of `input`.
pub fn md5(input: &[u8]) -> [u8; 16] {
    let mut a0: u32 = 0x6745_2301;
    let mut b0: u32 = 0xefcd_ab89;
    let mut c0: u32 = 0x98ba_dcfe;
    let mut d0: u32 = 0x1032_5476;

    // Message padding: 0x80, zero fill to 56 mod 64, then 64-bit little-endian bit length.
    let mut msg = input.to_vec();
    let bit_len = (input.len() as u64).wrapping_mul(8);
    msg.push(0x80);
    while msg.len() % 64 != 56 {
        msg.push(0);
    }
    msg.extend_from_slice(&bit_len.to_le_bytes());

    for chunk in msg.chunks(64) {
        let mut m = [0u32; 16];
        for (i, slot) in m.iter_mut().enumerate() {
            *slot = u32::from_le_bytes([
                chunk[i * 4],
                chunk[i * 4 + 1],
                chunk[i * 4 + 2],
                chunk[i * 4 + 3],
            ]);
        }

        let (mut a, mut b, mut c, mut d) = (a0, b0, c0, d0);
        for i in 0..64 {
            let (f, g) = if i < 16 {
                ((b & c) | (!b & d), i)
            } else if i < 32 {
                ((d & b) | (!d & c), (5 * i + 1) % 16)
            } else if i < 48 {
                (b ^ c ^ d, (3 * i + 5) % 16)
            } else {
                (c ^ (b | !d), (7 * i) % 16)
            };

            let tmp = d;
            d = c;
            c = b;
            let sum = a.wrapping_add(f).wrapping_add(K[i]).wrapping_add(m[g]);
            b = b.wrapping_add(sum.rotate_left(S[i]));
            a = tmp;
        }

        a0 = a0.wrapping_add(a);
        b0 = b0.wrapping_add(b);
        c0 = c0.wrapping_add(c);
        d0 = d0.wrapping_add(d);
    }

    let mut out = [0u8; 16];
    out[0..4].copy_from_slice(&a0.to_le_bytes());
    out[4..8].copy_from_slice(&b0.to_le_bytes());
    out[8..12].copy_from_slice(&c0.to_le_bytes());
    out[12..16].copy_from_slice(&d0.to_le_bytes());
    out
}

/// Lowercase hex encoding of a byte slice.
pub(crate) fn hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push(DIGITS[(b >> 4) as usize] as char);
        s.push(DIGITS[(b & 0x0f) as usize] as char);
    }
    s
}

/// Computes the lowercase hex MD5 digest of `input`.
pub fn md5_hex(input: &[u8]) -> String {
    hex(&md5(input))
}

/// Computes the lowercase hex SHA-256 digest of `input`.
///
/// Ports the SHA-256 half of Java `ReaderUtils.generateDocId`: `sha2` is already a dependency of
/// this crate (the Bailian request signer uses it), so no crate was added for this.
pub fn sha256_hex(input: &[u8]) -> String {
    use sha2::{Digest, Sha256};

    hex(&Sha256::digest(input))
}

/// Builds a version-3 (MD5, name-based) UUID from `name`.
///
/// Mirrors `java.util.UUID.nameUUIDFromBytes`, including the RFC 4122 variant/version
/// bit fixes and the canonical `8-4-4-4-12` lowercase hex rendering.
pub fn uuid_v3_from_name(name: &[u8]) -> String {
    let mut b = md5(name);
    // Version 3: high nibble of byte 6.
    b[6] = (b[6] & 0x0f) | 0x30;
    // RFC 4122 variant: top two bits of byte 8.
    b[8] = (b[8] & 0x3f) | 0x80;

    let h = hex(&b);
    format!(
        "{}-{}-{}-{}-{}",
        &h[0..8],
        &h[8..12],
        &h[12..16],
        &h[16..20],
        &h[20..32]
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn md5_matches_rfc1321_test_vectors() {
        assert_eq!(md5_hex(b""), "d41d8cd98f00b204e9800998ecf8427e");
        assert_eq!(md5_hex(b"a"), "0cc175b9c0f1b6a831c399e269772661");
        assert_eq!(md5_hex(b"abc"), "900150983cd24fb0d6963f7d28e17f72");
        assert_eq!(
            md5_hex(b"message digest"),
            "f96b697d7cb7938d525a2f31aaf161d0"
        );
        assert_eq!(
            md5_hex(b"abcdefghijklmnopqrstuvwxyz"),
            "c3fcd3d76192e4007dfb496cca67e13b"
        );
        assert_eq!(
            md5_hex(
                b"12345678901234567890123456789012345678901234567890123456789012345678901234567890"
            ),
            "57edf4a22be3c955ac49da2e2107b67a"
        );
    }

    #[test]
    fn sha256_matches_fips_vectors() {
        assert_eq!(
            sha256_hex(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_eq!(
            sha256_hex(b"test.jpg"),
            "0d407ee6406a1216f2366674a1a9ff71361d5bef47021f8eb8b51f95e319dd56"
        );
    }

    #[test]
    fn uuid_v3_has_version_and_variant_bits() {
        // MD5("hello") = 5d41402abc4b2a76b9719d911017c592, version nibble -> 3a,
        // variant byte b9 (top bits 10).
        let uuid = uuid_v3_from_name(b"hello");
        assert_eq!(uuid, "5d41402a-bc4b-3a76-b971-9d911017c592");
    }

    #[test]
    fn uuid_v3_is_deterministic() {
        let a = uuid_v3_from_name(b"{\"doc_id\":\"d\"}");
        let b = uuid_v3_from_name(b"{\"doc_id\":\"d\"}");
        assert_eq!(a, b);
        assert_eq!(a.len(), 36);
        assert_eq!(&a[14..15], "3");
        assert!(matches!(&a[19..20], "8" | "9" | "a" | "b"));
    }
}
