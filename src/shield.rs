//! OTIUM Shield: network ad/tracker blocking with Brave's adblock-rust engine.
//!
//! Filter lists are downloaded in the background, compiled once and cached as a
//! serialized engine, so later starts load in a few milliseconds.

use std::{
    fs,
    path::{Path, PathBuf},
    time::{Duration, SystemTime},
};

use adblock::{request::Request, Engine};

pub const LISTS: &[(&str, &str)] = &[
    ("EasyList", "https://easylist.to/easylist/easylist.txt"),
    ("EasyPrivacy", "https://easylist.to/easylist/easyprivacy.txt"),
    ("RU AdList", "https://easylist-downloads.adblockplus.org/advblock.txt"),
    ("uBlock filters", "https://ublockorigin.github.io/uAssets/filters/filters.txt"),
    ("uBlock privacy", "https://ublockorigin.github.io/uAssets/filters/privacy.txt"),
];

const MAX_AGE: Duration = Duration::from_secs(4 * 24 * 3600);

pub struct Shield {
    engine: Engine,
}

fn cache_path(dir: &Path) -> PathBuf {
    dir.join("engine.dat")
}

fn is_fresh(p: &Path) -> bool {
    fs::metadata(p)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| SystemTime::now().duration_since(t).ok())
        .is_some_and(|age| age < MAX_AGE)
}

impl Shield {
    /// Load the cached engine (fast path). Returns None if there is no cache yet.
    pub fn load_cached(dir: &Path) -> Option<(Self, bool)> {
        let bytes = fs::read(cache_path(dir)).ok()?;
        let mut engine = Engine::default();
        engine.deserialize(&bytes).ok()?;
        Some((Shield { engine }, is_fresh(&cache_path(dir))))
    }

    /// Download all lists and compile a new engine. Blocking: run on a worker thread.
    pub fn build(dir: &Path) -> Option<Self> {
        let _ = fs::create_dir_all(dir);
        let mut text = String::new();
        for (name, url) in LISTS {
            let file = dir.join(format!("{}.txt", name.replace(' ', "_")));
            let body = ureq::get(url)
                .timeout(Duration::from_secs(30))
                .call()
                .ok()
                .and_then(|r| r.into_string().ok())
                .filter(|b| b.len() > 1000);
            match body {
                Some(b) => {
                    let _ = fs::write(&file, &b);
                    text.push_str(&b);
                }
                // offline: fall back to the previous copy of this list
                None => text.push_str(&fs::read_to_string(&file).unwrap_or_default()),
            }
            text.push('\n');
        }
        if text.len() < 10_000 {
            return None;
        }
        let engine = Engine::new_with_list_text(text);
        let _ = fs::write(cache_path(dir), engine.serialize());
        Some(Shield { engine })
    }

    pub fn should_block(&self, url: &str, page: &str, kind: &str) -> bool {
        match Request::new(url, page, kind, "GET") {
            Ok(req) => self.engine.check_network_request(&req).should_block(),
            Err(_) => false,
        }
    }

    /// CSS that hides ad containers on this page (site-specific + generic by class/id).
    pub fn cosmetic_css(&self, url: &str, classes: &[String], ids: &[String]) -> String {
        let res = self.engine.url_cosmetic_resources(url);
        let mut sel: Vec<String> = res.hide_selectors.into_iter().collect();
        if !res.generichide {
            sel.extend(self.engine.hidden_class_id_selectors(classes, ids, &res.exceptions));
        }
        if sel.is_empty() {
            return String::new();
        }
        let mut css = String::new();
        for chunk in sel.chunks(500) {
            css.push_str(&chunk.join(","));
            css.push_str("{display:none!important}\n");
        }
        css
    }
}
