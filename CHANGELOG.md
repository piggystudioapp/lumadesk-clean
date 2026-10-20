# Changelog

LumaDesk follows semantic versioning; while it is a preview, interfaces may still change between minor versions.

## 0.8.0 — 2026-10-20

First public release.

- Desktop groups drawn natively on the Windows desktop (Win32 child windows of the desktop view, Direct2D/DirectWrite rendering). Groups hold references only: files, folders, programs and shortcuts stay at their original paths and are never moved, copied, renamed or hidden.
- Automatic organizing with a preview: personal and public desktop items are sorted into web links, documents, pictures, media, archives, folders, programs, scripts and other. Hidden, system, linked and cloud-only items are left in place with a reason.
- Optional auto-filing of new desktop items into their category group (on by default); downloads are filed once they have their final name.
- Original desktop icons are moved out of view while grouped and restored on ungroup or exit; positions are journaled and a separate watchdog restores them if LumaDesk stops unexpectedly.
- Views per group: large icons, small icons, list and details, with sorting by name, date, type or size, resizable and selectable details columns, and five item sizes (Ctrl+mouse wheel).
- Explorer-like handling: native context menu, F2 rename, Delete / Shift+Delete, type-to-select, Ctrl+F search, Ctrl/Shift multi-select, drag to reorder, between groups or back to the desktop, and paste copied files with Ctrl+V.
- Layout tools: snapping to other groups and work-area edges (Alt to place freely), edge resizing, collapsing, arranging without overlaps and recovering offscreen groups. Frosted wallpaper backdrop, light and dark themes.
- Persistent undo for the last 20 operations.
- Interface in 12 languages (繁體中文, 简体中文, English, 日本語, 한국어, Deutsch, Français, Español, Português (Brasil), Русский, Tiếng Việt, ไทย), following the Windows display language with a live switch under More › Language. Dates and number separators follow the selected language. Translations other than Traditional Chinese are machine-made.
- Tray icon and a launcher window that follows the Windows light/dark setting with a Mica backdrop.
- Offline: no telemetry, no update checks, no network access. Settings in `%APPDATA%\LumaDesk\layout.json` with an automatic backup; damaged settings are kept rather than overwritten.
- Portable ZIP with optional per-user install and uninstall scripts (no administrator rights). Builds are not yet code-signed.
