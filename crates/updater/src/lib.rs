//! Self-update engine for the single `lh.exe` binary (plan §3.2/§3.3).
//!
//! Layers, in the order a real update runs:
//!
//! - [`prepare`]: resolve the update source (`--source` flag > `LH_UPDATE_SOURCE`
//!   env > `<exe_dir>/update.json`), fetch + validate the manifest, compare
//!   versions and — unless `check_only` — download every artifact to a
//!   `.lh-new-<pid>-<name>` temp file next to the target (same volume, so the
//!   later renames are metadata-only) and sha256-verify it. The exes are never
//!   touched in this phase: any failure removes every staged temp.
//! - [`swap_all`]: per artifact whose target file exists, `rename(target →
//!   .lh-old-<pid>-<name>)` then `rename(staged → target)`. Both are
//!   metadata-only renames; on Windows renaming a running image is legal
//!   ("it can be renamed, but it cannot be unlinked"). A failed rename-in rolls
//!   back that file and every earlier swap of the batch in reverse order.
//! - [`cleanup_stale`]: startup recovery — auto-restore the newest
//!   `.lh-old-*` onto a target that vanished between the two renames (crash
//!   window), then delete leftover `.lh-old-*`/`.lh-new-*` best-effort once the
//!   main binary is back in place.
//! - [`update`]: prepare + swap_all, then relaunch via `restart_args` when
//!   artifacts were actually swapped.
//!
//! sha256 is MANDATORY for every manifest artifact (review note 7): a manifest
//! whose artifact lacks a valid 64-hex sha256 is rejected instead of silently
//! skipping integrity verification.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use serde::Deserialize;
use sha2::{Digest, Sha256};

/// Env var consulted by [`resolve_source`] when `--source` is not given.
pub const ENV_UPDATE_SOURCE: &str = "LH_UPDATE_SOURCE";

/// JSON file (`{"source": "..."}`) placed next to the executable, last
/// fallback of the source resolution chain.
const UPDATE_JSON: &str = "update.json";

const STALE_OLD_PREFIX: &str = ".lh-old-";
const STALE_NEW_PREFIX: &str = ".lh-new-";

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

#[derive(Debug, thiserror::Error)]
pub enum UpdateError {
    #[error(
        "no update source configured: pass --source, set LH_UPDATE_SOURCE, \
         or place an update.json file containing a source string next to the executable"
    )]
    SourceMissing,

    #[error("unsupported update source `{0}`: expected a directory path or an http(s) URL")]
    SourceUnsupported(String),

    #[error("failed to fetch `{name}` from {origin}: {error}")]
    Fetch {
        name: String,
        // NOTE: must not be named `source` — thiserror treats a field with that
        // name as the error's `source()` field (as_dyn_error on it).
        origin: String,
        error: String,
    },

    #[error("invalid manifest: {0}")]
    Manifest(String),

    #[error("sha256 mismatch for `{name}`: expected {expected}, got {actual}")]
    Verify {
        name: String,
        expected: String,
        actual: String,
    },

    #[error("swap failed at {stage} for `{name}`: {error} (rolled back: {rolled_back})")]
    Swap {
        name: String,
        stage: &'static str,
        error: String,
        rolled_back: String,
    },

    #[error("failed to relaunch `{program}`: {error}")]
    Relaunch {
        program: String,
        error: std::io::Error,
    },

    #[error(transparent)]
    Io(#[from] std::io::Error),
}

// ---------------------------------------------------------------------------
// Options / report / manifest
// ---------------------------------------------------------------------------

/// Options for [`update`] / [`prepare`].
#[derive(Debug, Clone, Default)]
pub struct UpdateOpts {
    /// Explicit source (directory path or http(s) URL); beats the env var and
    /// `update.json`.
    pub source: Option<String>,
    /// Fetch and compare the manifest only — never download or swap.
    pub check_only: bool,
    /// Install even when the manifest version equals the running version.
    pub force: bool,
    /// When set and artifacts were actually swapped, spawn `current_exe()`
    /// detached with these args (e.g. `["gui"]`) so the fresh image takes over.
    pub restart_args: Option<Vec<String>>,
}

/// Outcome report of a full [`update`] / [`swap_all`] run.
#[derive(Debug)]
pub struct UpdateReport {
    /// Version the install was at before the swap (running version).
    pub from: String,
    /// Version the manifest advertised (what was swapped in).
    pub to: String,
    /// Artifact names whose target file was swapped.
    pub updated: Vec<String>,
    /// Artifact names skipped because their target file does not exist in the
    /// install dir (single-file install scenario, plan §3.3 step 8).
    pub skipped: Vec<String>,
    /// Install directory (parent of the running executable).
    pub path: PathBuf,
    /// True when the manifest version equals the running version and `force`
    /// was not set: nothing was downloaded or swapped. Kept explicit so callers
    /// can distinguish "already latest" from "nothing had a target file".
    pub up_to_date: bool,
}

/// Distribution manifest: `{"version": "...", "artifacts": [{"name": ..., "sha256": ...}]}`.
#[derive(Debug, Clone, Deserialize)]
pub struct Manifest {
    pub version: String,
    pub artifacts: Vec<ManifestArtifact>,
}

/// One artifact entry of a [`Manifest`].
#[derive(Debug, Clone, Deserialize)]
pub struct ManifestArtifact {
    pub name: String,
    /// MANDATORY 64-hex digest — see [`validate_manifest`].
    pub sha256: String,
}

// ---------------------------------------------------------------------------
// Source trait + implementations
// ---------------------------------------------------------------------------

/// Byte fetcher for the distribution source (plan §3.3 step 2).
pub trait Source {
    /// Fetch one file by name relative to the source root.
    fn fetch(&self, name: &str) -> Result<Vec<u8>, UpdateError>;
    /// Human-readable identity for logs and error messages.
    fn describe(&self) -> String;
}

/// Directory / UNC share source backed by `std::fs` — for internal folders and
/// network shares.
pub struct FileSource {
    root: PathBuf,
}

impl FileSource {
    pub fn new(root: PathBuf) -> Self {
        Self { root }
    }
}

impl Source for FileSource {
    fn fetch(&self, name: &str) -> Result<Vec<u8>, UpdateError> {
        fs::read(self.root.join(name)).map_err(|e| UpdateError::Fetch {
            name: name.to_string(),
            origin: self.describe(),
            error: e.to_string(),
        })
    }

    fn describe(&self) -> String {
        self.root.display().to_string()
    }
}

/// HTTP(S) source backed by ureq (pure-Rust rustls TLS) — for distribution
/// servers. Never used in unit tests (no network).
pub struct HttpSource {
    base: String,
}

impl HttpSource {
    pub fn new(base: String) -> Self {
        Self {
            base: base.trim_end_matches('/').to_string(),
        }
    }
}

impl Source for HttpSource {
    fn fetch(&self, name: &str) -> Result<Vec<u8>, UpdateError> {
        let url = format!("{}/{}", self.base, name);
        let described = self.describe();
        let fail = |error: String| UpdateError::Fetch {
            name: name.to_string(),
            origin: described.clone(),
            error,
        };
        let mut res = ureq::get(&url).call().map_err(|e| fail(e.to_string()))?;
        let status = res.status().as_u16();
        if !(200..300).contains(&status) {
            return Err(fail(format!("HTTP {status}")));
        }
        res.body_mut().read_to_vec().map_err(|e| fail(e.to_string()))
    }

    fn describe(&self) -> String {
        self.base.clone()
    }
}

/// Pick the [`Source`] implementation from a resolved source string: http(s)
/// prefix → [`HttpSource`], anything else → [`FileSource`].
fn make_source(source: &str) -> Result<Box<dyn Source>, UpdateError> {
    let s = source.trim();
    if s.is_empty() {
        return Err(UpdateError::SourceMissing);
    }
    if s.starts_with("http://") || s.starts_with("https://") {
        return Ok(Box::new(HttpSource::new(s.to_string())));
    }
    if s.contains("://") {
        return Err(UpdateError::SourceUnsupported(s.to_string()));
    }
    Ok(Box::new(FileSource::new(PathBuf::from(s))))
}

// ---------------------------------------------------------------------------
// prepare
// ---------------------------------------------------------------------------

/// Result of [`prepare`].
#[derive(Debug)]
pub enum Prepared {
    /// Manifest version equals the running version and `force` was not set.
    UpToDate {
        current_version: String,
        manifest_version: String,
        exe_dir: PathBuf,
    },
    /// A different manifest version was found. With `check_only` nothing was
    /// downloaded; otherwise every artifact is staged in `exe_dir` and verified.
    Ready(Box<PreparedUpdate>),
}

/// The download/verify half of an update, ready for [`swap_all`].
#[derive(Debug)]
pub struct PreparedUpdate {
    pub current_version: String,
    pub manifest_version: String,
    pub exe_dir: PathBuf,
    /// Human-readable source description (path or URL).
    pub source: String,
    /// True for a `check_only` run: manifest fetched, no downloads, nothing to swap.
    pub check_only: bool,
    pub artifacts: Vec<PreparedArtifact>,
}

/// One artifact staged on disk and sha256-verified.
#[derive(Debug)]
pub struct PreparedArtifact {
    pub name: String,
    /// Verified sha256, lowercase hex.
    pub sha256: String,
    /// `.lh-new-<pid>-<name>` staged in `exe_dir` (same volume as the target).
    pub staged_path: PathBuf,
}

/// Version of the running build used for manifest comparison. The updater crate
/// shares the workspace version, which is the version baked into `lh.exe`.
pub fn current_version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

/// Production entry point of [`prepare_in`]: the install dir is the parent of
/// the running executable.
pub fn prepare(opts: &UpdateOpts) -> Result<Prepared, UpdateError> {
    let exe = std::env::current_exe()?;
    let exe_dir = exe.parent().ok_or_else(|| {
        UpdateError::Io(std::io::Error::other("current_exe has no parent directory"))
    })?;
    prepare_in(opts, exe_dir)
}

/// [`prepare`] with an explicit install dir — test seam and future callers that
/// already know their install dir.
pub fn prepare_in(opts: &UpdateOpts, exe_dir: &Path) -> Result<Prepared, UpdateError> {
    let source_str = resolve_source(opts, exe_dir)?;
    let source = make_source(&source_str)?;
    let manifest_bytes = source.fetch("manifest.json")?;
    let manifest: Manifest = serde_json::from_slice(&manifest_bytes).map_err(|e| {
        UpdateError::Manifest(format!(
            "failed to parse manifest.json from {}: {e}",
            source.describe()
        ))
    })?;
    validate_manifest(&manifest)?;

    let current = current_version().to_string();
    if !opts.force && manifest.version == current {
        log::info!(
            "update source {} serves version {current}; running {current} — up to date",
            source.describe()
        );
        return Ok(Prepared::UpToDate {
            current_version: current,
            manifest_version: manifest.version,
            exe_dir: exe_dir.to_path_buf(),
        });
    }

    if opts.check_only {
        log::info!(
            "update available: {current} -> {} at {} (check only, nothing downloaded)",
            manifest.version,
            source.describe()
        );
        return Ok(Prepared::Ready(Box::new(PreparedUpdate {
            current_version: current,
            manifest_version: manifest.version,
            exe_dir: exe_dir.to_path_buf(),
            source: source.describe(),
            check_only: true,
            artifacts: Vec::new(),
        })));
    }

    // Download + verify every artifact BEFORE touching any exe (plan §3.3
    // steps 5). On any failure every staged temp of this run is removed: the
    // install is still pristine.
    let pid = std::process::id();
    let mut staged: Vec<PreparedArtifact> = Vec::new();
    let result = (|| -> Result<(), UpdateError> {
        for artifact in &manifest.artifacts {
            let expected = artifact.sha256.trim().to_ascii_lowercase();
            let staged_path = exe_dir.join(format!("{STALE_NEW_PREFIX}{pid}-{}", artifact.name));
            let bytes = source.fetch(&artifact.name)?;
            fs::write(&staged_path, &bytes)?;
            // Preserve the target's permission bits on Unix: the swap renames
            // the staged file over the target, so a plain 0644 staged file
            // would make the updated `lh` non-executable. A target that does
            // not exist yet defaults to 0755 (the updater ships executables).
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let mode = fs::metadata(exe_dir.join(&artifact.name))
                    .map(|m| m.permissions().mode() & 0o777)
                    .unwrap_or(0o755);
                fs::set_permissions(&staged_path, fs::Permissions::from_mode(mode))?;
            }
            let actual = sha256_file(&staged_path)?;
            if actual != expected {
                let _ = fs::remove_file(&staged_path);
                return Err(UpdateError::Verify {
                    name: artifact.name.clone(),
                    expected,
                    actual,
                });
            }
            staged.push(PreparedArtifact {
                name: artifact.name.clone(),
                sha256: expected,
                staged_path,
            });
        }
        Ok(())
    })();

    match result {
        Ok(()) => {
            log::info!(
                "staged {} verified artifact(s) from {}",
                staged.len(),
                source.describe()
            );
            Ok(Prepared::Ready(Box::new(PreparedUpdate {
                current_version: current,
                manifest_version: manifest.version,
                exe_dir: exe_dir.to_path_buf(),
                source: source.describe(),
                check_only: false,
                artifacts: staged,
            })))
        }
        Err(e) => {
            for artifact in &staged {
                let _ = fs::remove_file(&artifact.staged_path);
            }
            Err(e)
        }
    }
}

/// Resolve the update source, three levels (plan §3.3 step 1):
/// `--source` flag > `LH_UPDATE_SOURCE` env > `<exe_dir>/update.json`.
fn resolve_source(opts: &UpdateOpts, exe_dir: &Path) -> Result<String, UpdateError> {
    if let Some(flag) = opts.source.as_deref() {
        let flag = flag.trim();
        if !flag.is_empty() {
            return Ok(flag.to_string());
        }
    }
    if let Ok(from_env) = std::env::var(ENV_UPDATE_SOURCE) {
        let from_env = from_env.trim();
        if !from_env.is_empty() {
            return Ok(from_env.to_string());
        }
    }
    let path = exe_dir.join(UPDATE_JSON);
    if let Ok(bytes) = fs::read(&path) {
        #[derive(Deserialize)]
        struct UpdateRef {
            source: String,
        }
        match serde_json::from_slice::<UpdateRef>(&bytes) {
            Ok(r) => {
                let source = r.source.trim();
                if !source.is_empty() {
                    return Ok(source.to_string());
                }
            }
            Err(e) => log::debug!("ignoring unreadable {}: {e}", path.display()),
        }
    }
    Err(UpdateError::SourceMissing)
}

// ---------------------------------------------------------------------------
// swap
// ---------------------------------------------------------------------------

/// Swap staged artifacts into the install dir (plan §3.3 steps 6-9). On a
/// `check_only` [`Prepared`] or an up-to-date one this is a no-op report.
pub fn swap_all(prepared: Prepared) -> Result<UpdateReport, UpdateError> {
    match prepared {
        Prepared::UpToDate {
            current_version,
            manifest_version,
            exe_dir,
        } => Ok(UpdateReport {
            from: current_version,
            to: manifest_version,
            updated: Vec::new(),
            skipped: Vec::new(),
            path: exe_dir,
            up_to_date: true,
        }),
        Prepared::Ready(update) => swap_staged(*update),
    }
}

fn swap_staged(p: PreparedUpdate) -> Result<UpdateReport, UpdateError> {
    if p.check_only {
        // --check run: manifest compared, nothing staged, nothing to swap.
        return Ok(UpdateReport {
            from: p.current_version,
            to: p.manifest_version,
            updated: Vec::new(),
            skipped: Vec::new(),
            path: p.exe_dir,
            up_to_date: false,
        });
    }

    let pid = std::process::id();
    let mut updated: Vec<String> = Vec::new();
    let mut skipped: Vec<String> = Vec::new();
    // (old path, target path) of completed swaps — rolled back in reverse order
    // when a later rename-in fails (review note 1).
    let mut swapped: Vec<(PathBuf, PathBuf)> = Vec::new();
    let staged_paths: Vec<PathBuf> = p
        .artifacts
        .iter()
        .map(|artifact| artifact.staged_path.clone())
        .collect();

    for artifact in &p.artifacts {
        let target = p.exe_dir.join(&artifact.name);
        if !target.exists() {
            // Single-file install scenario: the manifest may list more exes
            // than this install carries; only replace what exists.
            log::info!(
                "`{}` not present in {} — skipped",
                artifact.name,
                p.exe_dir.display()
            );
            skipped.push(artifact.name.clone());
            continue;
        }
        let old = p
            .exe_dir
            .join(format!("{STALE_OLD_PREFIX}{pid}-{}", artifact.name));

        // rename-away: metadata-only; legal on Windows for a running image.
        if let Err(e) = fs::rename(&target, &old) {
            remove_best_effort(&staged_paths);
            return Err(UpdateError::Swap {
                name: artifact.name.clone(),
                stage: "rename-away",
                error: e.to_string(),
                rolled_back: "none (nothing was swapped)".to_string(),
            });
        }
        // rename-in: metadata-only. Failure → roll back this file and every
        // earlier swap of the batch, newest first.
        if let Err(e) = fs::rename(&artifact.staged_path, &target) {
            let rolled_back = rollback(&old, &target, artifact.name.clone(), &swapped);
            remove_best_effort(&staged_paths);
            return Err(UpdateError::Swap {
                name: artifact.name.clone(),
                stage: "rename-in",
                error: e.to_string(),
                rolled_back,
            });
        }
        updated.push(artifact.name.clone());
        swapped.push((old, target));
    }

    // Batch complete. The renamed-away images may still be locked by processes
    // running from them — removal is best-effort; startup cleanup retries.
    for (old, _) in &swapped {
        let _ = fs::remove_file(old);
    }
    remove_best_effort(&staged_paths);

    Ok(UpdateReport {
        from: p.current_version,
        to: p.manifest_version,
        updated,
        skipped,
        path: p.exe_dir,
        up_to_date: false,
    })
}

/// Restore `old → target` for the failing file, then every earlier swap of the
/// batch in reverse order. Returns a human-readable rollback outcome for the
/// error message.
fn rollback(
    failed_old: &Path,
    failed_target: &Path,
    failed_name: String,
    swapped: &[(PathBuf, PathBuf)],
) -> String {
    let mut restored: Vec<String> = Vec::new();
    let mut failed: Vec<String> = Vec::new();

    if fs::rename(failed_old, failed_target).is_ok() {
        restored.push(failed_name);
    } else {
        failed.push(failed_name);
    }
    for (old, target) in swapped.iter().rev() {
        let name = target
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| target.display().to_string());
        if fs::rename(old, target).is_ok() {
            restored.push(name);
        } else {
            failed.push(name);
        }
    }

    if restored.is_empty() && failed.is_empty() {
        return "none".to_string();
    }
    let mut desc = format!("restored {}", restored.join(", "));
    if !failed.is_empty() {
        desc.push_str(&format!("; FAILED to restore {}", failed.join(", ")));
    }
    desc
}

fn remove_best_effort(paths: &[PathBuf]) {
    for path in paths {
        let _ = fs::remove_file(path);
    }
}

// ---------------------------------------------------------------------------
// update (prepare + swap + relaunch)
// ---------------------------------------------------------------------------

/// Full self-update: [`prepare`] + [`swap_all`], then relaunch via
/// `restart_args` when artifacts were actually swapped. A relaunch failure is
/// reported as an error even though the swap itself already succeeded.
pub fn update(opts: &UpdateOpts) -> Result<UpdateReport, UpdateError> {
    let report = swap_all(prepare(opts)?)?;
    if let Some(args) = &opts.restart_args
        && !report.up_to_date
        && !report.updated.is_empty()
    {
        relaunch(args)?;
    }
    Ok(report)
}

/// Spawn `current_exe()` detached with `args` (Windows:
/// DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP, same contract as the GUI spawn
/// helpers in cli/daemon).
fn relaunch(args: &[String]) -> Result<(), UpdateError> {
    let exe = std::env::current_exe()?;
    let mut cmd = std::process::Command::new(&exe);
    cmd.args(args);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const DETACHED_PROCESS: u32 = 0x0000_0008;
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
        cmd.creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP);
    }
    cmd.spawn()
        .map_err(|error| UpdateError::Relaunch {
            program: exe.display().to_string(),
            error,
        })?;
    Ok(())
}

// ---------------------------------------------------------------------------
// startup recovery
// ---------------------------------------------------------------------------

/// Startup recovery for the install directory of the running binary, guarding
/// the main executable `lh{EXE_SUFFIX}`. See [`cleanup_stale_for`].
pub fn cleanup_stale(exe_dir: &Path) {
    let suffix = std::env::consts::EXE_SUFFIX;
    let main_exe = format!("lh{suffix}");
    cleanup_stale_for(exe_dir, &main_exe);
}

/// Two-layer recovery (plan §3.3 steps 12-13), with `main_exe` injected so
/// tests and sibling binaries can reuse the logic:
///
/// 1. auto-restore: any target missing on disk (crash between rename-away and
///    rename-in) gets its newest `.lh-old-<pid>-<name>` renamed back;
/// 2. stale removal: once the main binary is present again, delete leftover
///    `.lh-old-*` / `.lh-new-*` best-effort (files may still be locked by
///    running processes — the next startup retries).
///
/// Restore runs BEFORE removal so a stale file that can still heal a missing
/// target is never deleted; an old file whose restore failed is kept for the
/// next startup.
pub fn cleanup_stale_for(exe_dir: &Path, main_exe: &str) {
    let (old_files, new_files) = match scan_stale(exe_dir) {
        Ok(found) => found,
        Err(e) => {
            log::debug!("cleanup_stale: cannot scan {}: {e}", exe_dir.display());
            return;
        }
    };
    if old_files.is_empty() && new_files.is_empty() {
        return;
    }

    // Newest `.lh-old` per artifact name (mtime wins — older leftovers are
    // duplicates from previous runs).
    let mut newest: HashMap<String, (PathBuf, SystemTime)> = HashMap::new();
    for (name, path, mtime) in &old_files {
        let is_newer = newest
            .get(name)
            .is_none_or(|(_, at)| mtime > at);
        if is_newer {
            newest.insert(name.clone(), (path.clone(), *mtime));
        }
    }

    let mut keep: HashSet<PathBuf> = HashSet::new();
    for (name, (old_path, _)) in &newest {
        let target = exe_dir.join(name);
        if target.exists() {
            continue;
        }
        match fs::rename(old_path, &target) {
            Ok(()) => log::warn!(
                "auto-restored `{}` from `{}` — previous update crashed between rename-away and rename-in",
                name,
                old_path.display()
            ),
            Err(e) => {
                log::warn!(
                    "failed to auto-restore `{}` from `{}`: {e}",
                    name,
                    old_path.display()
                );
                keep.insert(old_path.clone());
            }
        }
    }

    let main_path = exe_dir.join(main_exe);
    if main_path.exists() {
        for (_, path, _) in &old_files {
            if keep.contains(path) {
                continue;
            }
            let _ = fs::remove_file(path);
        }
        for path in &new_files {
            let _ = fs::remove_file(path);
        }
        log::debug!(
            "cleanup_stale: removed stale .lh-old/.lh-new files in {}",
            exe_dir.display()
        );
    } else {
        log::warn!(
            "`{}` is missing in {} and could not be restored — keeping stale files for the next startup",
            main_exe,
            exe_dir.display()
        );
    }
}

/// `.lh-old-*` entries as (artifact name, path, mtime), plus plain `.lh-new-*` paths.
type StaleFiles = (Vec<(String, PathBuf, SystemTime)>, Vec<PathBuf>);

/// Collect `.lh-old-*` files (derived artifact name + mtime) and `.lh-new-*`
/// files in `exe_dir`.
fn scan_stale(exe_dir: &Path) -> std::io::Result<StaleFiles> {
    let mut old_files = Vec::new();
    let mut new_files = Vec::new();
    for entry in fs::read_dir(exe_dir)? {
        let entry = entry?;
        if !entry.file_type()?.is_file() {
            continue;
        }
        let file_name = entry.file_name();
        let Some(name) = file_name.to_str() else {
            continue;
        };
        if let Some(rest) = name.strip_prefix(STALE_OLD_PREFIX) {
            let Some(artifact) = parse_stale_artifact(rest) else {
                log::debug!("cleanup_stale: ignoring unparseable stale file `{name}`");
                continue;
            };
            let mtime = entry
                .metadata()
                .and_then(|m| m.modified())
                .unwrap_or(SystemTime::UNIX_EPOCH);
            old_files.push((artifact, entry.path(), mtime));
        } else if name.starts_with(STALE_NEW_PREFIX) {
            new_files.push(entry.path());
        }
    }
    Ok((old_files, new_files))
}

/// `.lh-old-<pid>-<name>` → `<name>`; the pid is the leading digit run.
fn parse_stale_artifact(rest: &str) -> Option<String> {
    let digits = rest.chars().take_while(|c| c.is_ascii_digit()).count();
    if digits == 0 {
        return None;
    }
    let name = rest[digits..].strip_prefix('-')?;
    if name.is_empty() {
        return None;
    }
    Some(name.to_string())
}

// ---------------------------------------------------------------------------
// hashing / validation
// ---------------------------------------------------------------------------

/// Streaming sha256 of a staged file (artifacts can be ~20MB — no full
/// second copy in memory).
fn sha256_file(path: &Path) -> Result<String, UpdateError> {
    let mut file = fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 64 * 1024];
    loop {
        let read = file.read(&mut buf)?;
        if read == 0 {
            break;
        }
        hasher.update(&buf[..read]);
    }
    Ok(to_hex(hasher.finalize().as_slice()))
}

fn to_hex(bytes: &[u8]) -> String {
    bytes
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// Manifest integrity validation. sha256 is mandatory for every artifact
/// (review note 7): missing field fails at parse time (the field is not
/// `Option`), empty/short/non-hex fails here. Artifact names are restricted to
/// plain exe-like file names so a hostile manifest cannot point a swap outside
/// the install dir or collide with the `.lh-old-`/`.lh-new-` namespace.
fn validate_manifest(manifest: &Manifest) -> Result<(), UpdateError> {
    if manifest.version.trim().is_empty() {
        return Err(UpdateError::Manifest(
            "manifest has an empty version".to_string(),
        ));
    }
    if manifest.artifacts.is_empty() {
        return Err(UpdateError::Manifest(
            "manifest lists no artifacts".to_string(),
        ));
    }
    let mut seen: HashSet<String> = HashSet::new();
    for artifact in &manifest.artifacts {
        if !valid_artifact_name(&artifact.name) {
            return Err(UpdateError::Manifest(format!(
                "artifact name `{}` is not a plain file name (allowed: alphanumeric, '.', '-', '_')",
                artifact.name
            )));
        }
        let sha = artifact.sha256.trim();
        if sha.len() != 64 || !sha.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(UpdateError::Manifest(format!(
                "artifact `{}` must carry a sha256 (64 hex chars), got `{}` — sha256 is mandatory, verification is never skipped",
                artifact.name, artifact.sha256
            )));
        }
        if !seen.insert(artifact.name.clone()) {
            return Err(UpdateError::Manifest(format!(
                "duplicate artifact `{}`",
                artifact.name
            )));
        }
    }
    Ok(())
}

fn valid_artifact_name(name: &str) -> bool {
    !name.is_empty()
        && !name.starts_with('.')
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b'_'))
}

// ---------------------------------------------------------------------------
// tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use std::sync::{Mutex, PoisonError};
    use std::time::Duration;

    use tempfile::TempDir;

    use super::*;

    /// Tests that touch LH_UPDATE_SOURCE serialize on this lock; no other test
    /// reads the variable (they pass an explicit `source`), so mutating the
    /// process env here is sound even under the parallel test harness.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    fn opts(source: &str) -> UpdateOpts {
        UpdateOpts {
            source: Some(source.to_string()),
            ..UpdateOpts::default()
        }
    }

    fn tempdir() -> TempDir {
        tempfile::tempdir().unwrap()
    }

    fn write(dir: &Path, name: &str, content: &[u8]) -> PathBuf {
        let path = dir.join(name);
        fs::write(&path, content).unwrap();
        path
    }

    fn manifest_json(version: &str, artifacts: &[(&str, &str)]) -> String {
        let items: Vec<String> = artifacts
            .iter()
            .map(|(name, sha)| format!(r#"{{"name": "{name}", "sha256": "{sha}"}}"#))
            .collect();
        format!(r#"{{"version": "{version}", "artifacts": [{}]}}"#, items.join(", "))
    }

    fn write_manifest(dir: &Path, version: &str, artifacts: &[(&str, &str)]) {
        write(dir, "manifest.json", manifest_json(version, artifacts).as_bytes());
    }

    fn sha_hex(content: &[u8]) -> String {
        let mut hasher = Sha256::new();
        hasher.update(content);
        to_hex(hasher.finalize().as_slice())
    }

    /// File names in `dir` starting with `.lh-` (staged temps / stale files).
    fn lh_temp_files(dir: &Path) -> Vec<String> {
        let mut names = Vec::new();
        for entry in fs::read_dir(dir).unwrap() {
            let entry = entry.unwrap();
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.starts_with(".lh-") {
                names.push(name);
            }
        }
        names.sort();
        names
    }

    /// Build a [`Prepared::Ready`] by hand: staged temp files are written for
    /// `Some(content)` and deliberately absent for `None` (fail-injection).
    fn ready(exe_dir: &Path, artifacts: Vec<(&str, Option<Vec<u8>>)>) -> Prepared {
        let pid = std::process::id();
        let staged: Vec<PreparedArtifact> = artifacts
            .into_iter()
            .map(|(name, content)| {
                let staged_path = exe_dir.join(format!("{STALE_NEW_PREFIX}{pid}-{name}"));
                if let Some(bytes) = content {
                    fs::write(&staged_path, bytes).unwrap();
                }
                PreparedArtifact {
                    name: name.to_string(),
                    sha256: "0".repeat(64),
                    staged_path,
                }
            })
            .collect();
        Prepared::Ready(Box::new(PreparedUpdate {
            current_version: "0.1.0".to_string(),
            manifest_version: "0.2.0".to_string(),
            exe_dir: exe_dir.to_path_buf(),
            source: "test-source".to_string(),
            check_only: false,
            artifacts: staged,
        }))
    }

    fn set_mtime(path: &Path, at: SystemTime) {
        let file = fs::OpenOptions::new().write(true).open(path).unwrap();
        file.set_modified(at).unwrap();
    }

    // -- plan case 1: parse manifest ----------------------------------------

    #[test]
    fn manifest_parses() {
        let sha = "a".repeat(64);
        let manifest: Manifest =
            serde_json::from_str(&manifest_json("1.2.3", &[("lh.exe", &sha)])).unwrap();
        assert_eq!(manifest.version, "1.2.3");
        assert_eq!(manifest.artifacts.len(), 1);
        assert_eq!(manifest.artifacts[0].name, "lh.exe");
        assert_eq!(manifest.artifacts[0].sha256, sha);
        validate_manifest(&manifest).unwrap();
    }

    // -- plan case 5: sha256 missing → Err (review note 7) -------------------

    #[test]
    fn manifest_missing_sha256_rejected() {
        // field is not Option → parse fails naming the missing field
        let err = serde_json::from_str::<Manifest>(
            r#"{"version": "1", "artifacts": [{"name": "lh.exe"}]}"#,
        )
        .unwrap_err();
        assert!(err.to_string().contains("sha256"), "unexpected error: {err}");
        // present but empty → validation error
        let manifest: Manifest = serde_json::from_str(
            r#"{"version": "1", "artifacts": [{"name": "lh.exe", "sha256": ""}]}"#,
        )
        .unwrap();
        assert!(matches!(
            validate_manifest(&manifest),
            Err(UpdateError::Manifest(_))
        ));
    }

    #[test]
    fn manifest_invalid_entries_rejected() {
        // sha256 not 64 hex chars
        let bad: Manifest = serde_json::from_str(
            r#"{"version": "1", "artifacts": [{"name": "lh.exe", "sha256": "zz"}]}"#,
        )
        .unwrap();
        assert!(validate_manifest(&bad).is_err());
        // no artifacts at all
        let empty: Manifest =
            serde_json::from_str(r#"{"version": "1", "artifacts": []}"#).unwrap();
        assert!(validate_manifest(&empty).is_err());
        // duplicate artifact name
        let sha = "a".repeat(64);
        let dup: Manifest = serde_json::from_str(&manifest_json(
            "1",
            &[("lh.exe", &sha), ("lh.exe", &sha)],
        ))
        .unwrap();
        assert!(validate_manifest(&dup).is_err());
        // path-ish name must not escape the install dir
        let traversal: Manifest = serde_json::from_str(&manifest_json(
            "1",
            &[("sub/lh.exe", &sha)],
        ))
        .unwrap();
        assert!(validate_manifest(&traversal).is_err());
    }

    // -- plan case 2: version compare (--check / --force) --------------------

    #[test]
    fn version_compare_up_to_date_force_and_newer() {
        let src = tempdir();
        let exe = tempdir();
        let content = b"same-version payload";
        write(src.path(), "lh.exe", content);
        let sha = sha_hex(content);
        let current = current_version().to_string();
        let source = src.path().to_str().unwrap().to_string();

        // manifest == running version, no --force → UpToDate, nothing staged
        write_manifest(src.path(), &current, &[("lh.exe", &sha)]);
        let prepared = prepare_in(&opts(&source), exe.path()).unwrap();
        assert!(matches!(prepared, Prepared::UpToDate { .. }));

        // same version but --force → proceeds to stage
        let mut forced = opts(&source);
        forced.force = true;
        let prepared = prepare_in(&forced, exe.path()).unwrap();
        let Prepared::Ready(p) = &prepared else {
            panic!("expected Ready, got {prepared:?}")
        };
        assert!(!p.check_only);
        assert_eq!(p.artifacts.len(), 1);

        // different version, no --force → Ready carrying the manifest version
        write_manifest(src.path(), "9.9.9", &[("lh.exe", &sha)]);
        let prepared = prepare_in(&opts(&source), exe.path()).unwrap();
        let Prepared::Ready(p) = &prepared else {
            panic!("expected Ready, got {prepared:?}")
        };
        assert_eq!(p.manifest_version, "9.9.9");
    }

    // -- plan case 3: resolve_source (env + file, flag precedence) -----------

    #[test]
    fn resolve_source_precedence_flag_env_file() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(PoisonError::into_inner);
        // SAFETY: mutating the process env; ENV_LOCK serializes this test and
        // no other test reads LH_UPDATE_SOURCE (they pass an explicit source).
        unsafe { std::env::remove_var(ENV_UPDATE_SOURCE) };

        let exe = tempdir();
        // nothing configured anywhere → SourceMissing
        assert!(matches!(
            resolve_source(&UpdateOpts::default(), exe.path()),
            Err(UpdateError::SourceMissing)
        ));

        // file level: <exe_dir>/update.json {"source": ...}
        write(
            exe.path(),
            UPDATE_JSON,
            br#"{"source": "C:/from-update-json"}"#,
        );
        assert_eq!(
            resolve_source(&UpdateOpts::default(), exe.path()).unwrap(),
            "C:/from-update-json"
        );

        // env beats update.json; an empty env value is ignored
        unsafe { std::env::set_var(ENV_UPDATE_SOURCE, "C:/from-env") };
        assert_eq!(
            resolve_source(&UpdateOpts::default(), exe.path()).unwrap(),
            "C:/from-env"
        );
        unsafe { std::env::set_var(ENV_UPDATE_SOURCE, "   ") };
        assert_eq!(
            resolve_source(&UpdateOpts::default(), exe.path()).unwrap(),
            "C:/from-update-json"
        );

        // explicit flag beats everything
        let flagged = UpdateOpts {
            source: Some("C:/from-flag".to_string()),
            ..UpdateOpts::default()
        };
        assert_eq!(resolve_source(&flagged, exe.path()).unwrap(), "C:/from-flag");

        // leave the env clean for the rest of the harness
        unsafe { std::env::remove_var(ENV_UPDATE_SOURCE) };
    }

    // -- plan case 4: FileSource e2e -----------------------------------------

    #[test]
    fn file_source_e2e_stages_verified_artifacts() {
        let src = tempdir();
        let exe = tempdir();
        let content = b"fresh lh.exe payload";
        write(src.path(), "lh.exe", content);
        write_manifest(src.path(), "2.0.0", &[("lh.exe", &sha_hex(content))]);

        let prepared = prepare_in(&opts(src.path().to_str().unwrap()), exe.path()).unwrap();
        let Prepared::Ready(p) = &prepared else {
            panic!("expected Ready, got {prepared:?}")
        };
        assert_eq!(p.manifest_version, "2.0.0");
        assert!(!p.check_only);
        assert_eq!(p.artifacts.len(), 1);
        let artifact = &p.artifacts[0];
        assert_eq!(artifact.name, "lh.exe");
        assert_eq!(artifact.staged_path.parent().unwrap(), exe.path());
        let staged_name = artifact.staged_path.file_name().unwrap().to_str().unwrap();
        assert!(
            staged_name.starts_with(STALE_NEW_PREFIX) && staged_name.ends_with("-lh.exe"),
            "unexpected staged name: {staged_name}"
        );
        assert_eq!(fs::read(&artifact.staged_path).unwrap(), content);
    }

    // -- plan case 6: sha256 mismatch → abort + temps removed -----------------

    #[test]
    fn sha256_mismatch_aborts_and_cleans_temp() {
        let src = tempdir();
        let exe = tempdir();
        let good = b"trusted bytes";
        write(src.path(), "lh.exe", good);
        // first artifact stages fine, the second one mismatches
        write_manifest(
            src.path(),
            "2.0.0",
            &[
                ("lh.exe", &sha_hex(good)),
                ("lh-gui.exe", &sha_hex(b"different-bytes")),
            ],
        );
        write(src.path(), "lh-gui.exe", b"tampered payload");

        let err = prepare_in(&opts(src.path().to_str().unwrap()), exe.path()).unwrap_err();
        assert!(
            matches!(err, UpdateError::Verify { .. }),
            "expected Verify, got: {err}"
        );
        // abort removed every staged temp — nothing half-staged remains and no
        // exe was touched (prepare never touches targets)
        let leftovers = lh_temp_files(exe.path());
        assert!(leftovers.is_empty(), "leftovers: {leftovers:?}");
    }

    // -- plan case 7: swap_all success on fake files --------------------------

    #[test]
    fn swap_all_replaces_target_and_cleans_temp() {
        let exe = tempdir();
        write(exe.path(), "lh.exe", b"old image");
        let prepared = ready(exe.path(), vec![("lh.exe", Some(b"new image".to_vec()))]);

        let report = swap_all(prepared).unwrap();
        assert!(!report.up_to_date);
        assert_eq!(report.updated, vec!["lh.exe"]);
        assert!(report.skipped.is_empty());
        assert_eq!(fs::read(exe.path().join("lh.exe")).unwrap(), b"new image");
        let leftovers = lh_temp_files(exe.path());
        assert!(leftovers.is_empty(), "leftovers: {leftovers:?}");
        assert_eq!(report.from, "0.1.0");
        assert_eq!(report.to, "0.2.0");
        assert_eq!(report.path, exe.path());
    }

    // -- plan case 8: rename-in failure → original intact (rollback) ----------

    #[test]
    fn swap_all_missing_stage_rolls_back_original() {
        let exe = tempdir();
        write(exe.path(), "lh.exe", b"original bytes");
        // staged temp deliberately absent → rename-in must fail and roll back
        let prepared = ready(exe.path(), vec![("lh.exe", None)]);

        let err = swap_all(prepared).unwrap_err();
        assert!(
            matches!(err, UpdateError::Swap { .. }),
            "expected Swap, got: {err}"
        );
        assert_eq!(
            fs::read(exe.path().join("lh.exe")).unwrap(),
            b"original bytes"
        );
        let leftovers = lh_temp_files(exe.path());
        assert!(leftovers.is_empty(), "leftovers: {leftovers:?}");
    }

    // -- review note 1: a late failure rolls back the WHOLE batch, newest first

    #[test]
    fn swap_all_batch_failure_rolls_back_all_in_reverse() {
        let exe = tempdir();
        write(exe.path(), "lh.exe", b"old main");
        write(exe.path(), "lh-gui.exe", b"old gui");
        write(exe.path(), "lh-daemon.exe", b"old daemon");
        let prepared = ready(
            exe.path(),
            vec![
                ("lh.exe", Some(b"new main".to_vec())),
                ("lh-gui.exe", Some(b"new gui".to_vec())),
                ("lh-daemon.exe", None), // staged temp missing → batch aborts here
            ],
        );

        let err = swap_all(prepared).unwrap_err();
        assert!(
            matches!(err, UpdateError::Swap { .. }),
            "expected Swap, got: {err}"
        );
        // every target of the batch is back to its original bytes
        assert_eq!(fs::read(exe.path().join("lh.exe")).unwrap(), b"old main");
        assert_eq!(fs::read(exe.path().join("lh-gui.exe")).unwrap(), b"old gui");
        assert_eq!(
            fs::read(exe.path().join("lh-daemon.exe")).unwrap(),
            b"old daemon"
        );
        let leftovers = lh_temp_files(exe.path());
        assert!(leftovers.is_empty(), "leftovers: {leftovers:?}");
    }

    // -- plan §3.3 step 8: artifacts without an existing target are skipped ---

    #[test]
    fn swap_all_skips_artifacts_without_existing_target() {
        let exe = tempdir();
        write(exe.path(), "lh.exe", b"old main");
        // lh-gui.exe target missing → skipped, its staged temp is dropped
        let prepared = ready(
            exe.path(),
            vec![
                ("lh.exe", Some(b"new main".to_vec())),
                ("lh-gui.exe", Some(b"new gui".to_vec())),
            ],
        );

        let report = swap_all(prepared).unwrap();
        assert_eq!(report.updated, vec!["lh.exe"]);
        assert_eq!(report.skipped, vec!["lh-gui.exe"]);
        assert_eq!(fs::read(exe.path().join("lh.exe")).unwrap(), b"new main");
        let leftovers = lh_temp_files(exe.path());
        assert!(leftovers.is_empty(), "leftovers: {leftovers:?}");
    }

    // -- plan case 9: cleanup_stale auto-restore ------------------------------

    #[test]
    fn cleanup_stale_restores_missing_exe_from_newest_old() {
        let exe = tempdir();
        let older = write(exe.path(), ".lh-old-100-lh.exe", b"older image");
        let newest = write(exe.path(), ".lh-old-200-lh.exe", b"newest image");
        let now = SystemTime::now();
        set_mtime(&older, now - Duration::from_secs(60));
        set_mtime(&newest, now - Duration::from_secs(10));

        cleanup_stale_for(exe.path(), "lh.exe");

        // the newest .lh-old was renamed back onto the missing target
        assert_eq!(fs::read(exe.path().join("lh.exe")).unwrap(), b"newest image");
        // main binary restored → remaining stale files are deleted
        let leftovers = lh_temp_files(exe.path());
        assert!(leftovers.is_empty(), "leftovers: {leftovers:?}");
    }

    // -- plan case 10: cleanup_stale deletes stale files (unlocked) -----------

    #[test]
    fn cleanup_stale_deletes_stale_when_main_present_and_unlocked() {
        let exe = tempdir();
        write(exe.path(), "lh.exe", b"current image");
        write(exe.path(), ".lh-old-7-lh.exe", b"old image");
        write(exe.path(), ".lh-new-7-lh-gui.exe", b"staged gui");

        cleanup_stale_for(exe.path(), "lh.exe");

        assert_eq!(
            fs::read(exe.path().join("lh.exe")).unwrap(),
            b"current image"
        );
        let leftovers = lh_temp_files(exe.path());
        assert!(leftovers.is_empty(), "leftovers: {leftovers:?}");
    }
}
