use std::path::PathBuf;

pub fn data_dir() -> PathBuf {
    std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir)
        .join("OtiumBrowser")
}

/// URL as shown in the address bar ("" for internal pages).
pub fn display_url(url: &str) -> String {
    if let Some(rest) = url.strip_prefix(crate::INTERNAL) {
        let page = rest.split(['?', '#']).next().unwrap_or("");
        if page.is_empty() || page == "newtab" {
            String::new()
        } else {
            format!("otium://{page}")
        }
    } else if url == "about:blank" {
        String::new()
    } else {
        url.to_string()
    }
}

pub fn is_internal(url: &str) -> bool {
    url.starts_with(crate::INTERNAL)
}

pub fn host(url: &str) -> Option<String> {
    url::Url::parse(url).ok()?.host_str().map(str::to_owned)
}

/// Turn whatever the user typed into a URL: a full URL, a bare domain, or a search query.
pub fn resolve_input(input: &str, search_template: &str) -> String {
    let s = input.trim();
    if s.is_empty() {
        return crate::NEWTAB_URL.to_string();
    }
    if let Some(page) = s.strip_prefix("otium://") {
        return format!("{}{}", crate::INTERNAL, page.trim_end_matches('/'));
    }
    let has_scheme = ["http://", "https://", "file://", "about:", "data:", "edge://"]
        .iter()
        .any(|p| s.to_ascii_lowercase().starts_with(p));
    if has_scheme {
        return s.to_string();
    }
    if !s.contains(char::is_whitespace) {
        let lower = s.to_ascii_lowercase();
        let host_part = lower.split(['/', '?', '#']).next().unwrap_or("");
        let host_only = host_part.split(':').next().unwrap_or("");
        let is_local = host_only == "localhost"
            || host_only.parse::<std::net::Ipv4Addr>().is_ok();
        if is_local {
            return format!("http://{s}");
        }
        let tld_ok = host_only
            .rsplit_once('.')
            .map(|(name, tld)| !name.is_empty() && tld.len() >= 2 && tld.chars().all(|c| c.is_alphabetic()))
            .unwrap_or(false);
        if tld_ok {
            return format!("https://{s}");
        }
    }
    let q: String = url::form_urlencoded::byte_serialize(s.as_bytes()).collect();
    search_template.replace("%s", &q)
}

pub use crate::sys::{app_icon, memory, message_box, today, uninstall};

#[cfg(test)]
mod tests {
    const G: &str = "https://www.google.com/search?q=%s";
    fn resolve_input(s: &str) -> String {
        super::resolve_input(s, G)
    }

    #[test]
    fn resolves() {
        assert_eq!(resolve_input("otium://settings"), "http://otium.localhost/settings");
        assert_eq!(resolve_input("github.com"), "https://github.com");
        assert_eq!(resolve_input("localhost:3000"), "http://localhost:3000");
        assert_eq!(resolve_input("https://a.b/c"), "https://a.b/c");
        assert!(resolve_input("rust lang").starts_with("https://www.google.com/search?q=rust+lang"));
        assert!(resolve_input("v1.2").starts_with("https://www.google.com/search"));
    }
}
