//! Shell extensions may block. Resolve icons on a dedicated apartment, never UI.
use std::collections::HashMap;
use std::sync::{mpsc, Arc, Mutex, OnceLock};
use windows::core::PCWSTR;
use windows::Win32::Storage::FileSystem::{FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_NORMAL};
use windows::Win32::System::Ole::{OleInitialize, OleUninitialize};
use windows::Win32::UI::Controls::{IImageList, ILD_TRANSPARENT};
use windows::Win32::UI::Shell::{
    SHGetFileInfoW, SHGetImageList, SHFILEINFOW, SHGFI_FLAGS, SHGFI_ICON, SHGFI_OVERLAYINDEX,
    SHGFI_SYSICONINDEX, SHGFI_USEFILEATTRIBUTES, SHIL_EXTRALARGE, SHIL_JUMBO, SHIL_LARGE,
    SHIL_SMALL,
};
use windows::Win32::UI::WindowsAndMessaging::{DestroyIcon, HICON};

pub struct NativeIcon(usize);
impl NativeIcon {
    pub fn handle(&self) -> HICON {
        HICON(self.0 as *mut _)
    }
}
impl Drop for NativeIcon {
    fn drop(&mut self) {
        unsafe {
            let _ = DestroyIcon(self.handle());
        }
    }
}

enum State {
    Pending,
    Ready(Option<Arc<NativeIcon>>),
}
struct IconService {
    requests: mpsc::Sender<String>,
    states: Arc<Mutex<HashMap<String, State>>>,
}
impl IconService {
    fn start(loader: impl Fn(&str) -> Option<NativeIcon> + Send + 'static) -> Self {
        let (requests, receiver) = mpsc::channel::<String>();
        let states = Arc::new(Mutex::new(HashMap::new()));
        let worker_states = states.clone();
        std::thread::spawn(move || {
            let initialized = unsafe { OleInitialize(None) }.is_ok();
            for key in receiver {
                let icon = initialized.then(|| loader(&key)).flatten().map(Arc::new);
                worker_states
                    .lock()
                    .unwrap()
                    .insert(key, State::Ready(icon));
            }
            if initialized {
                unsafe {
                    OleUninitialize();
                }
            }
        });
        Self { requests, states }
    }

    fn request(&self, key: &str) -> Option<Option<Arc<NativeIcon>>> {
        let mut states = self.states.lock().unwrap();
        match states.get(key) {
            Some(State::Ready(icon)) => Some(icon.clone()),
            Some(State::Pending) => None,
            None => {
                states.insert(key.into(), State::Pending);
                if self.requests.send(key.into()).is_err() {
                    states.insert(key.into(), State::Ready(None));
                    return Some(None);
                }
                None
            }
        }
    }
}

pub fn request(key: &str) -> Option<Option<Arc<NativeIcon>>> {
    static SERVICES: OnceLock<[IconService; 2]> = OnceLock::new();
    let index = key
        .bytes()
        .fold(0usize, |n, b| n.wrapping_mul(31).wrapping_add(b as usize))
        % 2;
    SERVICES.get_or_init(|| [IconService::start(load), IconService::start(load)])[index]
        .request(key)
}

pub fn shell_path(path: &str) -> String {
    if let Some(tail) = path.strip_prefix(r"\\?\UNC\") {
        format!(r"\\{tail}")
    } else {
        path.strip_prefix(r"\\?\").unwrap_or(path).into()
    }
}

/// Icon edge to request for a drawn size (Shell image lists: 16, 32, 48, 256).
pub fn icon_class(pixels: i32) -> u32 {
    match pixels {
        p if p <= 16 => 16,
        p if p <= 32 => 32,
        p if p <= 48 => 48,
        _ => 256,
    }
}

/// Cache key "<item key>@<edge>" so each size is resolved once.
pub fn sized_key(key: &str, class: u32) -> String {
    format!("{key}@{class}")
}

fn load(sized: &str) -> Option<NativeIcon> {
    let (key, class) = match sized.rsplit_once('@') {
        Some((key, class)) if class.parse::<u32>().is_ok() => (key, class.parse().unwrap()),
        _ => (sized, 48),
    };
    let list = match class {
        16 => SHIL_SMALL,
        32 => SHIL_LARGE,
        48 => SHIL_EXTRALARGE,
        _ => SHIL_JUMBO,
    };
    let specific = key.strip_prefix(":path:");
    let folder = key == ":folder";
    let path = specific.map(shell_path).unwrap_or_else(|| {
        if folder {
            r"C:\LumaDeskFolder".into()
        } else {
            format!(r"C:\LumaDeskFile.{key}")
        }
    });
    let wide: Vec<_> = path.encode_utf16().chain(Some(0)).collect();
    let attributes = if folder {
        FILE_ATTRIBUTE_DIRECTORY
    } else {
        FILE_ATTRIBUTE_NORMAL
    };
    let generic = if specific.is_some() {
        SHGFI_FLAGS(0)
    } else {
        SHGFI_USEFILEATTRIBUTES
    };
    // Desktop-sized (48 px) artwork from the system image list, including the
    // shortcut arrow overlay. Falls back to the 32 px icon below.
    let mut info = SHFILEINFOW::default();
    let indexed = unsafe {
        SHGetFileInfoW(
            PCWSTR(wide.as_ptr()),
            attributes,
            Some(&mut info),
            std::mem::size_of::<SHFILEINFOW>() as u32,
            // The overlay index is only reported together with SHGFI_ICON.
            SHGFI_SYSICONINDEX | SHGFI_OVERLAYINDEX | SHGFI_ICON | generic,
        )
    };
    if !info.hIcon.is_invalid() {
        unsafe {
            let _ = DestroyIcon(info.hIcon);
        }
    }
    if indexed != 0 {
        let index = info.iIcon & 0x00ff_ffff;
        let overlay = ((info.iIcon >> 24) & 0x0f) as u32;
        let icon = unsafe {
            SHGetImageList::<IImageList>(list as i32)
                .and_then(|list| list.GetIcon(index, ILD_TRANSPARENT.0 | (overlay << 8)))
        };
        if let Ok(icon) = icon {
            if !icon.is_invalid() {
                return Some(NativeIcon(icon.0 as usize));
            }
        }
    }
    let mut info = SHFILEINFOW::default();
    let ok = unsafe {
        SHGetFileInfoW(
            PCWSTR(wide.as_ptr()),
            if folder {
                FILE_ATTRIBUTE_DIRECTORY
            } else {
                FILE_ATTRIBUTE_NORMAL
            },
            Some(&mut info),
            std::mem::size_of::<SHFILEINFOW>() as u32,
            if specific.is_some() {
                SHGFI_ICON
            } else {
                SHGFI_ICON | SHGFI_USEFILEATTRIBUTES
            },
        )
    };
    if ok != 0 && !info.hIcon.is_invalid() {
        Some(NativeIcon(info.hIcon.0 as usize))
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn sizes_map_to_shell_image_lists_and_keys_round_trip() {
        assert_eq!(
            [12, 16, 18, 24, 32, 40, 48, 64, 128].map(icon_class),
            [16, 16, 32, 32, 32, 48, 48, 256, 256]
        );
        assert_eq!(sized_key(r":path:C:\a@b.lnk", 48), r":path:C:\a@b.lnk@48");
    }

    #[test]
    fn normalizes_filesystem_prefixes_for_legacy_shell_apis() {
        assert_eq!(shell_path(r"\\?\C:\中文\程式.lnk"), r"C:\中文\程式.lnk");
        assert_eq!(
            shell_path(r"\\?\UNC\server\share\程式.lnk"),
            r"\\server\share\程式.lnk"
        );
        assert_eq!(shell_path(r"C:\Apps\工具.exe"), r"C:\Apps\工具.exe");
    }

    #[test]
    fn blocked_shell_lookup_never_blocks_requests_or_queues_duplicates() {
        let (entered, started) = mpsc::channel();
        let (release, gate) = mpsc::channel();
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let count = calls.clone();
        let service = IconService::start(move |_| {
            count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            entered.send(()).unwrap();
            gate.recv().unwrap();
            None
        });
        assert!(service.request("slow shortcut").is_none());
        started
            .recv_timeout(std::time::Duration::from_secs(5))
            .unwrap();
        assert!(service.request("slow shortcut").is_none());
        assert!(service.request("another icon").is_none());
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);
        release.send(()).unwrap();
        started
            .recv_timeout(std::time::Duration::from_secs(5))
            .unwrap();
        assert!(matches!(service.request("slow shortcut"), Some(None)));
        release.send(()).unwrap();
    }

    #[test]
    fn actual_extended_shortcut_path_returns_native_icon() {
        use windows::core::Interface;
        use windows::Win32::System::Com::{CoCreateInstance, IPersistFile, CLSCTX_INPROC_SERVER};
        use windows::Win32::UI::Shell::{IShellLinkW, ShellLink};
        unsafe {
            OleInitialize(None).unwrap();
            let path =
                std::env::temp_dir().join(format!("luma-icon-{}.lnk", crate::model::unique_id()));
            let link: IShellLinkW =
                CoCreateInstance(&ShellLink, None, CLSCTX_INPROC_SERVER).unwrap();
            let target: Vec<_> =
                format!(r"{}\System32\notepad.exe", std::env::var("WINDIR").unwrap())
                    .encode_utf16()
                    .chain(Some(0))
                    .collect();
            link.SetPath(PCWSTR(target.as_ptr())).unwrap();
            let persist: IPersistFile = link.cast().unwrap();
            let wide: Vec<_> = path
                .to_string_lossy()
                .encode_utf16()
                .chain(Some(0))
                .collect();
            persist.Save(PCWSTR(wide.as_ptr()), true).unwrap();
            let extended = path.canonicalize().unwrap();
            let icon = load(&format!(":path:{}", extended.display()));
            assert!(icon.is_some());
            drop(icon);
            drop(persist);
            drop(link);
            std::fs::remove_file(path).unwrap();
            OleUninitialize();
        }
    }
}
