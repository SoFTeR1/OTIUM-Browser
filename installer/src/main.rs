//! OTIUM installer: a single exe with the browser embedded.
//! Installs per-user (no admin rights) into %LOCALAPPDATA%\Programs\OTIUM,
//! creates Start menu and desktop shortcuts and an "Apps & features" entry.

#![windows_subsystem = "windows"]

use std::{fs, path::PathBuf, process::Command};

use windows::{
    core::HSTRING,
    Win32::UI::WindowsAndMessaging::{
        MessageBoxW, IDYES, MB_ICONERROR, MB_ICONINFORMATION, MB_ICONQUESTION, MB_OK, MB_YESNO,
    },
};

const OTIUM_EXE: &[u8] = include_bytes!(env!("OTIUM_EXE"));
const VERSION: &str = env!("CARGO_PKG_VERSION");
const UNINSTALL_KEY: &str = r"HKCU\Software\Microsoft\Windows\CurrentVersion\Uninstall\OTIUM";

fn msg(text: &str, error: bool) {
    let icon = if error { MB_ICONERROR } else { MB_ICONINFORMATION };
    unsafe {
        MessageBoxW(None, &HSTRING::from(text), &HSTRING::from("Установка OTIUM"), MB_OK | icon);
    }
}

fn ask(text: &str) -> bool {
    unsafe { MessageBoxW(None, &HSTRING::from(text), &HSTRING::from("Установка OTIUM"), MB_YESNO | MB_ICONQUESTION) == IDYES }
}

fn env_dir(var: &str) -> PathBuf {
    std::env::var_os(var).map(PathBuf::from).unwrap_or_default()
}

/// Create a .lnk via the Windows Script Host COM object.
fn shortcut(lnk: &PathBuf, target: &PathBuf) {
    let ps = format!(
        "$s=(New-Object -ComObject WScript.Shell).CreateShortcut('{}');$s.TargetPath='{}';$s.WorkingDirectory='{}';$s.IconLocation='{},0';$s.Description='OTIUM — быстрый браузер';$s.Save()",
        lnk.display().to_string().replace('\'', "''"),
        target.display().to_string().replace('\'', "''"),
        target.parent().unwrap().display().to_string().replace('\'', "''"),
        target.display().to_string().replace('\'', "''"),
    );
    let _ = Command::new("powershell").args(["-NoProfile", "-NonInteractive", "-Command", &ps]).output();
}

fn reg(name: &str, kind: &str, value: &str) {
    let _ = Command::new("reg").args(["add", UNINSTALL_KEY, "/v", name, "/t", kind, "/d", value, "/f"]).output();
}

fn main() {
    let dir = env_dir("LOCALAPPDATA").join("Programs").join("OTIUM");
    let exe = dir.join("otium.exe");
    let upgrade = exe.exists();
    let q = if upgrade {
        format!("Обновить OTIUM до версии {VERSION}?\n\nОткрытый OTIUM будет закрыт. Профиль, вкладки и пароли сохранятся.")
    } else {
        format!("Установить OTIUM {VERSION}?\n\nПапка: {}\nПрава администратора не нужны.", dir.display())
    };
    if !ask(&q) {
        return;
    }

    // a running OTIUM keeps the exe locked
    let _ = Command::new("taskkill").args(["/IM", "otium.exe", "/F"]).output();
    std::thread::sleep(std::time::Duration::from_millis(600));

    if let Err(e) = fs::create_dir_all(&dir).and_then(|_| fs::write(&exe, OTIUM_EXE)) {
        msg(&format!("Не удалось записать файлы:\n{e}"), true);
        return;
    }

    let start_menu = env_dir("APPDATA").join(r"Microsoft\Windows\Start Menu\Programs\OTIUM.lnk");
    shortcut(&start_menu, &exe);
    let desktop = env_dir("USERPROFILE").join(r"Desktop\OTIUM.lnk");
    if upgrade && !desktop.exists() {
        // the user removed the desktop shortcut before — respect that
    } else {
        shortcut(&desktop, &exe);
    }

    let exe_s = exe.display().to_string();
    reg("DisplayName", "REG_SZ", "OTIUM");
    reg("DisplayVersion", "REG_SZ", VERSION);
    reg("Publisher", "REG_SZ", "SoFTeR1");
    reg("DisplayIcon", "REG_SZ", &format!("{exe_s},0"));
    reg("InstallLocation", "REG_SZ", &dir.display().to_string());
    reg("UninstallString", "REG_SZ", &format!("\"{exe_s}\" --uninstall"));
    reg("NoModify", "REG_DWORD", "1");
    reg("NoRepair", "REG_DWORD", "1");
    reg("EstimatedSize", "REG_DWORD", &(OTIUM_EXE.len() / 1024).to_string());

    if ask("OTIUM установлен. Запустить сейчас?") {
        let _ = Command::new(&exe).spawn();
    }
}
