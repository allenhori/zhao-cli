//! Trait boundaries between zhao-core's neutral engine and anything
//! specific to a transformation tool or a warehouse.
//!
//! See `ARCHITECTURE.md` at the repository root for the reasoning behind
//! this split. dbt is the first (and, for now, only) implementation of
//! [`TransformationToolAdapter`]; see [`dbt`]. Snowflake, Databricks,
//! BigQuery, and DuckDB are the v1 implementations of
//! [`warehouse::WarehouseAdapter`]; see [`warehouse`].

pub mod dbt;
pub mod warehouse;

use crate::model::ParsedProject;
use std::error::Error;
use std::path::Path;

/// Reads a specific transformation-tool project format and produces zhao's
/// internal representation of it, and knows how to produce a fresh
/// compiled artifact for that format in the first place.
///
/// Implementations own everything specific to how their tool expresses
/// transformations. Nothing outside an adapter's own module should depend
/// on that tool's specific types -- callers only ever see this trait's
/// associated types and methods, never (for example) a raw dbt manifest
/// structure.
///
/// The boundary originally covered only `parse()` -- "how did the compiled
/// output get there" (dbt's `dbt compile`/`dbt deps`) was left entirely to
/// the caller, and every caller hardcoded the concrete `DbtAdapter` type to
/// do it. `compile`/`deps`/`query_executor` joined the trait so a new
/// adapter supplies its own refresh step instead of every call site
/// growing an `if is_dbt { ... } else if is_sqlmesh { ... }` branch. The
/// git-native Baseline resolution logic itself (merge-base, temporary
/// worktree, capturing the compiled manifest) has nothing to do with dbt
/// at all -- only "how do I make this worktree's project directory produce
/// a fresh parseable artifact" does, and that's exactly what these methods
/// isolate.
pub trait TransformationToolAdapter {
    /// The error type this adapter can fail with while parsing, compiling,
    /// or installing dependencies.
    type Error: Error;

    /// A successful `compile`/`deps` run's captured stdout/stderr, for a
    /// caller that wants to route it somewhere (e.g. a daily run log).
    type CommandOutput;

    /// Parses the project's compiled state into zhao's internal
    /// representation.
    ///
    /// `path` points at whatever this adapter's tool produces as its
    /// compiled output (for dbt, a `manifest.json` file). Resolving that
    /// path in the first place -- e.g. running a tool's own compile step --
    /// is the caller's responsibility, not this method's.
    fn parse(&self, path: &Path) -> Result<ParsedProject, Self::Error>;

    /// Maps zhao's neutral vocabulary to this tool's own terms, for
    /// surfacing in user-facing output (a dbt user should see "model," not
    /// "Node").
    fn vocabulary(&self) -> &dyn AdapterVocabulary;

    /// Runs whatever this tool's own "produce a fresh compiled artifact"
    /// step is (dbt: `dbt compile`) in `project_dir`, so a subsequent
    /// `parse` call reflects the project's current state.
    ///
    /// `command` is the executable/prefix to invoke (ordinarily just the
    /// tool's own name, resolved via `PATH`) -- exposed as a parameter
    /// (rather than hardcoded) so tests can point it at a stub script
    /// instead of depending on a real install. `extra_args` are appended
    /// verbatim after the tool's own compile subcommand; zhao never
    /// interprets or validates these, the tool itself does.
    fn compile(
        &self,
        project_dir: &Path,
        command: &str,
        extra_args: &[String],
    ) -> Result<Self::CommandOutput, Self::Error>;

    /// Installs whatever package dependencies this tool's project format
    /// declares (dbt: `dbt deps`, reading `packages.yml`/`dependencies.yml`)
    /// -- needed before a first [`Self::compile`] somewhere those
    /// dependencies have never been installed (e.g. a freshly checked-out
    /// git worktree). See [`Self::compile`] for `command`/`extra_args`.
    fn deps(
        &self,
        project_dir: &Path,
        command: &str,
        extra_args: &[String],
    ) -> Result<Self::CommandOutput, Self::Error>;

    /// Builds a [`warehouse::QueryExecutor`] for `--check-relations`'s live
    /// relation-existence checks, using this tool's own way of running a
    /// one-off query/macro against whatever warehouse connection the
    /// project already has (dbt: `dbt run-operation`) -- never a
    /// connection zhao holds itself. See [`Self::compile`] for
    /// `command`/`extra_args`.
    fn query_executor<'a>(
        &self,
        project_dir: &'a Path,
        command: &'a str,
        extra_args: &'a [String],
    ) -> Box<dyn warehouse::QueryExecutor + 'a>;
}

/// The label mapping a [`TransformationToolAdapter`] owns from zhao's
/// neutral core terms to a transformation tool's own familiar words.
///
/// Applied only at user-facing surfaces (CLI output, reports); the
/// underlying data always uses zhao's neutral terms regardless of which
/// adapter produced it.
pub trait AdapterVocabulary {
    /// This tool's word for a [`crate::model::Node`] (e.g. "model" for dbt).
    fn node_term(&self) -> &'static str;

    /// This tool's word for an [`crate::model::Origin`] (e.g. "source" for dbt).
    fn origin_term(&self) -> &'static str;

    /// A ready-to-run command, in this tool's own selector syntax,
    /// recommending exactly which Nodes to validate -- `node_ids` are
    /// zhao's own qualified `NodeId` strings (e.g. a dbt model's
    /// `unique_id`, `model.<package>.<name>`); this tool derives its own
    /// selectable name for each from that string itself, rather than
    /// requiring a caller to look up a full `Node` first -- a Node
    /// reached only via the Baseline (e.g. one deleted entirely in the
    /// current state) may have no corresponding `Node` to look up at all,
    /// but its ID string is still enough to name it. `None` if `node_ids`
    /// is empty: there's nothing to recommend validating.
    fn recommended_validation_command(&self, node_ids: &[String]) -> Option<String>;

    /// Derives this tool's own selectable/display name for a single Node
    /// from its zhao `NodeId` string -- the same name
    /// [`AdapterVocabulary::recommended_validation_command`] uses for each
    /// Node it names, exposed separately so a caller building its own
    /// Node list (e.g. a `--defer` plan's build/defer sets) can render
    /// them the same way, without needing a full `Node` to look up either.
    fn node_display_name(&self, node_id: &str) -> String;
}

// ---------------------------------------------------------------------
// Auto-detection: which Transformation Tool Adapter applies to a given
// project directory. See the accepted decision this implements: detection
// by marker file first, `zhao.yml`'s `tool:` key only as a fallback for
// the undetectable case (no match, or more than one), never a general
// override of a detection that already succeeded.
// ---------------------------------------------------------------------

/// Recognizes whether a directory holds this adapter's kind of project, by
/// a marker file only that tool's own project format has (dbt:
/// `dbt_project.yml`).
///
/// A sibling trait to [`TransformationToolAdapter`], not a supertrait --
/// [`TransformationToolAdapter`] carries an associated `Error` type (so a
/// caller that already knows which concrete adapter it has can call
/// `compile`/`deps`/`parse` with a concrete error type to match on), which
/// would make a single `dyn TransformationToolAdapter` unable to represent
/// two different adapters' distinct error types at once. Detection doesn't
/// need any of that -- only a yes/no marker check -- so it stays on its
/// own dyn-compatible trait, letting [`resolve_tool_name`] hold a plain
/// `&[&dyn AdapterDetector]` registry regardless of how many concrete
/// adapters (and error types) exist.
pub trait AdapterDetector {
    /// This adapter's own name, as it should be written for `zhao.yml`'s
    /// `tool:` key (e.g. `"dbt"`).
    fn tool_name(&self) -> &'static str;

    /// Whether `project_dir` looks like this adapter's kind of project.
    fn detect(&self, project_dir: &Path) -> bool;
}

/// Every registered adapter's detector. Extending this list -- alongside
/// implementing [`TransformationToolAdapter`]/[`AdapterDetector`] for the
/// new adapter's own type -- is the whole "registry entry" a second
/// adapter needs; nothing in [`resolve_tool_name`] or any of its callers
/// changes.
fn registered_detectors() -> Vec<&'static dyn AdapterDetector> {
    vec![&dbt::DbtAdapter]
}

/// Resolves which Transformation Tool Adapter applies to `project_dir`,
/// per the registered [`AdapterDetector`]s and, as a fallback only,
/// `configured_tool` (`zhao.yml`'s `tool:` key). Returns the resolved
/// adapter's own [`AdapterDetector::tool_name`].
///
/// Resolution order:
/// 1. Try every registered adapter's detector against `project_dir`.
///    Exactly one match -> that adapter, `configured_tool` never even
///    consulted.
/// 2. No match, or more than one match (ambiguous) -> fall back to
///    `configured_tool`, if set.
/// 3. Still nothing -> a [`ToolResolutionError`] naming what was checked.
///
/// `configured_tool` is deliberately never consulted when step 1 already
/// produced a single answer -- it's only ever a fallback for the
/// undetectable case, never a general override of a successful detection.
pub fn resolve_tool_name(
    project_dir: &Path,
    configured_tool: Option<&str>,
) -> Result<&'static str, ToolResolutionError> {
    resolve_tool_name_among(&registered_detectors(), project_dir, configured_tool)
}

/// The actual resolution logic behind [`resolve_tool_name`], parameterized
/// over the detector list so it can be exercised with fake, test-only
/// detectors -- proving the ambiguous-match and registry-selection logic
/// honestly, not just "there's currently only one real adapter so it
/// always wins."
fn resolve_tool_name_among(
    detectors: &[&dyn AdapterDetector],
    project_dir: &Path,
    configured_tool: Option<&str>,
) -> Result<&'static str, ToolResolutionError> {
    let matched: Vec<&'static str> = detectors
        .iter()
        .filter(|detector| detector.detect(project_dir))
        .map(|detector| detector.tool_name())
        .collect();

    if let [only] = matched.as_slice() {
        return Ok(only);
    }

    if let Some(configured) = configured_tool {
        let valid: Vec<&'static str> = detectors.iter().map(|d| d.tool_name()).collect();
        return valid
            .iter()
            .find(|&&name| name == configured)
            .copied()
            .ok_or_else(|| ToolResolutionError::UnknownConfiguredTool {
                configured: configured.to_string(),
                valid: valid.join(", "),
            });
    }

    if matched.is_empty() {
        Err(ToolResolutionError::Undetectable {
            project_dir: project_dir.display().to_string(),
        })
    } else {
        Err(ToolResolutionError::Ambiguous {
            project_dir: project_dir.display().to_string(),
            matched: matched.join(", "),
        })
    }
}

/// Everything that can go wrong resolving which Transformation Tool
/// Adapter applies to a project directory. See [`resolve_tool_name`].
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ToolResolutionError {
    /// No registered adapter's marker matched, and `zhao.yml` sets no
    /// `tool:` key either.
    #[error(
        "could not determine this project's transformation tool -- no known project marker \
         was found in {project_dir}, and zhao.yml sets no 'tool:' key either. Set tool: dbt \
         (or whichever adapter applies) in zhao.yml to resolve this."
    )]
    Undetectable {
        /// The directory that was checked.
        project_dir: String,
    },
    /// More than one registered adapter's marker matched, and `zhao.yml`
    /// sets no `tool:` key to disambiguate.
    #[error(
        "could not determine this project's transformation tool -- more than one adapter's \
         project marker matched in {project_dir} ({matched}), and zhao.yml sets no 'tool:' key \
         to disambiguate. Set tool: <name> in zhao.yml to resolve this."
    )]
    Ambiguous {
        /// The directory that was checked.
        project_dir: String,
        /// Every adapter name whose marker matched, comma-separated.
        matched: String,
    },
    /// `zhao.yml` sets a `tool:` key, but its value isn't a name any
    /// registered adapter recognizes.
    #[error(
        "zhao.yml sets tool: {configured:?}, which isn't a transformation tool zhao recognizes \
         (expected one of: {valid})"
    )]
    UnknownConfiguredTool {
        /// The unrecognized `tool:` value.
        configured: String,
        /// Every valid tool name, comma-separated.
        valid: String,
    },
}

#[cfg(test)]
mod resolution_tests {
    use super::*;

    /// A detector that always/never matches, purely for exercising
    /// [`resolve_tool_name_among`]'s own logic in isolation -- not a real
    /// [`TransformationToolAdapter`] implementation, since the resolution
    /// order (auto-detect first, `tool:` fallback, then a clear error)
    /// doesn't depend on anything else the trait carries.
    struct FakeDetector {
        name: &'static str,
        matches: bool,
    }

    impl AdapterDetector for FakeDetector {
        fn tool_name(&self) -> &'static str {
            self.name
        }

        fn detect(&self, _project_dir: &Path) -> bool {
            self.matches
        }
    }

    #[test]
    fn exactly_one_match_resolves_to_it_without_consulting_configured_tool() {
        let dbt = FakeDetector {
            name: "dbt",
            matches: true,
        };
        let sqlmesh = FakeDetector {
            name: "sqlmesh",
            matches: false,
        };
        let detectors: Vec<&dyn AdapterDetector> = vec![&dbt, &sqlmesh];

        // A `configured_tool` that would resolve to something else
        // entirely, to prove it's never even consulted when detection is
        // unambiguous.
        let resolved =
            resolve_tool_name_among(&detectors, Path::new("/does/not/matter"), Some("sqlmesh"))
                .expect("should resolve");
        assert_eq!(resolved, "dbt");
    }

    #[test]
    fn no_match_falls_back_to_configured_tool() {
        let dbt = FakeDetector {
            name: "dbt",
            matches: false,
        };
        let sqlmesh = FakeDetector {
            name: "sqlmesh",
            matches: false,
        };
        let detectors: Vec<&dyn AdapterDetector> = vec![&dbt, &sqlmesh];

        let resolved =
            resolve_tool_name_among(&detectors, Path::new("/does/not/matter"), Some("sqlmesh"))
                .expect("should resolve via the configured fallback");
        assert_eq!(resolved, "sqlmesh");
    }

    #[test]
    fn more_than_one_match_falls_back_to_configured_tool() {
        let dbt = FakeDetector {
            name: "dbt",
            matches: true,
        };
        let sqlmesh = FakeDetector {
            name: "sqlmesh",
            matches: true,
        };
        let detectors: Vec<&dyn AdapterDetector> = vec![&dbt, &sqlmesh];

        let resolved =
            resolve_tool_name_among(&detectors, Path::new("/does/not/matter"), Some("dbt"))
                .expect("should resolve via the configured fallback");
        assert_eq!(resolved, "dbt");
    }

    #[test]
    fn no_match_and_no_configured_tool_produces_an_undetectable_error() {
        let dbt = FakeDetector {
            name: "dbt",
            matches: false,
        };
        let detectors: Vec<&dyn AdapterDetector> = vec![&dbt];

        let err = resolve_tool_name_among(&detectors, Path::new("/some/project"), None)
            .expect_err("should fail");
        assert_eq!(
            err,
            ToolResolutionError::Undetectable {
                project_dir: "/some/project".to_string(),
            }
        );
        assert!(err.to_string().contains("tool:"), "{err}");
    }

    #[test]
    fn ambiguous_match_and_no_configured_tool_produces_an_ambiguous_error() {
        let dbt = FakeDetector {
            name: "dbt",
            matches: true,
        };
        let sqlmesh = FakeDetector {
            name: "sqlmesh",
            matches: true,
        };
        let detectors: Vec<&dyn AdapterDetector> = vec![&dbt, &sqlmesh];

        let err = resolve_tool_name_among(&detectors, Path::new("/some/project"), None)
            .expect_err("should fail");
        assert!(
            matches!(err, ToolResolutionError::Ambiguous { .. }),
            "{err}"
        );
    }

    #[test]
    fn an_unrecognized_configured_tool_produces_a_clear_error() {
        let dbt = FakeDetector {
            name: "dbt",
            matches: false,
        };
        let detectors: Vec<&dyn AdapterDetector> = vec![&dbt];

        let err = resolve_tool_name_among(&detectors, Path::new("/some/project"), Some("sqlmesh"))
            .expect_err("should fail");
        assert_eq!(
            err,
            ToolResolutionError::UnknownConfiguredTool {
                configured: "sqlmesh".to_string(),
                valid: "dbt".to_string(),
            }
        );
    }

    #[test]
    fn the_real_registry_resolves_dbt_when_a_dbt_project_yml_marker_is_present() {
        let dir = tempfile::tempdir().expect("should create temp dir");
        std::fs::write(dir.path().join("dbt_project.yml"), "name: fixture\n")
            .expect("should write marker file");

        let resolved =
            resolve_tool_name(dir.path(), None).expect("should resolve via the real registry");
        assert_eq!(resolved, "dbt");
    }

    #[test]
    fn the_real_registry_fails_clearly_with_no_marker_and_no_configured_tool() {
        let dir = tempfile::tempdir().expect("should create temp dir");

        let err = resolve_tool_name(dir.path(), None).expect_err("should fail");
        assert!(
            matches!(err, ToolResolutionError::Undetectable { .. }),
            "{err}"
        );
    }
}
