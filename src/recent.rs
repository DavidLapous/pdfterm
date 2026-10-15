use std::env;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

const MAX_RECENTS: usize = 50;
static TEMP_ID: AtomicU64 = AtomicU64::new(0);

/// Loads the recently opened documents, most-recent first. Returns an empty
/// list when no history file exists or it cannot be read.
pub fn load() -> Vec<PathBuf> {
    let Some(path) = recent_path() else {
        return Vec::new();
    };
    let Ok(text) = fs::read_to_string(&path) else {
        return Vec::new();
    };
    text.lines()
        .filter(|line| !line.trim().is_empty())
        .map(PathBuf::from)
        .collect()
}

/// Records `path` as the most recently opened document, promoting it to the
/// front of the history and capping the list. Failures are ignored so history
/// never blocks opening a document.
pub fn record(path: &Path) {
    if path
        .as_os_str()
        .as_bytes()
        .iter()
        .any(|byte| matches!(byte, b'\n' | b'\r'))
    {
        return;
    }
    let Some(store) = recent_path() else {
        return;
    };
    let entries = promote(load(), path, MAX_RECENTS);
    let Some(parent) = store.parent() else {
        return;
    };
    if fs::create_dir_all(parent).is_err() {
        return;
    }
    let contents = entries
        .iter()
        .map(|path| path.to_string_lossy().into_owned())
        .collect::<Vec<_>>()
        .join("\n");
    let id = TEMP_ID.fetch_add(1, Ordering::Relaxed);
    let temp = parent.join(format!(".recent.{}.{id}.tmp", std::process::id()));
    let Ok(mut file) = OpenOptions::new().write(true).create_new(true).open(&temp) else {
        return;
    };
    if file.write_all(contents.as_bytes()).is_ok() {
        drop(file);
        let _ = fs::rename(&temp, &store);
    } else {
        drop(file);
    }
    let _ = fs::remove_file(&temp);
}

/// Moves `path` to the front of `entries`, removing any earlier occurrence and
/// truncating to `max` items.
fn promote(mut entries: Vec<PathBuf>, path: &Path, max: usize) -> Vec<PathBuf> {
    entries.retain(|existing| existing != path);
    entries.insert(0, path.to_path_buf());
    entries.truncate(max);
    entries
}

fn recent_path() -> Option<PathBuf> {
    let base = env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .filter(|path| !path.as_os_str().is_empty())
        .or_else(|| env::var_os("HOME").map(|home| PathBuf::from(home).join(".cache")))?;
    Some(base.join("pdfterm").join("recent"))
}

#[cfg(test)]
mod tests {
    use super::promote;
    use std::path::PathBuf;

    #[test]
    fn promote_moves_existing_entry_to_front() {
        let entries = vec![PathBuf::from("/a.pdf"), PathBuf::from("/b.pdf")];

        let promoted = promote(entries, &PathBuf::from("/b.pdf"), 50);

        assert_eq!(
            promoted,
            vec![PathBuf::from("/b.pdf"), PathBuf::from("/a.pdf")]
        );
    }

    #[test]
    fn promote_caps_the_history_length() {
        let entries = vec![PathBuf::from("/a.pdf"), PathBuf::from("/b.pdf")];

        let promoted = promote(entries, &PathBuf::from("/c.pdf"), 2);

        assert_eq!(
            promoted,
            vec![PathBuf::from("/c.pdf"), PathBuf::from("/a.pdf")]
        );
    }
}
