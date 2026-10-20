//! Native cross-process selection/drag regression against isolated app data.
//! No global clicks/keystrokes are injected; only this test's HWNDs receive input.
use serde_json::Value;
use std::{
    ffi::c_void,
    fs,
    path::PathBuf,
    thread,
    time::{Duration, Instant},
};
use windows::core::w;
use windows::Win32::{
    Foundation::*,
    Graphics::Gdi::{ClientToScreen, ScreenToClient},
    System::LibraryLoader::GetModuleHandleW,
    UI::{
        HiDpi::*,
        Input::KeyboardAndMouse::{GetAsyncKeyState, IsWindowEnabled, VK_LBUTTON},
        WindowsAndMessaging::*,
    },
};

unsafe extern "system" fn host_proc(h: HWND, m: u32, w: WPARAM, l: LPARAM) -> LRESULT {
    if m == WM_CLOSE {
        PostQuitMessage(0);
        LRESULT(0)
    } else {
        DefWindowProcW(h, m, w, l)
    }
}
fn pack(x: i32, y: i32) -> LPARAM {
    LPARAM((((y as u16 as u32) << 16) | x as u16 as u32) as isize)
}
unsafe fn send(h: HWND, m: u32, w: usize, l: LPARAM) {
    SendMessageW(h, m, Some(WPARAM(w)), Some(l));
}
unsafe fn command(h: HWND, c: usize) {
    send(h, WM_COMMAND, c, LPARAM(0));
}
unsafe fn click(h: HWND, row: i32, modifiers: usize) {
    send(h, WM_LBUTTONDOWN, 1 | modifiers, pack(120, 54 + 26 * row));
    send(h, WM_LBUTTONUP, modifiers, pack(120, 54 + 26 * row));
}
unsafe fn key(h: HWND, k: usize) {
    send(h, WM_KEYDOWN, k, LPARAM(0));
}
unsafe fn class(h: HWND) -> String {
    let mut s = [0u16; 128];
    let n = GetClassNameW(h, &mut s);
    String::from_utf16_lossy(&s[..n.max(0) as usize])
}
fn load(path: &PathBuf) -> Value {
    serde_json::from_slice(&fs::read(path).unwrap()).unwrap()
}
fn ids(layout: &Value, index: usize) -> Vec<String> {
    layout["fences"][index]["entries"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["id"].as_str().unwrap().to_owned())
        .collect()
}
#[track_caller]
unsafe fn wait(path: &PathBuf, controller: HWND, predicate: impl Fn(&Value) -> bool) {
    let until = Instant::now() + Duration::from_secs(5);
    loop {
        // The foreign child can synchronously ask its temporary host to paint.
        // Keep the probe's message queue alive while awaiting the app worker.
        let mut msg = MSG::default();
        while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
        if let Ok(data) = fs::read(path) {
            if let Ok(layout) = serde_json::from_slice::<Value>(&data) {
                let launcher = FindWindowW(w!("LumaDeskLauncherV1"), None).unwrap();
                let button = GetDlgItem(Some(launcher), 1001).unwrap();
                if layout["pending"].is_null()
                    && predicate(&layout)
                    && IsWindowEnabled(button).as_bool()
                {
                    send(controller, WM_APP + 2, 0, LPARAM(0));
                    return;
                }
            }
        }
        assert!(
            Instant::now() < until,
            "Timed out waiting for item transaction: {}",
            fs::read_to_string(path).unwrap_or_default()
        );
        thread::sleep(Duration::from_millis(20));
    }
}
struct Restore {
    windows: Vec<(HWND, HWND, POINT)>,
    host: HWND,
    cursor: POINT,
    foreground: HWND,
    controller: HWND,
}
impl Drop for Restore {
    fn drop(&mut self) {
        unsafe {
            for (h, parent, origin) in &self.windows {
                let _ = SetParent(*h, Some(*parent));
                let _ = SetWindowPos(
                    *h,
                    Some(HWND_TOP),
                    origin.x,
                    origin.y,
                    0,
                    0,
                    SWP_NOSIZE | SWP_NOACTIVATE,
                );
            }
            let _ = DestroyWindow(self.host);
            send(self.controller, WM_EXITMENULOOP, 0, LPARAM(0));
            let _ = SetCursorPos(self.cursor.x, self.cursor.y);
            let _ = SetForegroundWindow(self.foreground);
        }
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<_> = std::env::args_os().skip(1).collect();
    let interactive = args[0] == "--interactive";
    let (source, target, path, pid) = if interactive || args[0] == "--automated" {
        let path = PathBuf::from(&args[1]);
        let pid = args[2].to_string_lossy().parse::<u32>()?;
        unsafe extern "system" fn collect(h: HWND, context: LPARAM) -> windows::core::BOOL {
            let (pid, windows) = &mut *(context.0 as *mut (u32, Vec<HWND>));
            let mut owner = 0;
            GetWindowThreadProcessId(h, Some(&mut owner));
            if owner == *pid && class(h) == "LumaDeskFenceV1" {
                windows.push(h);
            }
            windows::core::BOOL(1)
        }
        let mut found = (pid, Vec::<HWND>::new());
        unsafe {
            let _ = EnumChildWindows(
                Some(GetShellWindow()),
                Some(collect),
                LPARAM((&mut found as *mut (u32, Vec<HWND>)) as isize),
            );
        }
        if found.1.len() != 2 {
            return Err("Expected two isolated groups".into());
        }
        found.1.sort_by_key(|h| unsafe {
            let mut title = [0u16; 128];
            let n = GetWindowTextW(*h, &mut title);
            String::from_utf16_lossy(&title[..n as usize]) != "Smoke test"
        });
        (found.1[0], found.1[1], path, pid)
    } else {
        (
            HWND(args[0].to_string_lossy().parse::<usize>()? as *mut c_void),
            HWND(args[1].to_string_lossy().parse::<usize>()? as *mut c_void),
            PathBuf::from(&args[2]),
            std::env::var("LUMADESK_SMOKE_PID")?.parse::<u32>()?,
        )
    };
    if !path
        .canonicalize()?
        .starts_with(std::env::temp_dir().canonicalize()?)
        || load(&path)["fences"][0]["title"] != "Smoke test"
    {
        return Err("Only isolated smoke data is supported".into());
    }
    unsafe {
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
        if GetAsyncKeyState(VK_LBUTTON.0 as i32) < 0 {
            return Err("Release the mouse button before testing".into());
        }
        for h in [source, target] {
            let mut owner = 0;
            GetWindowThreadProcessId(h, Some(&mut owner));
            if owner != pid || class(h) != "LumaDeskFenceV1" {
                return Err("Unexpected window owner".into());
            }
        }
        let controller = FindWindowW(w!("LumaDeskControllerV1"), None)?;
        send(controller, WM_TIMER, 1, LPARAM(0));
        let instance = HINSTANCE(GetModuleHandleW(None)?.0);
        for name in [w!("LumaArrangeHost"), w!("ForeignOverlayProbe")] {
            let wc = WNDCLASSW {
                hInstance: instance,
                lpszClassName: name,
                lpfnWndProc: Some(host_proc),
                ..Default::default()
            };
            if RegisterClassW(&wc) == 0 {
                return Err(windows::core::Error::from_win32().into());
            }
        }
        let host = CreateWindowExW(
            if interactive {
                WS_EX_TOPMOST
            } else {
                WS_EX_TOPMOST | WS_EX_TOOLWINDOW
            },
            w!("LumaArrangeHost"),
            w!("LumaDesk isolated arrangement test"),
            if interactive {
                WS_OVERLAPPEDWINDOW
            } else {
                WS_POPUP
            },
            80,
            80,
            780,
            600,
            None,
            None,
            Some(instance),
            None,
        )?;
        let mut cursor = POINT::default();
        GetCursorPos(&mut cursor)?;
        let mut restore = Restore {
            windows: vec![],
            host,
            cursor,
            foreground: GetForegroundWindow(),
            controller,
        };
        send(controller, WM_ENTERMENULOOP, 0, LPARAM(0));
        for (n, h) in [source, target].into_iter().enumerate() {
            let parent = GetParent(h)?;
            let mut rect = RECT::default();
            GetWindowRect(h, &mut rect)?;
            let mut origin = POINT {
                x: rect.left,
                y: rect.top,
            };
            let _ = ScreenToClient(parent, &mut origin);
            restore.windows.push((h, parent, origin));
            SetParent(h, Some(host))?;
            SetWindowPos(
                h,
                Some(HWND_TOP),
                10 + n as i32 * 380,
                10,
                0,
                0,
                SWP_NOSIZE | SWP_NOACTIVATE,
            )?;
        }
        let overlay = CreateWindowExW(
            WINDOW_EX_STYLE(0),
            w!("ForeignOverlayProbe"),
            w!("Unsupported overlay"),
            WS_CHILD | WS_VISIBLE,
            10,
            480,
            400,
            60,
            Some(host),
            None,
            Some(instance),
            None,
        )?;
        let _ = ShowWindow(host, SW_SHOWNORMAL);
        SetWindowPos(
            host,
            Some(HWND_TOPMOST),
            0,
            0,
            0,
            0,
            SWP_NOMOVE | SWP_NOSIZE | SWP_SHOWWINDOW,
        )?;
        if interactive {
            println!("INTERACTIVE_READY pid={pid} original_paths=untouched");
            let mut msg = MSG::default();
            while GetMessageW(&mut msg, None, 0, 0).as_bool() {
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
            return Ok(());
        }
        let initial = load(&path);
        let base = ids(&initial, 0);
        assert!(base.len() >= 5);
        assert!(ids(&initial, 1).is_empty());
        let undo_count = std::cell::Cell::new(0);
        let undo = || {
            undo_count.set(undo_count.get() + 1);
            println!("UNDO_STAGE={}", undo_count.get());
            command(source, 1018);
            wait(&path, controller, |l| {
                ids(l, 0) == base && ids(l, 1).is_empty()
            });
            key(source, 0x24);
        };

        // Shift ranges, then Ctrl toggles: only the successful selected entries
        // belong to the one resulting history record.
        key(source, 0x24);
        click(source, 0, 0);
        click(source, 2, 4);
        command(source, 1004); // 從分組移除（保留檔案）
        wait(&path, controller, |l| ids(l, 0) == base[3..]);
        assert_eq!(
            load(&path)["history"].as_array().unwrap().last().unwrap()["changes"]
                .as_array()
                .unwrap()
                .len(),
            3
        );
        undo();
        click(source, 0, 0);
        click(source, 2, 8);
        click(source, 0, 8);
        command(source, 1004); // 從分組移除（保留檔案）
        let without_third: Vec<_> = base
            .iter()
            .enumerate()
            .filter(|(i, _)| *i != 2)
            .map(|(_, id)| id.clone())
            .collect();
        wait(&path, controller, |l| ids(l, 0) == without_third);
        undo();

        // Changing the query clears selection; Delete cannot affect hidden rows.
        command(source, 1021);
        command(source, 1012);
        send(source, WM_CHAR, 'z' as usize, LPARAM(0));
        command(source, 1004); // 從分組移除（保留檔案）
        thread::sleep(Duration::from_millis(30));
        assert_eq!(ids(&load(&path), 0), base);
        key(source, 0x1B);
        command(source, 1012);
        for c in "copy-only".chars() {
            send(source, WM_CHAR, c as usize, LPARAM(0));
        }
        command(source, 1021);
        command(source, 1004); // 從分組移除（保留檔案）
        let only_filtered: Vec<_> = initial["fences"][0]["entries"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|e| !e["path"].as_str().unwrap().contains("copy-only"))
            .map(|e| e["id"].as_str().unwrap().to_owned())
            .collect();
        wait(&path, controller, |l| ids(l, 0) == only_filtered);
        undo();
        key(source, 0x1B);

        // Batch insertion before a row and into the empty tail.
        click(source, 1, 0);
        click(source, 3, 8);
        send(source, WM_LBUTTONDOWN, 1, pack(120, 169));
        send(source, WM_MOUSEMOVE, 1, pack(120, 54));
        send(source, WM_LBUTTONUP, 0, pack(120, 54));
        let mut before = vec![base[1].clone(), base[3].clone()];
        before.extend(
            base.iter()
                .enumerate()
                .filter(|(i, _)| *i != 1 && *i != 3)
                .map(|(_, id)| id.clone()),
        );
        wait(&path, controller, |l| ids(l, 0) == before);
        undo();
        click(source, 0, 0);
        click(source, 2, 8);
        send(source, WM_LBUTTONDOWN, 1, pack(120, 54));
        send(source, WM_MOUSEMOVE, 1, pack(120, 390));
        send(source, WM_LBUTTONUP, 0, pack(120, 390));
        let mut tail: Vec<_> = base
            .iter()
            .enumerate()
            .filter(|(i, _)| *i != 0 && *i != 2)
            .map(|(_, id)| id.clone())
            .collect();
        tail.extend([base[0].clone(), base[2].clone()]);
        wait(&path, controller, |l| ids(l, 0) == tail);
        undo();

        let mut cross = POINT { x: 120, y: 54 };
        let _ = ClientToScreen(target, &mut cross);
        let _ = ScreenToClient(source, &mut cross);
        click(source, 0, 0);
        click(source, 2, 8);
        send(source, WM_LBUTTONDOWN, 1, pack(120, 54));
        send(source, WM_MOUSEMOVE, 1, pack(cross.x, cross.y));
        send(source, WM_LBUTTONUP, 0, pack(cross.x, cross.y));
        let picked = vec![base[0].clone(), base[2].clone()];
        wait(&path, controller, |l| {
            ids(l, 1) == picked && ids(l, 0).len() == base.len() - 2
        });
        undo();

        // Escape, lost capture, an unrelated window, and another app's overlay.
        for mode in 0..4 {
            click(source, 0, 0);
            send(source, WM_LBUTTONDOWN, 1, pack(120, 54));
            send(source, WM_MOUSEMOVE, 1, pack(cross.x, cross.y));
            match mode {
                0 => key(source, 0x1B),
                1 => {
                    send(source, WM_CAPTURECHANGED, 0, LPARAM(host.0 as isize));
                    key(source, 0x1B);
                }
                2 => {
                    let mut p = POINT { x: 740, y: 560 };
                    let _ = ClientToScreen(host, &mut p);
                    let _ = ScreenToClient(source, &mut p);
                    send(source, WM_LBUTTONUP, 0, pack(p.x, p.y));
                }
                _ => {
                    let mut p = POINT { x: 100, y: 20 };
                    let _ = ClientToScreen(overlay, &mut p);
                    assert_eq!(WindowFromPoint(p), overlay);
                    let _ = ScreenToClient(source, &mut p);
                    send(source, WM_LBUTTONUP, 0, pack(p.x, p.y));
                }
            }
            send(source, WM_LBUTTONUP, 0, pack(cross.x, cross.y));
            assert_eq!(ids(&load(&path), 0), base);
            assert!(ids(&load(&path), 1).is_empty());
        }

        // Find a genuine uncovered Explorer desktop point. Overlay windows are
        // deliberately excluded, as in the production destination check.
        let left = GetSystemMetrics(SM_XVIRTUALSCREEN);
        let top = GetSystemMetrics(SM_YVIRTUALSCREEN);
        let mut desktop_point = None;
        'scan: for y in (top + 20..top + GetSystemMetrics(SM_CYVIRTUALSCREEN) - 20).step_by(100) {
            for x in (left + 20..left + GetSystemMetrics(SM_CXVIRTUALSCREEN) - 20).step_by(100) {
                let p = POINT { x, y };
                let h = WindowFromPoint(p);
                if class(h) == "SysListView32"
                    && GetParent(h).is_ok_and(|p| class(p) == "SHELLDLL_DefView")
                {
                    desktop_point = Some(p);
                    break 'scan;
                }
            }
        }
        key(source, 0x23); // final outbound reference
        let desktop_mode = if let Some(mut desktop_point) = desktop_point {
            let _ = ScreenToClient(source, &mut desktop_point);
            send(source, WM_LBUTTONDOWN, 1, pack(120, 340));
            send(
                source,
                WM_MOUSEMOVE,
                1,
                pack(desktop_point.x, desktop_point.y),
            );
            send(
                source,
                WM_LBUTTONUP,
                0,
                pack(desktop_point.x, desktop_point.y),
            );
            "drag"
        } else {
            // Do not minimize unrelated user applications to expose the desktop.
            // Exercise the same ungroup command and report the unavailable drag.
            command(source, 1019);
            "command_fallback_no_exposed_desktop"
        };
        wait(&path, controller, |l| ids(l, 0).len() == base.len() - 1);
        let current = load(&path);
        let history = current["history"].as_array().unwrap().last().unwrap();
        assert!(history["changes"][0]["action"].is_null());
        let original = PathBuf::from(
            history["changes"][0]["before"]["entry"]["path"]
                .as_str()
                .unwrap(),
        );
        assert!(original.exists());
        undo();
        assert!(original.exists());
        println!("ARRANGE_OK ctrl_toggle shift_range filtered_select_all batch_before batch_tail cross_group escape capture_lost invalid_window overlay_excluded outbound_desktop={desktop_mode} undo");
    }
    Ok(())
}
