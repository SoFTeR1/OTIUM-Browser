//! Chrome extensions: download from the Chrome Web Store (CRX), unpack, read the manifest.
//! Loading into WebView2 happens in `native::add_extension`.

use std::{
    fs,
    io::{Cursor, Read},
    path::{Path, PathBuf},
};

use base64::Engine as _;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// What we keep about an installed extension (index.json in the extensions dir).
#[derive(Serialize, Deserialize, Clone, Default)]
#[serde(default)]
pub struct ExtMeta {
    /// WebView2 runtime id
    pub id: String,
    pub name: String,
    pub version: String,
    pub dir: PathBuf,
    /// relative path of the popup page, if the extension has one
    pub popup: String,
    /// options page, if any
    pub options: String,
    /// data: URL of the icon
    pub icon: String,
    pub enabled: bool,
}

pub fn index_path(root: &Path) -> PathBuf {
    root.join("index.json")
}

pub fn load_index(root: &Path) -> Vec<ExtMeta> {
    fs::read_to_string(index_path(root))
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

pub fn save_index(root: &Path, list: &[ExtMeta]) {
    if let Ok(s) = serde_json::to_string_pretty(list) {
        let _ = fs::write(index_path(root), s);
    }
}

/// Extract the 32-letter extension id from a Web Store URL or a bare id.
pub fn parse_store_id(input: &str) -> Option<String> {
    let s = input.trim();
    let is_id = |t: &str| t.len() == 32 && t.chars().all(|c| ('a'..='p').contains(&c));
    if is_id(s) {
        return Some(s.to_string());
    }
    s.split(['/', '?', '#', '&']).find(|p| is_id(p)).map(str::to_owned)
}

/// Strip the CRX header and return the zip payload.
fn crx_to_zip(data: &[u8]) -> Result<&[u8], String> {
    if data.len() < 16 || &data[0..4] != b"Cr24" {
        // some mirrors return a plain zip
        if data.starts_with(b"PK") {
            return Ok(data);
        }
        return Err("Это не файл расширения Chrome".into());
    }
    let u32le = |i: usize| u32::from_le_bytes([data[i], data[i + 1], data[i + 2], data[i + 3]]) as usize;
    let start = match u32le(4) {
        3 => 12 + u32le(8),
        2 => 16 + u32le(8) + u32le(12),
        v => return Err(format!("Неизвестная версия CRX: {v}")),
    };
    data.get(start..).ok_or_else(|| "Повреждённый CRX".into())
}

fn unzip(zip: &[u8], dest: &Path) -> Result<(), String> {
    let mut ar = zip::ZipArchive::new(Cursor::new(zip)).map_err(|e| e.to_string())?;
    for i in 0..ar.len() {
        let mut f = ar.by_index(i).map_err(|e| e.to_string())?;
        let Some(rel) = f.enclosed_name() else { continue }; // rejects ../ paths
        let out = dest.join(rel);
        if f.is_dir() {
            let _ = fs::create_dir_all(&out);
            continue;
        }
        if let Some(p) = out.parent() {
            let _ = fs::create_dir_all(p);
        }
        let mut buf = Vec::new();
        f.read_to_end(&mut buf).map_err(|e| e.to_string())?;
        fs::write(&out, buf).map_err(|e| e.to_string())?;
    }
    Ok(())
}

/// Download an extension from the Chrome Web Store and unpack it. Blocking.
pub fn download(store_id: &str, root: &Path) -> Result<PathBuf, String> {
    let url = format!(
        "https://clients2.google.com/service/update2/crx?response=redirect&prodversion=131.0.0.0\
         &acceptformat=crx2,crx3&x=id%3D{store_id}%26installsource%3Dondemand%26uc"
    );
    let resp = ureq::get(&url)
        .timeout(std::time::Duration::from_secs(60))
        .call()
        .map_err(|e| format!("Не удалось скачать: {e}"))?;
    let mut data = Vec::new();
    resp.into_reader()
        .take(200 * 1024 * 1024)
        .read_to_end(&mut data)
        .map_err(|e| e.to_string())?;
    let zip = crx_to_zip(&data)?;
    let dest = root.join(store_id);
    let _ = fs::remove_dir_all(&dest);
    fs::create_dir_all(&dest).map_err(|e| e.to_string())?;
    unzip(zip, &dest)?;
    // Web Store packages carry signature metadata Chromium rejects for unpacked loads.
    let _ = fs::remove_dir_all(dest.join("_metadata"));
    if !dest.join("manifest.json").exists() {
        return Err("В архиве нет manifest.json".into());
    }
    Ok(dest)
}

/// Copy an unpacked extension folder chosen by the user into our extensions dir.
pub fn import_folder(src: &Path, root: &Path) -> Result<PathBuf, String> {
    if !src.join("manifest.json").exists() {
        return Err("В папке нет manifest.json".into());
    }
    let name = src.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_else(|| "ext".into());
    let dest = root.join(format!("local-{name}"));
    let _ = fs::remove_dir_all(&dest);
    copy_dir(src, &dest).map_err(|e| e.to_string())?;
    Ok(dest)
}

pub fn copy_dir(src: &Path, dst: &Path) -> std::io::Result<()> {
    fs::create_dir_all(dst)?;
    for e in fs::read_dir(src)? {
        let e = e?;
        let to = dst.join(e.file_name());
        if e.file_type()?.is_dir() {
            copy_dir(&e.path(), &to)?;
        } else {
            fs::copy(e.path(), to)?;
        }
    }
    Ok(())
}

/// Resolve `__MSG_name__` placeholders using the default locale.
fn localize(dir: &Path, manifest: &Value, s: &str) -> String {
    let Some(key) = s.strip_prefix("__MSG_").and_then(|k| k.strip_suffix("__")) else { return s.to_string() };
    let mut locales = vec!["ru".to_string()];
    if let Some(d) = manifest.get("default_locale").and_then(|v| v.as_str()) {
        locales.push(d.to_string());
    }
    locales.push("en".into());
    for loc in locales {
        let p = dir.join("_locales").join(&loc).join("messages.json");
        if let Some(msgs) = fs::read_to_string(p).ok().and_then(|t| serde_json::from_str::<Value>(&t).ok()) {
            if let Some(obj) = msgs.as_object() {
                if let Some((_, v)) = obj.iter().find(|(k, _)| k.eq_ignore_ascii_case(key)) {
                    if let Some(m) = v.get("message").and_then(|m| m.as_str()) {
                        return m.to_string();
                    }
                }
            }
        }
    }
    s.to_string()
}

/// Read name, version, popup, options page and icon from the manifest.
pub fn read_manifest(dir: &Path) -> ExtMeta {
    let m: Value = fs::read_to_string(dir.join("manifest.json"))
        .ok()
        .and_then(|t| serde_json::from_str(t.trim_start_matches('\u{feff}')).ok())
        .unwrap_or(Value::Null);
    let s = |v: Option<&Value>| v.and_then(|x| x.as_str()).unwrap_or("").to_string();
    let action = m.get("action").or_else(|| m.get("browser_action")).or_else(|| m.get("page_action"));
    let popup = s(action.and_then(|a| a.get("default_popup")));
    let options = s(m.get("options_ui").and_then(|o| o.get("page")).or_else(|| m.get("options_page")));

    // biggest icon up to 128px
    let mut icon_rel = String::new();
    for src in [m.get("icons"), action.and_then(|a| a.get("default_icon"))] {
        match src {
            Some(Value::Object(map)) => {
                let mut best: Option<(u32, String)> = None;
                for (k, v) in map {
                    let size: u32 = k.parse().unwrap_or(0);
                    if let Some(p) = v.as_str() {
                        if size <= 128 && best.as_ref().is_none_or(|(b, _)| size > *b) {
                            best = Some((size, p.to_string()));
                        }
                    }
                }
                if let Some((_, p)) = best {
                    icon_rel = p;
                    break;
                }
            }
            Some(Value::String(p)) => {
                icon_rel = p.clone();
                break;
            }
            _ => {}
        }
    }
    let icon = if icon_rel.is_empty() {
        String::new()
    } else {
        let p = dir.join(icon_rel.trim_start_matches('/'));
        let mime = if icon_rel.ends_with(".svg") { "image/svg+xml" } else { "image/png" };
        fs::read(p)
            .map(|b| format!("data:{mime};base64,{}", base64::engine::general_purpose::STANDARD.encode(b)))
            .unwrap_or_default()
    };

    ExtMeta {
        id: String::new(),
        name: localize(dir, &m, &s(m.get("name"))),
        version: s(m.get("version")),
        dir: dir.to_path_buf(),
        popup: popup.trim_start_matches('/').to_string(),
        options: options.trim_start_matches('/').to_string(),
        icon,
        enabled: true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn store_ids() {
        let id = "cjpalhdlnbpafiamejdnhcphjbkeiagm";
        assert_eq!(parse_store_id(id).as_deref(), Some(id));
        assert_eq!(
            parse_store_id(&format!("https://chromewebstore.google.com/detail/ublock-origin/{id}?hl=ru")).as_deref(),
            Some(id)
        );
        assert!(parse_store_id("https://example.com").is_none());
    }

    #[test]
    fn crx3_header() {
        let mut d = b"Cr24".to_vec();
        d.extend(3u32.to_le_bytes());
        d.extend(2u32.to_le_bytes());
        d.extend([0, 0]);
        d.extend(b"PK..");
        assert_eq!(crx_to_zip(&d).unwrap(), b"PK..");
    }
}
