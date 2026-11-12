//! Registering OTIUM with Windows as a web browser, so it shows up in Settings → Default apps
//! and can open http(s) links and .html/.pdf files. Windows 10/11 don't let an app make itself
//! the default silently; we register and then open the Settings page where the user confirms.
//! Only the installed copy (the one next to the installer's marker file) registers itself.

use std::path::{Path, PathBuf};

use winreg::{
    enums::{HKEY_CURRENT_USER, KEY_READ, KEY_SET_VALUE},
    RegKey,
};

pub const APP: &str = "OTIUM Browser";
const PROG_URL: &str = "OtiumBrowserURL";
const PROG_HTML: &str = "OtiumBrowserHTML";
const CLIENT: &str = r"Software\Clients\StartMenuInternet\OTIUM Browser";
const UNINSTALL: &str = r"Software\Microsoft\Windows\CurrentVersion\Uninstall\OTIUM Browser";
pub const MARKER: &str = "otium-browser.install";
const URLS: &[&str] = &["http", "https"];
const FILES: &[&str] = &[".htm", ".html", ".shtml", ".xht", ".xhtml", ".svg", ".pdf"];

/// Path of this exe if it is an installed copy (not a dev or portable build).
pub fn installed_exe() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    exe.parent()?.join(MARKER).exists().then_some(exe)
}

fn open_command(exe: &Path) -> String {
    // --single-argument: everything after it is one link, so a crafted URL can't smuggle flags in
    format!("\"{}\" --single-argument %1", exe.display())
}

/// Write the registration if it is missing or points at another exe/version.
pub fn ensure_registered(exe: &Path, version: &str) {
    let hk = RegKey::predef(HKEY_CURRENT_USER);
    let cmd: Option<String> = hk
        .open_subkey_with_flags(format!(r"Software\Classes\{PROG_URL}\shell\open\command"), KEY_READ)
        .and_then(|k| k.get_value(""))
        .ok();
    let ver: Option<String> = hk.open_subkey_with_flags(UNINSTALL, KEY_READ).and_then(|k| k.get_value("DisplayVersion")).ok();
    let registered = hk.open_subkey_with_flags(r"Software\RegisteredApplications", KEY_READ).and_then(|k| k.get_value::<String, _>(APP)).is_ok();
    if registered && cmd.as_deref() == Some(open_command(exe).as_str()) && ver.as_deref().is_none_or(|v| v == version) {
        return;
    }
    if let Err(e) = register(&hk, exe, version) {
        crate::native::log::warn(&format!("browser registration failed: {e}"));
    }
}

fn register(hk: &RegKey, exe: &Path, version: &str) -> std::io::Result<()> {
    let e = exe.display().to_string();
    let icon = format!("\"{e}\",0");
    let open = open_command(exe);

    for (prog, desc) in [(PROG_URL, "OTIUM Browser URL"), (PROG_HTML, "OTIUM Browser HTML Document")] {
        let base = format!(r"Software\Classes\{prog}");
        let (k, _) = hk.create_subkey(&base)?;
        k.set_value("", &desc)?;
        k.set_value("FriendlyTypeName", &desc)?;
        hk.create_subkey(format!(r"{base}\DefaultIcon"))?.0.set_value("", &icon)?;
        hk.create_subkey(format!(r"{base}\shell\open\command"))?.0.set_value("", &open)?;
        let (a, _) = hk.create_subkey(format!(r"{base}\Application"))?;
        a.set_value("ApplicationName", &APP)?;
        a.set_value("ApplicationIcon", &icon)?;
    }

    let (c, _) = hk.create_subkey(CLIENT)?;
    c.set_value("", &APP)?;
    hk.create_subkey(format!(r"{CLIENT}\DefaultIcon"))?.0.set_value("", &icon)?;
    hk.create_subkey(format!(r"{CLIENT}\shell\open\command"))?.0.set_value("", &format!("\"{e}\""))?;
    let (cap, _) = hk.create_subkey(format!(r"{CLIENT}\Capabilities"))?;
    cap.set_value("ApplicationName", &APP)?;
    cap.set_value("ApplicationDescription", &"Быстрый и лёгкий браузер на Rust")?;
    cap.set_value("ApplicationIcon", &icon)?;
    hk.create_subkey(format!(r"{CLIENT}\Capabilities\StartMenu"))?.0.set_value("StartMenuInternet", &APP)?;
    let (u, _) = hk.create_subkey(format!(r"{CLIENT}\Capabilities\URLAssociations"))?;
    for s in URLS {
        u.set_value(s, &PROG_URL)?;
    }
    let (f, _) = hk.create_subkey(format!(r"{CLIENT}\Capabilities\FileAssociations"))?;
    for x in FILES {
        f.set_value(x, &PROG_HTML)?;
    }
    for x in FILES {
        // offer OTIUM in the "Open with" list too
        hk.create_subkey(format!(r"Software\Classes\{x}\OpenWithProgids"))?.0.set_value(PROG_HTML, &"")?;
    }
    hk.create_subkey(r"Software\RegisteredApplications")?.0.set_value(APP, &format!(r"{CLIENT}\Capabilities"))?;

    // keep "Installed apps" showing the right version after self-updates
    if let Ok(k) = hk.open_subkey_with_flags(UNINSTALL, KEY_SET_VALUE) {
        let _ = k.set_value("DisplayVersion", &version);
    }
    notify_assoc_changed();
    Ok(())
}

pub fn unregister() {
    let hk = RegKey::predef(HKEY_CURRENT_USER);
    let _ = hk.delete_subkey_all(format!(r"Software\Classes\{PROG_URL}"));
    let _ = hk.delete_subkey_all(format!(r"Software\Classes\{PROG_HTML}"));
    let _ = hk.delete_subkey_all(CLIENT);
    if let Ok(k) = hk.open_subkey_with_flags(r"Software\RegisteredApplications", KEY_SET_VALUE) {
        let _ = k.delete_value(APP);
    }
    for x in FILES {
        if let Ok(k) = hk.open_subkey_with_flags(format!(r"Software\Classes\{x}\OpenWithProgids"), KEY_SET_VALUE) {
            let _ = k.delete_value(PROG_HTML);
        }
    }
    notify_assoc_changed();
}

/// Whether Windows currently opens https links with OTIUM.
pub fn is_default() -> bool {
    let hk = RegKey::predef(HKEY_CURRENT_USER);
    let base = r"Software\Microsoft\Windows\Shell\Associations\UrlAssociations\https";
    // newer Windows 11 builds keep the choice under UserChoiceLatest
    ["UserChoiceLatest", "UserChoice"].iter().any(|k| {
        hk.open_subkey_with_flags(format!(r"{base}\{k}"), KEY_READ)
            .and_then(|k| k.get_value::<String, _>("ProgId"))
            .is_ok_and(|p| p == PROG_URL)
    })
}

/// Open Settings → Default apps right on OTIUM's page.
pub fn open_default_settings() {
    // ShellExecute resolves the ms-settings: protocol; explorer.exe would treat it as a path
    // (the "?" makes it invalid) and open Documents instead
    use windows::{
        core::{w, HSTRING},
        Win32::UI::{Shell::ShellExecuteW, WindowsAndMessaging::SW_SHOWNORMAL},
    };
    let uri = HSTRING::from("ms-settings:defaultapps?registeredAppUser=OTIUM%20Browser");
    unsafe {
        ShellExecuteW(None, w!("open"), &uri, None, None, SW_SHOWNORMAL);
    }
}

fn notify_assoc_changed() {
    use windows::Win32::UI::Shell::{SHChangeNotify, SHCNE_ASSOCCHANGED, SHCNF_IDLIST};
    unsafe { SHChangeNotify(SHCNE_ASSOCCHANGED, SHCNF_IDLIST, None, None) };
}
