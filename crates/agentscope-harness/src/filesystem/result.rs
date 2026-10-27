//! Rich file-operation result types, mirroring
//! `io.agentscope.harness.agent.filesystem.*Result`.
//!
//! These supplement the raw `Filesystem` return values (`FileInfo`, `FileData`,
//! `GrepMatch`) with structured metadata used by higher-level tools and by tests.

use super::{FileInfo, GrepMatch};

/// Result of an `edit` operation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EditResult {
    /// Number of replacements actually performed.
    pub replacements: usize,
    /// Whether the file content changed.
    pub file_modified: bool,
    /// Number of times `old_string` was found (informational).
    pub occurrences_found: usize,
}

impl EditResult {
    pub fn new(replacements: usize, file_modified: bool, occurrences_found: usize) -> Self {
        Self {
            replacements,
            file_modified,
            occurrences_found,
        }
    }
}

/// Result of a `write` operation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WriteResult {
    /// Bytes written to the file.
    pub bytes_written: usize,
    /// Whether the file was newly created (vs overwritten).
    pub created: bool,
}

impl WriteResult {
    pub fn new(bytes_written: usize, created: bool) -> Self {
        Self {
            bytes_written,
            created,
        }
    }
}

/// Result of a `read` operation, with pagination metadata.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReadResult {
    pub path: String,
    pub content: String,
    pub encoding: String,
    /// 1-indexed start line of the returned slice.
    pub start_line: usize,
    /// 1-indexed end line of the returned slice.
    pub end_line: usize,
    /// Total lines available in the file.
    pub total_lines: usize,
    pub truncated: bool,
}

/// Result of an `ls` operation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LsResult {
    pub path: String,
    pub entries: Vec<FileInfo>,
}

/// Result of a `glob` operation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GlobResult {
    pub pattern: String,
    pub root: String,
    pub matches: Vec<FileInfo>,
}

/// Result of a `grep` operation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GrepResult {
    pub pattern: String,
    pub matches: Vec<GrepMatch>,
    pub files_scanned: usize,
    pub match_count: usize,
}

/// Result of a file download (transfer from remote store to local).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileDownloadResponse {
    pub path: String,
    pub bytes: usize,
    pub encoding: String,
}

/// Result of a file upload (transfer from local to remote store).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileUploadResponse {
    pub path: String,
    pub bytes: usize,
}
