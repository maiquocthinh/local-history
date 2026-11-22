# local-history

Automatic, **Git-independent file history** — JetBrains Local History style.
Run the daemon once: every file change in your project is recorded as a
version, whoever makes it (editor, script, tool) — even while the editor is
closed. Undo a change from 5 days ago. Not a VCS, never replaces Git.

## Usage

```bash
cargo build --release

:: start the watcher for a project (background + system tray icon)
target\release\lh daemon --tray --dir D:\path\to\project

:: open the GUI (bare lh spawns it detached)
target\release\lh

:: or open one file's timeline
target\release\lh ui D:\path\to\project\src\main.rs
```

That's it. Work as usual — the GUI shows the timeline; click a version to see
a side-by-side diff, or hit **Revert to this version** to restore in one click.

## CLI (15 commands, all with `--json`)

| Command | What it does |
|---|---|
| `lh status` | daemon status + workspaces |
| `lh history <file>` | one file's timeline |
| `lh show <file> --at 2\|latest` | print a version's content |
| `lh diff <file> 1 2\|current` | unified diff between two versions |
| `lh revert <file> --at 1 --yes` | restore a file to an old version |
| `lh label <file> "before refactor"` | attach a semantic label |
| `lh batches` / `lh batch <id>` / `lh revert-batch <id> --yes` | inspect / revert a whole **batch** |
| `lh gc [--dry-run]` | prune old history |
| `lh workspaces` | workspaces registered in the store |
| `lh ui <file>` / `lh gui` | open the GUI (detached / foreground) |
| `lh daemon --tray --dir <path>` | start the watcher (`--dir` is repeatable) |
| `lh self-update [--check\|--force\|--restart]` | update in place |

Quick notes: `lh daemon` without `--dir` watches the current directory; run in
the console instead of the tray by dropping `--tray` (Ctrl+C to stop). Tray
menu: Open GUI · Pause/Resume · Clean up history · Update & restart daemon · Quit.

## What it does

- **Catches every change** — the daemon listens to native fs events, debounces
  ~400ms, dedups by content hash (blake3): a save with no content change never
  creates a junk version. Ignores `.git`, `target`, `node_modules`,
  `dist`, `build`…
- **zstd-compressed deltas** — a 100KB file with 3 changed lines ≈ 1–2KB per
  version; binary files store full snapshots only; 10MB per-file cap.
- **Rename keeps the history** — renaming a file moves its whole version chain
  to the new path (one `rename` version marks the move, labeled with the old
  name). Renaming onto a tracked file keeps the two chains separate — nothing
  is lost.
- **Batches** — files changed close in time are grouped into one batch: view
  it like a PR page, revert the whole batch with one command (files edited
  after the batch are kept, never clobbered).
- **JetBrains-style GC** — keeps 5 working days (Mon–Fri) + at least the newest
  50 versions per file; the daemon prunes at start and every 6 hours; orphan
  blobs are collected too.
- **Safe revert** — always records a `pre_revert` snapshot BEFORE overwriting,
  so it can never lose data. A no-op revert (disk already matches) records
  nothing.
- **In-place self-update** — fetches a multi-artifact manifest with mandatory
  sha256 verification, swaps via metadata-only renames, rolls back on error;
  a crash mid-update self-heals on the next start. Source: `--source` flag >
  `LH_UPDATE_SOURCE` env > `update.json` next to the exe. To publish a release:
  `powershell -File scripts\publish-update.ps1 -Source D:\dist\lh`.
- **i18n (en/vi)** — CLI `--lang en|vi`; the GUI switches language in-app.
  Nothing is stored inside your project.

## GUI

Pick a workspace (left combo) → file list → timeline (grouped by day, kind and
label badges, filter by kind + label) → side-by-side diff with synced scrolling,
soft-wrap, unchanged-region collapsing, compare against any version or the
current disk content. The **Activity** tab: PR-style batch view + whole-batch
revert. Right-click inside the diff: copy selection / pane / patch.

## Storage & design

Default store root `%LOCALAPPDATA%\local-history` (env `LOCAL_HISTORY_DIR`, or
`lh --store <PATH>`):

```
<root>/index.db                        # SQLite (WAL)
<root>/workspaces/<ws_id>/blobs/*.zst  # full/delta snapshots, zstd, write-once
```

One install = **one binary** (`gh`/`uv` model) — CLI, daemon and GUI share the
same exe. Workspace id = `blake3(canonical project root)`; one daemon per store
(lock file). Details: [`docs/architecture.md`](docs/architecture.md) ·
[`docs/goals.md`](docs/goals.md).

## Testing

```bash
cargo test --workspace                          # unit + integration (~200 tests)
lh-e2e --lh target\release\lh                   # 29 e2e scenarios (real daemon + CLI)
lh-e2e --suite smoke                            # fast: the original 12
lh-e2e --suite smoke --json                     # JSON summary for CI
cargo test -p gui --test kittest                # 13 headless GUI tests
env LH_E2E_KEEP=1 lh-e2e ...                    # keep temp dirs on failure for debugging
```

e2e covers: rename keeps the chain + collision fallback, debounce coalescing,
kill -9 mid-burst + respawn, corrupted blob → clean error, delta rollover
(21 edits, byte-exact at every seq), concurrent writers, stale lock takeover,
orphan blob GC, the 10MB cap, empty/NUL/CRLF/unicode files, all 15 CLI
commands, self-update (swap/rollback/noop/missing artifact). Green on both
**Windows and Linux**.

## Non-goals

- Not a Git replacement — no branch/merge/remote/commit.
- Nothing tracked outside registered workspaces; no history for files > 10MB.
- No encryption, no cross-machine history sync.

## Build

Rust stable MSVC + VS Build Tools (rusqlite bundled and zstd need a C
compiler). Runs on Windows and Linux; the GUI uses the Glow (OpenGL) renderer —
more stable than wgpu on VMs / limited GPUs.
