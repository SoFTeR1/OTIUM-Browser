//! Small Windows helpers: window icon, message boxes, dates, uninstall.

use std::path::PathBuf;

use tao::window::Icon;
use windows::{
    core::HSTRING,
    Win32::UI::WindowsAndMessaging::{
        MessageBoxW, IDYES, MB_ICONERROR, MB_ICONINFORMATION, MB_ICONQUESTION, MB_OK, MB_YESNO,
    },
};

/// Window icon decoded from the embedded PNG.
pub fn app_icon(png_bytes: &[u8]) -> Option<Icon> {
    let mut dec = png::Decoder::new(std::io::Cursor::new(png_bytes));
    dec.set_transformations(png::Transformations::EXPAND | png::Transformations::STRIP_16);
    let mut reader = dec.read_info().ok()?;
    let mut buf = vec![0; reader.output_buffer_size()?];
    let info = reader.next_frame(&mut buf).ok()?;
    let px = &buf[..info.buffer_size()];
    let rgba: Vec<u8> = match info.color_type {
        png::ColorType::Rgba => px.to_vec(),
        png::ColorType::Rgb => px.chunks(3).flat_map(|c| [c[0], c[1], c[2], 255]).collect(),
        _ => return None,
    };
    Icon::from_rgba(rgba, info.width, info.height).ok()
}

/// Date as YYYY-MM-DD (UTC; used for file names).
pub fn today() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    // civil-from-days (Howard Hinnant)
    let z = secs / 86400 + 719468;
    let era = z.div_euclid(146097);
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!("{y:04}-{m:02}-{d:02}")
}

pub fn message_box(title: &str, text: &str, error: bool) {
    let icon = if error { MB_ICONERROR } else { MB_ICONINFORMATION };
    unsafe {
        MessageBoxW(None, &HSTRING::from(text), &HSTRING::from(title), MB_OK | icon);
    }
}

fn confirm(title: &str, text: &str) -> bool {
    unsafe { MessageBoxW(None, &HSTRING::from(text), &HSTRING::from(title), MB_YESNO | MB_ICONQUESTION) == IDYES }
}

fn known_folder(id: &windows::core::GUID) -> Option<PathBuf> {
    use windows::Win32::{System::Com::CoTaskMemFree, UI::Shell::{SHGetKnownFolderPath, KF_FLAG_DEFAULT}};
    unsafe {
        let p = SHGetKnownFolderPath(id, KF_FLAG_DEFAULT, None).ok()?;
        let s = p.to_string().ok();
        CoTaskMemFree(Some(p.0 as *const _));
        s.map(PathBuf::from)
    }
}

/// System RAM state: (percent of physical memory in use, available bytes).
pub fn memory() -> Option<(u32, u64)> {
    use windows::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};
    let mut st = MEMORYSTATUSEX { dwLength: std::mem::size_of::<MEMORYSTATUSEX>() as u32, ..Default::default() };
    unsafe { GlobalMemoryStatusEx(&mut st).ok()? };
    Some((st.dwMemoryLoad, st.ullAvailPhys))
}

/// `otium-browser.exe --uninstall`, registered as the uninstall command by the installer.
pub fn uninstall() {
    use windows::Win32::UI::Shell::{FOLDERID_Desktop, FOLDERID_Programs};
    if !confirm("Удаление OTIUM Browser", "Удалить OTIUM Browser с этого компьютера?") {
        return;
    }
    let keep = confirm(
        "Удаление OTIUM Browser",
        "Сохранить профиль (вкладки, пароли, историю, расширения)?\n\nДа — оставить, Нет — удалить всё.",
    );
    for folder in [FOLDERID_Desktop, FOLDERID_Programs] {
        if let Some(d) = known_folder(&folder) {
            let _ = std::fs::remove_file(d.join("OTIUM Browser.lnk"));
        }
    }
    let _ = std::process::Command::new("reg")
        .args(["delete", r"HKCU\Software\Microsoft\Windows\CurrentVersion\Uninstall\OTIUM Browser", "/f"])
        .output();
    crate::integrate::unregister();
    if !keep {
        let _ = std::fs::remove_dir_all(crate::util::data_dir());
    }
    // The running exe can't delete itself: cmd removes our two files and then the folder,
    // and only if the folder is empty afterwards. Only folders made by the installer
    // (they carry the marker file) are touched — never e.g. a development build directory.
    if let Some(dir) = std::env::current_exe().ok().and_then(|e| e.parent().map(|p| p.to_path_buf())) {
        if dir.join("otium-browser.install").exists() {
            let d = dir.display();
            let script = format!(
                "ping 127.0.0.1 -n 3 >nul & del /f /q \"{d}\\otium-browser.exe\" \"{d}\\otium-browser.install\" & rmdir \"{d}\""
            );
            use std::os::windows::process::CommandExt;
            let _ = std::process::Command::new("cmd").arg("/c").raw_arg(&script).creation_flags(0x0800_0000).spawn();
        }
    }
    message_box("OTIUM Browser", "OTIUM Browser удалён.", false);
}
