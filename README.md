# local-history

Lịch sử thay đổi file tự động, độc lập với Git — tính năng kiểu **JetBrains Local History**,
đang được xây cho **Wu** (fork của Zed, Rust native, không Electron).

> **Định vị:** công cụ cứu hộ khẩn cấp ("undo thay đổi 5 ngày trước"), không thay thế VCS.

## Thành phần

| Crate | Binary | Vai trò |
|---|---|---|
| `crates/history` | — (lib) | Engine: lưu version dạng delta nén, tái tạo nội dung, diff, revert, GC |
| `crates/daemon` | `lh-daemon` | Watcher daemon — bắt mọi thay đổi trên disk (save, external edit, cả khi editor đóng) |
| `crates/cli` | `lh` | CLI: timeline, diff, revert, label, gc |
| `crates/gui` | `lh-gui` | App desktop (egui): xem timeline + diff 2 bên + revert 1 click |

## Trạng thái

✅ **Phase 1 hoàn tất** — engine + daemon + CLI + GUI chạy độc lập, 74 tests xanh, clippy sạch.
Công cụ đứng ngoài mọi editor: không cần sửa hay build lại Wu/Zed/VS Code — chỉ cần chạy
daemon và mọi thay đổi trong thư mục project đều được ghi lịch sử.

## Quickstart

```sh
cargo build --release -p cli -p daemon -p gui
# hoặc chạy từ target\debug khi dev

# 1) Bật watcher cho project (mặc định watch cwd)
lh-daemon --dir D:\path\to\project

# 2) Xem timeline / diff / revert qua CLI
lh history D:\path\to\project\src\main.rs
lh diff D:\path\to\project\src\main.rs 1 3
lh revert D:\path\to\project\src\main.rs --at 2

# 3) GUI
lh-gui                          # mở viewer
lh-gui --file <đường-dẫn-file>  # nhảy thẳng timeline của file
lh ui                           # hoặc từ CLI

# Dọn lịch sử cũ (mặc định giữ 5 ngày làm việc, tối thiểu 50 version/file)
lh gc --dry-run
```

Store nằm ở `%LOCALAPPDATA%\local-history` (override: env `LOCAL_HISTORY_DIR`) — không lưu
gì trong project của bạn. Smoke test end-to-end: `powershell -ExecutionPolicy Bypass -File scripts\smoke.ps1`.

## Tài liệu

- [`docs/goals.md`](docs/goals.md) — mục tiêu dự án, vì sao không làm extension, phạm vi
- [`docs/architecture.md`](docs/architecture.md) — kiến trúc chi tiết: watcher, storage, delta
  format, retention, API, tech stack
- [`docs/plan.md`](docs/plan.md) — plan thực hiện phase 1 theo milestone + tiến độ

## Build

```sh
cargo build --release
```

Yêu cầu: Rust stable (MSVC trên Windows) + VS Build Tools cho các crate C
(`rusqlite` bundled, `zstd`).
