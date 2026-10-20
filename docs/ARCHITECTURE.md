# Architecture

LumaDesk retains the native Win32/Direct2D desktop-child windows, OLE drop targets, single-instance controller and Explorer-parent reconciliation. Shell icon loading runs on two background OLE apartments, with a shared path/type cache. The GUI polls ready handles and creates its own Direct2D bitmaps outside WM_PAINT. Extended filesystem prefixes are removed only at legacy Shell API boundaries, including opening items. Ordinary app windows appear above desktop groups. Opening an item runs the Shell launch on its own STA thread so slow handlers never block the desktop; LumaDesk itself starts in the user profile folder so launched programs do not inherit its install folder.

## Visual references

Item commands update references only: pin, ungroup, transfer, ordering, retarget after rename and undo are metadata changes keyed by stable IDs. No code path moves, copies or creates files.

`organize.rs` performs an offline, top-level, read-only classification. Applying a selected preview rechecks file identity and category, reuses/creates groups, enforces capacity, and commits one metadata batch. Programs, in-use folders and HTML companion assets retain all paths. Both personal and Public Desktop roots are classified without changing shared files or permissions; virtual desktop objects are excluded. LNK and ClickOnce shortcuts join Programs.

## Explorer display mask

Before creating desktop child windows or starting the display mask, the GUI checks that the desktop and its ancestors are enabled. A disabled desktop suspends groups, restores any owned mask, and leaves a nonblocking launcher status. LumaDesk never enables another process's windows. Modal messages use LumaDesk's own top-level owner rather than an Explorer-owned ancestor of a fence.

`desktop_icons.rs` binds `SWC_DESKTOP` through IShellWindows/IServiceProvider/IShellBrowser to IFolderView2, never an arbitrary SHELLDLL_DefView or another app's host window. It parses selected child names in the desktop IShellFolder, verifies each complete filesystem path and relative PIDL, uses actual view PIDLs to disambiguate duplicate personal/Public names, journals original coordinates plus filesystem identity, then parks grouped icons outside the visible desktop. No file attributes or contents change. Group windows show the original paths. Ungrouping restores the previous coordinates; cross-group transfers keep the mask.

Before hiding icons, an independent copy of the executable starts in `--desktop-guardian` mode, opens a synchronization handle to the UI process and acknowledges readiness. On parent exit it restores journaled coordinates and the original arrange/grid flags. Normal shutdown restores explicitly. Stale journals are retried on startup; missing view items retain their journal for retry. Replaced items already at another coordinate are left alone; a replacement still at the exact parking coordinate is returned so it cannot inherit invisibility. Explorer refresh/recreation is reconciled by the existing timer. Unchanged complete masks use a rotating position/flag probe between full audits (at most 30 seconds apart), avoiding repeated filesystem identity scans every three seconds. Missing Shell interfaces or a dead guardian leave a visible status and retained recovery journal. The watchdog restores display state only and never moves files.

The temporary display mask is not a file-system filter. Files remain visible in Explorer at their original Desktop paths. It does not suppress icons generated independently by another desktop manager. Simultaneous-manager compatibility is not assumed.

## Languages

`i18n.rs` holds one compiled-in table of every interface string in twelve languages (a test checks that every language has every key with the same `{0}` placeholders). Lookups are an array index; a missing entry falls back to English, then Traditional Chinese. The language follows `GetUserPreferredUILanguages` unless `layout.language` names one. Switching rebuilds the text formats, because DirectWrite picks CJK and Thai fallback fonts from the format's locale, and refreshes the launcher, tray tip and groups without a restart. The launcher shrinks text (to 72 %) that would not fit, then trims it. Dates use `GetDateFormatEx`/`GetTimeFormatEx` with the selected locale. Category groups are matched by their title in any language, so changing the language never creates duplicates or renames groups.

## Data and upgrade

Schema 3 holds references, stable IDs, positions/theme and the most recent 20 metadata operations. Atomic settings writes retain a valid backup. A serialized worker and CONFIG_LOCK preserve concurrent visual settings. Explorer display calls run in the GUI OLE apartment, outside painting, with no state borrow/config lock held across reentrant COM messages; batches flush a pending record before commit. Search changes clear selection. Unsupported destinations, Escape and capture loss cancel internal drags. OLE prefers LINK, then COPY, and rejects MOVE-only sources so they cannot infer a successful physical move.

Settings written by early previews (which could move files into `collections`) are migrated read-only in `operations::initialize`. Schema 1 is backed up (layout and receipts) and its paths are pointed at collected copies using the receipts. Below schema 3 the layout is copied to `before-virtual-groups`, entries stored as `managed` deserialize as references at their stored path, and history records carrying an old move are dropped. A pending journal that still contains an old move is refused with a message and left untouched, since its state cannot be verified without the retired engine.

Tests exercise reference-only behavior, busy directories, nested/Unicode content, shortcut targets, preview replacement/capacity checks, read-only legacy migration, translation completeness, native multi-selection/drag/drop, normal-exit coordinate restoration and watchdog restoration after intentionally terminating only an isolated fixture UI process. GUI smoke fixtures never organize the user's real candidates.
