//! Frosted-glass backgrounds without DWM: groups are children of Explorer's
//! desktop view, where DWM backdrops do not apply, so the wallpaper of each
//! monitor is decoded, scaled to a quarter size and box-blurred once on a
//! background thread. Painting only samples the cached bitmap, so moving or
//! repainting a group costs no more than drawing one image.
use std::sync::{Arc, Mutex};
use windows::core::{Interface, PCWSTR};
use windows::Win32::Foundation::{GENERIC_READ, HWND, LPARAM, RECT, WPARAM};
use windows::Win32::Graphics::Imaging::*;
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CoTaskMemFree, CoUninitialize, CLSCTX_ALL,
    COINIT_APARTMENTTHREADED,
};
use windows::Win32::UI::Shell::{
    DesktopWallpaper, IDesktopWallpaper, DESKTOP_WALLPAPER_POSITION, DWPOS_CENTER, DWPOS_FIT,
    DWPOS_STRETCH,
};
use windows::Win32::UI::WindowsAndMessaging::PostMessageW;

/// Wallpaper pixels are kept at 1/SCALE resolution; the blur hides it.
pub const SCALE: i32 = 4;
const RADIUS: usize = 6;

/// A blurred, quarter-size BGRA copy of one monitor's wallpaper.
pub struct Blurred {
    /// The monitor in screen coordinates.
    pub monitor: RECT,
    pub width: u32,
    pub height: u32,
    pub pixels: Vec<u8>,
}

struct Cache {
    generation: u64,
    monitors: Vec<Arc<Blurred>>,
}

static CACHE: Mutex<Cache> = Mutex::new(Cache {
    generation: 0,
    monitors: Vec::new(),
});

/// The current blurred wallpapers and a generation that changes on refresh.
pub fn current() -> (u64, Vec<Arc<Blurred>>) {
    let cache = CACHE.lock().unwrap();
    (cache.generation, cache.monitors.clone())
}

/// Rebuilds the cache in the background, then posts `message` to `notify`.
pub fn refresh(notify: HWND, message: u32) {
    let target = notify.0 as usize;
    std::thread::spawn(move || unsafe {
        let initialized = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
        let monitors = load().unwrap_or_default();
        if initialized.is_ok() {
            CoUninitialize();
        }
        {
            let mut cache = CACHE.lock().unwrap();
            cache.generation += 1;
            cache.monitors = monitors.into_iter().map(Arc::new).collect();
        }
        let _ = PostMessageW(Some(HWND(target as *mut _)), message, WPARAM(0), LPARAM(0));
    });
}

unsafe fn load() -> windows::core::Result<Vec<Blurred>> {
    let wallpaper: IDesktopWallpaper = CoCreateInstance(&DesktopWallpaper, None, CLSCTX_ALL)?;
    let position = wallpaper.GetPosition().unwrap_or(DWPOS_CENTER);
    let color = wallpaper.GetBackgroundColor().map(|c| c.0).unwrap_or(0);
    let background = [
        ((color >> 16) & 0xff) as u8,
        ((color >> 8) & 0xff) as u8,
        (color & 0xff) as u8,
        255,
    ];
    let wic: IWICImagingFactory = CoCreateInstance(&CLSID_WICImagingFactory, None, CLSCTX_ALL)?;
    let mut result = Vec::new();
    for index in 0..wallpaper.GetMonitorDevicePathCount()? {
        let Ok(id) = wallpaper.GetMonitorDevicePathAt(index) else {
            continue;
        };
        let monitor = wallpaper.GetMonitorRECT(PCWSTR(id.0));
        let file = wallpaper.GetWallpaper(PCWSTR(id.0)).ok();
        let path = file.map(|p| {
            let text = p.to_string().unwrap_or_default();
            CoTaskMemFree(Some(p.0 as _));
            text
        });
        CoTaskMemFree(Some(id.0 as _));
        let Ok(monitor) = monitor else {
            continue;
        };
        let width = ((monitor.right - monitor.left) / SCALE).max(1) as u32;
        let height = ((monitor.bottom - monitor.top) / SCALE).max(1) as u32;
        let mut pixels = path
            .filter(|p| !p.is_empty() && std::path::Path::new(p).exists())
            // Wallpaper apps often delete the original after setting it;
            // Windows keeps showing its own transcoded copy.
            .or_else(|| transcoded(index))
            .and_then(|p| decode(&wic, &p, width, height, position, background).ok())
            .unwrap_or_else(|| solid(width, height, background));
        blur(&mut pixels, width as usize, height as usize, RADIUS);
        result.push(Blurred {
            monitor,
            width,
            height,
            pixels,
        });
    }
    Ok(result)
}

/// Windows' cached copy of the wallpaper (per monitor when they differ).
fn transcoded(index: u32) -> Option<String> {
    use windows::Win32::UI::Shell::{
        FOLDERID_RoamingAppData, SHGetKnownFolderPath, KF_FLAG_DEFAULT,
    };
    let roaming = unsafe {
        let value = SHGetKnownFolderPath(&FOLDERID_RoamingAppData, KF_FLAG_DEFAULT, None).ok()?;
        let text = value.to_string().ok();
        CoTaskMemFree(Some(value.0 as _));
        text?
    };
    let themes = std::path::PathBuf::from(roaming)
        .join("Microsoft")
        .join("Windows")
        .join("Themes");
    [
        themes.join(format!("Transcoded_{index:03}")),
        themes.join("TranscodedWallpaper"),
    ]
    .into_iter()
    .find(|p| p.is_file())
    .map(|p| p.to_string_lossy().into_owned())
}

fn solid(width: u32, height: u32, bgra: [u8; 4]) -> Vec<u8> {
    bgra.iter()
        .copied()
        .cycle()
        .take(width as usize * height as usize * 4)
        .collect()
}

/// Decodes and places the image like Windows does for the common modes
/// (fill/span/tile approximate as fill; fit letterboxes; stretch distorts).
unsafe fn decode(
    wic: &IWICImagingFactory,
    path: &str,
    width: u32,
    height: u32,
    position: DESKTOP_WALLPAPER_POSITION,
    background: [u8; 4],
) -> windows::core::Result<Vec<u8>> {
    let wide: Vec<u16> = path.encode_utf16().chain(Some(0)).collect();
    let decoder = wic.CreateDecoderFromFilename(
        PCWSTR(wide.as_ptr()),
        None,
        GENERIC_READ,
        WICDecodeMetadataCacheOnDemand,
    )?;
    let frame = decoder.GetFrame(0)?;
    let (mut iw, mut ih) = (0u32, 0u32);
    frame.GetSize(&mut iw, &mut ih)?;
    if iw == 0 || ih == 0 {
        return Ok(solid(width, height, background));
    }
    let (fw, fh) = (width as f64, height as f64);
    let (sw, sh) = (iw as f64, ih as f64);
    // Scaled image size and its offset inside the target.
    let (scaled_w, scaled_h) = if position == DWPOS_STRETCH {
        (fw, fh)
    } else if position == DWPOS_FIT {
        let k = (fw / sw).min(fh / sh);
        (sw * k, sh * k)
    } else if position == DWPOS_CENTER {
        (sw / SCALE as f64, sh / SCALE as f64)
    } else {
        let k = (fw / sw).max(fh / sh);
        (sw * k, sh * k)
    };
    let scaled_w = (scaled_w.round() as u32).max(1);
    let scaled_h = (scaled_h.round() as u32).max(1);
    let scaler = wic.CreateBitmapScaler()?;
    scaler.Initialize(&frame, scaled_w, scaled_h, WICBitmapInterpolationModeFant)?;
    let converter = wic.CreateFormatConverter()?;
    converter.Initialize(
        &scaler.cast::<IWICBitmapSource>()?,
        &GUID_WICPixelFormat32bppBGRA,
        WICBitmapDitherTypeNone,
        None,
        0.0,
        WICBitmapPaletteTypeCustom,
    )?;
    let mut image = vec![0u8; scaled_w as usize * scaled_h as usize * 4];
    converter.CopyPixels(std::ptr::null(), scaled_w * 4, &mut image)?;
    let mut out = solid(width, height, background);
    let off_x = (width as i64 - scaled_w as i64) / 2;
    let off_y = (height as i64 - scaled_h as i64) / 2;
    for y in 0..height as i64 {
        let sy = y - off_y;
        if sy < 0 || sy >= scaled_h as i64 {
            continue;
        }
        for x in 0..width as i64 {
            let sx = x - off_x;
            if sx < 0 || sx >= scaled_w as i64 {
                continue;
            }
            let s = (sy as usize * scaled_w as usize + sx as usize) * 4;
            let d = (y as usize * width as usize + x as usize) * 4;
            out[d..d + 3].copy_from_slice(&image[s..s + 3]);
            out[d + 3] = 255;
        }
    }
    Ok(out)
}

/// Three box-blur passes per axis approximate a Gaussian (radius ~ 3r).
pub fn blur(pixels: &mut [u8], width: usize, height: usize, radius: usize) {
    if width == 0 || height == 0 || radius == 0 {
        return;
    }
    let mut scratch = vec![0u8; pixels.len()];
    for _ in 0..3 {
        box_pass(pixels, &mut scratch, width, height, radius, true);
        box_pass(&scratch, pixels, width, height, radius, false);
    }
}

fn box_pass(src: &[u8], dst: &mut [u8], width: usize, height: usize, r: usize, horizontal: bool) {
    let (lines, length) = if horizontal {
        (height, width)
    } else {
        (width, height)
    };
    let index = |line: usize, i: usize| {
        if horizontal {
            (line * width + i) * 4
        } else {
            (i * width + line) * 4
        }
    };
    let window = (2 * r + 1) as u32;
    for line in 0..lines {
        for channel in 0..3 {
            let at = |i: isize| {
                src[index(line, i.clamp(0, length as isize - 1) as usize) + channel] as u32
            };
            let mut sum: u32 = (-(r as isize)..=r as isize).map(at).sum();
            for i in 0..length {
                dst[index(line, i) + channel] = (sum / window) as u8;
                sum += at(i as isize + r as isize + 1);
                sum -= at(i as isize - r as isize);
            }
        }
        for i in 0..length {
            dst[index(line, i) + 3] = 255;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blur_smooths_an_edge_and_keeps_flat_areas() {
        let (w, h) = (40, 4);
        let mut pixels = Vec::new();
        for _ in 0..h {
            for x in 0..w {
                let v = if x < w / 2 { 0 } else { 200 };
                pixels.extend([v, v, v, 255]);
            }
        }
        blur(&mut pixels, w, h, 3);
        let at = |x: usize| pixels[(w + x) * 4];
        assert_eq!(at(0), 0);
        assert_eq!(at(w - 1), 200);
        // The hard edge becomes a gradient.
        assert!(at(w / 2 - 1) > 20 && at(w / 2) < 180);
        assert!(at(w / 2 - 3) < at(w / 2 + 2));
        assert!(pixels.chunks(4).all(|p| p[3] == 255));
    }
}
