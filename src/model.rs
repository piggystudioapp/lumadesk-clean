//! Persistent state. No Windows API calls are needed here.

use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

pub const MIN_WIDTH: i32 = 290;
pub const MIN_HEIGHT: i32 = 180;
pub const HEADER_HEIGHT: i32 = 36;
pub const COLLAPSED_HEIGHT: i32 = 36;
pub const ROW_BOTTOM_PADDING: i32 = 10;
// Icon grid: 48 px Shell icon above a two-line centered label, as on the desktop.
pub const GRID_TOP: i32 = 44;
pub const GRID_PAD: i32 = 4;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Theme {
    #[default]
    Midnight,
    Cloud,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Fence {
    pub id: u64,
    pub title: String,
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
    #[serde(default)]
    pub collapsed: bool,
    #[serde(default)]
    pub paths: Vec<PathBuf>,
    /// Stable item records; paths is the compatibility/rendering projection.
    #[serde(default)]
    pub entries: Vec<Entry>,
    /// Legacy (0.6.4-0.7.1) flag for the old row view; read as List.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub list_view: bool,
    #[serde(default, skip_serializing_if = "is_default")]
    pub view: ViewMode,
    /// Display order; Manual is the stored (drag) order.
    #[serde(default, skip_serializing_if = "is_default")]
    pub sort: SortKey,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub sort_desc: bool,
    /// User-set widths/visibility of 修改日期, 類型, 大小 (None: automatic).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub details: Option<[DetailColumn; 3]>,
    /// Item size level (index into SIZES); None is the default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub size: Option<u8>,
}

/// Pixel metrics of one item size level.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SizeMetrics {
    /// Large-icon grid.
    pub icon: i32,
    pub tile_width: i32,
    pub tile_height: i32,
    /// Grid label font size in tenths of a pixel.
    pub label_font: i32,
    /// Small icons, list and details rows.
    pub row_icon: i32,
    pub line_height: i32,
    pub row_font: i32,
    pub small_tile_width: i32,
}

/// 小, 中 (default), 大, 特大, 超大 - like Explorer's icon sizes.
pub const SIZES: [SizeMetrics; 5] = [
    SizeMetrics {
        icon: 32,
        tile_width: 72,
        tile_height: 78,
        label_font: 110,
        row_icon: 16,
        line_height: 22,
        row_font: 115,
        small_tile_width: 150,
    },
    SizeMetrics {
        icon: 48,
        tile_width: 86,
        tile_height: 96,
        label_font: 120,
        row_icon: 18,
        line_height: 26,
        row_font: 125,
        small_tile_width: 170,
    },
    SizeMetrics {
        icon: 64,
        tile_width: 104,
        tile_height: 116,
        label_font: 130,
        row_icon: 24,
        line_height: 32,
        row_font: 140,
        small_tile_width: 200,
    },
    SizeMetrics {
        icon: 96,
        tile_width: 136,
        tile_height: 150,
        label_font: 140,
        row_icon: 32,
        line_height: 40,
        row_font: 150,
        small_tile_width: 240,
    },
    SizeMetrics {
        icon: 128,
        tile_width: 170,
        tile_height: 184,
        label_font: 150,
        row_icon: 40,
        line_height: 48,
        row_font: 160,
        small_tile_width: 280,
    },
];
pub const DEFAULT_SIZE: usize = 1;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct DetailColumn {
    pub width: i32,
    pub visible: bool,
}

pub const MIN_COLUMN: i32 = 44;
pub const MAX_COLUMN: i32 = 480;
pub const MIN_NAME_COLUMN: i32 = 80;

fn is_default<T: Default + PartialEq>(value: &T) -> bool {
    *value == T::default()
}

/// Explorer-style views of a collection.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ViewMode {
    #[default]
    Icons,
    SmallIcons,
    List,
    Details,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SortKey {
    #[default]
    Manual,
    Name,
    Modified,
    Type,
    Size,
}

/// Compact rows (small icons, list, details) start below the header.
pub const LIST_TOP: i32 = 42;
/// Details view: column titles between the header and the rows.
pub const COLUMNS_TOP: i32 = 38;
pub const DETAILS_TOP: i32 = 66;

/// One visible item cell in client coordinates.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Slot {
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ItemKind {
    /// Items collected by releases before 0.6 load as references at their
    /// stored path; LumaDesk no longer moves files.
    #[serde(alias = "managed")]
    Reference,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct Entry {
    pub id: String,
    pub path: PathBuf,
    pub kind: ItemKind,
}

pub fn unique_id() -> String {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    format!(
        "{:x}-{:x}-{:x}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos(),
        std::process::id(),
        NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    )
}

impl Fence {
    pub fn set_entries(&mut self, entries: Vec<Entry>) {
        self.paths = entries.iter().map(|e| e.path.clone()).collect();
        self.entries = entries;
    }

    pub fn reconcile_entries(&mut self) {
        let mut old = std::mem::take(&mut self.entries);
        self.entries = self
            .paths
            .iter()
            .map(|path| {
                if let Some(index) = old.iter().position(|e| e.path == *path) {
                    old.remove(index)
                } else {
                    Entry {
                        id: unique_id(),
                        path: path.clone(),
                        kind: ItemKind::Reference,
                    }
                }
            })
            .collect();
    }
    pub fn window_height(&self) -> i32 {
        if self.collapsed {
            COLLAPSED_HEIGHT
        } else {
            self.height
        }
    }

    /// The effective view (the legacy row flag reads as List).
    pub fn mode(&self) -> ViewMode {
        if self.view == ViewMode::Icons && self.list_view {
            ViewMode::List
        } else {
            self.view
        }
    }

    pub fn detail_columns(&self) -> [(SortKey, i32, i32); 4] {
        if self.details.is_some() || self.size_level() == DEFAULT_SIZE {
            return detail_columns(self.width, self.details);
        }
        // Automatic widths grow and shrink with the item text size.
        let scale = |w: i32| w * self.metrics().row_font / SIZES[DEFAULT_SIZE].row_font;
        let auto = detail_columns(self.width, None);
        let column = |i: usize, base: i32| DetailColumn {
            width: scale(base),
            visible: auto[i].2 > 0,
        };
        detail_columns(
            self.width,
            Some([column(1, 150), column(2, 120), column(3, 72)]),
        )
    }

    /// The current widths/visibility, materialised for editing.
    pub fn detail_settings(&self) -> [DetailColumn; 3] {
        self.details.unwrap_or_else(|| {
            let auto = detail_columns(self.width, None);
            [1, 2, 3].map(|i| DetailColumn {
                width: if auto[i].2 > 0 {
                    auto[i].2
                } else {
                    [150, 120, 72][i - 1]
                },
                visible: auto[i].2 > 0,
            })
        })
    }

    /// Item size (Ctrl+wheel / 「大小」), clamped to the defined levels.
    pub fn size_level(&self) -> usize {
        self.size
            .map_or(DEFAULT_SIZE, |s| (s as usize).min(SIZES.len() - 1))
    }

    pub fn metrics(&self) -> SizeMetrics {
        SIZES[self.size_level()]
    }

    /// The icon edge in pixels this group draws at, for its current view.
    pub fn icon_pixels(&self) -> i32 {
        let m = self.metrics();
        match self.mode() {
            ViewMode::Icons => m.icon,
            _ => m.row_icon,
        }
    }

    pub fn set_mode(&mut self, mode: ViewMode) {
        self.view = mode;
        self.list_view = false;
    }

    /// Items per visual row: tiles that fit in the icon views, otherwise 1.
    pub fn columns(&self) -> usize {
        let inner = self.width - 2 * GRID_PAD;
        match self.mode() {
            ViewMode::Icons => (inner / self.metrics().tile_width).max(1) as usize,
            ViewMode::SmallIcons => (inner / self.metrics().small_tile_width).max(1) as usize,
            ViewMode::List | ViewMode::Details => 1,
        }
    }

    /// Top of the first item row and the row pitch.
    fn rows_origin(&self) -> (i32, i32) {
        match self.mode() {
            ViewMode::Icons => (GRID_TOP, self.metrics().tile_height),
            ViewMode::SmallIcons | ViewMode::List => (LIST_TOP, self.metrics().line_height),
            ViewMode::Details => (DETAILS_TOP, self.metrics().line_height),
        }
    }

    /// Visual rows that fit; scrolling moves by whole rows.
    pub fn visible_rows(&self) -> usize {
        let (top, step) = self.rows_origin();
        ((self.height - top - ROW_BOTTOM_PADDING) / step).max(1) as usize
    }

    /// Items visible at once.
    pub fn visible_slots(&self) -> usize {
        self.visible_rows() * self.columns()
    }

    /// Largest scroll offset (in items, always a whole row) for `count` items.
    pub fn max_scroll_for(&self, count: usize) -> usize {
        let columns = self.columns();
        count.div_ceil(columns).saturating_sub(self.visible_rows()) * columns
    }

    pub fn max_scroll(&self) -> usize {
        self.max_scroll_for(self.paths.len())
    }

    /// Scroll offset that keeps visible position `index` on screen.
    pub fn scroll_to_show(&self, scroll: usize, index: usize) -> usize {
        let columns = self.columns();
        let row = index / columns;
        let first = scroll / columns;
        let rows = self.visible_rows();
        if row < first {
            row * columns
        } else if row >= first + rows {
            (row + 1 - rows) * columns
        } else {
            scroll
        }
    }

    /// Cell of the n-th visible slot (0-based from the scroll position).
    pub fn slot_rect(&self, n: usize) -> Slot {
        let (top, step) = self.rows_origin();
        let columns = self.columns();
        let width = (self.width - 2 * GRID_PAD - 6) / columns as i32;
        let (width, gap) = match self.mode() {
            ViewMode::Icons => ((self.width - 2 * GRID_PAD) / columns as i32, 4),
            _ => (width, 2),
        };
        Slot {
            x: GRID_PAD + (n % columns) as i32 * width,
            y: top + (n / columns) as i32 * step,
            width,
            height: step - gap,
        }
    }

    /// The visible slot under a client point, if any.
    pub fn slot_at(&self, x: i32, y: i32) -> Option<usize> {
        if self.collapsed || y >= self.height - ROW_BOTTOM_PADDING {
            return None;
        }
        (0..self.visible_slots()).find(|&n| {
            let s = self.slot_rect(n);
            x >= s.x && x < s.x + s.width && y >= s.y && y < s.y + s.height
        })
    }

    /// Exchange two existing pins without touching their target files.
    #[cfg(test)]
    pub fn swap_pins(&mut self, first: usize, second: usize) -> bool {
        if first == second || first >= self.paths.len() || second >= self.paths.len() {
            return false;
        }
        self.paths.swap(first, second);
        true
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Layout {
    #[serde(default = "layout_version")]
    pub version: u32,
    #[serde(default)]
    pub theme: Theme,
    #[serde(default)]
    pub fences: Vec<Fence>,
    #[serde(default)]
    pub history: Vec<crate::operations::Record>,
    #[serde(default)]
    pub pending: Option<crate::operations::Pending>,
    /// File new desktop items into category groups automatically.
    #[serde(default = "enabled")]
    pub auto_file: bool,
    /// Interface language code ("ja-JP"); absent follows Windows.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub language: Option<String>,
}

fn layout_version() -> u32 {
    1
}

fn enabled() -> bool {
    true
}

fn unused_fence_id(used: &HashSet<u64>, mut id: u64) -> u64 {
    while used.contains(&id) {
        id = id.wrapping_add(1);
    }
    id
}

pub const NEW_WIDTH: i32 = 360;
pub const NEW_HEIGHT: i32 = 420;
const FENCE_GAP: i32 = 24;

/// A rectangle in layout (desktop client) coordinates.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Area {
    pub left: i32,
    pub top: i32,
    pub right: i32,
    pub bottom: i32,
}

fn overlaps_any(fences: &[Fence], x: i32, y: i32, width: i32, height: i32) -> bool {
    // Use the expanded height so a collapsed collection keeps its space.
    fences.iter().any(|f| {
        x < f.x.saturating_add(f.width).saturating_add(FENCE_GAP)
            && f.x < x.saturating_add(width).saturating_add(FENCE_GAP)
            && y < f.y.saturating_add(f.height).saturating_add(FENCE_GAP)
            && f.y < y.saturating_add(height).saturating_add(FENCE_GAP)
    })
}

/// Free space beside the existing workspace: right of or below a collection
/// first (newest first), then a scan of the work area. Falls back to a
/// cascade from the newest collection when everything is occupied.
pub fn free_position(fences: &[Fence], width: i32, height: i32, area: Option<Area>) -> (i32, i32) {
    let inside = |x: i32, y: i32| {
        area.is_none_or(|a| {
            x >= a.left
                && y >= a.top
                && x.saturating_add(width) <= a.right
                && y.saturating_add(height) <= a.bottom
        })
    };
    let fits = |x: i32, y: i32| inside(x, y) && !overlaps_any(fences, x, y, width, height);
    let Some(last) = fences.last() else {
        return area.map_or((220, 120), |a| (a.left + FENCE_GAP, a.top + FENCE_GAP));
    };
    for f in fences.iter().rev() {
        let beside = [
            (f.x.saturating_add(f.width).saturating_add(FENCE_GAP), f.y),
            (f.x, f.y.saturating_add(f.height).saturating_add(FENCE_GAP)),
        ];
        if let Some(&(x, y)) = beside.iter().find(|(x, y)| fits(*x, *y)) {
            return (x, y);
        }
    }
    if let Some(a) = area {
        let mut y = a.top + FENCE_GAP;
        while y.saturating_add(height) <= a.bottom {
            let mut x = a.left + FENCE_GAP;
            while x.saturating_add(width) <= a.right {
                if fits(x, y) {
                    return (x, y);
                }
                x += FENCE_GAP;
            }
            y += FENCE_GAP;
        }
    }
    let (mut x, mut y) = (last.x.saturating_add(48), last.y.saturating_add(44));
    if let Some(a) = area {
        x = x.min(a.right - width).max(a.left);
        y = y.min(a.bottom - height).max(a.top);
    }
    (x, y)
}

/// Details columns as (key, left, width). Without custom widths, columns
/// that do not fit are hidden (width 0); with them, type then date yield
/// when the name column would get too narrow.
pub fn detail_columns(width: i32, custom: Option<[DetailColumn; 3]>) -> [(SortKey, i32, i32); 4] {
    let right = width - 12;
    let (mut date_w, mut type_w, size_w) = match custom {
        Some([date, kind, size]) => (
            if date.visible { date.width } else { 0 },
            if kind.visible { kind.width } else { 0 },
            if size.visible { size.width } else { 0 },
        ),
        None => (
            if width >= 380 { 150 } else { 0 },
            if width >= 520 { 120 } else { 0 },
            72,
        ),
    };
    // The name column keeps a usable width; detail columns give way.
    let name_room = |d: i32, t: i32| right - size_w - t - d - (GRID_PAD + 6);
    if name_room(date_w, type_w) < MIN_NAME_COLUMN {
        type_w = 0;
    }
    if name_room(date_w, type_w) < MIN_NAME_COLUMN {
        date_w = 0;
    }
    let size_x = right - size_w;
    let type_x = size_x - type_w;
    let date_x = type_x - date_w;
    let name_x = GRID_PAD + 6;
    [
        (SortKey::Name, name_x, date_x - name_x),
        (SortKey::Modified, date_x, date_w),
        (SortKey::Type, type_x, type_w),
        (SortKey::Size, size_x, size_w),
    ]
}

/// Gap left between snapped collections.
pub const SNAP_GAP: i32 = 8;
pub const SNAP_DISTANCE: i32 = 10;

/// Which edges of the moving rectangle snapped (for guide lines).
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Snapped {
    pub left: bool,
    pub top: bool,
    pub right: bool,
    pub bottom: bool,
}

/// Rectangles are (x, y, width, height) in layout coordinates.
pub type Bounds = (i32, i32, i32, i32);

/// Closest adjustment within SNAP_DISTANCE that puts `value` on a target.
fn nearest(value: i32, targets: impl Iterator<Item = i32>) -> Option<i32> {
    targets
        .map(|t| t - value)
        .filter(|d| d.abs() <= SNAP_DISTANCE)
        .min_by_key(|d| d.abs())
}

/// Edge positions a left/top edge may snap to, and a right/bottom edge may
/// snap to, along one axis: other collections' edges (aligned, or beside
/// them with SNAP_GAP) and the work-area edges inset by SNAP_GAP.
fn axis_targets(others: &[Bounds], areas: &[Area], horizontal: bool) -> (Vec<i32>, Vec<i32>) {
    let mut starts = Vec::new();
    let mut ends = Vec::new();
    for &(x, y, w, h) in others {
        let (lo, hi) = if horizontal { (x, x + w) } else { (y, y + h) };
        starts.extend([lo, hi + SNAP_GAP]);
        ends.extend([hi, lo - SNAP_GAP]);
    }
    for a in areas {
        let (lo, hi) = if horizontal {
            (a.left, a.right)
        } else {
            (a.top, a.bottom)
        };
        starts.push(lo + SNAP_GAP);
        ends.push(hi - SNAP_GAP);
    }
    (starts, ends)
}

/// Snaps a moving collection's position; its size never changes.
pub fn snap_move(rect: Bounds, others: &[Bounds], areas: &[Area]) -> (i32, i32, Snapped) {
    let (x, y, w, h) = rect;
    let mut snapped = Snapped::default();
    let solve = |start: i32, size: i32, horizontal: bool, snapped: &mut Snapped| {
        let (starts, ends) = axis_targets(others, areas, horizontal);
        let by_start = nearest(start, starts.into_iter());
        let by_end = nearest(start + size, ends.into_iter());
        let pick = match (by_start, by_end) {
            (Some(a), Some(b)) if b.abs() < a.abs() => Some((b, false)),
            (Some(a), _) => Some((a, true)),
            (None, Some(b)) => Some((b, false)),
            (None, None) => None,
        };
        if let Some((delta, is_start)) = pick {
            if horizontal {
                (snapped.left, snapped.right) = (is_start, !is_start);
            } else {
                (snapped.top, snapped.bottom) = (is_start, !is_start);
            }
            start + delta
        } else {
            start
        }
    };
    let nx = solve(x, w, true, &mut snapped);
    let ny = solve(y, h, false, &mut snapped);
    (nx, ny, snapped)
}

/// Snaps the right and/or bottom edge while resizing.
pub fn snap_resize(
    rect: Bounds,
    others: &[Bounds],
    areas: &[Area],
    horizontal: bool,
    vertical: bool,
) -> (i32, i32, Snapped) {
    let (x, y, mut w, mut h) = rect;
    let mut snapped = Snapped::default();
    if horizontal {
        let (_, ends) = axis_targets(others, areas, true);
        if let Some(d) = nearest(x + w, ends.into_iter()) {
            w += d;
            snapped.right = true;
        }
    }
    if vertical {
        let (_, ends) = axis_targets(others, areas, false);
        if let Some(d) = nearest(y + h, ends.into_iter()) {
            h += d;
            snapped.bottom = true;
        }
    }
    (w, h, snapped)
}

const TILE_GAP: i32 = 16;

/// Overlap-free shelf layout (left to right, then the next shelf) that keeps
/// every collection's size. Pass the expanded height of collapsed groups so
/// expanding one later cannot cover its neighbour. Only when the work area is
/// too small do the last shelves stack, clamped so headers stay reachable.
pub fn tile_positions(sizes: &[(i32, i32)], area: Area) -> Vec<(i32, i32)> {
    let left = area.left + TILE_GAP;
    let (mut x, mut y, mut shelf) = (left, area.top + TILE_GAP, 0);
    let lowest = (area.bottom - HEADER_HEIGHT - TILE_GAP).max(area.top);
    sizes
        .iter()
        .map(|&(width, height)| {
            if x > left && x.saturating_add(width) > area.right - TILE_GAP {
                x = left;
                y = y.saturating_add(shelf).saturating_add(TILE_GAP);
                shelf = 0;
            }
            let position = (x, y.min(lowest));
            x = x.saturating_add(width).saturating_add(TILE_GAP);
            shelf = shelf.max(height);
            position
        })
        .collect()
}

fn corrupt_path(path: &Path) -> PathBuf {
    path.with_extension(format!(
        "json.corrupt-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    ))
}

impl Default for Layout {
    fn default() -> Self {
        Self {
            version: 3,
            history: Vec::new(),
            pending: None,
            auto_file: true,
            language: None,
            theme: Theme::Midnight,
            fences: vec![Fence {
                id: 1,
                title: crate::i18n::t(crate::i18n::K::DefaultGroupTitle).to_string(),
                x: 240,
                y: 150,
                width: 360,
                height: 420,
                collapsed: false,
                paths: vec![],
                entries: vec![],
                list_view: false,
                view: ViewMode::Icons,
                sort: SortKey::Manual,
                sort_desc: false,
                details: None,
                size: None,
            }],
        }
    }
}

impl Layout {
    pub fn load(path: &Path) -> io::Result<Self> {
        match Self::read_existing(path) {
            Ok(layout) => Ok(layout),
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                let backup = path.with_extension("json.bak");
                match Self::read_existing(&backup) {
                    Ok(layout) => Ok(layout),
                    Err(backup_err) if backup_err.kind() == io::ErrorKind::NotFound => {
                        Ok(Self::default())
                    }
                    Err(backup_err) => Err(backup_err),
                }
            }
            Err(e) if e.kind() == io::ErrorKind::InvalidData => {
                // Keep corrupt state for inspection and restore the last valid
                // layout rather than replacing user pins with defaults.
                let Ok(backup) = Self::read_existing(&path.with_extension("json.bak")) else {
                    return Err(e);
                };
                fs::rename(path, corrupt_path(path))?;
                Ok(backup)
            }
            Err(e) => Err(e),
        }
    }

    fn read_existing(path: &Path) -> io::Result<Self> {
        let data = fs::read(path)?;
        let mut layout: Self = serde_json::from_slice(&data)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        if ![1, 2, 3].contains(&layout.version) {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                format!(
                    "Unsupported layout version {} (expected 1, 2 or 3)",
                    layout.version
                ),
            ));
        }
        layout.sanitize();
        Ok(layout)
    }

    pub fn save(&self, path: &Path) -> io::Result<()> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let tmp = path.with_extension("json.tmp");
        let serialized = serde_json::to_vec_pretty(self)?;
        use std::io::Write;
        let mut file = fs::File::create(&tmp)?;
        file.write_all(&serialized)?;
        file.sync_all()?;
        drop(file);
        // Keep the last *valid* layout. An externally corrupted primary must
        // never overwrite a good backup; a newer schema must not be replaced.
        let protected = (|| -> io::Result<()> {
            if !path.exists() {
                return Ok(());
            }
            match Self::read_existing(path) {
                Ok(_) => {
                    fs::copy(path, path.with_extension("json.bak"))?;
                    Ok(())
                }
                Err(err) if err.kind() == io::ErrorKind::InvalidData => {
                    fs::copy(path, corrupt_path(path))?;
                    Ok(())
                }
                Err(err) => Err(err),
            }
        })();
        if let Err(err) = protected {
            let _ = fs::remove_file(&tmp);
            return Err(err);
        }
        fs::rename(tmp, path)?;
        Ok(())
    }

    #[cfg(test)]
    pub fn add_fence(&mut self) -> Fence {
        self.add_fence_in(None)
    }

    /// Adds a collection in free space. `area` is the work area in layout
    /// (desktop client) coordinates; without it only overlap is avoided.
    pub fn add_fence_in(&mut self, area: Option<Area>) -> Fence {
        let used: HashSet<u64> = self.fences.iter().map(|f| f.id).collect();
        let next = unused_fence_id(
            &used,
            self.fences
                .iter()
                .map(|f| f.id)
                .max()
                .unwrap_or(0)
                .wrapping_add(1),
        );
        let (x, y) = free_position(&self.fences, NEW_WIDTH, NEW_HEIGHT, area);
        let fence = Fence {
            id: next,
            title: crate::i18n::tf(crate::i18n::K::NewGroupTitle, &[&next]),
            x,
            y,
            width: NEW_WIDTH,
            height: NEW_HEIGHT,
            collapsed: false,
            paths: vec![],
            entries: vec![],
            list_view: false,
            view: ViewMode::Icons,
            sort: SortKey::Manual,
            sort_desc: false,
            details: None,
            size: None,
        };
        self.fences.push(fence.clone());
        fence
    }

    pub fn sanitize(&mut self) {
        let mut ids = HashSet::new();
        let mut next = self
            .fences
            .iter()
            .map(|f| f.id)
            .max()
            .unwrap_or(0)
            .wrapping_add(1);
        for fence in &mut self.fences {
            if !ids.insert(fence.id) {
                next = unused_fence_id(&ids, next);
                fence.id = next;
                ids.insert(next);
                next = next.wrapping_add(1);
            }
            fence.width = fence.width.clamp(MIN_WIDTH, 1000);
            fence.height = fence.height.clamp(MIN_HEIGHT, 1000);
            // Do not discard existing titles, repeated paths, or excess pins.
            // The cap and duplicate check apply only when adding new items.
        }
    }
}

pub fn config_path() -> PathBuf {
    let root = std::env::var_os("APPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));
    root.join("LumaDesk").join("layout.json")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_layout() {
        let original = Layout::default();
        let encoded = serde_json::to_vec(&original).unwrap();
        let decoded: Layout = serde_json::from_slice(&encoded).unwrap();
        assert_eq!(decoded.fences[0].width, 360);
        assert_eq!(decoded.theme, Theme::Midnight);
    }

    #[test]
    fn collapsed_window_remembers_expanded_size() {
        let mut f = Layout::default().fences.remove(0);
        f.collapsed = true;
        assert_eq!(f.window_height(), COLLAPSED_HEIGHT);
        assert_eq!(f.height, 420);
    }

    #[test]
    fn sanitize_repairs_duplicate_ids_without_losing_collections() {
        let mut layout = Layout::default();
        let mut duplicate = layout.fences[0].clone();
        duplicate.paths.push("important.txt".into());
        layout.fences.push(duplicate);
        layout.fences[0].height = 1;
        layout.sanitize();
        assert_eq!(layout.fences.len(), 2);
        assert_ne!(layout.fences[0].id, layout.fences[1].id);
        assert_eq!(layout.fences[0].height, MIN_HEIGHT);
        assert_eq!(layout.fences[1].paths[0], PathBuf::from("important.txt"));
    }

    #[test]
    fn sanitization_preserves_full_unicode_title() {
        let mut layout = Layout::default();
        layout.fences[0].title = "工作區".repeat(50);
        layout.sanitize();
        assert_eq!(layout.fences[0].title.chars().count(), 150);
    }

    #[test]
    fn large_existing_collections_survive_loading_without_truncation() {
        let mut layout = Layout::default();
        let fence = &mut layout.fences[0];
        fence.paths = (0..190)
            .map(|i| PathBuf::from(format!("pin-{i}.txt")))
            .collect();
        fence.paths.push(PathBuf::from("pin-1.txt"));
        layout.sanitize();
        assert_eq!(layout.fences[0].paths.len(), 191);
        assert_eq!(
            layout.fences[0].paths.last(),
            Some(&PathBuf::from("pin-1.txt"))
        );
    }

    #[test]
    fn rejects_future_layout_versions_without_overwriting_files() {
        let folder = std::env::temp_dir().join(format!(
            "lumadesk-future-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&folder).unwrap();
        let path = folder.join("layout.json");
        let original = r#"{"version":42,"future_option":"preserve this"}"#;
        fs::write(&path, original).unwrap();
        assert_eq!(
            Layout::load(&path).unwrap_err().kind(),
            io::ErrorKind::Unsupported
        );
        assert_eq!(fs::read_to_string(&path).unwrap(), original);
        assert_eq!(
            Layout::default().save(&path).unwrap_err().kind(),
            io::ErrorKind::Unsupported
        );
        assert_eq!(fs::read_to_string(&path).unwrap(), original);
        fs::remove_dir_all(folder).unwrap();
    }

    #[test]
    fn saving_after_external_corruption_keeps_the_valid_backup() {
        let folder = std::env::temp_dir().join(format!(
            "lumadesk-preservation-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let path = folder.join("layout.json");
        let mut layout = Layout::default();
        layout.save(&path).unwrap();
        layout.save(&path).unwrap();
        fs::write(&path, "{external corruption").unwrap();
        layout.fences[0].title = "Updated layout".into();
        layout.save(&path).unwrap();
        let backup = Layout::load(&path.with_extension("json.bak")).unwrap();
        assert_eq!(backup.fences[0].title, Layout::default().fences[0].title);
        let recovered = Layout::load(&path).unwrap();
        assert_eq!(recovered.fences[0].title, "Updated layout");
        assert!(folder.read_dir().unwrap().any(|item| {
            item.unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with("layout.json.corrupt-")
        }));
        fs::remove_dir_all(folder).unwrap();
    }

    #[test]
    fn missing_primary_uses_existing_backup_after_interrupted_recovery() {
        let folder = std::env::temp_dir().join(format!(
            "lumadesk-interrupted-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let path = folder.join("layout.json");
        let mut layout = Layout::default();
        layout.fences[0]
            .paths
            .push(PathBuf::from("user-important.txt"));
        layout.save(&path).unwrap();
        layout.save(&path).unwrap();
        fs::remove_file(&path).unwrap();
        let recovered = Layout::load(&path).unwrap();
        assert_eq!(recovered.fences[0].paths, layout.fences[0].paths);
        fs::remove_dir_all(folder).unwrap();
    }

    #[test]
    fn can_add_fence_even_if_imported_id_is_maximum() {
        let mut layout = Layout::default();
        layout.fences[0].id = u64::MAX;
        let next = layout.add_fence();
        assert_ne!(next.id, u64::MAX);
        assert_eq!(layout.fences.len(), 2);
    }

    #[test]
    fn new_collection_follows_current_workspace_position() {
        let mut layout = Layout::default();
        layout.fences[0].x = 2100;
        layout.fences[0].y = 1150;
        let fence = layout.add_fence();
        assert_eq!((fence.x, fence.y), (2100 + 360 + 24, 1150));
    }

    #[test]
    fn moving_snaps_beside_and_aligned_with_neighbours_and_area_edges() {
        let area = Area {
            left: 0,
            top: 0,
            right: 1920,
            bottom: 1032,
        };
        let other = (100, 100, 300, 380);
        // 5 px right of the neighbour's right edge + gap -> snaps beside it,
        // and 4 px below its top -> aligns tops.
        let (x, y, s) = snap_move((413, 104, 300, 380), &[other], &[area]);
        assert_eq!((x, y), (408, 100));
        assert!(s.left && s.top);
        // Far from everything: unchanged.
        let (x, y, s) = snap_move((900, 500, 300, 380), &[other], &[area]);
        assert_eq!((x, y), (900, 500));
        assert_eq!(s, Snapped::default());
        // Near the work-area corner.
        let (x, y, _) = snap_move((3, 12, 300, 380), &[], &[area]);
        assert_eq!((x, y), (8, 8));
        // Right edge 6 px from the area's right inset.
        let (x, _, s) = snap_move((1606, 500, 300, 380), &[], &[area]);
        assert_eq!(x, 1912 - 300);
        assert!(s.right);
    }

    #[test]
    fn resizing_snaps_right_and_bottom_edges() {
        let other = (500, 100, 300, 380);
        let (w, h, s) = snap_resize((100, 100, 385, 375), &[other], &[], true, true);
        // Right edge to the neighbour's left minus gap; bottom aligned.
        assert_eq!((w, h), (392, 380));
        assert!(s.right && s.bottom);
        let (w, h, _) = snap_resize((100, 100, 385, 375), &[other], &[], false, true);
        assert_eq!((w, h), (385, 380));
    }

    #[test]
    fn view_and_sort_round_trip_and_columns_hide_when_narrow() {
        let mut fence = Layout::default().fences.remove(0);
        fence.set_mode(ViewMode::Details);
        fence.sort = SortKey::Size;
        fence.sort_desc = true;
        let json = serde_json::to_string(&fence).unwrap();
        let back: Fence = serde_json::from_str(&json).unwrap();
        assert_eq!(
            (back.mode(), back.sort, back.sort_desc),
            (ViewMode::Details, SortKey::Size, true)
        );
        let widths = |w| detail_columns(w, None).map(|(_, _, width)| width);
        assert!(widths(600).iter().all(|&w| w > 0));
        assert_eq!(widths(450)[2], 0); // type hidden
        assert_eq!(widths(300)[1], 0); // date hidden too
        assert!(widths(300)[0] > 100 && widths(300)[3] > 0);
        // Small icons wrap in columns; list and details are single-column.
        fence.width = 360;
        fence.set_mode(ViewMode::SmallIcons);
        assert_eq!(fence.columns(), 2);
        fence.set_mode(ViewMode::List);
        assert_eq!(fence.columns(), 1);
    }

    #[test]
    fn custom_detail_columns_resize_hide_and_keep_the_name_usable() {
        let column = |width, visible| DetailColumn { width, visible };
        let custom = Some([column(200, true), column(60, false), column(90, true)]);
        let c = detail_columns(600, custom);
        assert_eq!((c[1].2, c[2].2, c[3].2), (200, 0, 90));
        // Right-anchored: size ends at the right margin, date sits left of it.
        assert_eq!(c[3].1 + c[3].2, 600 - 12);
        assert_eq!(c[1].1 + c[1].2, c[3].1);
        assert!(c[0].2 >= MIN_NAME_COLUMN);
        // Too wide for the group: type, then date give way to the name.
        let wide = Some([column(300, true), column(200, true), column(90, true)]);
        let c = detail_columns(450, wide);
        assert_eq!(c[2].2, 0);
        assert!(c[0].2 >= MIN_NAME_COLUMN || c[1].2 == 0);
        let mut fence = Layout::default().fences.remove(0);
        fence.width = 600;
        let settings = fence.detail_settings();
        assert!(settings.iter().all(|c| c.visible && c.width > 0));
        fence.details = custom;
        let back: Fence = serde_json::from_str(&serde_json::to_string(&fence).unwrap()).unwrap();
        assert_eq!(back.details, custom);
    }

    #[test]
    fn item_size_levels_scale_rows_tiles_and_persist() {
        let mut fence = Layout::default().fences.remove(0);
        fence.width = 560;
        fence.height = 420;
        assert_eq!(fence.size_level(), DEFAULT_SIZE);
        let default_json = serde_json::to_string(&fence).unwrap();
        assert!(!default_json.contains("\"size\""));
        let grid_columns = fence.columns();
        fence.size = Some(4);
        assert!(fence.columns() < grid_columns);
        assert_eq!(fence.icon_pixels(), 128);
        fence.set_mode(ViewMode::Details);
        assert_eq!(fence.icon_pixels(), 40);
        let big_rows = fence.visible_rows();
        fence.size = Some(0);
        assert!(fence.visible_rows() > big_rows);
        assert_eq!(
            fence.slot_rect(1).y - fence.slot_rect(0).y,
            SIZES[0].line_height
        );
        fence.size = Some(9); // clamped
        assert_eq!(fence.size_level(), SIZES.len() - 1);
        let back: Fence = serde_json::from_str(&serde_json::to_string(&fence).unwrap()).unwrap();
        assert_eq!(back.size, Some(9));
    }

    #[test]
    fn grid_is_the_default_and_old_layouts_load_as_grid() {
        let json =
            r#"{"version":3,"fences":[{"id":1,"title":"t","x":0,"y":0,"width":300,"height":380}]}"#;
        let layout: Layout = serde_json::from_str(json).unwrap();
        assert!(!layout.fences[0].list_view);
        let saved = serde_json::to_string(&layout).unwrap();
        assert!(!saved.contains("list_view"));
        let mut list = layout.fences[0].clone();
        list.list_view = true;
        assert!(serde_json::to_string(&list)
            .unwrap()
            .contains("\"list_view\":true"));
    }

    #[test]
    fn grid_geometry_scrolls_by_whole_rows_and_hit_tests_tiles() {
        let mut fence = Layout::default().fences.remove(0);
        fence.width = 300;
        fence.height = 380;
        assert_eq!(fence.columns(), 3);
        assert_eq!(fence.visible_rows(), 3);
        assert_eq!(fence.visible_slots(), 9);
        fence.paths = (0..20).map(|i| PathBuf::from(format!("{i}"))).collect();
        // 20 items -> 7 rows; 3 visible -> scroll up to 4 rows = 12 items.
        assert_eq!(fence.max_scroll(), 12);
        assert_eq!(fence.scroll_to_show(0, 10), 3);
        assert_eq!(fence.scroll_to_show(9, 1), 0);
        assert_eq!(fence.scroll_to_show(3, 5), 3);
        let s = fence.slot_rect(4);
        assert_eq!(fence.slot_at(s.x + 5, s.y + 5), Some(4));
        assert_eq!(fence.slot_at(2, s.y + 5), None);
        assert_eq!(fence.slot_at(s.x + 5, 20), None);
        fence.width = 1000;
        assert_eq!(fence.columns(), 11);
    }

    #[test]
    fn tiled_collections_keep_sizes_and_never_overlap() {
        let area = Area {
            left: 0,
            top: 0,
            right: 1920,
            bottom: 1032,
        };
        let sizes = [
            (300, 380),
            (300, 380),
            (534, 368),
            (300, 380),
            (360, 420),
            (300, 380),
            (300, 380),
        ];
        let placed = tile_positions(&sizes, area);
        let rects: Vec<_> = placed
            .iter()
            .zip(sizes)
            .map(|(&(x, y), (w, h))| (x, y, w, h))
            .collect();
        for (i, a) in rects.iter().enumerate() {
            assert!(a.0 >= 0 && a.0 + a.2 <= 1920 && a.1 + a.3 <= 1032, "{a:?}");
            for b in &rects[i + 1..] {
                let apart =
                    a.0 + a.2 <= b.0 || b.0 + b.2 <= a.0 || a.1 + a.3 <= b.1 || b.1 + b.3 <= a.1;
                assert!(apart, "{a:?} overlaps {b:?}");
            }
        }
    }

    fn intersects(a: &Fence, b: &Fence) -> bool {
        a.x < b.x + b.width && b.x < a.x + a.width && a.y < b.y + b.height && b.y < a.y + a.height
    }

    #[test]
    fn new_collections_never_overlap_and_stay_in_the_work_area() {
        let area = Area {
            left: 0,
            top: 0,
            right: 1920,
            bottom: 1040,
        };
        let mut layout = Layout::default();
        layout.fences[0].x = 240;
        layout.fences[0].y = 150;
        for _ in 0..7 {
            layout.add_fence_in(Some(area));
        }
        for (i, a) in layout.fences.iter().enumerate() {
            assert!(a.x >= 0 && a.y >= 0 && a.x + a.width <= 1920 && a.y + a.height <= 1040);
            for b in &layout.fences[i + 1..] {
                assert!(!intersects(a, b), "{a:?} overlaps {b:?}");
            }
        }
    }

    #[test]
    fn collapsed_collections_keep_their_expanded_space() {
        let mut layout = Layout::default();
        layout.fences[0].x = 0;
        layout.fences[0].y = 0;
        layout.fences[0].collapsed = true;
        let area = Area {
            left: 0,
            top: 0,
            right: 400,
            bottom: 2000,
        };
        let fence = layout.add_fence_in(Some(area));
        assert!(fence.y >= 420, "placed at {}", fence.y);
    }

    #[test]
    fn full_work_area_falls_back_to_a_visible_cascade() {
        let area = Area {
            left: 0,
            top: 0,
            right: 500,
            bottom: 500,
        };
        let mut layout = Layout::default();
        layout.fences[0].x = 0;
        layout.fences[0].y = 0;
        let fence = layout.add_fence_in(Some(area));
        assert!(fence.x + fence.width <= 500 && fence.y + fence.height <= 500);
    }

    #[test]
    fn scrolling_uses_the_same_row_geometry_at_every_height() {
        let mut fence = Layout::default().fences.remove(0);
        // The legacy row flag is read as the compact List view.
        fence.list_view = true;
        assert_eq!(fence.mode(), ViewMode::List);
        assert_eq!(fence.visible_rows(), 14);
        fence.paths = (0..20)
            .map(|i| PathBuf::from(format!("item-{i}")))
            .collect();
        assert_eq!(fence.max_scroll(), 6);
        fence.height = MIN_HEIGHT;
        assert_eq!(fence.visible_rows(), 4);
        assert_eq!(fence.max_scroll(), 16);
        // Details rows start below the column titles.
        fence.set_mode(ViewMode::Details);
        assert!(!fence.list_view);
        assert_eq!(fence.slot_rect(0).y, DETAILS_TOP);
        assert_eq!(fence.visible_rows(), 4);
        fence.paths.clear();
        assert_eq!(fence.max_scroll(), 0);
    }

    #[test]
    fn pin_reordering_preserves_items_and_rejects_invalid_indices() {
        let mut fence = Layout::default().fences.remove(0);
        fence.paths = vec!["first.txt".into(), "second.txt".into(), "third.txt".into()];
        assert!(!fence.swap_pins(0, 3));
        assert!(!fence.swap_pins(0, 0));
        assert!(fence.swap_pins(0, 2));
        assert_eq!(fence.paths[0], PathBuf::from("third.txt"));
        assert_eq!(fence.paths[1], PathBuf::from("second.txt"));
        assert_eq!(fence.paths[2], PathBuf::from("first.txt"));
    }

    #[test]
    fn backup_recovers_corrupt_primary_without_destroying_it() {
        let folder = std::env::temp_dir().join(format!(
            "lumadesk-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let path = folder.join("layout.json");
        let layout = Layout::default();
        layout.save(&path).unwrap();
        layout.save(&path).unwrap(); // creates a valid backup
        fs::write(&path, "{bad json").unwrap();
        let recovered = Layout::load(&path).unwrap();
        assert_eq!(recovered.fences.len(), 1);
        assert!(!path.exists());
        assert!(folder.read_dir().unwrap().any(|e| {
            e.unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with("layout.json.corrupt-")
        }));
        fs::remove_dir_all(folder).unwrap();
    }
}
