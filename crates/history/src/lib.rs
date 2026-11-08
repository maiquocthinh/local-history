//! local-history engine — lịch sử thay đổi file dạng version chain (delta nén + full snapshot),
//! độc lập với VCS. Được dùng chung bởi daemon, CLI, GUI, và về sau nhúng thẳng vào Wu.
//!
//! Kiến trúc lưu trử: xem `docs/architecture.md`.
//! - `store`       — mở index SQLite, migration, đăng ký workspace, query dùng chung
//! - `model`       — kiểu dữ liệu hợp đồng (Version, Hunk, EventKind...)
//! - `record`      — ghi version mới (dedup blake3, binary detect, chọn full/delta, transaction)
//! - `patch`       — structured patch: sinh + apply hunks (định dạng delta trên disk)
//! - `reconstruct` — walk chain full→delta tái tạo nội dung version bất kỳ
//! - `diff`        — unified diff (text) + hunks có cấu trúc (GUI render)
//! - `retention`   — GC theo ngày làm việc + sweep blob mồ côi

pub mod model;
pub mod store;

mod diff;
mod patch;
mod record;
mod reconstruct;
mod retention;

pub use model::*;
pub use store::LocalHistory;
