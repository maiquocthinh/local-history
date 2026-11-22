# local-history

Automatic, **Git-independent file history** — JetBrains Local History style.
Run the daemon once: every file change in your project is recorded as a
version, whoever makes it (editor, script, tool) — even while the editor is
closed. Undo a change from 5 days ago. Not a VCS, never replaces Git.

## Concept

- **A daemon watches your project** and snapshots every change with a ~400ms
  debounce, deduplicated by content hash — no junk versions from no-op saves.
- **Everything is stored outside your project**, in a local SQLite store with
  zstd-compressed deltas — a 100KB file with 3 changed lines costs ~1–2KB per
  version.
- **History follows renames**: rename a file and its whole version chain moves
  with it, one `rename` version marks the move.
- **Revert is always safe**: a snapshot is recorded *before* overwriting, so a
  revert can never lose data.
- **Batches group related changes**: files edited close in time form a batch
  you can review like a PR page and undo with one command.
- **Old history is garbage-collected** the JetBrains way: 5 working days +
  the newest 50 versions per file are always kept.

## Usage

```bash
cargo build --release

# start the watcher for a project (background + system tray icon)
target/release/lh daemon --tray --dir D:\path\to\project

# open the GUI (bare lh spawns it detached)
target/release/lh

# or open one file's timeline
target/release/lh ui D:\path\to\project\src\main.rs
```

That's it. Work as usual — the GUI shows the timeline; click a version for a
side-by-side diff, or hit **Revert to this version** to restore in one click.

## CLI (15 commands, all with `--json`)

| Command | What it does |
|---|---|
| `lh status` | daemon status + workspaces |
| `lh history <file>` | one file's timeline |
| `lh show <file> --at 2\|latest` | print a version's content |
| `lh diff <file> 1 2\|current` | unified diff between two versions |
| `lh revert <file> --at 1 --yes` | restore a file to an old version |
| `lh label <file> "before refactor"` | attach a semantic label |
| `lh batches` / `lh batch <id>` / `lh revert-batch <id> --yes` | inspect / revert a whole batch |
| `lh gc [--dry-run]` | prune old history |
| `lh workspaces` | workspaces registered in the store |
| `lh ui <file>` / `lh gui` | open the GUI (detached / foreground) |
| `lh daemon --tray --dir <path>` | start the watcher (`--dir` is repeatable) |
| `lh self-update [--check\|--force\|--restart]` | update in place |

## GUI

Workspace picker → file list → timeline (grouped by day, filterable) →
side-by-side diff with synced scrolling, soft-wrap and unchanged-region
collapsing. The **Activity** tab shows batches like a PR page with one-click
whole-batch revert. Right-click inside the diff to copy selection, a pane, or
the patch.

## Storage

```
%LOCALAPPDATA%\local-history          # default store root (env: LOCAL_HISTORY_DIR)
├── index.db                          # SQLite (WAL) — versions, files, workspaces
└── workspaces/<id>/blobs/*.zst       # content snapshots, zstd, write-once
```

One install = one binary (`gh`/`uv` model): CLI, daemon and GUI share the same
exe, and `lh self-update` keeps it current from a self-hosted manifest source.

## Testing

```bash
cargo test --workspace
lh-e2e
lh-e2e --suite smoke
cargo test -p gui --test kittest
```

Green on Windows and Linux.

## Non-goals

Not a Git replacement (no branch/merge/commit), nothing tracked outside
registered workspaces, no history for files over 10MB, no encryption or
cross-machine sync.

## Build

Rust stable MSVC + VS Build Tools (rusqlite and zstd need a C compiler).
The GUI uses the Glow (OpenGL) renderer — more stable than wgpu on VMs.
