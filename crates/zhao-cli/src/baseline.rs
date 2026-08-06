//! Resolves the Baseline for `zhao check`: either the manifest explicitly
//! passed via `--state`, or -- when that's absent -- a Baseline zhao
//! compiles itself from the git-native merge-base commit, with no external
//! artifact required.

use std::path::{Path, PathBuf};

use zhao_core::adapters::TransformationToolAdapter;
use zhao_core::adapters::dbt::{DbtAdapter, DbtAdapterError};
use zhao_core::git::{self, GitError};
use zhao_core::model::ParsedProject;

/// Everything that can go wrong resolving a Baseline.
#[derive(Debug, thiserror::Error)]
pub enum BaselineError {
    /// The `--state` manifest couldn't be read or parsed.
    #[error("{path}: {source}")]
    Manifest {
        /// The `--state` path that failed.
        path: String,
        /// The underlying parse error.
        #[source]
        source: DbtAdapterError,
    },
    /// Resolving or checking out the git-native merge-base commit failed.
    #[error(transparent)]
    Git(#[from] GitError),
    /// Compiling or parsing the merge-base commit failed.
    #[error(transparent)]
    Dbt(#[from] DbtAdapterError),
}

/// Resolves the Baseline `zhao check` diffs the current project against.
///
/// If `state_path` is given, it's parsed directly (`extra_args` is
/// ignored in this path -- there's no `dbt` invocation to pass them to).
/// Otherwise, the merge-base commit between `HEAD` and `against` is
/// resolved in the git repository containing `project_dir`, checked out
/// into a temporary worktree, compiled there with `dbt`, and parsed from
/// that worktree -- all without requiring any externally-supplied
/// Baseline artifact. `dbt deps` is run first whenever the worktree's
/// project directory has a `packages.yml` or `dependencies.yml`, since
/// `dbt compile` fails on any `ref()`/macro from an as-yet-uninstalled
/// package -- exactly the state a freshly checked-out worktree is in the
/// first time. `extra_args` (from `--dbt-arg`/`--dbt-args`) are appended
/// verbatim to both the `dbt deps` and `dbt compile` invocations.
///
/// Before the temporary worktree is torn down (it's removed the moment
/// this function returns -- see [`git::Worktree`]'s own `Drop`), its
/// compiled `target/manifest.json` is copied to
/// `<project_dir>/target/zhao/baseline_manifest.json`, so what the
/// Baseline actually compiled to is still inspectable afterward instead
/// of vanishing along with the worktree. Best-effort: a failure to copy
/// (permissions, a read-only `target/`, ...) is reported to stderr as a
/// warning and never fails Baseline resolution itself -- the same
/// "a sidecar artifact failing to write shouldn't fail the actual
/// command" precedent `target/zhao/run-metadata.json` already follows.
/// Only meaningful for this git-native path -- `--state <path>` returns
/// above, before anything is compiled, so there's nothing to capture.
pub fn resolve(
    state_path: Option<&Path>,
    project_dir: &Path,
    against: &str,
    extra_args: &[String],
) -> Result<ParsedProject, BaselineError> {
    if let Some(path) = state_path {
        return DbtAdapter
            .parse(path)
            .map_err(|source| BaselineError::Manifest {
                path: path.display().to_string(),
                source,
            });
    }

    let repo_root = git::repo_root(project_dir)?;
    let canonical_repo_root = repo_root.canonicalize().unwrap_or(repo_root);
    let canonical_project_dir = project_dir
        .canonicalize()
        .unwrap_or_else(|_| project_dir.to_path_buf());
    let relative_project_dir = canonical_project_dir
        .strip_prefix(&canonical_repo_root)
        .unwrap_or(Path::new("."));

    let merge_base = git::resolve_merge_base(&canonical_repo_root, against)?;
    let worktree = git::create_worktree(&canonical_repo_root, &merge_base)?;
    let worktree_project_dir = worktree.path().join(relative_project_dir);

    if worktree_project_dir.join("packages.yml").exists()
        || worktree_project_dir.join("dependencies.yml").exists()
    {
        DbtAdapter.deps(&worktree_project_dir, "dbt", extra_args)?;
    }
    DbtAdapter.compile(&worktree_project_dir, "dbt", extra_args)?;

    let manifest_path = worktree_project_dir.join("target").join("manifest.json");
    capture_baseline_manifest(&manifest_path, project_dir);
    Ok(DbtAdapter.parse(&manifest_path)?)
}

/// Copies `manifest_path` (the just-compiled Baseline's manifest, still
/// inside the temporary worktree at this point) to
/// `<project_dir>/target/zhao/baseline_manifest.json`. See [`resolve`]'s
/// own doc comment for why, and for the best-effort/non-fatal contract.
fn capture_baseline_manifest(manifest_path: &Path, project_dir: &Path) {
    let dest_dir = project_dir.join("target").join("zhao");
    if let Err(err) = std::fs::create_dir_all(&dest_dir) {
        eprintln!(
            "warning: could not create {} to capture the baseline manifest: {err}",
            dest_dir.display()
        );
        return;
    }

    // Written via a temp file in the same directory, then renamed into
    // place -- the same atomic-write precedent `target/zhao/run-
    // metadata.json` (`crate::metadata::write`) already follows, so a
    // failure partway through (disk full, the process killed) can never
    // leave a truncated/corrupt `baseline_manifest.json` overwriting a
    // previously good one from an earlier run.
    let dest: PathBuf = dest_dir.join("baseline_manifest.json");
    let write_result = (|| -> std::io::Result<()> {
        let contents = std::fs::read(manifest_path)?;
        let mut temp_file = tempfile::NamedTempFile::new_in(&dest_dir)?;
        std::io::Write::write_all(&mut temp_file, &contents)?;
        temp_file.persist(&dest).map_err(|err| err.error)?;
        Ok(())
    })();
    if let Err(err) = write_result {
        eprintln!(
            "warning: could not capture the baseline manifest to {}: {err}",
            dest.display()
        );
    }
}
