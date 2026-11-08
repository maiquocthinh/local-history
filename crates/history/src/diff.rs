//! diff — so sánh nội dung: unified diff (text, cho CLI) + hunks có cấu trúc (cho GUI).
//! Nội dung version binary (không phải UTF-8) → `Error::BinaryContent(id)`.

use similar::TextDiff;

use crate::model::{Error, Hunk, Result, VersionId};
use crate::patch;
use crate::store::LocalHistory;

impl LocalHistory {
    /// Unified diff (context 3) giữa hai version, header `--- version {a}` / `+++ version {b}`.
    pub fn diff_text(&self, a: VersionId, b: VersionId) -> Result<String> {
        let sa = self.text_content(a)?;
        let sb = self.text_content(b)?;
        let diff = TextDiff::from_lines(sa.as_str(), sb.as_str());
        let mut udiff = diff.unified_diff();
        udiff.context_radius(3);
        udiff.header(&format!("version {a}"), &format!("version {b}"));
        Ok(udiff.to_string())
    }

    /// Hunks có cấu trúc giữa hai version (cùng ngữ nghĩa splice với structured patch).
    pub fn diff_lines(&self, a: VersionId, b: VersionId) -> Result<Vec<Hunk>> {
        let sa = self.text_content(a)?;
        let sb = self.text_content(b)?;
        patch::make_patch(sa.as_bytes(), sb.as_bytes())
    }

    /// Unified diff giữa nội dung thô (disk hiện tại) và một version.
    /// Nội dung thô không gắn version id → lỗi binary dùng sentinel id = -1.
    pub fn diff_text_content(&self, content: &[u8], b: VersionId) -> Result<String> {
        let sa = raw_as_text(content)?;
        let sb = self.text_content(b)?;
        let diff = TextDiff::from_lines(sa.as_str(), sb.as_str());
        let mut udiff = diff.unified_diff();
        udiff.context_radius(3);
        udiff.header("current", &format!("version {b}"));
        Ok(udiff.to_string())
    }

    /// Hunks có cấu trúc giữa nội dung thô (disk hiện tại) và một version.
    pub fn diff_lines_content(&self, content: &[u8], b: VersionId) -> Result<Vec<Hunk>> {
        let sa = raw_as_text(content)?;
        let sb = self.text_content(b)?;
        patch::make_patch(sa.as_bytes(), sb.as_bytes())
    }

    /// Nội dung của một version dưới dạng text — binary → `BinaryContent(id)`.
    fn text_content(&self, id: VersionId) -> Result<String> {
        let data = self.content(id)?;
        String::from_utf8(data).map_err(|_| Error::BinaryContent(id))
    }
}

/// Nội dung thô từ disk → text. Không thuộc version nào nên không có id — dùng -1.
fn raw_as_text(content: &[u8]) -> Result<String> {
    String::from_utf8(content.to_vec()).map_err(|_| Error::BinaryContent(-1))
}
