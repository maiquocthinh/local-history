//! Windows-only resources: embed the shared app icon (`../gui/assets/app.ico`)
//! as the `lh.exe` executable icon (visible in Explorer and on the taskbar) —
//! same pattern as `crates/gui/build.rs`, so the multi-call binary keeps an
//! icon when launched directly (bare `lh` opens the GUI). Non-Windows builds
//! compile to nothing.

fn main() {
    if std::env::var("CARGO_CFG_WINDOWS").is_ok() {
        winresource::WindowsResource::new()
            .set_icon("../gui/assets/app.ico")
            .compile()
            .expect("embed ../gui/assets/app.ico as the lh executable icon");
    }
}
