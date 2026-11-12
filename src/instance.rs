//! One running browser per user. A later launch (a link clicked in another app, the
//! shortcut, a restart) hands its link to the running instance over a named pipe and exits,
//! so two processes never fight over the same WebView2 profile.

use std::{
    io::{Read, Write},
    os::windows::io::FromRawHandle,
    time::Duration,
};

use windows::{
    core::HSTRING,
    Win32::{
        Foundation::{ERROR_PIPE_CONNECTED, INVALID_HANDLE_VALUE, WAIT_ABANDONED, WAIT_OBJECT_0},
        Storage::FileSystem::PIPE_ACCESS_INBOUND,
        System::{
            Pipes::{
                ConnectNamedPipe, CreateNamedPipeW, PIPE_READMODE_BYTE, PIPE_REJECT_REMOTE_CLIENTS, PIPE_TYPE_BYTE,
                PIPE_UNLIMITED_INSTANCES, PIPE_WAIT,
            },
            Threading::{CreateMutexW, WaitForSingleObject},
        },
        UI::WindowsAndMessaging::{AllowSetForegroundWindow, ASFW_ANY},
    },
};

fn user_tag() -> String {
    std::env::var("USERNAME").unwrap_or_default().chars().filter(|c| c.is_ascii_alphanumeric()).collect()
}

fn pipe_name() -> String {
    format!(r"\\.\pipe\otium-browser-{}", user_tag())
}

/// Try to become the running instance, waiting up to `wait_ms` for a previous one to exit.
/// The mutex stays owned by this (main) thread until the process ends.
pub fn claim(wait_ms: u32) -> bool {
    let name = HSTRING::from(format!(r"Local\OtiumBrowser-{}", user_tag()));
    unsafe {
        let Ok(h) = CreateMutexW(None, false, &name) else { return true };
        let r = WaitForSingleObject(h, wait_ms);
        r == WAIT_OBJECT_0 || r == WAIT_ABANDONED
    }
}

/// Send a link ("" just brings the window up) to the running instance.
pub fn forward(msg: &str) -> bool {
    // let the running instance take the foreground: this process got it from the user's click
    unsafe {
        let _ = AllowSetForegroundWindow(ASFW_ANY);
    }
    for _ in 0..40 {
        match std::fs::OpenOptions::new().write(true).open(pipe_name()) {
            Ok(mut f) => return f.write_all(msg.as_bytes()).is_ok(),
            Err(_) => std::thread::sleep(Duration::from_millis(50)),
        }
    }
    false
}

/// Accept messages from later launches on a background thread.
pub fn serve(on_msg: impl Fn(String) + Send + 'static) {
    std::thread::spawn(move || {
        let name = HSTRING::from(pipe_name());
        loop {
            let h = unsafe {
                CreateNamedPipeW(
                    &name,
                    PIPE_ACCESS_INBOUND,
                    PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS,
                    PIPE_UNLIMITED_INSTANCES,
                    0,
                    4096,
                    0,
                    None,
                )
            };
            if h.is_invalid() || h == INVALID_HANDLE_VALUE {
                std::thread::sleep(Duration::from_secs(1));
                continue;
            }
            let connected = match unsafe { ConnectNamedPipe(h, None) } {
                Ok(()) => true,
                Err(e) => e.code() == ERROR_PIPE_CONNECTED.to_hresult(),
            };
            // the File owns the handle from here on and closes it
            let f = unsafe { std::fs::File::from_raw_handle(h.0 as _) };
            if connected {
                let mut s = String::new();
                if f.take(16 * 1024).read_to_string(&mut s).is_ok() {
                    on_msg(s);
                }
            }
        }
    });
}
