//! Resolves the Baseline for `zhao check`: either the manifest explicitly
//! passed via `--state`, or -- when that's absent -- a Baseline zhao
//! compiles itself from the git-native merge-base commit, with no external
//! artifact required.

use std::path::Path;

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
    Ok(DbtAdapter.parse(&manifest_path)?)
}
