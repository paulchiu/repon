//! The git backend. gix reads; nothing here writes.
//!
//! Private, and nothing here is re-exported yet: see the crate root doc comment.

use std::path::Path;
use std::sync::Arc;

/// Error from a git read, cheap to clone because the whole state table is cloned
/// every frame. A shared trait object was rejected: it gives no discriminant to
/// branch on and nothing to serialise, and nothing in this crate reads a source chain.
#[derive(Clone, Debug)]
pub enum ProbeError {
    /// The path could not be opened as a git repository.
    Open(Arc<str>),
    /// An open repository's `HEAD` could not be read.
    Read(Arc<str>),
}

impl std::fmt::Display for ProbeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ProbeError::Open(message) => write!(f, "failed to open git repository: {message}"),
            ProbeError::Read(message) => write!(f, "failed to read HEAD: {message}"),
        }
    }
}

impl std::error::Error for ProbeError {}

/// Short name of the branch HEAD points at, or `None` when HEAD is detached.
///
/// This is the whole of the backend for now: the smallest read that proves gix is
/// wired, and the same call the scale benchmark measured at roughly 10ms per Repo.
#[allow(dead_code)] // no caller until a state model re-exports it
pub fn head_branch(repo: &Path) -> Result<Option<String>, ProbeError> {
    let repo = gix::open(repo).map_err(|error| ProbeError::Open(error.to_string().into()))?;
    let name = repo
        .head_name()
        .map_err(|error| ProbeError::Read(error.to_string().into()))?;
    Ok(name.map(|name| name.shorten().to_string()))
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

        assert!(matches!(head_branch(dir.path()), Err(ProbeError::Open(_))));
    }

    #[test]
    fn every_variant_clones() {
        let open = ProbeError::Open(Arc::from("boom"));
        let read = ProbeError::Read(Arc::from("boom"));

        assert_eq!(open.clone().to_string(), open.to_string());
        assert_eq!(read.clone().to_string(), read.to_string());
    }
}
