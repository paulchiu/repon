//! The git backend. gix reads; nothing here writes.

use std::path::Path;

/// Error from a git read. A concrete error type arrives with the API boundary.
pub type Error = Box<dyn std::error::Error + Send + Sync>;

/// Short name of the branch HEAD points at, or `None` when HEAD is detached.
///
/// This is the whole of the backend for now: the smallest read that proves gix is
/// wired, and the same call the scale benchmark measured at roughly 10ms per Repo.
pub fn head_branch(repo: &Path) -> Result<Option<String>, Error> {
    let repo = gix::open(repo)?;
    Ok(repo.head_name()?.map(|name| name.shorten().to_string()))
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;

    #[test]
    fn shortens_the_branch_that_head_points_at() {
        let dir = tempfile::tempdir().expect("temp dir");
        gix::init(dir.path()).expect("init");
        let head = fs::read_to_string(dir.path().join(".git/HEAD")).expect("read HEAD");
        let expected = head
            .trim()
            .strip_prefix("ref: refs/heads/")
            .expect("a symbolic HEAD");

        let branch = head_branch(dir.path()).expect("read the branch");

        assert_eq!(branch.as_deref(), Some(expected));
    }

    #[test]
    fn a_directory_that_is_not_a_repo_is_an_error() {
        let dir = tempfile::tempdir().expect("temp dir");

        assert!(head_branch(dir.path()).is_err());
    }
}
