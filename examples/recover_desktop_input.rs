//! Explicit incident repair after desktop managers have been closed.
//! Never called automatically by LumaDesk; does not restart Explorer.
use windows::Win32::UI::Input::KeyboardAndMouse::{EnableWindow, IsWindowEnabled};
use windows::Win32::UI::WindowsAndMessaging::*;
fn main() -> Result<(), Box<dyn std::error::Error>> {
    unsafe {
        let shell = GetShellWindow();
        let mut class = [0u16; 64];
        let len = GetClassNameW(shell, &mut class);
        if shell.is_invalid() || String::from_utf16_lossy(&class[..len as usize]) != "Progman" {
            return Err("No verified Explorer desktop root".into());
        }
        let popup = GetLastActivePopup(shell);
        if popup != shell && IsWindowVisible(popup).as_bool() {
            return Err("Desktop has an active popup; preserving its modal state".into());
        }
        if !std::env::args().any(|a| a == "--restore") {
            println!("DESKTOP_INPUT enabled={}", IsWindowEnabled(shell).as_bool());
            return Ok(());
        }
        let _ = EnableWindow(shell, true);
        if !IsWindowEnabled(shell).as_bool() {
            return Err("Desktop remains disabled".into());
        }
        println!("DESKTOP_INPUT_RESTORED explorer_not_restarted=true files=untouched");
    }
    Ok(())
}
