//! Windows-only resources: embed `assets/app.ico` as the executable icon
//! (visible in Explorer and on the taskbar). Non-Windows builds compile to nothing.

fn main() {
    if std::env::var("CARGO_CFG_WINDOWS").is_ok() {
        winresource::WindowsResource::new()
            .set_icon("assets/app.ico")
            .compile()
            .expect("embed app.ico as the lh-gui executable icon");
    }
}
