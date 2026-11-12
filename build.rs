//! Embeds the exe icon and version info (shown in Explorer and on shortcuts).

fn main() {
    println!("cargo:rerun-if-changed=assets/icon.ico");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        winresource::WindowsResource::new()
            .set_icon("assets/icon.ico")
            .set("ProductName", "OTIUM")
            .set("FileDescription", "OTIUM — быстрый браузер")
            .set("LegalCopyright", "© SoFTeR1")
            .compile()
            .expect("embedding the icon");
    }
}
