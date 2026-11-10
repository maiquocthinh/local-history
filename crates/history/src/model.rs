//! Shared data types of the local-history engine.
//! This is the contract: every consumer (record, reconstruct, retention, daemon, cli, gui)
//! uses these types instead of defining its own.

use serde::{Deserialize, Serialize};

pub type Result<T> = std::result::Result<T, Error>;

/// Largest file size that gets recorded; larger files are skipped to avoid bloating storage.
pub const MAX_FILE_SIZE: u64 = 10 * 1024 * 1024;

/// Errors returned by the engine.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("sqlite: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("json: {0}")]
    Json(#[from] serde_json::Error),
    #[error("path không hợp lệ: {0}")]
    InvalidPath(String),
    #[error("file nằm ngoài workspace: {0}")]
    OutsideWorkspace(String),
    #[error("không tìm thấy: {0}")]
    NotFound(String),
    #[error("chain hỏng: {0}")]
    CorruptChain(String),
    #[error("nội dung binary không diff được (version {0})")]
    BinaryContent(i64),
    #[error("kind không hợp lệ trong db: {0}")]
    BadKind(String),
    #[error("storage không hợp lệ trong db: {0}")]
    BadStorage(String),
}

/// What triggered a version.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EventKind {
    /// Editor-initiated save (phase 2 hook in Wu).
    Save,
    /// Change made outside the editor — recorded by the watcher daemon.
    External,
    /// File was deleted (marker; the previous content stays in the chain).
    Delete,
    /// Content was restored (after a revert/restore).
    Restore,
    /// Safety snapshot taken before a revert overwrites the current content.
    PreRevert,
    /// Rename (phase 1 records separate delete + create events).
    Rename,
    /// Semantic marker ("before running tests", "before commit", ...).
    Label,
}

impl EventKind {
    /// Canonical string form, as stored in the db and exchanged over IPC.
    pub fn as_str(self) -> &'static str {
        match self {
            EventKind::Save => "save",
            EventKind::External => "external",
            EventKind::Delete => "delete",
            EventKind::Restore => "restore",
            EventKind::PreRevert => "pre_revert",
            EventKind::Rename => "rename",
            EventKind::Label => "label",
        }
    }

    /// Parses the canonical string form; unknown values yield `Error::BadKind`.
    pub fn parse(s: &str) -> Result<Self> {
        match s {
            "save" => Ok(EventKind::Save),
            "external" => Ok(EventKind::External),
            "delete" => Ok(EventKind::Delete),
            "restore" => Ok(EventKind::Restore),
            "pre_revert" => Ok(EventKind::PreRevert),
            "rename" => Ok(EventKind::Rename),
            "label" => Ok(EventKind::Label),
            other => Err(Error::BadKind(other.to_string())),
        }
    }
}

/// How a version's content is stored: a complete snapshot or a delta against the previous
/// version in the chain.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Storage {
    Full,
    Delta,
}

impl Storage {
    /// Canonical string form, as stored in the db.
    pub fn as_str(self) -> &'static str {
        match self {
            Storage::Full => "full",
            Storage::Delta => "delta",
        }
    }

    /// Parses the canonical string form; unknown values yield `Error::BadStorage`.
    pub fn parse(s: &str) -> Result<Self> {
        match s {
            "full" => Ok(Storage::Full),
            "delta" => Ok(Storage::Delta),
            other => Err(Error::BadStorage(other.to_string())),
        }
    }
}

/// Primary key of a `workspaces` row.
pub type WorkspaceId = i64;
/// Primary key of a `files` row.
pub type FileId = i64;
/// Primary key of a `versions` row.
pub type VersionId = i64;

/// One version in a file's timeline.
#[derive(Debug, Clone)]
pub struct Version {
    pub id: VersionId,
    pub file_id: FileId,
    /// Per-file sequence number, 1-based and consecutive within a chain.
    pub seq: i64,
    /// UTC unix timestamp in microseconds.
    pub timestamp_micros: i64,
    pub kind: EventKind,
    pub label: Option<String>,
    pub storage: Storage,
    /// For deltas: seq of the immediate predecessor in the chain. `None` for fulls.
    pub base_seq: Option<i64>,
    /// Size of the reconstructed content, in bytes.
    pub size: u64,
    /// blake3 hash of the reconstructed content, hex-encoded.
    pub content_hash: String,
}

/// Summary of a file that has history, for file listings in the CLI/GUI.
#[derive(Debug, Clone)]
pub struct FileSummary {
    pub file_id: FileId,
    pub rel_path: String,
    pub current_seq: i64,
    pub version_count: i64,
    pub last_timestamp_micros: i64,
    pub last_size: u64,
    pub last_kind: EventKind,
}

/// One hunk of a structured patch — the on-disk delta format (JSON, zstd-compressed at the
/// blob layer).
///
/// Splice semantics (apply depends on this): lines are split after each `\n` and the
/// terminator stays part of the line; the last line may lack a terminator. With `lines`
/// (0-based) the lines of the base content:
///   result = lines[0..old_start-1] ++ split_lines(new_text) ++ lines[old_start-1+old_lines..]
/// When `old_lines == 0` (pure insert), `old_start - 1` is the number of lines before the
/// insertion point (`old_start = 1` inserts at the top; `old_start = len + 1` appends).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Hunk {
    pub old_start: u64,
    pub old_lines: u64,
    pub new_start: u64,
    pub new_lines: u64,
    /// Verbatim new lines including terminators, concatenated.
    pub new_text: String,
}

/// GC policy. JetBrains-style default: about 5 working days, at least 50 versions per file.
#[derive(Debug, Clone, Copy)]
pub struct RetentionPolicy {
    pub working_days: u32,
    pub min_versions: u32,
}

impl Default for RetentionPolicy {
    fn default() -> Self {
        Self { working_days: 5, min_versions: 50 }
    }
}

/// Result of one GC run.
#[derive(Debug, Clone, Default)]
pub struct GcReport {
    pub files_scanned: u64,
    pub versions_deleted: u64,
    pub bytes_freed: u64,
    pub orphan_blobs: u64,
    pub dry_run: bool,
}

/// GC input: all versions of one file, ordered by ascending seq.
#[derive(Debug, Clone)]
pub struct GcFile {
    pub ws_id: WorkspaceId,
    pub file_id: FileId,
    pub versions: Vec<Version>,
}
