# local-history

Lịch sử thay đổi file **tự động, độc lập với Git** — kiểu JetBrains Local History.
Bật daemon là xong: mọi file trong project sửa bởi bất kỳ ai (editor, script, tool)
đều được ghi thành version — kể cả khi editor đã đóng. Undo được cả thay đổi 5 ngày
trước. Không phải VCS, không thay Git.

## Dùng

```bat
cargo build --release

:: bật watcher cho project (chạy nền, icon trong system tray)
target\release\lh daemon --tray --dir D:\path\to\project

:: mở GUI (bare lh = spawn GUI detached)
target\release\lh

:: hoặc xem timeline của 1 file
target\release\lh ui D:\path\to\project\src\main.rs
```

Xong. Cứ làm việc bình thường — GUI hiện timeline, click chọn version để xem
diff side-by-side, nút **Revert to this version** khôi phục 1 click.

## CLI (15 lệnh, đều có `--json`)

| Lệnh | Việc |
|---|---|
| `lh status` | trạng thái daemon + workspaces |
| `lh history <file>` | timeline 1 file |
| `lh show <file> --at 2\|latest` | in nội dung 1 version |
| `lh diff <file> 1 2\|current` | unified diff 2 version |
| `lh revert <file> --at 1 --yes` | khôi phục 1 file về version cũ |
| `lh label <file> "before refactor"` | gắn nhãn ngữ nghĩa |
| `lh batches` / `lh batch <id>` / `lh revert-batch <id> --yes` | xem / revert **cả đợt** thay đổi |
| `lh gc [--dry-run]` | dọn history cũ |
| `lh workspaces` | danh sách workspace trong store |
| `lh ui <file>` / `lh gui` | mở GUI (detached / foreground) |
| `lh daemon --tray --dir <path>` | bật watcher (`--dir` lặp được) |
| `lh self-update [--check\|--force\|--restart]` | tự cập nhật |

Chú ý nhanh: `lh daemon` không `--dir` → watch thư mục hiện tại; chạy console thay tray
thì bỏ `--tray` (Ctrl+C để dừng). Menu tray: Open GUI · Pause/Resume · Clean up history ·
Cập nhật & restart daemon · Quit.

## Nó làm được gì

- **Bắt mọi thay đổi** — daemon nghe native fs events, debounce ~400ms, dedup theo
  content hash (blake3): save không đổi gì không sinh version rác. Bỏ qua
  `.git`, `target`, `node_modules`, `dist`, `build`…
- **Delta nén zstd** — file 100KB sửa 3 dòng ≈ 1–2KB mỗi version; binary file chỉ lưu
  full; cap 10MB/file.
- **Rename giữ nguyên history** — file đổi tên thì cả chain version theo sang tên mới
  (1 version `rename` đánh dấu, label = tên cũ). Rename đè lên file đã track → 2 chain
  tách rời, không mất gì.
- **Batch** — các file sửa gần nhau trong thời gian gom thành 1 đợt, xem cả đợt như PR
  page, revert cả đợt 1 lệnh (file sửa sau batch được giữ nguyên, không bị ghi đè).
- **GC kiểu JetBrains** — giữ 5 ngày làm việc (Mon–Fri) + tối thiểu 50 version mới nhất
  mỗi file; daemon dọn lúc start + mỗi 6 giờ; dọn cả blob mồ côi.
- **Revert an toàn** — luôn ghi version `pre_revert` TRƯỚC khi ghi đè → không bao giờ
  mất dữ liệu. No-op revert (đĩa đã đúng nội dung) không sinh version.
- **Self-update nội bộ** — tải manifest đa artifact + sha256 bắt buộc, swap rename
  metadata-only, lỗi → rollback; crash giữa chừng → lần chạy sau tự khôi phục. Nguồn:
  flag `--source` > env `LH_UPDATE_SOURCE` > `update.json` cạnh exe.
  Publish bản mới: `powershell -File scripts\publish-update.ps1 -Source D:\dist\lh`.
- **i18n vi/en** — CLI `--lang en|vi`; GUI đổi trong app. Lưu không đè project của bạn.

## GUI

Chọn workspace (combo trái) → file list → timeline (nhóm theo ngày, badge kind/label,
filter theo kind + label) → diff side-by-side scroll đồng bộ, soft-wrap, thu gọn đoạn
không đổi, so với version bất kỳ hoặc disk hiện tại. Tab **Activity**: batch view kiểu
PR + revert cả đợt. Right-click trong diff: copy selection / pane / patch.

## Storage & thiết kế

Store mặc định `%LOCALAPPDATA%\local-history` (env `LOCAL_HISTORY_DIR`, hoặc
`lh --store <PATH>`):

```
<root>/index.db                        # SQLite (WAL)
<root>/workspaces/<ws_id>/blobs/*.zst  # full/delta nén zstd, ghi một lần
```

Một bản cài = **một exe duy nhất** (mô hình `gh`/`uv`) — CLI, daemon, GUI nhúng chung.
Workspace id = `blake3(canonical project root)`; 1 daemon duy nhất mỗi store (lock).
Chi tiết: [`docs/architecture.md`](docs/architecture.md) · [`docs/goals.md`](docs/goals.md).

## Kiểm thử

```bat
cargo test --workspace                          :: unit + integration (~200 tests)
lh-e2e --lh target\release\lh                   :: 29 scenario e2e (daemon + CLI thật)
lh-e2e --suite smoke                            :: nhanh: 12 scenario gốc
lh-e2e --suite smoke --json                     :: summary JSON cho CI
cargo test -p gui --test kittest                :: 13 test GUI headless
env LH_E2E_KEEP=1 lh-e2e ...                    :: giữ tempdir khi fail để debug
```

e2e phủ: rename giữ chain + collision, debounce coalesce, kill -9 giữa burst +
respawn, blob hỏng → lỗi sạch, delta rollover (21 edit byte-exact từng seq),
concurrent writers, stale lock takeover, orphan blob GC, cap 10MB, file
rỗng/NUL/CRLF/unicode, đủ 15 lệnh CLI, self-update (swap/rollback/noop/missing
artifact). Chạy xanh trên **Windows và Linux**.

## Non-goals

- Không thay Git — không branch/merge/remote/commit.
- Không track ngoài workspace đã đăng ký; không lịch sử file > 10MB.
- Không mã hóa, không sync lịch sử giữa các máy.

## Build

Rust stable MSVC + VS Build Tools (rusqlite bundled + zstd cần C compiler).
Chạy trên Windows và Linux; GUI dùng renderer Glow (OpenGL) — ổn hơn wgpu trên
máy ảo / GPU hạn chế.
