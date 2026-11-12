//! History and bookmarks in a local SQLite database.

use std::path::Path;

use rusqlite::{params, Connection};
use serde::Serialize;

pub struct Store {
    db: Connection,
}

#[derive(Serialize, Clone)]
pub struct Entry {
    pub url: String,
    pub title: String,
    /// unix seconds
    pub time: i64,
    pub visits: i64,
}

#[derive(Serialize)]
pub struct Suggestion {
    pub kind: &'static str, // "history" | "bookmark"
    pub url: String,
    pub title: String,
}

fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn like(q: &str) -> String {
    let esc = q.replace('\\', "\\\\").replace('%', "\\%").replace('_', "\\_");
    format!("%{esc}%")
}

impl Store {
    pub fn open(path: &Path) -> rusqlite::Result<Self> {
        let db = Connection::open(path)?;
        db.execute_batch(
            "PRAGMA journal_mode=WAL;
             PRAGMA synchronous=NORMAL;
             CREATE TABLE IF NOT EXISTS history(
                url TEXT PRIMARY KEY, title TEXT NOT NULL DEFAULT '',
                visits INTEGER NOT NULL DEFAULT 1, last INTEGER NOT NULL);
             CREATE INDEX IF NOT EXISTS history_last ON history(last DESC);
             CREATE TABLE IF NOT EXISTS bookmarks(
                url TEXT PRIMARY KEY, title TEXT NOT NULL DEFAULT '', created INTEGER NOT NULL);",
        )?;
        Ok(Store { db })
    }

    pub fn visit(&self, url: &str, title: &str) {
        let _ = self.db.execute(
            "INSERT INTO history(url,title,visits,last) VALUES(?1,?2,1,?3)
             ON CONFLICT(url) DO UPDATE SET visits=visits+1, last=?3,
               title=CASE WHEN ?2<>'' THEN ?2 ELSE title END",
            params![url, title, now()],
        );
    }

    pub fn set_title(&self, url: &str, title: &str) {
        let _ = self.db.execute("UPDATE history SET title=?2 WHERE url=?1", params![url, title]);
        let _ = self.db.execute("UPDATE bookmarks SET title=?2 WHERE url=?1 AND title=''", params![url, title]);
    }

    pub fn history(&self, q: &str, offset: i64) -> Vec<Entry> {
        let mut st = match self.db.prepare_cached(
            "SELECT url,title,last,visits FROM history
             WHERE ?1='' OR url LIKE ?2 ESCAPE '\\' OR title LIKE ?2 ESCAPE '\\'
             ORDER BY last DESC LIMIT 100 OFFSET ?3",
        ) {
            Ok(s) => s,
            Err(_) => return Vec::new(),
        };
        st.query_map(params![q, like(q), offset], |r| {
            Ok(Entry { url: r.get(0)?, title: r.get(1)?, time: r.get(2)?, visits: r.get(3)? })
        })
        .map(|it| it.flatten().collect())
        .unwrap_or_default()
    }

    pub fn delete_history(&self, url: &str) {
        let _ = self.db.execute("DELETE FROM history WHERE url=?1", params![url]);
    }

    pub fn clear_history(&self) {
        let _ = self.db.execute("DELETE FROM history", []);
    }

    pub fn bookmarks(&self) -> Vec<Entry> {
        let mut st = match self.db.prepare_cached("SELECT url,title,created FROM bookmarks ORDER BY created DESC") {
            Ok(s) => s,
            Err(_) => return Vec::new(),
        };
        st.query_map([], |r| Ok(Entry { url: r.get(0)?, title: r.get(1)?, time: r.get(2)?, visits: 0 }))
            .map(|it| it.flatten().collect())
            .unwrap_or_default()
    }

    pub fn is_bookmarked(&self, url: &str) -> bool {
        self.db
            .query_row("SELECT 1 FROM bookmarks WHERE url=?1", params![url], |_| Ok(()))
            .is_ok()
    }

    /// Toggle a bookmark; returns the new state.
    pub fn toggle_bookmark(&self, url: &str, title: &str) -> bool {
        if self.is_bookmarked(url) {
            let _ = self.db.execute("DELETE FROM bookmarks WHERE url=?1", params![url]);
            false
        } else {
            let _ = self.db.execute(
                "INSERT INTO bookmarks(url,title,created) VALUES(?1,?2,?3)",
                params![url, title, now()],
            );
            true
        }
    }

    pub fn delete_bookmark(&self, url: &str) {
        let _ = self.db.execute("DELETE FROM bookmarks WHERE url=?1", params![url]);
    }

    /// Address bar suggestions: bookmarks first, then frequently/recently visited pages.
    pub fn suggest(&self, q: &str, limit: usize) -> Vec<Suggestion> {
        let q = q.trim();
        if q.is_empty() {
            return Vec::new();
        }
        let pat = like(q);
        let mut out: Vec<Suggestion> = Vec::new();
        if let Ok(mut st) = self.db.prepare_cached(
            "SELECT url,title FROM bookmarks WHERE url LIKE ?1 ESCAPE '\\' OR title LIKE ?1 ESCAPE '\\' LIMIT 3",
        ) {
            if let Ok(rows) = st.query_map(params![pat], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))) {
                out.extend(rows.flatten().map(|(url, title)| Suggestion { kind: "bookmark", url, title }));
            }
        }
        if let Ok(mut st) = self.db.prepare_cached(
            "SELECT url,title FROM history WHERE url LIKE ?1 ESCAPE '\\' OR title LIKE ?1 ESCAPE '\\'
             ORDER BY (visits * 10 + last / 86400) DESC LIMIT ?2",
        ) {
            if let Ok(rows) = st.query_map(params![pat, limit as i64], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))) {
                for (url, title) in rows.flatten() {
                    if out.len() >= limit {
                        break;
                    }
                    if !out.iter().any(|s| s.url == url) {
                        out.push(Suggestion { kind: "history", url, title });
                    }
                }
            }
        }
        out
    }
}

/// Fetch search-engine suggestions (blocking; call from a worker thread).
pub fn remote_suggestions(engine: &str, q: &str) -> Vec<String> {
    let enc: String = url::form_urlencoded::byte_serialize(q.as_bytes()).collect();
    let url = match engine {
        "yandex" => format!("https://suggest.yandex.ru/suggest-ff.cgi?part={enc}&uil=ru"),
        "duckduckgo" => format!("https://duckduckgo.com/ac/?q={enc}&type=list"),
        "bing" => format!("https://api.bing.com/osjson.aspx?query={enc}"),
        "brave" => format!("https://search.brave.com/api/suggest?q={enc}"),
        "google" => format!("https://suggestqueries.google.com/complete/search?client=firefox&q={enc}"),
        _ => return Vec::new(),
    };
    let Ok(resp) = ureq::get(&url).timeout(std::time::Duration::from_secs(3)).call() else { return Vec::new() };
    let Some(v) = resp.into_string().ok().and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok()) else { return Vec::new() };
    // OpenSearch format: [query, [suggestions...], ...]
    v.get(1)
        .and_then(|a| a.as_array())
        .map(|a| a.iter().filter_map(|s| s.as_str().map(str::to_owned)).take(5).collect())
        .unwrap_or_default()
}
