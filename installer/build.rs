//! The installer embeds the release build of OTIUM; `OTIUM_EXE` points at it (see build.ps1).

fn main() {
    println!("cargo:rerun-if-env-changed=OTIUM_EXE");
    let exe = std::env::var("OTIUM_EXE").expect("set OTIUM_EXE to the built otium.exe (use build.ps1)");
    println!("cargo:rerun-if-changed={exe}");
    println!("cargo:rerun-if-changed=../assets/icon.ico");
    winresource::WindowsResource::new()
        .set_icon("../assets/icon.ico")
        .set("ProductName", "OTIUM Setup")
        .set("FileDescription", "Установка OTIUM")
        .set("LegalCopyright", "© SoFTeR1")
        .compile()
        .expect("embedding the icon");
}
