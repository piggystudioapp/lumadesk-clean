//! The LumaDesk window opened from the tray: drawn with Direct2D in the
//! same style as the groups (no Win32 push buttons), Mica backdrop, and
//! Windows' light/dark setting. Actions are dispatched as WM_COMMAND to the
//! launcher window so the existing command handling is reused.
use crate::i18n::{self, K};
use std::cell::RefCell;
use windows::core::{w, Result, PCWSTR};
use windows::Win32::Foundation::{HWND, RECT};
use windows::Win32::Graphics::Direct2D::Common::*;
use windows::Win32::Graphics::Direct2D::*;
use windows::Win32::Graphics::DirectWrite::*;
use windows::Win32::Graphics::Dwm::{
    DwmExtendFrameIntoClientArea, DwmSetWindowAttribute, DWMWA_SYSTEMBACKDROP_TYPE,
    DWMWA_USE_IMMERSIVE_DARK_MODE, DWMWA_WINDOW_CORNER_PREFERENCE,
};
use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_B8G8R8A8_UNORM;
use windows::Win32::System::Registry::{RegGetValueW, HKEY_CURRENT_USER, RRF_RT_REG_DWORD};
use windows::Win32::UI::Controls::MARGINS;
use windows::Win32::UI::HiDpi::GetDpiForWindow;
use windows_numerics::Vector2;

/// Logical (96 DPI) client size; the render target scales it per monitor.
pub const WIDTH: f32 = 440.0;
pub const HEIGHT: f32 = 430.0;
const PAD: f32 = 20.0;
const GAP: f32 = 10.0;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Style {
    Primary,
    Tile,
    Danger,
}

#[derive(Clone, Copy, Debug)]
pub struct Button {
    pub command: usize,
    pub glyph: &'static str,
    pub label: &'static str,
    pub detail: &'static str,
    pub style: Style,
    /// Disabled while background work is queued.
    pub needs_idle: bool,
    pub rect: D2D_RECT_F,
}

/// What the window shows besides its buttons.
pub struct Status {
    pub groups: usize,
    pub text: String,
    pub busy: bool,
}

/// Button layout in logical pixels. Commands are LumaDesk's WM_COMMAND ids.
pub fn layout(commands: &[(usize, &'static str, &'static str, &'static str, bool)]) -> Vec<Button> {
    let rect = |left: f32, top: f32, width: f32, height: f32| D2D_RECT_F {
        left,
        top,
        right: left + width,
        bottom: top + height,
    };
    let mut buttons = Vec::new();
    let inner = WIDTH - 2.0 * PAD;
    let mut commands = commands.iter();
    if let Some(&(command, glyph, label, detail, needs_idle)) = commands.next() {
        buttons.push(Button {
            command,
            glyph,
            label,
            detail,
            style: Style::Primary,
            needs_idle,
            rect: rect(PAD, 92.0, inner, 56.0),
        });
    }
    let tile_w = (inner - GAP) / 2.0;
    let tiles: Vec<_> = commands.by_ref().take(6).collect();
    for (i, &&(command, glyph, label, detail, needs_idle)) in tiles.iter().enumerate() {
        let (col, row) = ((i % 2) as f32, (i / 2) as f32);
        buttons.push(Button {
            command,
            glyph,
            label,
            detail,
            style: Style::Tile,
            needs_idle,
            rect: rect(
                PAD + col * (tile_w + GAP),
                160.0 + row * (60.0 + GAP),
                tile_w,
                60.0,
            ),
        });
    }
    if let Some(&(command, glyph, label, detail, needs_idle)) = commands.next() {
        buttons.push(Button {
            command,
            glyph,
            label,
            detail,
            style: Style::Danger,
            needs_idle,
            rect: rect(WIDTH - PAD - 176.0, HEIGHT - PAD - 34.0, 176.0, 34.0),
        });
    }
    buttons
}

/// The button under a logical point.
pub fn hit(buttons: &[Button], x: f32, y: f32) -> Option<usize> {
    buttons
        .iter()
        .position(|b| x >= b.rect.left && x < b.rect.right && y >= b.rect.top && y < b.rect.bottom)
}

/// Windows' app theme (Settings → Personalization → Colors).
pub fn system_dark() -> bool {
    let mut value: u32 = 1;
    let mut size = std::mem::size_of::<u32>() as u32;
    let result = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            w!("Software\\Microsoft\\Windows\\CurrentVersion\\Themes\\Personalize"),
            w!("AppsUseLightTheme"),
            RRF_RT_REG_DWORD,
            None,
            Some((&mut value as *mut u32).cast()),
            Some(&mut size),
        )
    };
    result.is_ok() && value == 0
}

/// Dark title bar, rounded corners and a Mica backdrop (Windows 11; ignored
/// elsewhere). The client is drawn with alpha so Mica shows through.
pub unsafe fn apply_window_style(hwnd: HWND, dark: bool) {
    let dark_flag: i32 = dark.into();
    let _ = DwmSetWindowAttribute(
        hwnd,
        DWMWA_USE_IMMERSIVE_DARK_MODE,
        (&dark_flag as *const i32).cast(),
        4,
    );
    let round: i32 = 2; // DWMWCP_ROUND
    let _ = DwmSetWindowAttribute(
        hwnd,
        DWMWA_WINDOW_CORNER_PREFERENCE,
        (&round as *const i32).cast(),
        4,
    );
    let mica: i32 = 2; // DWMSBT_MAINWINDOW
    let _ = DwmSetWindowAttribute(
        hwnd,
        DWMWA_SYSTEMBACKDROP_TYPE,
        (&mica as *const i32).cast(),
        4,
    );
    let margins = MARGINS {
        cxLeftWidth: -1,
        cxRightWidth: -1,
        cyTopHeight: -1,
        cyBottomHeight: -1,
    };
    let _ = DwmExtendFrameIntoClientArea(hwnd, &margins);
}

fn color(r: u8, g: u8, b: u8, a: f32) -> D2D1_COLOR_F {
    D2D1_COLOR_F {
        r: r as f32 / 255.0,
        g: g as f32 / 255.0,
        b: b as f32 / 255.0,
        a,
    }
}

struct Palette {
    wash: D2D1_COLOR_F,
    text: D2D1_COLOR_F,
    muted: D2D1_COLOR_F,
    tile: D2D1_COLOR_F,
    tile_hover: D2D1_COLOR_F,
    tile_pressed: D2D1_COLOR_F,
    stroke: D2D1_COLOR_F,
    accent: D2D1_COLOR_F,
    accent_hover: D2D1_COLOR_F,
    on_accent: D2D1_COLOR_F,
    danger: D2D1_COLOR_F,
    focus: D2D1_COLOR_F,
    pill: D2D1_COLOR_F,
    good: D2D1_COLOR_F,
}

fn palette(dark: bool) -> Palette {
    if dark {
        Palette {
            wash: color(32, 32, 32, 0.55),
            text: color(255, 255, 255, 1.0),
            muted: color(255, 255, 255, 0.62),
            tile: color(255, 255, 255, 0.06),
            tile_hover: color(255, 255, 255, 0.09),
            tile_pressed: color(255, 255, 255, 0.04),
            stroke: color(255, 255, 255, 0.08),
            accent: color(166, 142, 255, 1.0),
            accent_hover: color(180, 160, 255, 1.0),
            on_accent: color(20, 16, 36, 1.0),
            danger: color(255, 153, 164, 1.0),
            focus: color(255, 255, 255, 0.9),
            pill: color(255, 255, 255, 0.07),
            good: color(108, 203, 95, 1.0),
        }
    } else {
        Palette {
            wash: color(243, 243, 243, 0.55),
            text: color(26, 26, 26, 1.0),
            muted: color(0, 0, 0, 0.6),
            tile: color(255, 255, 255, 0.7),
            tile_hover: color(249, 249, 249, 0.85),
            tile_pressed: color(249, 249, 249, 0.5),
            stroke: color(0, 0, 0, 0.07),
            accent: color(88, 85, 216, 1.0),
            accent_hover: color(104, 101, 228, 1.0),
            on_accent: color(255, 255, 255, 1.0),
            danger: color(196, 43, 28, 1.0),
            focus: color(0, 0, 0, 0.9),
            pill: color(0, 0, 0, 0.05),
            good: color(15, 123, 15, 1.0),
        }
    }
}

pub struct View {
    target: ID2D1HwndRenderTarget,
    write: IDWriteFactory,
    title: IDWriteTextFormat,
    small: IDWriteTextFormat,
    label: IDWriteTextFormat,
    detail: IDWriteTextFormat,
    glyph: IDWriteTextFormat,
    glyph_big: IDWriteTextFormat,
    icon: Option<ID2D1Bitmap>,
}

impl View {
    pub unsafe fn new(
        hwnd: HWND,
        icon: Option<windows::Win32::UI::WindowsAndMessaging::HICON>,
    ) -> Result<Self> {
        let factory: ID2D1Factory = D2D1CreateFactory(D2D1_FACTORY_TYPE_SINGLE_THREADED, None)?;
        let dpi = GetDpiForWindow(hwnd).max(96) as f32;
        let mut client = RECT::default();
        let _ = windows::Win32::UI::WindowsAndMessaging::GetClientRect(hwnd, &mut client);
        let target = factory.CreateHwndRenderTarget(
            &D2D1_RENDER_TARGET_PROPERTIES {
                r#type: D2D1_RENDER_TARGET_TYPE_DEFAULT,
                pixelFormat: D2D1_PIXEL_FORMAT {
                    format: DXGI_FORMAT_B8G8R8A8_UNORM,
                    alphaMode: D2D1_ALPHA_MODE_PREMULTIPLIED,
                },
                dpiX: dpi,
                dpiY: dpi,
                usage: D2D1_RENDER_TARGET_USAGE_NONE,
                minLevel: D2D1_FEATURE_LEVEL_DEFAULT,
            },
            &D2D1_HWND_RENDER_TARGET_PROPERTIES {
                hwnd,
                pixelSize: D2D_SIZE_U {
                    width: (client.right - client.left).max(1) as u32,
                    height: (client.bottom - client.top).max(1) as u32,
                },
                presentOptions: D2D1_PRESENT_OPTIONS_NONE,
            },
        )?;
        let write: IDWriteFactory = DWriteCreateFactory(DWRITE_FACTORY_TYPE_SHARED)?;
        let text = |family: PCWSTR, weight, size: f32| -> Result<IDWriteTextFormat> {
            let format = write.CreateTextFormat(
                family,
                None,
                weight,
                DWRITE_FONT_STYLE_NORMAL,
                DWRITE_FONT_STRETCH_NORMAL,
                size,
                crate::i18n::locale(),
            )?;
            format.SetWordWrapping(DWRITE_WORD_WRAPPING_NO_WRAP)?;
            format.SetParagraphAlignment(DWRITE_PARAGRAPH_ALIGNMENT_CENTER)?;
            let sign = write.CreateEllipsisTrimmingSign(&format)?;
            format.SetTrimming(
                &DWRITE_TRIMMING {
                    granularity: DWRITE_TRIMMING_GRANULARITY_CHARACTER,
                    delimiter: 0,
                    delimiterCount: 0,
                },
                &sign,
            )?;
            Ok(format)
        };
        let ui = w!("Segoe UI Variable Text");
        let icons = w!("Segoe Fluent Icons");
        let glyph = text(icons, DWRITE_FONT_WEIGHT_NORMAL, 18.0)?;
        glyph.SetTextAlignment(DWRITE_TEXT_ALIGNMENT_CENTER)?;
        let glyph_big = text(icons, DWRITE_FONT_WEIGHT_NORMAL, 22.0)?;
        glyph_big.SetTextAlignment(DWRITE_TEXT_ALIGNMENT_CENTER)?;
        let small = text(ui, DWRITE_FONT_WEIGHT_NORMAL, 12.0)?;
        small.SetWordWrapping(DWRITE_WORD_WRAPPING_WRAP)?;
        small.SetParagraphAlignment(DWRITE_PARAGRAPH_ALIGNMENT_NEAR)?;
        let icon = icon.and_then(|icon| {
            let wic: windows::Win32::Graphics::Imaging::IWICImagingFactory =
                windows::Win32::System::Com::CoCreateInstance(
                    &windows::Win32::Graphics::Imaging::CLSID_WICImagingFactory,
                    None,
                    windows::Win32::System::Com::CLSCTX_INPROC_SERVER,
                )
                .ok()?;
            let source = wic.CreateBitmapFromHICON(icon).ok()?;
            let converter = wic.CreateFormatConverter().ok()?;
            converter
                .Initialize(
                    &source,
                    &windows::Win32::Graphics::Imaging::GUID_WICPixelFormat32bppPBGRA,
                    windows::Win32::Graphics::Imaging::WICBitmapDitherTypeNone,
                    None,
                    0.0,
                    windows::Win32::Graphics::Imaging::WICBitmapPaletteTypeCustom,
                )
                .ok()?;
            target.CreateBitmapFromWicBitmap(&converter, None).ok()
        });
        Ok(Self {
            title: text(ui, DWRITE_FONT_WEIGHT_SEMI_BOLD, 20.0)?,
            small,
            label: text(ui, DWRITE_FONT_WEIGHT_SEMI_BOLD, 14.0)?,
            detail: text(ui, DWRITE_FONT_WEIGHT_NORMAL, 12.0)?,
            glyph,
            glyph_big,
            icon,
            target,
            write,
        })
    }

    pub unsafe fn resize(&self, width: u32, height: u32, dpi: u32) -> Result<()> {
        let dpi = dpi.max(96) as f32;
        self.target.SetDpi(dpi, dpi);
        self.target.Resize(&D2D_SIZE_U { width, height })
    }

    /// Draws text inside `rect`, shrinking it (to 72 % at most) when a
    /// translation is longer than the space; anything left is trimmed.
    unsafe fn text(
        &self,
        content: &str,
        format: &IDWriteTextFormat,
        rect: D2D_RECT_F,
        brush: &ID2D1SolidColorBrush,
    ) {
        let wide: Vec<u16> = content.encode_utf16().collect();
        let (width, height) = (
            (rect.right - rect.left).max(1.0),
            (rect.bottom - rect.top).max(1.0),
        );
        let size = format.GetFontSize();
        let wraps = format.GetWordWrapping() != DWRITE_WORD_WRAPPING_NO_WRAP;
        let all = DWRITE_TEXT_RANGE {
            startPosition: 0,
            length: wide.len() as u32,
        };
        // Measure without the box limit: trimming would report a cut width.
        let mut fitted = size;
        if let Ok(probe) = self.write.CreateTextLayout(
            &wide,
            format,
            if wraps { width } else { 100_000.0 },
            if wraps { 100_000.0 } else { height },
        ) {
            for _ in 0..8 {
                let mut metrics = DWRITE_TEXT_METRICS::default();
                if probe.GetMetrics(&mut metrics).is_err() {
                    break;
                }
                let over = if wraps {
                    metrics.height / height
                } else {
                    metrics.widthIncludingTrailingWhitespace / width
                };
                if over <= 1.0 || fitted <= size * 0.72 {
                    break;
                }
                fitted = (fitted / over.clamp(1.02, 1.15)).max(size * 0.72);
                let _ = probe.SetFontSize(fitted, all);
            }
        }
        if let Ok(layout) = self.write.CreateTextLayout(&wide, format, width, height) {
            if fitted < size {
                let _ = layout.SetFontSize(fitted, all);
            }
            self.target.DrawTextLayout(
                Vector2 {
                    X: rect.left,
                    Y: rect.top,
                },
                &layout,
                brush,
                D2D1_DRAW_TEXT_OPTIONS_CLIP,
            );
            return;
        }
        self.target.DrawText(
            &wide,
            format,
            &rect,
            brush,
            D2D1_DRAW_TEXT_OPTIONS_CLIP,
            DWRITE_MEASURING_MODE_NATURAL,
        );
    }

    #[allow(clippy::too_many_arguments)]
    pub unsafe fn paint(
        &self,
        buttons: &[Button],
        status: &Status,
        dark: bool,
        hover: Option<usize>,
        pressed: Option<usize>,
        focus: Option<usize>,
        help: &str,
    ) -> Result<()> {
        let p = palette(dark);
        let brush = |c: D2D1_COLOR_F| self.target.CreateSolidColorBrush(&c, None);
        let (text, muted, tile, tile_hover, tile_pressed, stroke) = (
            brush(p.text)?,
            brush(p.muted)?,
            brush(p.tile)?,
            brush(p.tile_hover)?,
            brush(p.tile_pressed)?,
            brush(p.stroke)?,
        );
        let (accent, accent_hover, on_accent, danger, focus_brush, pill, good) = (
            brush(p.accent)?,
            brush(p.accent_hover)?,
            brush(p.on_accent)?,
            brush(p.danger)?,
            brush(p.focus)?,
            brush(p.pill)?,
            brush(p.good)?,
        );
        let rounded = |r: D2D_RECT_F, radius: f32| D2D1_ROUNDED_RECT {
            rect: r,
            radiusX: radius,
            radiusY: radius,
        };
        let rect = |l: f32, t: f32, r: f32, b: f32| D2D_RECT_F {
            left: l,
            top: t,
            right: r,
            bottom: b,
        };
        self.target.BeginDraw();
        // Transparent client over Mica, with a light wash for contrast.
        self.target.Clear(Some(&p.wash));

        // Header: icon, name, version and a live status pill.
        if let Some(icon) = &self.icon {
            self.target.DrawBitmap(
                icon,
                Some(&rect(PAD, 22.0, PAD + 36.0, 58.0)),
                1.0,
                D2D1_BITMAP_INTERPOLATION_MODE_LINEAR,
                None,
            );
        }
        self.text(
            "LumaDesk",
            &self.title,
            rect(68.0, 18.0, 220.0, 44.0),
            &text,
        );
        self.text(
            concat!("v", env!("CARGO_PKG_VERSION")),
            &self.detail,
            rect(166.0, 22.0, 260.0, 42.0),
            &muted,
        );
        let state = if status.busy {
            i18n::t(K::PillBusy)
        } else {
            i18n::t(K::PillRunning)
        };
        let pill_text = i18n::tf(K::Pill, &[&state, &status.groups, &status.text]);
        let pill_rect = rect(68.0, 48.0, WIDTH - PAD, 72.0);
        self.target
            .FillRoundedRectangle(&rounded(pill_rect, 12.0), &pill);
        // Status dot.
        self.target.FillRoundedRectangle(
            &rounded(rect(77.0, 56.0, 85.0, 64.0), 4.0),
            if status.busy { &accent } else { &good },
        );
        self.text(
            &pill_text,
            &self.detail,
            rect(92.0, 48.0, WIDTH - PAD - 8.0, 72.0),
            &muted,
        );

        for (index, button) in buttons.iter().enumerate() {
            let disabled = button.needs_idle && status.busy;
            let is_hover = hover == Some(index) && !disabled;
            let is_pressed = pressed == Some(index) && !disabled;
            let r = button.rect;
            match button.style {
                Style::Primary => {
                    let fill = if is_hover { &accent_hover } else { &accent };
                    self.target.FillRoundedRectangle(&rounded(r, 8.0), fill);
                    let ink = if disabled { &muted } else { &on_accent };
                    self.text(
                        button.glyph,
                        &self.glyph_big,
                        rect(r.left + 12.0, r.top, r.left + 52.0, r.bottom),
                        ink,
                    );
                    self.text(
                        button.label,
                        &self.label,
                        rect(r.left + 56.0, r.top + 8.0, r.right - 12.0, r.top + 30.0),
                        ink,
                    );
                    self.text(
                        button.detail,
                        &self.detail,
                        rect(r.left + 56.0, r.top + 28.0, r.right - 12.0, r.bottom - 6.0),
                        ink,
                    );
                }
                Style::Tile => {
                    let fill = if is_pressed {
                        &tile_pressed
                    } else if is_hover {
                        &tile_hover
                    } else {
                        &tile
                    };
                    self.target.FillRoundedRectangle(&rounded(r, 8.0), fill);
                    self.target
                        .DrawRoundedRectangle(&rounded(r, 8.0), &stroke, 1.0, None);
                    let ink = if disabled { &muted } else { &text };
                    self.text(
                        button.glyph,
                        &self.glyph,
                        rect(r.left + 8.0, r.top, r.left + 44.0, r.bottom),
                        if disabled { &muted } else { &accent },
                    );
                    self.text(
                        button.label,
                        &self.label,
                        rect(r.left + 46.0, r.top + 9.0, r.right - 8.0, r.top + 31.0),
                        ink,
                    );
                    self.text(
                        button.detail,
                        &self.detail,
                        rect(r.left + 46.0, r.top + 30.0, r.right - 8.0, r.bottom - 8.0),
                        &muted,
                    );
                }
                Style::Danger => {
                    if is_hover || is_pressed {
                        self.target
                            .FillRoundedRectangle(&rounded(r, 6.0), &tile_hover);
                    }
                    self.text(
                        button.glyph,
                        &self.glyph,
                        rect(r.left + 6.0, r.top, r.left + 30.0, r.bottom),
                        &danger,
                    );
                    self.text(
                        button.label,
                        &self.label,
                        rect(r.left + 32.0, r.top, r.right - 6.0, r.bottom),
                        &danger,
                    );
                }
            }
            if focus == Some(index) {
                // Keyboard focus ring, as in Windows 11 controls.
                let ring = rect(r.left - 3.0, r.top - 3.0, r.right + 3.0, r.bottom + 3.0);
                self.target
                    .DrawRoundedRectangle(&rounded(ring, 10.0), &focus_brush, 2.0, None);
            }
        }
        // Shortcut help, muted, left of the exit button.
        self.text(
            help,
            &self.small,
            rect(
                PAD,
                HEIGHT - PAD - 40.0,
                WIDTH - PAD - 184.0,
                HEIGHT - PAD + 4.0,
            ),
            &muted,
        );
        self.target.EndDraw(None, None)
    }
}

thread_local! {
    pub static STATE: RefCell<Option<LauncherState>> = const { RefCell::new(None) };
}

/// Interaction state of the (single) launcher window.
pub struct LauncherState {
    pub view: Option<View>,
    pub buttons: Vec<Button>,
    pub hover: Option<usize>,
    pub pressed: Option<usize>,
    pub focus: Option<usize>,
    pub dark: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layout_places_primary_tiles_and_exit_without_overlap() {
        let commands = [
            (1, "a", "主要", "", true),
            (2, "b", "一", "", false),
            (3, "c", "二", "", false),
            (4, "d", "三", "", true),
            (5, "e", "四", "", false),
            (6, "f", "五", "", true),
            (7, "g", "六", "", false),
            (8, "h", "結束", "", false),
        ];
        let buttons = layout(&commands);
        assert_eq!(buttons.len(), 8);
        assert_eq!(buttons[0].style, Style::Primary);
        assert_eq!(buttons[7].style, Style::Danger);
        for (i, a) in buttons.iter().enumerate() {
            assert!(a.rect.left >= 0.0 && a.rect.right <= WIDTH && a.rect.bottom <= HEIGHT);
            for b in &buttons[i + 1..] {
                let apart = a.rect.right <= b.rect.left
                    || b.rect.right <= a.rect.left
                    || a.rect.bottom <= b.rect.top
                    || b.rect.bottom <= a.rect.top;
                assert!(apart, "{:?} overlaps {:?}", a.label, b.label);
            }
        }
        let center = |b: &Button| {
            (
                (b.rect.left + b.rect.right) / 2.0,
                (b.rect.top + b.rect.bottom) / 2.0,
            )
        };
        let (x, y) = center(&buttons[3]);
        assert_eq!(hit(&buttons, x, y), Some(3));
        assert_eq!(hit(&buttons, 2.0, 2.0), None);
    }
}
