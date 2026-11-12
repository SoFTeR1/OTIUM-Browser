//! Self-update from GitHub Releases.
//!
//! Every release carries `latest.json`: `{version, url, sha256, sig}`, where `sig` is an
//! Ed25519 signature over `"otium-update\n{version}\n{sha256}"` made with the release key
//! (kept offline by the maintainer; see `signer/`). Only the public half is compiled in,
//! so a tampered manifest or exe — even one uploaded to the real repo — is refused.
//! The new exe is downloaded in the background and swapped in when the browser exits:
//! Windows allows renaming a running exe, so no installer or admin rights are needed.

use std::{
    fs,
    io::{Read, Write},
    path::{Path, PathBuf},
    time::Duration,
};

use ed25519_dalek::{Signature, VerifyingKey};
use serde::Deserialize;
use sha2::{Digest, Sha256};

const PUBLIC_KEY_HEX: &str = include_str!("../release.pub");
const MANIFEST: &str = "https://github.com/SoFTeR1/OTIUM-Browser/releases/latest/download/latest.json";
const ASSETS: &str = "https://github.com/SoFTeR1/OTIUM-Browser/releases/download/";
const MAX_EXE: u64 = 150 << 20;

#[derive(Deserialize)]
struct Manifest {
    version: String,
    url: String,
    sha256: String,
    sig: String,
}

fn hex(s: &str) -> Option<Vec<u8>> {
    let s = s.trim();
    if s.len() % 2 != 0 {
        return None;
    }
    (0..s.len()).step_by(2).map(|i| u8::from_str_radix(s.get(i..i + 2)?, 16).ok()).collect()
}

fn to_hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// "1.2.3" > "1.2.0"
pub fn newer(candidate: &str, current: &str) -> bool {
    let parse = |v: &str| -> Option<Vec<u64>> { v.trim().trim_start_matches('v').split('.').map(|p| p.parse().ok()).collect() };
    match (parse(candidate), parse(current)) {
        (Some(a), Some(b)) => a > b,
        _ => false,
    }
}

pub fn signed_message(version: &str, sha256: &str) -> String {
    format!("otium-update\n{version}\n{}", sha256.to_ascii_lowercase())
}

fn verify(m: &Manifest) -> Result<(), String> {
    let key: [u8; 32] = hex(PUBLIC_KEY_HEX).and_then(|k| k.try_into().ok()).ok_or("нет ключа проверки")?;
    let sig: [u8; 64] = hex(&m.sig).and_then(|s| s.try_into().ok()).ok_or("повреждённая подпись")?;
    VerifyingKey::from_bytes(&key)
        .and_then(|k| k.verify_strict(signed_message(&m.version, &m.sha256).as_bytes(), &Signature::from_bytes(&sig)))
        .map_err(|_| "подпись обновления не прошла проверку".to_string())
}

fn agent() -> ureq::Agent {
    ureq::AgentBuilder::new()
        .timeout_connect(Duration::from_secs(15))
        .timeout_read(Duration::from_secs(60))
        .user_agent(concat!("OTIUM-Browser/", env!("CARGO_PKG_VERSION")))
        .build()
}

/// Check for a newer release and download it into `dir`.
/// Ok(None): already up to date; Ok(Some((version, path))): verified exe ready to install.
pub fn check_and_download(current: &str, dir: &Path) -> Result<Option<(String, PathBuf)>, String> {
    // OTIUM_UPDATE_URL points at a test manifest; its assets must come from the same origin
    let (manifest_url, assets) = match std::env::var("OTIUM_UPDATE_URL") {
        Ok(u) => {
            let origin = url::Url::parse(&u).map_err(|e| e.to_string())?.origin().ascii_serialization() + "/";
            (u, origin)
        }
        Err(_) => (MANIFEST.to_string(), ASSETS.to_string()),
    };
    let agent = agent();
    let text = agent
        .get(&manifest_url)
        .call()
        .map_err(|e| format!("не удалось проверить обновления: {e}"))?
        .into_string()
        .map_err(|e| e.to_string())?;
    let m: Manifest = serde_json::from_str(&text).map_err(|_| "некорректный файл обновления".to_string())?;
    verify(&m)?;
    if !newer(&m.version, current) {
        return Ok(None);
    }
    if !m.url.starts_with(&assets) {
        return Err("обновление ссылается на чужой адрес".into());
    }
    let want = m.sha256.to_ascii_lowercase();
    fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    let path = dir.join(format!("otium-{}.exe", m.version));
    if file_sha256(&path).is_some_and(|h| h == want) {
        return Ok(Some((m.version, path)));
    }

    let tmp = path.with_extension("part");
    let resp = agent.get(&m.url).call().map_err(|e| format!("не удалось скачать обновление: {e}"))?;
    let mut reader = resp.into_reader().take(MAX_EXE);
    let mut out = fs::File::create(&tmp).map_err(|e| e.to_string())?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 256 * 1024];
    loop {
        let n = reader.read(&mut buf).map_err(|e| format!("загрузка прервалась: {e}"))?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
        out.write_all(&buf[..n]).map_err(|e| e.to_string())?;
    }
    drop(out);
    if to_hex(&hasher.finalize()) != want {
        let _ = fs::remove_file(&tmp);
        return Err("скачанный файл повреждён или подменён".into());
    }
    fs::rename(&tmp, &path).map_err(|e| e.to_string())?;
    Ok(Some((m.version, path)))
}

fn file_sha256(path: &Path) -> Option<String> {
    let mut f = fs::File::open(path).ok()?;
    let mut h = Sha256::new();
    std::io::copy(&mut f, &mut h).ok()?;
    Some(to_hex(&h.finalize()))
}

fn old_path(exe: &Path) -> PathBuf {
    exe.with_file_name("otium-browser.old.exe")
}

/// Put the downloaded exe in place of the running one (it takes effect on the next start).
pub fn install(exe: &Path, new: &Path) -> std::io::Result<()> {
    let old = old_path(exe);
    let _ = fs::remove_file(&old);
    fs::rename(exe, &old)?;
    if let Err(e) = fs::copy(new, exe) {
        let _ = fs::rename(&old, exe);
        return Err(e);
    }
    let _ = fs::remove_file(new);
    Ok(())
}

/// Remove leftovers of a previous update.
pub fn cleanup(exe: &Path, dir: &Path) {
    let _ = fs::remove_file(old_path(exe));
    if let Ok(rd) = fs::read_dir(dir) {
        for e in rd.flatten() {
            let _ = fs::remove_file(e.path());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions() {
        assert!(newer("0.3.0", "0.2.1"));
        assert!(newer("v1.0.0", "0.9.9"));
        assert!(newer("0.10.0", "0.9.0"));
        assert!(!newer("0.2.1", "0.2.1"));
        assert!(!newer("0.2.0", "0.2.1"));
        assert!(!newer("garbage", "0.2.1"));
    }

    #[test]
    fn rejects_bad_signature() {
        let m = Manifest { version: "9.9.9".into(), url: String::new(), sha256: "00".repeat(32), sig: "00".repeat(64) };
        assert!(verify(&m).is_err());
    }
}
