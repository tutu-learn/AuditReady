use chrono::{DateTime, Utc};
use serde::Serialize;
use std::collections::HashMap;
use std::path::Path;
use std::time::SystemTime;

/// Max changed files reported per period, most recent first.
const MAX_ENTRIES: usize = 500;

/// Max folder-activity rows reported per period, busiest first.
const MAX_FOLDERS: usize = 500;

/// A file under the scan root that changed since the last watermark.
#[derive(Debug, Clone, Serialize)]
pub struct ChangedFile {
    pub path: String,
    pub size_bytes: u64,
    pub modified_at: DateTime<Utc>,
}

/// Per-folder write activity over the period: how many files under the folder
/// changed and when the newest change was. Aggregated from the full walk
/// (before the changed-file list is capped), so busy folders are not
/// undercounted.
#[derive(Debug, Clone, Serialize)]
pub struct FolderWrite {
    pub folder: String,
    pub write_count: u64,
    pub last_write_at: DateTime<Utc>,
}

/// Recursively walk `root` and return the files modified after `since` (most
/// recent first, capped at 500) together with the per-folder write activity
/// (busiest first, capped at 500 folders). Directory names (or slash-separated
/// relative paths) listed in `excluded` are skipped, as are symlinked
/// directories.
pub fn scan(
    root: &Path,
    since: SystemTime,
    excluded: &[String],
) -> (Vec<ChangedFile>, Vec<FolderWrite>) {
    let mut changed = Vec::new();
    walk(root, root, since, excluded, &mut changed);
    let folder_writes = folder_write_summary(&changed);
    changed.sort_by(|a, b| b.modified_at.cmp(&a.modified_at));
    changed.truncate(MAX_ENTRIES);
    (changed, folder_writes)
}

fn folder_write_summary(changed: &[ChangedFile]) -> Vec<FolderWrite> {
    let mut by_folder: HashMap<String, FolderWrite> = HashMap::new();
    for f in changed {
        let folder = match f.path.rfind(|c| c == '/' || c == '\\') {
            Some(sep) if sep > 0 => &f.path[..sep],
            _ => f.path.as_str(),
        };
        let entry = by_folder.entry(folder.to_string()).or_insert_with(|| FolderWrite {
            folder: folder.to_string(),
            write_count: 0,
            last_write_at: f.modified_at,
        });
        entry.write_count += 1;
        if f.modified_at > entry.last_write_at {
            entry.last_write_at = f.modified_at;
        }
    }
    let mut out: Vec<FolderWrite> = by_folder.into_values().collect();
    out.sort_by(|a, b| b.write_count.cmp(&a.write_count));
    out.truncate(MAX_FOLDERS);
    out
}

fn walk(
    root: &Path,
    dir: &Path,
    since: SystemTime,
    excluded: &[String],
    changed: &mut Vec<ChangedFile>,
) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return; // Unreadable directory (permissions); skip it.
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        // Never follow symlinks: avoids loops outside the scan root.
        if file_type.is_symlink() {
            continue;
        }
        if file_type.is_dir() {
            if is_excluded(root, &path, excluded) {
                continue;
            }
            walk(root, &path, since, excluded, changed);
        } else if file_type.is_file() {
            let Ok(metadata) = entry.metadata() else {
                continue;
            };
            let Ok(modified) = metadata.modified() else {
                continue;
            };
            if modified > since {
                changed.push(ChangedFile {
                    path: path.to_string_lossy().to_string(),
                    size_bytes: metadata.len(),
                    modified_at: DateTime::<Utc>::from(modified),
                });
            }
        }
    }
}

/// Match excluded entries against the directory's own name and its
/// slash-normalized path relative to the scan root (so both `"node_modules"`
/// and `"Library/Caches"` work).
fn is_excluded(root: &Path, dir: &Path, excluded: &[String]) -> bool {
    let name = dir.file_name().map(|n| n.to_string_lossy().to_string());
    let relative: Option<String> = dir
        .strip_prefix(root)
        .ok()
        .map(|p: &Path| p.to_string_lossy().replace('\\', "/"));
    excluded.iter().any(|ex| {
        let ex = ex.trim_matches(|c| c == '/' || c == '\\');
        Some(ex.to_string()) == name || relative.as_deref() == Some(ex)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::time::Duration;

    #[test]
    fn reports_only_files_newer_than_watermark_and_skips_excluded() {
        let root =
            std::env::temp_dir().join(format!("auditready-scan-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("sub")).unwrap();
        std::fs::create_dir_all(root.join("node_modules")).unwrap();
        std::fs::create_dir_all(root.join("Library/Caches")).unwrap();

        // An "old" file that exists before the watermark.
        std::fs::write(root.join("old.txt"), b"old").unwrap();
        let watermark = SystemTime::now();
        // Make sure "new" files get a strictly later mtime.
        std::thread::sleep(Duration::from_millis(20));

        std::fs::write(root.join("new.txt"), b"new").unwrap();
        std::fs::write(root.join("sub/nested.txt"), b"nested").unwrap();
        std::fs::write(root.join("node_modules/skip.txt"), b"skip").unwrap();
        std::fs::write(root.join("Library/Caches/skip.txt"), b"skip").unwrap();

        let excluded = vec!["node_modules".to_string(), "Library/Caches".to_string()];
        let (changed, folder_writes) = scan(&root, watermark, &excluded);
        let paths: Vec<PathBuf> = changed.iter().map(|f| PathBuf::from(&f.path)).collect();

        assert!(paths.contains(&root.join("new.txt")), "paths: {:?}", paths);
        assert!(
            paths.contains(&root.join("sub/nested.txt")),
            "paths: {:?}",
            paths
        );
        assert!(!paths.contains(&root.join("old.txt")), "paths: {:?}", paths);
        assert!(
            !paths.contains(&root.join("node_modules/skip.txt")),
            "paths: {:?}",
            paths
        );
        assert!(
            !paths.contains(&root.join("Library/Caches/skip.txt")),
            "paths: {:?}",
            paths
        );
        assert_eq!(changed.len(), 2);
        // Most recent first.
        assert!(changed[0].modified_at >= changed[1].modified_at);
        assert!(changed.iter().all(|f| f.size_bytes > 0));

        // Folder activity aggregates over the same walk: one row for the root
        // (new.txt) and one for sub/ (nested.txt); excluded dirs contribute
        // nothing.
        assert_eq!(folder_writes.len(), 2);
        assert!(folder_writes
            .iter()
            .all(|f| f.write_count == 1));
        let nested_mtime = changed
            .iter()
            .find(|f| f.path.ends_with("nested.txt"))
            .unwrap()
            .modified_at;
        let sub = folder_writes
            .iter()
            .find(|f| f.folder.ends_with("sub"))
            .expect("sub folder row");
        assert_eq!(sub.last_write_at, nested_mtime);

        std::fs::remove_dir_all(&root).ok();
    }
}
