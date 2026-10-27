//! MurmurHash3 (x64, 128-bit) — byte-for-byte compatible with Apache Commons Codec.
//!
//! `PostgresSandboxExecutionGuard` derives its advisory-lock key from the isolation key
//! with `org.apache.commons.codec.digest.MurmurHash3.hash128(bytes)` and takes `hash[0]`.
//! Reproducing that here keeps Rust and Java processes that share one PostgreSQL database
//! contending on the *same* advisory-lock key; using `DefaultHasher` (SipHash) would let
//! both sides acquire "the same" lock independently, silently defeating mutual exclusion.
//!
//! Two details are load-bearing and easy to get wrong:
//!
//! * Commons Codec's `hash128` seeds with [`DEFAULT_SEED`] (`104729`), **not** `0`
//!   (`hash128x64(byte[])` is the zero-seeded overload and yields different keys).
//! * Tail bytes are masked with `0xff` before shifting.
//!
//! Golden vectors below were produced by running Commons Codec 1.15 on the JVM, mirroring
//! the "pin the behaviour with real output" approach used for the Bailian ACS4 signing work.

/// Seed used by `MurmurHash3.hash128(byte[])`.
pub const DEFAULT_SEED: u64 = 104_729;

const C1: u64 = 0x87c3_7b91_1142_53d5;
const C2: u64 = 0x4cf5_ad43_2745_937f;

#[inline]
fn fmix64(mut k: u64) -> u64 {
    k ^= k >> 33;
    k = k.wrapping_mul(0xff51_afd7_ed55_8ccd);
    k ^= k >> 33;
    k = k.wrapping_mul(0xc4ce_b9fe_1a85_ec53);
    k ^= k >> 33;
    k
}

#[inline]
fn le_u64(bytes: &[u8], offset: usize) -> u64 {
    let mut buf = [0u8; 8];
    buf.copy_from_slice(&bytes[offset..offset + 8]);
    u64::from_le_bytes(buf)
}

/// Computes the 128-bit MurmurHash3 of `data` with an explicit `seed`.
pub fn hash128_with_seed(data: &[u8], seed: u64) -> (u64, u64) {
    let len = data.len();
    let nblocks = len / 16;

    let mut h1 = seed;
    let mut h2 = seed;

    for i in 0..nblocks {
        let offset = i * 16;
        let mut k1 = le_u64(data, offset);
        let mut k2 = le_u64(data, offset + 8);

        k1 = k1.wrapping_mul(C1);
        k1 = k1.rotate_left(31);
        k1 = k1.wrapping_mul(C2);
        h1 ^= k1;

        h1 = h1.rotate_left(27);
        h1 = h1.wrapping_add(h2);
        h1 = h1.wrapping_mul(5).wrapping_add(0x52dc_e729);

        k2 = k2.wrapping_mul(C2);
        k2 = k2.rotate_left(33);
        k2 = k2.wrapping_mul(C1);
        h2 ^= k2;

        h2 = h2.rotate_left(31);
        h2 = h2.wrapping_add(h1);
        h2 = h2.wrapping_mul(5).wrapping_add(0x3849_5ab5);
    }

    let tail = &data[nblocks * 16..];
    let mut k1: u64 = 0;
    let mut k2: u64 = 0;

    // Identical to the JVM switch's fall-through: high bytes first, low byte last, each
    // masked to 0xff.
    if tail.len() >= 15 {
        k2 ^= (tail[14] as u64) << 48;
    }
    if tail.len() >= 14 {
        k2 ^= (tail[13] as u64) << 40;
    }
    if tail.len() >= 13 {
        k2 ^= (tail[12] as u64) << 32;
    }
    if tail.len() >= 12 {
        k2 ^= (tail[11] as u64) << 24;
    }
    if tail.len() >= 11 {
        k2 ^= (tail[10] as u64) << 16;
    }
    if tail.len() >= 10 {
        k2 ^= (tail[9] as u64) << 8;
    }
    if tail.len() >= 9 {
        k2 ^= tail[8] as u64;
        k2 = k2.wrapping_mul(C2);
        k2 = k2.rotate_left(33);
        k2 = k2.wrapping_mul(C1);
        h2 ^= k2;
    }

    if tail.len() >= 8 {
        k1 ^= (tail[7] as u64) << 56;
    }
    if tail.len() >= 7 {
        k1 ^= (tail[6] as u64) << 48;
    }
    if tail.len() >= 6 {
        k1 ^= (tail[5] as u64) << 40;
    }
    if tail.len() >= 5 {
        k1 ^= (tail[4] as u64) << 32;
    }
    if tail.len() >= 4 {
        k1 ^= (tail[3] as u64) << 24;
    }
    if tail.len() >= 3 {
        k1 ^= (tail[2] as u64) << 16;
    }
    if tail.len() >= 2 {
        k1 ^= (tail[1] as u64) << 8;
    }
    if !tail.is_empty() {
        k1 ^= tail[0] as u64;
        k1 = k1.wrapping_mul(C1);
        k1 = k1.rotate_left(31);
        k1 = k1.wrapping_mul(C2);
        h1 ^= k1;
    }

    let len64 = len as u64;
    h1 ^= len64;
    h2 ^= len64;

    h1 = h1.wrapping_add(h2);
    h2 = h2.wrapping_add(h1);

    h1 = fmix64(h1);
    h2 = fmix64(h2);

    h1 = h1.wrapping_add(h2);
    h2 = h2.wrapping_add(h1);

    (h1, h2)
}

/// `MurmurHash3.hash128(data)[0]` as a signed 64-bit key.
pub fn lock_key(value: &str) -> i64 {
    hash128_with_seed(value.as_bytes(), DEFAULT_SEED).0 as i64
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Golden vectors produced by Apache Commons Codec 1.15
    /// `MurmurHash3.hash128(s.getBytes(UTF_8))[0]` on OpenJDK 21.
    #[test]
    fn matches_commons_codec_hash128_golden_vectors() {
        let cases: &[(&str, i64)] = &[
            ("", -7_122_613_646_888_064_702),
            ("a", 3_702_349_557_448_790_408),
            ("ab", 8_790_855_041_734_910_294),
            ("test:sess1", -3_373_129_922_000_855_771),
            ("agentscope:org:abc", 8_693_554_169_034_462_011),
            (
                "The quick brown fox jumps over the lazy dog",
                -1_779_949_751_512_360_504,
            ),
            ("prefix:SESSION:value-123", 2_321_378_978_135_067_515),
        ];
        for (input, expected) in cases {
            assert_eq!(
                lock_key(input),
                *expected,
                "mismatch for {input:?}: got {}",
                lock_key(input)
            );
        }
    }

    /// Cross-check against `MurmurHash3.hash128x64(byte[])`, which is the zero-seeded
    /// overload — proves the body/tail/finalisation apart from the seed choice.
    #[test]
    fn zero_seed_matches_commons_codec_x64_overload() {
        let cases: &[(&str, i64)] = &[
            ("", 0),
            ("a", -8_839_064_797_231_613_815),
            ("ab", -7_815_133_031_266_706_642),
            ("test:sess1", -5_884_920_801_670_859_752),
        ];
        for (input, expected) in cases {
            assert_eq!(
                hash128_with_seed(input.as_bytes(), 0).0 as i64,
                *expected,
                "zero-seed mismatch for {input:?}"
            );
        }
    }

    #[test]
    fn empty_input_is_not_zero() {
        assert_ne!(lock_key(""), 0);
    }

    /// Byte-length sweep to localise any mismatch (tail vs body vs finalisation).
    #[test]
    fn length_sweep_golden_vectors() {
        let cases: &[(&str, i64)] = &[
            ("12345678", -2_455_429_847_547_687_578),
            ("123456789", 6_502_726_292_040_185_600),
            ("123456789012", -6_350_179_390_026_660_262),
            ("123456789012345", -5_907_740_662_398_844_928),
            ("1234567890123456", 9_145_637_354_579_578_008),
            (
                "12345678901234567890123456789012",
                8_334_282_961_331_573_876,
            ),
        ];
        for (input, expected) in cases {
            eprintln!(
                "len={} got={} expected={}",
                input.len(),
                lock_key(input),
                expected
            );
            assert_eq!(
                lock_key(input),
                *expected,
                "mismatch for len={}",
                input.len()
            );
        }
    }

    /// Multi-block input exercises the 16-byte body loop.
    #[test]
    fn multi_block_input_is_stable() {
        let long = "agentscope:session:0f1e2d3c-4b5a-6978-8796-a5b4c3d2e1f0";
        assert!(long.len() > 32);
        assert_eq!(lock_key(long), lock_key(long));
        assert_ne!(lock_key(long), lock_key("agentscope:session:other"));
    }
}
