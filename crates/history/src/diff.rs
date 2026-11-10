//! Content comparison: unified text diff (CLI) + structured hunks (GUI).
//! Binary version content (not valid UTF-8) yields `Error::BinaryContent(id)`.

use similar::TextDiff;

use crate::model::{Error, Hunk, Result, VersionId};
use crate::patch;
use crate::store::LocalHistory;

impl LocalHistory {
    /// Unified diff (context 3) between two versions, with headers
    /// `--- version {a}` / `+++ version {b}`.
    pub fn diff_text(&self, a: VersionId, b: VersionId) -> Result<String> {
        let sa = self.text_content(a)?;
        let sb = self.text_content(b)?;
        let diff = TextDiff::from_lines(sa.as_str(), sb.as_str());
        let mut udiff = diff.unified_diff();
        udiff.context_radius(3);
        udiff.header(&format!("version {a}"), &format!("version {b}"));
        Ok(udiff.to_string())
    }

    /// Structured hunks between two versions (same splice semantics as the structured patch).
    pub fn diff_lines(&self, a: VersionId, b: VersionId) -> Result<Vec<Hunk>> {
        let sa = self.text_content(a)?;
        let sb = self.text_content(b)?;
        patch::make_patch(sa.as_bytes(), sb.as_bytes())
    }

    /// Unified diff between raw content (e.g. the current disk state) and a version.
    /// Raw content is not tied to a version id, so binary errors use the sentinel id `-1`.
    pub fn diff_text_content(&self, content: &[u8], b: VersionId) -> Result<String> {
        let sa = raw_as_text(content)?;
        let sb = self.text_content(b)?;
        let diff = TextDiff::from_lines(sa.as_str(), sb.as_str());
        let mut udiff = diff.unified_diff();
        udiff.context_radius(3);
        udiff.header("current", &format!("version {b}"));
        Ok(udiff.to_string())
    }

    /// Structured hunks between raw content and a version.
    pub fn diff_lines_content(&self, content: &[u8], b: VersionId) -> Result<Vec<Hunk>> {
        let sa = raw_as_text(content)?;
        let sb = self.text_content(b)?;
        patch::make_patch(sa.as_bytes(), sb.as_bytes())
    }

    /// Content of a version as text — binary content yields `BinaryContent(id)`.
    fn text_content(&self, id: VersionId) -> Result<String> {
        let data = self.content(id)?;
        String::from_utf8(data).map_err(|_| Error::BinaryContent(id))
    }
}

/// Raw disk content as text. Not tied to any version, so binary errors use the sentinel id `-1`.
fn raw_as_text(content: &[u8]) -> Result<String> {
    String::from_utf8(content.to_vec()).map_err(|_| Error::BinaryContent(-1))
}
