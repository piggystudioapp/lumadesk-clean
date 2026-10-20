//! LumaDesk's single-threaded Windows Shell integration and input handling.
//!
//! A collection is a child of Explorer's SHELLDLL_DefView, above its icon
//! ListView sibling. Ordinary application windows remain in front.
use crate::i18n::{self, K};

use crate::model::{
    config_path, free_position, Area, Bounds, Fence, Layout, Snapped, SortKey, Theme, ViewMode,
    HEADER_HEIGHT, MIN_HEIGHT, MIN_WIDTH,
};
use crate::operations::{Command, Outcome};
use crate::render::{ItemDisplay, Renderer};
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::collections::HashSet;
use std::ffi::{c_void, OsString};
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, Sender};
use std::time::{Duration, Instant};
use windows::core::{implement, w, Interface, BOOL, HRESULT, PCWSTR};
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Gdi::{
    BeginPaint, ClientToScreen, CreateRoundRectRgn, EndPaint, EnumDisplayMonitors, GetMonitorInfoW,
    GetStockObject, GetSysColorBrush, InvalidateRect, SetWindowRgn, COLOR_WINDOW, DEFAULT_GUI_FONT,
    HDC, HMONITOR, MONITORINFO, PAINTSTRUCT,
};
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CoUninitialize, IDataObject, CLSCTX_ALL,
    CLSCTX_INPROC_SERVER, CLSCTX_LOCAL_SERVER, COINIT_APARTMENTTHREADED, COINIT_DISABLE_OLE1DDE,
    COINIT_MULTITHREADED, DVASPECT_CONTENT, FORMATETC, TYMED_HGLOBAL,
};
use windows::Win32::System::DataExchange::{CloseClipboard, GetClipboardData, OpenClipboard};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Ole::{
    IDropTarget, IDropTarget_Impl, OleInitialize, OleUninitialize, RegisterDragDrop,
    ReleaseStgMedium, RevokeDragDrop, CF_HDROP, DROPEFFECT, DROPEFFECT_COPY, DROPEFFECT_LINK,
    DROPEFFECT_NONE,
};
use windows::Win32::System::Registry::*;
use windows::Win32::System::SystemServices::MODIFIERKEYS_FLAGS;
use windows::Win32::System::Threading::CreateMutexW;
use windows::Win32::UI::HiDpi::{
    SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    EnableWindow, GetKeyState, IsWindowEnabled, ReleaseCapture, SetCapture, SetFocus,
    TrackMouseEvent, TME_LEAVE, TRACKMOUSEEVENT, VK_CONTROL, VK_MENU, VK_SHIFT,
};
use windows::Win32::UI::Shell::*;
use windows::Win32::UI::WindowsAndMessaging::*;

const FENCE_CLASS: PCWSTR = w!("LumaDeskFenceV1");
const CONTROLLER_CLASS: PCWSTR = w!("LumaDeskControllerV1");
const LAUNCHER_CLASS: PCWSTR = w!("LumaDeskLauncherV1");
const PREVIEW_CLASS: PCWSTR = w!("LumaDeskOrganizePreviewV1");
const CMD_SHOW_DESKTOP: usize = 1016;
const CMD_COLLECT_DESKTOP: usize = 1017;
const CMD_NEW: usize = 1001;
const CMD_COLLAPSE: usize = 1002;
const CMD_THEME: usize = 1003;
const CMD_REMOVE_ITEM: usize = 1004;
const CMD_REMOVE_FENCE: usize = 1005;
const CMD_OPEN_SETTINGS: usize = 1006;
const CMD_EXIT: usize = 1007;
const CMD_RENAME: usize = 1008;
const CMD_RESET_POSITIONS: usize = 1009;
const CMD_SHOW_ALL: usize = 1010;
const CMD_AUTOSTART: usize = 1011;
const CMD_FIND: usize = 1012;
const CMD_MOVE_UP: usize = 1013;
const CMD_MOVE_DOWN: usize = 1014;
const CMD_PIN_CLIPBOARD: usize = 1015;
const WM_TRAY: u32 = WM_APP + 1;
const WM_WORK: u32 = WM_APP + 2;
const WM_DESKTOP_CHANGED: u32 = WM_APP + 3;
/// The blurred wallpaper cache was rebuilt.
const WM_BACKDROP_READY: u32 = WM_APP + 4;
const AUTOFILE_TIMER: usize = 4;
const CMD_UNDO: usize = 1018;
const CMD_DESKTOP: usize = 1019;
const CMD_HISTORY: usize = 1020;
const CMD_SELECT_ALL: usize = 1021;
const CMD_AUTO_PREVIEW: usize = 1022;
const CMD_AUTO_APPLY: usize = 1023;
const CMD_ARRANGE: usize = 1024;
const CMD_VIEW_ICONS: usize = 1025;
const CMD_VIEW_SMALL: usize = 1029;
const CMD_VIEW_LIST: usize = 1030;
const CMD_VIEW_DETAILS: usize = 1031;
/// 1040..=1044: Manual, Name, Modified, Type, Size; 1045 toggles descending.
const CMD_SORT_BASE: usize = 1040;
const CMD_SORT_DESC: usize = 1045;
const SORT_KEYS: [SortKey; 5] = [
    SortKey::Manual,
    SortKey::Name,
    SortKey::Modified,
    SortKey::Type,
    SortKey::Size,
];
const VIEWS: [(usize, ViewMode); 4] = [
    (CMD_VIEW_ICONS, ViewMode::Icons),
    (CMD_VIEW_SMALL, ViewMode::SmallIcons),
    (CMD_VIEW_LIST, ViewMode::List),
    (CMD_VIEW_DETAILS, ViewMode::Details),
];
const CMD_DELETE_FILES: usize = 1026;
const CMD_DELETE_PERMANENT: usize = 1027;
const CMD_AUTO_FILE: usize = 1028;
const CMD_TRANSFER_BASE: usize = 2000;
// WM_MOUSELEAVE lives in the optional Win32_UI_Controls module in windows-rs.
const WM_MOUSELEAVE: u32 = 0x02A3;
const TRAY_ID: u32 = 1;
const ANIMATION_TIMER: usize = 2;
const ICON_TIMER: usize = 3;
const ANIMATION_DURATION: Duration = Duration::from_millis(170);
const RUN_KEY: PCWSTR = w!("Software\\Microsoft\\Windows\\CurrentVersion\\Run");
const RUN_VALUE: PCWSTR = w!("LumaDesk");

// Win32 MAKEINTRESOURCE deliberately encodes resource ID 1 in a low pointer.
#[allow(clippy::manual_dangling_ptr)]
unsafe fn app_icon() -> HICON {
    GetModuleHandleW(None)
        .ok()
        .and_then(|module| LoadIconW(Some(HINSTANCE(module.0)), PCWSTR(1 as *const u16)).ok())
        .or_else(|| LoadIconW(None, IDI_APPLICATION).ok())
        .unwrap_or_default()
}

struct InstanceGuard(HANDLE);

impl Drop for InstanceGuard {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseHandle(self.0);
        }
    }
}

struct OleGuard;

impl Drop for OleGuard {
    fn drop(&mut self) {
        unsafe { OleUninitialize() };
    }
}

// Explorer uses OLE drag-and-drop for files and desktop shortcuts. Accepting
// WM_DROPFILES alone works for synthetic tests but misses many actual drags.
#[implement(IDropTarget)]
struct FenceDropTarget {
    hwnd: HWND,
    accepts_files: Cell<bool>,
}

impl IDropTarget_Impl for FenceDropTarget_Impl {
    fn DragEnter(
        &self,
        data: windows::core::Ref<'_, IDataObject>,
        _keys: MODIFIERKEYS_FLAGS,
        _pt: &POINTL,
        effect: *mut DROPEFFECT,
    ) -> windows::core::Result<()> {
        let supported = unsafe {
            effect
                .as_ref()
                .is_some_and(|e| pin_drop_effect(*e, true) != DROPEFFECT_NONE)
        };
        let accepts = supported
            && data
                .as_ref()
                .is_some_and(|data| unsafe { data.QueryGetData(&file_drop_format()).is_ok() });
        self.accepts_files.set(accepts);
        unsafe { select_file_effect(effect, accepts) };
        unsafe { set_drop_hover(self.hwnd, !effect.is_null() && *effect != DROPEFFECT_NONE) };
        Ok(())
    }

    fn DragOver(
        &self,
        _keys: MODIFIERKEYS_FLAGS,
        _pt: &POINTL,
        effect: *mut DROPEFFECT,
    ) -> windows::core::Result<()> {
        unsafe { select_file_effect(effect, self.accepts_files.get()) };
        unsafe { set_drop_hover(self.hwnd, !effect.is_null() && *effect != DROPEFFECT_NONE) };
        Ok(())
    }

    fn DragLeave(&self) -> windows::core::Result<()> {
        self.accepts_files.set(false);
        unsafe { set_drop_hover(self.hwnd, false) };
        Ok(())
    }

    fn Drop(
        &self,
        data: windows::core::Ref<'_, IDataObject>,
        _keys: MODIFIERKEYS_FLAGS,
        _pt: &POINTL,
        effect: *mut DROPEFFECT,
    ) -> windows::core::Result<()> {
        self.accepts_files.set(false);
        unsafe { set_drop_hover(self.hwnd, false) };
        if effect.is_null() || unsafe { pin_drop_effect(*effect, true) } == DROPEFFECT_NONE {
            if !effect.is_null() {
                unsafe { *effect = DROPEFFECT_NONE };
            }
            return Ok(());
        }
        // Validate the object supplied to Drop rather than relying on a prior
        // DragEnter. A failed read must never report a successful operation.
        let accepted = data
            .as_ref()
            .is_some_and(|data| unsafe { data.QueryGetData(&file_drop_format()).is_ok() });
        let paths = data
            .as_ref()
            .and_then(|data| unsafe { drop_paths(data).ok() });
        debug_log(&format!(
            "drop accepted={accepted} count={:?} incoming_effect={:?} live={}",
            paths.as_ref().map(Vec::len),
            unsafe { effect.as_ref() },
            unsafe { IsWindow(Some(self.hwnd)).as_bool() }
        ));
        unsafe { select_file_effect(effect, accepted) };
        if !accepted || effect.is_null() || unsafe { *effect } == DROPEFFECT_NONE {
            return Ok(());
        }
        if let Some(paths) = paths {
            let added = unsafe { pin_paths(self.hwnd, paths) };
            debug_log(&format!("drop added={added}"));
            if added == 0 {
                unsafe { *effect = DROPEFFECT_NONE };
            }
        } else {
            unsafe { *effect = DROPEFFECT_NONE };
        }
        Ok(())
    }
}

fn file_drop_format() -> FORMATETC {
    FORMATETC {
        cfFormat: CF_HDROP.0,
        ptd: std::ptr::null_mut(),
        dwAspect: DVASPECT_CONTENT.0,
        lindex: -1,
        tymed: TYMED_HGLOBAL.0 as u32,
    }
}

unsafe fn drop_paths(data: &IDataObject) -> windows::core::Result<Vec<PathBuf>> {
    let mut medium = data.GetData(&file_drop_format())?;
    let paths = paths_from_drop(HDROP(medium.u.hGlobal.0));
    ReleaseStgMedium(&mut medium);
    Ok(paths)
}

fn all_desktop_items(paths: &[PathBuf]) -> bool {
    !paths.is_empty()
        && crate::collection::desktop_paths().is_ok_and(|desktops| {
            paths
                .iter()
                .all(|path| crate::collection::is_desktop_item(path, &desktops))
        })
}

unsafe fn select_file_effect(effect: *mut DROPEFFECT, accepts: bool) {
    if effect.is_null() {
        return;
    }
    *effect = pin_drop_effect(*effect, accepts);
}

fn pin_drop_effect(allowed: DROPEFFECT, accepts: bool) -> DROPEFFECT {
    if accepts && allowed.0 & DROPEFFECT_LINK.0 != 0 {
        DROPEFFECT_LINK
    } else if accepts && allowed.0 & DROPEFFECT_COPY.0 != 0 {
        DROPEFFECT_COPY
    } else {
        DROPEFFECT_NONE
    }
}

unsafe fn set_drop_hover(hwnd: HWND, hover: bool) {
    let ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut FenceWindow;
    if !ptr.is_null() && (*ptr).drop_hover != hover {
        (*ptr).drop_hover = hover;
        let _ = InvalidateRect(Some(hwnd), None, false);
    }
}

/// A collection id with its desktop-client position.
type Placement = (u64, i32, i32);

struct Manager {
    layout: Layout,
    path: PathBuf,
    handles: HashMap<u64, HWND>,
    controller: Option<HWND>,
    desktop_parent: Option<HWND>,
    launcher: Option<HWND>,
    preview: Option<HWND>,
    organize_plan: Option<crate::organize::Plan>,
    icons: Option<Result<crate::desktop_icons::DesktopIcons, String>>,
    icon_syncing: bool,
    shell_pause_depth: u32,
    sender: Option<Sender<Job>>,
    events: Option<Receiver<WorkEvent>>,
    worker: Option<std::thread::JoinHandle<()>>,
    queued: usize,
    status: String,
    /// Positions before the last rearrangement (time, [(id, x, y)]).
    arranged: Option<(u64, Vec<Placement>)>,
    /// Detects new desktop items for automatic filing.
    filer: Option<crate::autofile::AutoFiler>,
}

struct Job {
    owner: usize,
    command: Command,
}
enum WorkEvent {
    Progress(usize, usize),
    Done(usize, Result<Outcome, String>),
}

impl Manager {
    fn persist(&self) {
        if let Err(err) = crate::operations::save_visual(&self.layout, &self.path) {
            eprintln!("Cannot save layout {}: {err}", self.path.display());
        }
    }
}

thread_local! {
    // The Win32 message pump guarantees all access is on this GUI thread.
    static MANAGER: RefCell<Option<Manager>> = const { RefCell::new(None) };
    static TASKBAR_CREATED: Cell<u32> = const { Cell::new(0) };
    static SHOW_LAUNCHER_MESSAGE: Cell<u32> = const { Cell::new(0) };
}

#[derive(Clone, Copy)]
enum DragMode {
    None,
    Move {
        cursor: POINT,
        x: i32,
        y: i32,
    },
    Resize {
        cursor: POINT,
        width: i32,
        height: i32,
        horizontal: bool,
        vertical: bool,
    },
    /// Dragging a details column divider.
    Column {
        index: usize,
        start_x: i32,
        start_width: i32,
    },
    Reorder {
        source_index: usize,
        start: POINT,
        active: bool,
    },
}

struct HeightAnimation {
    from: i32,
    to: i32,
    started: Instant,
}

struct FenceWindow {
    fence: Fence,
    items: Vec<ItemDisplay>,
    visible_indices: Vec<usize>,
    search_active: bool,
    search_text: String,
    theme: Theme,
    renderer: Option<Renderer>,
    hover: Option<usize>,
    selected: Option<usize>,
    selection: HashSet<String>,
    anchor: Option<String>,
    drag_text: String,
    collapse_on_release: bool,
    scroll: usize,
    drag: DragMode,
    render_failure_reported: bool,
    rename_edit: Option<HWND>,
    animation: Option<HeightAnimation>,
    drop_target: Option<IDropTarget>,
    drop_hover: bool,
    /// Explorer-style type-to-select prefix and the time of its last key.
    typeahead: String,
    typeahead_at: Option<Instant>,
    /// Edges snapped during the current move/resize (drawn as guide lines).
    snap: Snapped,
    /// Entry whose file name is being edited (None: the group title).
    rename_item: Option<String>,
    /// The pointer is over the group: show count and header buttons.
    pointer_inside: bool,
}

impl FenceWindow {
    fn rebuild_visible(&mut self) {
        let query = self.search_text.trim().to_lowercase();
        self.visible_indices = self
            .items
            .iter()
            .enumerate()
            .filter(|(_, item)| query.is_empty() || item.matches_query(&query))
            .map(|(index, _)| index)
            .collect();
        let keys: Vec<SortValue> = self
            .fence
            .paths
            .iter()
            .zip(&self.items)
            .map(|(path, item)| sort_value(self.fence.sort, path, item))
            .collect();
        sort_visible(
            &mut self.visible_indices,
            &keys,
            self.fence.sort,
            self.fence.sort_desc,
        );
        self.scroll = self.scroll.min(self.max_scroll());
    }

    /// File details are still loading for a view or order that needs them.
    fn waiting_for_details(&self) -> bool {
        (self.fence.mode() == ViewMode::Details
            || !matches!(self.fence.sort, SortKey::Manual | SortKey::Name))
            && self
                .fence
                .paths
                .iter()
                .any(|p| crate::meta::get(p).is_none())
    }

    fn max_scroll(&self) -> usize {
        if self.search_text.trim().is_empty() {
            self.fence.max_scroll()
        } else {
            self.fence.max_scroll_for(self.visible_indices.len())
        }
    }
}

pub unsafe fn run() -> Result<(), Box<dyn std::error::Error>> {
    let background = std::env::args_os().skip(1).any(|arg| arg == "--background");
    let activate_message = RegisterWindowMessageW(w!("LumaDesk.ShowLauncherV1"));
    SHOW_LAUNCHER_MESSAGE.with(|message| message.set(activate_message));
    let instance_guard = InstanceGuard(CreateMutexW(
        None,
        false,
        w!("Local\\LumaDeskSingleInstanceV1"),
    )?);
    if GetLastError() == ERROR_ALREADY_EXISTS {
        if background {
            return Ok(());
        }
        // A second launch is a request to open the running application. The
        // first process may still be creating its hidden controller.
        for _ in 0..50 {
            if let Ok(controller) = FindWindowW(CONTROLLER_CLASS, None) {
                let mut owner = 0;
                GetWindowThreadProcessId(controller, Some(&mut owner));
                let _ = AllowSetForegroundWindow(owner);
                PostMessageW(Some(controller), activate_message, WPARAM(0), LPARAM(0))?;
                return Ok(());
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        return Err(i18n::t(K::AlreadyStarting).into());
    }
    OleInitialize(None)?;
    let _ole_guard = OleGuard;
    let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
    let instance = GetModuleHandleW(None)?;
    let path = config_path();
    let layout = crate::operations::initialize(&path)?;
    i18n::set(
        layout
            .language
            .as_deref()
            .and_then(crate::i18n::Lang::from_code),
    );
    let manager = Manager {
        layout,
        path,
        handles: HashMap::new(),
        controller: None,
        desktop_parent: None,
        launcher: None,
        preview: None,
        organize_plan: None,
        icons: None,
        icon_syncing: false,
        shell_pause_depth: 0,
        sender: None,
        events: None,
        worker: None,
        queued: 0,
        status: String::new(),
        arranged: None,
        filer: None,
    };
    manager.persist();
    MANAGER.with(|m| *m.borrow_mut() = Some(manager));
    TASKBAR_CREATED.with(|message| message.set(RegisterWindowMessageW(w!("TaskbarCreated"))));

    let cursor = LoadCursorW(None, IDC_ARROW)?;
    let fence_class = WNDCLASSW {
        hInstance: HINSTANCE(instance.0),
        lpszClassName: FENCE_CLASS,
        lpfnWndProc: Some(fence_proc),
        hCursor: cursor,
        hIcon: app_icon(),
        style: CS_HREDRAW | CS_VREDRAW | CS_DBLCLKS,
        ..Default::default()
    };
    if RegisterClassW(&fence_class) == 0 {
        return Err(Box::new(windows::core::Error::from_win32()));
    }
    let controller_class = WNDCLASSW {
        hInstance: HINSTANCE(instance.0),
        lpszClassName: CONTROLLER_CLASS,
        lpfnWndProc: Some(controller_proc),
        hIcon: app_icon(),
        ..Default::default()
    };
    if RegisterClassW(&controller_class) == 0 {
        return Err(Box::new(windows::core::Error::from_win32()));
    }
    let launcher_class = WNDCLASSW {
        hInstance: HINSTANCE(instance.0),
        lpszClassName: LAUNCHER_CLASS,
        lpfnWndProc: Some(launcher_proc),
        hCursor: cursor,
        hIcon: app_icon(),
        hbrBackground: GetSysColorBrush(COLOR_WINDOW),
        ..Default::default()
    };
    if RegisterClassW(&launcher_class) == 0 {
        return Err(Box::new(windows::core::Error::from_win32()));
    }
    let preview_class = WNDCLASSW {
        lpszClassName: PREVIEW_CLASS,
        lpfnWndProc: Some(preview_proc),
        ..launcher_class
    };
    if RegisterClassW(&preview_class) == 0 {
        return Err(Box::new(windows::core::Error::from_win32()));
    }
    // A hidden top-level controller persists when Explorer restarts.
    let controller = CreateWindowExW(
        WINDOW_EX_STYLE(0),
        CONTROLLER_CLASS,
        w!("LumaDesk"),
        WS_OVERLAPPED,
        0,
        0,
        0,
        0,
        None,
        None,
        Some(HINSTANCE(instance.0)),
        None,
    )?;
    MANAGER.with(|m| {
        if let Some(manager) = m.borrow_mut().as_mut() {
            manager.controller = Some(controller);
        }
    });
    start_worker(controller);
    crate::backdrop::refresh(controller, WM_BACKDROP_READY);
    // The first Shell context menu loads every installed shell extension
    // (archivers, cloud drives, ...) and can take seconds. Load them once in
    // the background so the first right-click on an item opens promptly.
    std::thread::spawn(|| {
        std::thread::sleep(Duration::from_secs(5));
        let initialized = CoInitializeEx(None, COINIT_APARTMENTTHREADED | COINIT_DISABLE_OLE1DDE);
        if let Ok(exe) = std::env::current_exe() {
            if let Some(context) = shell_context_menu(&[exe], HWND::default()) {
                if let Ok(menu) = CreatePopupMenu() {
                    let _ = context.QueryContextMenu(menu, 0, SHELL_FIRST, SHELL_LAST, CMF_NORMAL);
                    let _ = DestroyMenu(menu);
                }
            }
        }
        if initialized.is_ok() {
            CoUninitialize();
        }
    });
    if let Ok(desktops) = crate::collection::desktop_paths() {
        let filer = crate::autofile::AutoFiler::new(&desktops);
        MANAGER.with(|m| {
            if let Some(manager) = m.borrow_mut().as_mut() {
                manager.filer = Some(filer);
            }
        });
        crate::autofile::watch(desktops, controller, WM_DESKTOP_CHANGED);
    }
    add_tray_icon(controller);
    ensure_fences();
    if !background {
        show_launcher(controller);
    }
    sync_desktop_icons();
    let _ = SetTimer(Some(controller), 1, 3000, None);
    let mut msg = MSG::default();
    while GetMessageW(&mut msg, None, 0, 0).0 > 0 {
        let preview = MANAGER.with(|m| m.borrow().as_ref().and_then(|m| m.preview));
        if let Some(preview) = preview {
            if GetAncestor(msg.hwnd, GA_ROOT) == preview
                && IsDialogMessageW(preview, &msg).as_bool()
            {
                continue;
            }
        }
        let launcher = MANAGER.with(|m| m.borrow().as_ref().and_then(|manager| manager.launcher));
        if let Some(launcher) = launcher {
            if GetAncestor(msg.hwnd, GA_ROOT) == launcher
                && msg.message == WM_KEYDOWN
                && msg.wParam.0 == b'Z' as usize
                && GetKeyState(VK_CONTROL.0 as i32) < 0
            {
                request_undo(launcher);
                continue;
            }
        }
        // Edit controls get keyboard messages directly. Handle Enter/Escape
        // before dispatch so renaming works without a custom subclass.
        if msg.message == WM_KEYDOWN && (msg.wParam.0 == 0x0D || msg.wParam.0 == 0x1B) {
            if let Ok(parent) = GetParent(msg.hwnd) {
                let ptr = GetWindowLongPtrW(parent, GWLP_USERDATA) as *mut FenceWindow;
                if !ptr.is_null() && (*ptr).rename_edit == Some(msg.hwnd) {
                    finish_rename(parent, ptr, msg.wParam.0 == 0x0D);
                    continue;
                }
            }
        }
        let _ = TranslateMessage(&msg);
        DispatchMessageW(&msg);
    }
    let _ = KillTimer(Some(controller), 1);
    let worker = MANAGER.with(|m| {
        let mut guard = m.borrow_mut();
        let manager = guard.as_mut().expect("initialized");
        manager.sender.take();
        manager.worker.take()
    });
    if let Some(worker) = worker {
        // Explorer may synchronously ask our desktop child windows to paint
        // while the worker restores icon positions. Keep servicing those
        // messages until COM and the serialized queue have drained.
        while !worker.is_finished() {
            while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
                if !matches!(msg.message, WM_QUIT | WM_WORK | WM_TIMER) {
                    let _ = TranslateMessage(&msg);
                    DispatchMessageW(&msg);
                }
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        let _ = worker.join();
    }
    // Revoke registered drop targets while OLE is still initialized. The
    // Explorer-owned parent outlives our hidden controller.
    let manager = MANAGER.with(|m| m.borrow_mut().take());
    let handles = manager
        .map(|mut manager| {
            drop(manager.icons.take());
            manager
                .handles
                .into_values()
                .chain(manager.launcher)
                .chain(manager.preview)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    for hwnd in handles {
        if IsWindow(Some(hwnd)).as_bool() {
            let _ = DestroyWindow(hwnd);
        }
    }
    drop(instance_guard);
    Ok(())
}

unsafe fn shell_desktop() -> Option<HWND> {
    // Explorer's shell window is stable even when other Explorer windows and
    // virtual-desktop hosts also contain SHELLDLL_DefView controls.
    let shell = GetShellWindow();
    if let Ok(view) = FindWindowExW(Some(shell), None, w!("SHELLDLL_DefView"), None) {
        if IsWindowVisible(view).as_bool() {
            return Some(view);
        }
    }
    unsafe extern "system" fn search(hwnd: HWND, lp: LPARAM) -> BOOL {
        let mut class = [0u16; 32];
        let len = GetClassNameW(hwnd, &mut class);
        if len == 0 || String::from_utf16_lossy(&class[..len as usize]) != "WorkerW" {
            return BOOL(1);
        }
        if let Ok(defview) = FindWindowExW(Some(hwnd), None, w!("SHELLDLL_DefView"), None) {
            if IsWindowVisible(defview).as_bool() {
                unsafe {
                    *(lp.0 as *mut Option<HWND>) = Some(defview);
                }
                return BOOL(0);
            }
        }
        BOOL(1)
    }
    let mut answer: Option<HWND> = None;
    let _ = EnumWindows(
        Some(search),
        LPARAM((&mut answer as *mut Option<HWND>) as isize),
    );
    answer
}

#[derive(Clone, Copy)]
struct MonitorRegion {
    work: RECT,
    primary: bool,
}

unsafe fn monitor_regions() -> Vec<MonitorRegion> {
    unsafe extern "system" fn collect(
        monitor: HMONITOR,
        _dc: HDC,
        _clip: *mut RECT,
        lp: LPARAM,
    ) -> BOOL {
        let mut info = MONITORINFO {
            cbSize: std::mem::size_of::<MONITORINFO>() as u32,
            ..Default::default()
        };
        if GetMonitorInfoW(monitor, &mut info).as_bool() {
            // MONITORINFOF_PRIMARY is bit 0 of dwFlags.
            let region = MonitorRegion {
                work: info.rcWork,
                primary: info.dwFlags & 1 != 0,
            };
            unsafe { (*(lp.0 as *mut Vec<MonitorRegion>)).push(region) };
        }
        BOOL(1)
    }
    let mut regions = Vec::new();
    let _ = EnumDisplayMonitors(
        None,
        None,
        Some(collect),
        LPARAM((&mut regions as *mut Vec<MonitorRegion>) as isize),
    );
    regions
}

unsafe fn desktop_origin(parent: HWND) -> POINT {
    let mut origin = POINT { x: 0, y: 0 };
    let _ = ClientToScreen(parent, &mut origin);
    origin
}

fn header_reachable(fence: &Fence, origin: POINT, regions: &[MonitorRegion]) -> bool {
    let left = origin.x.saturating_add(fence.x);
    let top = origin.y.saturating_add(fence.y);
    let right = left.saturating_add(fence.width);
    let bottom = top.saturating_add(HEADER_HEIGHT);
    regions.iter().any(|monitor| {
        let visible_width = right
            .min(monitor.work.right)
            .saturating_sub(left.max(monitor.work.left))
            .max(0);
        let visible_height = bottom
            .min(monitor.work.bottom)
            .saturating_sub(top.max(monitor.work.top))
            .max(0);
        // Most of the (slim) header must be visible and grabbable.
        visible_width >= 100 && visible_height >= HEADER_HEIGHT - 12
    })
}

/// A monitor work area in desktop client (layout) coordinates.
fn client_area(region: MonitorRegion, origin: POINT) -> Area {
    Area {
        left: region.work.left.saturating_sub(origin.x),
        top: region.work.top.saturating_sub(origin.y),
        right: region.work.right.saturating_sub(origin.x),
        bottom: region.work.bottom.saturating_sub(origin.y),
    }
}

/// Monitor showing a collection's header centre.
fn region_of(fence: &Fence, origin: POINT, regions: &[MonitorRegion]) -> Option<MonitorRegion> {
    let x = origin.x + fence.x + fence.width / 2;
    let y = origin.y + fence.y + HEADER_HEIGHT / 2;
    regions
        .iter()
        .find(|r| x >= r.work.left && x < r.work.right && y >= r.work.top && y < r.work.bottom)
        .copied()
}

/// Positions for every collection: each monitor's collections are tiled in
/// its work area (or all on `primary`), ordered by their current position.
fn arranged_positions(
    fences: &[Fence],
    origin: POINT,
    regions: &[MonitorRegion],
    primary: MonitorRegion,
    primary_only: bool,
) -> Vec<(u64, i32, i32)> {
    let mut groups: Vec<(MonitorRegion, Vec<&Fence>)> = Vec::new();
    for fence in fences {
        let region = if primary_only {
            primary
        } else {
            region_of(fence, origin, regions).unwrap_or(primary)
        };
        match groups.iter_mut().find(|(r, _)| r.work == region.work) {
            Some((_, members)) => members.push(fence),
            None => groups.push((region, vec![fence])),
        }
    }
    let mut placed = Vec::new();
    for (region, mut members) in groups {
        members.sort_by_key(|f| (f.y, f.x));
        let sizes: Vec<_> = members.iter().map(|f| (f.width, f.height)).collect();
        let positions = crate::model::tile_positions(&sizes, client_area(region, origin));
        placed.extend(
            members
                .iter()
                .zip(positions)
                .map(|(f, (x, y))| (f.id, x, y)),
        );
    }
    placed
}

unsafe fn apply_fence_positions(positions: Vec<(HWND, i32, i32)>) {
    for (hwnd, x, y) in positions {
        let state = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut FenceWindow;
        if state.is_null() {
            continue;
        }
        (*state).fence.x = x;
        (*state).fence.y = y;
        let _ = SetWindowPos(
            hwnd,
            None,
            x,
            y,
            0,
            0,
            SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE,
        );
    }
}

unsafe fn recover_offscreen_fences(parent: HWND) {
    let regions = monitor_regions();
    let Some(primary) = regions
        .iter()
        .find(|m| m.primary)
        .or_else(|| regions.first())
        .copied()
    else {
        return;
    };
    let origin = desktop_origin(parent);
    let placements = MANAGER.with(|m| {
        let mut guard = m.borrow_mut();
        let Some(manager) = guard.as_mut() else {
            return vec![];
        };
        let mut placements = Vec::new();
        let mut changed = false;
        // Recovered collections go to free space; visible ones never move.
        let mut occupied: Vec<Fence> = manager
            .layout
            .fences
            .iter()
            .filter(|f| header_reachable(f, origin, &regions))
            .cloned()
            .collect();
        let area = client_area(primary, origin);
        for fence in manager.layout.fences.iter_mut() {
            if header_reachable(fence, origin, &regions) {
                continue;
            }
            let (x, y) = free_position(&occupied, fence.width, fence.height, Some(area));
            fence.x = x;
            fence.y = y;
            occupied.push(fence.clone());
            changed = true;
            if let Some(hwnd) = manager.handles.get(&fence.id) {
                placements.push((*hwnd, x, y));
            }
        }
        if changed {
            manager.persist();
        }
        placements
    });
    apply_fence_positions(placements);
}

// A previous handle can go stale when Explorer recreates its view. Reconcile
// against actual HWNDs so repeated timer ticks never create duplicate fences.
unsafe fn existing_fences(parent: HWND) -> Vec<(u64, HWND)> {
    unsafe extern "system" fn collect(hwnd: HWND, lp: LPARAM) -> BOOL {
        let mut pid = 0;
        GetWindowThreadProcessId(hwnd, Some(&mut pid));
        if pid != std::process::id() {
            return BOOL(1);
        }
        let mut class = [0u16; 40];
        let len = GetClassNameW(hwnd, &mut class);
        if len <= 0 || String::from_utf16_lossy(&class[..len as usize]) != "LumaDeskFenceV1" {
            return BOOL(1);
        }
        let state = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut FenceWindow;
        if !state.is_null() {
            unsafe { (*(lp.0 as *mut Vec<(u64, HWND)>)).push(((*state).fence.id, hwnd)) };
        }
        BOOL(1)
    }
    let mut windows = Vec::new();
    let _ = EnumChildWindows(
        Some(parent),
        Some(collect),
        LPARAM((&mut windows as *mut Vec<(u64, HWND)>) as isize),
    );
    windows
}

unsafe fn ensure_fences() {
    let Some(parent) = shell_desktop() else {
        return;
    };
    if !desktop_accepts_input(parent) {
        suspend_desktop_groups();
        return;
    }
    // When Explorer reconstructs its desktop window, its coordinate origin
    // may change. Recover positions before creating replacement child HWNDs.
    let parent_changed = MANAGER.with(|m| {
        let mut guard = m.borrow_mut();
        let Some(manager) = guard.as_mut() else {
            return false;
        };
        let changed = manager.desktop_parent != Some(parent);
        manager.desktop_parent = Some(parent);
        changed
    });
    if parent_changed {
        recover_offscreen_fences(parent);
    }
    let observed = existing_fences(parent);
    let (discard, missing) = MANAGER.with(|m| {
        let mut guard = m.borrow_mut();
        let Some(manager) = guard.as_mut() else {
            return (vec![], vec![]);
        };
        let previous = std::mem::take(&mut manager.handles);
        let mut candidates: HashMap<u64, Vec<HWND>> = HashMap::new();
        for (id, hwnd) in observed {
            candidates.entry(id).or_default().push(hwnd);
        }
        let mut discard = Vec::new();
        let mut missing = Vec::new();
        for fence in &manager.layout.fences {
            let mut windows = candidates.remove(&fence.id).unwrap_or_default();
            let preferred = previous
                .get(&fence.id)
                .copied()
                .filter(|hwnd| windows.contains(hwnd));
            let selected = preferred.or_else(|| windows.first().copied());
            if let Some(hwnd) = selected {
                manager.handles.insert(fence.id, hwnd);
                windows.retain(|other| *other != hwnd);
            } else {
                missing.push((fence.clone(), manager.layout.theme));
            }
            discard.extend(windows);
        }
        discard.extend(candidates.into_values().flatten());
        for (_, hwnd) in previous {
            if !manager.handles.values().any(|current| *current == hwnd)
                && IsWindow(Some(hwnd)).as_bool()
            {
                discard.push(hwnd);
            }
        }
        (discard, missing)
    });
    for hwnd in discard {
        let _ = DestroyWindow(hwnd);
    }
    for (fence, theme) in missing {
        match create_fence(parent, fence.clone(), theme) {
            Ok(hwnd) => MANAGER.with(|m| {
                if let Some(mgr) = m.borrow_mut().as_mut() {
                    mgr.handles.insert(fence.id, hwnd);
                }
            }),
            Err(e) => eprintln!("Could not create collection {}: {e}", fence.id),
        }
    }
    keep_fences_above_icons(parent);
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Layer {
    Group,
    Icons,
    Other,
}

/// Explorer's icon list answers hit tests across the whole desktop, so a
/// collection beneath it paints but never receives mouse input.
fn groups_need_raise(order: &[Layer]) -> bool {
    order
        .iter()
        .position(|layer| *layer == Layer::Icons)
        .is_some_and(|icons| order[icons + 1..].contains(&Layer::Group))
}

/// New child windows are placed under SysListView32, and Explorer may raise
/// it again later. Only the desktop's children are reordered; normal
/// application windows and focus are unaffected.
unsafe fn keep_fences_above_icons(parent: HWND) {
    let ours = MANAGER.with(|m| {
        m.borrow()
            .as_ref()
            .map(|m| m.handles.values().copied().collect::<Vec<_>>())
            .unwrap_or_default()
    });
    if ours.is_empty() {
        return;
    }
    let mut order = Vec::new();
    let mut groups = Vec::new();
    let mut child = GetWindow(parent, GW_CHILD).ok();
    while let Some(hwnd) = child {
        if ours.contains(&hwnd) {
            order.push(Layer::Group);
            groups.push(hwnd);
        } else if class_name(hwnd) == "SysListView32" {
            order.push(Layer::Icons);
        } else {
            order.push(Layer::Other);
        }
        child = GetWindow(hwnd, GW_HWNDNEXT).ok();
    }
    if !groups_need_raise(&order) {
        return;
    }
    // Raise the bottom-most first so the stacking among collections is kept.
    for hwnd in groups.into_iter().rev() {
        let _ = SetWindowPos(
            hwnd,
            Some(HWND_TOP),
            0,
            0,
            0,
            0,
            SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE | SWP_NOOWNERZORDER,
        );
    }
}

unsafe fn create_fence(parent: HWND, fence: Fence, theme: Theme) -> windows::core::Result<HWND> {
    // WM_NCCREATE takes ownership. If CreateWindowExW fails earlier, `pending`
    // still owns the allocation, avoiding a double free on WM_NCDESTROY.
    let items: Vec<ItemDisplay> = fence
        .paths
        .iter()
        .map(|path| ItemDisplay::from_path(path))
        .collect();
    let mut pending = Some(Box::new(FenceWindow {
        fence: fence.clone(),
        visible_indices: (0..items.len()).collect(),
        items,
        search_active: false,
        search_text: String::new(),
        theme,
        renderer: None,
        hover: None,
        selected: None,
        selection: HashSet::new(),
        anchor: None,
        drag_text: String::new(),
        collapse_on_release: false,
        scroll: 0,
        drag: DragMode::None,
        render_failure_reported: false,
        rename_edit: None,
        animation: None,
        drop_target: None,
        drop_hover: false,
        typeahead: String::new(),
        typeahead_at: None,
        snap: Snapped::default(),
        rename_item: None,
        pointer_inside: false,
    }));
    let instance = GetModuleHandleW(None)?;
    let title: Vec<u16> = fence.title.encode_utf16().chain(Some(0)).collect();
    CreateWindowExW(
        WS_EX_ACCEPTFILES | WS_EX_NOPARENTNOTIFY,
        FENCE_CLASS,
        PCWSTR(title.as_ptr()),
        WS_CHILD | WS_VISIBLE | WS_CLIPCHILDREN,
        fence.x,
        fence.y,
        fence.width,
        fence.window_height(),
        Some(parent),
        None,
        Some(HINSTANCE(instance.0)),
        Some((&mut pending as *mut Option<Box<FenceWindow>>).cast::<c_void>()),
    )
}

unsafe fn round_window(hwnd: HWND, width: i32, height: i32) {
    if width > 0 && height > 0 {
        let region = CreateRoundRectRgn(0, 0, width + 1, height + 1, 16, 16);
        if !region.0.is_null() {
            // SetWindowRgn takes ownership of region on success.
            let _ = SetWindowRgn(hwnd, Some(region), true);
        }
    }
}

unsafe fn start_worker(controller: HWND) {
    let (sender, jobs) = mpsc::channel::<Job>();
    let (events, receiver) = mpsc::channel::<WorkEvent>();
    let path = config_path();
    let notify = controller.0 as usize;
    let worker = std::thread::spawn(move || {
        let initialized = CoInitializeEx(None, COINIT_MULTITHREADED)
            .ok()
            .map_err(|e| e.to_string());
        for job in jobs {
            let command = job.command;
            let result = if let Err(error) = &initialized {
                Err(error.clone())
            } else {
                crate::collection::desktop_paths()
                    .map_err(|e| e.to_string())
                    .and_then(|desktops| {
                        crate::operations::execute(&path, command, &desktops, |done, total| {
                            let _ = events.send(WorkEvent::Progress(done, total));
                            let _ = PostMessageW(
                                Some(HWND(notify as *mut c_void)),
                                WM_WORK,
                                WPARAM(0),
                                LPARAM(0),
                            );
                        })
                        .map_err(|e| e.to_string())
                    })
            };
            let _ = events.send(WorkEvent::Done(job.owner, result));
            let _ = PostMessageW(
                Some(HWND(notify as *mut c_void)),
                WM_WORK,
                WPARAM(0),
                LPARAM(0),
            );
        }
        if initialized.is_ok() {
            CoUninitialize();
        }
    });
    MANAGER.with(|m| {
        let mut guard = m.borrow_mut();
        let manager = guard.as_mut().expect("initialized");
        manager.sender = Some(sender);
        manager.events = Some(receiver);
        manager.worker = Some(worker);
    });
}

fn work_busy() -> bool {
    MANAGER.with(|m| m.borrow().as_ref().is_some_and(|m| m.queued > 0))
}

/// Files settled new desktop items into their category groups. Returns true
/// when another scan is needed (items still settling or the worker is busy).
unsafe fn auto_file_new_items() -> bool {
    let Ok(desktops) = crate::collection::desktop_paths() else {
        return false;
    };
    if work_busy() {
        return true;
    }
    let scanned = MANAGER.with(|m| {
        let mut guard = m.borrow_mut();
        let manager = guard.as_mut()?;
        if !manager.layout.auto_file {
            return None;
        }
        let storage = manager.path.parent()?.to_owned();
        let filer = manager.filer.as_mut()?;
        let ready = filer.scan(&desktops, Instant::now());
        Some((ready, filer.waiting(), storage))
    });
    let Some((ready, waiting, storage)) = scanned else {
        return false;
    };
    let items: Vec<_> = ready
        .iter()
        .filter_map(|path| {
            let candidate = crate::organize::inspect(path).ok()?;
            crate::organize::protect_storage(path, &storage).ok()?;
            Some(candidate)
        })
        .collect();
    if !items.is_empty() {
        let plan = crate::organize::Plan {
            items,
            skipped: vec![],
        };
        // A null owner marks a background job: failures are not shown as
        // dialogs (the item simply stays on the desktop).
        enqueue(HWND::default(), Command::AutoOrganize { plan });
    }
    waiting
}

unsafe fn toggle_auto_file() {
    let desktops = crate::collection::desktop_paths().unwrap_or_default();
    MANAGER.with(|m| {
        if let Some(manager) = m.borrow_mut().as_mut() {
            manager.layout.auto_file = !manager.layout.auto_file;
            // Switching on never sweeps up what is already on the desktop.
            if manager.layout.auto_file {
                manager.filer = Some(crate::autofile::AutoFiler::new(&desktops));
            }
            manager.persist();
        }
    });
}

unsafe fn enqueue(owner: HWND, command: Command) -> bool {
    let sent = MANAGER.with(|m| {
        let mut guard = m.borrow_mut();
        let Some(manager) = guard.as_mut() else {
            return false;
        };
        if manager.sender.as_ref().is_some_and(|s| {
            s.send(Job {
                owner: owner.0 as usize,
                command,
            })
            .is_ok()
        }) {
            manager.queued += 1;
            manager.status = i18n::tf(K::StatusQueued, &[&manager.queued]);
            true
        } else {
            false
        }
    });
    invalidate_fences();
    sent
}

unsafe fn invalidate_fences() {
    MANAGER.with(|m| {
        if let Some(manager) = m.borrow().as_ref() {
            if let Some(launcher) = manager.launcher {
                for id in [CMD_NEW, CMD_AUTO_PREVIEW, CMD_UNDO, CMD_COLLECT_DESKTOP] {
                    if let Ok(button) = GetDlgItem(Some(launcher), id as i32) {
                        let _ = EnableWindow(button, manager.queued == 0);
                    }
                }
                // The drawn status pill and busy states follow the manager.
                let _ = InvalidateRect(Some(launcher), None, false);
            }
        }
    });
    let handles = MANAGER.with(|m| {
        m.borrow()
            .as_ref()
            .map(|m| m.handles.values().copied().collect::<Vec<_>>())
            .unwrap_or_default()
    });
    for hwnd in handles {
        let _ = InvalidateRect(Some(hwnd), None, false);
    }
}

unsafe fn receive_work() {
    let events = MANAGER.with(|m| {
        m.borrow()
            .as_ref()
            .and_then(|m| m.events.as_ref())
            .map(|r| r.try_iter().collect::<Vec<_>>())
            .unwrap_or_default()
    });
    for event in events {
        match event {
            WorkEvent::Progress(done, total) => MANAGER.with(|m| {
                if let Some(m) = m.borrow_mut().as_mut() {
                    m.status = i18n::tf(K::StatusProgress, &[&done, &total]);
                }
            }),
            WorkEvent::Done(owner, result) => {
                MANAGER.with(|m| {
                    if let Some(m) = m.borrow_mut().as_mut() {
                        m.queued = m.queued.saturating_sub(1);
                        m.status = if m.queued > 0 {
                            i18n::tf(K::StatusWaiting, &[&m.queued])
                        } else {
                            match &result {
                                Ok(o) => i18n::tf(K::StatusDone, &[&o.completed]),
                                Err(_) => i18n::t(K::StatusFailed).into(),
                            }
                        };
                    }
                });
                if let Ok(outcome) = &result {
                    if let Some(plan) = &outcome.preview {
                        show_organize_preview(plan.clone());
                        continue;
                    }
                }
                refresh_collections_after_move(HWND(owner as *mut c_void));
                sync_desktop_icons();
                let failures = match result {
                    Ok(o) => o.failures,
                    Err(e) => vec![e],
                };
                // Owner 0 is background auto-filing: unfiled items stay on the desktop.
                if !failures.is_empty() && owner != 0 {
                    collection_error(
                        HWND(owner as *mut c_void),
                        &i18n::tf(
                            K::FailuresKept,
                            &[&failures.join(
                                "
",
                            )],
                        ),
                    );
                }
            }
        }
    }
    invalidate_fences();
}

unsafe fn sync_desktop_icons() {
    if !shell_desktop().is_some_and(|parent| desktop_accepts_input(parent)) {
        suspend_desktop_groups();
        return;
    }
    let Some((root, paths, previous)) = MANAGER.with(|m| {
        let mut guard = m.borrow_mut();
        let manager = guard.as_mut()?;
        if manager.icon_syncing {
            return None;
        }
        manager.icon_syncing = true;
        Some((
            manager.path.parent()?.to_owned(),
            manager
                .layout
                .fences
                .iter()
                .flat_map(|f| f.paths.iter().cloned())
                .collect::<Vec<_>>(),
            manager.icons.take(),
        ))
    }) else {
        return;
    };
    // Shell display calls share the GUI's OLE apartment with the desktop child
    // windows. No RefCell/config lock is held across reentrant COM messages.
    let mut icons = previous.unwrap_or_else(|| {
        crate::desktop_icons::DesktopIcons::start(&root).map_err(|e| e.to_string())
    });
    let result = icons
        .as_mut()
        .map_err(|e| e.clone())
        .and_then(|icons| icons.sync(&paths).map_err(|e| e.to_string()));
    MANAGER.with(|m| {
        if let Some(manager) = m.borrow_mut().as_mut() {
            manager.icons = Some(icons);
            manager.icon_syncing = false;
            match result {
                Err(error) => manager.status = i18n::tf(K::IconSyncError, &[&error]),
                Ok(())
                    if manager
                        .status
                        .starts_with(i18n::tf(K::IconSyncError, &[&""]).as_str()) =>
                {
                    manager.status = i18n::t(K::IconsSynced).into()
                }
                Ok(()) => {}
            }
        }
    });
    invalidate_fences();
}

fn selected_ids(w: &FenceWindow) -> Vec<String> {
    w.fence
        .entries
        .iter()
        .filter(|e| w.selection.contains(&e.id))
        .map(|e| e.id.clone())
        .collect()
}

fn select_all(w: &mut FenceWindow) {
    w.selection = w
        .visible_indices
        .iter()
        .filter_map(|&i| w.fence.entries.get(i).map(|e| e.id.clone()))
        .collect();
    w.selected = w.visible_indices.first().copied();
    w.anchor = w
        .selected
        .and_then(|i| w.fence.entries.get(i).map(|e| e.id.clone()));
}

fn select_one(w: &mut FenceWindow, index: usize) {
    w.selection.clear();
    if let Some(entry) = w.fence.entries.get(index) {
        w.selection.insert(entry.id.clone());
        w.anchor = Some(entry.id.clone());
        w.selected = Some(index);
    }
}

enum Destination {
    Invalid,
    Desktop,
    Fence { fence: u64, before: Option<String> },
}

unsafe fn class_name(hwnd: HWND) -> String {
    let mut name = [0u16; 128];
    let len = GetClassNameW(hwnd, &mut name);
    String::from_utf16_lossy(&name[..len.max(0) as usize])
}

unsafe fn drag_destination(source: HWND, lp: LPARAM) -> Destination {
    let (x, y) = point(lp);
    let mut screen = POINT { x, y };
    let _ = ClientToScreen(source, &mut screen);
    let hit = WindowFromPoint(screen);
    if hit.0.is_null() {
        return Destination::Invalid;
    }
    let windows = MANAGER.with(|m| {
        m.borrow()
            .as_ref()
            .map(|m| {
                m.handles
                    .iter()
                    .map(|(&id, &h)| (id, h))
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default()
    });
    // A collection can sit below Explorer's icon list (which then wins
    // WindowFromPoint). When the hit is one of ours or the icon layer itself,
    // test our own rectangles in z-order before treating it as the desktop.
    let desktop = windows.first().and_then(|(_, h)| GetParent(*h).ok());
    let icon_layer = desktop.is_some_and(|desktop| {
        matches!(
            class_name(hit).as_str(),
            "SysListView32" | "SHELLDLL_DefView"
        ) && GetAncestor(hit, GA_ROOT) == GetAncestor(desktop, GA_ROOT)
    });
    let target = if let Some(&(id, hwnd)) = windows.iter().find(|(_, h)| *h == hit) {
        Some((id, hwnd))
    } else if icon_layer {
        let mut stacked = Vec::new();
        let mut child = desktop.and_then(|p| GetWindow(p, GW_CHILD).ok());
        while let Some(hwnd) = child {
            if let Some(&(id, _)) = windows.iter().find(|(_, h)| *h == hwnd) {
                let mut rect = RECT::default();
                if IsWindowVisible(hwnd).as_bool() && GetWindowRect(hwnd, &mut rect).is_ok() {
                    stacked.push((id, rect));
                }
            }
            child = GetWindow(hwnd, GW_HWNDNEXT).ok();
        }
        group_at(screen, &stacked).and_then(|id| windows.iter().find(|(i, _)| *i == id).copied())
    } else {
        None
    };
    if let Some((id, hwnd)) = target {
        let ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut FenceWindow;
        if ptr.is_null() {
            return Destination::Invalid;
        }
        let mut local = screen;
        let _ = windows::Win32::Graphics::Gdi::ScreenToClient(hwnd, &mut local);
        let window = &*ptr;
        let before = item_at(window, local.x, local.y)
            .and_then(|i| window.fence.entries.get(i).map(|e| e.id.clone()));
        return Destination::Fence { fence: id, before };
    }
    // Only Explorer's actual icon view qualifies. Another app's overlay, an arbitrary
    // WorkerW, Progman, or an Explorer folder must never be mistaken for it.
    let view = if class_name(hit) == "SysListView32" {
        GetParent(hit).ok()
    } else if class_name(hit) == "SHELLDLL_DefView" {
        Some(hit)
    } else {
        None
    };
    if let Some(view) = view {
        let mut owner = 0;
        let mut shell_owner = 0;
        GetWindowThreadProcessId(view, Some(&mut owner));
        GetWindowThreadProcessId(GetShellWindow(), Some(&mut shell_owner));
        if class_name(view) == "SHELLDLL_DefView" && owner != 0 && owner == shell_owner {
            return Destination::Desktop;
        }
    }
    Destination::Invalid
}

/// The topmost collection containing `point`; `stacked` is in z-order, top first.
fn group_at(point: POINT, stacked: &[(u64, RECT)]) -> Option<u64> {
    stacked
        .iter()
        .find(|(_, r)| {
            point.x >= r.left && point.x < r.right && point.y >= r.top && point.y < r.bottom
        })
        .map(|(id, _)| *id)
}

unsafe fn clear_drag_feedback() {
    let windows = MANAGER.with(|m| {
        m.borrow()
            .as_ref()
            .map(|m| m.handles.values().copied().collect::<Vec<_>>())
            .unwrap_or_default()
    });
    for hwnd in windows {
        let ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut FenceWindow;
        if !ptr.is_null() {
            (*ptr).drag_text.clear();
            (*ptr).drop_hover = false;
            let _ = InvalidateRect(Some(hwnd), None, false);
        }
    }
}

unsafe fn update_drag_feedback(hwnd: HWND, ptr: *mut FenceWindow, lp: LPARAM) {
    let destination = drag_destination(hwnd, lp);
    let count = (*ptr).selection.len();
    let text = match &destination {
        Destination::Invalid => i18n::tf(K::DragInvalid, &[&count]),
        Destination::Desktop => i18n::tf(K::DragDesktop, &[&count]),
        Destination::Fence { fence, .. } if *fence == (*ptr).fence.id => {
            i18n::tf(K::DragReorder, &[&count])
        }
        Destination::Fence { fence, .. } => i18n::tf(K::DragMove, &[&count, fence]),
    };
    (*ptr).drag_text = text;
    let cursor = if matches!(destination, Destination::Invalid) {
        IDC_NO
    } else {
        IDC_SIZEALL
    };
    if let Ok(cursor) = LoadCursorW(None, cursor) {
        SetCursor(Some(cursor));
    }
    let _ = InvalidateRect(Some(hwnd), None, false);
}

// Disabled ancestors suppress input to otherwise enabled desktop children.
// Never enable another process's windows to work around a desktop manager.
unsafe fn desktop_accepts_input(mut hwnd: HWND) -> bool {
    loop {
        if !IsWindow(Some(hwnd)).as_bool() || !IsWindowEnabled(hwnd).as_bool() {
            return false;
        }
        let Ok(parent) = GetParent(hwnd) else {
            return true;
        };
        hwnd = parent;
    }
}

unsafe fn suspend_desktop_groups() {
    let state = MANAGER.with(|m| {
        let mut manager = m.borrow_mut();
        let manager = manager.as_mut()?;
        manager.status = i18n::t(K::DesktopUnavailable).into();
        Some((manager.icons.take(), std::mem::take(&mut manager.handles)))
    });
    if let Some((icons, handles)) = state {
        // Drop can call Shell COM; release the Manager borrow first.
        drop(icons);
        for hwnd in handles.into_values() {
            let _ = DestroyWindow(hwnd);
        }
    }
    invalidate_fences();
}

// A fence is a child of Explorer. Passing it directly to MessageBox disables
// Explorer's root window. Modal dialogs must belong to our own top-level UI.
unsafe fn dialog_owner(hwnd: HWND) -> Option<HWND> {
    let root = GetAncestor(hwnd, GA_ROOT);
    let mut pid = 0;
    GetWindowThreadProcessId(root, Some(&mut pid));
    if pid == std::process::id() {
        return Some(root);
    }
    MANAGER.with(|m| {
        m.borrow()
            .as_ref()
            .and_then(|m| m.launcher.or(m.controller))
    })
}

unsafe fn show_history(owner: HWND) {
    let text = MANAGER.with(|m| {
        m.borrow()
            .as_ref()
            .map(|m| {
                if m.layout.history.is_empty() {
                    return i18n::t(K::HistoryEmpty).into();
                }
                let rows = m
                    .layout
                    .history
                    .iter()
                    .rev()
                    .enumerate()
                    .map(|(n, r)| {
                        i18n::tf(
                            K::HistoryRow,
                            &[&(n + 1), &i18n::relabel(&r.label), &r.changes.len()],
                        )
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                i18n::tf(K::HistoryBody, &[&rows])
            })
            .unwrap_or_default()
    });
    let wide: Vec<u16> = text.encode_utf16().chain(Some(0)).collect();
    let _ = MessageBoxW(
        dialog_owner(owner),
        PCWSTR(wide.as_ptr()),
        i18n::w(K::HistoryTitle),
        MB_OK | MB_ICONINFORMATION,
    );
}

fn write_back(fence: &Fence) {
    MANAGER.with(|m| {
        let mut guard = m.borrow_mut();
        let Some(manager) = guard.as_mut() else {
            return;
        };
        if let Some(existing) = manager.layout.fences.iter_mut().find(|f| f.id == fence.id) {
            *existing = fence.clone();
            manager.persist();
        }
    });
}

unsafe extern "system" fn controller_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    if msg != 0 && SHOW_LAUNCHER_MESSAGE.with(|value| value.get()) == msg {
        ensure_fences();
        show_all_fences();
        show_launcher(hwnd);
        return LRESULT(0);
    }
    if TASKBAR_CREATED.with(|value| value.get()) == msg {
        add_tray_icon(hwnd);
        return LRESULT(0);
    }
    match msg {
        WM_WORK => {
            receive_work();
            LRESULT(0)
        }
        WM_ENTERMENULOOP => {
            MANAGER.with(|m| {
                if let Some(manager) = m.borrow_mut().as_mut() {
                    manager.shell_pause_depth += 1;
                }
            });
            let _ = KillTimer(Some(hwnd), 1);
            LRESULT(0)
        }
        WM_EXITMENULOOP => {
            let resume = MANAGER.with(|m| {
                m.borrow_mut().as_mut().is_some_and(|manager| {
                    manager.shell_pause_depth = manager.shell_pause_depth.saturating_sub(1);
                    manager.shell_pause_depth == 0
                })
            });
            if resume {
                let _ = SetTimer(Some(hwnd), 1, 3000, None);
            }
            LRESULT(0)
        }
        WM_TIMER if wp.0 == 1 => {
            if MANAGER.with(|m| m.borrow().as_ref().is_some_and(|m| m.shell_pause_depth > 0)) {
                return LRESULT(0);
            }
            ensure_fences();
            sync_desktop_icons();
            // Schedule from completion so a busy Shell cannot accumulate
            // refresh work or immediately recreate a freshly reconciled view.
            let _ = SetTimer(Some(hwnd), 1, 3000, None);
            LRESULT(0)
        }
        WM_BACKDROP_READY => {
            invalidate_fences();
            LRESULT(0)
        }
        WM_DESKTOP_CHANGED => {
            // Debounce bursts of change notifications (copies, downloads).
            let _ = SetTimer(Some(hwnd), AUTOFILE_TIMER, 800, None);
            LRESULT(0)
        }
        WM_TIMER if wp.0 == AUTOFILE_TIMER => {
            let _ = KillTimer(Some(hwnd), AUTOFILE_TIMER);
            if auto_file_new_items() {
                let _ = SetTimer(Some(hwnd), AUTOFILE_TIMER, 1000, None);
            }
            LRESULT(0)
        }
        WM_DISPLAYCHANGE | WM_SETTINGCHANGE => {
            // SPI_SETDESKWALLPAPER (0x14): rebuild the frosted backgrounds.
            if msg == WM_DISPLAYCHANGE || wp.0 == 0x14 {
                crate::backdrop::refresh(hwnd, WM_BACKDROP_READY);
            }
            if let Some(parent) = shell_desktop() {
                recover_offscreen_fences(parent);
            }
            ensure_fences();
            LRESULT(0)
        }
        WM_TRAY => {
            on_tray_event(hwnd, lp);
            LRESULT(0)
        }
        WM_DESTROY => {
            remove_tray_icon(hwnd);
            PostQuitMessage(0);
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, msg, wp, lp),
    }
}

unsafe fn show_launcher(controller: HWND) {
    let existing = MANAGER.with(|m| m.borrow().as_ref().and_then(|manager| manager.launcher));
    let hwnd = if let Some(hwnd) = existing.filter(|hwnd| IsWindow(Some(*hwnd)).as_bool()) {
        hwnd
    } else {
        // Logical client size scaled for the monitor under the cursor.
        let style = WS_OVERLAPPED | WS_CAPTION | WS_SYSMENU | WS_MINIMIZEBOX;
        let dpi = {
            let mut cursor = POINT::default();
            let _ = GetCursorPos(&mut cursor);
            let monitor = windows::Win32::Graphics::Gdi::MonitorFromPoint(
                cursor,
                windows::Win32::Graphics::Gdi::MONITOR_DEFAULTTONEAREST,
            );
            let (mut x, mut y) = (96u32, 96u32);
            let _ = windows::Win32::UI::HiDpi::GetDpiForMonitor(
                monitor,
                windows::Win32::UI::HiDpi::MDT_EFFECTIVE_DPI,
                &mut x,
                &mut y,
            );
            x.max(96)
        };
        let mut frame = RECT {
            left: 0,
            top: 0,
            right: (crate::launcher::WIDTH * dpi as f32 / 96.0) as i32,
            bottom: (crate::launcher::HEIGHT * dpi as f32 / 96.0) as i32,
        };
        let _ = windows::Win32::UI::HiDpi::AdjustWindowRectExForDpi(
            &mut frame,
            style,
            false,
            WS_EX_APPWINDOW,
            dpi,
        );
        let created = CreateWindowExW(
            WS_EX_APPWINDOW,
            LAUNCHER_CLASS,
            w!("LumaDesk"),
            style,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            frame.right - frame.left,
            frame.bottom - frame.top,
            Some(controller),
            None,
            Some(HINSTANCE(GetModuleHandleW(None).unwrap_or_default().0)),
            None,
        );
        let hwnd = match created {
            Ok(hwnd) => hwnd,
            Err(error) => {
                let text: Vec<u16> = i18n::tf(K::OpenWindowFailed, &[&error])
                    .encode_utf16()
                    .chain(Some(0))
                    .collect();
                let _ = MessageBoxW(
                    None,
                    PCWSTR(text.as_ptr()),
                    w!("LumaDesk"),
                    MB_OK | MB_ICONERROR,
                );
                return;
            }
        };
        MANAGER.with(|m| {
            if let Some(manager) = m.borrow_mut().as_mut() {
                manager.launcher = Some(hwnd);
            }
        });
        hwnd
    };
    let _ = ShowWindow(hwnd, SW_RESTORE);
    // A hidden-console launch can override the first ShowWindow request.
    let _ = SetWindowPos(
        hwnd,
        None,
        0,
        0,
        0,
        0,
        SWP_NOMOVE | SWP_NOSIZE | SWP_NOZORDER | SWP_SHOWWINDOW,
    );
    let _ = SetForegroundWindow(hwnd);
}

const CMD_MORE: usize = 1080;

/// Primary action, six tiles, then the exit button (see launcher::layout).
fn launcher_buttons() -> Vec<crate::launcher::Button> {
    crate::launcher::layout(&[
        (
            CMD_AUTO_PREVIEW,
            "\u{E8CB}",
            i18n::t(K::LAuto),
            i18n::t(K::LAutoDetail),
            true,
        ),
        (
            CMD_SHOW_DESKTOP,
            "\u{E7F4}",
            i18n::t(K::LShow),
            i18n::t(K::LShowDetail),
            false,
        ),
        (
            CMD_RESET_POSITIONS,
            "\u{E72C}",
            i18n::t(K::LFind),
            i18n::t(K::LFindDetail),
            false,
        ),
        (
            CMD_NEW,
            "\u{E710}",
            i18n::t(K::NewGroup),
            i18n::t(K::LNewDetail),
            true,
        ),
        (
            CMD_ARRANGE,
            "\u{F0E2}",
            i18n::t(K::LArrange),
            i18n::t(K::LArrangeDetail),
            false,
        ),
        (
            CMD_UNDO,
            "\u{E7A7}",
            i18n::t(K::LUndo),
            i18n::t(K::LUndoDetail),
            true,
        ),
        (
            CMD_MORE,
            "\u{E712}",
            i18n::t(K::LMore),
            i18n::t(K::LMoreDetail),
            false,
        ),
        (CMD_EXIT, "\u{E7E8}", i18n::t(K::Exit), "", false),
    ])
}

fn launcher_status() -> crate::launcher::Status {
    MANAGER.with(|m| {
        let guard = m.borrow();
        let Some(manager) = guard.as_ref() else {
            return crate::launcher::Status {
                groups: 0,
                text: String::new(),
                busy: false,
            };
        };
        crate::launcher::Status {
            groups: manager.layout.fences.len(),
            text: if manager.status.is_empty() {
                i18n::t(K::IconsSynced).to_owned()
            } else {
                manager.status.clone()
            },
            busy: manager.queued > 0,
        }
    })
}

/// Physical client pixels -> the launcher's logical (96 DPI) layout.
unsafe fn launcher_point(hwnd: HWND, lp: LPARAM) -> (f32, f32) {
    let (x, y) = point(lp);
    let scale = 96.0 / windows::Win32::UI::HiDpi::GetDpiForWindow(hwnd).max(96) as f32;
    (x as f32 * scale, y as f32 * scale)
}

unsafe fn launcher_activate(hwnd: HWND, index: usize) {
    let busy = launcher_status().busy;
    let button = crate::launcher::STATE.with(|s| {
        s.borrow()
            .as_ref()
            .and_then(|state| state.buttons.get(index).copied())
    });
    let Some(button) = button else {
        return;
    };
    if button.needs_idle && busy {
        return;
    }
    if button.command == CMD_MORE {
        launcher_more_menu(hwnd, button.rect);
    } else {
        let _ = SendMessageW(
            hwnd,
            WM_COMMAND,
            Some(WPARAM(button.command)),
            Some(LPARAM(0)),
        );
    }
}

/// Less frequent actions, under the "more" tile.
unsafe fn launcher_more_menu(
    hwnd: HWND,
    anchor: windows::Win32::Graphics::Direct2D::Common::D2D_RECT_F,
) {
    let Ok(menu) = CreatePopupMenu() else {
        return;
    };
    let auto_file = MANAGER.with(|m| m.borrow().as_ref().is_some_and(|m| m.layout.auto_file));
    let check = |on: bool| if on { MF_CHECKED } else { MF_UNCHECKED };
    let _ = AppendMenuW(
        menu,
        MF_STRING | check(auto_file),
        CMD_AUTO_FILE,
        i18n::w(K::AutoFile),
    );
    let _ = AppendMenuW(
        menu,
        MF_STRING | check(autostart_enabled()),
        CMD_AUTOSTART,
        i18n::w(K::Autostart),
    );
    let _ = AppendMenuW(menu, MF_SEPARATOR, 0, None);
    let _ = AppendMenuW(menu, MF_STRING, CMD_HISTORY, i18n::w(K::HistoryMenu));
    let _ = AppendMenuW(
        menu,
        MF_STRING,
        CMD_COLLECT_DESKTOP,
        i18n::w(K::RefreshGroups),
    );
    let _ = AppendMenuW(menu, MF_STRING, CMD_THEME, i18n::w(K::GroupTheme));
    append_language_menu(menu);
    let _ = AppendMenuW(menu, MF_STRING, CMD_OPEN_SETTINGS, i18n::w(K::OpenSettings));
    let scale = windows::Win32::UI::HiDpi::GetDpiForWindow(hwnd).max(96) as f32 / 96.0;
    let mut pt = POINT {
        x: (anchor.left * scale) as i32,
        y: (anchor.bottom * scale) as i32,
    };
    let _ = ClientToScreen(hwnd, &mut pt);
    let command = TrackPopupMenu(
        menu,
        TPM_RETURNCMD | TPM_NONOTIFY | TPM_LEFTBUTTON,
        pt.x,
        pt.y,
        None,
        hwnd,
        None,
    )
    .0 as usize;
    let _ = DestroyMenu(menu);
    if command != 0 {
        let _ = SendMessageW(hwnd, WM_COMMAND, Some(WPARAM(command)), Some(LPARAM(0)));
    }
}

unsafe fn launcher_update(hwnd: HWND, f: impl FnOnce(&mut crate::launcher::LauncherState) -> bool) {
    let changed = crate::launcher::STATE.with(|s| s.borrow_mut().as_mut().is_some_and(f));
    if changed {
        let _ = InvalidateRect(Some(hwnd), None, false);
    }
}

unsafe extern "system" fn launcher_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match msg {
        WM_CREATE => {
            let dark = crate::launcher::system_dark();
            crate::launcher::apply_window_style(hwnd, dark);
            crate::launcher::STATE.with(|s| {
                *s.borrow_mut() = Some(crate::launcher::LauncherState {
                    view: None,
                    buttons: launcher_buttons(),
                    hover: None,
                    pressed: None,
                    focus: None,
                    dark,
                })
            });
            // Hidden idle indicator kept for automation (enabled when no work
            // is queued); the visible UI is drawn.
            let _ = CreateWindowExW(
                WINDOW_EX_STYLE(0),
                w!("STATIC"),
                w!(""),
                WS_CHILD,
                0,
                0,
                0,
                0,
                Some(hwnd),
                Some(HMENU(CMD_NEW as *mut c_void)),
                None,
                None,
            );
            LRESULT(0)
        }
        WM_ERASEBKGND => LRESULT(1),
        WM_PAINT => {
            let mut ps = PAINTSTRUCT::default();
            let _ = BeginPaint(hwnd, &mut ps);
            let status = launcher_status();
            let failed = crate::launcher::STATE.with(|s| {
                let mut guard = s.borrow_mut();
                let Some(state) = guard.as_mut() else {
                    return false;
                };
                if state.view.is_none() {
                    state.view = crate::launcher::View::new(hwnd, Some(app_icon())).ok();
                }
                let Some(view) = &state.view else {
                    return false;
                };
                let result = view.paint(
                    &state.buttons,
                    &status,
                    state.dark,
                    state.hover,
                    state.pressed,
                    state.focus,
                    i18n::t(K::LauncherHelp),
                );
                let failed = result.is_err();
                if failed {
                    state.view = None;
                }
                failed
            });
            let _ = EndPaint(hwnd, &ps);
            if failed {
                let _ = InvalidateRect(Some(hwnd), None, false);
            }
            LRESULT(0)
        }
        WM_SIZE => {
            let (width, height) = point(lp);
            let dpi = windows::Win32::UI::HiDpi::GetDpiForWindow(hwnd);
            crate::launcher::STATE.with(|s| {
                if let Some(view) = s.borrow().as_ref().and_then(|s| s.view.as_ref()) {
                    let _ = view.resize(width.max(1) as u32, height.max(1) as u32, dpi);
                }
            });
            LRESULT(0)
        }
        WM_DPICHANGED => {
            let suggested = &*(lp.0 as *const RECT);
            let _ = SetWindowPos(
                hwnd,
                None,
                suggested.left,
                suggested.top,
                suggested.right - suggested.left,
                suggested.bottom - suggested.top,
                SWP_NOZORDER | SWP_NOACTIVATE,
            );
            crate::launcher::STATE.with(|s| {
                if let Some(state) = s.borrow_mut().as_mut() {
                    state.view = None;
                }
            });
            let _ = InvalidateRect(Some(hwnd), None, false);
            LRESULT(0)
        }
        WM_SETTINGCHANGE => {
            // Follow the Windows light/dark switch.
            let dark = crate::launcher::system_dark();
            crate::launcher::apply_window_style(hwnd, dark);
            launcher_update(hwnd, |s| {
                let changed = s.dark != dark;
                s.dark = dark;
                changed
            });
            LRESULT(0)
        }
        WM_MOUSEMOVE => {
            let mut tracking = TRACKMOUSEEVENT {
                cbSize: std::mem::size_of::<TRACKMOUSEEVENT>() as u32,
                dwFlags: TME_LEAVE,
                hwndTrack: hwnd,
                ..Default::default()
            };
            let _ = TrackMouseEvent(&mut tracking);
            let (x, y) = launcher_point(hwnd, lp);
            launcher_update(hwnd, |s| {
                let hover = crate::launcher::hit(&s.buttons, x, y);
                let changed = s.hover != hover;
                s.hover = hover;
                changed
            });
            LRESULT(0)
        }
        WM_MOUSELEAVE => {
            launcher_update(hwnd, |s| {
                let changed = s.hover.is_some();
                s.hover = None;
                changed
            });
            LRESULT(0)
        }
        WM_LBUTTONDOWN => {
            let (x, y) = launcher_point(hwnd, lp);
            launcher_update(hwnd, |s| {
                s.pressed = crate::launcher::hit(&s.buttons, x, y);
                s.focus = None;
                true
            });
            let _ = SetCapture(hwnd);
            LRESULT(0)
        }
        WM_LBUTTONUP => {
            let _ = ReleaseCapture();
            let (x, y) = launcher_point(hwnd, lp);
            let mut target = None;
            launcher_update(hwnd, |s| {
                let over = crate::launcher::hit(&s.buttons, x, y);
                if over.is_some() && over == s.pressed {
                    target = over;
                }
                s.pressed = None;
                true
            });
            if let Some(index) = target {
                launcher_activate(hwnd, index);
            }
            LRESULT(0)
        }
        WM_KEYDOWN => {
            // Keyboard: Tab / arrows move focus, Enter / Space activate.
            let key = wp.0 as u16;
            let shift = GetKeyState(VK_SHIFT.0 as i32) < 0;
            let mut activate = None;
            launcher_update(hwnd, |s| {
                let count = s.buttons.len();
                let current = s.focus;
                let back =
                    |f: Option<usize>| Some(f.map_or(count - 1, |f| (f + count - 1) % count));
                let next = |f: Option<usize>| Some(f.map_or(0, |f| (f + 1) % count));
                s.focus = match key {
                    0x09 if shift => back(current),
                    0x09 | 0x27 | 0x28 => next(current),
                    0x25 | 0x26 => back(current),
                    0x0D | 0x20 => {
                        activate = current;
                        current
                    }
                    _ => current,
                };
                true
            });
            if key == 0x1B {
                let _ = ShowWindow(hwnd, SW_HIDE);
            }
            if let Some(index) = activate {
                launcher_activate(hwnd, index);
            }
            LRESULT(0)
        }
        WM_GETDLGCODE => LRESULT(DLGC_WANTALLKEYS as isize),
        WM_COMMAND => {
            if wp.0 == CMD_UNDO {
                request_undo(hwnd);
                return LRESULT(0);
            }
            if wp.0 == CMD_HISTORY {
                show_history(hwnd);
                return LRESULT(0);
            }
            match wp.0 & 0xffff {
                CMD_AUTO_PREVIEW => request_organize_preview(hwnd),
                CMD_SHOW_DESKTOP => {
                    show_all_fences();
                    // This is an explicit user action on a visible button.
                    let result = CoCreateInstance::<_, IShellDispatch4>(
                        &Shell,
                        None,
                        CLSCTX_INPROC_SERVER | CLSCTX_LOCAL_SERVER,
                    )
                    .and_then(|shell| shell.MinimizeAll());
                    if result.is_err() {
                        let _ = MessageBoxW(
                            dialog_owner(hwnd),
                            i18n::w(K::WinD),
                            w!("LumaDesk"),
                            MB_OK | MB_ICONINFORMATION,
                        );
                    }
                }
                CMD_RESET_POSITIONS => {
                    reset_positions();
                    show_all_fences();
                }
                CMD_ARRANGE => arrange_fences(false),
                CMD_COLLECT_DESKTOP => collect_existing_desktop(hwnd),
                CMD_NEW => create_collection(),
                CMD_AUTO_FILE => toggle_auto_file(),
                command if language_command(command) => {}
                command if language_command(command) => {}
                CMD_THEME => switch_theme(),
                CMD_OPEN_SETTINGS => open_settings_folder(),
                CMD_AUTOSTART => {
                    if let Err(err) = toggle_autostart() {
                        let wide: Vec<u16> = err.encode_utf16().chain(Some(0)).collect();
                        let _ = MessageBoxW(
                            dialog_owner(hwnd),
                            PCWSTR(wide.as_ptr()),
                            w!("LumaDesk"),
                            MB_OK | MB_ICONERROR,
                        );
                    }
                }
                CMD_EXIT => exit_app(),
                _ => {}
            }
            let _ = InvalidateRect(Some(hwnd), None, false);
            LRESULT(0)
        }
        WM_CLOSE => {
            let _ = ShowWindow(hwnd, SW_HIDE);
            LRESULT(0)
        }
        WM_DESTROY => {
            crate::launcher::STATE.with(|s| *s.borrow_mut() = None);
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, msg, wp, lp),
    }
}

unsafe fn request_organize_preview(owner: HWND) {
    let existing = MANAGER.with(|m| m.borrow().as_ref().and_then(|m| m.preview));
    if let Some(preview) = existing.filter(|h| IsWindow(Some(*h)).as_bool()) {
        let _ = ShowWindow(preview, SW_RESTORE);
        let _ = SetForegroundWindow(preview);
        return;
    }
    if !work_busy() {
        enqueue(owner, Command::PreviewAuto);
    }
}

unsafe fn show_organize_preview(plan: crate::organize::Plan) {
    let controller = MANAGER.with(|m| m.borrow().as_ref().and_then(|m| m.controller));
    let overview = plan.preview_text();
    let text: Vec<u16> = i18n::tf(
        K::PreviewHint,
        &[&overview.split("\r\n\r\n").next().unwrap_or_default()],
    )
    .encode_utf16()
    .chain(Some(0))
    .collect();
    let skipped: Vec<u16> = i18n::tf(K::PreviewSkipped, &[&plan.skipped.join("\r\n")])
        .encode_utf16()
        .chain(Some(0))
        .collect();
    let has_items = !plan.items.is_empty();
    let created = CreateWindowExW(
        WS_EX_APPWINDOW,
        PREVIEW_CLASS,
        i18n::w(K::PreviewTitle),
        WS_OVERLAPPED | WS_CAPTION | WS_SYSMENU | WS_MINIMIZEBOX,
        CW_USEDEFAULT,
        CW_USEDEFAULT,
        650,
        620,
        controller,
        None,
        Some(HINSTANCE(GetModuleHandleW(None).unwrap_or_default().0)),
        None,
    );
    let preview = match created {
        Ok(h) => h,
        Err(error) => {
            if let Some(owner) = controller {
                collection_error(owner, &error.to_string());
            }
            return;
        }
    };
    if let Ok(list) = GetDlgItem(Some(preview), 1101) {
        for item in &plan.items {
            let label: Vec<u16> = i18n::tf(
                K::Bracketed,
                &[
                    &item.category.title(),
                    &item.path.file_name().unwrap_or_default().to_string_lossy(),
                ],
            )
            .encode_utf16()
            .chain(Some(0))
            .collect();
            let added = SendMessageW(
                list,
                0x0180,
                Some(WPARAM(0)),
                Some(LPARAM(label.as_ptr() as isize)),
            ); // LB_ADDSTRING
            if added.0 < 0 {
                let _ = DestroyWindow(preview);
                if let Some(owner) = controller {
                    collection_error(owner, i18n::t(K::PreviewFailed));
                }
                return;
            }
        }
        SendMessageW(list, 0x0185, Some(WPARAM(1)), Some(LPARAM(-1))); // LB_SETSEL
        SendMessageW(list, 0x0194, Some(WPARAM(2400)), Some(LPARAM(0))); // horizontal extent
    }
    if let Ok(edit) = GetDlgItem(Some(preview), 1102) {
        SendMessageW(edit, 0x00C5, Some(WPARAM(8 * 1024 * 1024)), Some(LPARAM(0)));
        let _ = SetWindowTextW(edit, PCWSTR(skipped.as_ptr()));
    }
    MANAGER.with(|m| {
        if let Some(m) = m.borrow_mut().as_mut() {
            m.preview = Some(preview);
            m.organize_plan = Some(plan);
            m.status = i18n::t(K::PreviewReady).into();
        }
    });
    if let Ok(edit) = GetDlgItem(Some(preview), 1100) {
        // Read-only edit defaults to 32 KiB; permit large desktop previews.
        SendMessageW(edit, 0x00C5, Some(WPARAM(8 * 1024 * 1024)), Some(LPARAM(0)));
        let _ = SetWindowTextW(edit, PCWSTR(text.as_ptr()));
    }
    if let Ok(button) = GetDlgItem(Some(preview), CMD_AUTO_APPLY as i32) {
        let _ = windows::Win32::UI::Input::KeyboardAndMouse::EnableWindow(button, has_items);
    }
    let _ = ShowWindow(preview, SW_SHOW);
    let _ = SetWindowPos(
        preview,
        None,
        0,
        0,
        0,
        0,
        SWP_NOMOVE | SWP_NOSIZE | SWP_NOZORDER | SWP_SHOWWINDOW,
    );
    let _ = SetForegroundWindow(preview);
    // Cancellation is the default keyboard action, so Enter cannot move files
    // merely because a scan finished while the user was typing.
    if let Ok(cancel) = GetDlgItem(Some(preview), 2) {
        let _ = SetFocus(Some(cancel));
    }
}

unsafe extern "system" fn preview_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match msg {
        WM_CREATE => {
            for (class, text, x, y, width, height, id, style) in [
                (
                    w!("EDIT"),
                    i18n::w(K::PreviewPreparing),
                    20,
                    20,
                    594,
                    110,
                    1100usize,
                    WS_BORDER
                        | WS_VSCROLL
                        | WS_HSCROLL
                        | WINDOW_STYLE(
                            (ES_MULTILINE | ES_READONLY | ES_AUTOVSCROLL | ES_AUTOHSCROLL) as u32,
                        ),
                ),
                (
                    w!("LISTBOX"),
                    w!(""),
                    20,
                    140,
                    594,
                    260,
                    1101,
                    WS_BORDER
                        | WS_VSCROLL
                        | WS_HSCROLL
                        | WINDOW_STYLE(
                            (LBS_EXTENDEDSEL | LBS_NOINTEGRALHEIGHT | LBS_HASSTRINGS) as u32,
                        ),
                ),
                (
                    w!("EDIT"),
                    w!(""),
                    20,
                    412,
                    594,
                    80,
                    1102,
                    WS_BORDER
                        | WS_VSCROLL
                        | WS_HSCROLL
                        | WINDOW_STYLE(
                            (ES_MULTILINE | ES_READONLY | ES_AUTOVSCROLL | ES_AUTOHSCROLL) as u32,
                        ),
                ),
                (
                    w!("BUTTON"),
                    i18n::w(K::SelectAll),
                    20,
                    520,
                    94,
                    36,
                    1103,
                    WINDOW_STYLE(0),
                ),
                (
                    w!("BUTTON"),
                    i18n::w(K::ClearSelection),
                    124,
                    520,
                    100,
                    36,
                    1104,
                    WINDOW_STYLE(0),
                ),
                (
                    w!("BUTTON"),
                    i18n::w(K::StartOrganize),
                    288,
                    520,
                    156,
                    36,
                    CMD_AUTO_APPLY,
                    WINDOW_STYLE(0),
                ),
                (
                    w!("BUTTON"),
                    i18n::w(K::Cancel),
                    466,
                    520,
                    148,
                    36,
                    2,
                    WINDOW_STYLE(BS_DEFPUSHBUTTON as u32),
                ),
            ] {
                if let Ok(child) = CreateWindowExW(
                    WINDOW_EX_STYLE(0),
                    class,
                    text,
                    WS_CHILD | WS_VISIBLE | WS_TABSTOP | style,
                    x,
                    y,
                    width,
                    height,
                    Some(hwnd),
                    Some(HMENU(id as *mut c_void)),
                    None,
                    None,
                ) {
                    SendMessageW(
                        child,
                        WM_SETFONT,
                        Some(WPARAM(GetStockObject(DEFAULT_GUI_FONT).0 as usize)),
                        Some(LPARAM(1)),
                    );
                }
            }
            LRESULT(0)
        }
        WM_COMMAND if wp.0 & 0xffff == CMD_AUTO_APPLY => {
            if work_busy() {
                return LRESULT(0);
            }
            let mut plan =
                MANAGER.with(|m| m.borrow().as_ref().and_then(|m| m.organize_plan.clone()));
            if let Some(plan) = &mut plan {
                if let Ok(list) = GetDlgItem(Some(hwnd), 1101) {
                    plan.items = std::mem::take(&mut plan.items)
                        .into_iter()
                        .enumerate()
                        .filter_map(|(index, item)| {
                            (SendMessageW(list, 0x0187, Some(WPARAM(index)), Some(LPARAM(0))).0 > 0)
                                .then_some(item)
                        })
                        .collect();
                } else {
                    return LRESULT(0);
                }
                if plan.items.is_empty() {
                    return LRESULT(0);
                }
            }
            let owner = MANAGER
                .with(|m| {
                    m.borrow()
                        .as_ref()
                        .and_then(|m| m.launcher.or(m.controller))
                })
                .unwrap_or(hwnd);
            let _ = DestroyWindow(hwnd);
            if let Some(plan) = plan {
                enqueue(owner, Command::AutoOrganize { plan });
            }
            LRESULT(0)
        }
        WM_COMMAND if wp.0 & 0xffff == 2 => {
            let _ = DestroyWindow(hwnd);
            LRESULT(0)
        }
        WM_COMMAND if wp.0 & 0xffff == 1103 || wp.0 & 0xffff == 1104 => {
            if let Ok(list) = GetDlgItem(Some(hwnd), 1101) {
                SendMessageW(
                    list,
                    0x0185,
                    Some(WPARAM(usize::from(wp.0 & 0xffff == 1103))),
                    Some(LPARAM(-1)),
                );
            }
            LRESULT(0)
        }
        WM_CLOSE => {
            let _ = DestroyWindow(hwnd);
            LRESULT(0)
        }
        WM_DESTROY => {
            MANAGER.with(|m| {
                if let Some(m) = m.borrow_mut().as_mut() {
                    m.preview = None;
                    m.organize_plan = None;
                    if m.queued == 0 {
                        m.status.clear();
                    }
                }
            });
            invalidate_fences();
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, msg, wp, lp),
    }
}

unsafe fn tray_data(hwnd: HWND) -> NOTIFYICONDATAW {
    let mut data = NOTIFYICONDATAW {
        cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
        hWnd: hwnd,
        uID: TRAY_ID,
        uFlags: NIF_ICON | NIF_MESSAGE | NIF_TIP,
        uCallbackMessage: WM_TRAY,
        ..Default::default()
    };
    data.hIcon = app_icon();
    for (destination, value) in data
        .szTip
        .iter_mut()
        .zip(i18n::tf(K::TrayTip, &[&env!("CARGO_PKG_VERSION")]).encode_utf16())
    {
        *destination = value;
    }
    data
}

unsafe fn add_tray_icon(hwnd: HWND) {
    let mut data = tray_data(hwnd);
    if Shell_NotifyIconW(NIM_ADD, &data).as_bool() {
        data.Anonymous.uVersion = NOTIFYICON_VERSION_4;
        let _ = Shell_NotifyIconW(NIM_SETVERSION, &data);
    }
}

unsafe fn remove_tray_icon(hwnd: HWND) {
    let data = tray_data(hwnd);
    let _ = Shell_NotifyIconW(NIM_DELETE, &data);
}

unsafe fn on_tray_event(hwnd: HWND, lp: LPARAM) {
    match lp.0 as u32 & 0xffff {
        WM_RBUTTONUP | WM_CONTEXTMENU => tray_menu(hwnd),
        WM_LBUTTONDBLCLK => show_launcher(hwnd),
        _ => {}
    }
}

unsafe fn tray_menu(hwnd: HWND) {
    let Ok(menu) = CreatePopupMenu() else { return };
    let _ = AppendMenuW(
        menu,
        MF_STRING,
        CMD_AUTO_PREVIEW,
        i18n::w(K::AutoPreviewMenu),
    );
    let _ = AppendMenuW(menu, MF_STRING, CMD_NEW, i18n::w(K::NewGroup));
    let _ = AppendMenuW(menu, MF_STRING, CMD_SHOW_ALL, i18n::w(K::ShowAll));
    let _ = AppendMenuW(menu, MF_STRING, CMD_RESET_POSITIONS, i18n::w(K::FindGroups));
    let _ = AppendMenuW(menu, MF_STRING, CMD_ARRANGE, i18n::w(K::ArrangeGroups));
    if let Ok(sizes) = CreatePopupMenu() {
        for (level, label) in SIZE_LABELS.iter().enumerate() {
            let text: Vec<u16> = i18n::t(*label).encode_utf16().chain(Some(0)).collect();
            let _ = AppendMenuW(
                sizes,
                MF_STRING,
                CMD_ALL_SIZE_BASE + level,
                PCWSTR(text.as_ptr()),
            );
        }
        let _ = AppendMenuW(menu, MF_POPUP, sizes.0 as usize, i18n::w(K::AllSizes));
    }
    let _ = AppendMenuW(menu, MF_STRING, CMD_THEME, i18n::w(K::ToggleTheme));
    let auto_file = MANAGER.with(|m| m.borrow().as_ref().is_some_and(|m| m.layout.auto_file));
    let _ = AppendMenuW(
        menu,
        if auto_file {
            MF_STRING | MF_CHECKED
        } else {
            MF_STRING
        },
        CMD_AUTO_FILE,
        i18n::w(K::AutoFile),
    );
    let autostart_flags = if autostart_enabled() {
        MF_STRING | MF_CHECKED
    } else {
        MF_STRING
    };
    let _ = AppendMenuW(menu, autostart_flags, CMD_AUTOSTART, i18n::w(K::Autostart));
    append_language_menu(menu);
    let _ = AppendMenuW(menu, MF_SEPARATOR, 0, None);
    let _ = AppendMenuW(menu, MF_STRING, CMD_OPEN_SETTINGS, i18n::w(K::OpenSettings));
    let _ = AppendMenuW(menu, MF_STRING, CMD_EXIT, i18n::w(K::Exit));
    let mut cursor = POINT::default();
    let _ = GetCursorPos(&mut cursor);
    let _ = SetForegroundWindow(hwnd);
    let command = TrackPopupMenu(
        menu,
        TPM_RETURNCMD | TPM_NONOTIFY | TPM_RIGHTBUTTON,
        cursor.x,
        cursor.y,
        None,
        hwnd,
        None,
    )
    .0 as usize;
    let _ = DestroyMenu(menu);
    let _ = PostMessageW(Some(hwnd), WM_NULL, WPARAM(0), LPARAM(0));
    match command {
        CMD_AUTO_PREVIEW => request_organize_preview(hwnd),
        CMD_NEW => create_collection(),
        CMD_SHOW_ALL => show_all_fences(),
        CMD_RESET_POSITIONS => reset_positions(),
        CMD_ARRANGE => arrange_fences(false),
        CMD_AUTO_FILE => toggle_auto_file(),
        command if language_command(command) => {}
        command
            if (CMD_ALL_SIZE_BASE..CMD_ALL_SIZE_BASE + SIZE_LABELS.len()).contains(&command) =>
        {
            set_all_sizes(command - CMD_ALL_SIZE_BASE)
        }
        CMD_THEME => switch_theme(),
        CMD_AUTOSTART => {
            if let Err(err) = toggle_autostart() {
                let wide: Vec<u16> = err.encode_utf16().chain(Some(0)).collect();
                let _ = MessageBoxW(
                    dialog_owner(hwnd),
                    PCWSTR(wide.as_ptr()),
                    w!("LumaDesk"),
                    MB_OK | MB_ICONERROR,
                );
            }
        }
        CMD_OPEN_SETTINGS => open_settings_folder(),
        CMD_EXIT => exit_app(),
        _ => {}
    }
}

unsafe fn open_run_key(access: REG_SAM_FLAGS) -> Result<HKEY, String> {
    let mut key = HKEY::default();
    let result = RegOpenKeyExW(HKEY_CURRENT_USER, RUN_KEY, None, access, &mut key);
    if result == ERROR_SUCCESS {
        Ok(key)
    } else {
        Err(i18n::tf(K::AutostartFailed, &[&format!("{result:?}")]))
    }
}

unsafe fn autostart_enabled() -> bool {
    let Ok(key) = open_run_key(KEY_QUERY_VALUE) else {
        return false;
    };
    let mut len = 0;
    let found = RegQueryValueExW(key, RUN_VALUE, None, None, None, Some(&mut len)) == ERROR_SUCCESS;
    let _ = RegCloseKey(key);
    found
}

unsafe fn toggle_autostart() -> Result<(), String> {
    let enabled = autostart_enabled();
    let key = open_run_key(KEY_SET_VALUE)?;
    let result = if enabled {
        RegDeleteValueW(key, RUN_VALUE)
    } else {
        let exe = std::env::current_exe().map_err(|error| error.to_string())?;
        let command = format!("\"{}\" --background", exe.display());
        let wide: Vec<u16> = command.encode_utf16().chain(Some(0)).collect();
        let bytes = std::slice::from_raw_parts(wide.as_ptr().cast::<u8>(), wide.len() * 2);
        RegSetValueExW(key, RUN_VALUE, None, REG_SZ, Some(bytes))
    };
    let _ = RegCloseKey(key);
    if result == ERROR_SUCCESS {
        Ok(())
    } else {
        Err(i18n::tf(K::AutostartFailed, &[&format!("{result:?}")]))
    }
}

unsafe fn create_collection() {
    if work_busy() {
        return;
    }
    let area = new_fence_area();
    MANAGER.with(|m| {
        if let Some(manager) = m.borrow_mut().as_mut() {
            manager.layout.add_fence_in(area);
            manager.persist();
        }
    });
    ensure_fences();
}

/// Work area (in desktop client coordinates) of the monitor showing the
/// newest collection's header, or the primary monitor.
unsafe fn new_fence_area() -> Option<crate::model::Area> {
    let parent = shell_desktop()?;
    let origin = desktop_origin(parent);
    let regions = monitor_regions();
    let last = MANAGER.with(|m| {
        m.borrow()
            .as_ref()
            .and_then(|m| m.layout.fences.last().cloned())
    });
    let region = last
        .and_then(|f| {
            let x = origin.x.saturating_add(f.x).saturating_add(f.width / 2);
            let y = origin
                .y
                .saturating_add(f.y)
                .saturating_add(HEADER_HEIGHT / 2);
            regions.iter().find(|r| {
                x >= r.work.left && x < r.work.right && y >= r.work.top && y < r.work.bottom
            })
        })
        .or_else(|| regions.iter().find(|r| r.primary))
        .or_else(|| regions.first())?;
    Some(crate::model::Area {
        left: region.work.left - origin.x,
        top: region.work.top - origin.y,
        right: region.work.right - origin.x,
        bottom: region.work.bottom - origin.y,
    })
}

/// Language commands: +0 follows Windows, then one per `i18n::LANGS` entry.
const CMD_LANG_BASE: usize = 1100;

/// 「語言」 submenu; each language is listed in its own name.
unsafe fn append_language_menu(menu: HMENU) {
    let Ok(languages) = CreatePopupMenu() else {
        return;
    };
    let chosen = crate::i18n::chosen();
    let check = |on: bool| if on { MF_CHECKED } else { MF_UNCHECKED };
    let item = |flags, id: usize, text: &str| {
        let wide: Vec<u16> = text.encode_utf16().chain(Some(0)).collect();
        let _ = AppendMenuW(languages, flags, id, PCWSTR(wide.as_ptr()));
    };
    item(
        MF_STRING | check(chosen.is_none()),
        CMD_LANG_BASE,
        &format!(
            "{} · {}",
            i18n::t(K::LanguageAuto),
            crate::i18n::system().native()
        ),
    );
    let _ = AppendMenuW(languages, MF_SEPARATOR, 0, None);
    for (index, lang) in crate::i18n::LANGS.into_iter().enumerate() {
        item(
            MF_STRING | check(chosen == Some(lang)),
            CMD_LANG_BASE + 1 + index,
            lang.native(),
        );
    }
    let _ = AppendMenuW(menu, MF_POPUP, languages.0 as usize, i18n::w(K::Language));
}

/// Applies a language command live; false when `command` is not one.
unsafe fn language_command(command: usize) -> bool {
    let languages = crate::i18n::LANGS;
    if !(CMD_LANG_BASE..=CMD_LANG_BASE + languages.len()).contains(&command) {
        return false;
    }
    let choice = languages
        .get(command.wrapping_sub(CMD_LANG_BASE + 1))
        .copied();
    i18n::set(choice);
    let (handles, launcher, controller) = MANAGER.with(|m| {
        let mut guard = m.borrow_mut();
        let Some(manager) = guard.as_mut() else {
            return (vec![], None, None);
        };
        manager.layout.language = choice.map(|l| l.code().to_owned());
        manager.persist();
        // Status text was written in the previous language.
        if manager.queued == 0 {
            manager.status.clear();
        }
        (
            manager.handles.values().copied().collect::<Vec<_>>(),
            manager.launcher,
            manager.controller,
        )
    });
    // Text formats carry the locale used for font fallback: rebuild them.
    for hwnd in handles {
        let state = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut FenceWindow;
        if !state.is_null() {
            (*state).renderer = None;
            let _ = InvalidateRect(Some(hwnd), None, false);
        }
    }
    crate::launcher::STATE.with(|s| {
        if let Some(state) = s.borrow_mut().as_mut() {
            state.buttons = launcher_buttons();
            state.view = None;
        }
    });
    if let Some(launcher) = launcher {
        let _ = InvalidateRect(Some(launcher), None, false);
    }
    if let Some(controller) = controller {
        let data = tray_data(controller);
        let _ = Shell_NotifyIconW(NIM_MODIFY, &data);
    }
    true
}

unsafe fn switch_theme() {
    let changed = MANAGER.with(|m| {
        let mut guard = m.borrow_mut();
        let manager = guard.as_mut()?;
        manager.layout.theme = match manager.layout.theme {
            Theme::Midnight => Theme::Cloud,
            Theme::Cloud => Theme::Midnight,
        };
        manager.persist();
        Some((
            manager.layout.theme,
            manager.handles.values().copied().collect::<Vec<_>>(),
        ))
    });
    if let Some((theme, handles)) = changed {
        for hwnd in handles {
            let state = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut FenceWindow;
            if !state.is_null() {
                (*state).theme = theme;
                let _ = InvalidateRect(Some(hwnd), None, false);
            }
        }
    }
}

unsafe fn show_all_fences() {
    let windows = MANAGER.with(|m| {
        m.borrow()
            .as_ref()
            .map(|mgr| mgr.handles.values().copied().collect::<Vec<_>>())
            .unwrap_or_default()
    });
    for hwnd in windows {
        if IsWindow(Some(hwnd)).as_bool() {
            let _ = SetWindowPos(
                hwnd,
                Some(HWND_TOP),
                0,
                0,
                0,
                0,
                SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
            );
        }
    }
}

/// "找回分組": tile every collection onto the primary monitor.
unsafe fn reset_positions() {
    arrange_fences(true);
}

/// Tiles collections without overlap, keeping sizes. The previous positions
/// are kept so the next Undo restores them as one step.
unsafe fn arrange_fences(primary_only: bool) {
    let Some(parent) = shell_desktop() else {
        return;
    };
    let regions = monitor_regions();
    let Some(primary) = regions
        .iter()
        .find(|m| m.primary)
        .or_else(|| regions.first())
        .copied()
    else {
        return;
    };
    let origin = desktop_origin(parent);
    let placements = MANAGER.with(|m| {
        let mut guard = m.borrow_mut();
        let Some(manager) = guard.as_mut() else {
            return vec![];
        };
        let before: Vec<_> = manager
            .layout
            .fences
            .iter()
            .map(|f| (f.id, f.x, f.y))
            .collect();
        let after = arranged_positions(
            &manager.layout.fences,
            origin,
            &regions,
            primary,
            primary_only,
        );
        let mut placements = vec![];
        for (id, x, y) in after {
            if let Some(fence) = manager.layout.fences.iter_mut().find(|f| f.id == id) {
                fence.x = x;
                fence.y = y;
                if let Some(hwnd) = manager.handles.get(&id) {
                    placements.push((*hwnd, x, y));
                }
            }
        }
        manager.arranged = Some((now_secs(), before));
        manager.persist();
        placements
    });
    apply_fence_positions(placements);
    keep_fences_above_icons(parent);
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

/// Undo restores a rearrangement when it is newer than the last item
/// operation; otherwise the item history is undone by the worker.
unsafe fn request_undo(owner: HWND) {
    let restore = MANAGER.with(|m| {
        let mut guard = m.borrow_mut();
        let manager = guard.as_mut()?;
        let newest = manager.layout.history.last().map_or(0, |r| r.time);
        if manager.queued == 0 && manager.arranged.as_ref().is_some_and(|(t, _)| *t >= newest) {
            let (_, before) = manager.arranged.take()?;
            let mut placements = vec![];
            for (id, x, y) in before {
                if let Some(fence) = manager.layout.fences.iter_mut().find(|f| f.id == id) {
                    fence.x = x;
                    fence.y = y;
                    if let Some(hwnd) = manager.handles.get(&id) {
                        placements.push((*hwnd, x, y));
                    }
                }
            }
            manager.persist();
            Some(placements)
        } else {
            None
        }
    });
    match restore {
        Some(placements) => apply_fence_positions(placements),
        None => {
            enqueue(owner, Command::Undo);
        }
    }
}

fn undo_available() -> bool {
    MANAGER.with(|m| {
        m.borrow().as_ref().is_some_and(|m| {
            m.queued == 0 && (!m.layout.history.is_empty() || m.arranged.is_some())
        })
    })
}

unsafe fn open_settings_folder() {
    let path = MANAGER.with(|m| {
        m.borrow()
            .as_ref()
            .and_then(|manager| manager.path.parent().map(|path| path.to_path_buf()))
    });
    if let Some(path) = path {
        launch_path(&path);
    }
}

unsafe fn exit_app() {
    let controller = MANAGER.with(|m| m.borrow().as_ref().and_then(|manager| manager.controller));
    if let Some(hwnd) = controller {
        let _ = DestroyWindow(hwnd);
    } else {
        PostQuitMessage(0);
    }
}

fn point(lp: LPARAM) -> (i32, i32) {
    let v = lp.0 as u32;
    (
        ((v & 0xffff) as u16 as i16) as i32,
        ((v >> 16) as u16 as i16) as i32,
    )
}

unsafe extern "system" fn fence_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    if msg == WM_NCCREATE {
        let info = &*(lp.0 as *const CREATESTRUCTW);
        let pending = &mut *(info.lpCreateParams as *mut Option<Box<FenceWindow>>);
        let Some(state) = pending.take() else {
            return LRESULT(0);
        };
        SetWindowLongPtrW(hwnd, GWLP_USERDATA, Box::into_raw(state) as isize);
        return DefWindowProcW(hwnd, msg, wp, lp);
    }
    let ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut FenceWindow;
    if ptr.is_null() {
        return DefWindowProcW(hwnd, msg, wp, lp);
    }
    match msg {
        WM_CREATE => {
            DragAcceptFiles(hwnd, true);
            set_ime(hwnd, false);
            let drop_target: IDropTarget = FenceDropTarget {
                hwnd,
                accepts_files: Cell::new(false),
            }
            .into();
            if let Err(err) = RegisterDragDrop(hwnd, &drop_target) {
                eprintln!("Unable to accept OLE file drops: {err}");
            } else {
                (*ptr).drop_target = Some(drop_target);
            }
            let f = &(*ptr).fence;
            round_window(hwnd, f.width, f.window_height());
            LRESULT(0)
        }
        WM_ERASEBKGND => LRESULT(1),
        WM_INITMENUPOPUP | WM_DRAWITEM | WM_MEASUREITEM | WM_MENUCHAR => {
            forward_menu_message(msg, wp, lp).unwrap_or_else(|| DefWindowProcW(hwnd, msg, wp, lp))
        }
        WM_SETCURSOR
            if (lp.0 & 0xffff) as u32 == HTCLIENT
                && HWND(wp.0 as *mut c_void) == hwnd
                && set_frame_cursor(hwnd, ptr) =>
        {
            LRESULT(1)
        }
        WM_PAINT => {
            paint(hwnd, ptr);
            LRESULT(0)
        }
        WM_SIZE => {
            let (width, height) = point(lp);
            let window = &mut *ptr;
            if let Some(ref renderer) = window.renderer {
                if renderer
                    .resize(width.max(1) as u32, height.max(1) as u32)
                    .is_err()
                {
                    window.renderer = None;
                }
            }
            window.scroll = window.scroll.min(window.max_scroll());
            round_window(hwnd, width, height);
            if let Some(edit) = window.rename_edit {
                let _ = SetWindowPos(
                    edit,
                    None,
                    0,
                    0,
                    (width - 90).max(80),
                    30,
                    SWP_NOMOVE | SWP_NOZORDER | SWP_NOACTIVATE,
                );
            }
            LRESULT(0)
        }
        WM_COMMAND => {
            if wp.0 == CMD_AUTO_PREVIEW {
                request_organize_preview(hwnd);
                return LRESULT(0);
            }
            if wp.0 == CMD_ARRANGE {
                arrange_fences(false);
                return LRESULT(0);
            }
            if view_or_sort_command(hwnd, ptr, wp.0) {
                return LRESULT(0);
            }
            if wp.0 == CMD_DELETE_FILES || wp.0 == CMD_DELETE_PERMANENT {
                delete_selected(hwnd, ptr, wp.0 == CMD_DELETE_PERMANENT);
                return LRESULT(0);
            }
            if wp.0 == CMD_SELECT_ALL {
                select_all(&mut *ptr);
                let _ = InvalidateRect(Some(hwnd), None, false);
                return LRESULT(0);
            }

            if wp.0 == CMD_UNDO {
                request_undo(hwnd);
                return LRESULT(0);
            }
            if wp.0 == CMD_HISTORY {
                show_history(hwnd);
                return LRESULT(0);
            }
            if wp.0 == CMD_DESKTOP {
                enqueue(
                    hwnd,
                    Command::Desktop {
                        ids: selected_ids(&*ptr),
                    },
                );
                return LRESULT(0);
            }

            if wp.0 == CMD_COLLECT_DESKTOP {
                let paths = (*ptr).fence.paths.clone();
                collect_desktop_paths(hwnd, paths);
                return LRESULT(0);
            }
            if wp.0 == CMD_REMOVE_ITEM {
                if let Some(index) = (*ptr).selected {
                    remove_pinned(hwnd, ptr, index);
                }
                return LRESULT(0);
            }
            if wp.0 == CMD_FIND {
                start_search(hwnd, ptr);
                return LRESULT(0);
            }
            if wp.0 == CMD_PIN_CLIPBOARD {
                pin_from_clipboard(hwnd);
                return LRESULT(0);
            }
            if wp.0 == CMD_MOVE_UP || wp.0 == CMD_MOVE_DOWN {
                if let Some(index) = (*ptr).selected {
                    move_pinned(hwnd, ptr, index, wp.0 == CMD_MOVE_DOWN);
                }
                return LRESULT(0);
            }
            let notification = (wp.0 >> 16) as u32;
            if notification == EN_KILLFOCUS && Some(HWND(lp.0 as *mut c_void)) == (*ptr).rename_edit
            {
                finish_rename(hwnd, ptr, true);
            }
            LRESULT(0)
        }
        WM_LBUTTONDOWN => {
            mouse_down(hwnd, ptr, lp, wp.0);
            LRESULT(0)
        }
        WM_TIMER if wp.0 == ANIMATION_TIMER => {
            animate_height(hwnd, ptr);
            LRESULT(0)
        }
        WM_TIMER if wp.0 == ICON_TIMER => {
            // Polls background icon and file-detail loading for this group.
            let window = &mut *ptr;
            let waiting_details = window.waiting_for_details();
            if window.fence.sort != SortKey::Manual {
                window.rebuild_visible();
            }
            let mut missing_icons = false;
            if let Some(renderer) = &window.renderer {
                let visible = window.fence.visible_slots();
                renderer.load_one_visible_icon(
                    &window.items,
                    &window.visible_indices,
                    window.scroll,
                    visible,
                    crate::shell_icons::icon_class(window.fence.icon_pixels()),
                );
                missing_icons = renderer.has_missing_visible_icons(
                    &window.items,
                    &window.visible_indices,
                    window.scroll,
                    visible,
                    crate::shell_icons::icon_class(window.fence.icon_pixels()),
                );
            }
            let _ = InvalidateRect(Some(hwnd), None, false);
            if !missing_icons && !waiting_details {
                let _ = KillTimer(Some(hwnd), ICON_TIMER);
            }
            LRESULT(0)
        }
        WM_KEYDOWN if wp.0 == 0x71 => {
            // F2 renames the selected file, or the group when nothing is selected.
            if (*ptr).selected.is_some() {
                start_item_rename(hwnd, ptr);
            } else {
                start_rename(hwnd, ptr);
            }
            LRESULT(0)
        } // F2
        WM_KEYDOWN if wp.0 == 0x5A && GetKeyState(VK_CONTROL.0 as i32) < 0 => {
            request_undo(hwnd);
            LRESULT(0)
        }
        WM_KEYDOWN if wp.0 == 0x41 && GetKeyState(VK_CONTROL.0 as i32) < 0 => {
            select_all(&mut *ptr);
            let _ = InvalidateRect(Some(hwnd), None, false);
            LRESULT(0)
        }
        WM_KEYDOWN if wp.0 == 0x46 && GetKeyState(VK_CONTROL.0 as i32) < 0 => {
            start_search(hwnd, ptr);
            LRESULT(0)
        } // Ctrl+F
        WM_KEYDOWN if wp.0 == 0x56 && GetKeyState(VK_CONTROL.0 as i32) < 0 => {
            pin_from_clipboard(hwnd);
            LRESULT(0)
        } // Ctrl+V: pin copied files or folders
        WM_KEYDOWN if (wp.0 == 0x26 || wp.0 == 0x28) && GetKeyState(VK_CONTROL.0 as i32) < 0 => {
            if let Some(index) = (*ptr).selected {
                move_pinned(hwnd, ptr, index, wp.0 == 0x28);
            }
            LRESULT(0)
        } // Ctrl+Up / Ctrl+Down: reorder selected pin
        WM_KEYDOWN => {
            key_down(hwnd, ptr, wp.0);
            LRESULT(0)
        }
        WM_CHAR => {
            search_char(hwnd, ptr, wp.0);
            LRESULT(0)
        }
        WM_LBUTTONDBLCLK => {
            double_click(hwnd, ptr, lp);
            LRESULT(0)
        }
        WM_MOUSEMOVE => {
            mouse_move(hwnd, ptr, lp);
            LRESULT(0)
        }
        WM_MOUSELEAVE => {
            let w = &mut *ptr;
            w.pointer_inside = false;
            if !matches!(w.drag, DragMode::Reorder { .. }) {
                w.hover = None;
                let _ = InvalidateRect(Some(hwnd), None, false);
            }
            LRESULT(0)
        }
        WM_LBUTTONUP | WM_CAPTURECHANGED => {
            let previous = std::mem::replace(&mut (*ptr).drag, DragMode::None);
            (*ptr).snap = Snapped::default();
            if msg == WM_LBUTTONUP {
                if matches!(previous, DragMode::Reorder { active: true, .. }) {
                    let destination = drag_destination(hwnd, lp);
                    let ids = selected_ids(&*ptr);
                    match destination {
                        Destination::Fence { fence, before } => {
                            // A sorted group has no manual position to drop at.
                            let sorted = MANAGER.with(|m| {
                                m.borrow().as_ref().is_some_and(|m| {
                                    m.layout
                                        .fences
                                        .iter()
                                        .any(|f| f.id == fence && f.sort != SortKey::Manual)
                                })
                            });
                            if !(sorted && fence == (*ptr).fence.id) {
                                let before = if sorted { None } else { before };
                                enqueue(hwnd, Command::Transfer { ids, fence, before });
                            }
                        }
                        Destination::Desktop => {
                            enqueue(hwnd, Command::Desktop { ids });
                        }
                        Destination::Invalid => {}
                    }
                } else if matches!(previous, DragMode::Reorder { active: false, .. })
                    && (*ptr).collapse_on_release
                {
                    if let Some(index) = (*ptr).selected {
                        select_one(&mut *ptr, index);
                    }
                } else if matches!(
                    previous,
                    DragMode::Move { .. } | DragMode::Resize { .. } | DragMode::Column { .. }
                ) {
                    write_back(&(*ptr).fence);
                }
                if !matches!(previous, DragMode::None) {
                    let _ = ReleaseCapture();
                }
            }
            clear_drag_feedback();
            let _ = InvalidateRect(Some(hwnd), None, false);
            LRESULT(0)
        }
        WM_RBUTTONUP => {
            let (x, y) = point(lp);
            if in_column_titles(&(*ptr).fence, y) {
                column_menu(hwnd, ptr);
                return LRESULT(0);
            }
            let index = item_at(&*ptr, x, y);
            popup_menu(hwnd, ptr, index);
            LRESULT(0)
        }
        WM_MOUSEWHEEL => {
            let delta = (wp.0 >> 16) as u16 as i16;
            if GetKeyState(VK_CONTROL.0 as i32) < 0 {
                // Ctrl+wheel: item size, like Explorer.
                let level = (*ptr).fence.size_level() as i32 + if delta > 0 { 1 } else { -1 };
                set_size(
                    hwnd,
                    ptr,
                    level.clamp(0, crate::model::SIZES.len() as i32 - 1) as usize,
                );
                return LRESULT(0);
            }
            let window = &mut *ptr;
            let amount = (delta.unsigned_abs() as usize / 120).max(1) * window.fence.columns();
            if delta < 0 {
                window.scroll = window
                    .scroll
                    .saturating_add(amount)
                    .min(window.max_scroll());
            }
            if delta > 0 {
                window.scroll = window.scroll.saturating_sub(amount);
            }
            let _ = InvalidateRect(Some(hwnd), None, false);
            LRESULT(0)
        }
        WM_DROPFILES => {
            handle_drop(hwnd, ptr, wp);
            LRESULT(0)
        }
        WM_NCDESTROY => {
            let _ = KillTimer(Some(hwnd), ANIMATION_TIMER);
            let _ = KillTimer(Some(hwnd), ICON_TIMER);
            if (*ptr).drop_target.is_some() {
                let _ = RevokeDragDrop(hwnd);
            }
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
            drop(Box::from_raw(ptr));
            DefWindowProcW(hwnd, msg, wp, lp)
        }
        _ => DefWindowProcW(hwnd, msg, wp, lp),
    }
}

unsafe fn paint(hwnd: HWND, ptr: *mut FenceWindow) {
    let mut ps = PAINTSTRUCT::default();
    let _ = BeginPaint(hwnd, &mut ps);
    let window = &mut *ptr;
    let mut client = RECT::default();
    let _ = GetClientRect(hwnd, &mut client);
    let view_height = (client.bottom - client.top).max(1);
    if window.renderer.is_none() {
        match Renderer::new(hwnd, window.fence.width as u32, view_height as u32) {
            Ok(renderer) => window.renderer = Some(renderer),
            Err(err) => {
                if !window.render_failure_reported {
                    debug_log(&format!("Renderer initialization failed: {err}"));
                    window.render_failure_reported = true;
                }
            }
        }
    }
    let mut screen = RECT::default();
    let _ = GetWindowRect(hwnd, &mut screen);
    let backdrop = window
        .renderer
        .as_ref()
        .and_then(|renderer| renderer.backdrop_for(screen));
    if let Some(ref renderer) = window.renderer {
        let status = MANAGER.with(|m| {
            m.borrow()
                .as_ref()
                .map(|m| {
                    if m.queued > 0 {
                        m.status.clone()
                    } else {
                        String::new()
                    }
                })
                .unwrap_or_default()
        });
        let show_body = !window.fence.collapsed || window.animation.is_some();
        if let Err(err) = renderer.draw(
            &window.fence,
            &window.items,
            &window.visible_indices,
            window.search_active.then_some(window.search_text.as_str()),
            window.theme,
            window.hover,
            &window.selection,
            if window.drag_text.is_empty() {
                &status
            } else {
                &window.drag_text
            },
            window.scroll,
            view_height,
            show_body,
            window.drop_hover,
            matches!(window.drag, DragMode::Reorder { active: true, .. }),
            window.snap,
            window.pointer_inside,
            backdrop.as_ref().map(|(bitmap, source)| (bitmap, *source)),
        ) {
            debug_log(&format!("Renderer lost target: {err}"));
            window.renderer = None; // Direct2D device loss, lazily recreated.
            let _ = InvalidateRect(Some(hwnd), None, false);
        }
    }
    if let Some(renderer) = &window.renderer {
        if !window.fence.collapsed
            && (renderer.has_missing_visible_icons(
                &window.items,
                &window.visible_indices,
                window.scroll,
                window.fence.visible_slots(),
                crate::shell_icons::icon_class(window.fence.icon_pixels()),
            ) || window.waiting_for_details())
        {
            let _ = SetTimer(Some(hwnd), ICON_TIMER, 25, None);
        }
    }
    let _ = EndPaint(hwnd, &ps);
}

#[cfg(debug_assertions)]
fn debug_log(message: &str) {
    use std::io::Write;
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .append(true)
        .create(true)
        .open(config_path().with_file_name("diagnostics.log"))
    {
        let _ = writeln!(file, "{message}");
    }
}

#[cfg(not(debug_assertions))]
fn debug_log(_: &str) {}

/// The item (index into the collection) under a client point, in either view.
fn item_at(w: &FenceWindow, x: i32, y: i32) -> Option<usize> {
    if w.animation.is_some() {
        return None;
    }
    let slot = w.fence.slot_at(x, y)? + w.scroll;
    w.visible_indices.get(slot).copied()
}

/// Keyboard navigation over visible positions; `columns` is 1 in list view.
fn next_selection(
    current: Option<usize>,
    count: usize,
    key: usize,
    columns: usize,
) -> Option<usize> {
    if count == 0 {
        return None;
    }
    let last = count - 1;
    match (key, current) {
        (0x24, _) => Some(0),                                                // Home
        (0x23, _) => Some(last),                                             // End
        (0x25..=0x28, None) => Some(0),                                      // first arrow
        (0x25, Some(i)) => Some(i.saturating_sub(1)),                        // Left
        (0x27, Some(i)) => Some((i + 1).min(last)),                          // Right
        (0x26, Some(i)) => Some(if i >= columns { i - columns } else { i }), // Up
        (0x28, Some(i)) => Some(if i + columns <= last { i + columns } else { i }), // Down
        _ => current,
    }
}

unsafe fn remove_pinned(hwnd: HWND, ptr: *mut FenceWindow, _index: usize) {
    let ids = selected_ids(&*ptr);
    if !ids.is_empty() {
        enqueue(hwnd, Command::Desktop { ids });
    }
}

unsafe fn move_pinned(hwnd: HWND, ptr: *mut FenceWindow, _index: usize, down: bool) {
    let w = &*ptr;
    let selected: Vec<_> = w
        .visible_indices
        .iter()
        .enumerate()
        .filter(|(_, i)| {
            w.fence
                .entries
                .get(**i)
                .is_some_and(|e| w.selection.contains(&e.id))
        })
        .map(|(slot, _)| slot)
        .collect();
    let (Some(&first), Some(&last)) = (selected.first(), selected.last()) else {
        return;
    };
    let before = if down {
        if last + 1 >= w.visible_indices.len() {
            return;
        }
        w.visible_indices
            .get(last + 2)
            .and_then(|&i| w.fence.entries.get(i))
            .map(|e| e.id.clone())
    } else {
        if first == 0 {
            return;
        }
        w.fence
            .entries
            .get(w.visible_indices[first - 1])
            .map(|e| e.id.clone())
    };
    enqueue(
        hwnd,
        Command::Transfer {
            ids: selected_ids(w),
            fence: w.fence.id,
            before,
        },
    );
}

unsafe fn key_down(hwnd: HWND, ptr: *mut FenceWindow, key: usize) {
    if key == 0x1B {
        {
            let w = &mut *ptr;
            w.drag = DragMode::None;
            w.selection.clear();
            w.anchor = None;
            w.selected = None;
            w.search_active = false;
            set_ime(hwnd, false);
            w.search_text.clear();
            w.rebuild_visible();
            w.scroll = 0;
        }
        let _ = ReleaseCapture();
        clear_drag_feedback();
        let _ = InvalidateRect(Some(hwnd), None, false);
        return;
    }
    let w = &mut *ptr;
    if w.fence.collapsed || w.animation.is_some() {
        return;
    }
    match key {
        0x23..=0x28 => {
            let current = w
                .selected
                .and_then(|selected| w.visible_indices.iter().position(|&i| i == selected));
            let next = next_selection(current, w.visible_indices.len(), key, w.fence.columns());
            w.selected = next.and_then(|slot| w.visible_indices.get(slot).copied());
            if let Some(index) = w.selected {
                select_one(w, index);
            }
            if let Some(index) = next {
                w.scroll = w.fence.scroll_to_show(w.scroll, index);
            }
        }
        0x0D => {
            if let Some(index) = w.selected {
                if let Some(path) = w.fence.paths.get(index) {
                    launch_path(path);
                }
            }
        }
        0x2E => {
            // Like Explorer: Delete recycles, Shift+Delete deletes permanently.
            if w.selected.is_some() {
                delete_selected(hwnd, ptr, GetKeyState(VK_SHIFT.0 as i32) < 0);
            }
        }
        _ => return,
    }
    let _ = InvalidateRect(Some(hwnd), None, false);
}

unsafe fn start_search(hwnd: HWND, ptr: *mut FenceWindow) {
    let w = &mut *ptr;
    if w.fence.collapsed || w.animation.is_some() {
        return;
    }
    w.search_active = true;
    w.search_text.clear();
    w.rebuild_visible();
    w.selected = None;
    w.selection.clear();
    w.anchor = None;
    w.scroll = 0;
    w.hover = None;
    let _ = SetFocus(Some(hwnd));
    let _ = InvalidateRect(Some(hwnd), None, false);
}

unsafe fn search_char(hwnd: HWND, ptr: *mut FenceWindow, code: usize) {
    let w = &mut *ptr;
    if w.fence.collapsed || w.animation.is_some() {
        return;
    }
    let Some(c) = char::from_u32(code as u32) else {
        return;
    };
    if !w.search_active {
        type_to_select(hwnd, w, c);
        return;
    }
    match c {
        '\u{8}' => {
            if w.search_text.pop().is_none() {
                return;
            }
        }
        _ if !c.is_control() && w.search_text.chars().count() < 64 => {
            w.search_text.push(c);
        }
        _ => return,
    }
    w.rebuild_visible();
    w.scroll = 0;
    w.selected = None;
    w.selection.clear();
    w.anchor = None;
    w.hover = None;
    let _ = InvalidateRect(Some(hwnd), None, false);
}

/// What an item is ordered by; folders come first, as in Explorer.
#[derive(Clone, Debug, PartialEq, PartialOrd)]
struct SortValue {
    folder_last: bool,
    key: SortField,
    name: String,
}

#[derive(Clone, Debug, PartialEq, PartialOrd)]
enum SortField {
    None,
    Text(String),
    Number(u64),
}

fn sort_value(sort: SortKey, path: &Path, item: &ItemDisplay) -> SortValue {
    let meta = (!matches!(sort, SortKey::Manual | SortKey::Name))
        .then(|| crate::meta::get(path))
        .flatten();
    let key = match sort {
        SortKey::Manual | SortKey::Name => SortField::None,
        SortKey::Modified => meta
            .and_then(|m| m.modified)
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map_or(SortField::None, |d| SortField::Number(d.as_secs())),
        SortKey::Type => meta.map_or(SortField::None, |m| {
            SortField::Text(m.type_name.to_lowercase())
        }),
        SortKey::Size => meta
            .and_then(|m| m.size)
            .map_or(SortField::None, SortField::Number),
    };
    SortValue {
        folder_last: !item.is_folder(),
        key,
        name: item.name().to_lowercase(),
    }
}

/// Orders visible positions by `keys` (indexed by item). Manual keeps the
/// stored order; descending reverses the key but keeps folders first.
fn sort_visible(visible: &mut [usize], keys: &[SortValue], sort: SortKey, desc: bool) {
    if sort == SortKey::Manual {
        return;
    }
    visible.sort_by(|&a, &b| {
        let (ka, kb) = (&keys[a], &keys[b]);
        let order = (&ka.key, &ka.name)
            .partial_cmp(&(&kb.key, &kb.name))
            .unwrap_or(std::cmp::Ordering::Equal);
        ka.folder_last
            .cmp(&kb.folder_last)
            .then(if desc { order.reverse() } else { order })
    });
}

/// Explorer resets type-ahead after a pause of about a second.
const TYPEAHEAD_TIMEOUT: Duration = Duration::from_millis(1000);

/// The visible position to select for a type-ahead `prefix`. A new search
/// starts after the current item; repeating one letter cycles its matches.
fn typeahead_match(names: &[&str], current: Option<usize>, prefix: &str) -> Option<usize> {
    let count = names.len();
    let mut chars = prefix.chars();
    let first = chars.next()?;
    let repeated = prefix.chars().count() > 1 && chars.all(|c| c == first);
    let (needle, start) = if repeated || prefix.chars().count() == 1 {
        (
            first.to_lowercase().collect::<String>(),
            current.map_or(0, |i| i + 1),
        )
    } else {
        (prefix.to_lowercase(), current.unwrap_or(0))
    };
    (0..count)
        .map(|k| (start + k) % count)
        .find(|&i| names[i].to_lowercase().starts_with(&needle))
}

unsafe fn type_to_select(hwnd: HWND, w: &mut FenceWindow, c: char) {
    if c.is_control() {
        return;
    }
    if w.typeahead_at
        .is_none_or(|at| at.elapsed() > TYPEAHEAD_TIMEOUT)
    {
        w.typeahead.clear();
    }
    w.typeahead.push(c);
    w.typeahead_at = Some(Instant::now());
    let names: Vec<&str> = w
        .visible_indices
        .iter()
        .filter_map(|&i| w.items.get(i).map(|item| item.name()))
        .collect();
    let current = w
        .selected
        .and_then(|selected| w.visible_indices.iter().position(|&i| i == selected));
    if let Some(slot) = typeahead_match(&names, current, &w.typeahead) {
        if let Some(&index) = w.visible_indices.get(slot) {
            select_one(w, index);
            w.selected = Some(index);
            w.scroll = w.fence.scroll_to_show(w.scroll, slot);
            let _ = InvalidateRect(Some(hwnd), None, false);
        }
    }
}

/// The IME turns letters into VK_PROCESSKEY (even in English mode), so a
/// group only owns an input context while its search box is active.
unsafe fn set_ime(hwnd: HWND, enabled: bool) {
    use windows::Win32::UI::Input::Ime::{ImmAssociateContextEx, HIMC, IACE_DEFAULT};
    let _ = if enabled {
        ImmAssociateContextEx(hwnd, HIMC::default(), IACE_DEFAULT)
    } else {
        ImmAssociateContextEx(hwnd, HIMC::default(), 0)
    };
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum FrameHit {
    Menu,
    Collapse,
    Resize { horizontal: bool, vertical: bool },
    Move,
    Body,
}

const RESIZE_EDGE: i32 = 8;
const RESIZE_CORNER: i32 = 24;

/// Classifies a client point; shared by mouse-down and cursor feedback so the
/// cursor always shows what a press will do.
fn frame_hit(x: i32, y: i32, width: i32, height: i32, resizable: bool) -> FrameHit {
    if y <= HEADER_HEIGHT && x >= width - 38 {
        return FrameHit::Menu;
    }
    if y <= HEADER_HEIGHT && x >= width - 70 {
        return FrameHit::Collapse;
    }
    if resizable {
        let corner = x > width - RESIZE_CORNER && y > height - RESIZE_CORNER;
        let right = x >= width - RESIZE_EDGE && y > HEADER_HEIGHT;
        let bottom = y >= height - RESIZE_EDGE;
        if corner || right || bottom {
            return FrameHit::Resize {
                horizontal: corner || right,
                vertical: corner || bottom,
            };
        }
    }
    if y < HEADER_HEIGHT {
        FrameHit::Move
    } else {
        FrameHit::Body
    }
}

unsafe fn set_frame_cursor(hwnd: HWND, ptr: *mut FenceWindow) -> bool {
    let mut cursor = POINT::default();
    let _ = GetCursorPos(&mut cursor);
    let _ = windows::Win32::Graphics::Gdi::ScreenToClient(hwnd, &mut cursor);
    let mut rect = RECT::default();
    let _ = GetClientRect(hwnd, &mut rect);
    if divider_at(&(*ptr).fence, cursor.x, cursor.y).is_some() {
        if let Ok(cursor) = LoadCursorW(None, IDC_SIZEWE) {
            SetCursor(Some(cursor));
        }
        return true;
    }
    let w = &*ptr;
    let resizable = !w.fence.collapsed && w.animation.is_none();
    let shape = match frame_hit(cursor.x, cursor.y, w.fence.width, rect.bottom, resizable) {
        FrameHit::Resize {
            horizontal: true,
            vertical: true,
        } => IDC_SIZENWSE,
        FrameHit::Resize {
            horizontal: true, ..
        } => IDC_SIZEWE,
        FrameHit::Resize { .. } => IDC_SIZENS,
        FrameHit::Move => IDC_SIZEALL,
        _ => return false,
    };
    if let Ok(cursor) = LoadCursorW(None, shape) {
        SetCursor(Some(cursor));
    }
    true
}

unsafe fn mouse_down(hwnd: HWND, ptr: *mut FenceWindow, lp: LPARAM, modifiers: usize) {
    let (x, y) = point(lp);
    let _ = SetFocus(Some(hwnd));
    let w = &mut *ptr;
    let width = w.fence.width;
    let mut rect = RECT::default();
    let _ = GetClientRect(hwnd, &mut rect);
    let height = rect.bottom - rect.top;
    let resizable = !w.fence.collapsed && w.animation.is_none();
    let hit = frame_hit(x, y, width, height, resizable);
    if hit == FrameHit::Menu {
        popup_menu(hwnd, ptr, None);
    } else if hit == FrameHit::Collapse {
        toggle_collapse(hwnd, ptr);
    } else if let FrameHit::Resize {
        horizontal,
        vertical,
    } = hit
    {
        let mut cursor = POINT::default();
        let _ = GetCursorPos(&mut cursor);
        w.drag = DragMode::Resize {
            cursor,
            width: w.fence.width,
            height: w.fence.height,
            horizontal,
            vertical,
        };
        let _ = SetCapture(hwnd);
    } else if hit == FrameHit::Move {
        let mut cursor = POINT::default();
        let _ = GetCursorPos(&mut cursor);
        w.drag = DragMode::Move {
            cursor,
            x: w.fence.x,
            y: w.fence.y,
        };
        let _ = SetCapture(hwnd);
    } else if let Some(index) = divider_at(&w.fence, x, y) {
        w.drag = DragMode::Column {
            index,
            start_x: x,
            start_width: w.fence.detail_settings()[index].width,
        };
        let _ = SetCapture(hwnd);
    } else if let Some(key) = column_at(&w.fence, x, y) {
        sort_by_column(hwnd, ptr, key);
    } else if let Some(index) = item_at(w, x, y) {
        let control = modifiers & 8 != 0;
        let shift = modifiers & 4 != 0;
        let id = w.fence.entries.get(index).map(|e| e.id.clone());
        w.collapse_on_release = false;
        if shift {
            let anchor = w.anchor.as_ref().and_then(|id| {
                w.visible_indices
                    .iter()
                    .position(|&i| w.fence.entries.get(i).is_some_and(|e| &e.id == id))
            });
            let end = w.visible_indices.iter().position(|&i| i == index);
            if !control {
                w.selection.clear();
            }
            if let (Some(a), Some(b)) = (anchor, end) {
                for &i in &w.visible_indices[a.min(b)..=a.max(b)] {
                    if let Some(e) = w.fence.entries.get(i) {
                        w.selection.insert(e.id.clone());
                    }
                }
            } else {
                select_one(w, index);
            }
        } else if control {
            if let Some(id) = id {
                if !w.selection.remove(&id) {
                    w.selection.insert(id.clone());
                }
                w.anchor = Some(id);
            }
        } else if id.as_ref().is_some_and(|id| w.selection.contains(id)) {
            w.collapse_on_release = true;
        } else {
            select_one(w, index);
        }
        w.selected = Some(index);
        w.drag = DragMode::Reorder {
            source_index: index,
            start: POINT { x, y },
            active: false,
        };
        let _ = SetCapture(hwnd);
        let _ = InvalidateRect(Some(hwnd), None, false);
    }
}

/// Other collections' bounds and the monitor work areas, in layout
/// coordinates, for snapping. Holding Alt disables snapping.
unsafe fn snap_context(hwnd: HWND, own: u64) -> Option<(Vec<Bounds>, Vec<Area>)> {
    if GetKeyState(VK_MENU.0 as i32) < 0 {
        return None;
    }
    let parent = GetParent(hwnd).ok()?;
    let origin = desktop_origin(parent);
    let areas = monitor_regions()
        .into_iter()
        .map(|region| client_area(region, origin))
        .collect();
    let others = MANAGER.with(|m| {
        m.borrow()
            .as_ref()
            .map(|m| {
                m.layout
                    .fences
                    .iter()
                    .filter(|f| f.id != own && m.handles.contains_key(&f.id))
                    .map(|f| (f.x, f.y, f.width, f.window_height()))
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default()
    });
    Some((others, areas))
}

unsafe fn mouse_move(hwnd: HWND, ptr: *mut FenceWindow, lp: LPARAM) {
    let (x, y) = point(lp);
    let w = &mut *ptr;
    let mut tracking = TRACKMOUSEEVENT {
        cbSize: std::mem::size_of::<TRACKMOUSEEVENT>() as u32,
        dwFlags: TME_LEAVE,
        hwndTrack: hwnd,
        ..Default::default()
    };
    let _ = TrackMouseEvent(&mut tracking);
    if !w.pointer_inside {
        w.pointer_inside = true;
        let _ = InvalidateRect(Some(hwnd), None, false);
    }
    let mut cursor = POINT::default();
    let _ = GetCursorPos(&mut cursor);
    let reposition = match w.drag {
        DragMode::Move {
            cursor: from,
            x: start_x,
            y: start_y,
        } => {
            // Virtual desktop coordinates can be negative on left/top monitors.
            w.fence.x = start_x.saturating_add(cursor.x - from.x);
            w.fence.y = start_y.saturating_add(cursor.y - from.y);
            w.snap = Snapped::default();
            if let Some((others, areas)) = snap_context(hwnd, w.fence.id) {
                let (x, y, snapped) = crate::model::snap_move(
                    (w.fence.x, w.fence.y, w.fence.width, w.fence.window_height()),
                    &others,
                    &areas,
                );
                (w.fence.x, w.fence.y, w.snap) = (x, y, snapped);
            }
            Some((w.fence.x, w.fence.y, w.fence.width, w.fence.window_height()))
        }
        DragMode::Resize {
            cursor: from,
            width,
            height,
            horizontal,
            vertical,
        } => {
            if horizontal {
                w.fence.width = (width + cursor.x - from.x).clamp(MIN_WIDTH, 1000);
            }
            if vertical {
                w.fence.height = (height + cursor.y - from.y).clamp(MIN_HEIGHT, 1000);
            }
            w.snap = Snapped::default();
            if let Some((others, areas)) = snap_context(hwnd, w.fence.id) {
                let (nw, nh, snapped) = crate::model::snap_resize(
                    (w.fence.x, w.fence.y, w.fence.width, w.fence.height),
                    &others,
                    &areas,
                    horizontal,
                    vertical,
                );
                w.fence.width = nw.clamp(MIN_WIDTH, 1000);
                w.fence.height = nh.clamp(MIN_HEIGHT, 1000);
                w.snap = snapped;
            }
            w.scroll = w.scroll.min(w.max_scroll());
            Some((w.fence.x, w.fence.y, w.fence.width, w.fence.height))
        }
        DragMode::Column {
            index,
            start_x,
            start_width,
        } => {
            // Columns are right-anchored: dragging a divider left widens
            // the column to its right; the name column takes the rest.
            let mut settings = w.fence.detail_settings();
            settings[index].width = (start_width - (x - start_x))
                .clamp(crate::model::MIN_COLUMN, crate::model::MAX_COLUMN);
            settings[index].visible = true;
            w.fence.details = Some(settings);
            let _ = InvalidateRect(Some(hwnd), None, false);
            None
        }
        DragMode::Reorder {
            source_index,
            start,
            active,
        } => {
            let active = active || (x - start.x).abs() + (y - start.y).abs() >= 8;
            w.drag = DragMode::Reorder {
                source_index,
                start,
                active,
            };
            let hovered = if active { item_at(w, x, y) } else { None };
            if hovered != w.hover {
                w.hover = hovered;
                let _ = InvalidateRect(Some(hwnd), None, false);
            }
            None
        }
        DragMode::None => {
            let hovered = item_at(w, x, y);
            if hovered != w.hover {
                w.hover = hovered;
                let _ = InvalidateRect(Some(hwnd), None, false);
            }
            None
        }
    };
    if matches!((*ptr).drag, DragMode::Reorder { active: true, .. }) {
        update_drag_feedback(hwnd, ptr, lp);
    }
    if let Some((x, y, width, height)) = reposition {
        let _ = SetWindowPos(
            hwnd,
            None,
            x,
            y,
            width,
            height,
            SWP_NOACTIVATE | SWP_NOZORDER,
        );
        let _ = InvalidateRect(Some(hwnd), None, false);
    }
}

unsafe fn double_click(hwnd: HWND, ptr: *mut FenceWindow, lp: LPARAM) {
    let (x, y) = point(lp);
    if let Some(index) = divider_at(&(*ptr).fence, x, y) {
        auto_fit_column(hwnd, ptr, index);
        return;
    }
    if y < HEADER_HEIGHT && x < (*ptr).fence.width - 72 {
        toggle_collapse(hwnd, ptr);
    } else if let Some(index) = item_at(&*ptr, x, y) {
        let window = &*ptr;
        if let Some(path) = window.fence.paths.get(index) {
            launch_path(path);
        }
    }
}

unsafe fn handle_drop(hwnd: HWND, ptr: *mut FenceWindow, wp: WPARAM) {
    let drop = HDROP(wp.0 as *mut c_void);
    let incoming = paths_from_drop(drop);
    DragFinish(drop);
    if all_desktop_items(&incoming) {
        collect_desktop_paths(hwnd, incoming);
    } else {
        pin_paths_with_state(hwnd, ptr, incoming);
    }
}

unsafe fn paths_from_drop(drop: HDROP) -> Vec<PathBuf> {
    let count = DragQueryFileW(drop, u32::MAX, None);
    let mut incoming = Vec::new();
    for i in 0..count {
        let len = DragQueryFileW(drop, i, None);
        if len == 0 {
            continue;
        }
        let mut buffer = vec![0u16; len as usize + 1];
        let actual = DragQueryFileW(drop, i, Some(&mut buffer)) as usize;
        incoming.push(PathBuf::from(OsString::from_wide(&buffer[..actual])));
    }
    incoming
}

unsafe fn pin_paths(hwnd: HWND, incoming: Vec<PathBuf>) -> usize {
    let ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut FenceWindow;
    if !ptr.is_null() {
        pin_paths_with_state(hwnd, ptr, incoming)
    } else {
        0
    }
}

unsafe fn pin_from_clipboard(hwnd: HWND) {
    if OpenClipboard(Some(hwnd)).is_err() {
        let _ = MessageBoxW(
            dialog_owner(hwnd),
            i18n::w(K::ClipboardBusy),
            w!("LumaDesk"),
            MB_OK | MB_ICONERROR,
        );
        return;
    }
    let files = GetClipboardData(CF_HDROP.0 as u32)
        .map(|handle| paths_from_drop(HDROP(handle.0)))
        .unwrap_or_default();
    let _ = CloseClipboard();
    if files.is_empty() {
        let _ = MessageBoxW(
            dialog_owner(hwnd),
            i18n::w(K::ClipboardEmpty),
            w!("LumaDesk"),
            MB_OK,
        );
        return;
    }
    pin_paths(hwnd, files);
}

unsafe fn collection_error(hwnd: HWND, message: &str) {
    let text: Vec<u16> = message.encode_utf16().chain(Some(0)).collect();
    let _ = MessageBoxW(
        dialog_owner(hwnd),
        PCWSTR(text.as_ptr()),
        i18n::w(K::ErrorTitle),
        MB_OK | MB_ICONERROR,
    );
}

unsafe fn refresh_collections_after_move(owner: HWND) {
    let loaded = {
        let _lock = crate::operations::CONFIG_LOCK.lock().unwrap();
        Layout::load(&config_path())
    };
    let layout = match loaded {
        Ok(l) => l,
        Err(e) => {
            collection_error(owner, &e.to_string());
            return;
        }
    };
    let windows = MANAGER.with(|m| {
        let mut guard = m.borrow_mut();
        let manager = guard.as_mut().expect("initialized");
        manager.layout = layout;
        manager
            .layout
            .fences
            .iter()
            .filter_map(|f| manager.handles.get(&f.id).map(|&h| (h, f.clone())))
            .collect::<Vec<_>>()
    });
    let missing = MANAGER.with(|m| {
        m.borrow().as_ref().is_some_and(|m| {
            m.layout
                .fences
                .iter()
                .any(|f| !m.handles.contains_key(&f.id))
                || m.handles
                    .keys()
                    .any(|id| !m.layout.fences.iter().any(|f| &f.id == id))
        })
    });
    if missing {
        ensure_fences();
    }
    for (hwnd, fence) in windows {
        let ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut FenceWindow;
        if ptr.is_null() {
            continue;
        }
        let w = &mut *ptr;
        w.fence = fence;
        let title: Vec<u16> = w.fence.title.encode_utf16().chain(Some(0)).collect();
        let _ = SetWindowTextW(hwnd, PCWSTR(title.as_ptr()));
        w.items = w
            .fence
            .paths
            .iter()
            .map(|p| ItemDisplay::from_path(p))
            .collect();
        w.selection
            .retain(|id| w.fence.entries.iter().any(|e| &e.id == id));
        w.selected = w
            .fence
            .entries
            .iter()
            .position(|e| w.selection.contains(&e.id));
        w.rebuild_visible();
        let _ = InvalidateRect(Some(hwnd), None, false);
    }
}

unsafe fn collect_desktop_paths(hwnd: HWND, paths: Vec<PathBuf>) -> usize {
    let ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut FenceWindow;
    if ptr.is_null() {
        return 0;
    }
    let desktops = crate::collection::desktop_paths().unwrap_or_default();
    let paths: Vec<_> = paths
        .into_iter()
        .filter(|p| crate::collection::is_desktop_item(p, &desktops))
        .filter(|p| {
            !(*ptr)
                .fence
                .paths
                .iter()
                .any(|existing| crate::operations::same_path(existing, p))
        })
        .collect();
    if paths.is_empty() {
        sync_desktop_icons();
        return 0;
    }
    let w = &mut *ptr;
    w.search_active = false;
    set_ime(hwnd, false);
    w.search_text.clear();
    w.selection.clear();
    w.selected = None;
    w.rebuild_visible();
    if w.fence.collapsed {
        toggle_collapse(hwnd, ptr);
    }
    let count = paths.len();
    if count > 0
        && enqueue(
            hwnd,
            Command::Pin {
                fence: (*ptr).fence.id,
                paths,
            },
        )
    {
        count
    } else {
        0
    }
}

unsafe fn collect_existing_desktop(_owner: HWND) {
    let windows = MANAGER.with(|m| {
        m.borrow()
            .as_ref()
            .map(|m| {
                m.layout
                    .fences
                    .iter()
                    .filter_map(|f| m.handles.get(&f.id).map(|&h| (h, f.paths.clone())))
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default()
    });
    for (hwnd, paths) in windows {
        collect_desktop_paths(hwnd, paths);
    }
}

unsafe fn pin_paths_with_state(hwnd: HWND, ptr: *mut FenceWindow, incoming: Vec<PathBuf>) -> usize {
    let count = incoming.len();
    if count == 0 {
        return 0;
    }
    let w = &mut *ptr;
    w.search_text.clear();
    w.search_active = false;
    set_ime(hwnd, false);
    w.selection.clear();
    w.selected = None;
    w.rebuild_visible();
    let fence = w.fence.id;
    let collapsed = w.fence.collapsed;
    if collapsed {
        toggle_collapse(hwnd, ptr);
    }
    if enqueue(
        hwnd,
        Command::Pin {
            fence,
            paths: incoming,
        },
    ) {
        count
    } else {
        0
    }
}

/// 「檢視」 and 「排序方式」 submenus (radio-checked current choice).
unsafe fn append_view_menus(menu: HMENU, fence: &Fence) {
    if let Ok(views) = CreatePopupMenu() {
        let labels = [
            i18n::t(K::ViewLarge),
            i18n::t(K::ViewSmall),
            i18n::t(K::ViewList),
            i18n::t(K::ViewDetails),
        ];
        for ((command, mode), label) in VIEWS.iter().zip(labels) {
            let text: Vec<u16> = label.encode_utf16().chain(Some(0)).collect();
            let checked = if fence.mode() == *mode {
                MF_CHECKED
            } else {
                MF_UNCHECKED
            };
            let _ = AppendMenuW(views, MF_STRING | checked, *command, PCWSTR(text.as_ptr()));
        }
        let _ = AppendMenuW(views, MF_SEPARATOR, 0, None);
        let _ = AppendMenuW(views, MF_STRING | MF_GRAYED, 0, i18n::w(K::SizeHeader));
        for (level, label) in SIZE_LABELS.iter().enumerate() {
            let text: Vec<u16> = i18n::t(*label).encode_utf16().chain(Some(0)).collect();
            let checked = if fence.size_level() == level {
                MF_CHECKED
            } else {
                MF_UNCHECKED
            };
            let _ = AppendMenuW(
                views,
                MF_STRING | checked,
                CMD_SIZE_BASE + level,
                PCWSTR(text.as_ptr()),
            );
        }
        let _ = AppendMenuW(menu, MF_POPUP, views.0 as usize, i18n::w(K::View));
    }
    if let Ok(sorts) = CreatePopupMenu() {
        let labels = [
            i18n::t(K::SortManual),
            i18n::t(K::ColName),
            i18n::t(K::ColModified),
            i18n::t(K::ColType),
            i18n::t(K::ColSize),
        ];
        for (index, (key, label)) in SORT_KEYS.iter().zip(labels).enumerate() {
            let text: Vec<u16> = label.encode_utf16().chain(Some(0)).collect();
            let checked = if fence.sort == *key {
                MF_CHECKED
            } else {
                MF_UNCHECKED
            };
            let _ = AppendMenuW(
                sorts,
                MF_STRING | checked,
                CMD_SORT_BASE + index,
                PCWSTR(text.as_ptr()),
            );
        }
        let _ = AppendMenuW(sorts, MF_SEPARATOR, 0, None);
        let desc = if fence.sort_desc {
            MF_CHECKED
        } else {
            MF_UNCHECKED
        };
        let grayed = if fence.sort == SortKey::Manual {
            MF_GRAYED
        } else {
            MF_ENABLED
        };
        let _ = AppendMenuW(
            sorts,
            MF_STRING | desc | grayed,
            CMD_SORT_DESC,
            i18n::w(K::Descending),
        );
        let _ = AppendMenuW(menu, MF_POPUP, sorts.0 as usize, i18n::w(K::SortBy));
    }
}

/// Handles a view or sort command; false when `command` is neither.
unsafe fn view_or_sort_command(hwnd: HWND, ptr: *mut FenceWindow, command: usize) -> bool {
    if (CMD_SIZE_BASE..CMD_SIZE_BASE + SIZE_LABELS.len()).contains(&command) {
        set_size(hwnd, ptr, command - CMD_SIZE_BASE);
        return true;
    }
    if let Some(&(_, mode)) = VIEWS.iter().find(|(c, _)| *c == command) {
        set_view(hwnd, ptr, mode);
        return true;
    }
    let w = &mut *ptr;
    if (CMD_SORT_BASE..CMD_SORT_BASE + SORT_KEYS.len()).contains(&command) {
        w.fence.sort = SORT_KEYS[command - CMD_SORT_BASE];
        if w.fence.sort == SortKey::Manual {
            w.fence.sort_desc = false;
        }
    } else if command == CMD_SORT_DESC && w.fence.sort != SortKey::Manual {
        w.fence.sort_desc = !w.fence.sort_desc;
    } else {
        return false;
    }
    apply_order(hwnd, ptr);
    true
}

/// Whether a client point is in the details column-title row.
fn in_column_titles(fence: &Fence, y: i32) -> bool {
    fence.mode() == ViewMode::Details
        && !fence.collapsed
        && (crate::model::COLUMNS_TOP..crate::model::DETAILS_TOP - 2).contains(&y)
}

/// The divider (left edge of 修改日期 / 類型 / 大小 = 0 / 1 / 2) under a point.
fn divider_at(fence: &Fence, x: i32, y: i32) -> Option<usize> {
    if !in_column_titles(fence, y) {
        return None;
    }
    let columns = fence.detail_columns();
    (1..4)
        .find(|&k| columns[k].2 > 0 && (x - (columns[k].1 - 6)).abs() <= 4)
        .map(|k| k - 1)
}

/// Double-click on a divider: fit the column to its widest value.
unsafe fn auto_fit_column(hwnd: HWND, ptr: *mut FenceWindow, index: usize) {
    let w = &mut *ptr;
    let Some(renderer) = &w.renderer else {
        return;
    };
    let title = [
        i18n::t(K::ColModified),
        i18n::t(K::ColType),
        i18n::t(K::ColSize),
    ][index];
    let widest = w
        .fence
        .paths
        .iter()
        .filter_map(|p| crate::meta::get(p))
        .map(|m| match index {
            0 => crate::meta::format_time(m.modified),
            1 => m.type_name.clone(),
            _ => crate::meta::format_size(m.size),
        })
        .chain(Some(format!("{title} ▲")))
        .map(|text| renderer.text_width(&text))
        .fold(0.0f32, f32::max);
    let mut settings = w.fence.detail_settings();
    settings[index].width =
        (widest.ceil() as i32 + 16).clamp(crate::model::MIN_COLUMN, crate::model::MAX_COLUMN);
    settings[index].visible = true;
    w.fence.details = Some(settings);
    write_back(&w.fence);
    let _ = InvalidateRect(Some(hwnd), None, false);
}

const CMD_COLUMN_BASE: usize = 1050;
const CMD_COLUMNS_AUTO: usize = 1053;

/// Right-click on the column titles: choose columns (名稱 is always shown).
unsafe fn column_menu(hwnd: HWND, ptr: *mut FenceWindow) {
    let Ok(menu) = CreatePopupMenu() else {
        return;
    };
    let settings = (*ptr).fence.detail_settings();
    let custom = (*ptr).fence.details.is_some();
    let _ = AppendMenuW(
        menu,
        MF_STRING | MF_CHECKED | MF_GRAYED,
        0,
        i18n::w(K::ColName),
    );
    for (index, label) in [
        i18n::t(K::ColModified),
        i18n::t(K::ColType),
        i18n::t(K::ColSize),
    ]
    .into_iter()
    .enumerate()
    {
        let text: Vec<u16> = label.encode_utf16().chain(Some(0)).collect();
        let shown = if custom {
            settings[index].visible
        } else {
            (*ptr).fence.detail_columns()[index + 1].2 > 0
        };
        let checked = if shown { MF_CHECKED } else { MF_UNCHECKED };
        let _ = AppendMenuW(
            menu,
            MF_STRING | checked,
            CMD_COLUMN_BASE + index,
            PCWSTR(text.as_ptr()),
        );
    }
    let _ = AppendMenuW(menu, MF_SEPARATOR, 0, None);
    let _ = AppendMenuW(menu, MF_STRING, CMD_COLUMNS_AUTO, i18n::w(K::ColumnsAuto));
    let mut pt = POINT::default();
    let _ = GetCursorPos(&mut pt);
    let command = TrackPopupMenu(
        menu,
        TPM_RETURNCMD | TPM_NONOTIFY | TPM_RIGHTBUTTON,
        pt.x,
        pt.y,
        None,
        hwnd,
        None,
    )
    .0 as usize;
    let _ = DestroyMenu(menu);
    let w = &mut *ptr;
    if (CMD_COLUMN_BASE..CMD_COLUMN_BASE + 3).contains(&command) {
        let mut settings = w.fence.detail_settings();
        let index = command - CMD_COLUMN_BASE;
        settings[index].visible = !settings[index].visible;
        w.fence.details = Some(settings);
    } else if command == CMD_COLUMNS_AUTO {
        w.fence.details = None;
    } else {
        return;
    }
    write_back(&w.fence);
    let _ = InvalidateRect(Some(hwnd), None, false);
}

/// The details column whose title is under a client point.
fn column_at(fence: &Fence, x: i32, y: i32) -> Option<SortKey> {
    if fence.mode() != ViewMode::Details
        || fence.collapsed
        || !(crate::model::COLUMNS_TOP..crate::model::DETAILS_TOP - 2).contains(&y)
    {
        return None;
    }
    fence
        .detail_columns()
        .into_iter()
        .find(|&(_, left, width)| width > 0 && x >= left - 6 && x < left + width - 6)
        .map(|(key, _, _)| key)
}

const CMD_SIZE_BASE: usize = 1060;
const CMD_ALL_SIZE_BASE: usize = 1070;
const SIZE_LABELS: [K; 5] = [K::SizeS, K::SizeM, K::SizeL, K::SizeXL, K::SizeXXL];

/// Item size of one group (Ctrl+wheel or the 「檢視」 menu).
unsafe fn set_size(hwnd: HWND, ptr: *mut FenceWindow, level: usize) {
    let w = &mut *ptr;
    if w.fence.size_level() == level {
        return;
    }
    w.fence.size = (level != crate::model::DEFAULT_SIZE).then_some(level as u8);
    let anchor = w
        .selected
        .and_then(|index| w.visible_indices.iter().position(|&i| i == index))
        .unwrap_or(w.scroll);
    w.scroll = w.fence.scroll_to_show(0, anchor).min(w.max_scroll());
    write_back(&w.fence);
    // Icons of the new size come from the background cache.
    let _ = SetTimer(Some(hwnd), ICON_TIMER, 25, None);
    let _ = InvalidateRect(Some(hwnd), None, false);
}

/// Tray: the same item size for every group.
unsafe fn set_all_sizes(level: usize) {
    let handles = MANAGER.with(|m| {
        m.borrow()
            .as_ref()
            .map(|m| m.handles.values().copied().collect::<Vec<_>>())
            .unwrap_or_default()
    });
    for hwnd in handles {
        let ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut FenceWindow;
        if !ptr.is_null() {
            set_size(hwnd, ptr, level);
        }
    }
}

/// Details header click: sort ascending, then descending, then manual.
unsafe fn sort_by_column(hwnd: HWND, ptr: *mut FenceWindow, key: SortKey) {
    let w = &mut *ptr;
    (w.fence.sort, w.fence.sort_desc) = match (w.fence.sort == key, w.fence.sort_desc) {
        (false, _) => (key, false),
        (true, false) => (key, true),
        (true, true) => (SortKey::Manual, false),
    };
    apply_order(hwnd, ptr);
}

unsafe fn apply_order(hwnd: HWND, ptr: *mut FenceWindow) {
    let w = &mut *ptr;
    w.rebuild_visible();
    write_back(&w.fence);
    if w.waiting_for_details() {
        let _ = SetTimer(Some(hwnd), ICON_TIMER, 25, None);
    }
    let _ = InvalidateRect(Some(hwnd), None, false);
}

unsafe fn set_view(hwnd: HWND, ptr: *mut FenceWindow, mode: ViewMode) {
    let w = &mut *ptr;
    w.fence.set_mode(mode);
    w.hover = None;
    w.scroll = w.selected.map_or(0, |index| {
        let slot = w
            .visible_indices
            .iter()
            .position(|&i| i == index)
            .unwrap_or(0);
        w.fence.scroll_to_show(0, slot)
    });
    w.scroll = w.scroll.min(w.max_scroll());
    write_back(&w.fence);
    let _ = InvalidateRect(Some(hwnd), None, false);
}

unsafe fn toggle_collapse(hwnd: HWND, ptr: *mut FenceWindow) {
    let w = &mut *ptr;
    let mut rect = RECT::default();
    let _ = GetWindowRect(hwnd, &mut rect);
    let current_height = (rect.bottom - rect.top).max(HEADER_HEIGHT);
    w.fence.collapsed = !w.fence.collapsed;
    let fence = w.fence.clone();
    write_back(&fence);
    w.animation = Some(HeightAnimation {
        from: current_height,
        to: fence.window_height(),
        started: Instant::now(),
    });
    if SetTimer(Some(hwnd), ANIMATION_TIMER, 16, None) == 0 {
        animate_height(hwnd, ptr);
    }
}

unsafe fn animate_height(hwnd: HWND, ptr: *mut FenceWindow) {
    let w = &mut *ptr;
    let Some(animation) = &w.animation else {
        let _ = KillTimer(Some(hwnd), ANIMATION_TIMER);
        return;
    };
    let progress =
        (animation.started.elapsed().as_secs_f32() / ANIMATION_DURATION.as_secs_f32()).min(1.0);
    let eased = 1.0 - (1.0 - progress).powi(3);
    let height =
        (animation.from as f32 + (animation.to - animation.from) as f32 * eased).round() as i32;
    if progress >= 1.0 {
        w.animation = None;
        let _ = KillTimer(Some(hwnd), ANIMATION_TIMER);
    }
    let _ = SetWindowPos(
        hwnd,
        None,
        w.fence.x,
        w.fence.y,
        w.fence.width,
        height,
        SWP_NOACTIVATE | SWP_NOZORDER,
    );
    let _ = InvalidateRect(Some(hwnd), None, false);
}

unsafe fn start_rename(hwnd: HWND, ptr: *mut FenceWindow) {
    let w = &mut *ptr;
    if let Some(existing) = w.rename_edit {
        let _ = SetFocus(Some(existing));
        return;
    }
    let title: Vec<u16> = w.fence.title.encode_utf16().chain(Some(0)).collect();
    let Ok(instance) = GetModuleHandleW(None) else {
        return;
    };
    let Ok(edit) = CreateWindowExW(
        WINDOW_EX_STYLE(0),
        w!("EDIT"),
        PCWSTR(title.as_ptr()),
        WS_CHILD | WS_VISIBLE | WS_BORDER | WS_TABSTOP | WINDOW_STYLE(ES_AUTOHSCROLL as u32),
        10,
        5,
        (w.fence.width - 90).max(80),
        26,
        Some(hwnd),
        None,
        Some(HINSTANCE(instance.0)),
        None,
    ) else {
        return;
    };
    w.rename_edit = Some(edit);
    let font = GetStockObject(DEFAULT_GUI_FONT);
    let _ = SendMessageW(
        edit,
        WM_SETFONT,
        Some(WPARAM(font.0 as usize)),
        Some(LPARAM(1)),
    );
    let _ = SetFocus(Some(edit));
    let _ = SendMessageW(edit, 0x00B1, Some(WPARAM(0)), Some(LPARAM(-1))); // EM_SETSEL
}

/// The name shown/edited for a file: shortcut extensions are hidden, as in
/// Explorer, and kept when the file is renamed.
fn editable_name(path: &Path) -> (String, Option<String>) {
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let hidden = path
        .extension()
        .map(|e| e.to_string_lossy().into_owned())
        .filter(|e| e.eq_ignore_ascii_case("lnk") || e.eq_ignore_ascii_case("url"));
    match hidden {
        Some(ext) => (name[..name.len() - ext.len() - 1].to_owned(), Some(ext)),
        None => (name, None),
    }
}

/// F2 on a selected item: edit its file name in place, like Explorer.
unsafe fn start_item_rename(hwnd: HWND, ptr: *mut FenceWindow) {
    let w = &mut *ptr;
    if w.rename_edit.is_some() || w.fence.collapsed {
        return;
    }
    let Some(index) = w.selected else {
        return;
    };
    let (Some(entry), Some(slot)) = (
        w.fence.entries.get(index).cloned(),
        w.visible_indices.iter().position(|&i| i == index),
    ) else {
        return;
    };
    w.scroll = w.fence.scroll_to_show(w.scroll, slot);
    let cell = w.fence.slot_rect(slot - w.scroll);
    let (x, y, width) = match w.fence.mode() {
        ViewMode::Icons => (cell.x + 2, cell.y + 54, cell.width - 4),
        ViewMode::Details => (cell.x + 26, cell.y, w.fence.detail_columns()[0].2 - 20),
        _ => (cell.x + 26, cell.y, cell.width - 30),
    };
    let (shown, _) = editable_name(&entry.path);
    let text: Vec<u16> = shown.encode_utf16().chain(Some(0)).collect();
    let Ok(instance) = GetModuleHandleW(None) else {
        return;
    };
    let Ok(edit) = CreateWindowExW(
        WINDOW_EX_STYLE(0),
        w!("EDIT"),
        PCWSTR(text.as_ptr()),
        WS_CHILD | WS_VISIBLE | WS_BORDER | WINDOW_STYLE(ES_AUTOHSCROLL as u32),
        x,
        y,
        width.max(60),
        26,
        Some(hwnd),
        None,
        Some(HINSTANCE(instance.0)),
        None,
    ) else {
        return;
    };
    w.rename_edit = Some(edit);
    w.rename_item = Some(entry.id);
    let font = GetStockObject(DEFAULT_GUI_FONT);
    let _ = SendMessageW(
        edit,
        WM_SETFONT,
        Some(WPARAM(font.0 as usize)),
        Some(LPARAM(1)),
    );
    let _ = SetFocus(Some(edit));
    // Select the name without its extension, as Explorer does.
    let stem = match shown.rfind('.') {
        Some(dot) if dot > 0 && !entry.path.is_dir() => shown[..dot].encode_utf16().count(),
        _ => shown.encode_utf16().count(),
    };
    let _ = SendMessageW(edit, 0x00B1, Some(WPARAM(0)), Some(LPARAM(stem as isize))); // EM_SETSEL
    let _ = InvalidateRect(Some(hwnd), None, false);
}

/// Renames the file through the Shell (conflict prompts and Explorer undo
/// included), then points the same reference at the new name.
unsafe fn rename_item_file(hwnd: HWND, id: &str, value: &str) {
    let w = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut FenceWindow;
    if w.is_null() {
        return;
    }
    let Some(entry) = (*w).fence.entries.iter().find(|e| e.id == id).cloned() else {
        return;
    };
    let (shown, hidden) = editable_name(&entry.path);
    let value = value.trim();
    if value.is_empty() || value == shown {
        return;
    }
    let name = match hidden {
        Some(ext)
            if !value
                .to_lowercase()
                .ends_with(&format!(".{}", ext.to_lowercase())) =>
        {
            format!("{value}.{ext}")
        }
        _ => value.to_owned(),
    };
    let result = (|| -> windows::core::Result<()> {
        let operation: IFileOperation = CoCreateInstance(&FileOperation, None, CLSCTX_ALL)?;
        if let Some(owner) = dialog_owner(hwnd) {
            operation.SetOwnerWindow(owner)?;
        }
        operation.SetOperationFlags(FOF_ALLOWUNDO)?;
        let shell = crate::shell_icons::shell_path(&entry.path.to_string_lossy());
        let wide: Vec<u16> = shell.encode_utf16().chain(Some(0)).collect();
        let item: IShellItem = SHCreateItemFromParsingName(PCWSTR(wide.as_ptr()), None)?;
        let new_name: Vec<u16> = name.encode_utf16().chain(Some(0)).collect();
        operation.RenameItem(&item, PCWSTR(new_name.as_ptr()), None)?;
        operation.PerformOperations()
    })();
    if let Err(error) = result {
        if error.code() != HRESULT::from_win32(ERROR_CANCELLED.0) {
            collection_error(hwnd, &i18n::tf(K::RenameFailed, &[&error]));
        }
        return;
    }
    let target = entry.path.with_file_name(&name);
    if target.exists() {
        enqueue(
            hwnd,
            Command::Retarget {
                id: id.to_owned(),
                path: target,
            },
        );
    }
}

unsafe fn finish_rename(hwnd: HWND, ptr: *mut FenceWindow, save: bool) {
    let w = &mut *ptr;
    let Some(edit) = w.rename_edit.take() else {
        return;
    };
    if let Some(id) = w.rename_item.take() {
        let mut buffer = [0u16; 260];
        let len = GetWindowTextW(edit, &mut buffer).max(0) as usize;
        let value = String::from_utf16_lossy(&buffer[..len]);
        let _ = DestroyWindow(edit);
        let _ = InvalidateRect(Some(hwnd), None, false);
        if save {
            rename_item_file(hwnd, &id, &value);
        }
        return;
    }
    if save {
        let mut buffer = [0u16; 128];
        let len = GetWindowTextW(edit, &mut buffer).max(0) as usize;
        let value = String::from_utf16_lossy(&buffer[..len]);
        let value = value.trim();
        debug_log(&format!(
            "rename finish: length={len}, changed={}",
            value != w.fence.title
        ));
        if !value.is_empty() {
            w.fence.title = value.chars().take(64).collect();
            write_back(&w.fence);
        }
    }
    let _ = DestroyWindow(edit);
    let _ = InvalidateRect(Some(hwnd), None, false);
}

unsafe fn launch_path(path: &Path) {
    // ShellExecute does not consistently accept the extended filesystem prefix
    // used for stable reference paths (particularly for .lnk and folders).
    let shell_path = PathBuf::from(crate::shell_icons::shell_path(&path.to_string_lossy()));
    let path = shell_path.as_path();
    let wide: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
    // Programs and scripts should resolve relative resources beside the item,
    // rather than in the desktop organizer's own working directory. Shortcuts
    // retain their Shell-defined target and working-directory behavior.
    let directory = path
        .parent()
        .filter(|_| {
            path.extension().is_some_and(|ext| {
                [
                    "exe", "com", "bat", "cmd", "ps1", "py", "pyw", "sh", "js", "vbs", "ahk",
                    "lua", "rb",
                ]
                .iter()
                .any(|kind| ext.eq_ignore_ascii_case(kind))
            })
        })
        .map(|parent| {
            parent
                .as_os_str()
                .encode_wide()
                .chain(Some(0))
                .collect::<Vec<_>>()
        });
    // Shell verbs, DDE handlers and associations can block for a long time.
    // Our GUI thread shares Explorer's input queue, so opening runs on its
    // own STA thread; the click returns immediately. Foreground rights come
    // from this process's recent input and remain valid for the launch.
    let _ = AllowSetForegroundWindow(ASFW_ANY);
    std::thread::spawn(move || {
        let initialized = CoInitializeEx(None, COINIT_APARTMENTTHREADED | COINIT_DISABLE_OLE1DDE);
        let mut info = SHELLEXECUTEINFOW {
            cbSize: std::mem::size_of::<SHELLEXECUTEINFOW>() as u32,
            // NOASYNC: this thread waits for the Shell to finish the launch.
            fMask: SEE_MASK_NOASYNC,
            lpVerb: w!("open"),
            lpFile: PCWSTR(wide.as_ptr()),
            lpDirectory: directory
                .as_ref()
                .map_or(PCWSTR::null(), |value| PCWSTR(value.as_ptr())),
            nShow: SW_SHOWNORMAL.0,
            ..Default::default()
        };
        let _ = ShellExecuteExW(&mut info);
        if initialized.is_ok() {
            CoUninitialize();
        }
    });
}

/// Explorer-style delete of the selected items' files: Delete sends them to
/// the Recycle Bin, Shift+Delete deletes permanently. Windows shows its own
/// confirmation (always for permanent deletion) and progress; a reference
/// is dropped only for files that are actually gone afterwards.
unsafe fn delete_selected(hwnd: HWND, ptr: *mut FenceWindow, permanent: bool) {
    if work_busy() {
        return;
    }
    let w = &*ptr;
    let items: Vec<(String, PathBuf)> = w
        .fence
        .entries
        .iter()
        .filter(|e| w.selection.contains(&e.id))
        .map(|e| (e.id.clone(), e.path.clone()))
        .collect();
    if items.is_empty() {
        return;
    }
    let result = (|| -> windows::core::Result<()> {
        let operation: IFileOperation = CoCreateInstance(&FileOperation, None, CLSCTX_ALL)?;
        // Never parent Shell UI to a desktop child (it would disable Explorer).
        if let Some(owner) = dialog_owner(hwnd) {
            operation.SetOwnerWindow(owner)?;
        }
        operation.SetOperationFlags(if permanent {
            FOF_WANTNUKEWARNING
        } else {
            FOF_ALLOWUNDO | FOF_WANTNUKEWARNING | FOFX_RECYCLEONDELETE
        })?;
        for (_, path) in &items {
            let shell = crate::shell_icons::shell_path(&path.to_string_lossy());
            let wide: Vec<u16> = shell.encode_utf16().chain(Some(0)).collect();
            let item: IShellItem = SHCreateItemFromParsingName(PCWSTR(wide.as_ptr()), None)?;
            operation.DeleteItem(&item, None)?;
        }
        operation.PerformOperations()
    })();
    if let Err(error) = result {
        // Cancelling the confirmation is not an error worth reporting.
        if error.code() != HRESULT::from_win32(ERROR_CANCELLED.0) {
            collection_error(hwnd, &i18n::tf(K::DeleteFailed, &[&error]));
        }
    }
    let gone: Vec<String> = items
        .into_iter()
        .filter(|(_, path)| std::fs::symlink_metadata(path).is_err())
        .map(|(id, _)| id)
        .collect();
    if !gone.is_empty() {
        enqueue(hwnd, Command::Desktop { ids: gone });
    }
}

const SHELL_FIRST: u32 = 0x8000;
const SHELL_LAST: u32 = 0xEFFF;

thread_local! {
    /// The Shell menu being tracked, for owner-drawn/submenu messages.
    static SHELL_MENU: RefCell<Option<IContextMenu>> = const { RefCell::new(None) };
}

/// Lets "Send to", "Open with" and other Shell submenus populate and draw.
unsafe fn forward_menu_message(msg: u32, wp: WPARAM, lp: LPARAM) -> Option<LRESULT> {
    let context = SHELL_MENU.with(|m| m.borrow().clone())?;
    if let Ok(three) = context.cast::<IContextMenu3>() {
        let mut result = LRESULT(0);
        if three.HandleMenuMsg2(msg, wp, lp, Some(&mut result)).is_ok() {
            return Some(result);
        }
    } else if let Ok(two) = context.cast::<IContextMenu2>() {
        if two.HandleMenuMsg(msg, wp, lp).is_ok() {
            return Some(LRESULT(0));
        }
    }
    None
}

/// Windows' context menu for the files (those in the first item's folder).
unsafe fn shell_context_menu(paths: &[PathBuf], owner: HWND) -> Option<IContextMenu> {
    let folder_of = |p: &Path| p.parent().map(|d| d.to_path_buf());
    let parent = folder_of(paths.first()?);
    let mut absolute = Vec::new();
    for path in paths.iter().filter(|p| folder_of(p) == parent) {
        let shell = crate::shell_icons::shell_path(&path.to_string_lossy());
        let wide: Vec<u16> = shell.encode_utf16().chain(Some(0)).collect();
        let mut pidl = std::ptr::null_mut();
        if SHParseDisplayName(PCWSTR(wide.as_ptr()), None, &mut pidl, 0, None).is_ok() {
            absolute.push(pidl);
        }
    }
    let result = (|| {
        let mut children = Vec::new();
        let mut folder: Option<IShellFolder> = None;
        for &pidl in &absolute {
            let mut last = std::ptr::null_mut();
            let bound: IShellFolder = SHBindToParent(pidl, Some(&mut last)).ok()?;
            folder.get_or_insert(bound);
            children.push(last as *const _);
        }
        folder?
            .GetUIObjectOf::<IContextMenu>(owner, &children, None)
            .ok()
    })();
    for pidl in absolute {
        ILFree(Some(pidl));
    }
    result
}

unsafe fn invoke_shell_command(
    hwnd: HWND,
    ptr: *mut FenceWindow,
    context: &IContextMenu,
    offset: u32,
    pt: POINT,
) {
    let mut verb = [0u16; 64];
    let verb = context
        .GetCommandString(
            offset as usize,
            GCS_VERBW,
            None,
            windows::core::PSTR(verb.as_mut_ptr().cast()),
            verb.len() as u32,
        )
        .ok()
        .map(|_| {
            let end = verb.iter().position(|&c| c == 0).unwrap_or(verb.len());
            String::from_utf16_lossy(&verb[..end]).to_lowercase()
        })
        .unwrap_or_default();
    // Verbs that need a view or change our references are handled here.
    match verb.as_str() {
        "rename" => return start_item_rename(hwnd, ptr),
        "delete" => {
            let permanent = GetKeyState(VK_SHIFT.0 as i32) < 0;
            return delete_selected(hwnd, ptr, permanent);
        }
        "open" => {
            let w = &*ptr;
            for entry in w
                .fence
                .entries
                .iter()
                .filter(|e| w.selection.contains(&e.id))
            {
                launch_path(&entry.path);
            }
            return;
        }
        _ => {}
    }
    let info = CMINVOKECOMMANDINFOEX {
        cbSize: std::mem::size_of::<CMINVOKECOMMANDINFOEX>() as u32,
        // CMIC_MASK_UNICODE and CMIC_MASK_ASYNCOK share SEE_MASK values.
        fMask: SEE_MASK_UNICODE | CMIC_MASK_PTINVOKE | SEE_MASK_ASYNCOK,
        hwnd: dialog_owner(hwnd).unwrap_or(hwnd),
        lpVerb: windows::core::PCSTR(offset as usize as *const u8),
        lpVerbW: PCWSTR(offset as usize as *const u16),
        nShow: SW_SHOWNORMAL.0,
        ptInvoke: pt,
        ..Default::default()
    };
    let _ = context.InvokeCommand(&info as *const _ as *const CMINVOKECOMMANDINFO);
}

unsafe fn popup_menu(hwnd: HWND, ptr: *mut FenceWindow, selection: Option<usize>) {
    if let Some(index) = selection {
        let window = &*ptr;
        if let Some(entry) = window.fence.entries.get(index) {
            if !(*ptr).selection.contains(&entry.id) {
                select_one(&mut *ptr, index);
            }
        }
    }
    let Ok(menu) = CreatePopupMenu() else {
        return;
    };
    let mut transfers = Vec::new();
    let groups = MANAGER.with(|m| {
        m.borrow()
            .as_ref()
            .map(|m| {
                m.layout
                    .fences
                    .iter()
                    .map(|f| (f.id, f.title.clone()))
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default()
    });
    let undo = undo_available();
    let _ = AppendMenuW(
        menu,
        if undo {
            MF_STRING
        } else {
            MF_STRING | MF_GRAYED
        },
        CMD_UNDO,
        i18n::w(K::UndoMenu),
    );
    let _ = AppendMenuW(menu, MF_STRING, CMD_HISTORY, i18n::w(K::HistoryMenu));
    if !(*ptr).selection.is_empty() {
        let _ = AppendMenuW(menu, MF_SEPARATOR, 0, None);
        let _ = AppendMenuW(menu, MF_STRING, CMD_DESKTOP, i18n::w(K::Ungroup));
        let _ = AppendMenuW(
            menu,
            MF_STRING,
            CMD_REMOVE_ITEM,
            i18n::w(K::RemoveFromGroup),
        );
        let _ = AppendMenuW(menu, MF_STRING, CMD_DELETE_FILES, i18n::w(K::DeleteToBin));
        let _ = AppendMenuW(
            menu,
            MF_STRING,
            CMD_DELETE_PERMANENT,
            i18n::w(K::DeletePermanent),
        );
        if let Ok(submenu) = CreatePopupMenu() {
            for (id, title) in groups {
                if id == (*ptr).fence.id {
                    continue;
                }
                let command = CMD_TRANSFER_BASE + transfers.len();
                transfers.push(id);
                let label: Vec<u16> = title
                    .replace('&', "&&")
                    .encode_utf16()
                    .chain(Some(0))
                    .collect();
                let _ = AppendMenuW(submenu, MF_STRING, command, PCWSTR(label.as_ptr()));
            }
            let _ = AppendMenuW(menu, MF_POPUP, submenu.0 as usize, i18n::w(K::MoveToGroup));
        }
        let _ = AppendMenuW(menu, MF_STRING, CMD_MOVE_UP, i18n::w(K::MoveUp));
        let _ = AppendMenuW(menu, MF_STRING, CMD_MOVE_DOWN, i18n::w(K::MoveDown));
    }
    let _ = AppendMenuW(
        menu,
        MF_STRING,
        CMD_SELECT_ALL,
        i18n::w(K::SelectAllResults),
    );
    let _ = AppendMenuW(menu, MF_SEPARATOR, 0, None);
    let _ = AppendMenuW(menu, MF_STRING, CMD_NEW, i18n::w(K::NewGroup));
    let _ = AppendMenuW(
        menu,
        MF_STRING,
        CMD_AUTO_PREVIEW,
        i18n::w(K::AutoPreviewMenu),
    );
    let _ = AppendMenuW(menu, MF_STRING, CMD_PIN_CLIPBOARD, i18n::w(K::PinClipboard));
    let _ = AppendMenuW(
        menu,
        MF_STRING,
        CMD_COLLECT_DESKTOP,
        i18n::w(K::UpdateGroups),
    );
    let _ = AppendMenuW(menu, MF_STRING, CMD_RENAME, i18n::w(K::RenameGroup));
    let _ = AppendMenuW(menu, MF_STRING, CMD_FIND, i18n::w(K::Search));
    let _ = AppendMenuW(
        menu,
        MF_STRING,
        CMD_COLLAPSE,
        if (*ptr).fence.collapsed {
            i18n::w(K::Expand)
        } else {
            i18n::w(K::Collapse)
        },
    );
    append_view_menus(menu, &(*ptr).fence);
    let _ = AppendMenuW(menu, MF_STRING, CMD_THEME, i18n::w(K::ToggleTheme));
    let _ = AppendMenuW(menu, MF_STRING, CMD_ARRANGE, i18n::w(K::ArrangeGroups));
    let _ = AppendMenuW(
        menu,
        MF_STRING,
        CMD_RESET_POSITIONS,
        i18n::w(K::ResetPositions),
    );
    let _ = AppendMenuW(menu, MF_STRING, CMD_OPEN_SETTINGS, i18n::w(K::OpenSettings));
    let _ = AppendMenuW(menu, MF_STRING, CMD_REMOVE_FENCE, i18n::w(K::DeleteGroup));
    let _ = AppendMenuW(menu, MF_STRING, CMD_EXIT, i18n::w(K::Exit));
    let mut pt = POINT::default();
    let _ = GetCursorPos(&mut pt);
    // Right-clicking an item shows Windows' own menu for the selected files,
    // followed by LumaDesk's commands; the group menu becomes a submenu.
    let paths: Vec<PathBuf> = if selection.is_some() {
        let w = &*ptr;
        w.fence
            .entries
            .iter()
            .filter(|e| w.selection.contains(&e.id))
            .map(|e| e.path.clone())
            .collect()
    } else {
        vec![]
    };
    let shell = if paths.is_empty() {
        None
    } else {
        shell_context_menu(&paths, dialog_owner(hwnd).unwrap_or(hwnd))
    };
    let root = match &shell {
        Some(context) => {
            let root = CreatePopupMenu().unwrap_or(menu);
            let extended = GetKeyState(VK_SHIFT.0 as i32) < 0;
            let _ = context.QueryContextMenu(
                root,
                0,
                SHELL_FIRST,
                SHELL_LAST,
                CMF_NORMAL | CMF_CANRENAME | if extended { CMF_EXTENDEDVERBS } else { 0 },
            );
            let _ = AppendMenuW(root, MF_SEPARATOR, 0, None);
            let _ = AppendMenuW(
                root,
                MF_STRING,
                CMD_REMOVE_ITEM,
                i18n::w(K::RemoveFromGroup),
            );
            let _ = AppendMenuW(root, MF_POPUP, menu.0 as usize, w!("LumaDesk"));
            root
        }
        None => menu,
    };
    SHELL_MENU.with(|m| *m.borrow_mut() = shell.clone());
    let command = TrackPopupMenu(
        root,
        TPM_RETURNCMD | TPM_RIGHTBUTTON,
        pt.x,
        pt.y,
        None,
        hwnd,
        None,
    )
    .0 as usize;
    SHELL_MENU.with(|m| m.borrow_mut().take());
    // Destroying the root also destroys the attached LumaDesk submenu.
    let _ = DestroyMenu(root);
    if let Some(context) = shell {
        if (SHELL_FIRST as usize..=SHELL_LAST as usize).contains(&command) {
            invoke_shell_command(hwnd, ptr, &context, command as u32 - SHELL_FIRST, pt);
            return;
        }
    }
    if command >= CMD_TRANSFER_BASE && command < CMD_TRANSFER_BASE + transfers.len() {
        enqueue(
            hwnd,
            Command::Transfer {
                ids: selected_ids(&*ptr),
                fence: transfers[command - CMD_TRANSFER_BASE],
                before: None,
            },
        );
        return;
    }
    match command {
        CMD_SELECT_ALL => {
            select_all(&mut *ptr);
            let _ = InvalidateRect(Some(hwnd), None, false);
        }
        CMD_UNDO => {
            request_undo(hwnd);
        }
        CMD_HISTORY => show_history(hwnd),
        CMD_DESKTOP => {
            enqueue(
                hwnd,
                Command::Desktop {
                    ids: selected_ids(&*ptr),
                },
            );
        }
        CMD_REMOVE_ITEM => {
            enqueue(
                hwnd,
                Command::Desktop {
                    ids: selected_ids(&*ptr),
                },
            );
        }
        CMD_MOVE_UP | CMD_MOVE_DOWN => {
            if let Some(i) = (*ptr).selected {
                move_pinned(hwnd, ptr, i, command == CMD_MOVE_DOWN);
            }
        }
        CMD_RENAME => start_rename(hwnd, ptr),
        CMD_FIND => start_search(hwnd, ptr),
        CMD_PIN_CLIPBOARD => pin_from_clipboard(hwnd),
        CMD_COLLECT_DESKTOP => {
            collect_desktop_paths(hwnd, (*ptr).fence.paths.clone());
        }
        CMD_NEW => create_collection(),
        CMD_AUTO_PREVIEW => request_organize_preview(hwnd),
        CMD_COLLAPSE => toggle_collapse(hwnd, ptr),
        CMD_THEME => switch_theme(),
        CMD_RESET_POSITIONS => reset_positions(),
        CMD_ARRANGE => arrange_fences(false),
        command if view_or_sort_command(hwnd, ptr, command) => {}
        CMD_DELETE_FILES => delete_selected(hwnd, ptr, false),
        CMD_DELETE_PERMANENT => delete_selected(hwnd, ptr, true),
        CMD_OPEN_SETTINGS => open_settings_folder(),
        CMD_EXIT => exit_app(),
        CMD_REMOVE_FENCE => {
            if work_busy() {
                return;
            }
            enqueue(
                hwnd,
                Command::DeleteFence {
                    fence: (*ptr).fence.id,
                },
            );
        }
        _ => {}
    }
}

#[cfg(test)]
mod monitor_tests {
    use super::*;

    #[test]
    fn disabled_ancestor_blocks_input_and_dialogs_never_own_the_shell() {
        unsafe {
            let root = CreateWindowExW(
                WINDOW_EX_STYLE(0),
                w!("STATIC"),
                None,
                WS_POPUP,
                0,
                0,
                40,
                40,
                None,
                None,
                None,
                None,
            )
            .unwrap();
            let child = CreateWindowExW(
                WINDOW_EX_STYLE(0),
                w!("STATIC"),
                None,
                WS_CHILD,
                0,
                0,
                20,
                20,
                Some(root),
                None,
                None,
                None,
            )
            .unwrap();
            assert!(desktop_accepts_input(child));
            assert_eq!(dialog_owner(child), Some(root));
            let _ = EnableWindow(root, false);
            assert!(IsWindowEnabled(child).as_bool());
            assert!(!desktop_accepts_input(child));
            let _ = EnableWindow(root, true);
            assert!(desktop_accepts_input(child));
            let shell = GetShellWindow();
            if !shell.is_invalid() {
                assert_eq!(dialog_owner(shell), None);
            }
            let _ = DestroyWindow(root);
        }
    }

    #[test]
    fn pinning_never_negotiates_a_move_or_unsupported_data() {
        use windows::Win32::System::Ole::DROPEFFECT_MOVE;
        assert_eq!(pin_drop_effect(DROPEFFECT_MOVE, true), DROPEFFECT_NONE);
        assert_eq!(pin_drop_effect(DROPEFFECT_COPY, true), DROPEFFECT_COPY);
        assert_eq!(
            pin_drop_effect(DROPEFFECT_COPY | DROPEFFECT_LINK | DROPEFFECT_MOVE, true),
            DROPEFFECT_LINK
        );
        assert_eq!(
            pin_drop_effect(DROPEFFECT_COPY | DROPEFFECT_LINK, false),
            DROPEFFECT_NONE
        );
        assert_eq!(pin_drop_effect(DROPEFFECT_NONE, true), DROPEFFECT_NONE);
    }

    fn region(left: i32, top: i32, right: i32, bottom: i32) -> MonitorRegion {
        MonitorRegion {
            work: RECT {
                left,
                top,
                right,
                bottom: bottom - 48,
            },
            primary: true,
        }
    }

    #[test]
    fn recovers_window_outside_primary_due_to_desktop_origin() {
        let monitor = region(0, 0, 1920, 1080);
        let origin = POINT { x: 0, y: -92 };
        let mut fence = Layout::default().fences.remove(0);
        fence.x = 300;
        fence.y = 1170;
        assert!(!header_reachable(&fence, origin, &[monitor]));
        let area = client_area(monitor, origin);
        assert_eq!(area.top, 92);
        let (x, y) = free_position(&[], fence.width, fence.height, Some(area));
        fence.x = x;
        fence.y = y;
        assert!(header_reachable(&fence, origin, &[monitor]));
    }

    #[test]
    fn arranging_tiles_each_monitor_without_overlap() {
        let left = MonitorRegion {
            work: RECT {
                left: 0,
                top: 0,
                right: 1920,
                bottom: 1032,
            },
            primary: true,
        };
        let right = MonitorRegion {
            work: RECT {
                left: 1920,
                top: -92,
                right: 3000,
                bottom: 1780,
            },
            primary: false,
        };
        let origin = POINT { x: 0, y: -92 };
        let mut fences = Vec::new();
        // A real-world overlapping layout plus one collection on the second monitor.
        for (id, x, y) in [
            (2, 414, 116),
            (3, 804, 116),
            (5, 84, 130),
            (6, 24, 420),
            (7, 972, 420),
            (8, 24, 816),
            (9, 2100, 300),
        ] {
            let mut f = Layout::default().fences.remove(0);
            (f.id, f.x, f.y, f.width, f.height) = (id, x, y, 300, 380);
            fences.push(f);
        }
        let placed = arranged_positions(&fences, origin, &[left, right], left, false);
        assert_eq!(placed.len(), fences.len());
        let rect = |&(id, x, y): &(u64, i32, i32)| (id, x, y, x + 300, y + 380);
        for (i, a) in placed.iter().map(rect).enumerate() {
            for b in placed.iter().skip(i + 1).map(rect) {
                assert!(
                    a.3 <= b.1 || b.3 <= a.1 || a.4 <= b.2 || b.4 <= a.2,
                    "{a:?} {b:?}"
                );
            }
        }
        let on_right = placed.iter().find(|p| p.0 == 9).unwrap();
        assert!(on_right.1 >= 1920, "stays on its monitor: {on_right:?}");
        let all_primary = arranged_positions(&fences, origin, &[left, right], left, true);
        assert!(all_primary.iter().all(|p| p.1 + 300 <= 1920));
    }

    #[test]
    fn recognizes_negative_virtual_screen_coordinates() {
        let left_monitor = region(-1600, -200, 0, 700);
        let origin = POINT { x: -1600, y: -200 };
        let mut fence = Layout::default().fences.remove(0);
        fence.x = 30;
        fence.y = 50;
        assert!(header_reachable(&fence, origin, &[left_monitor]));
    }

    #[test]
    fn moves_header_hidden_behind_taskbar() {
        let monitor = region(0, 0, 1920, 1080);
        let origin = POINT { x: 0, y: 0 };
        let mut fence = Layout::default().fences.remove(0);
        fence.y = 1024;
        assert!(!header_reachable(&fence, origin, &[monitor]));
        let area = client_area(monitor, origin);
        let (_, y) = free_position(&[], fence.width, fence.height, Some(area));
        assert_eq!(y, 24);
    }

    #[test]
    fn keyboard_navigation_stops_at_bounds() {
        assert_eq!(next_selection(None, 0, 0x28, 1), None);
        assert_eq!(next_selection(None, 5, 0x28, 1), Some(0));
        assert_eq!(next_selection(Some(4), 5, 0x28, 1), Some(4));
        assert_eq!(next_selection(Some(0), 5, 0x26, 1), Some(0));
        assert_eq!(next_selection(Some(2), 5, 0x23, 1), Some(4));
        assert_eq!(next_selection(Some(2), 5, 0x24, 1), Some(0));
        // Grid of 3 columns over 8 items: arrows move by tile or by row.
        assert_eq!(next_selection(Some(1), 8, 0x28, 3), Some(4));
        assert_eq!(next_selection(Some(6), 8, 0x28, 3), Some(6));
        assert_eq!(next_selection(Some(4), 8, 0x26, 3), Some(1));
        assert_eq!(next_selection(Some(1), 8, 0x26, 3), Some(1));
        assert_eq!(next_selection(Some(4), 8, 0x27, 3), Some(5));
        assert_eq!(next_selection(Some(0), 8, 0x25, 3), Some(0));
        assert_eq!(next_selection(Some(7), 8, 0x27, 3), Some(7));
    }

    #[test]
    fn groups_below_the_icon_list_are_raised() {
        use Layer::*;
        assert!(groups_need_raise(&[Icons, Group]));
        assert!(groups_need_raise(&[Group, Icons, Other, Group]));
        assert!(!groups_need_raise(&[Group, Group, Icons, Other]));
        assert!(!groups_need_raise(&[Other, Group]));
        assert!(!groups_need_raise(&[]));
    }

    #[test]
    fn sorting_keeps_folders_first_and_reverses_only_the_key() {
        let value = |folder: bool, key: SortField, name: &str| SortValue {
            folder_last: !folder,
            key,
            name: name.into(),
        };
        let keys = vec![
            value(false, SortField::Number(300), "c.txt"),
            value(true, SortField::None, "zeta"),
            value(false, SortField::Number(100), "a.txt"),
            value(false, SortField::Number(200), "b.txt"),
            value(true, SortField::None, "alpha"),
        ];
        let mut visible = vec![0, 1, 2, 3, 4];
        sort_visible(&mut visible, &keys, SortKey::Size, false);
        assert_eq!(visible, vec![4, 1, 2, 3, 0]);
        sort_visible(&mut visible, &keys, SortKey::Size, true);
        assert_eq!(visible, vec![1, 4, 0, 3, 2]);
        let mut manual = vec![3, 0, 2];
        sort_visible(&mut manual, &keys, SortKey::Manual, true);
        assert_eq!(manual, vec![3, 0, 2]);
    }

    #[test]
    fn typeahead_selects_by_prefix_and_cycles_repeated_letters() {
        let names = [
            "Paint",
            "luma-folder",
            "Example site",
            "記事本",
            "luma-test.txt",
            "nginx.conf",
        ];
        assert_eq!(typeahead_match(&names, None, "l"), Some(1));
        assert_eq!(typeahead_match(&names, Some(1), "l"), Some(4));
        assert_eq!(typeahead_match(&names, Some(4), "ll"), Some(1)); // cycles
        assert_eq!(typeahead_match(&names, Some(1), "luma-t"), Some(4));
        assert_eq!(typeahead_match(&names, Some(1), "LUMA-F"), Some(1)); // stays
        assert_eq!(typeahead_match(&names, Some(0), "記"), Some(3));
        assert_eq!(typeahead_match(&names, Some(0), "zz"), None);
        assert_eq!(typeahead_match(&[], None, "a"), None);
    }

    #[test]
    fn frame_regions_match_header_buttons_edges_and_corner() {
        let hit = |x, y| frame_hit(x, y, 300, 380, true);
        assert_eq!(hit(290, 20), FrameHit::Menu);
        assert_eq!(hit(240, 20), FrameHit::Collapse);
        assert_eq!(hit(100, 20), FrameHit::Move);
        assert_eq!(hit(100, 200), FrameHit::Body);
        let both = FrameHit::Resize {
            horizontal: true,
            vertical: true,
        };
        assert_eq!(hit(290, 370), both);
        assert_eq!(
            hit(296, 200),
            FrameHit::Resize {
                horizontal: true,
                vertical: false
            }
        );
        assert_eq!(
            hit(100, 376),
            FrameHit::Resize {
                horizontal: false,
                vertical: true
            }
        );
        // Collapsed or animating collections only move.
        assert_eq!(frame_hit(290, 370, 300, 380, false), FrameHit::Body);
        assert_eq!(frame_hit(100, 20, 300, 66, false), FrameHit::Move);
    }

    #[test]
    fn drop_targets_the_topmost_group_under_the_point() {
        let rect = |left, top, right, bottom| RECT {
            left,
            top,
            right,
            bottom,
        };
        let stacked = [(2, rect(100, 100, 400, 500)), (1, rect(0, 0, 360, 420))];
        assert_eq!(group_at(POINT { x: 200, y: 200 }, &stacked), Some(2));
        assert_eq!(group_at(POINT { x: 50, y: 50 }, &stacked), Some(1));
        assert_eq!(group_at(POINT { x: 450, y: 50 }, &stacked), None);
        assert_eq!(group_at(POINT { x: 400, y: 200 }, &stacked), None);
    }
}
