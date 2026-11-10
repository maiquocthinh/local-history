//! Windows-only resources: embed the shared `app.ico` as the executable icon.
//! Non-Windows builds compile to nothing.

fn main() {
    if std::env::var("CARGO_CFG_WINDOWS").is_ok() {
        winresource::WindowsResource::new()
            .set_icon("../gui/assets/app.ico")
            .compile()
            .expect("embed app.ico as the lh-daemon executable icon");
    }
}
