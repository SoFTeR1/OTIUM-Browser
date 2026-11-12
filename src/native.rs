//! Thin wrappers over WebView2 COM APIs that wry doesn't expose:
//! request filtering (ad blocking), downloads, fullscreen, audio,
//! password autosave, browser extensions, z-order and DPAPI.

use std::path::Path;

use webview2_com::{
    take_pwstr, BrowserExtensionEnableCompletedHandler, BrowserExtensionRemoveCompletedHandler,
    BytesReceivedChangedEventHandler, ContainsFullScreenElementChangedEventHandler,
    DownloadStartingEventHandler, IsDocumentPlayingAudioChangedEventHandler,
    Microsoft::Web::WebView2::Win32::*, ProfileAddBrowserExtensionCompletedHandler,
    ProfileGetBrowserExtensionsCompletedHandler, StateChangedEventHandler,
    WebResourceRequestedEventHandler,
};
use windows::{
    core::{Interface, BOOL, HSTRING, PWSTR},
    Win32::{
        Foundation::{HWND, HLOCAL, LocalFree},
        Security::Cryptography::{CryptProtectData, CryptUnprotectData, CRYPT_INTEGER_BLOB},
        UI::WindowsAndMessaging::{SetWindowPos, HWND_TOP, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE},
    },
};
use wry::{WebView, WebViewExtWindows};

fn pwstr(f: impl FnOnce(*mut PWSTR) -> windows::core::Result<()>) -> String {
    let mut p = PWSTR::null();
    if f(&mut p).is_ok() {
        take_pwstr(p)
    } else {
        String::new()
    }
}

fn boolean(f: impl FnOnce(*mut BOOL) -> windows::core::Result<()>) -> bool {
    let mut b = BOOL::default();
    f(&mut b).is_ok() && b.as_bool()
}

/// Turn on the built-in password manager and form autofill.
pub fn enable_autofill(view: &WebView) {
    unsafe {
        if let Ok(s) = view.webview().Settings().and_then(|s| s.cast::<ICoreWebView2Settings4>()) {
            let _ = s.SetIsPasswordAutosaveEnabled(true);
            let _ = s.SetIsGeneralAutofillEnabled(true);
        }
    }
}

/// Raise a webview's host window above its siblings (used for popovers).
pub fn bring_to_top(view: &WebView) {
    unsafe {
        let mut hwnd = HWND::default();
        if view.controller().ParentWindow(&mut hwnd).is_ok() && !hwnd.is_invalid() {
            let _ = SetWindowPos(hwnd, Some(HWND_TOP), 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE);
        }
    }
}

/// Request type as understood by adblock-rust.
fn context_name(c: COREWEBVIEW2_WEB_RESOURCE_CONTEXT) -> &'static str {
    match c {
        COREWEBVIEW2_WEB_RESOURCE_CONTEXT_DOCUMENT => "document",
        COREWEBVIEW2_WEB_RESOURCE_CONTEXT_STYLESHEET => "stylesheet",
        COREWEBVIEW2_WEB_RESOURCE_CONTEXT_IMAGE => "image",
        COREWEBVIEW2_WEB_RESOURCE_CONTEXT_MEDIA => "media",
        COREWEBVIEW2_WEB_RESOURCE_CONTEXT_FONT => "font",
        COREWEBVIEW2_WEB_RESOURCE_CONTEXT_SCRIPT => "script",
        COREWEBVIEW2_WEB_RESOURCE_CONTEXT_XML_HTTP_REQUEST => "xhr",
        COREWEBVIEW2_WEB_RESOURCE_CONTEXT_FETCH => "fetch",
        COREWEBVIEW2_WEB_RESOURCE_CONTEXT_WEBSOCKET => "websocket",
        COREWEBVIEW2_WEB_RESOURCE_CONTEXT_PING => "ping",
        COREWEBVIEW2_WEB_RESOURCE_CONTEXT_MANIFEST => "other",
        _ => "other",
    }
}

/// Intercept every network request. `decide(url, page_url, type)` returns true to block.
pub fn install_request_filter(view: &WebView, decide: impl Fn(&str, &str, &str) -> bool + 'static) {
    unsafe {
        let wv = view.webview();
        let env = view.environment();
        let _ = wv.AddWebResourceRequestedFilter(&HSTRING::from("*"), COREWEBVIEW2_WEB_RESOURCE_CONTEXT_ALL);
        let handler = WebResourceRequestedEventHandler::create(Box::new(move |sender, args| {
            let (Some(sender), Some(args)) = (sender, args) else { return Ok(()) };
            let req = args.Request()?;
            let url = pwstr(|p| req.Uri(p));
            if url.starts_with(crate::INTERNAL) || url.starts_with("data:") || url.starts_with("blob:") {
                return Ok(());
            }
            let mut ctx = COREWEBVIEW2_WEB_RESOURCE_CONTEXT_ALL;
            let _ = args.ResourceContext(&mut ctx);
            if ctx == COREWEBVIEW2_WEB_RESOURCE_CONTEXT_DOCUMENT {
                return Ok(()); // never block top-level navigations
            }
            let page = pwstr(|p| sender.Source(p));
            if decide(&url, &page, context_name(ctx)) {
                let resp = env.CreateWebResourceResponse(None, 403, &HSTRING::from("Blocked"), &HSTRING::new())?;
                args.SetResponse(&resp)?;
            }
            Ok(())
        }));
        let mut token = 0i64;
        let _ = wv.add_WebResourceRequested(&handler, &mut token);
    }
}

pub fn on_fullscreen(view: &WebView, cb: impl Fn(bool) + 'static) {
    unsafe {
        let handler = ContainsFullScreenElementChangedEventHandler::create(Box::new(move |sender, _| {
            if let Some(w) = sender {
                cb(boolean(|b| w.ContainsFullScreenElement(b)));
            }
            Ok(())
        }));
        let mut token = 0i64;
        let _ = view.webview().add_ContainsFullScreenElementChanged(&handler, &mut token);
    }
}

pub fn on_audio(view: &WebView, cb: impl Fn(bool) + 'static) {
    unsafe {
        let Ok(w8) = view.webview().cast::<ICoreWebView2_8>() else { return };
        let handler = IsDocumentPlayingAudioChangedEventHandler::create(Box::new(move |sender, _| {
            if let Some(w) = sender.and_then(|s| s.cast::<ICoreWebView2_8>().ok()) {
                cb(boolean(|b| w.IsDocumentPlayingAudio(b)));
            }
            Ok(())
        }));
        let mut token = 0i64;
        let _ = w8.add_IsDocumentPlayingAudioChanged(&handler, &mut token);
    }
}

pub fn set_muted(view: &WebView, muted: bool) {
    unsafe {
        if let Ok(w8) = view.webview().cast::<ICoreWebView2_8>() {
            let _ = w8.SetIsMuted(muted);
        }
    }
}

#[derive(Clone, Debug)]
pub enum DownloadEvent {
    Started { id: u32, url: String, path: String, total: i64 },
    Progress { id: u32, received: i64, total: i64 },
    Done { id: u32, ok: bool, path: String },
}

/// Take over downloads (hides Edge's own flyout) and report progress.
/// `cancel_slot` receives the operation so the UI can cancel it later.
pub fn on_downloads(
    view: &WebView,
    next_id: std::rc::Rc<std::cell::Cell<u32>>,
    ops: std::rc::Rc<std::cell::RefCell<Vec<(u32, ICoreWebView2DownloadOperation)>>>,
    cb: impl Fn(DownloadEvent) + Clone + 'static,
) {
    unsafe {
        let Ok(w4) = view.webview().cast::<ICoreWebView2_4>() else { return };
        let handler = DownloadStartingEventHandler::create(Box::new(move |_, args| {
            let Some(args) = args else { return Ok(()) };
            let op = args.DownloadOperation()?;
            args.SetHandled(true)?;
            let id = next_id.get();
            next_id.set(id + 1);
            let url = pwstr(|p| op.Uri(p));
            let path = pwstr(|p| args.ResultFilePath(p));
            let mut total = 0i64;
            let _ = op.TotalBytesToReceive(&mut total);
            cb(DownloadEvent::Started { id, url, path, total });
            ops.borrow_mut().push((id, op.clone()));

            let cb2 = cb.clone();
            let progress = BytesReceivedChangedEventHandler::create(Box::new(move |op, _| {
                if let Some(op) = op {
                    let (mut r, mut t) = (0i64, 0i64);
                    let _ = op.BytesReceived(&mut r);
                    let _ = op.TotalBytesToReceive(&mut t);
                    cb2(DownloadEvent::Progress { id, received: r, total: t });
                }
                Ok(())
            }));
            let cb3 = cb.clone();
            let state = StateChangedEventHandler::create(Box::new(move |op, _| {
                if let Some(op) = op {
                    let mut s = COREWEBVIEW2_DOWNLOAD_STATE_IN_PROGRESS;
                    let _ = op.State(&mut s);
                    if s != COREWEBVIEW2_DOWNLOAD_STATE_IN_PROGRESS {
                        let path = pwstr(|p| op.ResultFilePath(p));
                        cb3(DownloadEvent::Done { id, ok: s == COREWEBVIEW2_DOWNLOAD_STATE_COMPLETED, path });
                    }
                }
                Ok(())
            }));
            let mut t = 0i64;
            let _ = op.add_BytesReceivedChanged(&progress, &mut t);
            let _ = op.add_StateChanged(&state, &mut t);
            Ok(())
        }));
        let mut token = 0i64;
        let _ = w4.add_DownloadStarting(&handler, &mut token);
    }
}

pub fn cancel_download(op: &ICoreWebView2DownloadOperation) {
    unsafe {
        let _ = op.Cancel();
    }
}

// ---------------------------------------------------------------- extensions

fn profile7(view: &WebView) -> Option<ICoreWebView2Profile7> {
    unsafe {
        view.webview()
            .cast::<ICoreWebView2_13>()
            .ok()?
            .Profile()
            .ok()?
            .cast::<ICoreWebView2Profile7>()
            .ok()
    }
}

#[derive(Clone, Debug)]
pub struct ExtInfo {
    pub id: String,
    pub name: String,
    pub enabled: bool,
}

/// Load an unpacked extension folder into the profile. The callback gets the runtime id.
pub fn add_extension(view: &WebView, dir: &Path, cb: impl FnOnce(Result<ExtInfo, String>) + 'static) {
    let Some(p) = profile7(view) else { return cb(Err("Расширения не поддерживаются этой версией WebView2".into())) };
    unsafe {
        let handler = ProfileAddBrowserExtensionCompletedHandler::create(Box::new(move |hr, ext| {
            match (hr.is_ok(), ext) {
                (true, Some(e)) => cb(Ok(ExtInfo {
                    id: pwstr(|v| e.Id(v)),
                    name: pwstr(|v| e.Name(v)),
                    enabled: boolean(|v| e.IsEnabled(v)),
                })),
                _ => cb(Err(format!("WebView2 отклонил расширение ({:?})", hr))),
            }
            Ok(())
        }));
        if let Err(e) = p.AddBrowserExtension(&HSTRING::from(dir.as_os_str()), &handler) {
            log::warn(&format!("AddBrowserExtension: {e}"));
        }
    }
}

/// Run `f` on every installed extension (async; `done` gets the list after `f` ran).
pub fn with_extensions(
    view: &WebView,
    f: impl Fn(&ICoreWebView2BrowserExtension) + 'static,
    done: impl FnOnce(Vec<ExtInfo>) + 'static,
) {
    let Some(p) = profile7(view) else { return done(Vec::new()) };
    unsafe {
        let handler = ProfileGetBrowserExtensionsCompletedHandler::create(Box::new(move |_hr, list| {
            let mut out = Vec::new();
            if let Some(list) = list {
                let mut n = 0u32;
                let _ = list.Count(&mut n);
                for i in 0..n {
                    if let Ok(e) = list.GetValueAtIndex(i) {
                        f(&e);
                        out.push(ExtInfo {
                            id: pwstr(|v| e.Id(v)),
                            name: pwstr(|v| e.Name(v)),
                            enabled: boolean(|v| e.IsEnabled(v)),
                        });
                    }
                }
            }
            done(out);
            Ok(())
        }));
        let _ = p.GetBrowserExtensions(&handler);
    }
}

pub fn ext_id(e: &ICoreWebView2BrowserExtension) -> String {
    unsafe { pwstr(|v| e.Id(v)) }
}

pub fn ext_remove(e: &ICoreWebView2BrowserExtension) {
    unsafe {
        let _ = e.Remove(&BrowserExtensionRemoveCompletedHandler::create(Box::new(|_| Ok(()))));
    }
}

pub fn ext_enable(e: &ICoreWebView2BrowserExtension, on: bool) {
    unsafe {
        let _ = e.Enable(on, &BrowserExtensionEnableCompletedHandler::create(Box::new(|_| Ok(()))));
    }
}

// ---------------------------------------------------------------- DPAPI

fn blob(data: &[u8]) -> CRYPT_INTEGER_BLOB {
    CRYPT_INTEGER_BLOB { cbData: data.len() as u32, pbData: data.as_ptr() as *mut u8 }
}

unsafe fn take_blob(b: CRYPT_INTEGER_BLOB) -> Vec<u8> {
    let v = std::slice::from_raw_parts(b.pbData, b.cbData as usize).to_vec();
    let _ = LocalFree(Some(HLOCAL(b.pbData as _)));
    v
}

/// Decrypt data protected for the current Windows user.
pub fn dpapi_unprotect(data: &[u8]) -> Option<Vec<u8>> {
    unsafe {
        let mut out = CRYPT_INTEGER_BLOB::default();
        CryptUnprotectData(&blob(data), None, None, None, None, 0, &mut out).ok()?;
        Some(take_blob(out))
    }
}

/// Encrypt data so only the current Windows user can read it.
pub fn dpapi_protect(data: &[u8]) -> Option<Vec<u8>> {
    unsafe {
        let mut out = CRYPT_INTEGER_BLOB::default();
        CryptProtectData(&blob(data), None, None, None, None, 0, &mut out).ok()?;
        Some(take_blob(out))
    }
}

pub mod log {
    use std::io::Write;

    /// Problems worth diagnosing later go to `otium.log` in the profile (kept under 256 KB).
    pub fn warn(msg: &str) {
        #[cfg(debug_assertions)]
        eprintln!("otium: {msg}");
        let path = crate::util::data_dir().join("otium.log");
        if std::fs::metadata(&path).is_ok_and(|m| m.len() > 256 * 1024) {
            let _ = std::fs::rename(&path, path.with_extension("log.old"));
        }
        if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&path) {
            let s = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0) % 86400;
            let _ = writeln!(
                f,
                "{} {:02}:{:02}:{:02} UTC v{}: {msg}",
                crate::util::today(),
                s / 3600,
                s % 3600 / 60,
                s % 60,
                env!("CARGO_PKG_VERSION")
            );
        }
    }
}
