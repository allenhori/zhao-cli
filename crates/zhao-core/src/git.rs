//! Git operations backing zhao's git-native Baseline resolution: finding
//! the merge-base commit to compile as a Baseline when a caller doesn't
//! supply one explicitly (e.g. via `--state`), and materializing that
//! commit into an isolated worktree to compile it in.
//!
//! This module is deliberately independent of any [`crate::adapters`] --
//! resolving a merge-base and checking it out into a worktree has nothing
//! to do with dbt specifically, and any future `TransformationToolAdapter`
//! wanting the same git-native Baseline behavior can reuse it unchanged.

use std::path::{Path, PathBuf};
use std::process::Command;

/// A git worktree checked out at a specific commit, in a temporary
/// directory. Removed (via `git worktree remove`) when this value is
/// dropped, so a caller doesn't need to remember to clean it up, and a
/// panic partway through a Baseline resolution doesn't leak it either.
#[derive(Debug)]
pub struct Worktree {
    path: PathBuf,
    repo_root: PathBuf,
}

impl Worktree {
    /// The worktree's checked-out path on disk.
    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for Worktree {
    fn drop(&mut self) {
        // Best-effort: if this fails (e.g. the directory was already
        // removed some other way), there's no meaningful way to surface
        // that from a `Drop` impl, and nothing for a caller to act on.
        let _ = Command::new("git")
            .arg("-C")
            .arg(&self.repo_root)
            .arg("worktree")
            .arg("remove")
            .arg("--force")
            .arg(&self.path)
            .output();
    }
}

/// Everything that can go wrong while resolving a git-native Baseline.
#[derive(Debug, thiserror::Error)]
pub enum GitError {
    /// `dir` isn't inside a git repository (or `git` couldn't otherwise
    /// determine its toplevel).
    #[error("{dir}: not inside a git repository ({stderr})")]
    NotAGitRepository {
        /// The directory that isn't inside a git repository.
        dir: String,
        /// `git`'s captured stderr.
        stderr: String,
    },
    /// No merge-base could be found between `HEAD` and `against` -- most
    /// likely the histories are unrelated, or `against` doesn't exist.
    #[error(
        "could not find a merge-base between HEAD and {against:?} in {repo_root} -- the \
         histories may be unrelated, or {against:?} may not exist ({stderr})"
    )]
    MergeBaseNotFound {
        /// The repository the merge-base was sought in.
        repo_root: String,
        /// The ref `HEAD` was compared against.
        against: String,
        /// `git`'s captured stderr.
        stderr: String,
    },
    /// `git worktree add` ran but exited with a failure.
    #[error("could not create a git worktree for commit {commit} in {repo_root}: {stderr}")]
    WorktreeCreationFailed {
        /// The repository the worktree was created from.
        repo_root: String,
        /// The commit the worktree was checked out at.
        commit: String,
        /// `git`'s captured stderr.
        stderr: String,
    },
    /// `git` itself could not be run.
    #[error("could not run git -- is it installed and on PATH? ({source})")]
    CommandNotFound {
        /// The underlying I/O error from trying to spawn `git`.
        #[source]
        source: std::io::Error,
    },
}

/// Finds the root of the git repository containing `dir`
/// (`git rev-parse --show-toplevel`).
pub fn repo_root(dir: &Path) -> Result<PathBuf, GitError> {
    let output = run_git(dir, &["rev-parse", "--show-toplevel"])?;
    if !output.status.success() {
        return Err(GitError::NotAGitRepository {
            dir: dir.display().to_string(),
            stderr: stderr_of(&output),
        });
    }
    Ok(PathBuf::from(stdout_of(&output)))
}

/// Resolves the merge-base commit SHA between `HEAD` and `against` (e.g.
/// `"master"`) in the repository at `repo_root`.
pub fn resolve_merge_base(repo_root: &Path, against: &str) -> Result<String, GitError> {
    let output = run_git(repo_root, &["merge-base", "HEAD", against])?;
    if !output.status.success() {
        return Err(GitError::MergeBaseNotFound {
            repo_root: repo_root.display().to_string(),
            against: against.to_string(),
            stderr: stderr_of(&output),
        });
    }
    Ok(stdout_of(&output))
}

/// Creates a new worktree in a fresh temporary directory, checked out at
/// `commit`.
pub fn create_worktree(repo_root: &Path, commit: &str) -> Result<Worktree, GitError> {
    let dir = tempfile::Builder::new()
        .prefix("zhao-baseline-")
        .tempdir()
        .map_err(|source| GitError::CommandNotFound { source })?;
    // `git worktree add <path>` wants to create `<path>` itself (it
    // refuses to check out into a directory that already exists), so the
    // temp dir is only used to reserve a unique path, then immediately
    // removed via `close` -- as opposed to just dropping `dir`, `close`
    // consumes it outright, so there's no lingering `TempDir` handle left
    // that would try to remove the directory again (and this time
    // destroy the real worktree) once it goes out of scope. `Worktree`'s
    // own `Drop` takes over cleanup from here via `git worktree remove`.
    let path = dir.path().to_path_buf();
    dir.close()
        .map_err(|source| GitError::WorktreeCreationFailed {
            repo_root: repo_root.display().to_string(),
            commit: commit.to_string(),
            stderr: source.to_string(),
        })?;

    let output = run_git(
        repo_root,
        &[
            "worktree",
            "add",
            "--detach",
            path.to_str().unwrap_or_default(),
            commit,
        ],
    )?;
    if !output.status.success() {
        return Err(GitError::WorktreeCreationFailed {
            repo_root: repo_root.display().to_string(),
            commit: commit.to_string(),
            stderr: stderr_of(&output),
        });
    }

    Ok(Worktree {
        path,
        repo_root: repo_root.to_path_buf(),
    })
}

fn run_git(cwd: &Path, args: &[&str]) -> Result<std::process::Output, GitError> {
    Command::new("git")
        .current_dir(cwd)
        .args(args)
        .output()
        .map_err(|source| GitError::CommandNotFound { source })
}

fn stdout_of(output: &std::process::Output) -> String {
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

fn stderr_of(output: &std::process::Output) -> String {
    String::from_utf8_lossy(&output.stderr).trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A throwaway git repository under a temp dir, with one commit on
    /// its initial branch -- enough to exercise `repo_root` and
    /// `resolve_merge_base` without any network access or real remote.
    struct TestRepo {
        _dir: tempfile::TempDir,
        path: PathBuf,
    }

    impl TestRepo {
        fn git(&self, args: &[&str]) -> std::process::Output {
            Command::new("git")
                .current_dir(&self.path)
                .args(args)
                .output()
                .expect("git should be runnable in tests")
        }

        fn commit(&self, message: &str) -> String {
            std::fs::write(self.path.join("file.txt"), message).expect("should write file");
            self.git(&["add", "."]);
            let output = self.git(&["commit", "-m", message]);
            assert!(output.status.success(), "commit should succeed: {output:?}");
            stdout_of(&self.git(&["rev-parse", "HEAD"]))
        }
    }

    fn new_test_repo() -> TestRepo {
        let dir = tempfile::tempdir().expect("should create temp dir");
        let path = dir.path().to_path_buf();
        let repo = TestRepo { _dir: dir, path };

        let init = repo.git(&["init", "--initial-branch=master"]);
        assert!(init.status.success(), "git init should succeed: {init:?}");
        repo.git(&["config", "user.email", "test@zhao.invalid"]);
        repo.git(&["config", "user.name", "zhao test"]);
        repo
    }

    #[test]
    fn repo_root_resolves_to_the_repositorys_toplevel() {
        let repo = new_test_repo();
        repo.commit("initial");
        let nested = repo.path.join("models").join("staging");
        std::fs::create_dir_all(&nested).expect("should create nested dir");

        let root = repo_root(&nested).expect("should resolve repo root");

        assert_eq!(
            std::fs::canonicalize(&root).expect("should canonicalize"),
            std::fs::canonicalize(&repo.path).expect("should canonicalize")
        );
    }

    #[test]
    fn repo_root_produces_a_clear_error_outside_any_git_repository() {
        let dir = tempfile::tempdir().expect("should create temp dir");

        let result = repo_root(dir.path());

        assert!(matches!(result, Err(GitError::NotAGitRepository { .. })));
    }

    #[test]
    fn resolve_merge_base_finds_the_common_ancestor_of_head_and_a_branch() {
        let repo = new_test_repo();
        let base_commit = repo.commit("on master");
        repo.git(&["checkout", "-b", "feature"]);
        repo.commit("on feature, ahead of master");

        let merge_base =
            resolve_merge_base(&repo.path, "master").expect("should find a merge-base");

        assert_eq!(merge_base, base_commit);
    }

    #[test]
    fn resolve_merge_base_produces_a_clear_error_when_against_does_not_exist() {
        let repo = new_test_repo();
        repo.commit("initial");

        let result = resolve_merge_base(&repo.path, "this-branch-does-not-exist");

        assert!(matches!(result, Err(GitError::MergeBaseNotFound { .. })));
    }

    #[test]
    fn create_worktree_checks_out_the_given_commit_in_a_new_directory() {
        let repo = new_test_repo();
        let first_commit = repo.commit("first");
        repo.commit("second");

        let worktree = create_worktree(&repo.path, &first_commit).expect("should create worktree");

        assert_eq!(
            std::fs::read_to_string(worktree.path().join("file.txt")).expect("should read file"),
            "first",
            "the worktree should reflect the checked-out commit, not HEAD"
        );
    }

    #[test]
    fn create_worktree_cleans_up_its_directory_when_dropped() {
        let repo = new_test_repo();
        let commit = repo.commit("only commit");

        let worktree = create_worktree(&repo.path, &commit).expect("should create worktree");
        let path = worktree.path().to_path_buf();
        assert!(path.exists());

        drop(worktree);

        assert!(
            !path.exists(),
            "the worktree directory should be removed once dropped"
        );
    }
}
