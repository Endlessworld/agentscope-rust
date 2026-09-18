//! AES-256-CBC decryption helpers shared by the Feishu and WeCom callback crypto modules.
//!
//! Both Java implementations use `Cipher.getInstance("AES/CBC/NoPadding")` and then apply
//! their *own* PKCS#7 unpadding helper rather than letting the JCE provider unpad. That matters:
//! the Java helper is deliberately tolerant (a padding byte outside `1..=32` or larger than the
//! buffer leaves the plaintext **unchanged** instead of raising), so the Rust port must use
//! `NoPadding` + the same hand-rolled unpad to stay byte-for-byte compatible.

use aes::cipher::{BlockDecryptMut, KeyIvInit};
use aes::Aes256;

use super::error::{ChannelError, Result};

/// AES-256-CBC decryptor operating on raw (still-padded) blocks.
type Aes256CbcDec = cbc::Decryptor<Aes256>;

/// AES block size in bytes.
pub const AES_BLOCK_SIZE: usize = 16;
/// WeCom / Feishu AES key length in bytes (AES-256).
pub const AES_KEY_SIZE: usize = 32;

/// Tolerant PKCS#7 unpadding, byte-for-byte equivalent to the Java `pkcs7Unpad(byte[])` helper
/// shared by `FeishuCrypto` and `WeComCrypto`.
///
/// Returns the input *unchanged* when:
/// - the input is empty, or
/// - the trailing byte is outside `1..=32`, or
/// - the trailing byte is larger than the input length.
///
/// This deliberately reproduces the Java quirk (the bound is 32, not the AES block size 16) so
/// that malformed ciphertext produces exactly the same downstream error as the Java code, rather
/// than a different one.
pub fn pkcs7_unpad(input: &[u8]) -> Vec<u8> {
    if input.is_empty() {
        return Vec::new();
    }
    let pad = input[input.len() - 1] as usize;
    if pad < 1 || pad > 32 || pad > input.len() {
        return input.to_vec();
    }
    input[..input.len() - pad].to_vec()
}

/// `AES-256-CBC` decrypt with **no** provider-side unpadding.
///
/// Mirrors `Cipher.getInstance("AES/CBC/NoPadding")` + `cipher.init(DECRYPT_MODE, key, iv)`:
/// a ciphertext whose length is not a multiple of the AES block size is rejected (the JCE throws
/// `IllegalBlockSizeException`, surfaced here as [`ChannelError::Crypto`]).
pub fn aes_256_cbc_decrypt(key: &[u8], iv: &[u8], ciphertext: &[u8]) -> Result<Vec<u8>> {
    if key.len() != AES_KEY_SIZE {
        return Err(ChannelError::Crypto(format!(
            "AES key must be {AES_KEY_SIZE} bytes (got {})",
            key.len()
        )));
    }
    if iv.len() != AES_BLOCK_SIZE {
        return Err(ChannelError::Crypto(format!(
            "AES IV must be {AES_BLOCK_SIZE} bytes (got {})",
            iv.len()
        )));
    }
    if ciphertext.is_empty() || ciphertext.len() % AES_BLOCK_SIZE != 0 {
        return Err(ChannelError::Crypto(
            "ciphertext length must be a non-zero multiple of the AES block size".to_string(),
        ));
    }
    let mut buffer = ciphertext.to_vec();
    let decryptor = Aes256CbcDec::new_from_slices(key, iv)
        .map_err(|e| ChannelError::Crypto(format!("AES init failed: {e}")))?;
    decryptor
        .decrypt_padded_mut::<aes::cipher::block_padding::NoPadding>(&mut buffer)
        .map_err(|e| ChannelError::Crypto(format!("AES decrypt failed: {e}")))?;
    Ok(buffer)
}

/// Lower-case hex encoding of a digest, identical to `String.format("%02x")` loops in Java.
pub fn hex_encode(bytes: &[u8]) -> String {
    super::text::to_hex(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::common::to_hex;

    #[test]
    fn pkcs7_unpad_strips_valid_padding() {
        // 13 plaintext bytes + 3 padding bytes of value 0x03.
        assert_eq!(pkcs7_unpad(&[1, 2, 3, 0x03, 0x03, 0x03]), vec![1, 2, 3]);
        // Full-block padding of 16.
        let mut data = vec![0x11u8; 16];
        data.extend_from_slice(&[16u8; 16]);
        assert_eq!(pkcs7_unpad(&data), vec![0x11u8; 16]);
    }

    #[test]
    fn pkcs7_unpad_is_tolerant_like_java() {
        // Empty input passes through.
        assert!(pkcs7_unpad(&[]).is_empty());
        // Padding byte 0 is invalid -> unchanged (Java: pad < 1).
        assert_eq!(pkcs7_unpad(&[1, 2, 0x00]), vec![1, 2, 0x00]);
        // Padding byte larger than the buffer -> unchanged (Java: pad > in.length).
        assert_eq!(pkcs7_unpad(&[1, 2, 9]), vec![1, 2, 9]);
        // Padding byte 32 is accepted by the Java bound (<= 32) when the buffer is long enough.
        let mut data = vec![7u8; 40];
        for _ in 0..32 {
            data.push(32u8);
        }
        assert_eq!(pkcs7_unpad(&data), vec![7u8; 40]);
        // 33 must be rejected -> unchanged.
        let mut data = vec![7u8; 40];
        for _ in 0..33 {
            data.push(33u8);
        }
        assert_eq!(pkcs7_unpad(&data).len(), 73);
    }

    #[test]
    fn aes_decrypt_rejects_bad_key_iv_and_length() {
        assert!(aes_256_cbc_decrypt(&[0u8; 16], &[0u8; 16], &[0u8; 16]).is_err());
        assert!(aes_256_cbc_decrypt(&[0u8; 32], &[0u8; 15], &[0u8; 16]).is_err());
        assert!(aes_256_cbc_decrypt(&[0u8; 32], &[0u8; 16], &[]).is_err());
        assert!(aes_256_cbc_decrypt(&[0u8; 32], &[0u8; 16], &[0u8; 17]).is_err());
    }

    #[test]
    fn hex_encode_matches_lowercase_padded_format() {
        assert_eq!(hex_encode(&[0x00, 0x0f, 0xff]), "000fff");
        assert_eq!(hex_encode(&[]), to_hex(&[]));
    }
}
