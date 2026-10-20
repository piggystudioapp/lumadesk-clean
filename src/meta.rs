//! File details for the Details view and sorting, read on a background
//! thread (disk and Shell calls never run in WM_PAINT). Results are cached
//! and refreshed when older than REFRESH, so changes appear on their own.
use crate::i18n::{self, K};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{mpsc, Arc, Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime};
use windows::core::PCWSTR;
use windows::Win32::Foundation::{FILETIME, SYSTEMTIME};
use windows::Win32::Globalization::{
    GetDateFormatEx, GetTimeFormatEx, DATE_SHORTDATE, TIME_NOSECONDS,
};
use windows::Win32::Storage::FileSystem::{
    FileTimeToLocalFileTime, FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_NORMAL,
};
use windows::Win32::System::Ole::{OleInitialize, OleUninitialize};
use windows::Win32::System::Time::FileTimeToSystemTime;
use windows::Win32::UI::Shell::{
    SHGetFileInfoW, SHFILEINFOW, SHGFI_TYPENAME, SHGFI_USEFILEATTRIBUTES,
};

const REFRESH: Duration = Duration::from_secs(30);

#[derive(Clone, Debug, Default)]
pub struct Meta {
    pub modified: Option<SystemTime>,
    /// None for folders.
    pub size: Option<u64>,
    /// Explorer's type name, e.g. "文字文件" or "檔案資料夾".
    pub type_name: String,
}

enum State {
    Pending,
    Ready(Arc<Meta>, Instant),
}

struct Service {
    requests: mpsc::Sender<PathBuf>,
    states: Arc<Mutex<HashMap<PathBuf, State>>>,
}

fn service() -> &'static Service {
    static SERVICE: OnceLock<Service> = OnceLock::new();
    SERVICE.get_or_init(|| {
        let (requests, receiver) = mpsc::channel::<PathBuf>();
        let states: Arc<Mutex<HashMap<PathBuf, State>>> = Arc::new(Mutex::new(HashMap::new()));
        let worker = states.clone();
        std::thread::spawn(move || {
            let initialized = unsafe { OleInitialize(None) }.is_ok();
            let mut types: HashMap<String, String> = HashMap::new();
            for path in receiver {
                let meta = read(&path, &mut types);
                worker
                    .lock()
                    .unwrap()
                    .insert(path, State::Ready(Arc::new(meta), Instant::now()));
            }
            if initialized {
                unsafe { OleUninitialize() };
            }
        });
        Service { requests, states }
    })
}

/// Cached details, or None while they are being read. Stale entries are
/// returned and refreshed in the background.
pub fn get(path: &Path) -> Option<Arc<Meta>> {
    let service = service();
    let mut states = service.states.lock().unwrap();
    match states.get(path) {
        Some(State::Ready(meta, at)) => {
            let meta = meta.clone();
            if at.elapsed() > REFRESH {
                let at = Instant::now();
                states.insert(path.to_owned(), State::Ready(meta.clone(), at));
                let _ = service.requests.send(path.to_owned());
            }
            Some(meta)
        }
        Some(State::Pending) => None,
        None => {
            states.insert(path.to_owned(), State::Pending);
            let _ = service.requests.send(path.to_owned());
            None
        }
    }
}

fn read(path: &Path, types: &mut HashMap<String, String>) -> Meta {
    let metadata = std::fs::metadata(path).ok();
    let dir = metadata.as_ref().is_some_and(|m| m.is_dir());
    let extension = if dir {
        ":folder".to_owned()
    } else {
        path.extension()
            .map(|e| e.to_string_lossy().to_lowercase())
            .unwrap_or_default()
    };
    let type_name = types
        .entry(extension.clone())
        .or_insert_with(|| type_name(&extension, dir))
        .clone();
    Meta {
        modified: metadata.as_ref().and_then(|m| m.modified().ok()),
        size: metadata.filter(|m| !m.is_dir()).map(|m| m.len()),
        type_name,
    }
}

/// Shell type name by extension only (no file access).
fn type_name(extension: &str, dir: bool) -> String {
    let probe = if dir {
        r"C:\LumaDeskFolder".to_owned()
    } else {
        format!(r"C:\LumaDeskFile.{extension}")
    };
    let wide: Vec<u16> = probe.encode_utf16().chain(Some(0)).collect();
    let mut info = SHFILEINFOW::default();
    let ok = unsafe {
        SHGetFileInfoW(
            PCWSTR(wide.as_ptr()),
            if dir {
                FILE_ATTRIBUTE_DIRECTORY
            } else {
                FILE_ATTRIBUTE_NORMAL
            },
            Some(&mut info),
            std::mem::size_of::<SHFILEINFOW>() as u32,
            SHGFI_TYPENAME | SHGFI_USEFILEATTRIBUTES,
        )
    };
    let end = info
        .szTypeName
        .iter()
        .position(|&c| c == 0)
        .unwrap_or(info.szTypeName.len());
    if ok != 0 && end > 0 {
        String::from_utf16_lossy(&info.szTypeName[..end])
    } else if dir {
        i18n::t(K::Folder).to_owned()
    } else {
        i18n::tf(K::FileType, &[&extension.to_uppercase()])
    }
}

/// Explorer-like size: whole kilobytes, rounded up, with the current
/// language's thousands separator.
pub fn format_size(size: Option<u64>) -> String {
    let Some(bytes) = size else {
        return String::new();
    };
    group_digits(bytes.div_ceil(1024), i18n::group_separator()) + " KB"
}

fn group_digits(value: u64, separator: &str) -> String {
    let digits = value.to_string();
    let mut grouped = String::new();
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i) % 3 == 0 {
            grouped.push_str(separator);
        }
        grouped.push(c);
    }
    grouped
}

/// Local short date and time (no seconds) in the selected language's
/// format, e.g. "2026/10/20 下午 03:04" or "10/20/2026 3:04 PM".
pub fn format_time(time: Option<SystemTime>) -> String {
    let Some(time) = time else {
        return String::new();
    };
    let Ok(since) = time.duration_since(SystemTime::UNIX_EPOCH) else {
        return String::new();
    };
    // FILETIME counts 100 ns intervals since 1601-01-01.
    let ticks = since.as_nanos() as u64 / 100 + 116_444_736_000_000_000;
    let utc = FILETIME {
        dwLowDateTime: ticks as u32,
        dwHighDateTime: (ticks >> 32) as u32,
    };
    let mut local = FILETIME::default();
    let mut st = SYSTEMTIME::default();
    unsafe {
        if FileTimeToLocalFileTime(&utc, &mut local).is_err()
            || FileTimeToSystemTime(&local, &mut st).is_err()
        {
            return String::new();
        }
        let locale = i18n::locale();
        let mut date = [0u16; 80];
        let mut clock = [0u16; 80];
        let d = GetDateFormatEx(
            locale,
            DATE_SHORTDATE,
            Some(&st),
            PCWSTR::null(),
            Some(&mut date),
            PCWSTR::null(),
        );
        let c = GetTimeFormatEx(
            locale,
            TIME_NOSECONDS,
            Some(&st),
            PCWSTR::null(),
            Some(&mut clock),
        );
        if d <= 1 || c <= 1 {
            return format!(
                "{}-{:02}-{:02} {:02}:{:02}",
                st.wYear, st.wMonth, st.wDay, st.wHour, st.wMinute
            );
        }
        format!(
            "{} {}",
            String::from_utf16_lossy(&date[..d as usize - 1]),
            String::from_utf16_lossy(&clock[..c as usize - 1])
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes_round_up_to_kilobytes_with_separators() {
        assert_eq!(format_size(None), "");
        assert_eq!(format_size(Some(0)), "0 KB");
        assert_eq!(format_size(Some(1)), "1 KB");
        assert_eq!(format_size(Some(1024)), "1 KB");
        assert_eq!(format_size(Some(1025)), "2 KB");
        assert_eq!(format_size(Some(5_838 * 1024)), "5,838 KB");
        assert_eq!(format_size(Some(776_223 * 1024)), "776,223 KB");
        assert_eq!(group_digits(1_234_567, "."), "1.234.567");
        assert_eq!(group_digits(999, "."), "999");
    }

    #[test]
    fn details_are_read_in_the_background() {
        let file =
            std::env::temp_dir().join(format!("luma-meta-{}.txt", crate::model::unique_id()));
        std::fs::write(&file, vec![b'x'; 3000]).unwrap();
        let mut meta = get(&file);
        for _ in 0..200 {
            if meta.is_some() {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
            meta = get(&file);
        }
        let meta = meta.expect("metadata arrives");
        assert_eq!(meta.size, Some(3000));
        assert!(meta.modified.is_some());
        assert!(!meta.type_name.is_empty());
        assert!(!format_time(meta.modified).is_empty());
        std::fs::remove_file(file).unwrap();
    }
}
