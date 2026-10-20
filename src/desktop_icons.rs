//! A reversible display mask for Explorer's desktop icons. Only icon positions
//! change. No filesystem attributes, names, contents or shortcut targets change.
//! A separate watchdog restores positions if the UI process exits unexpectedly.
use crate::i18n::{self, K};
use crate::operations::{identity, Identity};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::time::{Duration, Instant};
use windows::core::{Interface, Result, PCWSTR};
use windows::Win32::Foundation::*;
use windows::Win32::System::Com::*;
use windows::Win32::System::Ole::{OleInitialize, OleUninitialize};
use windows::Win32::System::Threading::*;
use windows::Win32::System::Variant::VARIANT;
use windows::Win32::UI::Shell::Common::ITEMIDLIST;
use windows::Win32::UI::Shell::*;

#[derive(Clone, Serialize, Deserialize)]
struct Icon {
    path: PathBuf,
    identity: Identity,
    x: i32,
    y: i32,
}
#[derive(Default, Serialize, Deserialize)]
struct Session {
    owner: u32,
    flags: u32,
    icons: Vec<Icon>,
}
const ARRANGE: u32 = FWF_AUTOARRANGE.0 as u32 | FWF_SNAPTOGRID.0 as u32;
const PARK: POINT = POINT {
    x: -1000000,
    y: -1000000,
};

struct Pidl(*mut ITEMIDLIST);
impl Drop for Pidl {
    fn drop(&mut self) {
        unsafe { CoTaskMemFree(Some(self.0.cast())) }
    }
}
fn key(path: &Path) -> PathBuf {
    // Do not resolve every desktop object: unrelated junctions/cloud/network
    // items may block for seconds. Only selected paths are canonicalized.
    let text = path.to_string_lossy().replace('/', "\\");
    if let Some(tail) = text.strip_prefix("\\\\?\\UNC\\") {
        PathBuf::from(format!("\\\\{tail}"))
    } else {
        PathBuf::from(text.strip_prefix("\\\\?\\").unwrap_or(&text))
    }
}
unsafe fn view() -> Result<IFolderView2> {
    // SWC_DESKTOP binds Explorer's actual desktop, never another app's window or an
    // ordinary Explorer folder that happens to contain a SHELLDLL_DefView.
    let shell: IShellWindows = CoCreateInstance(&ShellWindows, None, CLSCTX_LOCAL_SERVER)?;
    let empty = VARIANT::default();
    let mut hwnd = 0;
    let dispatch = shell.FindWindowSW(&empty, &empty, SWC_DESKTOP, &mut hwnd, SWFO_NEEDDISPATCH)?;
    let provider: IServiceProvider = dispatch.cast()?;
    let browser: IShellBrowser = provider.QueryService(&SID_STopLevelBrowser)?;
    browser.QueryActiveShellView()?.cast()
}
unsafe fn items(view: &IFolderView2, paths: &[PathBuf]) -> Result<Vec<(PathBuf, Pidl)>> {
    let mut items = vec![];
    let folder: IShellFolder = view.GetFolder()?;
    // Desktop Item PIDLs are relative. Parsing a full filesystem path produces
    // an absolute PIDL that GetItemPosition cannot match. Parse the child name
    // in the actual desktop folder, then verify its complete path (including
    // user/Public Desktop ambiguity). Never resolve unrelated desktop objects.
    for wanted in paths {
        let Some(name) = wanted.file_name() else {
            continue;
        };
        let wide: Vec<_> = name
            .to_string_lossy()
            .encode_utf16()
            .chain(Some(0))
            .collect();
        let mut raw = std::ptr::null_mut();
        let mut attributes = 0;
        if folder
            .ParseDisplayName(
                HWND::default(),
                None,
                PCWSTR(wide.as_ptr()),
                None,
                &mut raw,
                &mut attributes,
            )
            .is_err()
        {
            continue;
        }
        let pidl = Pidl(raw);
        let item: Result<IShellItem> = SHCreateItemWithParent(None, &folder, pidl.0);
        if let Ok(name) = item.and_then(|item| item.GetDisplayName(SIGDN_FILESYSPATH)) {
            let path = name.to_string().ok().map(PathBuf::from);
            CoTaskMemFree(Some(name.0.cast()));
            if let Some(path) = path {
                if crate::operations::same_path(&path, wanted) {
                    items.push((wanted.clone(), pidl));
                }
            }
        }
    }
    // The merged desktop can contain identically named user/Public items.
    // ParseDisplayName picks only one. For unresolved names, obtain the actual
    // view-relative PIDL, checking full paths only for matching child names.
    let mut missing: Vec<_> = paths
        .iter()
        .filter(|p| !items.iter().any(|(found, _)| found == *p))
        .collect();
    if !missing.is_empty() {
        for index in 0..view.ItemCount(SVGIO_ALLVIEW)? {
            let Ok(raw) = view.Item(index) else { continue };
            let pidl = Pidl(raw);
            let mut name = Default::default();
            if folder
                .GetDisplayNameOf(
                    pidl.0,
                    SHGDNF(SHGDN_INFOLDER.0 | SHGDN_FORPARSING.0),
                    &mut name,
                )
                .is_err()
            {
                continue;
            }
            let mut buffer = [0u16; 1024];
            if StrRetToBufW(&mut name, Some(pidl.0), &mut buffer).is_err() {
                continue;
            }
            let end = buffer.iter().position(|c| *c == 0).unwrap_or(buffer.len());
            let child = String::from_utf16_lossy(&buffer[..end]);
            if !missing.iter().any(|p| {
                p.file_name()
                    .is_some_and(|n| n.to_string_lossy().eq_ignore_ascii_case(&child))
            }) {
                continue;
            }
            let item: Result<IShellItem> = SHCreateItemWithParent(None, &folder, pidl.0);
            if let Ok(name) = item.and_then(|item| item.GetDisplayName(SIGDN_FILESYSPATH)) {
                let path = name.to_string().ok().map(PathBuf::from);
                CoTaskMemFree(Some(name.0.cast()));
                if let Some(index) = path.and_then(|path| {
                    missing
                        .iter()
                        .position(|p| crate::operations::same_path(&path, p))
                }) {
                    let wanted = missing.remove(index);
                    items.push((wanted.clone(), pidl));
                    if missing.is_empty() {
                        break;
                    }
                }
            }
        }
    }
    Ok(items)
}
unsafe fn position(view: &IFolderView2, pidl: &Pidl, point: POINT) -> Result<()> {
    let ptr = pidl.0 as *const ITEMIDLIST;
    view.SelectAndPositionItems(1, &ptr, Some(&point), SVSI_POSITIONITEM.0 as u32)
}
fn write_session(path: &Path, session: &Session) -> io::Result<()> {
    let tmp = path.with_extension("tmp");
    let mut file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(&tmp)?;
    file.write_all(&serde_json::to_vec(session)?)?;
    file.sync_all()?;
    drop(file);
    fs::rename(tmp, path)
}
fn read_session(path: &Path) -> io::Result<Session> {
    let bytes = fs::read(path)?;
    if bytes.len() > 8 * 1024 * 1024 {
        return Err(io::Error::other(i18n::t(K::IconJournalTooLarge)));
    }
    Ok(serde_json::from_slice(&bytes)?)
}
unsafe fn restore(path: &Path) -> io::Result<()> {
    if !path.exists() {
        return Ok(());
    }
    let session = read_session(path)?;
    if session.icons.is_empty() {
        fs::remove_file(path)?;
        return Ok(());
    }
    let desktop = view().map_err(|e| io::Error::other(e.to_string()))?;
    let paths: Vec<_> = session.icons.iter().map(|i| i.path.clone()).collect();
    let current = items(&desktop, &paths).map_err(|e| io::Error::other(e.to_string()))?;
    let mut failed = false;
    for icon in &session.icons {
        if let Some((_, pidl)) = current.iter().find(|(path, _)| path == &icon.path) {
            // A replacement at this path must not inherit our invisible
            // coordinate. Only reclaim the exact parking point we own; leave
            // a replacement that is already elsewhere completely alone.
            let parked = desktop
                .GetItemPosition(pidl.0)
                .is_ok_and(|p| p.x == PARK.x && p.y == PARK.y);
            if identity(&icon.path).ok().as_ref() != Some(&icon.identity) && !parked {
                continue;
            }
            if position(
                &desktop,
                pidl,
                POINT {
                    x: icon.x,
                    y: icon.y,
                },
            )
            .is_err()
            {
                failed = true;
            }
        } else if icon.path.exists() {
            failed = true;
        }
    }
    if !failed {
        desktop
            .SetCurrentFolderFlags(ARRANGE, session.flags)
            .map_err(|e| io::Error::other(e.to_string()))?;
        fs::remove_file(path)?;
    } else {
        return Err(io::Error::other(i18n::t(K::IconsPartlyRestored)));
    }
    Ok(())
}

pub struct DesktopIcons {
    path: PathBuf,
    session: Session,
    guardian: Child,
    pidls: Vec<(PathBuf, Pidl)>,
    desktop: IFolderView2,
    ready: bool,
    started: Instant,
    last_paths: Vec<PathBuf>,
    last_full_sync: Option<Instant>,
    last_complete: bool,
    probe_index: usize,
}
impl DesktopIcons {
    fn trace(&self, stage: &str) {
        if std::env::var_os("LUMADESK_ICON_TRACE").is_some() {
            if let Ok(mut file) = OpenOptions::new()
                .create(true)
                .append(true)
                .open(self.path.with_extension("trace"))
            {
                let _ = writeln!(file, "{:?} {stage}", std::time::SystemTime::now());
            }
        }
    }
    pub unsafe fn start(root: &Path) -> io::Result<Self> {
        // A previous watchdog may not have reached Explorer yet at sign-in.
        for entry in fs::read_dir(root)? {
            let entry = entry?;
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.starts_with("icon-session-") && name.ends_with(".json") {
                let previous = read_session(&entry.path())?;
                if let Ok(handle) = OpenProcess(PROCESS_SYNCHRONIZE, false, previous.owner) {
                    let live = WaitForSingleObject(handle, 0) == WAIT_TIMEOUT;
                    let _ = CloseHandle(handle);
                    if live {
                        continue;
                    }
                }
                restore(&entry.path())?;
            }
        }
        let owner = std::process::id();
        let path = root.join(format!(
            "icon-session-{owner}-{}.json",
            crate::model::unique_id()
        ));
        let desktop = view().map_err(|e| io::Error::other(e.to_string()))?;
        let session = Session {
            owner,
            flags: desktop
                .GetCurrentFolderFlags()
                .map_err(|e| io::Error::other(e.to_string()))?,
            icons: vec![],
        };
        write_session(&path, &session)?;
        let guardian = Command::new(std::env::current_exe()?)
            .arg("--desktop-guardian")
            .arg(owner.to_string())
            .arg(&path)
            .creation_flags(0x08000000)
            .spawn()?;
        Ok(Self {
            path,
            session,
            guardian,
            pidls: vec![],
            desktop,
            ready: false,
            started: Instant::now(),
            last_paths: vec![],
            last_full_sync: None,
            last_complete: false,
            probe_index: 0,
        })
    }

    pub unsafe fn sync(&mut self, paths: &[PathBuf]) -> io::Result<()> {
        self.trace("sync begin");
        if self.guardian.try_wait()?.is_some() {
            restore(&self.path)?;
            return Err(io::Error::other(i18n::t(K::GuardianStopped)));
        }
        if !self.ready {
            let ready = self.path.with_extension("ready");
            if ready.exists() {
                fs::remove_file(ready)?;
                self.ready = true;
            } else {
                return Err(io::Error::other(
                    if self.started.elapsed() < Duration::from_secs(10) {
                        i18n::t(K::GuardianPreparing)
                    } else {
                        i18n::t(K::GuardianNotReady)
                    },
                ));
            }
        }
        // Idle ticks only check the view and one rotating coordinate. Avoid
        // hundreds of canonicalize/identity/COM calls every three seconds.
        // A full audit still catches external replacements/rearrangement.
        if self.last_complete
            && self.last_paths == paths
            && self
                .last_full_sync
                .is_some_and(|t| t.elapsed() < Duration::from_secs(30))
        {
            if self.session.icons.is_empty() {
                return Ok(());
            }
            if self
                .desktop
                .GetCurrentFolderFlags()
                .is_ok_and(|f| f & ARRANGE == 0)
                && !self.pidls.is_empty()
            {
                self.probe_index = (self.probe_index + 1) % self.pidls.len();
                if self
                    .desktop
                    .GetItemPosition(self.pidls[self.probe_index].1 .0)
                    .is_ok_and(|p| p.x == PARK.x && p.y == PARK.y)
                {
                    return Ok(());
                }
            }
        }
        let desktops: Vec<_> = crate::collection::desktop_paths()?
            .iter()
            .map(|p| key(p))
            .collect();
        let desired: HashSet<_> = paths
            .iter()
            .map(|p| key(&p.canonicalize().unwrap_or_else(|_| p.to_owned())))
            .filter(|p| {
                p.parent()
                    .is_some_and(|parent| desktops.contains(&parent.to_owned()))
            })
            .collect();
        self.trace(&format!(
            "desired ready count={} roots={desktops:?} refs={paths:?}",
            desired.len()
        ));
        if desired.is_empty() && self.session.icons.is_empty() {
            self.last_paths = paths.to_vec();
            self.last_complete = true;
            self.last_full_sync = Some(Instant::now());
            return Ok(());
        }
        if self.desktop.GetCurrentFolderFlags().is_err() {
            self.desktop = view().map_err(|e| io::Error::other(e.to_string()))?;
            self.pidls.clear();
        }
        let desktop = self.desktop.clone();
        self.trace("view ready");
        let refresh = desired
            .iter()
            .chain(self.session.icons.iter().map(|i| &i.path))
            .any(|path| {
                self.pidls
                    .iter()
                    .find(|(p, _)| p == path)
                    .is_none_or(|(_, pidl)| desktop.GetItemPosition(pidl.0).is_err())
            });
        if refresh {
            self.trace("enumerate");
            let paths: HashSet<_> = desired
                .iter()
                .chain(self.session.icons.iter().map(|i| &i.path))
                .cloned()
                .collect();
            self.pidls = items(&desktop, &paths.into_iter().collect::<Vec<_>>())
                .map_err(|e| io::Error::other(e.to_string()))?;
        }
        self.trace("cache ready");
        if std::env::var_os("LUMADESK_ICON_TRACE").is_some() {
            self.trace(&format!(
                "cache paths={:?}",
                self.pidls
                    .iter()
                    .filter(|(p, _)| p.to_string_lossy().contains("LumaDesk-"))
                    .map(|(p, _)| p)
                    .collect::<Vec<_>>()
            ));
        }
        let current = &self.pidls;
        // Returning an icon is independent of the underlying file permissions.
        let mut retained = vec![];
        for icon in &self.session.icons {
            if desired.contains(&icon.path) {
                retained.push(icon.clone());
                continue;
            }
            if let Some((_, pidl)) = current.iter().find(|(path, _)| path == &icon.path) {
                if identity(&icon.path).ok().as_ref() == Some(&icon.identity) {
                    position(
                        &desktop,
                        pidl,
                        POINT {
                            x: icon.x,
                            y: icon.y,
                        },
                    )
                    .map_err(|e| io::Error::other(e.to_string()))?;
                } else if desktop
                    .GetItemPosition(pidl.0)
                    .is_ok_and(|p| p.x == PARK.x && p.y == PARK.y)
                {
                    position(
                        &desktop,
                        pidl,
                        POINT {
                            x: icon.x,
                            y: icon.y,
                        },
                    )
                    .map_err(|e| io::Error::other(e.to_string()))?;
                }
            } else if identity(&icon.path).ok().as_ref() == Some(&icon.identity) {
                retained.push(icon.clone());
            }
        }
        let mut changed = retained.len() != self.session.icons.len();
        self.session.icons = retained;
        for icon in &mut self.session.icons {
            if let Some((_, pidl)) = current.iter().find(|(path, _)| path == &icon.path) {
                if let Ok(expected) = identity(&icon.path) {
                    if expected != icon.identity {
                        if let Ok(point) = desktop.GetItemPosition(pidl.0) {
                            if point.x != PARK.x || point.y != PARK.y {
                                icon.x = point.x;
                                icon.y = point.y;
                            }
                        }
                        icon.identity = expected;
                        changed = true;
                    }
                }
            }
        }
        for (path, pidl) in current {
            if desired.contains(path) && !self.session.icons.iter().any(|icon| &icon.path == path) {
                if let (Ok(point), Ok(expected)) = (desktop.GetItemPosition(pidl.0), identity(path))
                {
                    self.session.icons.push(Icon {
                        path: path.clone(),
                        identity: expected,
                        x: point.x,
                        y: point.y,
                    });
                    changed = true;
                } else if std::env::var_os("LUMADESK_ICON_TRACE").is_some() {
                    self.trace(&format!(
                        "missing coordinate {path:?}: {:?}, identity {:?}",
                        desktop.GetItemPosition(pidl.0),
                        identity(path)
                    ));
                }
            }
        }
        // The original coordinates are durable before any icon is concealed.
        self.trace("positions ready");
        if changed {
            write_session(&self.path, &self.session)?;
        }
        if !self.session.icons.is_empty() {
            desktop
                .SetCurrentFolderFlags(ARRANGE, 0)
                .map_err(|e| io::Error::other(e.to_string()))?;
        } else {
            desktop
                .SetCurrentFolderFlags(ARRANGE, self.session.flags)
                .map_err(|e| io::Error::other(e.to_string()))?;
        }
        for icon in &self.session.icons {
            if !desired.contains(&icon.path) {
                continue;
            }
            if let Some((_, pidl)) = current.iter().find(|(path, _)| path == &icon.path) {
                if identity(&icon.path).ok().as_ref() == Some(&icon.identity) {
                    let actual = desktop
                        .GetItemPosition(pidl.0)
                        .map_err(|e| io::Error::other(e.to_string()))?;
                    if actual.x != PARK.x || actual.y != PARK.y {
                        position(&desktop, pidl, PARK)
                            .map_err(|e| io::Error::other(e.to_string()))?;
                    }
                }
            }
        }
        self.trace("sync done");
        self.last_paths = paths.to_vec();
        self.last_complete = desired.len()
            == self
                .session
                .icons
                .iter()
                .filter(|i| desired.contains(&i.path))
                .count();
        self.last_full_sync = Some(Instant::now());
        Ok(())
    }
    pub unsafe fn finish(&mut self) -> io::Result<()> {
        restore(&self.path)
    }
}
impl Drop for DesktopIcons {
    fn drop(&mut self) {
        unsafe {
            let _ = self.finish();
        }
    }
}

/// Invoked before the normal single-instance/UI path, with a live parent handle
/// acquired before acknowledging readiness. No polling of arbitrary programs.
pub unsafe fn guardian(owner: u32, path: &Path) -> io::Result<()> {
    let session = read_session(path)?;
    if session.owner != owner
        || !path
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .starts_with(&format!("icon-session-{owner}-"))
    {
        return Err(io::Error::other(i18n::t(K::InvalidIconSession)));
    }
    let handle = OpenProcess(PROCESS_SYNCHRONIZE, false, owner)
        .map_err(|e| io::Error::other(e.to_string()))?;
    fs::write(path.with_extension("ready"), b"ready")?;
    WaitForSingleObject(handle, INFINITE);
    let _ = CloseHandle(handle);
    OleInitialize(None).map_err(|e| io::Error::other(e.to_string()))?;
    // Explorer can be restarting when the UI terminates; retain the journal
    // and retry briefly rather than discarding the original positions.
    let mut result = Ok(());
    for _ in 0..30 {
        result = restore(path);
        if result.is_ok() {
            break;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    OleUninitialize();
    result
}
