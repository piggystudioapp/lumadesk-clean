//! Offline, non-recursive desktop classification. Scanning never moves files.
use crate::i18n::{self, K};
use crate::operations::{identity, Identity};
use std::fs;
use std::io;
use std::os::windows::fs::MetadataExt;
use std::path::{Path, PathBuf};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Category {
    Shortcuts,
    Documents,
    Pictures,
    Media,
    Archives,
    Folders,
    Programs,
    Scripts,
    /// Ordinary files of any other type, including extensionless dotfiles.
    Other,
}

impl Category {
    pub const ALL: [Self; 9] = [
        Self::Shortcuts,
        Self::Documents,
        Self::Pictures,
        Self::Media,
        Self::Archives,
        Self::Folders,
        Self::Programs,
        Self::Scripts,
        Self::Other,
    ];

    /// Category group titles exist in every language; see `title`.
    pub fn key(self) -> K {
        match self {
            Self::Shortcuts => K::CatShortcuts,
            Self::Documents => K::CatDocuments,
            Self::Pictures => K::CatPictures,
            Self::Media => K::CatMedia,
            Self::Archives => K::CatArchives,
            Self::Folders => K::CatFolders,
            Self::Programs => K::CatPrograms,
            Self::Scripts => K::CatScripts,
            Self::Other => K::CatOther,
        }
    }

    pub fn title(self) -> &'static str {
        i18n::t(self.key())
    }
}

fn classify(path: &Path) -> Option<Category> {
    let ext = path.extension()?.to_str()?.to_ascii_lowercase();
    Some(match ext.as_str() {
        "url" => Category::Shortcuts,
        "lnk" | "appref-ms" => Category::Programs,
        "txt" | "md" | "pdf" | "doc" | "docx" | "odt" | "rtf" | "xls" | "xlsx" | "ods" | "csv"
        | "ppt" | "pptx" | "odp" | "html" | "htm" | "epub" => Category::Documents,
        "png" | "jpg" | "jpeg" | "gif" | "webp" | "bmp" | "tif" | "tiff" | "svg" | "heic"
        | "avif" | "ico" | "psd" => Category::Pictures,
        "mp4" | "mkv" | "avi" | "mov" | "wmv" | "webm" | "m4v" | "mp3" | "wav" | "flac" | "aac"
        | "m4a" | "ogg" | "wma" => Category::Media,
        "zip" | "rar" | "7z" | "tar" | "gz" | "bz2" | "xz" => Category::Archives,
        "exe" | "com" | "msi" | "msix" | "msixbundle" | "appx" | "appxbundle" | "dll" => {
            Category::Programs
        }
        "ps1" | "psm1" | "psd1" | "bat" | "cmd" | "py" | "pyw" | "sh" | "js" | "vbs" | "ahk"
        | "lua" | "rb" => Category::Scripts,
        _ => return None,
    })
}

#[derive(Clone, Debug)]
pub struct Candidate {
    pub path: PathBuf,
    pub category: Category,
    pub identity: Identity,
}

#[derive(Clone, Debug, Default)]
pub struct Plan {
    pub items: Vec<Candidate>,
    pub skipped: Vec<String>,
}

/// Both filesystem desktop roots; virtual icons are not filesystem candidates.
pub fn scan(desktops: &[PathBuf], storage: &Path) -> io::Result<Plan> {
    let mut plan = Plan::default();
    let mut visited = std::collections::HashSet::new();
    for desktop in desktops {
        if !visited.insert(desktop.canonicalize().unwrap_or_else(|_| desktop.clone())) {
            continue;
        }
        let entries = match fs::read_dir(desktop) {
            Ok(entries) => entries,
            Err(error) => {
                plan.skipped
                    .push(i18n::tf(K::ItemError, &[&desktop.display(), &error]));
                continue;
            }
        };
        for result in entries {
            let entry = match result {
                Ok(entry) => entry,
                Err(error) => {
                    plan.skipped.push(i18n::tf(K::ReadItemFailed, &[&error]));
                    continue;
                }
            };
            let path = entry.path();
            let candidate = inspect(&path).and_then(|item| {
                protect_storage(&path, storage)?;
                Ok(item)
            });
            match candidate {
                Ok(item) => plan.items.push(item),
                Err(reason) => plan.skipped.push(i18n::tf(
                    K::ItemError,
                    &[&entry.file_name().to_string_lossy(), &reason],
                )),
            }
        }
    }
    plan.items.sort_by_key(|item| {
        item.path
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .to_lowercase()
    });
    plan.skipped.sort();
    Ok(plan)
}

pub fn protect_storage(path: &Path, storage: &Path) -> io::Result<()> {
    if path.is_dir()
        && path.canonicalize().is_ok_and(|source| {
            storage
                .canonicalize()
                .is_ok_and(|root| root.starts_with(&source))
        })
    {
        return Err(io::Error::other(i18n::t(K::StorageFolder)));
    }
    Ok(())
}

/// Recheck safety rules as well as identity after the user reviews a preview.
pub fn inspect(path: &Path) -> io::Result<Candidate> {
    let metadata = fs::symlink_metadata(path)?;
    if metadata.file_attributes() & (0x2 | 0x4 | 0x400 | 0x1000 | 0x40000 | 0x400000) != 0 {
        return Err(io::Error::other(i18n::t(K::HiddenItem)));
    }
    let shell_metadata = path.file_name().is_some_and(|name| {
        let name = name.to_string_lossy();
        name.eq_ignore_ascii_case("desktop.ini") || name.eq_ignore_ascii_case("thumbs.db")
    });
    let category = if metadata.is_dir() {
        Category::Folders
    } else if shell_metadata {
        return Err(io::Error::other(i18n::t(K::DesktopIni)));
    } else if metadata.is_file() {
        classify(path).unwrap_or(Category::Other)
    } else {
        return Err(io::Error::other(i18n::t(K::Unsupported)));
    };
    Ok(Candidate {
        path: path.to_owned(),
        category,
        identity: identity(path)?,
    })
}

impl Plan {
    pub fn preview_text(&self) -> String {
        let mut text = i18n::tf(K::PreviewSummary, &[&self.items.len(), &self.skipped.len()]);
        text.push_str(
            "

",
        );
        for category in Category::ALL {
            let items: Vec<_> = self
                .items
                .iter()
                .filter(|i| i.category == category)
                .collect();
            if !items.is_empty() {
                text.push_str(&i18n::tf(
                    K::PreviewCategory,
                    &[&category.title(), &items.len()],
                ));
                text.push_str("\r\n");
                for item in items {
                    text.push_str(&format!(
                        "  {}\r\n",
                        item.path.file_name().unwrap_or_default().to_string_lossy()
                    ));
                }
                text.push_str("\r\n");
            }
        }
        if !self.skipped.is_empty() {
            text.push_str(i18n::t(K::PreviewKept));
            text.push_str("\r\n");
            for reason in &self.skipped {
                text.push_str(reason);
                text.push_str("\r\n");
            }
        }
        text
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_case_insensitive_including_programs_and_scripts() {
        for (name, expected) in [
            ("瀏覽器.LNK", Category::Programs),
            ("線上程式.appref-ms", Category::Programs),
            ("網站.url", Category::Shortcuts),
            ("報價.XLSX", Category::Documents),
            ("照片.JpG", Category::Pictures),
            ("片段.MP4", Category::Media),
            ("備份.7z", Category::Archives),
            ("tool.EXE", Category::Programs),
            ("helper.DLL", Category::Programs),
            ("run.PS1", Category::Scripts),
            ("run.cmd", Category::Scripts),
            ("run.bat", Category::Scripts),
            ("run.py", Category::Scripts),
        ] {
            assert_eq!(classify(Path::new(name)), Some(expected));
        }
        for name in [".env", "unknown"] {
            assert_eq!(classify(Path::new(name)), None);
        }
    }

    #[test]
    fn unmatched_ordinary_files_go_to_other_but_shell_metadata_stays() {
        let dir = std::env::temp_dir().join(format!("luma-other-{}", crate::model::unique_id()));
        fs::create_dir(&dir).unwrap();
        for name in [".env", "nginx.conf", "compose.yml", "deploy.log", "noext"] {
            fs::write(dir.join(name), b"x").unwrap();
            assert_eq!(inspect(&dir.join(name)).unwrap().category, Category::Other);
        }
        for name in ["desktop.ini", "Thumbs.db"] {
            fs::write(dir.join(name), b"x").unwrap();
            assert!(inspect(&dir.join(name)).is_err());
        }
        fs::remove_dir_all(dir).unwrap();
    }
}
