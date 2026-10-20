//! Desktop locations, plus read-only support for layouts written by releases
//! that moved desktop items into collections\ (retired in 0.6).
use crate::model::{Fence, Layout};
use serde::Deserialize;
use std::fs;
use std::io;
use std::os::windows::ffi::OsStringExt;
use std::path::{Component, Path, PathBuf};
use windows::Win32::System::Com::CoTaskMemFree;
use windows::Win32::UI::Shell::*;

#[derive(Deserialize)]
struct Receipt {
    version: u32,
    fence_id: u64,
    original: PathBuf,
    name: PathBuf,
    #[serde(default)]
    restore_to: Option<PathBuf>,
}

pub fn desktop_paths() -> io::Result<Vec<PathBuf>> {
    let mut paths = Vec::new();
    for id in [FOLDERID_Desktop, FOLDERID_PublicDesktop] {
        unsafe {
            if let Ok(value) = SHGetKnownFolderPath(&id, KF_FLAG_DEFAULT, None) {
                let path = PathBuf::from(std::ffi::OsString::from_wide(value.as_wide()));
                CoTaskMemFree(Some(value.0.cast()));
                if let Ok(path) = path.canonicalize() {
                    paths.push(path);
                }
            }
        }
    }
    if paths.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            "Cannot locate the Windows desktop",
        ));
    }
    Ok(paths)
}

pub fn is_desktop_item(path: &Path, desktops: &[PathBuf]) -> bool {
    path.is_absolute()
        && path.file_name().is_some()
        && path
            .parent()
            .and_then(|p| p.canonicalize().ok())
            .is_some_and(|parent| {
                desktops
                    .iter()
                    .any(|desktop| desktop.canonicalize().is_ok_and(|path| path == parent))
            })
        && path
            .file_name()
            .is_some_and(|name| !name.eq_ignore_ascii_case("desktop.ini"))
}

fn read_receipt(directory: &Path) -> io::Result<Receipt> {
    let receipt: Receipt = serde_json::from_slice(&fs::read(directory.join("receipt.json"))?)?;
    let mut components = receipt.name.components();
    if receipt.version != 1
        || !receipt.original.is_absolute()
        || !matches!(components.next(), Some(Component::Normal(_)))
        || components.next().is_some()
        || receipt
            .restore_to
            .as_ref()
            .is_some_and(|p| !p.is_absolute())
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "Invalid collection receipt",
        ));
    }
    Ok(receipt)
}

/// Version 1 layouts listed original desktop paths; files collected by those
/// releases live under collections\ with a receipt. Point the layout at the
/// stored copies. Read only: no file is moved.
pub fn legacy_paths(root: &Path, layout: &mut Layout) -> io::Result<()> {
    let collections = root.join("collections");
    if !collections.exists() {
        return Ok(());
    }
    for fence in fs::read_dir(collections)? {
        let fence = fence?.path();
        if !fs::symlink_metadata(&fence)?.is_dir() {
            continue;
        }
        for entry in fs::read_dir(fence)? {
            let directory = entry?.path();
            if !fs::symlink_metadata(&directory)?.is_dir() {
                continue;
            }
            if !directory.join("receipt.json").exists() {
                continue;
            }
            let receipt = read_receipt(&directory)?;
            let stored = directory.join("files").join(&receipt.name);
            if stored.try_exists()? {
                if !layout.fences.iter().any(|f| f.id == receipt.fence_id) {
                    layout.fences.push(Fence {
                        id: receipt.fence_id,
                        title: "Recovered collection".into(),
                        ..Layout::default().fences.remove(0)
                    });
                }
                for fence in &mut layout.fences {
                    for path in &mut fence.paths {
                        if *path == receipt.original && !receipt.original.exists() {
                            *path = stored.clone();
                        }
                    }
                }
                let fence = layout
                    .fences
                    .iter_mut()
                    .find(|f| f.id == receipt.fence_id)
                    .expect("created");
                if !fence.paths.contains(&stored) {
                    fence.paths.push(stored);
                }
            } else if receipt
                .restore_to
                .as_ref()
                .is_some_and(|target| target.exists())
            {
                for fence in &mut layout.fences {
                    fence.paths.retain(|path| path != &stored);
                }
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn legacy_item(root: &Path, fence: u64, original: &Path, restored: Option<&Path>) -> PathBuf {
        let directory = root
            .join("collections")
            .join(fence.to_string())
            .join("1-1-0");
        fs::create_dir_all(directory.join("files")).unwrap();
        let name = original.file_name().unwrap().to_string_lossy().into_owned();
        let receipt = serde_json::json!({
            "version": 1,
            "fence_id": fence,
            "original": original,
            "name": &name,
            "restore_to": restored,
        });
        fs::write(directory.join("receipt.json"), receipt.to_string()).unwrap();
        directory.join("files").join(name)
    }

    #[test]
    fn version_one_layout_points_at_collected_copies_without_moving_them() {
        let root = std::env::temp_dir().join(format!("luma-legacy-{}", crate::model::unique_id()));
        let original = root.join("Desktop").join("app.lnk");
        let stored = legacy_item(&root, 77, &original, None);
        fs::write(&stored, "shortcut bytes").unwrap();
        let mut layout = Layout::default();
        layout.fences[0].paths.push(original.clone());
        legacy_paths(&root, &mut layout).unwrap();
        legacy_paths(&root, &mut layout).unwrap();
        assert_eq!(layout.fences[0].paths, vec![stored.clone()]);
        let recovered = layout.fences.iter().find(|f| f.id == 77).unwrap();
        assert_eq!(recovered.paths, vec![stored.clone()]);
        assert_eq!(fs::read_to_string(&stored).unwrap(), "shortcut bytes");
        assert!(!original.exists());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn restored_or_invalid_receipts_never_add_paths() {
        let root = std::env::temp_dir().join(format!("luma-legacy-{}", crate::model::unique_id()));
        let desktop = root.join("Desktop");
        fs::create_dir_all(&desktop).unwrap();
        let restored = desktop.join("back.txt");
        fs::write(&restored, "home").unwrap();
        let stored = legacy_item(&root, 1, &restored, Some(&restored));
        let mut layout = Layout::default();
        layout.fences[0].paths.push(stored.clone());
        legacy_paths(&root, &mut layout).unwrap();
        assert!(layout.fences[0].paths.is_empty());
        let bad = root.join("collections/2/bad");
        fs::create_dir_all(&bad).unwrap();
        fs::write(
            bad.join("receipt.json"),
            r#"{"version":1,"fence_id":2,"original":"rel","name":"..\\x"}"#,
        )
        .unwrap();
        assert!(legacy_paths(&root, &mut layout).is_err());
        assert_eq!(fs::read_to_string(restored).unwrap(), "home");
        fs::remove_dir_all(root).unwrap();
    }
}
