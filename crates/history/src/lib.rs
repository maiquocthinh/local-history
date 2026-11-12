//! local-history engine — per-file change history as version chains (compressed deltas +
//! full snapshots), independent of VCS. Shared by the daemon, CLI, and GUI.
//!
//! Storage architecture: see `docs/architecture.md`.
//! - `store`       — SQLite index, schema migration, workspace registration, shared queries
//! - `model`       — shared data types (Version, Hunk, EventKind, ...)
//! - `record`      — writes new versions (blake3 dedup, binary detection, full/delta policy, transactional)
//! - `batch`       — batch grouping: listing queries, version→batch map, whole-batch revert
//! - `patch`       — structured patch: hunk generation and apply (the on-disk delta format)
//! - `reconstruct` — walks full→delta chains to rebuild any version's content
//! - `diff`        — unified text diff + structured hunks (GUI rendering)
//! - `retention`   — working-day GC + orphan blob sweep

pub mod model;
pub mod store;

mod batch;
mod diff;
mod patch;
mod record;
mod reconstruct;
mod retention;

pub use batch::{BatchEntry, BatchId, BatchPolicy, BatchSummary, RevertReport};
pub use model::*;
pub use store::LocalHistory;
