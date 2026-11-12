//! OTIUM — a fast, minimal, stylish browser.
//!
//! Architecture: one frameless native window (tao) hosting several child
//! WebView2 instances (wry):
//! - the "chrome" webview draws the window UI (sidebar or classic bar, tabs, address bar);
//! - one content webview per tab, layered over the chrome;
//! - an "overlay" webview for popovers (suggestions, menus, downloads, extensions)
//!   that is raised above everything when shown;
//! - an optional popup webview for browser-extension popups.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod extensions;
mod instance;
mod integrate;
mod native;
mod settings;
mod shield;
mod store;
mod sys;
mod update;
mod util;
mod vault;

use std::{
    borrow::Cow,
    cell::{Cell, RefCell},
    collections::HashMap,
    fs,
    io::Write as _,
    path::{Path, PathBuf},
    rc::Rc,
    sync::{Arc, RwLock},
    time::{Duration, Instant},
};

use extensions::ExtMeta;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use settings::Settings;
use tao::{
    dpi::{LogicalPosition, LogicalSize, PhysicalPosition, PhysicalSize},
    event::{Event, WindowEvent},
    event_loop::{ControlFlow, EventLoopBuilder, EventLoopProxy},
    window::{Fullscreen, ResizeDirection, Window, WindowBuilder},
};
use webview2_com::Microsoft::Web::WebView2::Win32::ICoreWebView2DownloadOperation;
use wry::{
    http::{header::CONTENT_TYPE, Request, Response},
    MemoryUsageLevel, NewWindowResponse, PageLoadEvent, Rect, ScrollBarStyle, Theme, WebContext,
    WebView, WebViewBuilder, WebViewBuilderExtWindows, WebViewExtWindows,
};

#[cfg(windows)]
use tao::platform::windows::WindowBuilderExtWindows;

const CHROME_HTML: &str = include_str!("ui/chrome.html");
const NEWTAB_HTML: &str = include_str!("ui/newtab.html");
const SETTINGS_HTML: &str = include_str!("ui/settings.html");
const HISTORY_HTML: &str = include_str!("ui/history.html");
const OVERLAY_HTML: &str = include_str!("ui/overlay.html");
const CONTENT_JS: &str = include_str!("ui/content.js");
const ICON_PNG: &[u8] = include_bytes!("../assets/icon.png");

pub const INTERNAL: &str = "http://otium.localhost/";
pub const NEWTAB_URL: &str = "http://otium.localhost/newtab";
pub const SETTINGS_URL: &str = "http://otium.localhost/settings";
pub const HISTORY_URL: &str = "http://otium.localhost/history";
pub const BOOKMARKS_URL: &str = "http://otium.localhost/bookmarks";
const OVERLAY_URL: &str = "http://otium.localhost/overlay";
/// With "sleep tabs" on, background tabs stay alive while RAM is plentiful. Under memory
/// pressure the longest-unused ones are unloaded first (like Chrome's Memory Saver); tabs
/// forgotten for hours are unloaded regardless.
const TICK: Duration = Duration::from_secs(20);
/// a tab you just left is never unloaded, even under pressure
const DISCARD_MIN_IDLE: Duration = Duration::from_secs(60);
const DISCARD_FORGOTTEN: Duration = Duration::from_secs(2 * 60 * 60);
/// pressure: RAM load % or free bytes below which tabs get unloaded
const PRESSURE_LOAD: u32 = 85;
const PRESSURE_FREE: u64 = 1536 << 20;
/// critical: unload several tabs per tick instead of one
const CRITICAL_LOAD: u32 = 93;
const BG: (u8, u8, u8, u8) = (10, 10, 14, 255);
/// wry's default WebView2 flags (replaced when we pass our own).
const BASE_ARGS: &str = "--disable-features=msWebOOUI,msPdfOOUI,msSmartScreenProtection";

#[derive(Debug)]
enum UserEvent {
    /// from the chrome or overlay webview (trusted)
    Chrome(String),
    /// (tab id, sender document URL, message)
    Content(u32, String, String),
    Title(u32, String),
    Load(u32, bool, String),
    OpenTab(String, bool),
    Fullscreen(u32, bool),
    Audio(u32, bool),
    Download(native::DownloadEvent),
    ShieldBuilt,
    ShieldTick,
    Suggest(u64, Vec<String>),
    ExtDownloaded(Result<PathBuf, String>),
    ExtAdded(PathBuf, Result<native::ExtInfo, String>),
    ExtList(Vec<native::ExtInfo>),
    /// periodic housekeeping (unloading idle background tabs, update checks)
    Tick,
    /// a link (or "" to just show the window) from a later launch of the browser
    Remote(String),
    /// result of an update check: Ok(Some((version, exe))) when one is downloaded
    Update(Result<Option<(String, PathBuf)>, String>),
    Exit,
}

/// Commands sent from the chrome UI, overlay and internal pages.
#[derive(Deserialize)]
#[serde(tag = "cmd", rename_all = "snake_case")]
enum Cmd {
    Ready,
    Navigate { text: String },
    NewTab,
    NewPrivateTab,
    CloseTab { id: u32 },
    SelectTab { id: u32 },
    MoveTab { id: u32, to: usize },
    Back,
    Forward,
    Reload,
    Minimize,
    Maximize,
    Close,
    Drag,
    Resize { dir: String },
    /// Content area insets in logical px: left, top, and right/bottom margin.
    Layout { x: f64, y: f64, m: f64 },
    Key { k: String },
    Favicon { href: String },
    Cosmetic { classes: Vec<String>, ids: Vec<String> },
    Dismiss,
    OpenSettings,
    Set { patch: Value },
    Restart,
    ClearData,
    // popovers
    Popover { kind: String, x: f64, y: f64, w: f64, #[serde(default)] arg: Value },
    OvSize { h: f64 },
    Hide,
    Suggest { q: String, x: f64, y: f64, w: f64 },
    SuggestSel { i: i64 },
    TabAction { id: u32, action: String },
    Menu { action: String },
    // bookmarks / history
    Bookmark,
    Data { what: String, #[serde(default)] q: String, #[serde(default)] offset: i64 },
    DeleteHistory { url: String },
    ClearHistory,
    DeleteBookmark { url: String },
    // shield
    ShieldToggleSite,
    ShieldUpdate,
    // downloads
    DownloadOpen { id: u32, folder: bool },
    DownloadCancel { id: u32 },
    DownloadsClear,
    // extensions
    ExtInstall { url: String },
    ExtFolder,
    ExtToggle { id: String, on: bool },
    ExtRemove { id: String },
    ExtOpen { id: String, page: String },
    // account & vault
    AccountCreate { name: String, kind: String, secret: String },
    AccountDelete { secret: String },
    VaultExport { secret: String },
    VaultPick,
    VaultImport { path: String, secret: String },
    // system integration
    MakeDefault,
    UpdateCheck,
    UpdateNow,
}

/// Auto-update progress (only the installed copy updates itself).
#[derive(Default)]
struct UpdateState {
    busy: bool,
    /// downloaded and verified: (version, exe path)
    ready: Option<(String, PathBuf)>,
    error: String,
    up_to_date: bool,
    checked: Option<Instant>,
}

/// how often the installed browser looks for a new release
const UPDATE_EVERY: Duration = Duration::from_secs(6 * 60 * 60);
/// first check shortly after start, not during it
const UPDATE_FIRST: Duration = Duration::from_secs(40);

struct Tab {
    id: u32,
    /// `None` while the tab is unloaded (restored lazily or discarded when idle)
    view: Option<WebView>,
    /// last time this tab was the active one
    seen: Instant,
    url: String,
    title: String,
    favicon: String,
    loading: bool,
    private: bool,
    pinned: bool,
    audio: bool,
    muted: bool,
    zoom: f64,
}

impl Tab {
    fn new(id: u32, view: Option<WebView>, url: &str, private: bool) -> Self {
        Tab {
            id,
            loading: view.is_some(),
            view,
            seen: Instant::now(),
            url: url.to_string(),
            title: String::new(),
            favicon: String::new(),
            private,
            pinned: false,
            audio: false,
            muted: false,
            zoom: 1.0,
        }
    }
}

#[derive(Serialize, Deserialize, Default)]
#[serde(default)]
struct Session {
    tabs: Vec<String>,
    /// titles and favicons, so unloaded tabs still look right in the strip
    titles: Vec<String>,
    favicons: Vec<String>,
    pinned: Vec<usize>,
    active: usize,
    /// window geometry in logical px: x, y, w, h
    window: Option<(f64, f64, f64, f64)>,
    maximized: bool,
}

#[derive(Serialize, Clone)]
struct Download {
    id: u32,
    url: String,
    path: String,
    name: String,
    received: i64,
    total: i64,
    /// "progress" | "done" | "failed"
    state: &'static str,
}

/// Shared with request-filter closures (UI thread only).
struct ShieldState {
    engine: Option<shield::Shield>,
    enabled: bool,
    allow: Vec<String>,
    counts: HashMap<u32, u32>,
    total: u64,
    tick_pending: bool,
}

struct App {
    // Field order matters: webviews must be dropped before the window.
    tabs: Vec<Tab>,
    popup: Option<WebView>,
    overlay: WebView,
    chrome: WebView,
    ctx: WebContext,
    proxy: EventLoopProxy<UserEvent>,
    settings: Arc<RwLock<Settings>>,
    store: Option<store::Store>,
    shield: Rc<RefCell<ShieldState>>,
    shield_updating: bool,
    downloads: Vec<Download>,
    dl_ops: Rc<RefCell<Vec<(u32, ICoreWebView2DownloadOperation)>>>,
    dl_next: Rc<Cell<u32>>,
    exts: Vec<ExtMeta>,
    ext_busy: bool,
    closed: Vec<String>,
    /// kind of the visible popover, if any
    popover: Option<String>,
    /// last closed popover, so a click on its toggle button doesn't reopen it at once
    last_hidden: Option<(String, Instant)>,
    suggest_seq: u64,
    suggest_items: Vec<Value>,
    /// whether the WebView2 environment was started with forced dark mode
    force_dark_active: bool,
    browser_args: String,
    active: usize,
    next_id: u32,
    inset: (f64, f64, f64),
    fullscreen: bool,
    html_fullscreen: bool,
    data_dir: PathBuf,
    /// path of the installed exe; None for dev/portable builds (no registration, no updates)
    installed: Option<PathBuf>,
    update: UpdateState,
    started: Instant,
    window: Window,
}

fn html_response(status: u16, body: Cow<'static, [u8]>) -> Response<Cow<'static, [u8]>> {
    Response::builder()
        .status(status)
        .header(CONTENT_TYPE, "text/html; charset=utf-8")
        .header("X-Frame-Options", "DENY")
        // internal pages carry live settings; never let the HTTP cache serve a stale copy
        .header("Cache-Control", "no-store")
        .header("Content-Security-Policy", "frame-ancestors 'none'")
        .body(body)
        .unwrap()
}

fn serve_internal(req: Request<Vec<u8>>, payload: Value) -> Response<Cow<'static, [u8]>> {
    let page = match req.uri().path() {
        "/" | "/newtab" => NEWTAB_HTML,
        "/settings" => SETTINGS_HTML,
        "/history" | "/bookmarks" => HISTORY_HTML,
        "/overlay" => OVERLAY_HTML,
        "/icon.png" => {
            return Response::builder()
                .header(CONTENT_TYPE, "image/png")
                .body(Cow::Borrowed(ICON_PNG))
                .unwrap()
        }
        _ => return html_response(404, Cow::Borrowed(b"<h1>404</h1>")),
    };
    let html = page.replacen("/*__OTIUM__*/null", &settings::script_json(&payload), 1);
    html_response(200, Cow::Owned(html.into_bytes()))
}

fn theme_of(s: &Settings) -> Theme {
    match s.site_theme.as_str() {
        "dark" => Theme::Dark,
        "light" => Theme::Light,
        _ => Theme::Auto,
    }
}

fn file_name(path: &str) -> String {
    Path::new(path).file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default()
}

fn rect(x: f64, y: f64, w: f64, h: f64) -> Rect {
    Rect { position: LogicalPosition::new(x, y).into(), size: LogicalSize::new(w.max(1.0), h.max(1.0)).into() }
}

impl App {
    fn new(window: Window, proxy: EventLoopProxy<UserEvent>, session: Session, target: Option<String>) -> wry::Result<Self> {
        let data_dir = util::data_dir();
        let _ = fs::create_dir_all(&data_dir);
        let settings = Settings::load(&data_dir.join("settings.json"));
        let force_dark_active = settings.force_dark;
        let mut browser_args = BASE_ARGS.to_string();
        if force_dark_active {
            browser_args.push_str(" --enable-features=WebContentsForceDark");
        }
        let mut ctx = WebContext::new(Some(data_dir.join("profile")));

        // After a restart the previous browser processes may still be shutting
        // down; WebView2 refuses a different config on the same profile until then.
        let mut attempt = 0;
        let chrome = loop {
            let p = proxy.clone();
            let built = Self::builder(&mut ctx, &browser_args)
                .with_html(CHROME_HTML)
                .with_bounds(full_rect(&window))
                .with_hotkeys_zoom(false)
                .with_default_context_menus(false)
                .with_browser_accelerator_keys(false)
                .with_ipc_handler(move |req| {
                    let _ = p.send_event(UserEvent::Chrome(req.into_body()));
                })
                .build_as_child(&window);
            match built {
                Ok(v) => break v,
                Err(e) if attempt < 40 => {
                    attempt += 1;
                    native::log::warn(&format!("chrome init retry {attempt}: {e}"));
                    std::thread::sleep(Duration::from_millis(250));
                }
                Err(e) => return Err(e),
            }
        };
        let _ = chrome.set_theme(theme_of(&settings));

        let shared = Arc::new(RwLock::new(settings));
        let overlay = {
            let p = proxy.clone();
            let sh = shared.clone();
            Self::builder(&mut ctx, &browser_args)
                .with_url(OVERLAY_URL)
                .with_bounds(rect(0.0, 0.0, 1.0, 1.0))
                .with_visible(false)
                .with_background_color((24, 24, 32, 255))
                .with_hotkeys_zoom(false)
                .with_default_context_menus(false)
                .with_browser_accelerator_keys(false)
                .with_custom_protocol("otium".into(), move |_, req| {
                    let payload = sh.read().unwrap().payload(force_dark_active);
                    serve_internal(req, payload)
                })
                .with_ipc_handler(move |req| {
                    // only trust messages from our own overlay page
                    if req.uri().to_string().starts_with(OVERLAY_URL) {
                        let _ = p.send_event(UserEvent::Chrome(req.into_body()));
                    }
                })
                .build_as_child(&window)?
        };

        let cfg = shared.read().unwrap().clone();
        let store = store::Store::open(&data_dir.join("otium.db")).ok();
        let shield_dir = data_dir.join("shield");
        let cached = shield::Shield::load_cached(&shield_dir);
        let needs_build = cached.as_ref().is_none_or(|(_, fresh)| !*fresh);
        let shield = Rc::new(RefCell::new(ShieldState {
            engine: cached.map(|(s, _)| s),
            enabled: cfg.shield,
            allow: cfg.shield_allow.clone(),
            counts: HashMap::new(),
            total: 0,
            tick_pending: false,
        }));
        let exts = extensions::load_index(&data_dir.join("extensions"));

        let mut app = App {
            tabs: Vec::new(),
            popup: None,
            overlay,
            chrome,
            ctx,
            proxy,
            settings: shared,
            store,
            shield,
            shield_updating: false,
            downloads: Vec::new(),
            dl_ops: Rc::new(RefCell::new(Vec::new())),
            dl_next: Rc::new(Cell::new(1)),
            exts,
            ext_busy: false,
            closed: Vec::new(),
            popover: None,
            last_hidden: None,
            suggest_seq: 0,
            suggest_items: Vec::new(),
            force_dark_active,
            browser_args,
            active: 0,
            next_id: 1,
            inset: (264.0, 8.0, 8.0),
            fullscreen: false,
            html_fullscreen: false,
            data_dir,
            installed: integrate::installed_exe(),
            update: UpdateState::default(),
            started: Instant::now(),
            window,
        };
        if let Some(exe) = &app.installed {
            update::cleanup(exe, &app.data_dir.join("update"));
            integrate::ensure_registered(exe, env!("CARGO_PKG_VERSION"));
        }

        if cfg.restore_session && !session.tabs.is_empty() {
            let active = session.active.min(session.tabs.len() - 1);
            // only the active tab is loaded now; the rest load on first visit
            for (i, url) in session.tabs.iter().enumerate() {
                let id = app.next_id;
                app.next_id += 1;
                let mut tab = Tab::new(id, None, url, false);
                tab.title = session.titles.get(i).cloned().unwrap_or_default();
                tab.favicon = session.favicons.get(i).cloned().unwrap_or_default();
                tab.pinned = session.pinned.contains(&i);
                app.tabs.push(tab);
            }
            match &target {
                Some(url) => app.open_tab(url, true, false),
                None => app.activate(active),
            }
        } else {
            app.open_tab(target.as_deref().unwrap_or(NEWTAB_URL), true, false);
        }
        if needs_build {
            app.rebuild_shield();
        }
        app.register_missing_extensions();
        Ok(app)
    }

    /// Builder with the environment options every webview must share.
    fn builder<'a>(ctx: &'a mut WebContext, args: &str) -> WebViewBuilder<'a> {
        WebViewBuilder::new_with_web_context(ctx)
            .with_background_color(BG)
            .with_additional_browser_args(args)
            .with_browser_extensions_enabled(true)
            .with_scroll_bar_style(ScrollBarStyle::FluentOverlay)
    }

    fn cfg(&self) -> Settings {
        self.settings.read().unwrap().clone()
    }

    fn session_path(&self) -> PathBuf {
        self.data_dir.join("session.json")
    }

    fn save_session(&self) {
        let normal: Vec<&Tab> = self.tabs.iter().filter(|t| !t.private).collect();
        let active_id = self.current().map(|t| t.id);
        let pos = self.window.outer_position().ok().map(|p| p.to_logical::<f64>(self.window.scale_factor()));
        let size = self.window.inner_size().to_logical::<f64>(self.window.scale_factor());
        let s = Session {
            tabs: normal.iter().map(|t| t.url.clone()).collect(),
            titles: normal.iter().map(|t| t.title.clone()).collect(),
            favicons: normal.iter().map(|t| t.favicon.clone()).collect(),
            pinned: normal.iter().enumerate().filter(|(_, t)| t.pinned).map(|(i, _)| i).collect(),
            active: normal.iter().position(|t| Some(t.id) == active_id).unwrap_or(0),
            window: if self.window.is_maximized() || self.fullscreen {
                fs::read_to_string(self.session_path())
                    .ok()
                    .and_then(|s| serde_json::from_str::<Session>(&s).ok())
                    .and_then(|s| s.window)
            } else {
                pos.map(|p| (p.x, p.y, size.width, size.height))
            },
            maximized: self.window.is_maximized(),
        };
        if let Ok(text) = serde_json::to_string(&s) {
            let _ = fs::write(self.session_path(), text);
        }
    }

    /// Area of the window covered by the active content webview.
    fn content_rect(&self) -> Rect {
        if self.fullscreen {
            return full_rect(&self.window);
        }
        let size = self.window.inner_size();
        let s = self.window.scale_factor();
        let (x, y, m) = self.inset;
        let (x, y, m) = ((x * s).round(), (y * s).round(), (m * s).round());
        let w = (size.width as f64 - x - m).max(1.0);
        let h = (size.height as f64 - y - m).max(1.0);
        Rect {
            position: PhysicalPosition::new(x, y).into(),
            size: PhysicalSize::new(w as u32, h as u32).into(),
        }
    }

    fn relayout(&mut self) {
        self.hide_popover();
        let _ = self.chrome.set_bounds(full_rect(&self.window));
        let r = self.content_rect();
        for v in self.tabs.iter().filter_map(|t| t.view.as_ref()) {
            let _ = v.set_bounds(r);
        }
        self.sync();
    }

    fn open_tab(&mut self, url: &str, activate: bool, private: bool) {
        let id = self.next_id;
        self.next_id += 1;
        let Some(view) = self.make_view(id, url, private) else { return };
        let tab = Tab::new(id, Some(view), url, private);
        // new tabs go right after the current one (pinned tabs stay first)
        let pos = if self.tabs.is_empty() { 0 } else { (self.active + 1).max(self.pinned_count()).min(self.tabs.len()) };
        self.tabs.insert(pos, tab);
        if !activate && pos <= self.active && !self.tabs.is_empty() {
            self.active += 1;
        }
        if activate {
            self.activate(pos);
        } else {
            if self.settings.read().unwrap().sleep_tabs {
                if let Some(v) = &self.tabs[pos].view {
                    let _ = v.set_memory_usage_level(MemoryUsageLevel::Low);
                }
            }
            self.sync();
        }
    }

    /// Create the content webview for a tab (hidden; `activate` shows it).
    fn make_view(&mut self, id: u32, url: &str, private: bool) -> Option<WebView> {
        let bounds = self.content_rect();
        let theme = theme_of(&self.settings.read().unwrap());

        let p = self.proxy.clone();
        let (p_ipc, p_title, p_load, p_new, p_fs, p_audio, p_dl) =
            (p.clone(), p.clone(), p.clone(), p.clone(), p.clone(), p.clone(), p.clone());
        let shared = self.settings.clone();
        let fd = self.force_dark_active;
        let view = Self::builder(&mut self.ctx, &self.browser_args)
            .with_url(url)
            .with_bounds(bounds)
            .with_visible(false)
            .with_devtools(true)
            .with_incognito(private)
            .with_initialization_script(CONTENT_JS)
            .with_custom_protocol("otium".into(), move |_, req| {
                let payload = shared.read().unwrap().payload(fd);
                serve_internal(req, payload)
            })
            .with_ipc_handler(move |req| {
                let uri = req.uri().to_string();
                let _ = p_ipc.send_event(UserEvent::Content(id, uri, req.into_body()));
            })
            .with_document_title_changed_handler(move |title| {
                let _ = p_title.send_event(UserEvent::Title(id, title));
            })
            .with_on_page_load_handler(move |ev, url| {
                let started = matches!(ev, PageLoadEvent::Started);
                let _ = p_load.send_event(UserEvent::Load(id, started, url));
            })
            .with_new_window_req_handler(move |url, _features| {
                let _ = p_new.send_event(UserEvent::OpenTab(url, private));
                NewWindowResponse::Deny
            })
            .build_as_child(&self.window);

        let view = match view {
            Ok(v) => v,
            Err(e) => {
                native::log::warn(&format!("failed to create tab: {e}"));
                return None;
            }
        };
        let _ = view.set_theme(theme);
        native::enable_autofill(&view);
        native::on_fullscreen(&view, move |on| {
            let _ = p_fs.send_event(UserEvent::Fullscreen(id, on));
        });
        native::on_audio(&view, move |on| {
            let _ = p_audio.send_event(UserEvent::Audio(id, on));
        });
        native::on_downloads(&view, self.dl_next.clone(), self.dl_ops.clone(), move |ev| {
            let _ = p_dl.send_event(UserEvent::Download(ev));
        });
        let sh = self.shield.clone();
        let p_tick = self.proxy.clone();
        native::install_request_filter(&view, move |req_url, page, kind| {
            let mut s = sh.borrow_mut();
            if !s.enabled || s.engine.is_none() {
                return false;
            }
            let host = util::host(page).unwrap_or_default();
            if s.allow.iter().any(|a| host == *a || host.ends_with(&format!(".{a}"))) {
                return false;
            }
            let block = s.engine.as_ref().unwrap().should_block(req_url, page, kind);
            if block {
                *s.counts.entry(id).or_default() += 1;
                s.total += 1;
                if !s.tick_pending {
                    s.tick_pending = true;
                    let _ = p_tick.send_event(UserEvent::ShieldTick);
                }
            }
            block
        });
        Some(view)
    }

    /// Load an unloaded tab back into a live webview.
    fn ensure_loaded(&mut self, idx: usize) {
        if self.tabs[idx].view.is_some() {
            return;
        }
        let (id, url, private) = (self.tabs[idx].id, self.tabs[idx].url.clone(), self.tabs[idx].private);
        let Some(view) = self.make_view(id, &url, private) else { return };
        let t = &mut self.tabs[idx];
        if (t.zoom - 1.0).abs() > f64::EPSILON {
            let _ = view.zoom(t.zoom);
        }
        if t.muted {
            native::set_muted(&view, true);
        }
        t.loading = true;
        t.view = Some(view);
    }

    /// Unload background tabs: the longest-unused first while RAM is short, plus any
    /// tab nobody has looked at for hours.
    fn discard_idle(&mut self) {
        if !self.settings.read().unwrap().sleep_tabs {
            return;
        }
        // a download may belong to a background tab; dropping it would cancel the download
        if self.downloads.iter().any(|d| d.state == "progress") {
            return;
        }
        let active = self.active;
        // candidates, longest-unused first
        let mut idle: Vec<usize> = (0..self.tabs.len())
            .filter(|&i| {
                let t = &self.tabs[i];
                i != active && t.view.is_some() && !t.audio && !t.private && t.seen.elapsed() >= DISCARD_MIN_IDLE
            })
            .collect();
        idle.sort_by_key(|&i| self.tabs[i].seen);

        let (load, free) = util::memory().unwrap_or((0, u64::MAX));
        let budget = if load >= CRITICAL_LOAD {
            3
        } else if load >= PRESSURE_LOAD || free < PRESSURE_FREE {
            1
        } else {
            0
        };
        let mut drop: Vec<usize> = idle.iter().take(budget).copied().collect();
        drop.extend(idle.iter().skip(budget).filter(|&&i| self.tabs[i].seen.elapsed() >= DISCARD_FORGOTTEN));
        if drop.is_empty() {
            return;
        }
        for i in drop {
            let t = &mut self.tabs[i];
            t.view = None; // dropping the WebView frees its renderer process
            t.loading = false;
        }
        self.sync();
    }

    fn pinned_count(&self) -> usize {
        self.tabs.iter().filter(|t| t.pinned).count()
    }

    fn activate(&mut self, idx: usize) {
        if idx >= self.tabs.len() {
            return;
        }
        self.hide_popover();
        let sleep = self.settings.read().unwrap().sleep_tabs;
        let r = self.content_rect();
        if let Some(prev) = self.tabs.get_mut(self.active) {
            prev.seen = Instant::now();
        }
        self.ensure_loaded(idx);
        self.tabs[idx].seen = Instant::now();
        if let Some(new) = &self.tabs[idx].view {
            let _ = new.set_memory_usage_level(MemoryUsageLevel::Normal);
            let _ = new.set_bounds(r);
            let _ = new.set_visible(true);
        }
        for (i, t) in self.tabs.iter().enumerate() {
            if i != idx {
                let Some(v) = &t.view else { continue };
                let _ = v.set_visible(false);
                if sleep && !t.audio {
                    let _ = v.set_memory_usage_level(MemoryUsageLevel::Low);
                }
            }
        }
        self.active = idx;
        if let Some(v) = &self.tabs[idx].view {
            let _ = v.focus();
        }
        self.sync();
    }

    fn close_tab(&mut self, id: u32) {
        let Some(idx) = self.index_of(id) else { return };
        let was_active = idx == self.active;
        let tab = self.tabs.remove(idx); // dropping the WebView destroys it
        if !tab.private && !util::is_internal(&tab.url) || tab.url.starts_with(SETTINGS_URL) {
            self.closed.push(tab.url.clone());
            if self.closed.len() > 25 {
                self.closed.remove(0);
            }
        }
        drop(tab);
        self.shield.borrow_mut().counts.remove(&id);
        if self.tabs.is_empty() {
            self.open_tab(NEWTAB_URL, true, false);
        } else if was_active {
            self.activate(idx.min(self.tabs.len() - 1));
        } else {
            if idx < self.active {
                self.active -= 1;
            }
            self.sync();
        }
        self.save_session();
    }

    fn open_internal(&mut self, url: &str) {
        match self.tabs.iter().position(|t| t.url.starts_with(url)) {
            Some(i) => self.activate(i),
            None => self.open_tab(url, true, false),
        }
    }

    fn index_of(&self, id: u32) -> Option<usize> {
        self.tabs.iter().position(|t| t.id == id)
    }

    fn current(&self) -> Option<&Tab> {
        self.tabs.get(self.active)
    }

    fn set_fullscreen(&mut self, on: bool) {
        self.fullscreen = on;
        self.window.set_fullscreen(on.then(|| Fullscreen::Borderless(None)));
        self.relayout();
    }

    // ------------------------------------------------------------ popovers

    /// Show the overlay webview at a rect given in logical window coordinates.
    fn show_popover(&mut self, kind: &str, x: f64, y: f64, w: f64, h: f64, mut data: Value, focus: bool) {
        self.close_ext_popup();
        if let Value::Object(m) = &mut data {
            m.insert("_accent".into(), json!(self.settings.read().unwrap().accent_colors()));
        }
        let size = self.window.inner_size().to_logical::<f64>(self.window.scale_factor());
        let x = x.clamp(4.0, (size.width - w - 4.0).max(4.0));
        let h = h.min(size.height - y - 8.0);
        let _ = self.overlay.set_bounds(rect(x, y, w, h));
        let js = format!("window.ov && ov.show({}, {})", json!(kind), settings::script_json(&data));
        let _ = self.overlay.evaluate_script(&js);
        let _ = self.overlay.set_visible(true);
        native::bring_to_top(&self.overlay);
        if focus {
            let _ = self.overlay.focus();
        }
        self.popover = Some(kind.to_string());
    }

    fn hide_popover(&mut self) {
        if let Some(k) = self.popover.take() {
            self.last_hidden = Some((k, std::time::Instant::now()));
            let _ = self.overlay.set_visible(false);
            let _ = self.overlay.evaluate_script("window.ov && ov.hide()");
            let _ = self.chrome.evaluate_script("window.otium && otium.popoverClosed()");
        }
        self.close_ext_popup();
    }

    fn close_ext_popup(&mut self) {
        self.popup = None;
    }

    fn popover_data(&self, kind: &str, arg: &Value) -> Value {
        match kind {
            "downloads" => json!({ "items": self.downloads }),
            "extensions" => json!({ "items": self.exts_json(), "busy": self.ext_busy }),
            "shield" => self.shield_json(),
            "tabmenu" => {
                let id = arg.as_u64().unwrap_or(0) as u32;
                match self.index_of(id).map(|i| &self.tabs[i]) {
                    Some(t) => json!({ "id": id, "pinned": t.pinned, "muted": t.muted, "private": t.private, "count": self.tabs.len() }),
                    None => Value::Null,
                }
            }
            "menu" => json!({
                "zoom": self.current().map(|t| t.zoom).unwrap_or(1.0),
                "canReopen": !self.closed.is_empty(),
            }),
            _ => Value::Null,
        }
    }

    fn refresh_popover(&self, kind: &str) {
        if self.popover.as_deref() == Some(kind) {
            let js = format!("window.ov && ov.update({})", settings::script_json(&self.popover_data(kind, &Value::Null)));
            let _ = self.overlay.evaluate_script(&js);
        }
    }

    // ------------------------------------------------------------ shield

    fn rebuild_shield(&mut self) {
        if self.shield_updating {
            return;
        }
        self.shield_updating = true;
        let dir = self.data_dir.join("shield");
        let p = self.proxy.clone();
        std::thread::spawn(move || {
            let _ = shield::Shield::build(&dir);
            let _ = p.send_event(UserEvent::ShieldBuilt);
        });
    }

    fn shield_json(&self) -> Value {
        let s = self.shield.borrow();
        let host = self.current().and_then(|t| util::host(&t.url)).unwrap_or_default();
        let site_on = !s.allow.iter().any(|a| host == *a || host.ends_with(&format!(".{a}")));
        json!({
            "ready": s.engine.is_some(),
            "updating": self.shield_updating,
            "enabled": s.enabled,
            "host": host,
            "siteOn": site_on,
            "blocked": self.current().and_then(|t| s.counts.get(&t.id)).copied().unwrap_or(0),
            "total": s.total,
            "lists": shield::LISTS.iter().map(|l| l.0).collect::<Vec<_>>(),
        })
    }

    // ------------------------------------------------------------ extensions

    fn ext_root(&self) -> PathBuf {
        self.data_dir.join("extensions")
    }

    fn exts_json(&self) -> Value {
        json!(self
            .exts
            .iter()
            .map(|e| json!({"id": e.id, "name": e.name, "version": e.version, "icon": e.icon, "popup": e.popup, "options": e.options, "enabled": e.enabled}))
            .collect::<Vec<_>>())
    }

    fn register_extension(&mut self, dir: PathBuf) {
        let p = self.proxy.clone();
        let d = dir.clone();
        native::add_extension(&self.chrome, &dir, move |res| {
            let _ = p.send_event(UserEvent::ExtAdded(d, res));
        });
    }

    /// Extensions copied from another machine (vault import) need registering again.
    fn register_missing_extensions(&mut self) {
        let missing: Vec<PathBuf> = self.exts.iter().filter(|e| e.id.is_empty()).map(|e| e.dir.clone()).collect();
        for d in missing {
            self.register_extension(d);
        }
        let p = self.proxy.clone();
        native::with_extensions(&self.chrome, |_| {}, move |list| {
            let _ = p.send_event(UserEvent::ExtList(list));
        });
    }

    fn with_ext(&self, id: &str, f: impl Fn(&webview2_com::Microsoft::Web::WebView2::Win32::ICoreWebView2BrowserExtension) + 'static) {
        let id = id.to_string();
        let p = self.proxy.clone();
        native::with_extensions(
            &self.chrome,
            move |e| {
                if native::ext_id(e) == id {
                    f(e)
                }
            },
            move |_| {
                // re-read the list once the action had time to apply
                let _ = p.send_event(UserEvent::Chrome(r#"{"cmd":"key","k":"__ext_refresh"}"#.into()));
            },
        );
    }

    fn open_ext_popup(&mut self, id: &str, page: &str) {
        let size = self.window.inner_size().to_logical::<f64>(self.window.scale_factor());
        let (w, h) = (380.0, 560.0f64.min(size.height - 100.0));
        let classic = self.settings.read().unwrap().layout == "classic";
        let (x, y) = if classic { (size.width - w - 12.0, 86.0) } else { (self.inset.0 + 8.0, 50.0) };
        self.hide_popover();
        let url = format!("chrome-extension://{id}/{page}");
        let popup = Self::builder(&mut self.ctx, &self.browser_args)
            .with_url(&url)
            .with_bounds(rect(x, y, w, h))
            .with_background_color((255, 255, 255, 255))
            .with_devtools(true)
            .build_as_child(&self.window);
        if let Ok(v) = popup {
            native::bring_to_top(&v);
            let _ = v.focus();
            self.popup = Some(v);
        }
    }

    // ------------------------------------------------------------ internal pages

    fn runtime_json(&self) -> Value {
        let acc = vault::load_account(&self.data_dir);
        json!({
            "extensions": self.exts_json(),
            "extBusy": self.ext_busy,
            "account": acc.map(|a| json!({"name": a.name, "kind": a.kind, "created": a.created})),
            "shield": self.shield_json(),
            "dataDir": self.data_dir.to_string_lossy(),
            "installed": self.installed.is_some(),
            "isDefault": self.installed.is_some() && integrate::is_default(),
            "update": {
                "busy": self.update.busy,
                "ready": self.update.ready.as_ref().map(|r| &r.0),
                "error": self.update.error,
                "upToDate": self.update.up_to_date,
            },
        })
    }

    // ------------------------------------------------------------ updates

    fn start_update_check(&mut self) {
        if self.installed.is_none() || self.update.busy || self.update.ready.is_some() {
            return;
        }
        self.update.busy = true;
        self.update.error.clear();
        self.update.checked = Some(Instant::now());
        let dir = self.data_dir.join("update");
        let p = self.proxy.clone();
        std::thread::spawn(move || {
            let res = update::check_and_download(env!("CARGO_PKG_VERSION"), &dir);
            let _ = p.send_event(UserEvent::Update(res));
        });
        self.push_internal("runtime", &self.runtime_json());
    }

    /// Swap in a downloaded update; it runs from the next start. Called on every exit path.
    fn install_pending_update(&mut self) {
        let (Some(exe), Some((ver, path))) = (self.installed.clone(), self.update.ready.take()) else { return };
        if let Err(e) = update::install(&exe, &path) {
            native::log::warn(&format!("update {ver} not installed: {e}"));
        }
    }

    fn push_internal(&self, what: &str, data: &Value) {
        let js = format!("window.otiumData && otiumData({}, {})", json!(what), settings::script_json(data));
        for t in self.tabs.iter().filter(|t| util::is_internal(&t.url)) {
            if let Some(v) = &t.view {
                let _ = v.evaluate_script(&js);
            }
        }
    }

    fn reply(&self, idx: usize, what: &str, data: &Value) {
        let js = format!("window.otiumData && otiumData({}, {})", json!(what), settings::script_json(data));
        if let Some(v) = &self.tabs[idx].view {
            let _ = v.evaluate_script(&js);
        }
    }

    fn toast(&self, idx: usize, msg: &str, ok: bool) {
        self.reply(idx, "toast", &json!({"msg": msg, "ok": ok}));
    }

    /// Apply a settings patch coming from an internal page.
    /// Returns whether the change was saved to disk.
    fn apply_patch(&mut self, patch: &Value) -> bool {
        let old = self.cfg();
        let Some(new) = old.patched(patch) else {
            native::log::warn(&format!("settings patch rejected: {patch}"));
            return false;
        };
        let saved = match new.save(&self.data_dir.join("settings.json")) {
            Ok(()) => true,
            Err(e) => {
                native::log::warn(&format!("settings not saved: {e}"));
                false
            }
        };
        *self.settings.write().unwrap() = new.clone();

        if new.site_theme != old.site_theme {
            let _ = self.chrome.set_theme(theme_of(&new));
            for v in self.tabs.iter().filter_map(|t| t.view.as_ref()) {
                let _ = v.set_theme(theme_of(&new));
            }
        }
        if new.sleep_tabs != old.sleep_tabs && !new.sleep_tabs {
            for v in self.tabs.iter().filter_map(|t| t.view.as_ref()) {
                let _ = v.set_memory_usage_level(MemoryUsageLevel::Normal);
            }
        }
        {
            let mut s = self.shield.borrow_mut();
            s.enabled = new.shield;
            s.allow = new.shield_allow.clone();
        }
        // Live-update every open internal page (new tab, settings).
        let js = format!(
            "window.otiumApply && otiumApply({})",
            settings::script_json(&new.payload(self.force_dark_active))
        );
        for v in self.tabs.iter().filter(|t| util::is_internal(&t.url)).filter_map(|t| t.view.as_ref()) {
            let _ = v.evaluate_script(&js);
        }
        self.sync();
        saved
    }

    /// Restart the browser, optionally running a vault operation in between.
    fn restart_with(&mut self, vault_op: Option<(&str, &str, &str)>) {
        self.save_session();
        let exe = self.installed.clone().or_else(|| std::env::current_exe().ok());
        self.install_pending_update();
        if let Some(exe) = exe {
            let mut cmd = std::process::Command::new(exe);
            if let Some((op, path, _)) = vault_op {
                cmd.args(["--vault", op, path]).stdin(std::process::Stdio::piped());
            } else {
                // wait for this process to release the profile instead of handing over to it
                cmd.arg("--restart");
            }
            if let Ok(mut child) = cmd.spawn() {
                if let (Some((_, _, secret)), Some(mut stdin)) = (vault_op, child.stdin.take()) {
                    // the secret travels through a pipe, never through disk or argv
                    let _ = stdin.write_all(secret.as_bytes());
                }
            }
        }
        let _ = self.proxy.send_event(UserEvent::Exit);
    }

    /// Push the full UI state into the chrome webview.
    fn sync(&self) {
        let cfg = self.settings.read().unwrap();
        let sh = self.shield.borrow();
        let tabs: Vec<_> = self
            .tabs
            .iter()
            .map(|t| {
                json!({
                    "id": t.id,
                    "title": t.title,
                    "url": util::display_url(&t.url),
                    "favicon": t.favicon,
                    "loading": t.loading,
                    "private": t.private,
                    "pinned": t.pinned,
                    "audio": t.audio,
                    "muted": t.muted,
                })
            })
            .collect();
        let cur = self.current();
        let bookmarked = cur
            .filter(|t| !util::is_internal(&t.url))
            .and_then(|t| self.store.as_ref().map(|s| s.is_bookmarked(&t.url)))
            .unwrap_or(false);
        let host = cur.and_then(|t| util::host(&t.url)).unwrap_or_default();
        let active_dl: Vec<&Download> = self.downloads.iter().filter(|d| d.state == "progress").collect();
        let (rx, tot) = active_dl.iter().fold((0i64, 0i64), |a, d| (a.0 + d.received, a.1 + d.total.max(0)));
        let state = json!({
            "tabs": tabs,
            "active": cur.map(|t| t.id),
            "maximized": self.window.is_maximized(),
            "fullscreen": self.fullscreen,
            "bookmarked": bookmarked,
            "shield": {
                "on": sh.enabled && sh.engine.is_some() && !sh.allow.iter().any(|a| host == *a || host.ends_with(&format!(".{a}"))),
                "count": cur.and_then(|t| sh.counts.get(&t.id)).copied().unwrap_or(0),
            },
            "downloads": {
                "any": !self.downloads.is_empty(),
                "active": active_dl.len(),
                "progress": if tot > 0 { rx as f64 / tot as f64 } else { -1.0 },
            },
            "exts": self.exts.iter().filter(|e| e.enabled && !e.popup.is_empty()).count(),
            "ui": {
                "layout": cfg.layout,
                "floating": cfg.floating,
                "fullUrl": cfg.full_url,
                "accent": cfg.accent_colors(),
            },
        });
        let _ = self
            .chrome
            .evaluate_script(&format!("window.otium && otium.render({})", settings::script_json(&state)));
    }

    fn eval_current(&self, js: &str) {
        if let Some(v) = self.current().and_then(|t| t.view.as_ref()) {
            let _ = v.evaluate_script(js);
        }
    }

    fn set_zoom(&mut self, z: f64) {
        if let Some(t) = self.tabs.get_mut(self.active) {
            t.zoom = (z * 10.0).round() / 10.0;
            t.zoom = t.zoom.clamp(0.3, 3.0);
            if let Some(v) = &t.view {
                let _ = v.zoom(t.zoom);
            }
        }
        self.refresh_popover("menu");
    }

    fn shortcut(&mut self, k: &str) {
        match k {
            "new_tab" => self.open_tab(NEWTAB_URL, true, false),
            "new_private" => self.open_tab(NEWTAB_URL, true, true),
            "close_tab" => {
                if let Some(id) = self.current().map(|t| t.id) {
                    self.close_tab(id)
                }
            }
            "reopen_tab" => {
                if let Some(url) = self.closed.pop() {
                    self.open_tab(&url, true, false);
                }
            }
            "next_tab" | "prev_tab" => {
                let n = self.tabs.len();
                if n > 1 {
                    let i = if k == "next_tab" { (self.active + 1) % n } else { (self.active + n - 1) % n };
                    self.activate(i);
                }
            }
            "focus_address" => {
                let _ = self.chrome.focus();
                let _ = self.chrome.evaluate_script("otium.focusAddress()");
            }
            "fullscreen" => {
                self.html_fullscreen = false;
                self.set_fullscreen(!self.fullscreen)
            }
            "exit_fullscreen" if self.fullscreen => {
                self.html_fullscreen = false;
                self.set_fullscreen(false)
            }
            "devtools" => {
                if let Some(v) = self.current().and_then(|t| t.view.as_ref()) {
                    v.open_devtools()
                }
            }
            "reload" => {
                if let Some(v) = self.current().and_then(|t| t.view.as_ref()) {
                    let _ = v.reload();
                }
            }
            "toggle_sidebar" => {
                let _ = self.chrome.evaluate_script("otium.toggleSidebar()");
            }
            "settings" => self.open_internal(SETTINGS_URL),
            "history" => self.open_internal(HISTORY_URL),
            "bookmarks" => self.open_internal(BOOKMARKS_URL),
            "downloads" => {
                let _ = self.chrome.evaluate_script("otium.openPopover('downloads')");
            }
            "bookmark" => self.handle_cmd(Cmd::Bookmark),
            "zoom_in" => self.set_zoom(self.current().map(|t| t.zoom).unwrap_or(1.0) + 0.1),
            "zoom_out" => self.set_zoom(self.current().map(|t| t.zoom).unwrap_or(1.0) - 0.1),
            "zoom_reset" => self.set_zoom(1.0),
            "dismiss" => self.hide_popover(),
            "__ext_refresh" => {
                let p = self.proxy.clone();
                native::with_extensions(&self.chrome, |_| {}, move |list| {
                    let _ = p.send_event(UserEvent::ExtList(list));
                });
            }
            other => {
                if let Some(n) = other.strip_prefix("tab_").and_then(|n| n.parse::<usize>().ok()) {
                    let idx = if n == 9 { self.tabs.len().saturating_sub(1) } else { n.saturating_sub(1) };
                    self.activate(idx);
                }
            }
        }
    }

    fn navigate_tab(&mut self, idx: usize, text: &str) {
        let tpl = self.settings.read().unwrap().search_template();
        let url = util::resolve_input(text, &tpl);
        match &self.tabs[idx].view {
            Some(v) => {
                let _ = v.load_url(&url);
            }
            None => {
                self.tabs[idx].url = url;
                self.ensure_loaded(idx);
            }
        }
        if let Some(v) = &self.tabs[idx].view {
            let _ = v.focus();
        }
    }

    fn tab_action(&mut self, id: u32, action: &str) {
        let Some(i) = self.index_of(id) else { return };
        self.hide_popover();
        match action {
            "pin" => {
                let mut t = self.tabs.remove(i);
                t.pinned = !t.pinned;
                let active_id = self.current().map(|x| x.id).filter(|_| i != self.active).unwrap_or(id);
                let to = if t.pinned { self.pinned_count() } else { self.pinned_count() };
                self.tabs.insert(to.min(self.tabs.len()), t);
                self.active = self.index_of(active_id).unwrap_or(0);
                self.sync();
                self.save_session();
            }
            "mute" => {
                let t = &mut self.tabs[i];
                t.muted = !t.muted;
                if let Some(v) = &t.view {
                    native::set_muted(v, t.muted);
                }
                self.sync();
            }
            "duplicate" => {
                let (url, private) = (self.tabs[i].url.clone(), self.tabs[i].private);
                self.active = i;
                self.open_tab(&url, true, private);
            }
            "reload" => match &self.tabs[i].view {
                Some(v) => {
                    let _ = v.reload();
                }
                None => {
                    self.ensure_loaded(i);
                    self.sync();
                }
            },
            "close" => self.close_tab(id),
            "close_others" => {
                let ids: Vec<u32> = self.tabs.iter().filter(|t| t.id != id && !t.pinned).map(|t| t.id).collect();
                for x in ids {
                    self.close_tab(x);
                }
            }
            "close_right" => {
                let ids: Vec<u32> = self.tabs.iter().skip(i + 1).filter(|t| !t.pinned).map(|t| t.id).collect();
                for x in ids {
                    self.close_tab(x);
                }
            }
            "bookmark" => {
                let (u, t) = (self.tabs[i].url.clone(), self.tabs[i].title.clone());
                if let Some(s) = &self.store {
                    s.toggle_bookmark(&u, &t);
                }
                self.sync();
            }
            _ => {}
        }
    }

    fn menu_action(&mut self, action: &str) {
        match action {
            "pip" => {
                self.hide_popover();
                self.eval_current(
                    "(()=>{const v=[...document.querySelectorAll('video')].sort((a,b)=>b.clientWidth*b.clientHeight-a.clientWidth*a.clientHeight)[0];\
                     if(!v)return alert('На странице нет видео');\
                     (document.pictureInPictureElement?document.exitPictureInPicture():v.requestPictureInPicture()).catch(e=>alert('Картинка в картинке недоступна: '+e.message));})()",
                );
            }
            "print" => {
                self.hide_popover();
                self.eval_current("window.print()");
            }
            "find" => {
                self.hide_popover();
                // WebView2 has its own find bar bound to Ctrl+F inside the page
                if let Some(v) = self.current().and_then(|t| t.view.as_ref()) {
                    let _ = v.focus();
                }
                self.eval_current("document.dispatchEvent(new KeyboardEvent('keydown',{key:'f',ctrlKey:true}))");
            }
            "zoom_in" | "zoom_out" | "zoom_reset" => self.shortcut(action),
            "extensions" => {
                self.hide_popover();
                self.open_internal(SETTINGS_URL);
                self.eval_current("location.hash='#extensions'");
            }
            other => {
                self.hide_popover();
                self.shortcut(other);
            }
        }
    }

    fn handle_cmd(&mut self, cmd: Cmd) {
        match cmd {
            Cmd::Ready => self.sync(),
            Cmd::Navigate { text } => {
                self.hide_popover();
                if self.current().is_some() {
                    self.navigate_tab(self.active, &text);
                }
            }
            Cmd::NewTab => self.open_tab(NEWTAB_URL, true, false),
            Cmd::NewPrivateTab => self.open_tab(NEWTAB_URL, true, true),
            Cmd::CloseTab { id } => self.close_tab(id),
            Cmd::SelectTab { id } => {
                if let Some(i) = self.index_of(id) {
                    self.activate(i)
                }
            }
            Cmd::MoveTab { id, to } => {
                if let Some(from) = self.index_of(id) {
                    let active_id = self.current().map(|t| t.id);
                    let tab = self.tabs.remove(from);
                    let pins = self.pinned_count();
                    // keep pinned tabs in front of the others
                    let to = if tab.pinned { to.min(pins) } else { to.max(pins) };
                    self.tabs.insert(to.min(self.tabs.len()), tab);
                    if let Some(a) = active_id.and_then(|a| self.index_of(a)) {
                        self.active = a;
                    }
                    self.sync();
                    self.save_session();
                }
            }
            Cmd::Back => {
                if let Some(v) = self.current().and_then(|t| t.view.as_ref()) {
                    let _ = v.go_back();
                }
            }
            Cmd::Forward => {
                if let Some(v) = self.current().and_then(|t| t.view.as_ref()) {
                    let _ = v.go_forward();
                }
            }
            Cmd::Reload => self.shortcut("reload"),
            Cmd::Minimize => self.window.set_minimized(true),
            Cmd::Maximize => {
                self.hide_popover();
                self.window.set_maximized(!self.window.is_maximized());
            }
            Cmd::Close => {
                self.save_session();
                let _ = self.proxy.send_event(UserEvent::Exit);
            }
            Cmd::Drag => {
                self.hide_popover();
                let _ = self.window.drag_window();
            }
            Cmd::Resize { dir } => {
                let d = match dir.as_str() {
                    "n" => ResizeDirection::North,
                    "s" => ResizeDirection::South,
                    "e" => ResizeDirection::East,
                    "w" => ResizeDirection::West,
                    "ne" => ResizeDirection::NorthEast,
                    "nw" => ResizeDirection::NorthWest,
                    "se" => ResizeDirection::SouthEast,
                    _ => ResizeDirection::SouthWest,
                };
                if !self.window.is_maximized() {
                    let _ = self.window.drag_resize_window(d);
                }
            }
            Cmd::Layout { x, y, m } => {
                self.inset = (x.clamp(0.0, 480.0), y.clamp(0.0, 200.0), m.clamp(0.0, 32.0));
                self.relayout();
            }
            Cmd::Key { k } => self.shortcut(&k),
            Cmd::Dismiss | Cmd::Hide => self.hide_popover(),
            Cmd::OpenSettings => self.open_internal(SETTINGS_URL),
            Cmd::Set { patch } => {
                self.apply_patch(&patch);
            }
            Cmd::Restart => self.restart_with(None),
            Cmd::ClearData => {
                if let Some(v) = self.current().and_then(|t| t.view.as_ref()) {
                    let _ = v.clear_all_browsing_data();
                }
                if let Some(s) = &self.store {
                    s.clear_history();
                }
            }
            Cmd::Popover { kind, x, y, w, arg } => {
                if self.popover.as_deref() == Some(kind.as_str()) && kind != "tabmenu" {
                    self.hide_popover();
                    return;
                }
                // the overlay closes itself on blur right before the toggle click arrives
                if self.last_hidden.as_ref().is_some_and(|(k, t)| *k == kind && t.elapsed() < Duration::from_millis(350)) {
                    return;
                }
                if kind == "extensions" {
                    let p = self.proxy.clone();
                    native::with_extensions(&self.chrome, |_| {}, move |list| {
                        let _ = p.send_event(UserEvent::ExtList(list));
                    });
                }
                let data = self.popover_data(&kind, &arg);
                self.show_popover(&kind, x, y, w, 120.0, data, true);
            }
            Cmd::OvSize { h } => {
                if self.popover.is_some() {
                    if let Ok(r) = self.overlay.bounds() {
                        let s = self.window.scale_factor();
                        let pos = r.position.to_logical::<f64>(s);
                        let size = r.size.to_logical::<f64>(s);
                        let win = self.window.inner_size().to_logical::<f64>(s);
                        let h = h.min(win.height - pos.y - 8.0);
                        let _ = self.overlay.set_bounds(rect(pos.x, pos.y, size.width, h));
                    }
                }
            }
            Cmd::Suggest { q, x, y, w } => self.suggest(&q, x, y, w),
            Cmd::SuggestSel { i } => {
                if self.popover.as_deref() == Some("suggest") {
                    let _ = self.overlay.evaluate_script(&format!("window.ov && ov.select({i})"));
                }
            }
            Cmd::TabAction { id, action } => self.tab_action(id, &action),
            Cmd::Menu { action } => self.menu_action(&action),
            Cmd::Bookmark => {
                if let (Some(t), Some(s)) = (self.current(), &self.store) {
                    if !util::is_internal(&t.url) {
                        s.toggle_bookmark(&t.url, &t.title);
                    }
                }
                self.sync();
                self.push_internal("bookmarks", &json!(self.store.as_ref().map(|s| s.bookmarks()).unwrap_or_default()));
            }
            Cmd::ShieldToggleSite => {
                let host = self.current().and_then(|t| util::host(&t.url)).unwrap_or_default();
                if host.is_empty() {
                    return;
                }
                let mut allow = self.cfg().shield_allow;
                if let Some(p) = allow.iter().position(|a| *a == host) {
                    allow.remove(p);
                } else {
                    allow.push(host);
                }
                self.apply_patch(&json!({ "shield_allow": allow }));
                self.refresh_popover("shield");
                self.shortcut("reload");
            }
            Cmd::ShieldUpdate => {
                self.rebuild_shield();
                self.refresh_popover("shield");
                self.push_internal("runtime", &self.runtime_json());
            }
            Cmd::DownloadOpen { id, folder } => {
                if let Some(d) = self.downloads.iter().find(|d| d.id == id) {
                    let mut c = std::process::Command::new("explorer.exe");
                    if folder {
                        c.arg(format!("/select,{}", d.path));
                    } else {
                        c.arg(&d.path);
                    }
                    let _ = c.spawn();
                }
                self.hide_popover();
            }
            Cmd::DownloadCancel { id } => {
                if let Some((_, op)) = self.dl_ops.borrow().iter().find(|(i, _)| *i == id) {
                    native::cancel_download(op);
                }
            }
            Cmd::DownloadsClear => {
                self.downloads.retain(|d| d.state == "progress");
                self.refresh_popover("downloads");
                self.sync();
            }
            Cmd::ExtOpen { id, page } => {
                if let Some(e) = self.exts.iter().find(|e| e.id == id) {
                    let options = page == "options";
                    let page = if options { e.options.clone() } else { e.popup.clone() };
                    if page.is_empty() {
                        return;
                    }
                    self.hide_popover();
                    if options {
                        self.open_tab(&format!("chrome-extension://{id}/{page}"), true, false);
                    } else {
                        self.open_ext_popup(&id, &page);
                    }
                }
            }
            // The remaining commands come from internal pages and are routed via handle_internal.
            other => self.handle_internal(None, other),
        }
    }

    /// Privileged commands from internal otium:// pages (and the chrome).
    fn handle_internal(&mut self, from: Option<usize>, cmd: Cmd) {
        let reply_to = from.unwrap_or(self.active);
        match cmd {
            Cmd::Data { what, q, offset } => {
                let data = match what.as_str() {
                    "history" => json!(self.store.as_ref().map(|s| s.history(&q, offset)).unwrap_or_default()),
                    "bookmarks" => json!(self.store.as_ref().map(|s| s.bookmarks()).unwrap_or_default()),
                    "runtime" => self.runtime_json(),
                    _ => Value::Null,
                };
                if from.is_some() {
                    self.reply(reply_to, &what, &data);
                }
            }
            Cmd::DeleteHistory { url } => {
                if let Some(s) = &self.store {
                    s.delete_history(&url);
                }
            }
            Cmd::ClearHistory => {
                if let Some(s) = &self.store {
                    s.clear_history();
                }
            }
            Cmd::DeleteBookmark { url } => {
                if let Some(s) = &self.store {
                    s.delete_bookmark(&url);
                }
                self.sync();
            }
            Cmd::ExtInstall { url } => {
                let Some(store_id) = extensions::parse_store_id(&url) else {
                    return self.toast(reply_to, "Вставьте ссылку на расширение из Chrome Web Store", false);
                };
                if self.ext_busy {
                    return;
                }
                self.ext_busy = true;
                self.push_internal("runtime", &self.runtime_json());
                let root = self.ext_root();
                let p = self.proxy.clone();
                std::thread::spawn(move || {
                    let _ = fs::create_dir_all(&root);
                    let r = extensions::download(&store_id, &root);
                    let _ = p.send_event(UserEvent::ExtDownloaded(r));
                });
            }
            Cmd::ExtFolder => {
                let picked = rfd::FileDialog::new()
                    .set_title("Папка распакованного расширения")
                    .set_parent(&self.window)
                    .pick_folder();
                if let Some(src) = picked {
                    let root = self.ext_root();
                    let _ = fs::create_dir_all(&root);
                    match extensions::import_folder(&src, &root) {
                        Ok(dir) => {
                            self.ext_busy = true;
                            self.register_extension(dir);
                        }
                        Err(e) => self.toast(reply_to, &e, false),
                    }
                }
            }
            Cmd::ExtToggle { id, on } => {
                self.with_ext(&id, move |e| native::ext_enable(e, on));
                if let Some(e) = self.exts.iter_mut().find(|e| e.id == id) {
                    e.enabled = on;
                }
                extensions::save_index(&self.ext_root(), &self.exts);
                self.sync();
            }
            Cmd::ExtRemove { id } => {
                self.with_ext(&id, native::ext_remove);
                if let Some(pos) = self.exts.iter().position(|e| e.id == id) {
                    let e = self.exts.remove(pos);
                    let _ = fs::remove_dir_all(&e.dir);
                }
                extensions::save_index(&self.ext_root(), &self.exts);
                self.push_internal("runtime", &self.runtime_json());
                self.sync();
            }
            Cmd::AccountCreate { name, kind, secret } => {
                if vault::load_account(&self.data_dir).is_some() {
                    return self.toast(reply_to, "Аккаунт уже создан", false);
                }
                if name.trim().is_empty() {
                    return self.toast(reply_to, "Введите имя", false);
                }
                match vault::create_account(&self.data_dir, &name, &kind, &secret) {
                    Ok(_) => self.toast(reply_to, "Аккаунт создан", true),
                    Err(e) => self.toast(reply_to, &e, false),
                }
                self.push_internal("runtime", &self.runtime_json());
            }
            Cmd::AccountDelete { secret } => match vault::load_account(&self.data_dir) {
                Some(a) if vault::verify(&a, &secret) => {
                    vault::delete_account(&self.data_dir);
                    self.toast(reply_to, "Аккаунт удалён", true);
                    self.push_internal("runtime", &self.runtime_json());
                }
                _ => self.toast(reply_to, "Неверный пароль, PIN-код или секретное слово", false),
            },
            Cmd::VaultExport { secret } => {
                let Some(acc) = vault::load_account(&self.data_dir) else {
                    return self.toast(reply_to, "Сначала создайте аккаунт", false);
                };
                if !vault::verify(&acc, &secret) {
                    return self.toast(reply_to, "Неверный пароль, PIN-код или секретное слово", false);
                }
                let date = util::today();
                let picked = rfd::FileDialog::new()
                    .set_title("Сохранить резервную копию OTIUM")
                    .set_file_name(format!("OTIUM-{}-{date}.otium", acc.name.replace(['\\', '/', ':', '*', '?', '"', '<', '>', '|'], "_")))
                    .add_filter("Резервная копия OTIUM", &["otium"])
                    .set_parent(&self.window)
                    .save_file();
                if let Some(path) = picked {
                    self.restart_with(Some(("export", &path.to_string_lossy(), &secret)));
                }
            }
            Cmd::VaultPick => {
                let picked = rfd::FileDialog::new()
                    .set_title("Файл резервной копии OTIUM")
                    .add_filter("Резервная копия OTIUM", &["otium"])
                    .set_parent(&self.window)
                    .pick_file();
                if let Some(p) = picked {
                    self.reply(reply_to, "vault_file", &json!({ "path": p.to_string_lossy() }));
                }
            }
            Cmd::VaultImport { path, secret } => {
                let p = PathBuf::from(&path);
                match vault::check_vault(&p, &secret) {
                    Ok(()) => self.restart_with(Some(("import", &path, &secret))),
                    Err(e) => self.toast(reply_to, &e, false),
                }
            }
            Cmd::MakeDefault => {
                if let Some(exe) = &self.installed {
                    integrate::ensure_registered(exe, env!("CARGO_PKG_VERSION"));
                    integrate::open_default_settings();
                } else {
                    self.toast(reply_to, "Доступно только в установленной версии OTIUM", false);
                }
            }
            Cmd::UpdateCheck => {
                if self.installed.is_none() {
                    return self.toast(reply_to, "Обновления работают только в установленной версии OTIUM", false);
                }
                self.update.up_to_date = false;
                self.start_update_check();
            }
            Cmd::UpdateNow => {
                if self.update.ready.is_some() {
                    self.restart_with(None);
                }
            }
            _ => {}
        }
    }

    fn suggest(&mut self, q: &str, x: f64, y: f64, w: f64) {
        self.suggest_seq += 1;
        let q = q.trim().to_string();
        if q.is_empty() {
            self.suggest_items.clear();
            if self.popover.as_deref() == Some("suggest") {
                self.hide_popover();
            }
            return;
        }
        let ql = q.to_lowercase();
        let mut items: Vec<Value> = Vec::new();
        // open tabs
        for t in &self.tabs {
            if items.len() >= 2 {
                break;
            }
            if self.current().map(|c| c.id) != Some(t.id)
                && !util::is_internal(&t.url)
                && (t.title.to_lowercase().contains(&ql) || t.url.to_lowercase().contains(&ql))
            {
                items.push(json!({"kind": "tab", "id": t.id, "url": t.url, "title": t.title}));
            }
        }
        if let Some(s) = &self.store {
            for sug in s.suggest(&q, 6) {
                if !items.iter().any(|i| i["url"] == sug.url.as_str()) {
                    items.push(json!({"kind": sug.kind, "url": sug.url, "title": sug.title}));
                }
            }
        }
        items.truncate(6);
        self.suggest_items = items.clone();
        let cfg = self.cfg();
        let data = json!({"q": q, "items": items, "remote": [], "engine": cfg.search_name()});
        let _ = self.chrome.evaluate_script(&format!("otium.suggestions({})", settings::script_json(&data)));
        self.show_popover("suggest", x, y, w, 60.0, data, false);
        if cfg.suggest_remote && cfg.search != "custom" {
            let (seq, p, engine) = (self.suggest_seq, self.proxy.clone(), cfg.search.clone());
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(120)); // debounce fast typing
                let r = store::remote_suggestions(&engine, &q);
                let _ = p.send_event(UserEvent::Suggest(seq, r));
            });
        }
    }

    /// Messages from tab content. Web pages can post here too, so ordinary
    /// pages only get a harmless subset; privileged commands require the
    /// message to come from an internal otium:// page.
    fn handle_content(&mut self, id: u32, uri: &str, msg: &str) {
        let Some(i) = self.index_of(id) else {
            if util::is_internal(uri) {
                native::log::warn(&format!("message from unknown tab {id} ({uri}) dropped"));
            }
            return;
        };
        let Ok(cmd) = serde_json::from_str::<Cmd>(msg) else {
            if util::is_internal(uri) {
                native::log::warn(&format!("unparsable message from {uri}: {}", msg.chars().take(200).collect::<String>()));
            }
            return;
        };
        let internal = util::is_internal(uri) && util::is_internal(&self.tabs[i].url);
        match cmd {
            Cmd::Key { k } if i == self.active => self.shortcut(&k),
            Cmd::Dismiss => self.hide_popover(),
            Cmd::Favicon { href } => {
                if href.starts_with("https://") || href.starts_with("http://") || href.starts_with("data:image/") {
                    self.tabs[i].favicon = href;
                    self.sync();
                }
            }
            Cmd::Cosmetic { classes, ids } => {
                let s = self.shield.borrow();
                let host = util::host(&self.tabs[i].url).unwrap_or_default();
                if !s.enabled || s.allow.iter().any(|a| host == *a || host.ends_with(&format!(".{a}"))) {
                    return;
                }
                if let Some(engine) = &s.engine {
                    let css = engine.cosmetic_css(&self.tabs[i].url, &classes[..classes.len().min(3000)], &ids[..ids.len().min(3000)]);
                    if !css.is_empty() {
                        let js = format!("window.__otiumCss && __otiumCss({})", json!(css));
                        if let Some(v) = &self.tabs[i].view {
                            let _ = v.evaluate_script(&js);
                        }
                    }
                }
            }
            Cmd::Navigate { text } if internal => self.navigate_tab(i, &text),
            Cmd::Set { patch } if internal => {
                let ok = self.apply_patch(&patch);
                // the settings page waits for this and warns if it never comes
                if let Some(i) = self.index_of(id) {
                    self.reply(i, "saved", &json!(ok));
                }
            }
            Cmd::OpenSettings if internal => self.open_internal(SETTINGS_URL),
            Cmd::Restart if internal => self.restart_with(None),
            Cmd::ShieldUpdate if internal => self.handle_cmd(Cmd::ShieldUpdate),
            Cmd::ExtOpen { id, page } if internal => self.handle_cmd(Cmd::ExtOpen { id, page }),
            Cmd::ClearData if internal => {
                if let Some(v) = &self.tabs[i].view {
                    let _ = v.clear_all_browsing_data();
                }
                if let Some(s) = &self.store {
                    s.clear_history();
                }
            }
            other if internal => self.handle_internal(Some(i), other),
            _ => {
                // an internal page whose command was refused: worth knowing why
                if util::is_internal(uri) {
                    let name = serde_json::from_str::<Value>(msg).ok().and_then(|v| v["cmd"].as_str().map(String::from));
                    native::log::warn(&format!(
                        "command {:?} from {uri} refused: tab {id} is at {:?}",
                        name.unwrap_or_default(),
                        self.tabs[i].url
                    ));
                }
            }
        }
    }

    fn handle(&mut self, ev: UserEvent, flow: &mut ControlFlow) {
        match ev {
            UserEvent::Exit => {
                self.install_pending_update();
                *flow = ControlFlow::Exit
            }
            UserEvent::Tick => {
                self.discard_idle();
                let due = match self.update.checked {
                    Some(t) => t.elapsed() >= UPDATE_EVERY,
                    None => self.started.elapsed() >= UPDATE_FIRST,
                };
                if due && self.cfg().auto_update {
                    self.start_update_check();
                }
            }
            UserEvent::Remote(msg) => {
                self.window.set_minimized(false);
                self.window.set_focus();
                let url = msg.trim();
                // only web pages and local files; never internal pages or other schemes
                if ["http://", "https://", "file:///"].iter().any(|s| url.starts_with(s)) && !util::is_internal(url) {
                    self.open_tab(url, true, false);
                    self.save_session();
                }
            }
            UserEvent::Update(res) => {
                self.update.busy = false;
                match res {
                    Ok(Some(r)) => {
                        self.update.ready = Some(r);
                        self.update.up_to_date = false;
                    }
                    Ok(None) => self.update.up_to_date = true,
                    Err(e) => self.update.error = e,
                }
                self.push_internal("runtime", &self.runtime_json());
            }
            UserEvent::Chrome(msg) => {
                if let Ok(cmd) = serde_json::from_str::<Cmd>(&msg) {
                    self.handle_cmd(cmd);
                }
            }
            UserEvent::Content(id, uri, msg) => self.handle_content(id, &uri, &msg),
            UserEvent::Title(id, title) => {
                if let Some(i) = self.index_of(id) {
                    let t = &self.tabs[i];
                    if !t.private && !util::is_internal(&t.url) {
                        if let Some(s) = &self.store {
                            s.set_title(&t.url, &title);
                        }
                    }
                    self.tabs[i].title = title;
                    self.sync();
                }
            }
            UserEvent::Load(id, started, url) => {
                if let Some(i) = self.index_of(id) {
                    let t = &mut self.tabs[i];
                    if started && util::host(&t.url) != util::host(&url) {
                        t.favicon.clear();
                    }
                    if started {
                        self.shield.borrow_mut().counts.insert(id, 0);
                    }
                    t.loading = started;
                    t.url = url.clone();
                    if !started && !t.private && (url.starts_with("http://") || url.starts_with("https://")) && !util::is_internal(&url) {
                        if let Some(s) = &self.store {
                            s.visit(&url, &t.title);
                        }
                    }
                    self.sync();
                    if !started {
                        self.save_session();
                    }
                }
            }
            UserEvent::OpenTab(url, private) => self.open_tab(&url, true, private),
            UserEvent::Fullscreen(id, on) => {
                if self.current().map(|t| t.id) == Some(id) && on != self.fullscreen {
                    self.html_fullscreen = on;
                    self.set_fullscreen(on);
                } else if !on && self.html_fullscreen {
                    self.html_fullscreen = false;
                    self.set_fullscreen(false);
                }
            }
            UserEvent::Audio(id, on) => {
                if let Some(i) = self.index_of(id) {
                    self.tabs[i].audio = on;
                    self.sync();
                }
            }
            UserEvent::Download(ev) => {
                match ev {
                    native::DownloadEvent::Started { id, url, path, total } => {
                        self.downloads.insert(
                            0,
                            Download { id, url, name: file_name(&path), path, received: 0, total, state: "progress" },
                        );
                        self.downloads.truncate(50);
                        let _ = self.chrome.evaluate_script("otium.downloadStarted && otium.downloadStarted()");
                    }
                    native::DownloadEvent::Progress { id, received, total } => {
                        if let Some(d) = self.downloads.iter_mut().find(|d| d.id == id) {
                            d.received = received;
                            d.total = total;
                        }
                    }
                    native::DownloadEvent::Done { id, ok, path } => {
                        if let Some(d) = self.downloads.iter_mut().find(|d| d.id == id) {
                            d.state = if ok { "done" } else { "failed" };
                            if ok {
                                d.received = d.total.max(d.received);
                            }
                            if !path.is_empty() {
                                d.name = file_name(&path);
                                d.path = path;
                            }
                        }
                        self.dl_ops.borrow_mut().retain(|(i, _)| *i != id);
                    }
                }
                self.refresh_popover("downloads");
                self.sync();
            }
            UserEvent::ShieldBuilt => {
                self.shield_updating = false;
                if let Some((s, _)) = shield::Shield::load_cached(&self.data_dir.join("shield")) {
                    self.shield.borrow_mut().engine = Some(s);
                }
                self.refresh_popover("shield");
                self.push_internal("runtime", &self.runtime_json());
                self.sync();
            }
            UserEvent::ShieldTick => {
                self.shield.borrow_mut().tick_pending = false;
                self.sync();
                self.refresh_popover("shield");
            }
            UserEvent::Suggest(seq, remote) => {
                if seq == self.suggest_seq && self.popover.as_deref() == Some("suggest") {
                    let js = format!("window.ov && ov.remote({})", json!(remote));
                    let _ = self.overlay.evaluate_script(&js);
                    let _ = self.chrome.evaluate_script(&format!("otium.remoteSuggestions({})", json!(remote)));
                }
            }
            UserEvent::ExtDownloaded(res) => match res {
                Ok(dir) => self.register_extension(dir),
                Err(e) => {
                    self.ext_busy = false;
                    self.push_internal("toast", &json!({"msg": e, "ok": false}));
                    self.push_internal("runtime", &self.runtime_json());
                }
            },
            UserEvent::ExtAdded(dir, res) => {
                self.ext_busy = false;
                match res {
                    Ok(info) => {
                        let mut meta = extensions::read_manifest(&dir);
                        meta.id = info.id;
                        meta.enabled = info.enabled;
                        if meta.name.is_empty() {
                            meta.name = info.name;
                        }
                        let msg = format!("Расширение «{}» установлено", meta.name);
                        self.exts.retain(|e| e.dir != dir && e.id != meta.id);
                        self.exts.push(meta);
                        extensions::save_index(&self.ext_root(), &self.exts);
                        self.push_internal("toast", &json!({"msg": msg, "ok": true}));
                    }
                    Err(e) => {
                        let _ = fs::remove_dir_all(&dir);
                        self.exts.retain(|x| x.dir != dir);
                        self.push_internal("toast", &json!({"msg": e, "ok": false}));
                    }
                }
                self.push_internal("runtime", &self.runtime_json());
                self.refresh_popover("extensions");
                self.sync();
            }
            UserEvent::ExtList(list) => {
                for e in &mut self.exts {
                    if let Some(i) = list.iter().find(|i| i.id == e.id) {
                        e.enabled = i.enabled;
                    }
                }
                self.push_internal("runtime", &self.runtime_json());
                self.refresh_popover("extensions");
                self.sync();
            }
        }
    }
}

fn full_rect(window: &Window) -> Rect {
    let size = window.inner_size();
    Rect {
        position: PhysicalPosition::new(0, 0).into(),
        size: PhysicalSize::new(size.width, size.height).into(),
    }
}

/// The link this launch should open: `--single-argument <url>` (how Windows passes links and
/// files to a registered browser) or a plain first argument. Files become file:/// URLs.
fn launch_target(args: &[String]) -> Option<String> {
    let raw = match args.iter().position(|a| a == "--single-argument") {
        Some(i) => args[i + 1..].join(" "),
        None => args.iter().skip(1).find(|a| !a.starts_with("--"))?.clone(),
    };
    let raw = raw.trim().trim_matches('"');
    if raw.is_empty() {
        return None;
    }
    let lower = raw.to_ascii_lowercase();
    if lower.starts_with("http://") || lower.starts_with("https://") || lower.starts_with("file:///") {
        return Some(raw.to_string());
    }
    let p = Path::new(raw);
    if p.is_absolute() && p.exists() {
        return url::Url::from_file_path(p).ok().map(|u| u.to_string());
    }
    None
}

/// `otium --vault export|import <path>` with the secret on stdin: runs before any webview exists.
fn run_vault_op(args: &[String]) {
    let (Some(op), Some(path)) = (args.get(2), args.get(3)) else { return };
    let mut secret = String::new();
    let _ = std::io::stdin().read_line(&mut secret);
    let data = util::data_dir();
    vault::wait_profile_free(&data);
    let path = PathBuf::from(path);
    let (ok, msg) = match op.as_str() {
        "export" => match vault::export(&data, &path, &secret) {
            Ok(n) => (true, format!("Резервная копия сохранена:\n{}\n\nФайлов в архиве: {n}.\nХраните файл и пароль в надёжном месте.", path.display())),
            Err(e) => (false, format!("Не удалось сохранить резервную копию:\n{e}")),
        },
        "import" => match vault::import(&data, &path, &secret) {
            Ok(name) => (true, format!("Профиль{} восстановлен: входы, пароли, история, закладки, расширения и настройки.", if name.is_empty() { String::new() } else { format!(" «{name}»") })),
            Err(e) => (false, format!("Не удалось восстановить профиль:\n{e}")),
        },
        _ => return,
    };
    secret.clear();
    util::message_box("OTIUM", &msg, !ok);
}

fn main() -> wry::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    if args.get(1).map(String::as_str) == Some("--vault") {
        run_vault_op(&args);
    }
    if args.get(1).map(String::as_str) == Some("--uninstall") {
        util::uninstall();
        return Ok(());
    }

    // one browser per user: hand the link to the running one, or wait for it to exit on restart
    let restarting = args.iter().skip(1).any(|a| a == "--restart" || a == "--vault");
    let target = if args.get(1).map(String::as_str) == Some("--vault") { None } else { launch_target(&args) };
    if !instance::claim(if restarting { 20_000 } else { 0 }) {
        if instance::forward(target.as_deref().unwrap_or("")) {
            return Ok(());
        }
        // the running one is shutting down: start once it's gone
        if !instance::claim(8_000) {
            return Ok(());
        }
    }

    let event_loop = EventLoopBuilder::<UserEvent>::with_user_event().build();
    let proxy = event_loop.create_proxy();

    let session: Session = fs::read_to_string(util::data_dir().join("session.json"))
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default();

    let mut builder = WindowBuilder::new()
        .with_title("OTIUM")
        .with_inner_size(LogicalSize::new(1360.0, 860.0))
        .with_min_inner_size(LogicalSize::new(760.0, 480.0))
        .with_decorations(false)
        .with_visible(false)
        .with_background_color(BG)
        .with_maximized(session.maximized)
        .with_window_icon(util::app_icon(ICON_PNG));
    if let Some((x, y, w, h)) = session.window {
        if w >= 760.0 && h >= 480.0 && x > -10000.0 && y > -10000.0 {
            builder = builder.with_position(LogicalPosition::new(x, y)).with_inner_size(LogicalSize::new(w, h));
        }
    }
    #[cfg(windows)]
    let builder = builder.with_undecorated_shadow(true);
    let window = builder.build(&event_loop).expect("failed to create window");

    let tick = proxy.clone();
    let remote = proxy.clone();
    let mut app = App::new(window, proxy, session, target)?;
    instance::serve(move |msg| {
        let _ = remote.send_event(UserEvent::Remote(msg));
    });
    std::thread::spawn(move || loop {
        std::thread::sleep(TICK);
        if tick.send_event(UserEvent::Tick).is_err() {
            break;
        }
    });
    app.window.set_visible(true);

    event_loop.run(move |event, _, flow| {
        *flow = ControlFlow::Wait;
        match event {
            Event::WindowEvent { event, .. } => match event {
                WindowEvent::CloseRequested => {
                    app.save_session();
                    app.install_pending_update();
                    *flow = ControlFlow::Exit;
                }
                WindowEvent::Resized(_) | WindowEvent::ScaleFactorChanged { .. } => app.relayout(),
                WindowEvent::Moved(_) => app.hide_popover(),
                WindowEvent::Focused(false) => {
                    if app.popover.as_deref() == Some("suggest") {
                        app.hide_popover();
                    }
                }
                _ => {}
            },
            Event::UserEvent(ev) => app.handle(ev, flow),
            _ => {}
        }
    });
}
