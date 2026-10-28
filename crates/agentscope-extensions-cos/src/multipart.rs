//! Multipart (分块上传) planning helpers for large COS objects.
//!
//! The Java module only calls `COSClient.putObject`, which COS caps at 5 GiB. The
//! SDK's `TransferManager` is the component that adds multipart upload, and the
//! rules below are lifted from it (`com.qcloud:cos_api:5.6.245`):
//!
//! | Rule | SDK source |
//! |---|---|
//! | switch to multipart when `len >= threshold` | `TransferManagerUtils.shouldUseMultipartUpload` (`lcmp` against `multipartUploadThreshold`) |
//! | part size = `max(ceil(len / 10000), minimumUploadPartSize)` | `TransferManagerUtils.calculateOptimalPartSize` |
//! | defaults `threshold = minimumUploadPartSize = 5242880` (5 MiB) | `TransferManagerConfiguration.<init>` |
//! | one worker thread by default | `TransferManagerUtils.createDefaultExecutorService` → `newFixedThreadPool(1, ...)` |
//!
//! Service-side limits (COS API docs, *Upload Part*): each part is 1 MiB – 5 GiB
//! (the last part may be smaller), and an upload has at most 10000 parts.
//!
//! Everything in this module is pure so the arithmetic and the generated XML can be
//! asserted without a network.

use crate::http_object_store::escape_xml;
use crate::{CosError, Result};

/// Object size at which [`crate::CosHttpObjectStore::put`] switches to multipart.
///
/// Java: `TransferManagerConfiguration.DEFAULT_MULTIPART_UPLOAD_THRESHOLD` =
/// `5242880` (5 MiB).
pub const DEFAULT_MULTIPART_THRESHOLD: u64 = 5 * 1024 * 1024;

/// Part size used when the configured size still covers the object.
///
/// Java: `TransferManagerConfiguration.DEFAULT_MINIMUM_UPLOAD_PART_SIZE` =
/// `5242880` (5 MiB).
pub const DEFAULT_MULTIPART_PART_SIZE: u64 = 5 * 1024 * 1024;

/// Smallest legal part size (COS: 1 MiB; only the final part may be smaller).
pub const MIN_MULTIPART_PART_SIZE: u64 = 1024 * 1024;

/// Largest legal part size (COS: 5 GiB).
pub const MAX_MULTIPART_PART_SIZE: u64 = 5 * 1024 * 1024 * 1024;

/// COS rejects an upload with more than 10000 parts.
pub const MAX_MULTIPART_PARTS: u64 = 10_000;

/// Number of parts uploaded at the same time.
///
/// Java: `TransferManagerUtils.createDefaultExecutorService()` builds a
/// `newFixedThreadPool(1, ...)`, i.e. the SDK uploads parts with a single worker
/// unless the caller supplies its own executor. Raise it through
/// `CosHttpObjectStoreBuilder::multipart_concurrency`.
pub const DEFAULT_MULTIPART_CONCURRENCY: usize = 1;

/// How a body is split into parts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MultipartPlan {
    /// Bytes per part; every part except the last is exactly this large.
    pub part_size: usize,
    /// Total number of parts (`1..=10000`).
    pub part_count: usize,
    /// Length of the body being split.
    pub total_len: usize,
}

impl MultipartPlan {
    /// Byte range `[start, end)` of the 1-based `part_number`.
    pub fn range(&self, part_number: u32) -> Option<(usize, usize)> {
        if part_number == 0 || part_number as usize > self.part_count {
            return None;
        }
        let start = (part_number as usize - 1) * self.part_size;
        Some((start, (start + self.part_size).min(self.total_len)))
    }
}

/// Splits `total_len` into parts, applying the COS/SDK rules.
///
/// The configured part size is the *minimum*: it grows when a 10000-part upload
/// would otherwise be exceeded (`ceil(total_len / 10000)`, exactly like Java
/// `calculateOptimalPartSize`).
pub fn plan_parts(total_len: usize, configured_part_size: usize) -> Result<MultipartPlan> {
    if total_len == 0 {
        return Err(CosError::Config(
            "multipart upload requires a non-empty body".to_string(),
        ));
    }

    let configured = configured_part_size as u64;
    if !(MIN_MULTIPART_PART_SIZE..=MAX_MULTIPART_PART_SIZE).contains(&configured) {
        return Err(CosError::Config(format!(
            "multipart part size must be between {MIN_MULTIPART_PART_SIZE} and \
             {MAX_MULTIPART_PART_SIZE} bytes (COS limit), got {configured}"
        )));
    }

    let total = total_len as u64;
    let capacity = MAX_MULTIPART_PART_SIZE * MAX_MULTIPART_PARTS;
    if total > capacity {
        return Err(CosError::Config(format!(
            "object of {total} bytes exceeds the multipart capacity of {capacity} bytes \
             ({MAX_MULTIPART_PARTS} parts x {MAX_MULTIPART_PART_SIZE} bytes)"
        )));
    }

    let part_size = configured.max(total.div_ceil(MAX_MULTIPART_PARTS));
    let part_count = total.div_ceil(part_size);

    Ok(MultipartPlan {
        part_size: part_size as usize,
        part_count: part_count as usize,
        total_len,
    })
}

/// Strips the `W/` prefix and the surrounding quotes from an `ETag` header.
///
/// COS returns `ETag: "a1f8e5e4..."`; the complete request wants the bare digest
/// (it is re-quoted when rendered).
pub fn normalize_etag(raw: &str) -> String {
    let trimmed = raw.trim();
    let trimmed = trimmed.strip_prefix("W/").unwrap_or(trimmed).trim();
    trimmed
        .strip_prefix('"')
        .and_then(|inner| inner.strip_suffix('"'))
        .unwrap_or(trimmed)
        .trim()
        .to_string()
}

/// Builds the `CompleteMultipartUpload` body from `(part_number, etag)` pairs.
///
/// Callers must pass the parts ordered by `part_number`: COS answers
/// `400 InvalidPartOrder` otherwise.
pub fn complete_multipart_body(parts: &[(u32, String)]) -> String {
    let mut body = String::from("<CompleteMultipartUpload>");
    for (part_number, etag) in parts {
        body.push_str("<Part><PartNumber>");
        body.push_str(&part_number.to_string());
        body.push_str("</PartNumber><ETag>\"");
        body.push_str(&escape_xml(etag));
        body.push_str("\"</ETag></Part>");
    }
    body.push_str("</CompleteMultipartUpload>");
    body
}

#[cfg(test)]
mod tests {
    use super::*;

    const MIB: u64 = 1024 * 1024;

    /// Defaults mirror `TransferManagerConfiguration` (5 MiB threshold, 5 MiB part)
    /// and the SDK's single-worker executor.
    #[test]
    fn defaults_match_the_java_transfer_manager() {
        assert_eq!(DEFAULT_MULTIPART_THRESHOLD, 5 * MIB);
        assert_eq!(DEFAULT_MULTIPART_PART_SIZE, 5 * MIB);
        assert_eq!(DEFAULT_MULTIPART_CONCURRENCY, 1);
        assert_eq!(MIN_MULTIPART_PART_SIZE, MIB);
        assert_eq!(MAX_MULTIPART_PART_SIZE, 5 * 1024 * MIB);
        assert_eq!(MAX_MULTIPART_PARTS, 10_000);
    }

    #[test]
    fn configured_part_size_is_used_when_it_covers_the_object() {
        let plan = plan_parts(12 * MIB as usize, 5 * MIB as usize).unwrap();
        assert_eq!(plan.part_size, 5 * MIB as usize);
        assert_eq!(plan.part_count, 3);
        assert_eq!(plan.range(1), Some((0, 5 * MIB as usize)));
        assert_eq!(plan.range(2), Some((5 * MIB as usize, 10 * MIB as usize)));
        // The last part is allowed to be smaller than the minimum part size.
        assert_eq!(plan.range(3), Some((10 * MIB as usize, 12 * MIB as usize)));
        assert_eq!(plan.range(0), None);
        assert_eq!(plan.range(4), None);
    }

    /// `ceil(total / 10000)` from Java `calculateOptimalPartSize`: a 100 GiB body
    /// needs a 10 MiB part so it stays within the 10000-part cap.
    #[test]
    fn part_size_grows_to_stay_within_the_part_count_cap() {
        let total = 100_000 * MIB as usize;
        let plan = plan_parts(total, 5 * MIB as usize).unwrap();
        assert_eq!(plan.part_size, 10 * MIB as usize);
        assert_eq!(plan.part_count, 10_000);
        assert!(plan.part_count <= MAX_MULTIPART_PARTS as usize);

        // Just over the cap with the configured size: the part size grows to
        // `ceil(total / 10000)` bytes (Java does not round to MiB boundaries).
        let total = 10_001 * MIB;
        let plan = plan_parts(total as usize, MIB as usize).unwrap();
        assert_eq!(plan.part_size as u64, total.div_ceil(MAX_MULTIPART_PARTS));
        assert_eq!(plan.part_size, 1_048_681);
        assert_eq!(plan.part_count, 10_000);
    }

    #[test]
    fn plan_rejects_illegal_part_sizes_and_bodies() {
        let err = plan_parts(MIB as usize, (MIB / 2) as usize).unwrap_err();
        assert!(err.to_string().contains("part size must be between"));

        let err = plan_parts(MIB as usize, (6 * 1024 * MIB) as usize).unwrap_err();
        assert!(err.to_string().contains("part size must be between"));

        let err = plan_parts(0, MIB as usize).unwrap_err();
        assert!(err.to_string().contains("non-empty body"));

        let too_big = (MAX_MULTIPART_PART_SIZE * MAX_MULTIPART_PARTS) as usize + 1;
        let err = plan_parts(too_big, MIB as usize).unwrap_err();
        assert!(err.to_string().contains("multipart capacity"));
    }

    /// Sizes just at the boundaries are accepted.
    #[test]
    fn plan_accepts_boundary_sizes() {
        assert!(plan_parts(1, MIN_MULTIPART_PART_SIZE as usize).is_ok());
        assert!(plan_parts(1, MAX_MULTIPART_PART_SIZE as usize).is_ok());
        assert!(plan_parts(
            (MAX_MULTIPART_PART_SIZE * MAX_MULTIPART_PARTS) as usize,
            MAX_MULTIPART_PART_SIZE as usize
        )
        .is_ok());
    }

    #[test]
    fn etags_are_unquoted() {
        assert_eq!(normalize_etag("\"a1f8e5e4\""), "a1f8e5e4");
        assert_eq!(normalize_etag("a1f8e5e4"), "a1f8e5e4");
        assert_eq!(normalize_etag("  \"abc\"  "), "abc");
        assert_eq!(normalize_etag("W/\"abc\""), "abc");
        // A single stray quote is left alone rather than truncated.
        assert_eq!(normalize_etag("\"abc"), "\"abc");
    }

    #[test]
    fn complete_body_lists_parts_in_order() {
        let parts = vec![(1, "etag-1".to_string()), (2, "etag-2".to_string())];
        assert_eq!(
            complete_multipart_body(&parts),
            "<CompleteMultipartUpload>\
             <Part><PartNumber>1</PartNumber><ETag>\"etag-1\"</ETag></Part>\
             <Part><PartNumber>2</PartNumber><ETag>\"etag-2\"</ETag></Part>\
             </CompleteMultipartUpload>"
        );
        assert_eq!(
            complete_multipart_body(&[]),
            "<CompleteMultipartUpload></CompleteMultipartUpload>"
        );
    }

    /// ETags are opaque strings, so they must be XML-escaped.
    #[test]
    fn complete_body_escapes_etags() {
        let body = complete_multipart_body(&[(1, "a&b<c".to_string())]);
        assert_eq!(
            body,
            "<CompleteMultipartUpload>\
             <Part><PartNumber>1</PartNumber><ETag>\"a&amp;b&lt;c\"</ETag></Part>\
             </CompleteMultipartUpload>"
        );
    }
}
