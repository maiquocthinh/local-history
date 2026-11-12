//! lh-gui — local-history desktop viewer (egui/eframe), dark theme.
//!
//! Thin binary wrapper: all logic lives in the `gui` library (see `lib.rs`),
//! which is also embedded by the multi-call `lh` binary (crate `cli`).
//!
//! Usage: `lh-gui [--file <path>]` — `--file` opens straight into that file's
//! timeline (the contract for editors launching the GUI).

fn main() -> eframe::Result<()> {
    let args = gui::parse_args(
        std::env::args_os()
            .skip(1)
            .map(|a| a.to_string_lossy().into_owned()),
    );
    gui::run(args.file.as_deref().map(gui::normalize_user_path))
}
