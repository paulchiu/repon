//! Stale ref locks: the `.lock` files an interrupted ref update leaves behind, which make
//! every later fetch into that repository fail with an error that does not name them.
//! Read-only: this module lists them and never deletes one.

use std::path::{Path, PathBuf};

/// Every `.lock` file under `common_dir`'s `refs/`, plus its `packed-refs.lock`, sorted.
/// `common_dir` may be the git dir itself or a working tree holding one at `.git`. An
/// unreadable directory contributes nothing rather than failing the read.
pub(crate) fn stale_ref_locks(common_dir: &Path) -> Vec<PathBuf> {
    let git_dir = if common_dir.join("refs").is_dir() {
        common_dir.to_path_buf()
    } else {
        common_dir.join(".git")
    };
    let mut locks = Vec::new();
    collect_locks(&git_dir.join("refs"), &mut locks);
    let packed = git_dir.join("packed-refs.lock");
    if packed.is_file() {
        locks.push(packed);
    }
    locks.sort();
    locks
}

fn collect_locks(dir: &Path, locks: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        match entry.file_type() {
            Ok(kind) if kind.is_dir() => collect_locks(&path, locks),
            Ok(kind) if kind.is_file() && path.extension().is_some_and(|ext| ext == "lock") => {
                locks.push(path);
            }
            _ => {}
        }
    }
}
