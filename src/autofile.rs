//! Files new desktop items into groups. A background thread waits for
//! directory change notifications on both Desktop folders; the GUI thread
//! debounces them and scans. Items that existed when LumaDesk started (or
//! when the option was switched on) are never swept up, partial downloads
//! wait for their final name, and files are only filed once their size and
//! modification time have settled. Nothing is moved: groups hold references.
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};
use windows::core::PCWSTR;
use windows::Win32::Foundation::{HANDLE, HWND, LPARAM, WAIT_OBJECT_0, WPARAM};
use windows::Win32::Storage::FileSystem::{
    FindFirstChangeNotificationW, FindNextChangeNotification, FILE_NOTIFY_CHANGE_DIR_NAME,
    FILE_NOTIFY_CHANGE_FILE_NAME, FILE_NOTIFY_CHANGE_LAST_WRITE, FILE_NOTIFY_CHANGE_SIZE,
};
use windows::Win32::System::Threading::{WaitForMultipleObjects, INFINITE};
use windows::Win32::UI::WindowsAndMessaging::PostMessageW;

/// A file must look unchanged for this long before it is filed.
const SETTLE: Duration = Duration::from_millis(1500);

/// Browser/download-manager temporaries and Office lock files.
pub fn is_partial(path: &Path) -> bool {
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().to_lowercase())
        .unwrap_or_default();
    name.starts_with("~$")
        || [
            ".crdownload",
            ".part",
            ".partial",
            ".download",
            ".tmp",
            ".temp",
            ".opdownload",
            ".!ut",
            ".aria2",
        ]
        .iter()
        .any(|suffix| name.ends_with(suffix))
}

fn entries(dirs: &[PathBuf]) -> HashSet<PathBuf> {
    dirs.iter()
        .filter_map(|dir| fs::read_dir(dir).ok())
        .flatten()
        .filter_map(|entry| entry.ok().map(|e| e.path()))
        .collect()
}

fn signature(path: &Path) -> Option<(u64, SystemTime)> {
    let metadata = fs::symlink_metadata(path).ok()?;
    Some((metadata.len(), metadata.modified().ok()?))
}

pub struct AutoFiler {
    known: HashSet<PathBuf>,
    pending: HashMap<PathBuf, ((u64, SystemTime), Instant)>,
}

impl AutoFiler {
    /// Everything already on the desktop counts as known.
    pub fn new(dirs: &[PathBuf]) -> Self {
        Self {
            known: entries(dirs),
            pending: HashMap::new(),
        }
    }

    /// Whether new items are still settling (scan again later).
    pub fn waiting(&self) -> bool {
        !self.pending.is_empty()
    }

    /// New, settled desktop items to file now.
    pub fn scan(&mut self, dirs: &[PathBuf], now: Instant) -> Vec<PathBuf> {
        let current = entries(dirs);
        // Forget deleted items so a new file with the same name is filed.
        self.known.retain(|p| current.contains(p));
        self.pending.retain(|p, _| current.contains(p));
        let mut ready = Vec::new();
        for path in current {
            if self.known.contains(&path) || is_partial(&path) {
                continue;
            }
            let Some(sig) = signature(&path) else {
                continue;
            };
            match self.pending.get(&path) {
                Some(&(seen, since)) if seen == sig => {
                    if now.duration_since(since) >= SETTLE {
                        self.pending.remove(&path);
                        self.known.insert(path.clone());
                        ready.push(path);
                    }
                }
                _ => {
                    self.pending.insert(path, (sig, now));
                }
            }
        }
        ready.sort();
        ready
    }
}

/// Posts `message` to `notify` whenever either folder changes. The thread
/// lives for the rest of the process and never touches the files.
pub fn watch(dirs: Vec<PathBuf>, notify: HWND, message: u32) {
    let target = notify.0 as usize;
    std::thread::spawn(move || unsafe {
        let handles: Vec<HANDLE> = dirs
            .iter()
            .filter_map(|dir| {
                let shell = crate::shell_icons::shell_path(&dir.to_string_lossy());
                let wide: Vec<u16> = shell.encode_utf16().chain(Some(0)).collect();
                FindFirstChangeNotificationW(
                    PCWSTR(wide.as_ptr()),
                    false,
                    FILE_NOTIFY_CHANGE_FILE_NAME
                        | FILE_NOTIFY_CHANGE_DIR_NAME
                        | FILE_NOTIFY_CHANGE_SIZE
                        | FILE_NOTIFY_CHANGE_LAST_WRITE,
                )
                .ok()
            })
            .collect();
        if handles.is_empty() {
            return;
        }
        loop {
            let signaled = WaitForMultipleObjects(&handles, false, INFINITE);
            let index = signaled.0.wrapping_sub(WAIT_OBJECT_0.0) as usize;
            let Some(handle) = handles.get(index) else {
                return;
            };
            let target = HWND(target as *mut _);
            if PostMessageW(Some(target), message, WPARAM(0), LPARAM(0)).is_err() {
                return;
            }
            if FindNextChangeNotification(*handle).is_err() {
                return;
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn partial_downloads_and_lock_files_wait() {
        for name in [
            "setup.exe.crdownload",
            "movie.mp4.part",
            "~$Report.docx",
            "x.TMP",
        ] {
            assert!(is_partial(Path::new(name)), "{name}");
        }
        for name in ["setup.exe", ".env", "nginx.conf", "partial.txt"] {
            assert!(!is_partial(Path::new(name)), "{name}");
        }
    }

    #[test]
    fn only_new_settled_items_are_filed_once() {
        let dir = std::env::temp_dir().join(format!("luma-autofile-{}", crate::model::unique_id()));
        fs::create_dir(&dir).unwrap();
        fs::write(dir.join("existing.txt"), b"old").unwrap();
        let dirs = vec![dir.clone()];
        let mut filer = AutoFiler::new(&dirs);
        let start = Instant::now();
        fs::write(dir.join("new.txt"), b"new").unwrap();
        fs::write(dir.join("big.zip.crdownload"), b"...").unwrap();
        // First sighting only starts the settle timer.
        assert!(filer.scan(&dirs, start).is_empty());
        assert!(filer.waiting());
        // Still settling.
        assert!(filer
            .scan(&dirs, start + Duration::from_millis(500))
            .is_empty());
        let ready = filer.scan(&dirs, start + SETTLE);
        assert_eq!(ready, vec![dir.join("new.txt")]);
        // Never filed twice; the existing file and the partial are ignored.
        assert!(filer.scan(&dirs, start + SETTLE * 3).is_empty());
        // The download completes under its final name.
        fs::rename(dir.join("big.zip.crdownload"), dir.join("big.zip")).unwrap();
        let t = start + SETTLE * 4;
        assert!(filer.scan(&dirs, t).is_empty());
        assert_eq!(filer.scan(&dirs, t + SETTLE), vec![dir.join("big.zip")]);
        // A file that keeps growing restarts its settle timer.
        fs::write(dir.join("growing.log"), b"1").unwrap();
        let t = t + SETTLE * 2;
        assert!(filer.scan(&dirs, t).is_empty());
        fs::write(dir.join("growing.log"), b"12345").unwrap();
        assert!(filer.scan(&dirs, t + SETTLE).is_empty());
        assert_eq!(
            filer.scan(&dirs, t + SETTLE * 2),
            vec![dir.join("growing.log")]
        );
        fs::remove_dir_all(dir).unwrap();
    }
}
