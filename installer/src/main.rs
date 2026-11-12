//! OTIUM Browser setup: a small native window with an HTML UI (WebView2),
//! installing per user (no admin rights) into a folder of the user's choice.

#![windows_subsystem = "windows"]

mod shell;

use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
};

use base64::Engine as _;
use serde::Deserialize;
use serde_json::json;
use shell::{APP_NAME, EXE_NAME, MARKER};
use tao::{
    dpi::LogicalSize,
    event::{Event, WindowEvent},
    event_loop::{ControlFlow, EventLoopBuilder, EventLoopProxy},
    platform::windows::WindowBuilderExtWindows,
    window::{Icon, WindowBuilder},
};
use wry::WebViewBuilder;

const OTIUM_EXE: &[u8] = include_bytes!(env!("OTIUM_EXE"));
const ICON_PNG: &[u8] = include_bytes!("../../assets/icon.png");
const UI: &str = include_str!("ui.html");
const VERSION: &str = env!("CARGO_PKG_VERSION");

enum Ev {
    Ui(String),
    Progress(u32, &'static str),
    Done(PathBuf),
    Failed(String),
}

#[derive(Deserialize)]
#[serde(tag = "cmd", rename_all = "snake_case")]
enum Cmd {
    Drag,
    Minimize,
    Close,
    Browse,
    Install { dir: String, desktop: bool, start_menu: bool },
    Launch,
}

struct Options {
    dir: PathBuf,
    desktop: bool,
    start_menu: bool,
}

fn default_dir() -> PathBuf {
    shell::previous_install().unwrap_or_else(|| {
        std::env::var_os("LOCALAPPDATA")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(r"C:\OTIUM"))
            .join("Programs")
            .join(APP_NAME)
    })
}

fn free_space_gb(dir: &Path) -> Option<f64> {
    use windows::{core::HSTRING, Win32::Storage::FileSystem::GetDiskFreeSpaceExW};
    // walk up to an existing ancestor (the target folder may not exist yet)
    let mut p = dir.to_path_buf();
    while !p.exists() {
        p = p.parent()?.to_path_buf();
    }
    let mut free = 0u64;
    unsafe { GetDiskFreeSpaceExW(&HSTRING::from(p.as_os_str()), Some(&mut free), None, None).ok()? };
    Some(free as f64 / 1_073_741_824.0)
}

fn dir_info(dir: &Path) -> serde_json::Value {
    json!({
        "dir": dir.display().to_string(),
        "upgrade": dir.join(EXE_NAME).exists(),
        "free": free_space_gb(dir),
        "need": (OTIUM_EXE.len() as f64 / 1_048_576.0).ceil(),
    })
}

/// The first version installed as %LOCALAPPDATA%\Programs\OTIUM\otium.exe, which clashes with the
/// OTIUM launcher's names. Remove it only if that exe is really the browser.
fn remove_legacy_install() {
    let Some(base) = std::env::var_os("LOCALAPPDATA").map(PathBuf::from) else { return };
    let dir = base.join("Programs").join("OTIUM");
    let exe = dir.join("otium.exe");
    let is_browser = fs::read(&exe)
        .map(|b| b.windows(15).any(|w| w == b"otium.localhost"))
        .unwrap_or(false);
    // no taskkill here: "otium.exe" is also the launcher's image name; a locked file is simply left
    if is_browser {
        if fs::remove_file(&exe).is_ok() {
            let _ = fs::remove_dir(&dir); // only if now empty
        }
    }
}

fn install(o: &Options, proxy: &EventLoopProxy<Ev>) -> Result<PathBuf, String> {
    let step = |p, t| {
        let _ = proxy.send_event(Ev::Progress(p, t));
    };
    step(4, "Подготовка…");
    fs::create_dir_all(&o.dir).map_err(|e| format!("Не удалось создать папку:\n{e}\n\nВыберите другую папку (например, в своём профиле)."))?;
    let probe = o.dir.join(".write-test");
    fs::write(&probe, b"ok").map_err(|_| "Нет прав на запись в эту папку. Выберите другую или запустите установщик от имени администратора.".to_string())?;
    let _ = fs::remove_file(probe);

    step(10, "Закрываю запущенный браузер…");
    let _ = shell::hidden("taskkill").args(["/F", "/IM", EXE_NAME]).output();
    std::thread::sleep(std::time::Duration::from_millis(500));

    let exe = o.dir.join(EXE_NAME);
    let tmp = o.dir.join(format!("{EXE_NAME}.new"));
    {
        let mut f = fs::File::create(&tmp).map_err(|e| e.to_string())?;
        let chunks = OTIUM_EXE.chunks(256 * 1024);
        let n = chunks.len().max(1);
        for (i, c) in chunks.enumerate() {
            f.write_all(c).map_err(|e| e.to_string())?;
            step(12 + (60 * (i + 1) / n) as u32, "Копирую файлы…");
        }
        f.sync_all().map_err(|e| e.to_string())?;
    }
    fs::rename(&tmp, &exe).map_err(|e| format!("Не удалось заменить файл (браузер ещё открыт?):\n{e}"))?;
    let _ = fs::write(o.dir.join(MARKER), format!("OTIUM Browser {VERSION}\n"));

    step(78, "Создаю ярлыки…");
    let mut made = Vec::new();
    if o.start_menu {
        if let Some(l) = shell::start_menu_link() {
            shell::write_shortcut(&l, &exe).map_err(|e| format!("Ярлык в меню Пуск: {e}"))?;
            made.push(l);
        }
    }
    if o.desktop {
        if let Some(l) = shell::desktop_link() {
            shell::write_shortcut(&l, &exe).map_err(|e| format!("Ярлык на рабочем столе: {e}"))?;
            made.push(l);
        }
    }
    shell::refresh_icons();

    step(90, "Регистрирую в системе…");
    let exe_s = exe.display().to_string();
    shell::register(
        &[
            ("DisplayName", APP_NAME.to_string()),
            ("DisplayVersion", VERSION.to_string()),
            ("Publisher", "SoFTeR1".to_string()),
            ("DisplayIcon", format!("\"{exe_s}\",0")),
            ("InstallLocation", o.dir.display().to_string()),
            ("UninstallString", format!("\"{exe_s}\" --uninstall")),
        ],
        (OTIUM_EXE.len() / 1024) as u32,
    )
    .map_err(|e| format!("Реестр: {e}"))?;

    step(96, "Убираю старую версию…");
    remove_legacy_install();
    step(100, "Готово");
    Ok(exe)
}

fn main() {
    let event_loop = EventLoopBuilder::<Ev>::with_user_event().build();
    let proxy = event_loop.create_proxy();

    let icon = {
        let mut dec = png::Decoder::new(std::io::Cursor::new(ICON_PNG));
        dec.set_transformations(png::Transformations::EXPAND);
        dec.read_info().ok().and_then(|mut r| {
            let mut buf = vec![0; r.output_buffer_size()?];
            let info = r.next_frame(&mut buf).ok()?;
            Icon::from_rgba(buf[..info.buffer_size()].to_vec(), info.width, info.height).ok()
        })
    };
    let window = WindowBuilder::new()
        .with_title("Установка OTIUM Browser")
        .with_inner_size(LogicalSize::new(820.0, 520.0))
        .with_resizable(false)
        .with_decorations(false)
        .with_undecorated_shadow(true)
        .with_visible(false)
        .with_background_color((12, 12, 18, 255))
        .with_window_icon(icon)
        .build(&event_loop)
        .expect("window");

    let init = json!({
        "version": VERSION,
        "logo": format!("data:image/png;base64,{}", base64::engine::general_purpose::STANDARD.encode(ICON_PNG)),
        "info": dir_info(&default_dir()),
    });
    let html = UI.replacen("/*__INIT__*/null", &init.to_string().replace('<', "\\u003c"), 1);
    let p = proxy.clone();
    let webview = WebViewBuilder::new()
        .with_html(html)
        .with_background_color((12, 12, 18, 255))
        .with_ipc_handler(move |req| {
            let _ = p.send_event(Ev::Ui(req.into_body()));
        })
        .build(&window)
        .expect("WebView2 недоступен");
    window.set_visible(true);

    let mut installed: Option<PathBuf> = None;
    let mut busy = false;
    event_loop.run(move |event, _, flow| {
        *flow = ControlFlow::Wait;
        let eval = |js: String| {
            let _ = webview.evaluate_script(&js);
        };
        match event {
            Event::WindowEvent { event: WindowEvent::CloseRequested, .. } if !busy => *flow = ControlFlow::Exit,
            Event::UserEvent(Ev::Progress(p, t)) => eval(format!("ui.progress({p}, {})", json!(t))),
            Event::UserEvent(Ev::Done(exe)) => {
                busy = false;
                installed = Some(exe);
                eval("ui.done()".into());
            }
            Event::UserEvent(Ev::Failed(e)) => {
                busy = false;
                eval(format!("ui.failed({})", json!(e)));
            }
            Event::UserEvent(Ev::Ui(msg)) => match serde_json::from_str::<Cmd>(&msg) {
                Ok(Cmd::Drag) => {
                    let _ = window.drag_window();
                }
                Ok(Cmd::Minimize) => window.set_minimized(true),
                Ok(Cmd::Close) if !busy => *flow = ControlFlow::Exit,
                Ok(Cmd::Browse) => {
                    let start = default_dir();
                    let picked = rfd::FileDialog::new()
                        .set_title("Папка для установки OTIUM Browser")
                        .set_directory(start.parent().unwrap_or(&start))
                        .set_parent(&window)
                        .pick_folder();
                    if let Some(mut d) = picked {
                        if d.file_name().map(|n| n.to_string_lossy().to_string()) != Some(APP_NAME.to_string()) {
                            d = d.join(APP_NAME);
                        }
                        eval(format!("ui.setInfo({})", dir_info(&d)));
                    }
                }
                Ok(Cmd::Install { dir, desktop, start_menu }) if !busy => {
                    let dir = PathBuf::from(dir.trim());
                    if !dir.is_absolute() {
                        eval(format!("ui.failed({})", json!("Укажите полный путь к папке")));
                        return;
                    }
                    busy = true;
                    let p = proxy.clone();
                    std::thread::spawn(move || {
                        let o = Options { dir, desktop, start_menu };
                        let ev = match install(&o, &p) {
                            Ok(exe) => Ev::Done(exe),
                            Err(e) => Ev::Failed(e),
                        };
                        let _ = p.send_event(ev);
                    });
                }
                Ok(Cmd::Launch) => {
                    if let Some(exe) = &installed {
                        let _ = std::process::Command::new(exe).current_dir(exe.parent().unwrap()).spawn();
                    }
                    *flow = ControlFlow::Exit;
                }
                _ => {}
            },
            _ => {}
        }
    });
}
