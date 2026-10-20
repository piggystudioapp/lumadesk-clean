//! Interactive, cross-process OLE regression probe used by scripts/smoke.ps1.
//! A temporary host exposes only the isolated test collection; other desktop
//! desktop apps are left running. No source files are moved.

use std::cell::Cell;
use std::ffi::c_void;
use std::os::windows::ffi::OsStrExt;
use std::path::PathBuf;
use std::time::Instant;
use windows::core::{implement, w, BOOL, HRESULT, PCWSTR};
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Gdi::ScreenToClient;
use windows::Win32::System::Com::{CoTaskMemFree, IDataObject};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Ole::*;
use windows::Win32::System::SystemServices::MODIFIERKEYS_FLAGS;
use windows::Win32::UI::HiDpi::*;
use windows::Win32::UI::Input::KeyboardAndMouse::*;
use windows::Win32::UI::Shell::Common::ITEMIDLIST;
use windows::Win32::UI::Shell::*;
use windows::Win32::UI::WindowsAndMessaging::*;

#[implement(IDropSource)]
struct ProbeSource {
    received_feedback: Cell<bool>,
    cancel: bool,
    expected: DROPEFFECT,
    started: Instant,
}

impl IDropSource_Impl for ProbeSource_Impl {
    fn QueryContinueDrag(&self, escape: BOOL, _keys: MODIFIERKEYS_FLAGS) -> HRESULT {
        if escape.as_bool()
            || self.started.elapsed().as_secs() >= 5
            || (self.cancel && self.received_feedback.get())
        {
            eprintln!(
                "OLE cancellation escape={} elapsed_ms={}",
                escape.as_bool(),
                self.started.elapsed().as_millis()
            );
            DRAGDROP_S_CANCEL
        } else if self.received_feedback.get() {
            DRAGDROP_S_DROP
        } else {
            HRESULT(0)
        }
    }

    fn GiveFeedback(&self, effect: DROPEFFECT) -> HRESULT {
        // OLE can give initial NONE feedback before the target has accepted the
        // data. A synthetic drop must wait for the negotiated effect.
        if (self.cancel || effect == self.expected) && !self.received_feedback.replace(true) {
            // Wake this probe's OLE loop without injecting a global mouse
            // click. QueryContinueDrag explicitly completes after feedback.
            eprintln!("OLE feedback effect={}", effect.0);
            let mut point = POINT::default();
            unsafe {
                let _ = GetCursorPos(&mut point);
                let packed = ((point.y as u16 as u32) << 16) | point.x as u16 as u32;
                let _ = PostMessageW(None, WM_LBUTTONUP, WPARAM(0), LPARAM(packed as isize));
            }
        }
        DRAGDROP_S_USEDEFAULTCURSORS
    }
}

unsafe extern "system" fn wake_drag_loop(_: HWND, _: u32, _: usize, _: u32) {
    let mut point = POINT::default();
    if GetCursorPos(&mut point).is_ok() {
        let packed = ((point.y as u16 as u32) << 16) | point.x as u16 as u32;
        // Only this probe's queue is woken, without injecting global input.
        let _ = PostMessageW(None, WM_MOUSEMOVE, WPARAM(0), LPARAM(packed as isize));
    }
}

struct Pidl(*mut ITEMIDLIST);
impl Drop for Pidl {
    fn drop(&mut self) {
        unsafe { CoTaskMemFree(Some(self.0.cast::<c_void>())) };
    }
}

struct Ole;
impl Drop for Ole {
    fn drop(&mut self) {
        unsafe { OleUninitialize() };
    }
}

struct Restore {
    fence: HWND,
    parent: HWND,
    origin: POINT,
    cursor: POINT,
    foreground: HWND,
    host: HWND,
    controller: HWND,
}

unsafe extern "system" fn host_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    DefWindowProcW(hwnd, msg, wp, lp)
}

impl Drop for Restore {
    fn drop(&mut self) {
        unsafe {
            let _ = SetParent(self.fence, Some(self.parent));
            let _ = SetWindowPos(
                self.fence,
                Some(HWND_TOP),
                self.origin.x,
                self.origin.y,
                0,
                0,
                SWP_NOSIZE | SWP_NOACTIVATE,
            );
            let _ = DestroyWindow(self.host);
            SendMessageW(
                self.controller,
                WM_EXITMENULOOP,
                Some(WPARAM(0)),
                Some(LPARAM(0)),
            );
            let _ = SetCursorPos(self.cursor.x, self.cursor.y);
            if !self.foreground.is_invalid() {
                let _ = SetForegroundWindow(self.foreground);
            }
        }
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args_os().skip(1);
    let mut fence = HWND(
        args.next()
            .ok_or("Expected the isolated smoke collection HWND")?
            .to_string_lossy()
            .parse::<usize>()? as *mut c_void,
    );
    let mode = args.next().ok_or("Expected link, copy, move, or cancel")?;
    let files: Vec<PathBuf> = args.map(PathBuf::from).collect();
    if files.is_empty() {
        return Err("Expected at least one source path".into());
    }
    unsafe {
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
        OleInitialize(None)?;
        let _ole = Ole;
        // The smoke script sets this marker only for its isolated test process.
        let expected_pid = std::env::var("LUMADESK_SMOKE_PID")?.parse::<u32>()?;
        let mut owner = 0;
        GetWindowThreadProcessId(fence, Some(&mut owner));
        if owner != expected_pid {
            return Err("The probe only accepts the isolated smoke process".into());
        }
        let desktop_parent = GetParent(fence)?;

        let mut pidls = Vec::new();
        eprintln!("OLE preparing Shell paths");
        for path in &files {
            let text: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
            let mut pidl = std::ptr::null_mut();
            SHParseDisplayName(PCWSTR(text.as_ptr()), None, &mut pidl, 0, None)?;
            pidls.push(Pidl(pidl));
        }
        let pointers: Vec<*const ITEMIDLIST> = pidls.iter().map(|p| p.0.cast_const()).collect();
        let items = SHCreateShellItemArrayFromIDLists(&pointers)?;
        // Use the same Shell IDataObject format as Explorer, rather than
        // bypassing OLE by posting WM_DROPFILES to the target.
        let data: IDataObject = items.BindToHandler(None, &BHID_DataObject)?;
        eprintln!("OLE Shell data ready");
        // Reconcile/rearm immediately before temporary hosting, after Shell
        // data preparation. Process start time is not the timer's start time.
        let controller = FindWindowW(w!("LumaDeskControllerV1"), None)?;
        GetWindowThreadProcessId(controller, Some(&mut owner));
        if owner != expected_pid {
            return Err("Unexpected controller owner".into());
        }
        SendMessageW(controller, WM_TIMER, Some(WPARAM(1)), Some(LPARAM(0)));
        fence = FindWindowExW(Some(desktop_parent), None, w!("LumaDeskFenceV1"), None)?;
        GetWindowThreadProcessId(fence, Some(&mut owner));
        if owner != expected_pid {
            return Err("Unexpected collection owner".into());
        }

        let instance = HINSTANCE(GetModuleHandleW(None)?.0);
        let class = WNDCLASSW {
            hInstance: instance,
            lpszClassName: w!("LumaDeskOleSmokeHost"),
            lpfnWndProc: Some(host_proc),
            ..Default::default()
        };
        if RegisterClassW(&class) == 0 {
            return Err(windows::core::Error::from_win32().into());
        }
        let parent = GetParent(fence)?;
        let mut rect = RECT::default();
        GetWindowRect(fence, &mut rect)?;
        let mut origin = POINT {
            x: rect.left,
            y: rect.top,
        };
        let _ = ScreenToClient(parent, &mut origin);
        let mut cursor = POINT::default();
        GetCursorPos(&mut cursor)?;
        let foreground = GetForegroundWindow();
        let host = CreateWindowExW(
            WS_EX_TOPMOST | WS_EX_TOOLWINDOW,
            class.lpszClassName,
            w!("LumaDesk OLE regression test"),
            WS_OVERLAPPEDWINDOW,
            100,
            100,
            420,
            520,
            None,
            None,
            Some(instance),
            None,
        )?;
        let _restore = Restore {
            fence,
            parent,
            origin,
            cursor,
            foreground,
            host,
            controller,
        };
        // The fixture's temporary host replaces normal desktop interaction.
        // Enter the controller's modal pause on its own thread so even a queued
        // refresh cannot reparent the target in the middle of DoDragDrop.
        SendMessageW(
            controller,
            WM_ENTERMENULOOP,
            Some(WPARAM(0)),
            Some(LPARAM(0)),
        );
        let started = Instant::now();
        SetParent(fence, Some(host))?;
        SetWindowPos(
            fence,
            Some(HWND_TOP),
            10,
            10,
            0,
            0,
            SWP_NOSIZE | SWP_NOACTIVATE,
        )?;
        let _ = ShowWindow(host, SW_SHOWNORMAL);
        // The script hides the probe's console via STARTUPINFO; its first
        // ShowWindow can consequently inherit SW_HIDE. Explicitly expose only
        // this short-lived test host, never the user's Explorer desktop.
        SetWindowPos(
            host,
            Some(HWND_TOPMOST),
            0,
            0,
            0,
            0,
            SWP_NOMOVE | SWP_NOSIZE | SWP_SHOWWINDOW,
        )?;
        let _ = SetForegroundWindow(host);
        GetWindowRect(fence, &mut rect)?;
        let point = POINT {
            x: rect.left + 100,
            y: rect.top + 110.min((rect.bottom - rect.top) / 2),
        };
        SetCursorPos(point.x, point.y)?;
        if WindowFromPoint(point) != fence {
            return Err("The test collection is obscured; OLE was not invoked".into());
        }
        if GetAsyncKeyState(VK_LBUTTON.0 as i32) < 0 {
            return Err("Release the mouse button before running the interactive probe".into());
        }
        let allowed = match mode.to_string_lossy().as_ref() {
            "copy" => DROPEFFECT_COPY,
            "move" | "collect" => DROPEFFECT_MOVE,
            "link" | "cancel" => DROPEFFECT_COPY | DROPEFFECT_MOVE | DROPEFFECT_LINK,
            _ => return Err("Unknown drop effect".into()),
        };
        let expected = match mode.to_string_lossy().as_ref() {
            "copy" => DROPEFFECT_COPY,
            "link" => DROPEFFECT_LINK,
            "collect" => DROPEFFECT_NONE,
            _ => DROPEFFECT_NONE,
        };
        let source: IDropSource = ProbeSource {
            received_feedback: Cell::new(false),
            cancel: mode == "cancel" || expected == DROPEFFECT_NONE,
            expected,
            started: Instant::now(),
        }
        .into();
        let mut effect = DROPEFFECT_NONE;
        eprintln!("OLE loop starting");
        let timer = SetTimer(None, 0, 50, Some(wake_drag_loop));
        if timer == 0 {
            return Err(windows::core::Error::from_win32().into());
        }
        let result = DoDragDrop(&data, &source, allowed, &mut effect);
        let _ = KillTimer(None, timer);
        let cancelled = mode == "cancel" || expected == DROPEFFECT_NONE;
        if effect != expected || (cancelled && result != DRAGDROP_S_CANCEL) {
            return Err(format!(
                "OLE failed: result={result:?}, effect={}, expected={}",
                effect.0, expected.0
            )
            .into());
        }
        if !cancelled && result != DRAGDROP_S_DROP {
            return Err(format!("OLE did not reach Drop: {result:?}").into());
        }
        println!(
            "OLE_DROP_OK mode={} effect={} paths={} elapsed_ms={}",
            mode.to_string_lossy(),
            effect.0,
            files.len(),
            started.elapsed().as_millis()
        );
    }
    Ok(())
}
