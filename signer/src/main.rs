//! Release signing for the OTIUM updater.
//!
//!   otium-sign keygen <private.key> <release.pub>
//!       new Ed25519 key pair; the private key never goes into the repo
//!   otium-sign manifest <private.key> <exe> <version> <download-url> <latest.json>
//!       hash the exe and write the signed manifest the browser checks
//!
//! The signed message must match `update::signed_message` in the browser.

use std::{fs, path::Path, process::exit};

use ed25519_dalek::{Signer, SigningKey};
use sha2::{Digest, Sha256};

fn to_hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

fn from_hex(s: &str) -> Option<Vec<u8>> {
    let s = s.trim();
    (0..s.len()).step_by(2).map(|i| u8::from_str_radix(s.get(i..i + 2)?, 16).ok()).collect()
}

fn die(msg: &str) -> ! {
    eprintln!("otium-sign: {msg}");
    exit(1)
}

fn load_key(path: &str) -> SigningKey {
    let text = fs::read_to_string(path).unwrap_or_else(|e| die(&format!("cannot read key {path}: {e}")));
    let seed: [u8; 32] = from_hex(&text).and_then(|b| b.try_into().ok()).unwrap_or_else(|| die("bad key file"));
    SigningKey::from_bytes(&seed)
}

fn main() {
    let a: Vec<String> = std::env::args().collect();
    match a.get(1).map(String::as_str) {
        Some("keygen") if a.len() == 4 => {
            if Path::new(&a[2]).exists() {
                die("private key already exists; refusing to overwrite it");
            }
            let mut seed = [0u8; 32];
            getrandom::getrandom(&mut seed).unwrap_or_else(|e| die(&e.to_string()));
            let key = SigningKey::from_bytes(&seed);
            if let Some(dir) = Path::new(&a[2]).parent() {
                let _ = fs::create_dir_all(dir);
            }
            fs::write(&a[2], to_hex(&seed)).unwrap_or_else(|e| die(&e.to_string()));
            fs::write(&a[3], to_hex(key.verifying_key().as_bytes())).unwrap_or_else(|e| die(&e.to_string()));
            println!("private key: {}\npublic key:  {}", a[2], a[3]);
        }
        Some("manifest") if a.len() == 7 => {
            let key = load_key(&a[2]);
            let bytes = fs::read(&a[3]).unwrap_or_else(|e| die(&format!("cannot read {}: {e}", a[3])));
            let sha = to_hex(&Sha256::digest(&bytes));
            let (version, url) = (a[4].trim_start_matches('v'), &a[5]);
            let msg = format!("otium-update\n{version}\n{sha}");
            let sig = to_hex(&key.sign(msg.as_bytes()).to_bytes());
            let json = serde_json::json!({ "version": version, "url": url, "sha256": sha, "sig": sig });
            fs::write(&a[6], serde_json::to_string_pretty(&json).unwrap()).unwrap_or_else(|e| die(&e.to_string()));
            println!("{} signed ({version}, sha256 {sha})", a[6]);
        }
        _ => die("usage: keygen <private.key> <release.pub> | manifest <private.key> <exe> <version> <url> <latest.json>"),
    }
}
