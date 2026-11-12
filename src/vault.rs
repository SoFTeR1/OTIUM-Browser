//! OTIUM account and encrypted profile backup ("vault").
//!
//! The account is local: a display name plus a secret (password, PIN or secret
//! word) stored only as an Argon2id hash. Exporting packs the whole browser
//! profile (logins, saved passwords, cookies, history, bookmarks, extensions,
//! settings) into a single `.otium` file encrypted with a key derived from the
//! secret (Argon2id → XChaCha20-Poly1305, streamed in 1 MiB chunks).
//!
//! Chromium encrypts cookies and passwords with a random master key that is
//! itself protected by Windows DPAPI, i.e. bound to this Windows user. The
//! vault carries the raw master key (inside the encryption) and on import
//! re-protects it with DPAPI for the new Windows user, so every login and
//! saved password keeps working on a fresh Windows install.

use std::{
    fs::{self, File},
    io::{BufReader, BufWriter, Read, Seek, Write},
    path::{Path, PathBuf},
};

use argon2::{
    password_hash::{rand_core::OsRng, PasswordHash, PasswordHasher, PasswordVerifier, SaltString},
    Algorithm, Argon2, Params, Version,
};
use base64::{engine::general_purpose::STANDARD as B64, Engine as _};
use chacha20poly1305::{
    aead::{Aead, KeyInit},
    XChaCha20Poly1305, XNonce,
};
use rand_core_shim::fill;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::native;

const MAGIC: &[u8; 11] = b"OTIUMVAULT1";
const CHUNK: usize = 1 << 20;
/// Argon2id cost: 64 MiB, 3 passes — slow enough to make guessing a PIN expensive.
const KDF_M: u32 = 64 * 1024;
const KDF_T: u32 = 3;

mod rand_core_shim {
    use argon2::password_hash::rand_core::{OsRng, RngCore};
    pub fn fill(buf: &mut [u8]) {
        OsRng.fill_bytes(buf);
    }
}

/// Folders that are pure cache and are rebuilt automatically.
const SKIP_DIRS: &[&str] = &[
    "Cache", "Code Cache", "GPUCache", "DawnCache", "DawnGraphiteCache", "DawnWebGPUCache",
    "GrShaderCache", "ShaderCache", "GraphiteDawnCache", "Crashpad", "component_crx_cache",
    "CacheStorage", "ScriptCache", "blob_storage", "shield", "Safe Browsing", "BrowserMetrics",
];

#[derive(Serialize, Deserialize, Clone, Default)]
pub struct Account {
    pub name: String,
    /// "password" | "pin" | "word"
    pub kind: String,
    /// Argon2id PHC string of the secret
    pub hash: String,
    pub created: i64,
}

fn account_path(data: &Path) -> PathBuf {
    data.join("account.json")
}

pub fn load_account(data: &Path) -> Option<Account> {
    serde_json::from_str(&fs::read_to_string(account_path(data)).ok()?).ok()
}

fn argon() -> Argon2<'static> {
    Argon2::new(Algorithm::Argon2id, Version::V0x13, Params::new(KDF_M, KDF_T, 1, Some(32)).unwrap())
}

pub fn validate_secret(kind: &str, secret: &str) -> Result<(), String> {
    match kind {
        "pin" if secret.len() < 4 || !secret.chars().all(|c| c.is_ascii_digit()) => {
            Err("PIN-код — минимум 4 цифры".into())
        }
        "password" if secret.chars().count() < 6 => Err("Пароль — минимум 6 символов".into()),
        "word" if secret.trim().chars().count() < 4 => Err("Секретное слово — минимум 4 символа".into()),
        "pin" | "password" | "word" => Ok(()),
        _ => Err("Неизвестный тип защиты".into()),
    }
}

pub fn create_account(data: &Path, name: &str, kind: &str, secret: &str) -> Result<Account, String> {
    validate_secret(kind, secret)?;
    let salt = SaltString::generate(&mut OsRng);
    let hash = argon().hash_password(secret.as_bytes(), &salt).map_err(|e| e.to_string())?.to_string();
    let acc = Account {
        name: name.trim().chars().take(40).collect(),
        kind: kind.into(),
        hash,
        created: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0),
    };
    let text = serde_json::to_string_pretty(&acc).map_err(|e| e.to_string())?;
    fs::write(account_path(data), text).map_err(|e| e.to_string())?;
    Ok(acc)
}

pub fn verify(acc: &Account, secret: &str) -> bool {
    PasswordHash::new(&acc.hash).is_ok_and(|h| argon().verify_password(secret.as_bytes(), &h).is_ok())
}

pub fn delete_account(data: &Path) {
    let _ = fs::remove_file(account_path(data));
}

// ---------------------------------------------------------------- streaming AEAD

fn derive_key(secret: &str, salt: &[u8], m: u32, t: u32) -> Result<[u8; 32], String> {
    let a = Argon2::new(
        Algorithm::Argon2id,
        Version::V0x13,
        Params::new(m, t, 1, Some(32)).map_err(|e| e.to_string())?,
    );
    let mut key = [0u8; 32];
    a.hash_password_into(secret.as_bytes(), salt, &mut key).map_err(|e| e.to_string())?;
    Ok(key)
}

fn chunk_nonce(prefix: &[u8; 19], counter: u32, last: bool) -> XNonce {
    let mut n = [0u8; 24];
    n[..19].copy_from_slice(prefix);
    n[19..23].copy_from_slice(&counter.to_be_bytes());
    n[23] = last as u8;
    *XNonce::from_slice(&n)
}

struct EncWriter<W: Write> {
    out: W,
    cipher: XChaCha20Poly1305,
    prefix: [u8; 19],
    counter: u32,
    buf: Vec<u8>,
}

impl<W: Write> EncWriter<W> {
    fn new(mut out: W, secret: &str) -> Result<Self, String> {
        let mut salt = [0u8; 16];
        let mut prefix = [0u8; 19];
        fill(&mut salt);
        fill(&mut prefix);
        let key = derive_key(secret, &salt, KDF_M, KDF_T)?;
        let w = |o: &mut W, b: &[u8]| o.write_all(b).map_err(|e| e.to_string());
        w(&mut out, MAGIC)?;
        w(&mut out, &salt)?;
        w(&mut out, &KDF_M.to_le_bytes())?;
        w(&mut out, &KDF_T.to_le_bytes())?;
        w(&mut out, &prefix)?;
        Ok(EncWriter { out, cipher: XChaCha20Poly1305::new((&key).into()), prefix, counter: 0, buf: Vec::with_capacity(CHUNK) })
    }

    fn seal(&mut self, last: bool) -> std::io::Result<()> {
        let nonce = chunk_nonce(&self.prefix, self.counter, last);
        let ct = self
            .cipher
            .encrypt(&nonce, self.buf.as_slice())
            .map_err(|_| std::io::Error::other("encrypt"))?;
        self.out.write_all(&(ct.len() as u32).to_le_bytes())?;
        self.out.write_all(&ct)?;
        self.counter += 1;
        self.buf.clear();
        Ok(())
    }

    fn finish(mut self) -> std::io::Result<W> {
        self.seal(true)?;
        self.out.flush()?;
        Ok(self.out)
    }
}

impl<W: Write> Write for EncWriter<W> {
    fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
        let room = CHUNK - self.buf.len();
        let n = room.min(data.len());
        self.buf.extend_from_slice(&data[..n]);
        if self.buf.len() == CHUNK {
            self.seal(false)?;
        }
        Ok(n)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

struct VaultReader<R: Read> {
    input: R,
    cipher: XChaCha20Poly1305,
    prefix: [u8; 19],
    counter: u32,
}

impl<R: Read> VaultReader<R> {
    fn open(mut input: R, secret: &str) -> Result<Self, String> {
        let mut head = [0u8; 11 + 16 + 8 + 19];
        input.read_exact(&mut head).map_err(|_| "Файл повреждён или это не файл OTIUM".to_string())?;
        if &head[..11] != MAGIC {
            return Err("Это не файл резервной копии OTIUM".into());
        }
        let salt = &head[11..27];
        let m = u32::from_le_bytes(head[27..31].try_into().unwrap());
        let t = u32::from_le_bytes(head[31..35].try_into().unwrap());
        if m > 1024 * 1024 || t > 20 {
            return Err("Некорректные параметры файла".into());
        }
        let mut prefix = [0u8; 19];
        prefix.copy_from_slice(&head[35..54]);
        let key = derive_key(secret, salt, m, t)?;
        Ok(VaultReader { input, cipher: XChaCha20Poly1305::new((&key).into()), prefix, counter: 0 })
    }

    /// Next decrypted chunk and whether it is the last one.
    fn next(&mut self) -> Result<(Vec<u8>, bool), String> {
        let mut len = [0u8; 4];
        self.input.read_exact(&mut len).map_err(|_| "Файл обрезан".to_string())?;
        let len = u32::from_le_bytes(len) as usize;
        if len > CHUNK + 64 {
            return Err("Файл повреждён".into());
        }
        let mut ct = vec![0u8; len];
        self.input.read_exact(&mut ct).map_err(|_| "Файл обрезан".to_string())?;
        for last in [false, true] {
            if let Ok(pt) = self.cipher.decrypt(&chunk_nonce(&self.prefix, self.counter, last), ct.as_slice()) {
                self.counter += 1;
                return Ok((pt, last));
            }
        }
        Err(if self.counter == 0 { "Неверный пароль, PIN-код или секретное слово".into() } else { "Файл повреждён".into() })
    }
}

/// Quick check that `secret` opens the vault (decrypts only the first chunk).
pub fn check_vault(path: &Path, secret: &str) -> Result<(), String> {
    let f = File::open(path).map_err(|e| e.to_string())?;
    VaultReader::open(BufReader::new(f), secret)?.next().map(|_| ())
}

// ---------------------------------------------------------------- export / import

fn local_state(data: &Path) -> PathBuf {
    data.join("profile").join("EBWebView").join("Local State")
}

/// Raw AES key Chromium uses for cookies/passwords, unwrapped from DPAPI.
fn read_master_key(data: &Path) -> Option<Vec<u8>> {
    let ls: Value = serde_json::from_str(&fs::read_to_string(local_state(data)).ok()?).ok()?;
    let b = B64.decode(ls["os_crypt"]["encrypted_key"].as_str()?).ok()?;
    native::dpapi_unprotect(b.strip_prefix(b"DPAPI")?)
}

fn write_master_key(data: &Path, key: &[u8]) -> Result<(), String> {
    let p = local_state(data);
    let mut ls: Value = serde_json::from_str(&fs::read_to_string(&p).map_err(|e| e.to_string())?).map_err(|e| e.to_string())?;
    let blob = native::dpapi_protect(key).ok_or("DPAPI недоступен")?;
    let mut v = b"DPAPI".to_vec();
    v.extend(blob);
    ls["os_crypt"]["encrypted_key"] = Value::String(B64.encode(v));
    fs::write(&p, serde_json::to_string(&ls).map_err(|e| e.to_string())?).map_err(|e| e.to_string())
}

fn collect(root: &Path, dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(rd) = fs::read_dir(dir) else { return };
    for e in rd.flatten() {
        let p = e.path();
        let name = e.file_name().to_string_lossy().to_string();
        if e.file_type().is_ok_and(|t| t.is_dir()) {
            if !SKIP_DIRS.contains(&name.as_str()) && !name.starts_with("backup-") {
                collect(root, &p, out);
            }
        } else if !name.ends_with(".tmp") && name != "LOCK" && name != "lockfile" {
            out.push(p);
        }
    }
}

/// Pack the whole data dir into an encrypted vault. The browser must not be running.
pub fn export(data: &Path, dest: &Path, secret: &str) -> Result<usize, String> {
    let acc = load_account(data).ok_or("Сначала создайте аккаунт")?;
    if !verify(&acc, secret) {
        return Err("Неверный пароль, PIN-код или секретное слово".into());
    }
    let master = read_master_key(data);
    let mut files = Vec::new();
    collect(data, data, &mut files);

    // 1) zip the profile into a temporary archive (same sensitivity as the profile itself;
    //    the master key is NOT in it)
    let zip_tmp = dest.with_extension("otium.zip.part");
    let mut count = 0;
    {
        let mut zip = zip::ZipWriter::new(BufWriter::new(File::create(&zip_tmp).map_err(|e| e.to_string())?));
        let opts = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Deflated)
            .large_file(true);
        for f in &files {
            let rel = f.strip_prefix(data).unwrap().to_string_lossy().replace('\\', "/");
            // Some files may be held open by other software; skip them rather than fail.
            let Ok(mut src) = File::open(f) else { continue };
            zip.start_file(format!("data/{rel}"), opts).map_err(|e| e.to_string())?;
            std::io::copy(&mut src, &mut zip).map_err(|e| e.to_string())?;
            count += 1;
        }
        zip.finish().map_err(|e| e.to_string())?.flush().map_err(|e| e.to_string())?;
    }

    // 2) encrypted stream = [u32 manifest length][manifest json][zip]
    let manifest = json!({
        "format": 1,
        "app": "OTIUM",
        "version": env!("CARGO_PKG_VERSION"),
        "account": acc.name,
        "created": acc.created,
        "masterKey": master.as_ref().map(|k| B64.encode(k)),
        "files": count,
    })
    .to_string();
    let part = dest.with_extension("otium.part");
    let result = (|| -> Result<(), String> {
        let out = BufWriter::new(File::create(&part).map_err(|e| e.to_string())?);
        let mut enc = EncWriter::new(out, secret)?;
        let io = |e: std::io::Error| e.to_string();
        enc.write_all(&(manifest.len() as u32).to_le_bytes()).map_err(io)?;
        enc.write_all(manifest.as_bytes()).map_err(io)?;
        std::io::copy(&mut BufReader::new(File::open(&zip_tmp).map_err(io)?), &mut enc).map_err(io)?;
        enc.finish().map_err(io)?;
        Ok(())
    })();
    let _ = fs::remove_file(&zip_tmp);
    if let Err(e) = result {
        let _ = fs::remove_file(&part);
        return Err(e);
    }
    let _ = fs::remove_file(dest);
    fs::rename(&part, dest).map_err(|e| e.to_string())?;
    Ok(count)
}

/// Replace the current profile with the one in the vault. The browser must not be running.
pub fn import(data: &Path, src: &Path, secret: &str) -> Result<String, String> {
    let parent = data.parent().ok_or("bad data dir")?;
    let tmp = parent.join("OtiumBrowser.import.tmp");
    let manifest: Value;
    {
        let mut reader = VaultReader::open(BufReader::new(File::open(src).map_err(|e| e.to_string())?), secret)?;
        let mut out = BufWriter::new(File::create(&tmp).map_err(|e| e.to_string())?);
        let mut first = true;
        let mut man = Value::Null;
        loop {
            let (chunk, last) = reader.next().inspect_err(|_| {
                let _ = fs::remove_file(&tmp);
            })?;
            let mut body = chunk.as_slice();
            if first {
                // the manifest (with the master key) stays in memory only
                first = false;
                let n = body.get(..4).map(|b| u32::from_le_bytes(b.try_into().unwrap()) as usize).unwrap_or(usize::MAX);
                let m = body.get(4..4 + n).ok_or("Файл повреждён")?;
                man = serde_json::from_slice(m).map_err(|e| e.to_string())?;
                body = &body[4 + n..];
            }
            out.write_all(body).map_err(|e| e.to_string())?;
            if last {
                break;
            }
        }
        out.flush().map_err(|e| e.to_string())?;
        manifest = man;
    }
    let result = unpack(data, &tmp, &manifest);
    let _ = fs::remove_file(&tmp);
    result
}

fn unpack(data: &Path, zip_path: &Path, manifest: &Value) -> Result<String, String> {
    let mut f = File::open(zip_path).map_err(|e| e.to_string())?;
    f.rewind().map_err(|e| e.to_string())?;
    let mut ar = zip::ZipArchive::new(BufReader::new(f)).map_err(|e| format!("Архив повреждён: {e}"))?;

    // Keep the previous profile as a backup next to the data dir.
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let backup = data.with_file_name(format!("OtiumBrowser.backup-{stamp}"));
    if data.exists() {
        fs::rename(data, &backup).map_err(|e| format!("Не удалось освободить профиль: {e}"))?;
    }
    fs::create_dir_all(data).map_err(|e| e.to_string())?;

    for i in 0..ar.len() {
        let mut entry = ar.by_index(i).map_err(|e| e.to_string())?;
        let Some(name) = entry.enclosed_name() else { continue };
        let Ok(rel) = name.strip_prefix("data") else { continue };
        let out = data.join(rel);
        if entry.is_dir() {
            let _ = fs::create_dir_all(&out);
            continue;
        }
        if let Some(p) = out.parent() {
            let _ = fs::create_dir_all(p);
        }
        let mut w = File::create(&out).map_err(|e| e.to_string())?;
        std::io::copy(&mut entry, &mut w).map_err(|e| e.to_string())?;
    }

    if let Some(k) = manifest["masterKey"].as_str().and_then(|s| B64.decode(s).ok()) {
        write_master_key(data, &k)?;
    }
    // Extensions were registered with absolute paths of the old machine; re-register on start.
    let ext_root = data.join("extensions");
    let mut list = crate::extensions::load_index(&ext_root);
    for e in &mut list {
        if let Some(dir_name) = e.dir.file_name() {
            e.dir = ext_root.join(dir_name);
        }
        e.id.clear();
    }
    crate::extensions::save_index(&ext_root, &list);

    Ok(manifest["account"].as_str().unwrap_or("").to_string())
}

/// Wait until the previous browser instance released the profile.
pub fn wait_profile_free(data: &Path) {
    let p = data.join("profile");
    for _ in 0..80 {
        if !p.exists() {
            return;
        }
        let probe = data.join("profile.lockprobe");
        if fs::rename(&p, &probe).is_ok() {
            let _ = fs::rename(&probe, &p);
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(250));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stream_roundtrip_and_wrong_secret() {
        let mut out = Vec::new();
        {
            let mut w = EncWriter::new(&mut out, "1234").unwrap();
            let data = vec![7u8; CHUNK * 2 + 123];
            w.write_all(&data).unwrap();
            w.finish().unwrap();
        }
        let mut r = VaultReader::open(out.as_slice(), "1234").unwrap();
        let mut total = 0;
        loop {
            let (c, last) = r.next().unwrap();
            total += c.len();
            if last {
                break;
            }
        }
        assert_eq!(total, CHUNK * 2 + 123);
        let mut bad = VaultReader::open(out.as_slice(), "0000").unwrap();
        assert!(bad.next().is_err());
    }

    /// Full export → import cycle on a fake profile, including the DPAPI master key.
    #[test]
    fn export_import_roundtrip() {
        let root = std::env::temp_dir().join(format!("otium-vault-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let src = root.join("OtiumBrowser");
        let ls_dir = src.join("profile").join("EBWebView");
        fs::create_dir_all(ls_dir.join("Default").join("Cache")).unwrap();
        let key: Vec<u8> = (0..32u8).collect();
        let mut wrapped = b"DPAPI".to_vec();
        wrapped.extend(native::dpapi_protect(&key).unwrap());
        fs::write(ls_dir.join("Local State"), json!({"os_crypt": {"encrypted_key": B64.encode(&wrapped)}}).to_string()).unwrap();
        fs::write(ls_dir.join("Default").join("Login Data"), b"passwords").unwrap();
        fs::write(ls_dir.join("Default").join("Cache").join("junk"), b"cache").unwrap();
        fs::write(src.join("settings.json"), b"{}").unwrap();
        create_account(&src, "Test", "pin", "4321").unwrap();

        let file = root.join("backup.otium");
        assert!(export(&src, &file, "0000").is_err(), "wrong secret must be refused");
        export(&src, &file, "4321").unwrap();
        assert!(check_vault(&file, "1111").is_err());
        check_vault(&file, "4321").unwrap();

        // restore into a fresh location ("new Windows")
        let dst = root.join("restore").join("OtiumBrowser");
        fs::create_dir_all(&dst).unwrap();
        assert_eq!(import(&dst, &file, "4321").unwrap(), "Test");
        assert_eq!(fs::read(dst.join("profile/EBWebView/Default/Login Data")).unwrap(), b"passwords");
        assert!(!dst.join("profile/EBWebView/Default/Cache").exists(), "caches are not exported");
        assert_eq!(read_master_key(&dst).unwrap(), key);
        assert!(load_account(&dst).is_some());
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn secrets() {
        assert!(validate_secret("pin", "12a4").is_err());
        assert!(validate_secret("pin", "1234").is_ok());
        assert!(validate_secret("password", "12345").is_err());
    }
}
