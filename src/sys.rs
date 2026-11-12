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

/// Folder the installer puts OTIUM into.
pub fn install_dir() -> PathBuf {
    std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .unwrap_or_default()
        .join("Programs")
        .join("OTIUM")
}

/// `otium.exe --uninstall`, registered as the uninstall command by the installer.
pub fn uninstall() {
    if !confirm("Удаление OTIUM", "Удалить OTIUM с этого компьютера?") {
        return;
    }
    let keep = confirm(
        "Удаление OTIUM",
        "Сохранить профиль (вкладки, пароли, историю, расширения)?\n\nДа — оставить, Нет — удалить всё.",
    );
    let appdata = std::env::var_os("APPDATA").map(PathBuf::from).unwrap_or_default();
    let home = std::env::var_os("USERPROFILE").map(PathBuf::from).unwrap_or_default();
    let _ = std::fs::remove_file(appdata.join(r"Microsoft\Windows\Start Menu\Programs\OTIUM.lnk"));
    let _ = std::fs::remove_file(home.join(r"Desktop\OTIUM.lnk"));
    let _ = std::process::Command::new("reg")
        .args(["delete", r"HKCU\Software\Microsoft\Windows\CurrentVersion\Uninstall\OTIUM", "/f"])
        .output();
    if !keep {
        let _ = std::fs::remove_dir_all(crate::util::data_dir());
    }
    // The running exe can't delete itself: let cmd remove the install folder after we exit.
    // Only ever the installer's own folder, never e.g. a development build directory.
    let dir = install_dir();
    let running_from_install = std::env::current_exe()
        .ok()
        .and_then(|e| e.parent().map(|p| p.to_path_buf()))
        .is_some_and(|p| p.eq_ignore_ascii_case_path(&dir));
    if running_from_install {
        let script = format!("ping 127.0.0.1 -n 3 >nul & rmdir /s /q \"{}\"", dir.display());
        let _ = std::process::Command::new("cmd").args(["/c", &script]).spawn();
    }
    message_box("OTIUM", "OTIUM удалён.", false);
}

trait PathEqExt {
    fn eq_ignore_ascii_case_path(&self, other: &std::path::Path) -> bool;
}

impl PathEqExt for PathBuf {
    fn eq_ignore_ascii_case_path(&self, other: &std::path::Path) -> bool {
        self.to_string_lossy().eq_ignore_ascii_case(&other.to_string_lossy())
    }
}
