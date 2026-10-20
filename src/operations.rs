//! Serialized reference transactions: journal the intent, then commit the layout.
//! Groups only reference files; no operation here moves or creates a file.
use crate::i18n::{self, K};
use crate::model::{unique_id, Entry, Fence, ItemKind, Layout};
use serde::{Deserialize, Serialize};
use std::fs::{self, File, OpenOptions};
use std::io;
use std::os::windows::fs::OpenOptionsExt;
use std::os::windows::io::AsRawHandle;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use windows::Win32::Foundation::HANDLE;
use windows::Win32::Storage::FileSystem::{
    GetFileInformationByHandle, BY_HANDLE_FILE_INFORMATION, FILE_FLAG_BACKUP_SEMANTICS,
    FILE_FLAG_OPEN_REPARSE_POINT,
};

pub static CONFIG_LOCK: Mutex<()> = Mutex::new(());

pub(crate) fn same_path(left: &Path, right: &Path) -> bool {
    fn spelling(path: &Path) -> String {
        let text = path.to_string_lossy().replace('/', "\\");
        if let Some(tail) = text.strip_prefix("\\\\?\\UNC\\") {
            format!("\\\\{tail}")
        } else {
            text.strip_prefix("\\\\?\\").unwrap_or(&text).to_owned()
        }
    }
    spelling(left).eq_ignore_ascii_case(&spelling(right))
        || (left
            .file_name()
            .zip(right.file_name())
            .is_some_and(|(a, b)| a.eq_ignore_ascii_case(b))
            && left
                .canonicalize()
                .ok()
                .zip(right.canonicalize().ok())
                .is_some_and(|(a, b)| spelling(&a).eq_ignore_ascii_case(&spelling(&b))))
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct Identity {
    volume: u32,
    high: u32,
    low: u32,
    created: u64,
}

pub fn identity(path: &Path) -> io::Result<Identity> {
    let file = OpenOptions::new()
        .access_mode(0x80) // FILE_READ_ATTRIBUTES; contents need not be readable.
        .share_mode(7)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS.0 | FILE_FLAG_OPEN_REPARSE_POINT.0)
        .open(path)?;
    let mut info = BY_HANDLE_FILE_INFORMATION::default();
    unsafe { GetFileInformationByHandle(HANDLE(file.as_raw_handle()), &mut info) }
        .map_err(|e| io::Error::other(e.to_string()))?;
    Ok(Identity {
        volume: info.dwVolumeSerialNumber,
        high: info.nFileIndexHigh,
        low: info.nFileIndexLow,
        created: ((info.ftCreationTime.dwHighDateTime as u64) << 32)
            | info.ftCreationTime.dwLowDateTime as u64,
    })
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Location {
    pub fence: u64,
    pub index: usize,
    pub entry: Entry,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Change {
    pub before: Option<Location>,
    pub after: Option<Location>,
    /// A physical move journaled by releases before 0.6. Read only so such
    /// records can be recognized and refused; new records never carry one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub action: Option<serde_json::Value>,
    pub guard: Option<Identity>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Record {
    pub id: String,
    pub label: String,
    pub time: u64,
    pub changes: Vec<Change>,
    #[serde(default)]
    pub groups: Vec<Fence>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Pending {
    pub record: Record,
    pub undo: bool,
}

#[derive(Clone, Debug)]
pub enum Command {
    PreviewAuto,
    AutoOrganize {
        plan: crate::organize::Plan,
    },
    Pin {
        fence: u64,
        paths: Vec<PathBuf>,
    },
    Desktop {
        ids: Vec<String>,
    },
    Transfer {
        ids: Vec<String>,
        fence: u64,
        before: Option<String>,
    },
    Undo,
    DeleteFence {
        fence: u64,
    },
    /// The referenced file was renamed through the Shell: point the same
    /// reference (same id, group and position) at its new path.
    Retarget {
        id: String,
        path: PathBuf,
    },
}

#[derive(Default)]
pub struct Outcome {
    pub completed: usize,
    pub failures: Vec<String>,
    pub preview: Option<crate::organize::Plan>,
}

fn err(message: &str) -> io::Error {
    io::Error::other(message)
}
fn root(path: &Path) -> &Path {
    path.parent().expect("layout directory")
}
fn timestamp() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}
fn empty_record(label: &str) -> Record {
    Record {
        id: unique_id(),
        label: label.into(),
        time: timestamp(),
        changes: vec![],
        groups: vec![],
    }
}

/// GUI geometry/theme writes must not overwrite transactions completed by the worker.
pub fn save_visual(layout: &Layout, path: &Path) -> io::Result<()> {
    let _lock = CONFIG_LOCK.lock().unwrap();
    let mut visual = layout.clone();
    if path.exists() {
        let disk = Layout::load(path)?;
        visual.history = disk.history;
        visual.pending = disk.pending;
        for fence in &mut visual.fences {
            if let Some(saved) = disk.fences.iter().find(|f| f.id == fence.id) {
                fence.set_entries(saved.entries.clone());
            }
        }
        // A worker can create category groups while the GUI still displays the
        // preceding layout. A geometry save must preserve those new owners.
        let known: Vec<_> = visual.fences.iter().map(|f| f.id).collect();
        visual
            .fences
            .extend(disk.fences.into_iter().filter(|f| !known.contains(&f.id)));
    }
    visual.save(path)
}

fn save_state(layout: &mut Layout, path: &Path) -> io::Result<()> {
    let _lock = CONFIG_LOCK.lock().unwrap();
    if path.exists() {
        let visual = Layout::load(path)?;
        layout.theme = visual.theme;
        layout.auto_file = visual.auto_file;
        layout.language = visual.language;
        for fence in &mut layout.fences {
            if let Some(v) = visual.fences.iter().find(|f| f.id == fence.id) {
                fence.title.clone_from(&v.title);
                (fence.x, fence.y, fence.width, fence.height, fence.collapsed) =
                    (v.x, v.y, v.width, v.height, v.collapsed);
                fence.list_view = v.list_view;
            }
        }
    }
    layout.save(path)
}

pub fn initialize(path: &Path) -> io::Result<Layout> {
    let mut layout = Layout::load(path)?;
    fs::create_dir_all(root(path))?;
    if layout.version == 1 {
        // Complete the backup before legacy reconciliation or schema conversion.
        let backup = root(path).join(format!("backup-v1-{}", unique_id()));
        fs::create_dir(&backup)?;
        if path.exists() {
            fs::copy(path, backup.join("layout.json"))?;
        }
        let old_backup = path.with_extension("json.bak");
        if old_backup.exists() {
            fs::copy(old_backup, backup.join("layout.json.bak"))?;
        }
        backup_receipts(&root(path).join("collections"), &backup.join("collections"))?;
        File::create(backup.join("BACKUP_COMPLETE"))?.sync_all()?;
        crate::collection::legacy_paths(root(path), &mut layout)?;
        layout.version = 2;
    }
    if layout.version < 3 {
        // Releases before 0.6 could move files into collections\. Those items
        // load as references at their stored path; nothing is moved back. Move
        // records are dropped so Ctrl+Z can never replay a physical move.
        let backup = root(path).join("before-virtual-groups");
        fs::create_dir_all(&backup)?;
        if path.exists() && !backup.join("layout.json").exists() {
            fs::copy(path, backup.join("layout.json"))?;
        }
        layout
            .history
            .retain(|r| r.changes.iter().all(|c| c.action.is_none()));
        layout.version = 3;
    }
    let mut ids = std::collections::HashSet::new();
    for fence in &mut layout.fences {
        fence.reconcile_entries();
        for entry in &fence.entries {
            if entry.id.is_empty()
                || !entry.id.chars().all(|c| c.is_ascii_hexdigit() || c == '-')
                || !ids.insert(entry.id.clone())
            {
                return Err(err(i18n::t(K::BadIds)));
            }
        }
    }
    layout.save(path)?;
    recover(path, &mut layout)?;
    Ok(layout)
}

fn backup_receipts(source: &Path, target: &Path) -> io::Result<()> {
    if !source.exists() {
        return Ok(());
    }
    for entry in fs::read_dir(source)? {
        let entry = entry?;
        if entry.file_type()?.is_dir() && entry.file_name() != "files" {
            backup_receipts(&entry.path(), &target.join(entry.file_name()))?;
        } else if entry.file_type()?.is_file() && entry.file_name() == "receipt.json" {
            fs::create_dir_all(target)?;
            fs::copy(entry.path(), target.join("receipt.json"))?;
        }
    }
    Ok(())
}

#[allow(clippy::filter_map_bool_then)]
fn locations(layout: &Layout, ids: &[String]) -> Vec<Location> {
    layout
        .fences
        .iter()
        .flat_map(|f| {
            f.entries.iter().enumerate().filter_map(|(index, e)| {
                ids.contains(&e.id).then(|| Location {
                    fence: f.id,
                    index,
                    entry: e.clone(),
                })
            })
        })
        .collect()
}

fn change_id(change: &Change) -> &str {
    &change
        .before
        .as_ref()
        .or(change.after.as_ref())
        .expect("item change")
        .entry
        .id
}

fn apply(layout: &mut Layout, changes: &[Change], undo: bool) {
    for fence in &mut layout.fences {
        fence
            .entries
            .retain(|e| !changes.iter().any(|c| change_id(c) == e.id));
    }
    let mut inserts: Vec<_> = changes
        .iter()
        .filter_map(|c| {
            if undo {
                c.before.clone()
            } else {
                c.after.clone()
            }
        })
        .collect();
    inserts.sort_by_key(|l| (l.fence, l.index));
    for item in inserts {
        if !layout.fences.iter().any(|f| f.id == item.fence) {
            layout.fences.push(Fence {
                id: item.fence,
                title: i18n::t(K::RecoveredGroup).into(),
                ..Layout::default().fences.remove(0)
            });
        }
        let fence = layout
            .fences
            .iter_mut()
            .find(|f| f.id == item.fence)
            .unwrap();
        fence
            .entries
            .insert(item.index.min(fence.entries.len()), item.entry);
    }
    for fence in &mut layout.fences {
        fence.paths = fence.entries.iter().map(|e| e.path.clone()).collect();
    }
}

fn committed(layout: &mut Layout, record: &Record, undo: bool) {
    let existing: Vec<_> = layout.fences.iter().map(|f| f.id).collect();
    apply(layout, &record.changes, undo);
    if undo {
        for fence in &mut layout.fences {
            if !existing.contains(&fence.id) {
                if let Some(original) = record.groups.iter().find(|f| f.id == fence.id) {
                    let entries = std::mem::take(&mut fence.entries);
                    *fence = original.clone();
                    fence.set_entries(entries);
                }
            }
        }
    }
    if undo {
        if let Some(old) = layout.history.iter_mut().find(|r| r.id == record.id) {
            old.changes.retain(|c| {
                !record
                    .changes
                    .iter()
                    .any(|done| change_id(done) == change_id(c))
            });
        }
        layout.history.retain(|r| !r.changes.is_empty());
    } else if let Some(old) = layout.history.iter_mut().find(|r| r.id == record.id) {
        old.changes.extend(record.changes.clone());
    } else {
        layout.history.push(record.clone());
        if layout.history.len() > 20 {
            layout.history.remove(0);
        }
    }
    layout.pending = None;
}

fn matches(path: &Path, expected: &Identity) -> bool {
    identity(path).is_ok_and(|actual| actual == *expected)
}

/// A journaled metadata change has no filesystem effect, so it is simply
/// committed. A pre-0.6 physical move cannot be verified without the retired
/// engine: refuse and keep the journal rather than guess where a file is.
pub fn recover(path: &Path, layout: &mut Layout) -> io::Result<()> {
    let Some(pending) = layout.pending.clone() else {
        return Ok(());
    };
    if pending.record.changes.iter().any(|c| c.action.is_some()) {
        return Err(err(i18n::t(K::LegacyPending)));
    }
    committed(layout, &pending.record, pending.undo);
    save_state(layout, path)
}

fn transaction(path: &Path, layout: &mut Layout, mut record: Record, undo: bool) -> io::Result<()> {
    if !undo && record.groups.is_empty() {
        record.groups = layout
            .fences
            .iter()
            .map(|f| {
                let mut group = f.clone();
                group.paths.clear();
                group.entries.clear();
                group
            })
            .collect();
    }
    layout.pending = Some(Pending {
        record: record.clone(),
        undo,
    });
    save_state(layout, path)?;
    committed(layout, &record, undo);
    if let Err(error) = save_state(layout, path) {
        layout.pending = Some(Pending { record, undo });
        return Err(error);
    }
    Ok(())
}

pub fn execute(
    path: &Path,
    command: Command,
    desktops: &[PathBuf],
    mut progress: impl FnMut(usize, usize),
) -> io::Result<Outcome> {
    if matches!(command, Command::Undo)
        && Layout::load(path)?
            .history
            .last()
            .is_some_and(|r| r.changes.iter().any(|c| c.action.is_some()))
    {
        return Err(err(i18n::t(K::LegacyUndo)));
    }
    execute_with_progress(path, command, desktops, &mut progress)
}

fn execute_with_progress(
    path: &Path,
    command: Command,
    desktops: &[PathBuf],
    progress: &mut dyn FnMut(usize, usize),
) -> io::Result<Outcome> {
    if matches!(command, Command::PreviewAuto) {
        if desktops.is_empty() {
            return Err(err(i18n::t(K::NoDesktop)));
        }
        return Ok(Outcome {
            preview: Some(crate::organize::scan(desktops, root(path))?),
            ..Outcome::default()
        });
    }
    let mut layout = {
        let _lock = CONFIG_LOCK.lock().unwrap();
        Layout::load(path)?
    };
    if layout.pending.is_some() {
        recover(path, &mut layout)?;
    }
    let mut outcome = Outcome::default();
    match command {
        Command::PreviewAuto => unreachable!(),
        Command::AutoOrganize { plan } => {
            let total = plan.items.len();
            let mut record = empty_record(i18n::t(K::LabelAuto));
            let original_layout = layout.clone();
            for (number, candidate) in plan.items.iter().enumerate() {
                let source = candidate.path.clone();
                let result = (|| -> io::Result<Change> {
                    // Only references are changed, including shared desktop items.
                    // Revalidate that this is still an immediate desktop child.
                    if !crate::collection::is_desktop_item(&source, desktops) {
                        return Err(err(i18n::t(K::NotOnDesktop)));
                    }
                    let current = crate::organize::inspect(&source)?;
                    if current.identity != candidate.identity
                        || current.category != candidate.category
                    {
                        return Err(err(i18n::t(K::ChangedSincePreview)));
                    }
                    crate::organize::protect_storage(&source, root(path))?;
                    let references: Vec<_> = original_layout
                        .fences
                        .iter()
                        .flat_map(|f| {
                            f.entries
                                .iter()
                                .enumerate()
                                .filter(|(_, entry)| same_path(&entry.path, &source))
                                .map(|(index, entry)| Location {
                                    fence: f.id,
                                    index,
                                    entry: entry.clone(),
                                })
                        })
                        .collect();
                    if references.len() > 1 {
                        return Err(err(i18n::t(K::MultipleRefs)));
                    }
                    let before = references.into_iter().next();
                    // Reuse the category group even if it was created while
                    // another language was selected; existing titles are kept.
                    let title = candidate.category.title();
                    let target_index = match layout
                        .fences
                        .iter()
                        .position(|f| f.title == title)
                        .or_else(|| {
                            layout.fences.iter().position(|f| {
                                i18n::all(candidate.category.key()).any(|t| t == f.title)
                            })
                        }) {
                        Some(index) => index,
                        None => {
                            let id = layout
                                .fences
                                .iter()
                                .map(|f| f.id)
                                .max()
                                .unwrap_or(0)
                                .checked_add(1)
                                .ok_or_else(|| err(i18n::t(K::IdsExhausted)))?;
                            let mut group = Layout::default().fences.remove(0);
                            group.id = id;
                            group.title = title.into();
                            group.width = 300;
                            group.height = 380;
                            // Choose a free tile without moving existing groups.
                            let slot = (0..100)
                                .find(|n| {
                                    let x = 24 + (n % 4) * 316;
                                    let y = 24 + (n / 4) * 396;
                                    layout.fences.iter().all(|f| {
                                        x + 300 <= f.x
                                            || x >= f.x + f.width
                                            || y + 380 <= f.y
                                            || y >= f.y + f.window_height()
                                    })
                                })
                                .unwrap_or(layout.fences.len() as i32);
                            group.x = 24 + (slot % 4) * 316;
                            group.y = 24 + (slot / 4) * 396;
                            layout.fences.push(group);
                            layout.fences.len() - 1
                        }
                    };
                    let target_group = &layout.fences[target_index];
                    let replacing = before.as_ref().is_some_and(|l| l.fence == target_group.id);
                    if target_group.entries.len() >= 160 && !replacing {
                        return Err(err(i18n::t(K::GroupFullKeep)));
                    }
                    let mut entry = before.as_ref().map(|l| l.entry.clone()).unwrap_or(Entry {
                        id: unique_id(),
                        path: source.clone(),
                        kind: ItemKind::Reference,
                    });
                    entry.path = source.clone();
                    entry.kind = ItemKind::Reference;
                    let index = before
                        .as_ref()
                        .filter(|l| l.fence == target_group.id)
                        .map_or(target_group.entries.len(), |l| l.index);
                    Ok(Change {
                        action: None,
                        guard: Some(current.identity),
                        after: Some(Location {
                            fence: target_group.id,
                            index,
                            entry,
                        }),
                        before,
                    })
                })();
                match result {
                    Ok(change) => {
                        // Apply to the working layout for capacity/order checks;
                        // the complete metadata-only batch is committed once.
                        apply(&mut layout, std::slice::from_ref(&change), false);
                        record.changes.push(change);
                    }
                    Err(error) => outcome
                        .failures
                        .push(i18n::tf(K::ItemError, &[&source.display(), &error])),
                }
                progress(number + 1, total);
            }
            if !record.changes.is_empty() {
                outcome.completed = record.changes.len();
                transaction(path, &mut layout, record, false)?;
            }
        }
        Command::Retarget { id, path: target } => {
            if !target.exists() {
                return Err(err(i18n::t(K::RenamedMissing)));
            }
            let entry = layout
                .fences
                .iter_mut()
                .flat_map(|f| f.entries.iter_mut())
                .find(|e| e.id == id)
                .ok_or_else(|| err(i18n::t(K::NotInGroup)))?;
            entry.path = target;
            for fence in &mut layout.fences {
                fence.paths = fence.entries.iter().map(|e| e.path.clone()).collect();
            }
            save_state(&mut layout, path)?;
            outcome.completed = 1;
        }
        Command::DeleteFence { fence } => {
            if layout.fences.len() <= 1 {
                return Err(err(i18n::t(K::KeepOneGroup)));
            }
            let ids = layout
                .fences
                .iter()
                .find(|f| f.id == fence)
                .ok_or_else(|| err(i18n::t(K::GroupMissing)))?
                .entries
                .iter()
                .map(|e| e.id.clone())
                .collect();
            outcome = execute_with_progress(path, Command::Desktop { ids }, desktops, progress)?;
            if outcome.failures.is_empty() {
                layout = Layout::load(path)?;
                layout.fences.retain(|f| f.id != fence);
                if outcome.completed > 0 {
                    if let Some(record) = layout.history.last_mut() {
                        record.label = i18n::t(K::LabelDeleteGroup).into();
                    }
                }
                save_state(&mut layout, path)?;
            }
        }
        Command::Transfer { ids, fence, before } => {
            let Some(target) = layout.fences.iter().find(|f| f.id == fence) else {
                return Err(err(i18n::t(K::GroupGone)));
            };
            let selected = locations(&layout, &ids);
            let mut room = 160usize.saturating_sub(target.entries.len());
            let accepted: Vec<_> = selected
                .into_iter()
                .filter(|l| {
                    if l.fence == fence {
                        true
                    } else if room > 0 {
                        room -= 1;
                        true
                    } else {
                        outcome.failures.push(i18n::tf(
                            K::ItemError,
                            &[&l.entry.path.display(), &i18n::t(K::GroupFull)],
                        ));
                        false
                    }
                })
                .collect();
            let accepted_ids: Vec<_> = accepted.iter().map(|l| l.entry.id.clone()).collect();
            if before.as_ref().is_some_and(|id| accepted_ids.contains(id)) {
                return Ok(outcome);
            }
            let remaining: Vec<_> = target
                .entries
                .iter()
                .filter(|e| !accepted_ids.contains(&e.id))
                .collect();
            let insert = before
                .and_then(|id| remaining.iter().position(|e| e.id == id))
                .unwrap_or(remaining.len());
            let mut record = empty_record(i18n::t(K::LabelTransfer));
            record.changes = accepted
                .into_iter()
                .enumerate()
                .map(|(n, old)| Change {
                    after: Some(Location {
                        fence,
                        index: insert + n,
                        entry: old.entry.clone(),
                    }),
                    guard: identity(&old.entry.path).ok(),
                    before: Some(old),
                    action: None,
                })
                .collect();
            if !record.changes.is_empty() {
                let mut preview = layout.clone();
                apply(&mut preview, &record.changes, false);
                if preview
                    .fences
                    .iter()
                    .zip(&layout.fences)
                    .any(|(a, b)| a.entries != b.entries)
                {
                    outcome.completed = record.changes.len();
                    transaction(path, &mut layout, record, false)?;
                }
            }
            progress(outcome.completed, outcome.completed);
        }
        Command::Pin { fence, paths } => {
            let record = empty_record(i18n::t(K::LabelPin));
            let total = paths.len();
            for (number, source) in paths.into_iter().enumerate() {
                let result = (|| -> io::Result<Change> {
                    let f = layout
                        .fences
                        .iter()
                        .find(|f| f.id == fence)
                        .ok_or_else(|| err(i18n::t(K::GroupMissing)))?;
                    if f.entries.iter().any(|e| same_path(&e.path, &source)) {
                        return Err(err(i18n::t(K::AlreadyInGroup)));
                    }
                    if f.entries.len() >= 160 {
                        return Err(err(i18n::t(K::GroupFull)));
                    }
                    Ok(Change {
                        after: Some(Location {
                            fence,
                            index: f.entries.len(),
                            entry: Entry {
                                id: unique_id(),
                                path: source.clone(),
                                kind: ItemKind::Reference,
                            },
                        }),
                        before: None,
                        action: None,
                        guard: Some(identity(&source)?),
                    })
                })();
                match result {
                    Ok(change) => {
                        let mut one = record.clone();
                        one.changes.push(change);
                        match transaction(path, &mut layout, one, false) {
                            Ok(()) => outcome.completed += 1,
                            Err(e) => outcome
                                .failures
                                .push(i18n::tf(K::ItemError, &[&source.display(), &e])),
                        }
                    }
                    Err(e) => outcome
                        .failures
                        .push(i18n::tf(K::ItemError, &[&source.display(), &e])),
                }
                progress(number + 1, total);
            }
        }
        Command::Desktop { ids } => {
            // Ungrouping drops references only; the files stay where they are.
            let selected = locations(&layout, &ids);
            if !selected.is_empty() {
                let mut record = empty_record(i18n::t(K::LabelUngroup));
                record.changes = selected
                    .into_iter()
                    .map(|old| Change {
                        guard: identity(&old.entry.path).ok(),
                        before: Some(old),
                        after: None,
                        action: None,
                    })
                    .collect();
                outcome.completed = record.changes.len();
                transaction(path, &mut layout, record, false)?;
            }
            progress(outcome.completed, outcome.completed);
        }
        Command::Undo => {
            let Some(record) = layout.history.last().cloned() else {
                return Ok(outcome);
            };
            // Validate the entire operation before changing anything.
            for change in &record.changes {
                let location = change.after.as_ref().or(change.before.as_ref());
                let name = location
                    .map(|l| l.entry.path.display().to_string())
                    .unwrap_or_default();
                if change.action.is_some() {
                    return Err(err(&i18n::tf(
                        K::ItemError,
                        &[&name, &i18n::t(K::LegacyUndo)],
                    )));
                }
                if let (Some(guard), Some(location)) = (&change.guard, location) {
                    if !matches(&location.entry.path, guard) {
                        return Err(err(&i18n::tf(
                            K::ItemError,
                            &[&name, &i18n::t(K::ItemReplaced)],
                        )));
                    }
                }
            }
            // One atomic change, including whole-batch ordering.
            outcome.completed = record.changes.len();
            transaction(path, &mut layout, record, true)?;
            progress(outcome.completed, outcome.completed);
        }
    }
    Ok(outcome)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::windows::ffi::OsStrExt;
    use windows::core::{Interface, PCWSTR};
    use windows::Win32::System::Com::{CoCreateInstance, IPersistFile, CLSCTX_INPROC_SERVER};
    use windows::Win32::UI::Shell::{IShellLinkW, ShellLink};

    fn make_link(link_path: &Path, target: &Path) {
        let wide = |p: &Path| {
            p.as_os_str()
                .encode_wide()
                .chain(Some(0))
                .collect::<Vec<_>>()
        };
        unsafe {
            let link: IShellLinkW =
                CoCreateInstance(&ShellLink, None, CLSCTX_INPROC_SERVER).unwrap();
            link.SetPath(PCWSTR(wide(target).as_ptr())).unwrap();
            let file: IPersistFile = link.cast().unwrap();
            file.Save(PCWSTR(wide(link_path).as_ptr()), true).unwrap();
        }
    }

    struct Fixture {
        dir: PathBuf,
        path: PathBuf,
        desktop: PathBuf,
    }
    impl Fixture {
        fn new() -> Self {
            let dir = std::env::temp_dir().join(format!("luma-v4-{}", unique_id()));
            let desktop = dir.join("Desktop");
            fs::create_dir_all(&desktop).unwrap();
            let path = dir.join("data/layout.json");
            Layout::default().save(&path).unwrap();
            initialize(&path).unwrap();
            Self { dir, path, desktop }
        }
        fn file(&self, name: &str) -> PathBuf {
            let path = self.desktop.join(name);
            fs::write(&path, format!("content:{name}")).unwrap();
            path
        }
        fn run(&self, cmd: Command) -> Outcome {
            execute_with_progress(
                &self.path,
                cmd,
                std::slice::from_ref(&self.desktop),
                &mut |_, _| {},
            )
            .unwrap()
        }
        fn layout(&self) -> Layout {
            Layout::load(&self.path).unwrap()
        }
        fn ids(&self, fence: u64) -> Vec<String> {
            self.layout()
                .fences
                .iter()
                .find(|f| f.id == fence)
                .unwrap()
                .entries
                .iter()
                .map(|e| e.id.clone())
                .collect()
        }
        fn pin(&self, paths: Vec<PathBuf>) {
            assert!(self
                .run(Command::Pin { fence: 1, paths })
                .failures
                .is_empty());
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.dir);
        }
    }

    #[test]
    fn pin_and_ungroup_only_edit_references() {
        let f = Fixture::new();
        let source = f.file("原檔.txt");
        let expected = identity(&source).unwrap();
        let run = |cmd| execute(&f.path, cmd, std::slice::from_ref(&f.desktop), |_, _| {}).unwrap();
        assert_eq!(
            run(Command::Pin {
                fence: 1,
                paths: vec![source.clone()],
            })
            .completed,
            1
        );
        let ids = f.ids(1);
        assert_eq!(f.layout().fences[0].entries[0].kind, ItemKind::Reference);
        assert_eq!(run(Command::Desktop { ids: ids.clone() }).completed, 1);
        assert_eq!(identity(&source).unwrap(), expected);
        assert_eq!(fs::read(&source).unwrap(), "content:原檔.txt".as_bytes());
        assert!(!f.desktop.join("原檔.txt.lnk").exists());
        assert!(!root(&f.path).join("collections").exists());
        run(Command::Undo);
        assert_eq!(f.ids(1), ids);
        assert!(f
            .layout()
            .history
            .iter()
            .flat_map(|r| &r.changes)
            .all(|c| c.action.is_none()));
        assert_eq!(identity(&source).unwrap(), expected);
    }

    #[test]
    fn extended_windows_paths_reuse_reference_ids_during_automatic_grouping() {
        let f = Fixture::new();
        let source = f.file("前綴.txt");
        let canonical = source.canonicalize().unwrap();
        let run = |cmd| execute(&f.path, cmd, std::slice::from_ref(&f.desktop), |_, _| {}).unwrap();
        run(Command::Pin {
            fence: 1,
            paths: vec![source.clone()],
        });
        let original_id = f.ids(1)[0].clone();
        assert_eq!(
            run(Command::Pin {
                fence: 1,
                paths: vec![canonical.clone()],
            })
            .completed,
            0
        );
        let candidate = crate::organize::inspect(&canonical).unwrap();
        let outcome = run(Command::AutoOrganize {
            plan: crate::organize::Plan {
                items: vec![candidate],
                skipped: vec![],
            },
        });
        assert!(outcome.failures.is_empty());
        let entries: Vec<_> = f
            .layout()
            .fences
            .into_iter()
            .flat_map(|g| g.entries)
            .collect();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].id, original_id);
        assert!(source.exists());
        run(Command::Undo);
        assert_eq!(f.ids(1), vec![original_id]);
        assert_eq!(f.layout().fences[0].entries[0].path, source);
    }

    #[test]
    fn grouping_a_busy_directory_needs_no_delete_access_and_undo_keeps_contents() {
        let f = Fixture::new();
        let folder = f.desktop.join("使用中資料夾");
        fs::create_dir(&folder).unwrap();
        fs::write(folder.join("unchanged.txt"), b"keep").unwrap();
        let lock = OpenOptions::new()
            .access_mode(0x80)
            .share_mode(1)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS.0)
            .open(&folder)
            .unwrap();
        let expected = identity(&folder).unwrap();
        let run = |cmd| execute(&f.path, cmd, std::slice::from_ref(&f.desktop), |_, _| {}).unwrap();
        let plan = run(Command::PreviewAuto).preview.unwrap();
        let outcome = run(Command::AutoOrganize { plan });
        assert_eq!(outcome.completed, 1);
        assert!(outcome.failures.is_empty());
        let group = f
            .layout()
            .fences
            .iter()
            .find(|g| g.title == "資料夾")
            .unwrap()
            .id;
        assert_eq!(identity(&folder).unwrap(), expected);
        run(Command::DeleteFence { fence: group });
        assert_eq!(fs::read(folder.join("unchanged.txt")).unwrap(), b"keep");
        run(Command::Undo);
        assert_eq!(identity(&folder).unwrap(), expected);
        drop(lock);
    }

    #[test]
    fn public_shortcuts_merge_into_programs_without_moves_and_undo_preserves_ids() {
        let f = Fixture::new();
        let personal = f.file("App.lnk");
        let public = f.dir.join("Public");
        fs::create_dir(&public).unwrap();
        let shared = public.join("App.lnk");
        fs::write(&shared, b"shared shortcut target unchanged").unwrap();
        let outside = f.dir.join("outside.lnk");
        fs::write(&outside, b"outside").unwrap();
        f.pin(vec![personal.clone()]);
        let original_id = f.ids(1)[0].clone();
        let roots = vec![f.desktop.clone(), public.clone(), public];
        let run = |cmd| execute(&f.path, cmd, &roots, |_, _| {}).unwrap();
        let mut plan = run(Command::PreviewAuto).preview.unwrap();
        assert_eq!(
            plan.items.len(),
            2,
            "duplicate roots must not duplicate items"
        );
        assert!(plan
            .items
            .iter()
            .all(|i| i.category == crate::organize::Category::Programs));
        plan.items.push(crate::organize::inspect(&outside).unwrap());
        let result = run(Command::AutoOrganize { plan });
        assert_eq!(result.completed, 2);
        assert_eq!(result.failures.len(), 1);
        let layout = f.layout();
        let programs = layout.fences.iter().find(|g| g.title == "程式").unwrap();
        assert_eq!(programs.entries.len(), 2);
        assert!(programs.entries.iter().any(|e| e.id == original_id));
        assert!(layout
            .history
            .last()
            .unwrap()
            .changes
            .iter()
            .all(|c| c.action.is_none()));
        assert_eq!(
            fs::read(&shared).unwrap(),
            b"shared shortcut target unchanged"
        );
        assert_eq!(run(Command::Undo).completed, 2);
        assert_eq!(f.ids(1), vec![original_id]);
        assert!(personal.exists() && shared.exists());
        assert!(!root(&f.path).join("collections").exists());
    }

    #[test]
    fn auto_preview_includes_public_desktop_and_skips_hidden_items() {
        use windows::Win32::Storage::FileSystem::{SetFileAttributesW, FILE_ATTRIBUTE_HIDDEN};
        let f = Fixture::new();
        f.file("中文.TXT");
        f.file("photo.PNG");
        f.file("play.mp4");
        f.file("archive.zip");
        f.file("website.url");
        f.file("run.exe");
        f.file("run.ps1");
        f.file("desktop.ini");
        f.file("page.html");
        fs::create_dir(f.desktop.join("page_files")).unwrap();
        fs::create_dir(f.desktop.join("project")).unwrap();
        let hidden = f.file("hidden.txt");
        let wide: Vec<_> = hidden.as_os_str().encode_wide().chain(Some(0)).collect();
        unsafe {
            SetFileAttributesW(PCWSTR(wide.as_ptr()), FILE_ATTRIBUTE_HIDDEN).unwrap();
        }
        let public = f.dir.join("Public");
        fs::create_dir(&public).unwrap();
        fs::write(public.join("public.txt"), b"shared").unwrap();
        let before = fs::read(&f.path).unwrap();
        let outcome = execute(
            &f.path,
            Command::PreviewAuto,
            &[f.desktop.clone(), public],
            |_, _| {},
        )
        .unwrap();
        let plan = outcome.preview.unwrap();
        assert_eq!(plan.items.len(), 11);
        assert_eq!(plan.skipped.len(), 2);
        assert_eq!(fs::read(&f.path).unwrap(), before);
        assert!(!f.path.parent().unwrap().join("collections").exists());
        assert!(plan.preview_text().contains("中文.TXT"));
        assert!(plan.items.iter().all(|i| i.path.exists()));
    }

    #[test]
    fn auto_folders_programs_scripts_keep_contents_siblings_and_restart_undo() {
        let f = Fixture::new();
        let program = f.file("工具.EXE");
        let library = f.file("helper.dll");
        let script = f.file("start.ps1");
        let folder = f.desktop.join("中文資料夾");
        fs::create_dir_all(folder.join("nested")).unwrap();
        fs::write(folder.join("nested/content.txt"), b"nested contents").unwrap();
        let original_folder = identity(&folder).unwrap();
        let plan = f.run(Command::PreviewAuto).preview.unwrap();
        assert_eq!(plan.items.len(), 4);
        let outcome = f.run(Command::AutoOrganize { plan });
        assert_eq!(outcome.completed, 4);
        assert!(outcome.failures.is_empty(), "{:?}", outcome.failures);
        let layout = initialize(&f.path).unwrap();
        let stored: Vec<_> = layout.fences.iter().flat_map(|f| f.paths.iter()).collect();
        assert_eq!(stored.len(), 4);
        let directory = stored[0].parent().unwrap();
        assert!(stored.iter().all(|p| p.parent() == Some(directory)));
        assert_eq!(
            fs::read(directory.join("工具.EXE")).unwrap(),
            b"content:\xe5\xb7\xa5\xe5\x85\xb7.EXE"
        );
        assert_eq!(
            fs::read(directory.join("helper.dll")).unwrap(),
            b"content:helper.dll"
        );
        assert_eq!(
            fs::read(directory.join("start.ps1")).unwrap(),
            b"content:start.ps1"
        );
        assert_eq!(
            fs::read(directory.join("中文資料夾/nested/content.txt")).unwrap(),
            b"nested contents"
        );
        assert_eq!(
            identity(&directory.join("中文資料夾")).unwrap(),
            original_folder
        );
        assert!([&program, &library, &script, &folder]
            .iter()
            .all(|p| p.exists()));
        let result = f.run(Command::Undo);
        assert_eq!(result.completed, 4);
        assert!([&program, &library, &script, &folder]
            .iter()
            .all(|p| p.exists()));
        assert_eq!(identity(&folder).unwrap(), original_folder);
        assert_eq!(
            fs::read(folder.join("nested/content.txt")).unwrap(),
            b"nested contents"
        );
        assert!(f.layout().history.is_empty());
    }

    #[test]
    fn auto_rejects_storage_and_running_app_ancestors() {
        let f = Fixture::new();
        // A user may relocate APPDATA into a desktop folder. Never move the
        // journal or collected files into themselves, even with a stale preview.
        let folder = f.desktop.join("appdata");
        fs::create_dir(&folder).unwrap();
        let nested_path = folder.join("layout.json");
        Layout::default().save(&nested_path).unwrap();
        let plan = execute(
            &nested_path,
            Command::PreviewAuto,
            std::slice::from_ref(&f.desktop),
            |_, _| {},
        )
        .unwrap()
        .preview
        .unwrap();
        assert!(plan.items.is_empty());
        assert_eq!(plan.skipped.len(), 1);
        let stale = crate::organize::Plan {
            items: vec![crate::organize::inspect(&folder).unwrap()],
            skipped: vec![],
        };
        let result = execute(
            &nested_path,
            Command::AutoOrganize { plan: stale },
            std::slice::from_ref(&f.desktop),
            |_, _| {},
        )
        .unwrap();
        assert_eq!(result.completed, 0);
        assert_eq!(result.failures.len(), 1);
        assert!(nested_path.exists());
        let exe = std::env::current_exe().unwrap();
        assert!(crate::organize::inspect(exe.parent().unwrap()).is_ok());
        assert!(crate::organize::inspect(&std::env::current_dir().unwrap()).is_ok());
    }

    #[test]
    fn auto_collects_all_categories_once_and_restart_undo_restores_references_and_links() {
        use windows::Win32::System::Ole::{OleInitialize, OleUninitialize};
        unsafe {
            OleInitialize(None).unwrap();
        }
        let f = Fixture::new();
        let names = ["中文.txt", "photo.png", "play.mp4", "archive.zip"];
        let mut sources: Vec<_> = names.iter().map(|n| f.file(n)).collect();
        let external = f.dir.join("external.exe");
        fs::write(&external, b"target stays here").unwrap();
        let shortcut = f.desktop.join("App.lnk");
        make_link(&shortcut, &external);
        sources.push(shortcut.clone());
        let contents: Vec<_> = sources.iter().map(|p| fs::read(p).unwrap()).collect();
        f.pin(vec![sources[0].clone()]);
        let reference_id = f.ids(1)[0].clone();
        let plan = f.run(Command::PreviewAuto).preview.unwrap();
        let result = f.run(Command::AutoOrganize { plan });
        assert_eq!(result.completed, 5);
        assert!(result.failures.is_empty(), "{:?}", result.failures);
        assert!(sources.iter().all(|p| p.exists()));
        let layout = initialize(&f.path).unwrap();
        assert_eq!(layout.history.len(), 2);
        assert_eq!(layout.history.last().unwrap().label, "自動分組桌面圖示");
        assert_eq!(layout.history.last().unwrap().changes.len(), 5);
        assert!(layout.fences[0].entries.is_empty());
        for group in layout.fences.iter().skip(1) {
            assert_eq!(group.entries.len(), 1);
            assert_eq!(group.entries[0].kind, ItemKind::Reference);
            let source = sources
                .iter()
                .position(|p| p.file_name() == group.paths[0].file_name())
                .unwrap();
            assert_eq!(fs::read(&group.paths[0]).unwrap(), contents[source]);
            if group.title == "文件" {
                assert_eq!(group.entries[0].id, reference_id);
            }
            if group.title == "程式" {
                unsafe {
                    let link: IShellLinkW =
                        CoCreateInstance(&ShellLink, None, CLSCTX_INPROC_SERVER).unwrap();
                    let persist: IPersistFile = link.cast().unwrap();
                    let wide: Vec<_> = group.paths[0]
                        .as_os_str()
                        .encode_wide()
                        .chain(Some(0))
                        .collect();
                    persist
                        .Load(
                            PCWSTR(wide.as_ptr()),
                            windows::Win32::System::Com::STGM_READ,
                        )
                        .unwrap();
                    let mut buffer = [0u16; 1024];
                    link.GetPath(&mut buffer, std::ptr::null_mut(), 0).unwrap();
                    let n = buffer.iter().position(|c| *c == 0).unwrap();
                    assert_eq!(
                        PathBuf::from(String::from_utf16_lossy(&buffer[..n]))
                            .canonicalize()
                            .unwrap(),
                        external.canonicalize().unwrap()
                    );
                }
            }
        }
        assert_eq!(f.run(Command::PreviewAuto).preview.unwrap().items.len(), 5);
        assert_eq!(f.run(Command::Undo).completed, 5);
        assert_eq!(f.ids(1), vec![reference_id]);
        assert_eq!(f.layout().fences.len(), 6); // Empty classification groups remain reusable.
        assert_eq!(f.layout().history.len(), 1);
        for (path, expected) in sources.iter().zip(contents) {
            assert_eq!(fs::read(path).unwrap(), expected);
        }
        assert_eq!(fs::read(external).unwrap(), b"target stays here");
        unsafe {
            OleUninitialize();
        }
    }

    #[test]
    fn auto_revalidates_preview_replacements_public_paths_locks_and_capacity() {
        let f = Fixture::new();
        let replaced = f.file("a.txt");
        let locked = f.file("b.txt");
        let accepted = f.file("c.txt");
        let overflow = f.file("d.txt");
        let public = f.dir.join("Public");
        fs::create_dir(&public).unwrap();
        let shared = public.join("shared.txt");
        fs::write(&shared, b"shared").unwrap();
        let mut plan = f.run(Command::PreviewAuto).preview.unwrap();
        fs::rename(&replaced, f.dir.join("original.txt")).unwrap();
        fs::write(&replaced, b"replacement").unwrap();
        plan.items.push(crate::organize::inspect(&shared).unwrap());
        let lock = OpenOptions::new()
            .read(true)
            .share_mode(0)
            .open(&locked)
            .unwrap();
        let mut layout = f.layout();
        layout.fences[0].title = "文件".into();
        layout.fences[0].set_entries(
            (0..159)
                .map(|_| Entry {
                    id: unique_id(),
                    path: shared.clone(),
                    kind: ItemKind::Reference,
                })
                .collect(),
        );
        layout.save(&f.path).unwrap();
        let result = execute(
            &f.path,
            Command::AutoOrganize { plan },
            &[f.desktop.clone(), public],
            |_, _| {},
        )
        .unwrap();
        drop(lock);
        assert_eq!(result.completed, 1);
        assert_eq!(result.failures.len(), 4, "{:?}", result.failures);
        assert_eq!(fs::read(&replaced).unwrap(), b"replacement");
        assert!(locked.exists());
        assert!(accepted.exists());
        assert!(overflow.exists());
        assert!(shared.exists());
        assert_eq!(f.layout().fences[0].entries.len(), 160);
        assert_eq!(f.layout().history.last().unwrap().changes.len(), 1);
        assert_eq!(f.run(Command::Undo).completed, 1);
        assert!(accepted.exists());
    }

    #[test]
    fn auto_duplicate_references_are_reported_without_dangling_links() {
        let f = Fixture::new();
        let source = f.file("duplicate.txt");
        f.pin(vec![source.clone()]);
        let mut layout = f.layout();
        let second = layout.add_fence();
        layout.save(&f.path).unwrap();
        f.run(Command::Pin {
            fence: second.id,
            paths: vec![source.clone()],
        });
        let plan = f.run(Command::PreviewAuto).preview.unwrap();
        let result = f.run(Command::AutoOrganize { plan });
        assert_eq!(result.completed, 0);
        assert_eq!(result.failures.len(), 1);
        assert!(source.exists());
        assert_eq!(
            f.layout()
                .fences
                .iter()
                .map(|f| f.entries.len())
                .sum::<usize>(),
            2
        );
    }

    #[test]
    fn category_group_titled_in_another_language_is_reused_not_renamed() {
        let f = Fixture::new();
        let photo = f.file("photo.png");
        let mut layout = f.layout();
        layout.fences[0].title = "Pictures".into();
        layout.save(&f.path).unwrap();
        let plan = f.run(Command::PreviewAuto).preview.unwrap();
        assert!(f.run(Command::AutoOrganize { plan }).failures.is_empty());
        let layout = f.layout();
        assert_eq!(layout.fences.len(), 1);
        assert_eq!(layout.fences[0].title, "Pictures");
        assert_eq!(layout.fences[0].paths, vec![photo]);
    }

    #[test]
    fn geometry_save_keeps_category_groups_created_by_worker() {
        let f = Fixture::new();
        let mut visual = f.layout();
        f.file("document.txt");
        let plan = f.run(Command::PreviewAuto).preview.unwrap();
        f.run(Command::AutoOrganize { plan });
        let saved = f.layout();
        visual.fences[0].x = 42;
        save_visual(&visual, &f.path).unwrap();
        let merged = f.layout();
        assert_eq!(merged.fences[0].x, 42);
        assert_eq!(merged.fences[1].entries, saved.fences[1].entries);
        assert_eq!(merged.history.len(), 1);
        assert!(merged.fences[1].paths[0].exists());
    }

    #[test]
    fn renamed_file_keeps_its_reference_id_group_and_position() {
        let f = Fixture::new();
        let first = f.file("first.txt");
        let second = f.file("second.txt");
        f.pin(vec![first.clone(), second.clone()]);
        let ids = f.ids(1);
        let renamed = f.desktop.join("renamed.txt");
        fs::rename(&first, &renamed).unwrap();
        let result = f.run(Command::Retarget {
            id: ids[0].clone(),
            path: renamed.clone(),
        });
        assert_eq!(result.completed, 1);
        let layout = f.layout();
        assert_eq!(f.ids(1), ids);
        assert_eq!(layout.fences[0].paths, vec![renamed, second]);
        // A missing target is refused and changes nothing.
        let missing = execute_with_progress(
            &f.path,
            Command::Retarget {
                id: ids[1].clone(),
                path: f.desktop.join("nope.txt"),
            },
            std::slice::from_ref(&f.desktop),
            &mut |_, _| {},
        );
        assert!(missing.is_err());
        assert_eq!(f.layout().fences[0].paths, layout.fences[0].paths);
    }

    #[test]
    fn delete_group_keeps_files_and_undo_restores_group_metadata() {
        let f = Fixture::new();
        let source = f.file("delete.txt");
        f.pin(vec![source.clone()]);
        let mut layout = f.layout();
        layout.fences[0].title = "需要保留的名稱".into();
        layout.add_fence();
        layout.save(&f.path).unwrap();
        let ids = f.ids(1);
        let result = f.run(Command::DeleteFence { fence: 1 });
        assert_eq!(result.completed, 1);
        assert!(result.failures.is_empty());
        assert!(source.exists());
        assert_eq!(f.layout().fences.len(), 1);
        f.run(Command::Undo);
        let loaded = f.layout();
        let restored = loaded.fences.iter().find(|g| g.id == 1).unwrap();
        assert_eq!(restored.title, "需要保留的名稱");
        assert_eq!(f.ids(1), ids);
        assert!(source.exists());
    }

    #[test]
    fn batch_pin_transfer_restart_and_continuous_undo() {
        let f = Fixture::new();
        let files = vec![f.file("中文.txt"), f.file("second.txt")];
        let nested = f.desktop.join("巢狀資料夾");
        fs::create_dir_all(nested.join("nested")).unwrap();
        fs::write(nested.join("nested/data"), b"nested").unwrap();
        let mut sources = files.clone();
        sources.push(nested.clone());
        f.pin(sources.clone());
        let initial = f.layout();
        assert_eq!(initial.history.len(), 1);
        assert_eq!(initial.history[0].changes.len(), 3);
        let stored = initial.fences[0].paths.clone();
        assert_eq!(stored, sources);
        let ids = f.ids(1);
        let mut layout = f.layout();
        let other = layout.add_fence().id;
        layout.save(&f.path).unwrap();
        assert_eq!(
            f.run(Command::Transfer {
                ids: ids.clone(),
                fence: other,
                before: None
            })
            .completed,
            3
        );
        let loaded = initialize(&f.path).unwrap();
        assert!(loaded.fences[0].entries.is_empty());
        assert_eq!(loaded.fences[1].paths, stored);
        assert_eq!(f.run(Command::Desktop { ids: ids.clone() }).completed, 3);
        for file in &sources {
            assert!(file.exists());
        }
        initialize(&f.path).unwrap();
        f.run(Command::Undo);
        assert_eq!(f.ids(other), ids);
        f.run(Command::Undo);
        assert_eq!(f.ids(1), ids);
        f.run(Command::Undo);
        assert!(f.ids(1).is_empty());
        assert!(f.layout().history.is_empty());
        assert_eq!(fs::read(nested.join("nested/data")).unwrap(), b"nested");
        assert_eq!(fs::read_to_string(&files[0]).unwrap(), "content:中文.txt");
    }

    #[test]
    fn batch_insert_before_anchor_and_tail_undo_restores_exact_order() {
        let f = Fixture::new();
        f.pin((0..6).map(|n| f.file(&format!("{n}.txt"))).collect());
        let ids = f.ids(1);
        let picked = vec![ids[1].clone(), ids[3].clone()];
        f.run(Command::Transfer {
            ids: picked.clone(),
            fence: 1,
            before: Some(ids[0].clone()),
        });
        assert_eq!(
            f.ids(1),
            vec![
                ids[1].clone(),
                ids[3].clone(),
                ids[0].clone(),
                ids[2].clone(),
                ids[4].clone(),
                ids[5].clone()
            ]
        );
        f.run(Command::Undo);
        assert_eq!(f.ids(1), ids);
        f.run(Command::Transfer {
            ids: picked,
            fence: 1,
            before: None,
        });
        assert_eq!(
            f.ids(1),
            vec![
                ids[0].clone(),
                ids[2].clone(),
                ids[4].clone(),
                ids[5].clone(),
                ids[1].clone(),
                ids[3].clone()
            ]
        );
        f.run(Command::Undo);
        assert_eq!(f.ids(1), ids);
    }

    #[test]
    fn locked_files_can_be_referenced_and_capacity_is_partial() {
        let f = Fixture::new();
        let first = f.file("locked.txt");
        let second = f.file("good.txt");
        let lock = OpenOptions::new()
            .read(true)
            .share_mode(0)
            .open(&first)
            .unwrap();
        let result = f.run(Command::Pin {
            fence: 1,
            paths: vec![first.clone(), second.clone()],
        });
        // Identity needs only FILE_READ_ATTRIBUTES, which an exclusive lock allows.
        assert_eq!(result.completed, 2);
        assert!(result.failures.is_empty());
        drop(lock);
        let mut layout = f.layout();
        let other = layout.add_fence().id;
        let filler = f.file("filler.txt");
        let target = layout.fences.last_mut().unwrap();
        target.set_entries(
            (0..159)
                .map(|_| Entry {
                    id: unique_id(),
                    path: filler.clone(),
                    kind: ItemKind::Reference,
                })
                .collect(),
        );
        layout.save(&f.path).unwrap();
        let ids = f.ids(1);
        let result = f.run(Command::Transfer {
            ids: ids.clone(),
            fence: other,
            before: None,
        });
        assert_eq!(result.completed, 1);
        assert_eq!(result.failures.len(), 1);
        assert_eq!(f.ids(1).len(), 1);
        assert_eq!(f.ids(other).len(), 160);
        let history = f.layout().history.last().unwrap().changes.len();
        assert_eq!(history, 1);
        f.run(Command::Undo);
        assert_eq!(f.ids(1), ids);
    }

    #[test]
    fn only_twenty_batches_survive_restart_and_expiration_keeps_files() {
        let f = Fixture::new();
        let source = f.file("kept.txt");
        f.pin(vec![source]);
        let stored = f.layout().fences[0].paths[0].clone();
        for n in 0..24 {
            f.pin(vec![f.file(&format!("ref{n}.txt"))]);
        }
        assert_eq!(f.layout().history.len(), 20);
        assert!(stored.exists());
        initialize(&f.path).unwrap();
        for _ in 0..20 {
            f.run(Command::Undo);
        }
        assert!(f.layout().history.is_empty());
        assert!(stored.exists());
        assert_eq!(f.ids(1).len(), 5);
    }

    #[test]
    fn metadata_write_during_worker_commit_preserves_both_sides() {
        let f = Fixture::new();
        let stale = f.layout();
        f.pin(vec![f.file("inflight.txt")]);
        let mut visual = stale;
        visual.fences[0].x = -500;
        visual.fences[0].title = "new title".into();
        save_visual(&visual, &f.path).unwrap();
        let loaded = f.layout();
        assert_eq!(loaded.fences[0].x, -500);
        assert_eq!(loaded.fences[0].entries.len(), 1);
        assert_eq!(loaded.history.len(), 1);
        assert_eq!(loaded.fences[0].title, "new title");
    }

    fn legacy_move(source: &Path, target: &Path) -> serde_json::Value {
        serde_json::json!({"Move": {"source": source, "target": target,
            "identity": {"volume": 1, "high": 2, "low": 3, "created": 4}}})
    }

    #[test]
    fn legacy_managed_entries_load_as_references_and_move_history_is_dropped() {
        let f = Fixture::new();
        let stored = root(&f.path).join(r"collections\v2\1-1-0\files\old.txt");
        fs::create_dir_all(stored.parent().unwrap()).unwrap();
        fs::write(&stored, b"collected by 0.5").unwrap();
        let kept = f.file("kept.txt");
        f.pin(vec![kept.clone()]);
        let mut layout = f.layout();
        let entry = Entry {
            id: unique_id(),
            path: stored.clone(),
            kind: ItemKind::Reference,
        };
        let mut entries = layout.fences[0].entries.clone();
        entries.push(entry.clone());
        layout.fences[0].set_entries(entries);
        let mut moved = empty_record("收納桌面項目");
        moved.changes.push(Change {
            before: None,
            after: Some(Location {
                fence: 1,
                index: 1,
                entry: entry.clone(),
            }),
            action: Some(legacy_move(&f.desktop.join("old.txt"), &stored)),
            guard: None,
        });
        layout.history.push(moved);
        layout.version = 2;
        // Mark the collected item the way releases before 0.6 stored it.
        let mut json = serde_json::to_value(&layout).unwrap();
        json["fences"][0]["entries"][1]["kind"] = "managed".into();
        fs::write(&f.path, json.to_string()).unwrap();

        let migrated = initialize(&f.path).unwrap();
        assert_eq!(migrated.version, 3);
        assert_eq!(migrated.fences[0].paths, vec![kept.clone(), stored.clone()]);
        assert!(migrated.fences[0]
            .entries
            .iter()
            .all(|e| e.kind == ItemKind::Reference));
        assert_eq!(migrated.fences[0].entries[1].id, entry.id);
        assert_eq!(
            migrated.history.len(),
            1,
            "only the reference batch survives"
        );
        assert!(root(&f.path)
            .join("before-virtual-groups/layout.json")
            .is_file());
        assert_eq!(fs::read(&stored).unwrap(), b"collected by 0.5");
        assert!(!f.desktop.join("old.txt").exists());
        // Undo reaches the remaining reference batch; nothing is moved.
        assert_eq!(f.run(Command::Undo).completed, 1);
        assert_eq!(f.layout().fences[0].paths, vec![stored.clone()]);
        assert!(kept.exists() && stored.exists());
    }

    #[test]
    fn legacy_pending_move_is_refused_and_kept_for_inspection() {
        let f = Fixture::new();
        let source = f.file("interrupted.txt");
        let mut layout = f.layout();
        let mut record = empty_record("收納桌面項目");
        record.changes.push(Change {
            before: None,
            after: Some(Location {
                fence: 1,
                index: 0,
                entry: Entry {
                    id: unique_id(),
                    path: root(&f.path).join("collections/x/files/interrupted.txt"),
                    kind: ItemKind::Reference,
                },
            }),
            action: Some(legacy_move(
                &source,
                Path::new(r"C:\nowhere\interrupted.txt"),
            )),
            guard: None,
        });
        layout.pending = Some(Pending {
            record,
            undo: false,
        });
        layout.save(&f.path).unwrap();
        let before = fs::read(&f.path).unwrap();
        let error = initialize(&f.path).unwrap_err().to_string();
        assert!(error.contains("pending"), "{error}");
        assert_eq!(fs::read(&f.path).unwrap(), before);
        assert!(source.exists());
    }

    #[test]
    fn version_one_layout_is_backed_up_and_mapped_to_collected_copies() {
        let f = Fixture::new();
        let original = f.desktop.join("legacy.txt");
        let directory = root(&f.path).join(r"collections\1\1-1-0");
        fs::create_dir_all(directory.join("files")).unwrap();
        let stored = directory.join("files/legacy.txt");
        fs::write(&stored, b"old").unwrap();
        let receipt = serde_json::json!({"version": 1, "fence_id": 1,
            "original": original, "name": "legacy.txt"});
        fs::write(directory.join("receipt.json"), receipt.to_string()).unwrap();
        let mut legacy = Layout {
            version: 1,
            ..Layout::default()
        };
        legacy.fences[0].paths = vec![original.clone()];
        legacy.save(&f.path).unwrap();
        let migrated = initialize(&f.path).unwrap();
        assert_eq!(migrated.version, 3);
        assert_eq!(migrated.fences[0].paths, vec![stored.clone()]);
        let backup = fs::read_dir(root(&f.path))
            .unwrap()
            .map(|e| e.unwrap().path())
            .find(|p| {
                p.file_name()
                    .unwrap()
                    .to_string_lossy()
                    .starts_with("backup-v1-")
            })
            .unwrap();
        assert!(backup.join("BACKUP_COMPLETE").exists());
        assert!(backup.join(r"collections\1\1-1-0\receipt.json").exists());
        assert_eq!(
            Layout::load(&backup.join("layout.json")).unwrap().version,
            1
        );
        assert_eq!(fs::read(&stored).unwrap(), b"old");
        assert!(!original.exists());
        let again = initialize(&f.path).unwrap();
        assert_eq!(again.fences[0].entries, migrated.fences[0].entries);
    }
}
