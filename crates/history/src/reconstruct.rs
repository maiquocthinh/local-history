//! reconstruct — tái tạo nội dung của một version: walk ngược chain delta tới Full gần nhất,
//! decompress Full rồi áp các delta xuôi theo seq (mỗi bước là splice mảng dòng, xem `patch`).

use crate::model::*;
use crate::patch;
use crate::store::{row_to_version, LocalHistory};

/// Giới hạn kích thước giải nén một blob — chặn blob hỏng/zip-bomb.
/// Full ≤ 10MB, delta (JSON hunks) nhỏ hơn nhiều → 2 * MAX_FILE_SIZE + 64KB là dư địa.
const DECOMPRESS_BOUND: usize = (MAX_FILE_SIZE as usize) * 2 + 64 * 1024;

impl LocalHistory {
    /// Đọc + giải nén blob của một version.
    pub(crate) fn load_blob(&self, ws: WorkspaceId, v: &Version) -> Result<Vec<u8>> {
        let path = self.blob_path(ws, v.id, v.storage);
        let compressed = match std::fs::read(&path) {
            Ok(data) => data,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Err(Error::NotFound(format!("blob {}", path.display())));
            }
            Err(e) => return Err(e.into()),
        };
        Ok(zstd::bulk::decompress(&compressed, DECOMPRESS_BOUND)?)
    }

    /// Lấy version theo id (join `files` để biết workspace của nó).
    pub(crate) fn version_by_id(&self, id: VersionId) -> Result<Version> {
        Ok(self.version_by_id_with_ws(id)?.0)
    }

    fn version_by_id_with_ws(&self, id: VersionId) -> Result<(Version, WorkspaceId)> {
        let conn = self.conn.lock().unwrap();
        conn.query_row(
            "SELECT v.id, v.file_id, v.seq, v.timestamp, v.kind, v.label, v.storage, v.base_seq, v.size, v.content_hash, f.workspace_id
             FROM versions v JOIN files f ON f.id = v.file_id
             WHERE v.id = ?1",
            [id],
            |r| {
                let ws_id: i64 = r.get(10)?;
                Ok((row_to_version(r)?, ws_id))
            },
        )
        .map_err(|e| match e {
            rusqlite::Error::QueryReturnedNoRows => Error::NotFound(format!("version {id}")),
            other => other.into(),
        })
    }

    /// Version theo (file_id, seq). Thiếu row giữa chain → chain hỏng, không phải NotFound.
    fn version_by_seq(&self, file_id: FileId, seq: i64) -> Result<Version> {
        let conn = self.conn.lock().unwrap();
        conn.query_row(
            "SELECT id, file_id, seq, timestamp, kind, label, storage, base_seq, size, content_hash
             FROM versions WHERE file_id = ?1 AND seq = ?2",
            rusqlite::params![file_id, seq],
            row_to_version,
        )
        .map_err(|e| match e {
            rusqlite::Error::QueryReturnedNoRows => {
                Error::CorruptChain(format!("version (file {file_id}, seq {seq}) thiếu — chain hỏng"))
            }
            other => other.into(),
        })
    }

    /// Nội dung đầy đủ của một version: walk ngược về Full gần nhất, decompress, áp delta
    /// xuôi theo seq tăng dần. base_seq phải khớp liên tục (seq - 1) — lệch → CorruptChain.
    pub fn content(&self, version_id: VersionId) -> Result<Vec<u8>> {
        let (mut cur, ws) = self.version_by_id_with_ws(version_id)?;
        let mut deltas: Vec<Version> = Vec::new();
        loop {
            match cur.storage {
                Storage::Full => break,
                Storage::Delta => {
                    let base_seq = cur.base_seq.ok_or_else(|| {
                        Error::CorruptChain(format!("version {} (delta) thiếu base_seq", cur.id))
                    })?;
                    if base_seq != cur.seq - 1 {
                        return Err(Error::CorruptChain(format!(
                            "version {} (seq {}) có base_seq {base_seq} — chain không liên tục",
                            cur.id, cur.seq
                        )));
                    }
                    let file_id = cur.file_id;
                    deltas.push(cur);
                    cur = self.version_by_seq(file_id, base_seq)?;
                }
            }
        }
        // `cur` giờ là Full gần nhất dưới chain
        let mut data = self.load_blob(ws, &cur)?;
        for dv in deltas.iter().rev() {
            let raw = self.load_blob(ws, dv)?;
            let hunks = patch::decode_hunks(&raw)?;
            data = patch::apply_patch(&data, &hunks)?;
        }
        Ok(data)
    }

    /// Nội dung của version mới nhất của file — dùng trong `record` để sinh delta.
    pub(crate) fn latest_content(&self, file_id: FileId) -> Result<Option<(Version, Vec<u8>)>> {
        let latest: Option<Version> = {
            let conn = self.conn.lock().unwrap();
            match conn.query_row(
                "SELECT id, file_id, seq, timestamp, kind, label, storage, base_seq, size, content_hash
                 FROM versions WHERE file_id = ?1 ORDER BY seq DESC LIMIT 1",
                [file_id],
                row_to_version,
            ) {
                Ok(v) => Some(v),
                Err(rusqlite::Error::QueryReturnedNoRows) => None,
                Err(e) => return Err(e.into()),
            }
        };
        match latest {
            None => Ok(None),
            Some(v) => {
                let data = self.content(v.id)?;
                Ok(Some((v, data)))
            }
        }
    }
}
