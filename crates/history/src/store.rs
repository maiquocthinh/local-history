//! Store — mở index SQLite, migration schema, đăng ký workspace, query dùng chung.
//!
//! Quy ước storage (xem docs/architecture.md):
//! - root/index.db            — SQLite WAL
//! - root/workspaces/<ws_id>/blobs/v<version_id>-full.zst | v<version_id>-delta.zst
//! - ws_id = 8 bytes đầu của blake3(canonical root), format 16 hex khi làm tên thư mục
//!
//! Ghi version (record) BẮT BUỘC chạy trong một transaction: BEGIN → INSERT row (lấy id) →
//! ghi blob (ghi `.tmp` + rename atomic) → COMMIT. Như vậy row đã commit luôn có blob, và crash
//! chỉ để lại blob mồ côi — GC dọn.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use rusqlite::{Connection, OpenFlags};

use crate::model::*;

pub struct LocalHistory {
    pub(crate) conn: Mutex<Connection>,
    pub(crate) root: PathBuf,
}

const MIGRATION_V1: &str = r#"
CREATE TABLE workspaces (
  id         INTEGER PRIMARY KEY,
  root       TEXT NOT NULL UNIQUE,
  created_at INTEGER NOT NULL
);

CREATE TABLE files (
  id           INTEGER PRIMARY KEY AUTOINCREMENT,
  workspace_id INTEGER NOT NULL REFERENCES workspaces(id),
  rel_path     TEXT NOT NULL,
  current_seq  INTEGER NOT NULL DEFAULT 0,
  UNIQUE (workspace_id, rel_path)
);

CREATE TABLE versions (
  id           INTEGER PRIMARY KEY AUTOINCREMENT,
  file_id      INTEGER NOT NULL REFERENCES files(id),
  seq          INTEGER NOT NULL,
  timestamp    INTEGER NOT NULL,
  kind         TEXT NOT NULL,
  label        TEXT,
  storage      TEXT NOT NULL,
  base_seq     INTEGER,
  size         INTEGER NOT NULL,
  content_hash TEXT NOT NULL,
  UNIQUE (file_id, seq)
);

CREATE INDEX idx_files_ws ON files(workspace_id, rel_path);
CREATE INDEX idx_versions_file ON versions(file_id, seq);
"#;

impl LocalHistory {
    /// Mở (hoặc tạo mới) store tại `root`. Tạo thư mục + schema nếu chưa có.
    pub fn open(root: impl AsRef<Path>) -> Result<Self> {
        let root = root.as_ref().to_path_buf();
        std::fs::create_dir_all(root.join("workspaces"))?; // blobs tạo theo ws khi ghi
        let conn = Connection::open(root.join("index.db"))?;
        Self::init_conn(&conn)?;
        Self::migrate(&conn)?;
        Ok(Self { conn: Mutex::new(conn), root })
    }

    /// Mở store tại vị trí mặc định: `%LOCALAPPDATA%\local-history` (Windows),
    /// override bằng env `LOCAL_HISTORY_DIR`.
    pub fn open_default() -> Result<Self> {
        let root = Self::default_root()?;
        Self::open(root)
    }

    /// Mở read-only — dành cho GUI đọc timeline song song với daemon.
    pub fn open_read_only(root: impl AsRef<Path>) -> Result<Self> {
        let root = root.as_ref().to_path_buf();
        let conn = Connection::open_with_flags(
            root.join("index.db"),
            OpenFlags::SQLITE_OPEN_READ_ONLY,
        )?;
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        Ok(Self { conn: Mutex::new(conn), root })
    }

    pub fn default_root() -> Result<PathBuf> {
        if let Ok(dir) = std::env::var("LOCAL_HISTORY_DIR")
            && !dir.is_empty()
        {
            return Ok(PathBuf::from(dir));
        }
        dirs::data_local_dir()
            .map(|d| d.join("local-history"))
            .ok_or_else(|| Error::InvalidPath("không tìm được data_local_dir".into()))
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    fn init_conn(conn: &Connection) -> Result<()> {
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        Ok(())
    }

    fn migrate(conn: &Connection) -> Result<()> {
        let version: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
        if version < 1 {
            conn.execute_batch(MIGRATION_V1)?;
            conn.pragma_update(None, "user_version", 1)?;
        }
        Ok(())
    }

    /// Đăng ký workspace. Id là deterministic theo canonical root — gọi lại với cùng root
    /// trả về cùng id, không tạo row trùng.
    pub fn register_workspace(&self, root: impl AsRef<Path>) -> Result<WorkspaceId> {
        let canonical = normalize_canonical(root.as_ref())?;
        let ws_id = ws_id_from_root(&canonical);
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT OR IGNORE INTO workspaces (id, root, created_at) VALUES (?1, ?2, ?3)",
            rusqlite::params![
                ws_id,
                canonical.to_string_lossy(),
                now_micros(),
            ],
        )?;
        Ok(ws_id)
    }

    /// Đường dẫn canonical của workspace (đã strip `\\?\`).
    pub fn workspace_root(&self, ws: WorkspaceId) -> Result<PathBuf> {
        let conn = self.conn.lock().unwrap();
        let root: String = conn
            .query_row(
                "SELECT root FROM workspaces WHERE id = ?1",
                [ws],
                |r| r.get(0),
            )
            .map_err(|e| match e {
                rusqlite::Error::QueryReturnedNoRows => {
                    Error::NotFound(format!("workspace {ws}"))
                }
                other => other.into(),
            })?;
        Ok(PathBuf::from(root))
    }

    /// Chuẩn hóa đường dẫn file tuyệt đối thành rel_path (phân tách '/').
    /// File tồn tại → canonicalize; không tồn tại (event delete) → canonicalize thư mục cha
    /// rồi ghép tên file.
    pub(crate) fn resolve_rel_path(&self, ws: WorkspaceId, file: &Path) -> Result<String> {
        let ws_root = self.workspace_root(ws)?;
        rel_path_against(&ws_root, file)
    }

    /// Tất cả version của một file, mới nhất trước.
    pub fn history(&self, ws: WorkspaceId, file: &Path) -> Result<Vec<Version>> {
        let rel = self.resolve_rel_path(ws, file)?;
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT v.id, v.file_id, v.seq, v.timestamp, v.kind, v.label, v.storage, v.base_seq, v.size, v.content_hash
             FROM versions v JOIN files f ON f.id = v.file_id
             WHERE f.workspace_id = ?1 AND f.rel_path = ?2
             ORDER BY v.seq DESC",
        )?;
        let rows = stmt.query_map(rusqlite::params![ws, rel], row_to_version)?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    }

    /// Danh sách file có lịch sử trong workspace, sửa gần nhất trước.
    /// `prefix` lọc đầu rel_path (không phân biệt hoa thường trên Windows-nghiêng).
    pub fn list_files(
        &self,
        ws: WorkspaceId,
        prefix: Option<&str>,
    ) -> Result<Vec<FileSummary>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT f.id, f.rel_path, f.current_seq, COUNT(v.id), COALESCE(MAX(v.timestamp), 0),
                    COALESCE((SELECT size FROM versions WHERE file_id = f.id ORDER BY seq DESC LIMIT 1), 0),
                    COALESCE((SELECT kind FROM versions WHERE file_id = f.id ORDER BY seq DESC LIMIT 1), 'external')
             FROM files f LEFT JOIN versions v ON v.file_id = f.id
             WHERE f.workspace_id = ?1
             GROUP BY f.id
             ORDER BY 5 DESC",
        )?;
        let rows = stmt.query_map([ws], |r| {
            let rel_path: String = r.get(1)?;
            let last_kind: String = r.get(6)?;
            let last_kind = EventKind::parse(&last_kind).map_err(|e| {
                rusqlite::Error::FromSqlConversionFailure(
                    6,
                    rusqlite::types::Type::Text,
                    Box::new(e),
                )
            })?;
            Ok(FileSummary {
                file_id: r.get(0)?,
                rel_path,
                current_seq: r.get(2)?,
                version_count: r.get(3)?,
                last_timestamp_micros: r.get(4)?,
                last_size: r.get::<_, i64>(5)? as u64,
                last_kind,
            })
        })?;
        let mut out = Vec::new();
        for r in rows {
            let f = r?;
            let keep = match prefix {
                Some(p) => f.rel_path.to_lowercase().contains(&p.to_lowercase()),
                None => true,
            };
            if keep {
                out.push(f);
            }
        }
        Ok(out)
    }

    // ----- helper cho GC (retention dùng) -----

    /// Quét toàn bộ version của mọi file, group theo file, seq tăng dần.
    pub(crate) fn gc_scan(&self) -> Result<Vec<GcFile>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT v.id, v.file_id, v.seq, v.timestamp, v.kind, v.label, v.storage, v.base_seq, v.size, v.content_hash, f.workspace_id
             FROM versions v JOIN files f ON f.id = v.file_id
             ORDER BY v.file_id, v.seq ASC",
        )?;
        let mut out: Vec<GcFile> = Vec::new();
        let rows = stmt.query_map([], |r| {
            let ws_id = r.get::<_, i64>(10)?;
            let v = row_to_version(r)?;
            Ok((ws_id, v))
        })?;
        for r in rows {
            let (ws_id, v) = r?;
            match out.last_mut() {
                Some(g) if g.file_id == v.file_id => g.versions.push(v),
                _ => out.push(GcFile { ws_id, file_id: v.file_id, versions: vec![v] }),
            }
        }
        Ok(out)
    }

    /// Xóa các version theo id (một transaction). Trả về danh sách blob cần xóa trên disk
    /// (gọi sau khi COMMIT thành công).
    pub(crate) fn delete_versions(&self, ids: &[VersionId]) -> Result<Vec<BlobRef>> {
        let conn = self.conn.lock().unwrap();
        let mut blobs = Vec::new();
        let tx = conn.unchecked_transaction()?;
        {
            let mut stmt = tx.prepare(
                "SELECT id, file_id, storage FROM versions WHERE id = ?1",
            )?;
            let mut del = tx.prepare("DELETE FROM versions WHERE id = ?1")?;
            for id in ids {
                let row = stmt
                    .query_row([id], |r| {
                        Ok((
                            r.get::<_, i64>(0)?,
                            r.get::<_, i64>(1)?,
                            r.get::<_, String>(2)?,
                        ))
                    })
                    .map_err(|e| match e {
                        rusqlite::Error::QueryReturnedNoRows => {
                            Error::NotFound(format!("version {id}"))
                        }
                        other => Error::Sqlite(other),
                    })?;
                let (vid, file_id, storage) = row;
                let ws_id: i64 = tx.query_row(
                    "SELECT workspace_id FROM files WHERE id = ?1",
                    [file_id],
                    |r| r.get(0),
                )?;
                blobs.push(BlobRef {
                    ws_id,
                    version_id: vid,
                    storage: Storage::parse(&storage)?,
                });
                del.execute([id])?;
            }
        }
        tx.commit()?;
        Ok(blobs)
    }

    /// Tất cả workspace: (ws_id, canonical root).
    /// Dành cho daemon (`lh status`) và CLI ở milestone sau — GC sweep duyệt disk nên chưa dùng.
    #[allow(dead_code)]
    pub(crate) fn all_workspaces(&self) -> Result<Vec<(WorkspaceId, PathBuf)>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt =
            conn.prepare("SELECT id, root FROM workspaces ORDER BY id")?;
        let rows = stmt.query_map([], |r| {
            Ok((r.get::<_, i64>(0)?, PathBuf::from(r.get::<_, String>(1)?)))
        })?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    }

    /// Danh sách workspace — API public duy nhất để liệt kê: (ws_id, canonical root).
    /// CLI/GUI cần resolve file → workspace (match canonical root là prefix của file)
    /// mà không đụng vào internals; wrapper mỏng quanh `all_workspaces` (M3).
    pub fn workspaces(&self) -> Result<Vec<(WorkspaceId, PathBuf)>> {
        self.all_workspaces()
    }

    /// Set mọi version id còn trong db — dùng để tìm blob mồ côi.
    pub(crate) fn known_version_ids(&self) -> Result<HashSet<VersionId>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare("SELECT id FROM versions")?;
        let rows = stmt.query_map([], |r| r.get::<_, i64>(0))?;
        let mut out = HashSet::new();
        for r in rows {
            out.insert(r?);
        }
        Ok(out)
    }

    // ----- đường dẫn blob -----

    pub(crate) fn workspaces_dir(&self) -> PathBuf {
        self.root.join("workspaces")
    }

    pub(crate) fn blobs_dir(&self, ws: WorkspaceId) -> PathBuf {
        self.workspaces_dir().join(format!("{:016x}", ws as u64)).join("blobs")
    }

    pub(crate) fn blob_path(&self, ws: WorkspaceId, version_id: VersionId, storage: Storage) -> PathBuf {
        self.blobs_dir(ws)
            .join(format!("v{version_id}-{}.zst", storage.as_str()))
    }
}

/// Tham chiếu blob cần xóa trên disk (do GC trả về sau khi xóa row).
#[derive(Debug, Clone)]
pub(crate) struct BlobRef {
    pub ws_id: WorkspaceId,
    pub version_id: VersionId,
    pub storage: Storage,
}

pub(crate) fn row_to_version(r: &rusqlite::Row<'_>) -> rusqlite::Result<Version> {
    let kind: String = r.get(4)?;
    let label: Option<String> = r.get(5)?;
    let storage: String = r.get(6)?;
    Ok(Version {
        id: r.get(0)?,
        file_id: r.get(1)?,
        seq: r.get(2)?,
        timestamp_micros: r.get(3)?,
        kind: EventKind::parse(&kind).map_err(|e| {
            rusqlite::Error::FromSqlConversionFailure(4, rusqlite::types::Type::Text, Box::new(e))
        })?,
        label,
        storage: Storage::parse(&storage).map_err(|e| {
            rusqlite::Error::FromSqlConversionFailure(6, rusqlite::types::Type::Text, Box::new(e))
        })?,
        base_seq: r.get(7)?,
        size: r.get::<_, i64>(8)? as u64,
        content_hash: r.get(9)?,
    })
}

/// Thời gian hiện tại, unix micros UTC.
pub(crate) fn now_micros() -> i64 {
    time::OffsetDateTime::now_utc().unix_timestamp_nanos() as i64 / 1000
}

/// Canonicalize + strip prefix `\\?\` của Windows (giữ UNC `\\?\UNC\...` → `\\...`).
pub(crate) fn normalize_canonical(p: &Path) -> Result<PathBuf> {
    let canonical = p.canonicalize().map_err(|e| {
        Error::InvalidPath(format!("{}: {e}", p.display()))
    })?;
    let s = canonical.as_os_str().to_string_lossy().to_string();
    if let Some(rest) = s.strip_prefix(r"\\?\UNC\") {
        Ok(PathBuf::from(format!(r"\\{rest}")))
    } else if let Some(rest) = s.strip_prefix(r"\\?\") {
        Ok(PathBuf::from(rest))
    } else {
        Ok(canonical)
    }
}

/// rel_path của `file` so với `ws_root` (cả hai phải là đường đã chuẩn hóa), phân tách '/'.
/// File tồn tại → canonicalize trực tiếp. Không tồn tại (event delete) → canonicalize ancestor
/// gần nhất còn tồn tại rồi ghép phần còn lại (giữ nguyên casing gốc của phần thiếu).
pub(crate) fn rel_path_against(ws_root: &Path, file: &Path) -> Result<String> {
    let abs = if file.exists() {
        normalize_canonical(file)?
    } else {
        let mut existing = file.to_path_buf();
        let mut tail: Vec<std::ffi::OsString> = Vec::new();
        loop {
            let name = existing
                .file_name()
                .ok_or_else(|| Error::InvalidPath(file.display().to_string()))?
                .to_os_string();
            let parent = existing
                .parent()
                .ok_or_else(|| Error::InvalidPath(file.display().to_string()))?;
            tail.push(name);
            existing = parent.to_path_buf();
            if existing.exists() {
                break;
            }
        }
        let base = normalize_canonical(&existing)?;
        let mut abs = base;
        for comp in tail.iter().rev() {
            abs.push(comp);
        }
        abs
    };
    let rel = abs
        .strip_prefix(ws_root)
        .map_err(|_| Error::OutsideWorkspace(abs.display().to_string()))?;
    Ok(rel
        .components()
        .map(|c| c.as_os_str().to_string_lossy().to_string())
        .collect::<Vec<_>>()
        .join("/"))
}

/// ws_id deterministic: 8 bytes đầu blake3(canonical root) → u64 → i64 dương.
pub(crate) fn ws_id_from_root(root: &Path) -> WorkspaceId {
    let hash = blake3::hash(root.to_string_lossy().as_bytes());
    let bytes: [u8; 8] = hash.as_bytes()[..8].try_into().unwrap();
    ((u64::from_be_bytes(bytes)) & !(1u64 << 63)) as WorkspaceId
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_store() -> (tempfile::TempDir, LocalHistory) {
        let dir = tempfile::tempdir().unwrap();
        let store = LocalHistory::open(dir.path()).unwrap();
        (dir, store)
    }

    #[test]
    fn open_creates_schema() {
        let (_d, store) = tmp_store();
        let conn = store.conn.lock().unwrap();
        let v: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0)).unwrap();
        assert_eq!(v, 1);
        // 3 bảng tồn tại
        let n: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name IN ('workspaces','files','versions')",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(n, 3);
    }

    #[test]
    fn register_workspace_deterministic() {
        let dir = tempfile::tempdir().unwrap();
        let (_d, store) = tmp_store();
        let root = dir.path();
        let ws1 = store.register_workspace(root).unwrap();
        let ws2 = store
            .register_workspace(PathBuf::from(format!("{}\\", root.display())))
            .unwrap();
        assert_eq!(ws1, ws2);
        // gọi lại không nhân đôi row
        let cnt: i64 = store
            .conn
            .lock()
            .unwrap()
            .query_row("SELECT COUNT(*) FROM workspaces", [], |r| r.get(0))
            .unwrap();
        assert_eq!(cnt, 1);
    }

    #[test]
    fn history_empty_and_list_files() {
        let dir = tempfile::tempdir().unwrap();
        let (_d, store) = tmp_store();
        let ws = store.register_workspace(dir.path()).unwrap();
        let file = dir.path().join("a.rs");
        assert!(store.history(ws, &file).unwrap().is_empty());
        assert!(store.list_files(ws, None).unwrap().is_empty());
        // file ngoài workspace → lỗi
        let outside = tempfile::tempdir().unwrap();
        assert!(store.history(ws, outside.path().join("b.rs").as_path()).is_err());
    }

    #[test]
    fn rel_path_handles_missing_file() {
        let dir = tempfile::tempdir().unwrap();
        let root = normalize_canonical(dir.path()).unwrap();
        std::fs::write(root.join("x.txt"), b"hi").unwrap();
        let rel = rel_path_against(&root, &root.join("x.txt")).unwrap();
        assert_eq!(rel, "x.txt");
        // file không tồn tại vẫn resolve được
        let rel2 = rel_path_against(&root, &root.join("gone").join("y.txt")).unwrap();
        assert_eq!(rel2, "gone/y.txt");
    }

    #[test]
    fn open_read_only_works() {
        let wsdir = tempfile::tempdir().unwrap();
        let (d, store) = tmp_store();
        store.register_workspace(wsdir.path()).unwrap();
        drop(store);
        let ro = LocalHistory::open_read_only(d.path()).unwrap();
        assert!(ro.all_workspaces().unwrap().len() == 1);
        // read-only không ghi được
        assert!(ro.register_workspace(wsdir.path()).is_err());
    }
}
