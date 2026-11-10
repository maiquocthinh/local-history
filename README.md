# local-history

Lịch sử thay đổi file **tự động và độc lập với Git**, tính năng kiểu **JetBrains Local History**:
mỗi lần file trong project thay đổi — bạn save, tool/script ghi đè, hay chương trình khác sửa từ
ngoài — một phiên bản mới được ghi lại vào store riêng. Tool đứng **ngoài mọi editor**: chỉ cần
bật daemon là mọi thay đổi trên disk đều được ghi, kể cả khi editor đóng. Định vị là **cứu hộ
khẩn cấp** ("undo thay đổi 5 ngày trước"), không phải VCS và không thay Git.

Phase 1 hoàn tất: 4 binary chạy độc lập, 101 test pass (`cargo test --workspace`), smoke
end-to-end xanh (`scripts\smoke.ps1`).

## Vì sao là app đứng ngoài, không phải extension?

Hệ extension của editor (Zed/Wu) là WASM sandbox và không cấp đủ ba khả năng mà tính năng này
cần: hook save event, render panel timeline + diff, và watch filesystem / revert file — tất cả
đều ngoài sandbox. Nên phải có thành phần chạy ngoài sandbox; phase 1 chọn hướng hoàn toàn độc
lập (không sửa editor), dùng được với mọi editor. Chi tiết quyết định kiến trúc:
[`docs/goals.md`](docs/goals.md).

## Thành phần

| Crate | Binary | Vai trò |
|---|---|---|
| `crates/history` | — (lib) | Engine: version chain (full + delta nén zstd), reconstruct, diff, revert, label, GC |
| `crates/daemon` | `lh-daemon` | Watcher daemon (native fs events + debounce), chạy console hoặc `--tray` trong system tray |
| `crates/cli` | `lh` | CLI: `status`, `history`, `show`, `diff`, `revert`, `label`, `gc`, `workspaces`, `ui` — đều có `--json` |
| `crates/gui` | `lh-gui` | App desktop (egui): chọn workspace + file list, timeline, diff side-by-side, revert 1 click |

## Quickstart

Lệnh dưới đây cho **cmd**; binary nằm trong `target\release` sau khi build.

```bat
cargo build --release

:: 1) Bật watcher cho project — chạy nền với icon trong system tray
target\release\lh-daemon.exe --tray --dir D:\path\to\project

:: 2) CLI hay dùng — trỏ đúng file trong thư mục đã watch
target\release\lh history D:\path\to\project\src\main.rs
target\release\lh diff   D:\path\to\project\src\main.rs 1 2
target\release\lh show   D:\path\to\project\src\main.rs --at 1
target\release\lh label  D:\path\to\project\src\main.rs "before refactor"
target\release\lh revert D:\path\to\project\src\main.rs --at 1 --yes
target\release\lh gc --dry-run
target\release\lh status

:: 3) GUI desktop (hoặc: target\release\lh ui <file> — spawn GUI detached, trả về ngay)
target\release\lh-gui
target\release\lh-gui --file D:\path\to\project\src\main.rs
```

Ghi chú:

- `--dir` lặp được để watch nhiều thư mục; không có `--dir` → watch thư mục hiện tại.
  Chạy console thay vì tray: bỏ `--tray` (dừng bằng Ctrl+C).
- `lh diff <file> <a> <b>`: `b` là seq, hoặc `current` = nội dung disk hiện tại;
  `lh show --at` nhận seq hoặc `latest`.
- Menu tray: Open GUI · Pause/Resume watch · Clean up history (gc) · Quit — double-click icon
  cũng mở GUI.
- Smoke test end-to-end (daemon → record → history → diff → show → revert → gc):
  `powershell -ExecutionPolicy Bypass -File scripts\smoke.ps1`

## Tính năng chính

- **Snapshot tự động 100%** — daemon bắt native fs events (Windows: `ReadDirectoryChangesW`),
  debounce ~400 ms gom event theo path, dedup theo content hash (blake3): save không đổi gì
  không sinh version rác. Bỏ qua `.git`, `.hg`, `.svn`, `target`, `node_modules`, `dist`,
  `build`, `.zcode`.
- **Bắt external changes** — file bị chương trình khác sửa ngoài editor vẫn được ghi lại
  (daemon không phân biệt save/external: mọi thay đổi disk đều như nhau). Đây là điểm phân biệt
  với undo trong editor.
- **Timeline + diff + revert 1 click** — GUI: timeline nhóm theo ngày với badge kind/label,
  diff side-by-side 2 pane scroll đồng bộ, so với version bất kỳ hoặc nội dung disk hiện tại.
  Revert luôn ghi version `pre_revert` TRƯỚC khi ghi đè nên không bao giờ làm mất dữ liệu.
- **Chọn dòng & copy diff** — trong diff view: click/ctrl+click/shift+click chọn dòng,
  Ctrl+C copy raw text; nút "Copy diff" copy unified diff của cặp đang xem ra clipboard.
- **Lưu dạng delta nén zstd** — full snapshot định kỳ + structured patch (delta) nén zstd;
  file 100 KB sửa 3 dòng/lần ≈ 1–2 KB mỗi version. Binary file: chỉ full; diff hiển thị
  "binary file". Cap 10 MB mỗi file.
- **GC kiểu JetBrains** — mặc định giữ **5 ngày làm việc** (Mon–Fri, cuối tuần không tính) và
  **tối thiểu 50 version mới nhất mỗi file**, luôn còn ít nhất 1 full mỗi file; daemon dọn lúc
  start + mỗi 6 giờ; `lh gc --dry-run` để xem trước, dọn thêm cả blob mồ côi.
- **Label ngữ nghĩa** — `lh label <file> "before test"` gắn nhãn sự kiện lên timeline
  (kind=label, chụp nội dung disk hiện tại).
- **i18n vi/en** — CLI: `--lang en|vi`, mặc định theo env `LH_LANG` rồi đến locale của OS;
  GUI đổi ngôn ngữ trong app (lưu vào gui-state.json); menu tray theo cùng luật.
- **System tray** — `lh-daemon --tray` chạy nền không cửa sổ console; pause/resume watch ngay
  từ menu; "Clean up history" chạy gc.

## Storage

Store mặc định: `%LOCALAPPDATA%\local-history` — **không lưu gì trong project của bạn**.

```
<root>/index.db                            # SQLite (WAL) — workspaces, files, versions
<root>/daemon.json                         # pid + workspaces — nguồn cho `lh status`
<root>/daemon.lock                         # chống chạy 2 daemon trên cùng workspace
<root>/workspaces/<ws_id>/blobs/*.zst      # full/delta nén zstd, ghi một lần không sửa
```

Override: env `LOCAL_HISTORY_DIR`, hoặc `lh --store <PATH>`. Workspace id là
`blake3(canonical project root)`. Kiến trúc chi tiết: [`docs/architecture.md`](docs/architecture.md).

## Tài liệu

- [`docs/goals.md`](docs/goals.md) — mục tiêu dự án, vì sao không làm extension, phạm vi phase 1
- [`docs/architecture.md`](docs/architecture.md) — kiến trúc: watcher pipeline, storage/delta
  format, retention, API của crate `history`, tech stack
- [`docs/plan.md`](docs/plan.md) — plan thực hiện phase 1 theo milestone

## Non-goals

- Không thay Git, không thay stash — không branch/merge/remote, không thay thế commit.
- Không track thư mục ngoài workspace đã đăng ký với daemon.
- Không lịch sử cho file > 10 MB (cap tránh phình storage).
- Không mã hóa, không sync lịch sử giữa các máy.

## Build

```bat
cargo build --release
```

Yêu cầu: **Rust stable với MSVC toolchain** trên Windows (`stable-x86_64-pc-windows-msvc`) và
**VS Build Tools** — `rusqlite` (bundled) và `zstd` cần C compiler. Đã chạy và test trên
Windows; GUI dùng renderer Glow (OpenGL) — ổn định hơn wgpu/DX12 trên máy ảo hoặc driver GPU
hạn chế.

Kiểm tra sau khi sửa code:

```bat
cargo test --workspace
powershell -ExecutionPolicy Bypass -File scripts\smoke.ps1
```
