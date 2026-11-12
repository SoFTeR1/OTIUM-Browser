//! Windows shell helpers for the install steps: known folders, .lnk shortcuts, registry.

use std::path::{Path, PathBuf};

use windows::{
    core::{Interface, GUID, HSTRING},
    Win32::{
        System::Com::{CoCreateInstance, CoInitializeEx, CoTaskMemFree, IPersistFile, CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED},
        UI::Shell::{
            FOLDERID_Desktop, FOLDERID_Programs, IShellLinkW, SHChangeNotify, SHGetKnownFolderPath, ShellLink,
            KF_FLAG_DEFAULT, SHCNE_ASSOCCHANGED, SHCNF_IDLIST,
        },
    },
};
use winreg::{enums::HKEY_CURRENT_USER, RegKey};

pub const APP_NAME: &str = "OTIUM Browser";
pub const EXE_NAME: &str = "otium-browser.exe";
/// Written next to the exe; the uninstaller only removes folders that carry it.
pub const MARKER: &str = "otium-browser.install";
const UNINSTALL_SUBKEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Uninstall\OTIUM Browser";

fn known_folder(id: &GUID) -> Option<PathBuf> {
    unsafe {
        let p = SHGetKnownFolderPath(id, KF_FLAG_DEFAULT, None).ok()?;
        let s = p.to_string().ok();
        CoTaskMemFree(Some(p.0 as *const _));
        s.map(PathBuf::from)
    }
}

pub fn desktop_link() -> Option<PathBuf> {
    known_folder(&FOLDERID_Desktop).map(|d| d.join(format!("{APP_NAME}.lnk")))
}

pub fn start_menu_link() -> Option<PathBuf> {
    known_folder(&FOLDERID_Programs).map(|d| d.join(format!("{APP_NAME}.lnk")))
}

pub fn write_shortcut(lnk: &Path, target: &Path) -> windows::core::Result<()> {
    unsafe {
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
        let link: IShellLinkW = CoCreateInstance(&ShellLink, None, CLSCTX_INPROC_SERVER)?;
        link.SetPath(&HSTRING::from(target.as_os_str()))?;
        if let Some(dir) = target.parent() {
            link.SetWorkingDirectory(&HSTRING::from(dir.as_os_str()))?;
        }
        link.SetIconLocation(&HSTRING::from(target.as_os_str()), 0)?;
        link.SetDescription(&HSTRING::from("OTIUM Browser — быстрый браузер"))?;
        link.cast::<IPersistFile>()?.Save(&HSTRING::from(lnk.as_os_str()), true)
    }
}

pub fn refresh_icons() {
    unsafe { SHChangeNotify(SHCNE_ASSOCCHANGED, SHCNF_IDLIST, None, None) };
}

/// Values for the "Apps & features" entry.
pub fn register(values: &[(&str, String)], size_kb: u32) -> std::io::Result<()> {
    let (k, _) = RegKey::predef(HKEY_CURRENT_USER).create_subkey(UNINSTALL_SUBKEY)?;
    for (n, v) in values {
        k.set_value(*n, v)?;
    }
    k.set_value("NoModify", &1u32)?;
    k.set_value("NoRepair", &1u32)?;
    k.set_value("EstimatedSize", &size_kb)?;
    Ok(())
}

/// InstallLocation of a previous install, if any.
pub fn previous_install() -> Option<PathBuf> {
    let k = RegKey::predef(HKEY_CURRENT_USER).open_subkey(UNINSTALL_SUBKEY).ok()?;
    let p: String = k.get_value("InstallLocation").ok()?;
    (!p.is_empty()).then(|| PathBuf::from(p))
}

/// A console tool started without flashing a console window.
pub fn hidden(program: &str) -> std::process::Command {
    use std::os::windows::process::CommandExt;
    let mut c = std::process::Command::new(program);
    c.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    c
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shortcut_is_written_and_points_to_target() {
        let dir = std::env::temp_dir().join(format!("otium-lnk-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let target = std::env::current_exe().unwrap();
        let lnk = dir.join("OTIUM Browser.lnk");
        write_shortcut(&lnk, &target).unwrap();
        let bytes = std::fs::read(&lnk).unwrap();
        // .lnk header magic + the target path stored as UTF-16 somewhere inside
        assert_eq!(&bytes[..4], &[0x4C, 0, 0, 0]);
        let name: Vec<u8> = target.file_name().unwrap().to_string_lossy().encode_utf16().flat_map(|c| c.to_le_bytes()).collect();
        assert!(bytes.windows(name.len()).any(|w| w == name.as_slice()));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn known_folders_resolve() {
        assert!(desktop_link().is_some_and(|p| p.ends_with("OTIUM Browser.lnk")));
        assert!(start_menu_link().is_some());
    }
}
