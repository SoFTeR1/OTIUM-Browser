//! User settings: persisted as JSON in the data dir, shared with internal pages.

use std::{fs, path::Path};

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

#[derive(Serialize, Deserialize, Clone, PartialEq)]
pub struct Tile {
    pub name: String,
    pub url: String,
}

#[derive(Serialize, Deserialize, Clone)]
#[serde(default)]
pub struct Settings {
    /// "sidebar" | "classic"
    pub layout: String,
    /// accent preset id, see [`ACCENTS`]
    pub accent: String,
    /// floating card with margins around the page
    pub floating: bool,
    /// search engine id, see [`ENGINES`], or "custom"
    pub search: String,
    /// custom search URL template, `%s` is replaced by the query
    pub search_custom: String,
    /// show the full URL in the address bar instead of a pretty host/path
    pub full_url: bool,
    /// preferred color scheme for sites: "auto" | "dark" | "light"
    pub site_theme: String,
    /// Chromium auto dark mode for all sites (needs restart)
    pub force_dark: bool,
    pub restore_session: bool,
    /// put background tabs into low-memory mode
    pub sleep_tabs: bool,
    pub nt_clock: bool,
    pub nt_greeting: bool,
    pub nt_tiles: bool,
    pub nt_hints: bool,
    pub nt_motion: bool,
    pub tiles: Vec<Tile>,
    /// built-in ad & tracker blocker
    pub shield: bool,
    /// sites where the blocker is turned off
    pub shield_allow: Vec<String>,
    /// search-engine suggestions in the address bar
    pub suggest_remote: bool,
    /// download signed updates from GitHub Releases and install them on restart
    pub auto_update: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            layout: "sidebar".into(),
            accent: "aurora".into(),
            floating: true,
            search: "google".into(),
            search_custom: String::new(),
            full_url: false,
            site_theme: "auto".into(),
            force_dark: false,
            restore_session: true,
            sleep_tabs: true,
            nt_clock: true,
            nt_greeting: true,
            nt_tiles: true,
            nt_hints: true,
            nt_motion: true,
            tiles: default_tiles(),
            shield: true,
            shield_allow: Vec::new(),
            suggest_remote: true,
            auto_update: true,
        }
    }
}

pub fn default_tiles() -> Vec<Tile> {
    [
        ("YouTube", "https://www.youtube.com"),
        ("GitHub", "https://github.com"),
        ("Яндекс", "https://ya.ru"),
        ("Википедия", "https://ru.wikipedia.org"),
        ("Reddit", "https://www.reddit.com"),
        ("Twitch", "https://www.twitch.tv"),
    ]
    .into_iter()
    .map(|(n, u)| Tile { name: n.into(), url: u.into() })
    .collect()
}

/// (id, name, url template)
pub const ENGINES: &[(&str, &str, &str)] = &[
    ("google", "Google", "https://www.google.com/search?q=%s"),
    ("yandex", "Яндекс", "https://ya.ru/search/?text=%s"),
    ("duckduckgo", "DuckDuckGo", "https://duckduckgo.com/?q=%s"),
    ("bing", "Bing", "https://www.bing.com/search?q=%s"),
    ("brave", "Brave Search", "https://search.brave.com/search?q=%s"),
];

/// (id, name, [accent1, accent2, accent3])
pub const ACCENTS: &[(&str, &str, [&str; 3])] = &[
    ("aurora", "Аврора", ["#8b5cf6", "#22d3ee", "#f472b6"]),
    ("sunset", "Закат", ["#f97316", "#f43f5e", "#facc15"]),
    ("ocean", "Океан", ["#3b82f6", "#06b6d4", "#6366f1"]),
    ("emerald", "Изумруд", ["#10b981", "#84cc16", "#14b8a6"]),
    ("sakura", "Сакура", ["#ec4899", "#c084fc", "#fb7185"]),
    ("mono", "Моно", ["#e4e4e7", "#a1a1aa", "#71717a"]),
];

impl Settings {
    pub fn load(path: &Path) -> Self {
        fs::read_to_string(path)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        let text = serde_json::to_string_pretty(self).map_err(std::io::Error::other)?;
        fs::write(path, text)
    }

    /// Merge a partial JSON object into the settings. Unknown or ill-typed keys are rejected.
    pub fn patched(&self, patch: &Value) -> Option<Self> {
        let mut base = serde_json::to_value(self).ok()?;
        let (Value::Object(b), Value::Object(p)) = (&mut base, patch) else { return None };
        for (k, v) in p {
            if b.contains_key(k) {
                b.insert(k.clone(), v.clone());
            }
        }
        let mut s: Settings = serde_json::from_value(base).ok()?;
        s.sanitize();
        Some(s)
    }

    fn sanitize(&mut self) {
        if !["sidebar", "classic"].contains(&self.layout.as_str()) {
            self.layout = "sidebar".into();
        }
        if !["auto", "dark", "light"].contains(&self.site_theme.as_str()) {
            self.site_theme = "auto".into();
        }
        if !ACCENTS.iter().any(|a| a.0 == self.accent) {
            self.accent = "aurora".into();
        }
        self.tiles.truncate(24);
        self.shield_allow.retain(|h| !h.is_empty() && h.len() < 256);
        self.shield_allow.dedup();
        self.tiles.retain(|t| t.url.starts_with("http://") || t.url.starts_with("https://"));
        for t in &mut self.tiles {
            t.name = t.name.chars().take(40).collect();
        }
    }

    pub fn search_template(&self) -> String {
        if self.search == "custom" && self.search_custom.contains("%s") {
            return self.search_custom.clone();
        }
        ENGINES
            .iter()
            .find(|e| e.0 == self.search)
            .unwrap_or(&ENGINES[0])
            .2
            .to_string()
    }

    pub fn search_name(&self) -> String {
        if self.search == "custom" {
            return "поисковике".into();
        }
        ENGINES.iter().find(|e| e.0 == self.search).unwrap_or(&ENGINES[0]).1.to_string()
    }

    pub fn accent_colors(&self) -> [&'static str; 3] {
        ACCENTS.iter().find(|a| a.0 == self.accent).unwrap_or(&ACCENTS[0]).2
    }

    /// Everything an internal page or the chrome needs to render.
    pub fn payload(&self, force_dark_active: bool) -> Value {
        json!({
            "settings": self,
            "accent": self.accent_colors(),
            "searchName": self.search_name(),
            "engines": ENGINES.iter().map(|e| json!({"id": e.0, "name": e.1})).collect::<Vec<_>>(),
            "accents": ACCENTS.iter().map(|a| json!({"id": a.0, "name": a.1, "colors": a.2})).collect::<Vec<_>>(),
            "forceDarkActive": force_dark_active,
            "defaultTiles": default_tiles(),
            "version": env!("CARGO_PKG_VERSION"),
        })
    }
}

/// JSON safe for embedding inside a `<script>` element.
pub fn script_json(v: &Value) -> String {
    v.to_string().replace('<', "\\u003c").replace('\u{2028}', "\\u2028").replace('\u{2029}', "\\u2029")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn patch_merges_and_sanitizes() {
        let s = Settings::default();
        let p = s.patched(&json!({"layout": "classic", "bogus": 1, "accent": "nope"})).unwrap();
        assert_eq!(p.layout, "classic");
        assert_eq!(p.accent, "aurora");
        assert!(s.patched(&json!({"floating": "yes"})).is_none());
        let p = s.patched(&json!({"tiles": [{"name": "x", "url": "javascript:alert(1)"}]})).unwrap();
        assert!(p.tiles.is_empty());
    }

    #[test]
    fn custom_search() {
        let s = Settings { search: "custom".into(), search_custom: "https://x.y/?q=%s".into(), ..Default::default() };
        assert_eq!(s.search_template(), "https://x.y/?q=%s");
    }
}
