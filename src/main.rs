#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod autofile;
mod backdrop;
mod collection;
mod desktop_icons;
mod i18n;
mod launcher;
mod meta;
mod model;
mod operations;
mod organize;
mod render;
mod shell_icons;

fn main() {
    let args: Vec<_> = std::env::args_os().collect();
    // Items opened from a group inherit this directory unless they get their
    // own; never keep the install or launch folder busy.
    if let Some(home) = std::env::var_os("USERPROFILE") {
        let _ = std::env::set_current_dir(home);
    }
    if args.get(1).is_some_and(|a| a == "--desktop-guardian") {
        if let (Some(owner), Some(path)) = (
            args.get(2)
                .and_then(|a| a.to_str())
                .and_then(|a| a.parse().ok()),
            args.get(3),
        ) {
            unsafe {
                let _ = desktop_icons::guardian(owner, std::path::Path::new(path));
            }
        }
        return;
    }
    if let Err(err) = unsafe { app::run() } {
        use windows::core::{w, PCWSTR};
        use windows::Win32::UI::WindowsAndMessaging::{MessageBoxW, MB_ICONERROR, MB_OK};
        let message = i18n::tf(i18n::K::StartFailed, &[&err]);
        let message: Vec<u16> = message.encode_utf16().chain(Some(0)).collect();
        unsafe {
            MessageBoxW(
                None,
                PCWSTR(message.as_ptr()),
                w!("LumaDesk"),
                MB_OK | MB_ICONERROR,
            );
        }
    }
}
