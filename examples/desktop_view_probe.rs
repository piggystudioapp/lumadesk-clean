//! Read-only validation of exact fixture icon coordinates in an isolated smoke
//! session. COM references are dropped before the apartment is uninitialized.
use serde_json::Value;
use std::{fs, path::PathBuf};
use windows::core::Interface;
use windows::Win32::System::{Com::*, Ole::*, Variant::VARIANT};
use windows::Win32::UI::Shell::*;
fn key(path: &str) -> PathBuf {
    PathBuf::from(path.strip_prefix("\\\\?\\").unwrap_or(path))
}
struct Ole;
impl Drop for Ole {
    fn drop(&mut self) {
        unsafe { OleUninitialize() }
    }
}
fn main() -> Result<(), Box<dyn std::error::Error>> {
    unsafe { OleInitialize(None)? };
    let _ole = Ole;
    let args: Vec<_> = std::env::args().skip(1).collect();
    let session: Value = serde_json::from_slice(&fs::read(&args[1])?)?;
    unsafe {
        let shell: IShellWindows = CoCreateInstance(&ShellWindows, None, CLSCTX_LOCAL_SERVER)?;
        let mut hwnd = 0;
        let empty = VARIANT::default();
        let provider: IServiceProvider = shell
            .FindWindowSW(&empty, &empty, SWC_DESKTOP, &mut hwnd, SWFO_NEEDDISPATCH)?
            .cast()?;
        let browser: IShellBrowser = provider.QueryService(&SID_STopLevelBrowser)?;
        let view: IFolderView2 = browser.QueryActiveShellView()?.cast()?;
        let folder: IShellFolder = view.GetFolder()?;
        let mut actual = std::collections::HashMap::new();
        // Independent verification enumerates actual view-relative PIDLs,
        // including duplicate names in personal and Public Desktop roots.
        for index in 0..view.ItemCount(SVGIO_ALLVIEW)? {
            let pidl = view.Item(index)?;
            let item: windows::core::Result<IShellItem> =
                SHCreateItemWithParent(None, &folder, pidl);
            if let Ok(name) = item.and_then(|item| item.GetDisplayName(SIGDN_FILESYSPATH)) {
                let path = name.to_string().ok().map(|p| key(&p));
                CoTaskMemFree(Some(name.0.cast()));
                if let Some(path) = path {
                    let point = view.GetItemPosition(pidl)?;
                    actual.insert(path, (point.x, point.y));
                }
            }
            CoTaskMemFree(Some(pidl.cast()));
        }
        let icons = session["icons"].as_array().ok_or("Missing icon snapshot")?;
        if icons.is_empty() {
            return Err("No icon fixture positions recorded".into());
        }
        let saved_flags = session["flags"].as_u64().unwrap() as u32;
        let arrange = FWF_AUTOARRANGE.0 as u32 | FWF_SNAPTOGRID.0 as u32;
        if args[0] != "hidden" && view.GetCurrentFolderFlags()? & arrange != saved_flags & arrange {
            return Err("Original desktop arrange/grid flags were not restored".into());
        }
        // Re-enabling Explorer's snap-to-grid can align a just-created icon
        // whose initial coordinate had not yet snapped. Bound that adjustment
        // to half a measured grid cell; otherwise require exact coordinates.
        let mut spacing = windows::Win32::Foundation::POINT::default();
        view.GetSpacing(&mut spacing)?;
        let tolerance = if args[0] != "hidden" && saved_flags & FWF_SNAPTOGRID.0 as u32 != 0 {
            (spacing.x.abs() / 2, spacing.y.abs() / 2)
        } else {
            (0, 0)
        };
        for icon in icons {
            let path = key(icon["path"].as_str().unwrap());
            let expected = if args[0] == "hidden" {
                (-1000000, -1000000)
            } else {
                (
                    icon["x"].as_i64().unwrap() as i32,
                    icon["y"].as_i64().unwrap() as i32,
                )
            };
            if !actual.get(&path).is_some_and(|p| {
                (p.0 - expected.0).abs() <= tolerance.0 && (p.1 - expected.1).abs() <= tolerance.1
            }) {
                return Err(format!(
                    "Unexpected icon coordinate for {path:?}: {:?}, expected {expected:?}",
                    actual.get(&path)
                )
                .into());
            }
        }
        println!(
            "ICON_DISPLAY_OK mode={} count={} files=untouched",
            args[0],
            icons.len()
        );
    }
    Ok(())
}
