//! Hardware accelerated Direct2D / DirectWrite drawing.
//! A fence only repaints on an input or model change: no idle animation timer.
use crate::i18n::{self, K};

use crate::model::{Fence, Snapped, SortKey, Theme, ViewMode, HEADER_HEIGHT, SIZES};

/// Corner radius matching the window region (CreateRoundRectRgn uses 2x).
const CORNER: f32 = 8.0;
use std::cell::RefCell;
use std::collections::HashMap;
use std::path::Path;
use windows::core::{w, Result};
use windows::Win32::Foundation::{HWND, RECT};
use windows::Win32::Graphics::Direct2D::Common::*;
use windows::Win32::Graphics::Direct2D::*;
use windows::Win32::Graphics::DirectWrite::*;
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_FORMAT_UNKNOWN};
use windows::Win32::Graphics::Imaging::*;
use windows::Win32::System::Com::{CoCreateInstance, CLSCTX_INPROC_SERVER};

/// Descriptions are created when the model changes, never in the paint loop.
pub struct ItemDisplay {
    name: String,
    kind: String,
    icon_key: String,
}

impl ItemDisplay {
    pub fn from_path(path: &Path) -> Self {
        let folder = path.is_dir();
        let extension = path
            .extension()
            .and_then(|ext| ext.to_str())
            .unwrap_or(i18n::t(K::File));
        let extension_key = extension.to_lowercase();
        // Like Explorer, shortcut extensions are never shown.
        let hide_extension = !folder && matches!(extension_key.as_str(), "lnk" | "url");
        let name = if hide_extension {
            path.file_stem()
        } else {
            path.file_name()
        }
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.to_string_lossy().into_owned());
        Self {
            name,
            kind: if folder {
                i18n::t(K::Folder).to_owned()
            } else {
                extension.to_uppercase()
            },
            icon_key: if folder {
                ":folder".to_owned()
            } else if matches!(
                extension_key.as_str(),
                "exe" | "lnk" | "ico" | "appref-ms" | "url"
            ) {
                // Executables and shortcuts often contain custom icons that
                // cannot be inferred from their shared file extension.
                format!(":path:{}", path.to_string_lossy())
            } else {
                extension_key
            },
        }
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn is_folder(&self) -> bool {
        self.icon_key == ":folder"
    }

    pub fn matches_query(&self, lower_query: &str) -> bool {
        self.name.to_lowercase().contains(lower_query)
            || self.kind.to_lowercase().contains(lower_query)
    }
}

#[derive(Clone, Copy)]
struct Palette {
    bg: D2D1_COLOR_F,
    tile: D2D1_COLOR_F,
    tile_hover: D2D1_COLOR_F,
    border: D2D1_COLOR_F,
    text: D2D1_COLOR_F,
    muted: D2D1_COLOR_F,
    accent: D2D1_COLOR_F,
    accent_soft: D2D1_COLOR_F,
    /// Laid over the blurred wallpaper.
    tint: D2D1_COLOR_F,
}

fn c(r: u8, g: u8, b: u8, a: f32) -> D2D1_COLOR_F {
    D2D1_COLOR_F {
        r: r as f32 / 255.0,
        g: g as f32 / 255.0,
        b: b as f32 / 255.0,
        a,
    }
}

fn palette(theme: Theme) -> Palette {
    match theme {
        // Neutral Windows 11 style surfaces; translucent fills read well on
        // both the solid background and the frosted wallpaper.
        Theme::Midnight => Palette {
            bg: c(28, 28, 30, 1.0),
            tile: c(255, 255, 255, 0.07),
            tile_hover: c(255, 255, 255, 0.10),
            border: c(255, 255, 255, 0.12),
            text: c(245, 245, 247, 1.0),
            muted: c(168, 170, 178, 1.0),
            accent: c(166, 142, 255, 1.0),
            accent_soft: c(255, 255, 255, 0.16),
            tint: c(24, 24, 26, 0.62),
        },
        Theme::Cloud => Palette {
            bg: c(243, 243, 245, 1.0),
            tile: c(0, 0, 0, 0.05),
            tile_hover: c(0, 0, 0, 0.07),
            border: c(0, 0, 0, 0.12),
            text: c(26, 26, 30, 1.0),
            muted: c(96, 98, 108, 1.0),
            accent: c(88, 85, 216, 1.0),
            accent_soft: c(0, 0, 0, 0.10),
            tint: c(246, 246, 248, 0.68),
        },
    }
}

// Brushes are GPU resources. A hover repaint reuses them; only a theme change
// allocates new ones.
struct Brushes {
    theme: Theme,
    tile: ID2D1SolidColorBrush,
    hover_tile: ID2D1SolidColorBrush,
    border: ID2D1SolidColorBrush,
    text: ID2D1SolidColorBrush,
    muted: ID2D1SolidColorBrush,
    accent: ID2D1SolidColorBrush,
    accent_soft: ID2D1SolidColorBrush,
    tint: ID2D1SolidColorBrush,
}

pub struct Renderer {
    target: ID2D1HwndRenderTarget,
    wic: Option<IWICImagingFactory>,
    icons: RefCell<HashMap<String, Option<ID2D1Bitmap>>>,
    title: IDWriteTextFormat,
    body: IDWriteTextFormat,
    caption: IDWriteTextFormat,
    tiny_right: IDWriteTextFormat,
    /// Single-line row text (list/details), trimmed with an ellipsis.
    row: IDWriteTextFormat,
    row_right: IDWriteTextFormat,
    /// Per item-size level (see model::SIZES): rows, right-aligned rows, labels.
    sized_rows: Vec<IDWriteTextFormat>,
    sized_rows_right: Vec<IDWriteTextFormat>,
    sized_labels: Vec<IDWriteTextFormat>,
    brushes: RefCell<Option<Brushes>>,
    write: IDWriteFactory,
    /// Device bitmaps of the blurred wallpapers, per backdrop generation.
    backdrops: RefCell<(u64, Vec<(RECT, ID2D1Bitmap)>)>,
}

impl Renderer {
    pub unsafe fn new(hwnd: HWND, width: u32, height: u32) -> Result<Self> {
        let factory: ID2D1Factory = D2D1CreateFactory(D2D1_FACTORY_TYPE_SINGLE_THREADED, None)?;
        let properties = D2D1_RENDER_TARGET_PROPERTIES {
            r#type: D2D1_RENDER_TARGET_TYPE_DEFAULT,
            pixelFormat: D2D1_PIXEL_FORMAT {
                format: DXGI_FORMAT_UNKNOWN,
                alphaMode: D2D1_ALPHA_MODE_IGNORE,
            },
            dpiX: 96.0,
            dpiY: 96.0,
            usage: D2D1_RENDER_TARGET_USAGE_NONE,
            minLevel: D2D1_FEATURE_LEVEL_DEFAULT,
        };
        let hwnd_properties = D2D1_HWND_RENDER_TARGET_PROPERTIES {
            hwnd,
            pixelSize: D2D_SIZE_U { width, height },
            presentOptions: D2D1_PRESENT_OPTIONS_NONE,
        };
        let target = factory.CreateHwndRenderTarget(&properties, &hwnd_properties)?;
        // Native Shell icons become cached device bitmaps. Executables and
        // shortcuts keep their per-file artwork, while regular types share
        // one bitmap per extension. Missing WIC falls back to text glyphs.
        let wic = CoCreateInstance(&CLSID_WICImagingFactory, None, CLSCTX_INPROC_SERVER).ok();
        let write: IDWriteFactory = DWriteCreateFactory(DWRITE_FACTORY_TYPE_SHARED)?;
        let title = write.CreateTextFormat(
            w!("Segoe UI"),
            None,
            DWRITE_FONT_WEIGHT_SEMI_BOLD,
            DWRITE_FONT_STYLE_NORMAL,
            DWRITE_FONT_STRETCH_NORMAL,
            14.0,
            crate::i18n::locale(),
        )?;
        let body = write.CreateTextFormat(
            w!("Segoe UI"),
            None,
            DWRITE_FONT_WEIGHT_MEDIUM,
            DWRITE_FONT_STYLE_NORMAL,
            DWRITE_FONT_STRETCH_NORMAL,
            13.0,
            crate::i18n::locale(),
        )?;
        let caption = write.CreateTextFormat(
            w!("Segoe UI"),
            None,
            DWRITE_FONT_WEIGHT_NORMAL,
            DWRITE_FONT_STYLE_NORMAL,
            DWRITE_FONT_STRETCH_NORMAL,
            11.5,
            crate::i18n::locale(),
        )?;
        let tiny_right = write.CreateTextFormat(
            w!("Segoe UI"),
            None,
            DWRITE_FONT_WEIGHT_NORMAL,
            DWRITE_FONT_STYLE_NORMAL,
            DWRITE_FONT_STRETCH_NORMAL,
            11.0,
            crate::i18n::locale(),
        )?;
        tiny_right.SetTextAlignment(DWRITE_TEXT_ALIGNMENT_TRAILING)?;
        for fmt in [&title, &body, &caption, &tiny_right] {
            fmt.SetWordWrapping(DWRITE_WORD_WRAPPING_NO_WRAP)?;
            // Longer translations end in an ellipsis instead of being cut.
            let sign = write.CreateEllipsisTrimmingSign(fmt)?;
            fmt.SetTrimming(
                &DWRITE_TRIMMING {
                    granularity: DWRITE_TRIMMING_GRANULARITY_CHARACTER,
                    delimiter: 0,
                    delimiterCount: 0,
                },
                &sign,
            )?;
        }
        let row = write.CreateTextFormat(
            w!("Segoe UI"),
            None,
            DWRITE_FONT_WEIGHT_NORMAL,
            DWRITE_FONT_STYLE_NORMAL,
            DWRITE_FONT_STRETCH_NORMAL,
            12.5,
            crate::i18n::locale(),
        )?;
        let row_right = write.CreateTextFormat(
            w!("Segoe UI"),
            None,
            DWRITE_FONT_WEIGHT_NORMAL,
            DWRITE_FONT_STYLE_NORMAL,
            DWRITE_FONT_STRETCH_NORMAL,
            12.0,
            crate::i18n::locale(),
        )?;
        row_right.SetTextAlignment(DWRITE_TEXT_ALIGNMENT_TRAILING)?;
        for fmt in [&row, &row_right] {
            fmt.SetWordWrapping(DWRITE_WORD_WRAPPING_NO_WRAP)?;
            let sign = write.CreateEllipsisTrimmingSign(fmt)?;
            fmt.SetTrimming(
                &DWRITE_TRIMMING {
                    granularity: DWRITE_TRIMMING_GRANULARITY_CHARACTER,
                    delimiter: 0,
                    delimiterCount: 0,
                },
                &sign,
            )?;
        }
        // Item text at every size level, created once.
        let make =
            |size: f32, align: DWRITE_TEXT_ALIGNMENT, wrap: bool| -> Result<IDWriteTextFormat> {
                let format = write.CreateTextFormat(
                    w!("Segoe UI"),
                    None,
                    DWRITE_FONT_WEIGHT_NORMAL,
                    DWRITE_FONT_STYLE_NORMAL,
                    DWRITE_FONT_STRETCH_NORMAL,
                    size,
                    crate::i18n::locale(),
                )?;
                format.SetTextAlignment(align)?;
                format.SetWordWrapping(if wrap {
                    DWRITE_WORD_WRAPPING_WRAP
                } else {
                    DWRITE_WORD_WRAPPING_NO_WRAP
                })?;
                if !wrap {
                    format.SetParagraphAlignment(DWRITE_PARAGRAPH_ALIGNMENT_CENTER)?;
                }
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
        let mut sized_rows = Vec::new();
        let mut sized_rows_right = Vec::new();
        let mut sized_labels = Vec::new();
        for m in SIZES {
            sized_rows.push(make(
                m.row_font as f32 / 10.0,
                DWRITE_TEXT_ALIGNMENT_LEADING,
                false,
            )?);
            sized_rows_right.push(make(
                m.row_font as f32 / 10.0 - 0.5,
                DWRITE_TEXT_ALIGNMENT_TRAILING,
                false,
            )?);
            sized_labels.push(make(
                m.label_font as f32 / 10.0,
                DWRITE_TEXT_ALIGNMENT_CENTER,
                true,
            )?);
        }
        Ok(Self {
            target,
            wic,
            icons: RefCell::new(HashMap::new()),
            title,
            body,
            caption,
            tiny_right,
            row,
            row_right,
            sized_rows,
            sized_rows_right,
            sized_labels,
            brushes: RefCell::new(None),
            write,
            backdrops: RefCell::new((0, Vec::new())),
        })
    }

    /// Width in pixels of single-line row text (for column auto-fit).
    pub fn text_width(&self, text: &str) -> f32 {
        let wide: Vec<u16> = text.encode_utf16().collect();
        unsafe {
            let mut metrics = DWRITE_TEXT_METRICS::default();
            self.write
                .CreateTextLayout(&wide, &self.row, 10_000.0, 40.0)
                .and_then(|layout| layout.GetMetrics(&mut metrics))
                .map_or(0.0, |_| metrics.widthIncludingTrailingWhitespace)
        }
    }

    /// The blurred wallpaper under `screen` (a window rectangle in screen
    /// coordinates) and the matching source rectangle, once available.
    pub fn backdrop_for(&self, screen: RECT) -> Option<(ID2D1Bitmap, D2D_RECT_F)> {
        let (generation, monitors) = crate::backdrop::current();
        let mut cache = self.backdrops.borrow_mut();
        if cache.0 != generation {
            cache.1 = monitors
                .iter()
                .filter_map(|m| unsafe {
                    self.target
                        .CreateBitmap(
                            D2D_SIZE_U {
                                width: m.width,
                                height: m.height,
                            },
                            Some(m.pixels.as_ptr().cast()),
                            m.width * 4,
                            &D2D1_BITMAP_PROPERTIES {
                                pixelFormat: D2D1_PIXEL_FORMAT {
                                    format: DXGI_FORMAT_B8G8R8A8_UNORM,
                                    alphaMode: D2D1_ALPHA_MODE_IGNORE,
                                },
                                dpiX: 96.0,
                                dpiY: 96.0,
                            },
                        )
                        .ok()
                        .map(|bitmap| (m.monitor, bitmap))
                })
                .collect();
            cache.0 = generation;
        }
        let (cx, cy) = (
            (screen.left + screen.right) / 2,
            (screen.top + screen.bottom) / 2,
        );
        let (monitor, bitmap) = cache
            .1
            .iter()
            .find(|(m, _)| cx >= m.left && cx < m.right && cy >= m.top && cy < m.bottom)
            .or_else(|| cache.1.first())?;
        let scale = crate::backdrop::SCALE as f32;
        Some((
            bitmap.clone(),
            D2D_RECT_F {
                left: (screen.left - monitor.left) as f32 / scale,
                top: (screen.top - monitor.top) as f32 / scale,
                right: (screen.right - monitor.left) as f32 / scale,
                bottom: (screen.bottom - monitor.top) as f32 / scale,
            },
        ))
    }

    pub unsafe fn resize(&self, width: u32, height: u32) -> Result<()> {
        self.target.Resize(&D2D_SIZE_U { width, height })
    }

    fn brush(&self, color: D2D1_COLOR_F) -> Result<ID2D1SolidColorBrush> {
        unsafe { self.target.CreateSolidColorBrush(&color, None) }
    }

    fn rr(left: f32, top: f32, right: f32, bottom: f32, radius: f32) -> D2D1_ROUNDED_RECT {
        D2D1_ROUNDED_RECT {
            rect: D2D_RECT_F {
                left,
                top,
                right,
                bottom,
            },
            radiusX: radius,
            radiusY: radius,
        }
    }

    unsafe fn fill(&self, rect: D2D1_ROUNDED_RECT, brush: &ID2D1SolidColorBrush) {
        self.target.FillRoundedRectangle(&rect, brush);
    }

    // Drawing each label needs the caller's exact bounding box and brush.
    #[allow(clippy::too_many_arguments)]
    unsafe fn text(
        &self,
        content: &str,
        format: &IDWriteTextFormat,
        x: f32,
        y: f32,
        width: f32,
        height: f32,
        brush: &ID2D1SolidColorBrush,
    ) {
        let content: Vec<u16> = content.encode_utf16().collect();
        self.target.DrawText(
            &content,
            format,
            &D2D_RECT_F {
                left: x,
                top: y,
                right: x + width,
                bottom: y + height,
            },
            brush,
            D2D1_DRAW_TEXT_OPTIONS_CLIP,
            DWRITE_MEASURING_MODE_NATURAL,
        );
    }

    unsafe fn bitmap_from_icon(
        &self,
        icon: &crate::shell_icons::NativeIcon,
    ) -> Option<ID2D1Bitmap> {
        let wic = self.wic.as_ref()?;
        let source = wic.CreateBitmapFromHICON(icon.handle());
        let source = source.ok()?;
        let converter = wic.CreateFormatConverter().ok()?;
        converter
            .Initialize(
                &source,
                &GUID_WICPixelFormat32bppPBGRA,
                WICBitmapDitherTypeNone,
                None,
                0.0,
                WICBitmapPaletteTypeCustom,
            )
            .ok()?;
        self.target.CreateBitmapFromWicBitmap(&converter, None).ok()
    }

    fn icon(&self, key: &str) -> Option<ID2D1Bitmap> {
        self.icons.borrow().get(key).cloned().flatten()
    }

    pub fn has_missing_visible_icons(
        &self,
        items: &[ItemDisplay],
        visible_indices: &[usize],
        scroll: usize,
        visible: usize,
        class: u32,
    ) -> bool {
        let icons = self.icons.borrow();
        visible_indices
            .iter()
            .skip(scroll)
            .take(visible)
            .filter_map(|&index| items.get(index))
            .any(|item| !icons.contains_key(&crate::shell_icons::sized_key(&item.icon_key, class)))
    }

    /// Polls the background cache; Shell extensions never run on the GUI thread.
    pub fn load_one_visible_icon(
        &self,
        items: &[ItemDisplay],
        visible_indices: &[usize],
        scroll: usize,
        visible: usize,
        class: u32,
    ) -> bool {
        let keys: Vec<_> = {
            let icons = self.icons.borrow();
            visible_indices
                .iter()
                .skip(scroll)
                .take(visible)
                .filter_map(|&index| items.get(index))
                .map(|item| crate::shell_icons::sized_key(&item.icon_key, class))
                .filter(|key| !icons.contains_key(key))
                .collect()
        };
        let mut changed = false;
        for key in keys {
            if let Some(icon) = crate::shell_icons::request(&key) {
                let bitmap = icon.and_then(|icon| unsafe { self.bitmap_from_icon(&icon) });
                self.icons.borrow_mut().insert(key, bitmap);
                changed = true;
            }
        }
        changed
    }

    // Keep explicit render state parameters to avoid storing stale UI state.
    #[allow(clippy::too_many_arguments)]
    pub unsafe fn draw(
        &self,
        fence: &Fence,
        items: &[ItemDisplay],
        visible_indices: &[usize],
        search: Option<&str>,
        theme: Theme,
        hover: Option<usize>,
        selected: &std::collections::HashSet<String>,
        status: &str,
        scroll: usize,
        view_height: i32,
        show_body: bool,
        drop_hover: bool,
        dragging: bool,
        snap: Snapped,
        hovered: bool,
        backdrop: Option<(&ID2D1Bitmap, D2D_RECT_F)>,
    ) -> Result<()> {
        let p = palette(theme);
        let w = fence.width as f32;
        let h = view_height as f32;
        let mut cache = self.brushes.borrow_mut();
        if cache.as_ref().map(|b| b.theme) != Some(theme) {
            *cache = Some(Brushes {
                theme,
                tile: self.brush(p.tile)?,
                hover_tile: self.brush(p.tile_hover)?,
                border: self.brush(p.border)?,
                text: self.brush(p.text)?,
                muted: self.brush(p.muted)?,
                accent: self.brush(p.accent)?,
                accent_soft: self.brush(p.accent_soft)?,
                tint: self.brush(p.tint)?,
            });
        }
        let b = cache.as_ref().expect("theme brushes initialized");
        let (tile, hover_tile, border, text, muted, accent, accent_soft) = (
            &b.tile,
            &b.hover_tile,
            &b.border,
            &b.text,
            &b.muted,
            &b.accent,
            &b.accent_soft,
        );

        self.target.BeginDraw();
        self.target.Clear(Some(&p.bg));
        if let Some((wallpaper, source)) = backdrop {
            // Frosted look: the blurred wallpaper under this group, then a
            // translucent tint so text stays legible on any wallpaper.
            self.target.DrawBitmap(
                wallpaper,
                Some(&D2D_RECT_F {
                    left: 0.0,
                    top: 0.0,
                    right: w,
                    bottom: h,
                }),
                1.0,
                D2D1_BITMAP_INTERPOLATION_MODE_LINEAR,
                Some(&source),
            );
            self.target.FillRectangle(
                &D2D_RECT_F {
                    left: 0.0,
                    top: 0.0,
                    right: w,
                    bottom: h,
                },
                &b.tint,
            );
        }
        // Thin edge like a Windows 11 surface (the window region rounds it).
        self.target.DrawRoundedRectangle(
            &Self::rr(0.5, 0.5, w - 0.5, h - 0.5, CORNER),
            border,
            1.0,
            None,
        );
        if drop_hover {
            self.target.DrawRoundedRectangle(
                &Self::rr(1.5, 1.5, w - 1.5, h - 1.5, CORNER),
                accent,
                2.0,
                None,
            );
        }

        // Snap guides: a bright line along each edge that is aligned to a
        // neighbour or the work area, so alignment is visible while dragging.
        let guides = [
            (snap.left, (0.0, 10.0, 2.0, h - 10.0)),
            (snap.right, (w - 2.0, 10.0, w, h - 10.0)),
            (snap.top, (10.0, 0.0, w - 10.0, 2.0)),
            (snap.bottom, (10.0, h - 2.0, w - 10.0, h)),
        ];
        for (on, (left, top, right, bottom)) in guides {
            if on {
                self.target.FillRectangle(
                    &D2D_RECT_F {
                        left,
                        top,
                        right,
                        bottom,
                    },
                    accent,
                );
            }
        }

        // Slim header: title only. Count, status and buttons appear while
        // the pointer is over the group, or while searching/selecting.
        let buttons = hovered || drop_hover;
        let caption_text = match (drop_hover, search) {
            (true, _) => Some(i18n::t(K::DropToAdd).to_owned()),
            (false, Some("")) => Some(i18n::t(K::SearchHint).to_owned()),
            (false, Some(query)) => {
                Some(i18n::tf(K::SearchCount, &[&query, &visible_indices.len()]))
            }
            (false, None) if !status.is_empty() => Some(status.to_owned()),
            (false, None) if !selected.is_empty() => {
                Some(i18n::tf(K::Selected, &[&selected.len()]))
            }
            (false, None) if hovered => Some(i18n::tf(K::Count, &[&fence.paths.len()])),
            _ => None,
        };
        let right_edge = if buttons { w - 72.0 } else { w - 12.0 };
        let caption_width = if caption_text.is_some() { 120.0 } else { 0.0 };
        self.text(
            &fence.title,
            &self.title,
            14.0,
            8.0,
            (right_edge - caption_width - 18.0).max(40.0),
            22.0,
            text,
        );
        if let Some(caption) = caption_text {
            self.text(
                &caption,
                &self.tiny_right,
                right_edge - caption_width,
                12.0,
                caption_width,
                16.0,
                muted,
            );
        }
        if buttons {
            self.fill(Self::rr(w - 66.0, 6.0, w - 40.0, 30.0, 6.0), tile);
            self.fill(Self::rr(w - 34.0, 6.0, w - 8.0, 30.0, 6.0), tile);
            self.text(
                if fence.collapsed { "+" } else { "−" },
                &self.title,
                w - 58.0,
                6.0,
                14.0,
                22.0,
                text,
            );
            self.text("···", &self.title, w - 29.0, 5.0, 18.0, 22.0, text);
        }

        if !show_body {
            return self.target.EndDraw(None, None);
        }

        // Empty state uses deliberately few shapes and no bitmap allocations.
        if fence.paths.is_empty() {
            if h < 280.0 {
                self.text(
                    i18n::t(K::EmptyDrag),
                    &self.body,
                    23.0,
                    99.0,
                    w - 46.0,
                    27.0,
                    text,
                );
                self.text(
                    i18n::t(K::EmptyHint),
                    &self.caption,
                    23.0,
                    127.0,
                    w - 46.0,
                    24.0,
                    muted,
                );
            } else {
                let center = (h - HEADER_HEIGHT as f32) / 2.0 + HEADER_HEIGHT as f32;
                self.fill(
                    Self::rr(
                        w * 0.5 - 39.0,
                        center - 66.0,
                        w * 0.5 + 39.0,
                        center + 12.0,
                        23.0,
                    ),
                    tile,
                );
                self.fill(
                    Self::rr(
                        w * 0.5 - 22.0,
                        center - 37.0,
                        w * 0.5 + 22.0,
                        center - 10.0,
                        8.0,
                    ),
                    accent_soft,
                );
                self.fill(
                    Self::rr(
                        w * 0.5 - 13.0,
                        center - 29.0,
                        w * 0.5 + 13.0,
                        center - 25.0,
                        2.0,
                    ),
                    accent,
                );
                self.fill(
                    Self::rr(
                        w * 0.5 - 13.0,
                        center - 19.0,
                        w * 0.5 + 6.0,
                        center - 15.0,
                        2.0,
                    ),
                    accent,
                );
                // Centred under the illustration, whatever the text length.
                let _ = self.body.SetTextAlignment(DWRITE_TEXT_ALIGNMENT_CENTER);
                let _ = self.caption.SetTextAlignment(DWRITE_TEXT_ALIGNMENT_CENTER);
                self.text(
                    i18n::t(K::EmptyBig),
                    &self.body,
                    16.0,
                    center + 30.0,
                    w - 32.0,
                    28.0,
                    text,
                );
                self.text(
                    i18n::t(K::EmptyBigHint),
                    &self.caption,
                    16.0,
                    center + 56.0,
                    w - 32.0,
                    22.0,
                    muted,
                );
                let _ = self.body.SetTextAlignment(DWRITE_TEXT_ALIGNMENT_LEADING);
                let _ = self.caption.SetTextAlignment(DWRITE_TEXT_ALIGNMENT_LEADING);
            }
        } else if visible_indices.is_empty() {
            let center = (h - HEADER_HEIGHT as f32) / 2.0 + HEADER_HEIGHT as f32;
            self.text(
                i18n::t(K::NoMatches),
                &self.body,
                24.0,
                center - 10.0,
                w - 48.0,
                27.0,
                text,
            );
            self.text(
                i18n::t(K::NoMatchesHint),
                &self.caption,
                24.0,
                center + 21.0,
                w - 48.0,
                24.0,
                muted,
            );
        } else {
            let mode = fence.mode();
            let columns = fence.detail_columns();
            let metrics = fence.metrics();
            let level = fence.size_level();
            let class = crate::shell_icons::icon_class(fence.icon_pixels());
            let (row_format, row_right_format, label_format) = (
                &self.sized_rows[level],
                &self.sized_rows_right[level],
                &self.sized_labels[level],
            );
            if mode == ViewMode::Details {
                // Column titles; the sorted column shows its direction.
                let top = crate::model::COLUMNS_TOP as f32;
                for (key, left, width) in columns {
                    if width <= 0 {
                        continue;
                    }
                    let title = match key {
                        SortKey::Name => i18n::t(K::ColName),
                        SortKey::Modified => i18n::t(K::ColModified),
                        SortKey::Type => i18n::t(K::ColType),
                        SortKey::Size => i18n::t(K::ColSize),
                        SortKey::Manual => "",
                    };
                    let arrow = match (fence.sort == key, fence.sort_desc) {
                        (true, false) => " ▲",
                        (true, true) => " ▼",
                        _ => "",
                    };
                    let (left, width) = (left as f32, width as f32);
                    let format = if key == SortKey::Size {
                        &self.row_right
                    } else {
                        &self.row
                    };
                    self.text(
                        &format!("{title}{arrow}"),
                        format,
                        left,
                        top + 3.0,
                        width - 6.0,
                        21.0,
                        muted,
                    );
                    if key != SortKey::Name {
                        self.target.FillRectangle(
                            &D2D_RECT_F {
                                left: left - 6.0,
                                top: top + 4.0,
                                right: left - 5.0,
                                bottom: top + 20.0,
                            },
                            border,
                        );
                    }
                }
                self.target.FillRectangle(
                    &D2D_RECT_F {
                        left: 8.0,
                        top: top + 25.0,
                        right: w - 8.0,
                        bottom: top + 26.0,
                    },
                    border,
                );
            }
            let visible = fence.visible_slots();
            for (slot, &i) in visible_indices
                .iter()
                .enumerate()
                .skip(scroll)
                .take(visible)
            {
                let Some(item) = items.get(i) else {
                    continue;
                };
                let cell = fence.slot_rect(slot - scroll);
                let is_selected = fence
                    .entries
                    .get(i)
                    .is_some_and(|e| selected.contains(&e.id));
                let (cx, cy) = (cell.x as f32, cell.y as f32);
                let (cw, ch) = (cell.width as f32, cell.height as f32);
                if mode == ViewMode::Icons {
                    // Desktop-style tile: no resting background, a soft plate
                    // on hover/selection, icon centred above a 2-line label.
                    let plate = Self::rr(cx + 2.0, cy, cx + cw - 2.0, cy + ch, 6.0);
                    if is_selected {
                        self.fill(plate, accent_soft);
                    } else if hover == Some(i) {
                        self.fill(plate, hover_tile);
                    }
                    if dragging && hover == Some(i) && !is_selected {
                        // Insertion point: the dragged items land before this tile.
                        self.fill(Self::rr(cx, cy + 6.0, cx + 3.0, cy + ch - 6.0, 1.5), accent);
                    }
                    let size = metrics.icon as f32;
                    let left = cx + (cw - size) / 2.0;
                    let top = cy + 6.0;
                    if let Some(icon) =
                        self.icon(&crate::shell_icons::sized_key(&item.icon_key, class))
                    {
                        let bounds = D2D_RECT_F {
                            left,
                            top,
                            right: left + size,
                            bottom: top + size,
                        };
                        self.target.DrawBitmap(
                            &icon,
                            Some(&bounds),
                            1.0,
                            D2D1_BITMAP_INTERPOLATION_MODE_LINEAR,
                            None,
                        );
                    } else {
                        self.fill(
                            Self::rr(
                                left + 4.0,
                                top + 4.0,
                                left + size - 4.0,
                                top + size - 4.0,
                                9.0,
                            ),
                            accent_soft,
                        );
                        let initial: String = item
                            .name
                            .chars()
                            .next()
                            .unwrap_or('?')
                            .to_uppercase()
                            .collect();
                        self.text(
                            &initial,
                            &self.title,
                            left + 17.0,
                            top + 11.0,
                            24.0,
                            26.0,
                            accent,
                        );
                    }
                    self.text(
                        &item.name,
                        label_format,
                        cx + 4.0,
                        cy + 6.0 + size + 3.0,
                        cw - 8.0,
                        metrics.label_font as f32 * 0.27,
                        text,
                    );
                    continue;
                }
                // Compact rows: small icons, list and details.
                let plate = Self::rr(cx, cy, cx + cw, cy + ch, 4.0);
                if is_selected {
                    self.fill(plate, accent_soft);
                } else if hover == Some(i) {
                    self.fill(plate, hover_tile);
                }
                if dragging && hover == Some(i) && !is_selected {
                    self.fill(Self::rr(cx, cy + 3.0, cx + 2.0, cy + ch - 3.0, 1.0), accent);
                }
                let icon_size = metrics.row_icon as f32;
                let icon_top = cy + (ch - icon_size) / 2.0;
                if let Some(icon) = self.icon(&crate::shell_icons::sized_key(&item.icon_key, class))
                {
                    self.target.DrawBitmap(
                        &icon,
                        Some(&D2D_RECT_F {
                            left: cx + 6.0,
                            top: icon_top,
                            right: cx + 6.0 + icon_size,
                            bottom: icon_top + icon_size,
                        }),
                        1.0,
                        D2D1_BITMAP_INTERPOLATION_MODE_LINEAR,
                        None,
                    );
                }
                let name_left = cx + 12.0 + icon_size;
                let text_top = cy;
                let text_height = ch;
                if mode != ViewMode::Details {
                    self.text(
                        &item.name,
                        row_format,
                        name_left,
                        text_top,
                        cw - 36.0,
                        text_height,
                        text,
                    );
                    continue;
                }
                let meta = fence.paths.get(i).and_then(|p| crate::meta::get(p));
                for (key, left, width) in columns {
                    if width <= 0 {
                        continue;
                    }
                    let (left, width) = (left as f32, width as f32);
                    match key {
                        SortKey::Name => self.text(
                            &item.name,
                            row_format,
                            name_left,
                            text_top,
                            left + width - name_left - 8.0,
                            text_height,
                            text,
                        ),
                        SortKey::Modified => self.text(
                            &crate::meta::format_time(meta.as_ref().and_then(|m| m.modified)),
                            row_format,
                            left,
                            text_top,
                            width - 8.0,
                            text_height,
                            muted,
                        ),
                        SortKey::Type => self.text(
                            meta.as_ref().map_or("", |m| m.type_name.as_str()),
                            row_format,
                            left,
                            text_top,
                            width - 8.0,
                            text_height,
                            muted,
                        ),
                        SortKey::Size => self.text(
                            &crate::meta::format_size(meta.as_ref().and_then(|m| m.size)),
                            row_right_format,
                            left,
                            text_top,
                            width - 4.0,
                            text_height,
                            muted,
                        ),
                        SortKey::Manual => {}
                    }
                }
            }
            // Thin scrollbar instead of a "scroll for more" label.
            let columns = fence.columns();
            let total_rows = visible_indices.len().div_ceil(columns);
            let rows = fence.visible_rows();
            if total_rows > rows {
                let top = fence.slot_rect(0).y as f32;
                let track = (h - 8.0 - top).max(24.0);
                let thumb = (track * rows as f32 / total_rows as f32).max(24.0);
                let first = (scroll / columns) as f32;
                let offset = (track - thumb) * first / (total_rows - rows) as f32;
                self.fill(
                    Self::rr(w - 6.0, top + offset, w - 3.0, top + offset + thumb, 1.5),
                    if hovered { muted } else { border },
                );
            }
        }
        self.target.EndDraw(None, None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cached_display_metadata_normalizes_file_extensions() {
        let jpg = ItemDisplay::from_path(Path::new(r"C:\Pictures\Photo.JpG"));
        assert_eq!(jpg.name, "Photo.JpG");
        assert_eq!(jpg.kind, "JPG");
        assert_eq!(jpg.icon_key, "jpg");

        let folder = ItemDisplay::from_path(&std::env::temp_dir());
        assert_eq!(folder.kind, "資料夾");
        assert_eq!(folder.icon_key, ":folder");

        let shortcut = ItemDisplay::from_path(Path::new(r"C:\Links\LumaDesk.lnk"));
        assert_eq!(shortcut.icon_key, r":path:C:\Links\LumaDesk.lnk");
        let executable = ItemDisplay::from_path(Path::new(r"C:\Apps\alpha.EXE"));
        assert_eq!(executable.icon_key, r":path:C:\Apps\alpha.EXE");
    }

    #[test]
    fn search_matches_name_or_kind_without_case_sensitivity() {
        let docs = ItemDisplay::from_path(Path::new(r"C:\Work\Design Notes.PDF"));
        assert!(docs.matches_query("design"));
        assert!(docs.matches_query("pdf"));
        assert!(!docs.matches_query("music"));

        let unicode = ItemDisplay::from_path(Path::new(r"C:\Work\會議紀錄.txt"));
        assert!(unicode.matches_query("會議"));
    }
}
