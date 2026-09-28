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
    let packed = Some(git_dir.join("packed-refs.lock")).filter(|path| path.is_file());
    let mut locks: Vec<PathBuf> = locks_under(&git_dir.join("refs"))
        .into_iter()
        .chain(packed)
        .collect();
    locks.sort();
    locks
}

fn locks_under(dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    entries
        .flatten()
        .flat_map(|entry| {
            let path = entry.path();
            match entry.file_type() {
                Ok(kind) if kind.is_dir() => locks_under(&path),
                Ok(kind) if kind.is_file() && path.extension().is_some_and(|ext| ext == "lock") => {
                    vec![path]
                }
                _ => Vec::new(),
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn touch(path: &Path) {
        std::fs::create_dir_all(path.parent().unwrap()).expect("create parent");
        std::fs::write(path, "").expect("write file");
    }

    #[test]
    fn a_git_dir_yields_every_ref_lock_at_any_depth_and_its_packed_refs_lock_sorted() {
        let dir = tempfile::tempdir().expect("temp dir");
        let git_dir = dir.path();
        touch(&git_dir.join("refs/tags/v1.lock"));
        touch(&git_dir.join("refs/remotes/origin/main.lock"));
        touch(&git_dir.join("refs/heads/main"));
        touch(&git_dir.join("packed-refs.lock"));

        assert_eq!(
            stale_ref_locks(git_dir),
            vec![
                git_dir.join("packed-refs.lock"),
                git_dir.join("refs/remotes/origin/main.lock"),
                git_dir.join("refs/tags/v1.lock"),
            ]
        );
    }

    #[test]
    fn a_working_tree_is_read_through_its_dot_git() {
        let dir = tempfile::tempdir().expect("temp dir");
        let lock = dir.path().join(".git/refs/tags/v1.lock");
        touch(&lock);

        assert_eq!(stale_ref_locks(dir.path()), vec![lock]);
    }

    #[test]
    fn a_directory_with_no_refs_yields_none() {
        let dir = tempfile::tempdir().expect("temp dir");

        assert!(stale_ref_locks(dir.path()).is_empty());
    }
}
