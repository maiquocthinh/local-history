//! Kiểu dữ liệu dùng chung của engine local-history.
//! Đây là hợp đồng: mọi module (record, reconstruct, retention, daemon, cli, gui) đều dùng các
//! kiểu này — không tự định nghĩa lại.

use serde::{Deserialize, Serialize};

pub type Result<T> = std::result::Result<T, Error>;

/// Kích thước file tối đa được record — lớn hơn thì bỏ qua (tránh phình storage).
pub const MAX_FILE_SIZE: u64 = 10 * 1024 * 1024;

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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EventKind {
    /// Editor chủ động save (phase 2 hook trong Wu)
    Save,
    /// Thay đổi ngoài editor — watcher daemon ghi
    External,
    /// File bị xóa (marker; nội dung cũ vẫn còn trong chain)
    Delete,
    /// File được khôi phục nội dung (sau revert/restore)
    Restore,
    /// Snapshot an toàn TRƯỚC khi revert ghi đè
    PreRevert,
    /// Đổi tên (phase 1: ghi 2 event delete+create riêng)
    Rename,
    /// Label ngữ nghĩa ("trước khi chạy test", "trước khi commit"...)
    Label,
}

impl EventKind {
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Storage {
    Full,
    Delta,
}

impl Storage {
    pub fn as_str(self) -> &'static str {
        match self {
            Storage::Full => "full",
            Storage::Delta => "delta",
        }
    }

    pub fn parse(s: &str) -> Result<Self> {
        match s {
            "full" => Ok(Storage::Full),
            "delta" => Ok(Storage::Delta),
            other => Err(Error::BadStorage(other.to_string())),
        }
    }
}

pub type WorkspaceId = i64;
pub type FileId = i64;
pub type VersionId = i64;

/// Một version trong timeline.
#[derive(Debug, Clone)]
pub struct Version {
    pub id: VersionId,
    pub file_id: FileId,
    pub seq: i64,
    /// unix micros UTC
    pub timestamp_micros: i64,
    pub kind: EventKind,
    pub label: Option<String>,
    pub storage: Storage,
    /// Với delta: seq của version ngay trước nó trong chain. Full: None.
    pub base_seq: Option<i64>,
    /// Kích thước nội dung sau khi tái tạo (bytes)
    pub size: u64,
    /// blake3 hex
    pub content_hash: String,
}

/// Tóm tắt một file có lịch sử (cho list file trong CLI/GUI).
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

/// Một hunk của structured patch — định dạng delta lưu trên disk (JSON + zstd).
///
/// Ngữ nghĩa splice (bắt buộc dùng đúng cho apply):
/// dòng được chia sau mỗi `\n`, terminator giữ nguyên trong dòng; dòng cuối có thể không có
/// terminator. Với danh sách dòng `lines` (0-based) của base:
///   kết quả = lines[0..old_start-1] ++ split_lines(new_text) ++ lines[old_start-1+old_lines..]
/// Khi old_lines == 0 (insert thuần), old_start-1 = số dòng trước điểm chèn
/// (old_start = 1 → chèn đầu file; old_start = len+1 → chèn cuối file).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Hunk {
    pub old_start: u64,
    pub old_lines: u64,
    pub new_start: u64,
    pub new_lines: u64,
    /// Nội dung dòng mới nguyên bản (gồm terminator), nối liền nhau
    pub new_text: String,
}

/// Chính sách GC. Mặc định kiểu JetBrains: ~5 ngày làm việc, tối thiểu 50 version/file.
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

/// Kết quả một lần chạy GC.
#[derive(Debug, Clone, Default)]
pub struct GcReport {
    pub files_scanned: u64,
    pub versions_deleted: u64,
    pub bytes_freed: u64,
    pub orphan_blobs: u64,
    pub dry_run: bool,
}

/// Dữ liệu đầu vào cho GC: mọi version của một file, sắp theo seq tăng dần.
#[derive(Debug, Clone)]
pub struct GcFile {
    pub ws_id: WorkspaceId,
    pub file_id: FileId,
    pub versions: Vec<Version>,
}
