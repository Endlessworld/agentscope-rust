//! Reader id helpers.
//!
//! Ports `io.agentscope.core.rag.reader.ReaderUtils`: deterministic document ids derived from a
//! hash of the input (typically a file path) plus a lowercase hex encoder. The MD5 half is still
//! used by the ported [`crate::reader::ImageReader`]; the SHA-256 half remains available for
//! callers that plug their own readers into the [`crate::reader::Reader`] trait.

use crate::error::{RagError, Result};
use crate::hash::{hex, md5_hex, sha256_hex};

/// Hash algorithms accepted by [`generate_doc_id`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DocIdAlgorithm {
    /// MD5 (`ReaderUtils.generateDocIdMD5`).
    Md5,
    /// SHA-256 (`ReaderUtils.generateDocIdSHA256`).
    Sha256,
}

impl DocIdAlgorithm {
    /// Java `"MD5"`.
    pub const MD5: &'static str = "MD5";
    /// Java `"SHA-256"`.
    pub const SHA256: &'static str = "SHA-256";

    /// Parses a Java algorithm name (case-insensitive; `SHA256` is accepted as an alias).
    pub fn from_name(name: &str) -> Option<Self> {
        match name.trim().to_ascii_uppercase().as_str() {
            "MD5" => Some(DocIdAlgorithm::Md5),
            "SHA-256" | "SHA256" => Some(DocIdAlgorithm::Sha256),
            _ => None,
        }
    }

    /// The canonical Java name.
    pub fn name(&self) -> &'static str {
        match self {
            DocIdAlgorithm::Md5 => Self::MD5,
            DocIdAlgorithm::Sha256 => Self::SHA256,
        }
    }
}

/// Generates a deterministic document id with the named hash algorithm.
///
/// Java threw a `RuntimeException` (`"<algorithm> algorithm not available"`) for an unknown
/// algorithm; here it is [`RagError::InvalidArgument`] with the same text.
pub fn generate_doc_id(input: &str, algorithm: &str) -> Result<String> {
    match DocIdAlgorithm::from_name(algorithm) {
        Some(DocIdAlgorithm::Md5) => Ok(generate_doc_id_md5(input)),
        Some(DocIdAlgorithm::Sha256) => Ok(generate_doc_id_sha256(input)),
        None => Err(RagError::InvalidArgument(format!(
            "{algorithm} algorithm not available"
        ))),
    }
}

/// Generates a deterministic document id with MD5 (`ReaderUtils.generateDocIdMD5`).
pub fn generate_doc_id_md5(input: &str) -> String {
    md5_hex(input.as_bytes())
}

/// Generates a deterministic document id with SHA-256 (`ReaderUtils.generateDocIdSHA256`).
pub fn generate_doc_id_sha256(input: &str) -> String {
    sha256_hex(input.as_bytes())
}

/// Lowercase hex encoding of a byte slice (`ReaderUtils.bytesToHex`).
pub fn bytes_to_hex(bytes: &[u8]) -> String {
    hex(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn md5_doc_id_matches_the_java_algorithm() {
        // Vectors computed independently with .NET's MD5 implementation.
        assert_eq!(
            generate_doc_id_md5("test.jpg"),
            "0412c29576c708cf0155e8de242169b1"
        );
        assert_eq!(
            generate_doc_id_md5("https://example.com/image.png"),
            "d67540393d81c9e621cb2deed9d80816"
        );
        assert_eq!(
            generate_doc_id_md5("abc"),
            "900150983cd24fb0d6963f7d28e17f72"
        );
    }

    #[test]
    fn sha256_doc_id_matches_the_java_algorithm() {
        assert_eq!(
            generate_doc_id_sha256("test.jpg"),
            "0d407ee6406a1216f2366674a1a9ff71361d5bef47021f8eb8b51f95e319dd56"
        );
        assert_eq!(
            generate_doc_id_sha256("abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn generate_doc_id_dispatches_by_name() {
        assert_eq!(
            generate_doc_id("abc", "MD5").unwrap(),
            generate_doc_id_md5("abc")
        );
        assert_eq!(
            generate_doc_id("abc", "sha-256").unwrap(),
            generate_doc_id_sha256("abc")
        );
        assert_eq!(
            generate_doc_id("abc", "SHA256").unwrap(),
            generate_doc_id_sha256("abc")
        );
    }

    #[test]
    fn unknown_algorithm_is_rejected_with_the_java_message() {
        let err = generate_doc_id("abc", "SHA-1").unwrap_err();
        assert!(
            err.to_string().contains("SHA-1 algorithm not available"),
            "err={err}"
        );
    }

    #[test]
    fn algorithm_names_round_trip() {
        assert_eq!(DocIdAlgorithm::from_name("MD5"), Some(DocIdAlgorithm::Md5));
        assert_eq!(
            DocIdAlgorithm::from_name("SHA-256"),
            Some(DocIdAlgorithm::Sha256)
        );
        assert_eq!(DocIdAlgorithm::from_name("sha1"), None);
        assert_eq!(DocIdAlgorithm::Md5.name(), "MD5");
        assert_eq!(DocIdAlgorithm::Sha256.name(), "SHA-256");
    }

    #[test]
    fn bytes_to_hex_is_lowercase_and_padded() {
        assert_eq!(bytes_to_hex(&[0x00, 0x0f, 0xff]), "000fff");
        assert_eq!(bytes_to_hex(&[]), "");
    }
}
