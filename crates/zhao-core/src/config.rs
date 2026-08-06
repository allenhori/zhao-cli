//! Project configuration (`zhao.yml`): a named Preset plus any per-Rule
//! Severity overrides layered on top. Preferred over CLI flags so a
//! team's policy is versioned and reviewable in the project itself,
//! rather than hidden in a CI script.
//!
//! `zhao.yml` is optional -- a project with none behaves identically to
//! zhao's hardcoded v1 defaults (see [`crate::rules::RuleId::default_severity`]).
//!
//! In a monorepo with multiple dbt projects, [`Config::load_for_project`]
//! cascades a root-level `zhao.yml` down to a project-local one: each
//! directory from the repo root (the nearest ancestor containing `.git`)
//! down to the dbt project directory may have its own `zhao.yml`, and a
//! project-local value wins over a root value for the same key -- the same
//! override relationship a Preset already has to individual Rule overrides,
//! one layer higher.
//!
//! Uses `serde_yaml`, which is archived upstream (no longer actively
//! developed) but remains functionally complete and widely used; YAML
//! itself isn't a moving target, so this is a deliberate, accepted
//! dependency choice rather than an oversight.

use crate::rules::{RuleId, Severity};
use serde::Deserialize;
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

/// A named bundle of default Severities across the whole Rule catalog,
/// applied before any per-Rule override in `zhao.yml`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Preset {
    /// zhao's built-in per-Rule defaults, unchanged.
    #[default]
    Default,
    /// Every Rule's default Severity raised one level: `Pass` -> `Warn`,
    /// `Warn` -> `Error`. `Error` stays `Error` (already the strictest).
    Strict,
    /// Every Rule's default Severity lowered one level: `Error` -> `Warn`,
    /// `Warn` -> `Pass`. `Pass` stays `Pass` (already the most lenient).
    Lenient,
}

impl Preset {
    fn severity_for(self, rule: RuleId) -> Severity {
        let base = rule.default_severity();
        match self {
            Preset::Default => base,
            Preset::Strict => match base {
                Severity::Pass => Severity::Warn,
                Severity::Warn | Severity::Error => Severity::Error,
            },
            Preset::Lenient => match base {
                Severity::Error => Severity::Warn,
                Severity::Warn | Severity::Pass => Severity::Pass,
            },
        }
    }

    fn from_config_name(name: &str) -> Option<Preset> {
        match name {
            "default" => Some(Preset::Default),
            "strict" => Some(Preset::Strict),
            "lenient" => Some(Preset::Lenient),
            _ => None,
        }
    }
}

/// `zhao.yml`'s `log.level` (and a CLI override) -- see issue #35. Both
/// variants currently produce identical (mirror-only) run-log content;
/// this exists so a later ticket can add real `Debug`-level content
/// without another config-shape change, not because `Debug` does
/// anything different yet.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum LogLevel {
    /// A literal mirror of whatever was already printed to stdout --
    /// the only content this ticket actually produces.
    #[default]
    Mirror,
    /// Reserved for a later ticket's richer, internal-only content.
    Debug,
}

impl LogLevel {
    fn from_config_name(name: &str) -> Option<LogLevel> {
        match name {
            "mirror" => Some(LogLevel::Mirror),
            "debug" => Some(LogLevel::Debug),
            _ => None,
        }
    }
}

/// zhao's project configuration: a [`Preset`] plus any per-Rule
/// [`Severity`] overrides layered on top, plus the `--defer` target/state
/// zhao's ready-to-run `--defer --state <path>` command generation needs
/// (see [`Config::defer_target`]/[`Config::defer_state`]), plus the run
/// log's configured verbosity (see [`Config::log_level`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    preset: Preset,
    overrides: HashMap<RuleId, Severity>,
    defer_target: Option<String>,
    defer_state: Option<String>,
    against: Option<String>,
    log_level: LogLevel,
    log_retention_days: Option<u32>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            preset: Preset::Default,
            overrides: HashMap::new(),
            defer_target: None,
            defer_state: None,
            against: None,
            log_level: LogLevel::default(),
            log_retention_days: None,
        }
    }
}

impl Config {
    /// The Severity a Rule should use: a per-Rule override if one is
    /// configured, otherwise the configured Preset's value for it.
    pub fn severity_for(&self, rule: RuleId) -> Severity {
        self.overrides
            .get(&rule)
            .copied()
            .unwrap_or_else(|| self.preset.severity_for(rule))
    }

    /// The configured name of the dbt target being deferred to (e.g.
    /// `"prod"`), if any -- purely a human-readable label surfaced
    /// alongside the generated `--defer` command, not itself passed as a
    /// `--target` flag (dbt's `--defer` mechanism only needs
    /// [`Config::defer_state`]'s path; this just documents what that
    /// path represents). `None` if `zhao.yml` sets no `defer.target` at
    /// any level of the cascade.
    pub fn defer_target(&self) -> Option<&str> {
        self.defer_target.as_deref()
    }

    /// The configured path to a compiled manifest to defer to, passed as
    /// `dbt ... --defer --state <path>` in the generated command. `None`
    /// if `zhao.yml` sets no `defer.state` at any level of the cascade
    /// (in which case no ready-to-run `--defer` command is generated at
    /// all -- that's a `zhao-cli`-side decision, not this crate's).
    pub fn defer_state(&self) -> Option<&str> {
        self.defer_state.as_deref()
    }

    /// The configured ref to resolve a git-native Baseline's merge-base
    /// against (e.g. `"main"`), if `zhao.yml` sets one. `None` if it
    /// doesn't -- callers should fall back to zhao's own default
    /// (`"master"`) the same way they already fall back when neither
    /// this nor a CLI override is given.
    pub fn against(&self) -> Option<&str> {
        self.against.as_deref()
    }

    /// The configured run-log verbosity (see [`LogLevel`]) -- defaults to
    /// [`LogLevel::Mirror`] if `zhao.yml` sets no `log.level` at any level
    /// of the cascade.
    pub fn log_level(&self) -> LogLevel {
        self.log_level
    }

    /// The configured run-log retention window, in days -- see issue
    /// #37. `None` (the default, if `zhao.yml` sets no
    /// `log.retention_days` at any level of the cascade) means "keep
    /// everything, purge nothing," matching the assumption that most
    /// environments running zhao are disposable anyway. `Some(n)` means
    /// purge log files older than `n` days on every run.
    pub fn log_retention_days(&self) -> Option<u32> {
        self.log_retention_days
    }

    /// Loads a single `zhao.yml` from the given path. Returns
    /// [`Config::default`] (unchanged v1 defaults) if the file doesn't
    /// exist -- `zhao.yml` is optional, not mandatory.
    ///
    /// For a monorepo with multiple dbt projects, prefer
    /// [`Config::load_for_project`], which also cascades in any root-level
    /// `zhao.yml`.
    pub fn load(path: &Path) -> Result<Config, ConfigError> {
        Ok(ConfigLayer::load(path)?.into_config())
    }

    /// Loads the effective `zhao.yml` for a dbt project directory,
    /// cascading a root-level `zhao.yml` down to a project-local one.
    ///
    /// Walks upward from `project_dir` to the repo root -- the nearest
    /// ancestor directory containing a `.git` entry, inclusive -- and reads
    /// any `zhao.yml` found at each level along the way. Values are merged
    /// root-to-leaf: a project-local Preset or per-Rule override wins over
    /// the same key set at the root, but a root value still applies for any
    /// key the project-local file leaves unset. If no `.git` is found (the
    /// project isn't inside a git repository), this behaves identically to
    /// [`Config::load`] on `project_dir`'s own `zhao.yml`.
    pub fn load_for_project(project_dir: &Path) -> Result<Config, ConfigError> {
        let mut layer = ConfigLayer::default();
        for dir in ancestor_dirs_from_repo_root(project_dir) {
            layer = ConfigLayer::load(&dir.join("zhao.yml"))?.onto(layer);
        }
        Ok(layer.into_config())
    }
}

/// The directories to read a possible `zhao.yml` from, root-most first:
/// every ancestor of `start` up to and including the nearest directory
/// containing a `.git` entry, or just `start` alone if no `.git` is found
/// before reaching the filesystem root.
fn ancestor_dirs_from_repo_root(start: &Path) -> Vec<PathBuf> {
    let mut chain = vec![start.to_path_buf()];
    let mut current = start.to_path_buf();

    loop {
        if current.join(".git").exists() {
            chain.reverse();
            return chain;
        }
        match current.parent() {
            Some(parent) => {
                current = parent.to_path_buf();
                chain.push(current.clone());
            }
            None => return vec![start.to_path_buf()],
        }
    }
}

/// One `zhao.yml`'s worth of settings, parsed but not yet merged with any
/// other layer -- unlike [`Config`], a field left unset in the file stays
/// `None` here rather than falling back to a default, so a later
/// [`ConfigLayer::onto`] call can tell "not set at this level" apart from
/// "explicitly set to the default."
#[derive(Debug, Default)]
struct ConfigLayer {
    preset: Option<Preset>,
    overrides: HashMap<RuleId, Severity>,
    defer_target: Option<String>,
    defer_state: Option<String>,
    against: Option<String>,
    log_level: Option<LogLevel>,
    log_retention_days: Option<u32>,
}

impl ConfigLayer {
    /// Parses the `zhao.yml` at `path` into a layer. A missing file
    /// produces an empty layer (nothing set), not an error.
    fn load(path: &Path) -> Result<ConfigLayer, ConfigError> {
        if !path.exists() {
            return Ok(ConfigLayer::default());
        }

        let raw = fs::read_to_string(path).map_err(|source| ConfigError::Io {
            path: path.display().to_string(),
            source,
        })?;
        let parsed: RawConfig =
            serde_yaml::from_str(&raw).map_err(|source| ConfigError::InvalidYaml {
                path: path.display().to_string(),
                source,
            })?;
        parsed.into_layer(path)
    }

    /// Layers `self` (the more specific, e.g. project-local, settings) on
    /// top of `base` (the less specific, e.g. root, settings): `self`'s
    /// Preset wins if set, otherwise `base`'s Preset is kept; overrides are
    /// merged key-by-key with `self`'s entries winning on conflict.
    fn onto(self, base: ConfigLayer) -> ConfigLayer {
        let mut overrides = base.overrides;
        overrides.extend(self.overrides);
        ConfigLayer {
            preset: self.preset.or(base.preset),
            overrides,
            defer_target: self.defer_target.or(base.defer_target),
            defer_state: self.defer_state.or(base.defer_state),
            against: self.against.or(base.against),
            log_level: self.log_level.or(base.log_level),
            log_retention_days: self.log_retention_days.or(base.log_retention_days),
        }
    }

    fn into_config(self) -> Config {
        Config {
            preset: self.preset.unwrap_or_default(),
            overrides: self.overrides,
            defer_target: self.defer_target,
            defer_state: self.defer_state,
            against: self.against,
            log_level: self.log_level.unwrap_or_default(),
            log_retention_days: self.log_retention_days,
        }
    }
}

/// Every valid `zhao.yml` rule name, comma-separated, for an "unknown
/// rule" error message -- delegates to `RuleId::all()` so this can't drift
/// out of sync with the actual Rule catalog.
fn valid_rule_names() -> String {
    RuleId::all()
        .iter()
        .map(|rule| rule.config_name())
        .collect::<Vec<_>>()
        .join(", ")
}

#[derive(Debug, Default, Deserialize)]
struct RawConfig {
    #[serde(default)]
    preset: Option<String>,
    #[serde(default)]
    rules: HashMap<String, String>,
    #[serde(default)]
    defer: Option<RawDeferConfig>,
    #[serde(default)]
    against: Option<String>,
    #[serde(default)]
    log: Option<RawLogConfig>,
}

/// The `log:` section of `zhao.yml` -- see [`Config::log_level`].
#[derive(Debug, Default, Deserialize)]
struct RawLogConfig {
    #[serde(default)]
    level: Option<String>,
    #[serde(default)]
    retention_days: Option<u32>,
}

/// The `defer:` section of `zhao.yml` -- see
/// [`Config::defer_target`]/[`Config::defer_state`].
#[derive(Debug, Default, Deserialize)]
struct RawDeferConfig {
    #[serde(default)]
    target: Option<String>,
    #[serde(default)]
    state: Option<String>,
}

impl RawConfig {
    fn into_layer(self, path: &Path) -> Result<ConfigLayer, ConfigError> {
        let preset =
            match self.preset {
                None => None,
                Some(name) => Some(Preset::from_config_name(&name).ok_or_else(|| {
                    ConfigError::UnknownPreset {
                        path: path.display().to_string(),
                        name: name.clone(),
                    }
                })?),
            };

        let mut overrides = HashMap::new();
        for (rule_name, severity_name) in self.rules {
            let rule =
                RuleId::from_config_name(&rule_name).ok_or_else(|| ConfigError::UnknownRule {
                    path: path.display().to_string(),
                    name: rule_name.clone(),
                    valid: valid_rule_names(),
                })?;
            let severity = Severity::from_config_name(&severity_name).ok_or_else(|| {
                ConfigError::InvalidSeverity {
                    path: path.display().to_string(),
                    rule: rule_name.clone(),
                    value: severity_name.clone(),
                }
            })?;
            overrides.insert(rule, severity);
        }

        let (defer_target, defer_state) = match self.defer {
            None => (None, None),
            Some(defer) => (defer.target, defer.state),
        };

        let (log_level_name, log_retention_days) = match self.log {
            None => (None, None),
            Some(log) => (log.level, log.retention_days),
        };
        let log_level = match log_level_name {
            None => None,
            Some(name) => Some(LogLevel::from_config_name(&name).ok_or_else(|| {
                ConfigError::InvalidLogLevel {
                    path: path.display().to_string(),
                    value: name.clone(),
                }
            })?),
        };

        Ok(ConfigLayer {
            preset,
            overrides,
            defer_target,
            defer_state,
            against: self.against,
            log_level,
            log_retention_days,
        })
    }
}

/// Everything that can go wrong while reading and parsing `zhao.yml`.
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    /// The file exists but couldn't be read from disk.
    #[error("could not read {path}: {source}")]
    Io {
        /// The path that couldn't be read.
        path: String,
        /// The underlying I/O error.
        #[source]
        source: std::io::Error,
    },
    /// The file's contents weren't valid YAML.
    #[error("could not parse {path} as YAML: {source}")]
    InvalidYaml {
        /// The path whose contents failed to parse.
        path: String,
        /// The underlying YAML error.
        #[source]
        source: serde_yaml::Error,
    },
    /// The configured `preset` isn't one zhao recognizes.
    #[error("{path}: unknown preset {name:?} (expected one of: default, strict, lenient)")]
    UnknownPreset {
        /// The file the unknown preset was declared in.
        path: String,
        /// The unrecognized preset name.
        name: String,
    },
    /// A key under `rules:` isn't a Rule zhao recognizes.
    #[error("{path}: unknown rule {name:?} (expected one of: {valid})")]
    UnknownRule {
        /// The file the unknown rule was declared in.
        path: String,
        /// The unrecognized rule name.
        name: String,
        /// Every valid rule name, comma-separated.
        valid: String,
    },
    /// A value under `rules:` isn't a Severity zhao recognizes.
    #[error(
        "{path}: rule {rule:?} has invalid severity {value:?} (expected one of: error, warn, pass)"
    )]
    InvalidSeverity {
        /// The file the invalid severity was declared in.
        path: String,
        /// The rule the invalid severity was assigned to.
        rule: String,
        /// The unrecognized severity value.
        value: String,
    },
    /// `log.level` isn't a value zhao recognizes.
    #[error("{path}: log.level {value:?} is not valid (expected one of: mirror, debug)")]
    InvalidLogLevel {
        /// The file the invalid log level was declared in.
        path: String,
        /// The unrecognized log level value.
        value: String,
    },
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    /// Writes `contents` to a guaranteed-unique temporary file (via
    /// `tempfile`, not a hand-rolled name) and returns the open handle --
    /// keep it bound for as long as the path is needed; the file is
    /// deleted automatically when the handle drops. A hand-rolled "unique"
    /// name (e.g. a timestamp) is exactly the kind of thing that looks
    /// fine until parallel test execution collides two tests on the same
    /// path -- `tempfile` exists specifically to avoid that failure mode.
    fn write_temp_yaml(contents: &str) -> tempfile::NamedTempFile {
        let mut file = tempfile::NamedTempFile::new().expect("should create temp file");
        file.write_all(contents.as_bytes())
            .expect("should write temp file");
        file
    }

    #[test]
    fn missing_file_produces_the_default_config() {
        let config =
            Config::load(Path::new("/nonexistent/zhao.yml")).expect("missing file should be ok");
        assert_eq!(
            config.severity_for(RuleId::ColumnTypeNarrowed),
            Severity::Warn
        );
        assert_eq!(config.severity_for(RuleId::ColumnAdded), Severity::Pass);
    }

    #[test]
    fn strict_preset_raises_warn_rules_to_error() {
        let file = write_temp_yaml("preset: strict\n");
        let config = Config::load(file.path()).expect("should parse");

        assert_eq!(
            config.severity_for(RuleId::ColumnTypeNarrowed),
            Severity::Error
        );
        assert_eq!(
            config.severity_for(RuleId::JoinCardinalityLoosened),
            Severity::Error
        );
        assert_eq!(
            config.severity_for(RuleId::ColumnRemovedWithActiveReferences),
            Severity::Error
        );
    }

    #[test]
    fn lenient_preset_lowers_error_rules_to_warn() {
        let file = write_temp_yaml("preset: lenient\n");
        let config = Config::load(file.path()).expect("should parse");

        assert_eq!(
            config.severity_for(RuleId::ColumnRemovedWithActiveReferences),
            Severity::Warn
        );
        assert_eq!(
            config.severity_for(RuleId::ColumnTypeNarrowed),
            Severity::Pass
        );
    }

    #[test]
    fn per_rule_override_wins_over_the_preset_for_that_rule_only() {
        let file = write_temp_yaml("preset: strict\nrules:\n  column-added: warn\n");
        let config = Config::load(file.path()).expect("should parse");

        // Overridden.
        assert_eq!(config.severity_for(RuleId::ColumnAdded), Severity::Warn);
        // Untouched -- still follows the strict Preset.
        assert_eq!(
            config.severity_for(RuleId::ColumnTypeNarrowed),
            Severity::Error
        );
    }

    #[test]
    fn unknown_preset_produces_a_clear_error() {
        let file = write_temp_yaml("preset: extreme\n");
        let result = Config::load(file.path());

        assert!(matches!(result, Err(ConfigError::UnknownPreset { .. })));
    }

    /// Issue #35's acceptance criterion: `zhao.yml`'s `log.level` key is
    /// accepted and parsed. Defaults to `Mirror` when unset.
    #[test]
    fn log_level_defaults_to_mirror_when_unset() {
        let config = Config::load(Path::new("/nonexistent/zhao.yml")).expect("should be ok");
        assert_eq!(config.log_level(), LogLevel::Mirror);
    }

    #[test]
    fn log_level_debug_is_parsed_from_zhao_yml() {
        let file = write_temp_yaml("log:\n  level: debug\n");
        let config = Config::load(file.path()).expect("should parse");
        assert_eq!(config.log_level(), LogLevel::Debug);
    }

    #[test]
    fn an_unrecognized_log_level_produces_a_clear_error() {
        let file = write_temp_yaml("log:\n  level: verbose\n");
        let result = Config::load(file.path());
        assert!(matches!(result, Err(ConfigError::InvalidLogLevel { .. })));
    }

    /// Issue #37's acceptance criterion: no purging happens (unset)
    /// unless explicitly configured -- the default remains "keep
    /// everything."
    #[test]
    fn log_retention_days_defaults_to_none_when_unset() {
        let config = Config::load(Path::new("/nonexistent/zhao.yml")).expect("should be ok");
        assert_eq!(config.log_retention_days(), None);
    }

    #[test]
    fn log_retention_days_is_parsed_from_zhao_yml() {
        let file = write_temp_yaml("log:\n  retention_days: 14\n");
        let config = Config::load(file.path()).expect("should parse");
        assert_eq!(config.log_retention_days(), Some(14));
    }

    /// `log.level` and `log.retention_days` are independent keys under
    /// the same `log:` section -- setting one doesn't require or clear
    /// the other.
    #[test]
    fn log_level_and_retention_days_are_independent() {
        let file = write_temp_yaml("log:\n  level: debug\n  retention_days: 7\n");
        let config = Config::load(file.path()).expect("should parse");
        assert_eq!(config.log_level(), LogLevel::Debug);
        assert_eq!(config.log_retention_days(), Some(7));
    }

    #[test]
    fn unknown_rule_name_produces_a_clear_error() {
        let file = write_temp_yaml("rules:\n  not-a-real-rule: error\n");
        let result = Config::load(file.path());

        assert!(matches!(result, Err(ConfigError::UnknownRule { .. })));
    }

    #[test]
    fn invalid_severity_value_produces_a_clear_error() {
        let file = write_temp_yaml("rules:\n  column-added: catastrophic\n");
        let result = Config::load(file.path());

        assert!(matches!(result, Err(ConfigError::InvalidSeverity { .. })));
    }

    #[test]
    fn malformed_yaml_produces_a_clear_error() {
        let file = write_temp_yaml("preset: [this is not\n  valid: yaml structure for us");
        let result = Config::load(file.path());

        assert!(matches!(result, Err(ConfigError::InvalidYaml { .. })));
    }

    /// Builds a fake repo under a fresh temp dir: a `.git` marker at the
    /// root, and a nested dbt project directory a few levels down --
    /// mirroring a real monorepo's shape closely enough to exercise
    /// [`ancestor_dirs_from_repo_root`] and [`Config::load_for_project`]
    /// without needing an actual git repository.
    struct FakeRepo {
        _dir: tempfile::TempDir,
        root: PathBuf,
        project_dir: PathBuf,
    }

    fn fake_repo() -> FakeRepo {
        let dir = tempfile::tempdir().expect("should create temp dir");
        let root = dir.path().to_path_buf();
        fs::create_dir_all(root.join(".git")).expect("should create .git marker");
        let project_dir = root.join("services").join("analytics");
        fs::create_dir_all(&project_dir).expect("should create nested project dir");
        FakeRepo {
            _dir: dir,
            root,
            project_dir,
        }
    }

    #[test]
    fn root_only_zhao_yml_applies_to_a_nested_dbt_project() {
        let repo = fake_repo();
        fs::write(repo.root.join("zhao.yml"), "preset: strict\n")
            .expect("should write root config");

        let config = Config::load_for_project(&repo.project_dir).expect("should parse");

        assert_eq!(
            config.severity_for(RuleId::ColumnTypeNarrowed),
            Severity::Error
        );
    }

    #[test]
    fn project_local_zhao_yml_wins_per_key_root_still_applies_for_the_rest() {
        let repo = fake_repo();
        fs::write(
            repo.root.join("zhao.yml"),
            "preset: strict\nrules:\n  column-added: warn\n",
        )
        .expect("should write root config");
        fs::write(
            repo.project_dir.join("zhao.yml"),
            "rules:\n  column-added: pass\n",
        )
        .expect("should write project-local config");

        let config = Config::load_for_project(&repo.project_dir).expect("should parse");

        // Project-local override wins for the key it sets.
        assert_eq!(config.severity_for(RuleId::ColumnAdded), Severity::Pass);
        // Root's Preset still applies for every key the project-local file
        // doesn't touch, including the Preset itself.
        assert_eq!(
            config.severity_for(RuleId::ColumnTypeNarrowed),
            Severity::Error
        );
    }

    /// Proves the cascade is genuinely N-level, not hardcoded to exactly
    /// "root + project": a `zhao.yml` at the intermediate directory
    /// (`root/services`, between the repo root and the leaf project dir)
    /// must win over the root's value for the key it sets, while the
    /// leaf's own `zhao.yml` still wins over that intermediate value for
    /// the key *it* sets, and the root's value keeps applying for the key
    /// neither the mid nor the leaf file touches.
    #[test]
    fn a_zhao_yml_at_an_intermediate_directory_cascades_correctly() {
        let repo = fake_repo();
        let mid_dir = repo.project_dir.parent().expect("project dir has a parent");

        fs::write(
            repo.root.join("zhao.yml"),
            "preset: strict\nrules:\n  column-added: warn\n",
        )
        .expect("should write root config");
        fs::write(
            mid_dir.join("zhao.yml"),
            "rules:\n  column-added: pass\n  column-type-narrowed: pass\n",
        )
        .expect("should write intermediate config");
        fs::write(
            repo.project_dir.join("zhao.yml"),
            "rules:\n  column-type-narrowed: warn\n",
        )
        .expect("should write project-local config");

        let config = Config::load_for_project(&repo.project_dir).expect("should parse");

        // Leaf wins over the intermediate directory's value for the key
        // the leaf sets.
        assert_eq!(
            config.severity_for(RuleId::ColumnTypeNarrowed),
            Severity::Warn
        );
        // Intermediate directory's value wins over root for the key only
        // the intermediate file sets (the leaf never touches this key).
        assert_eq!(config.severity_for(RuleId::ColumnAdded), Severity::Pass);
        // Root's Preset still applies for a key untouched at every more
        // specific level.
        assert_eq!(
            config.severity_for(RuleId::JoinCardinalityLoosened),
            Severity::Error
        );
    }

    #[test]
    fn project_local_preset_wins_over_root_preset() {
        let repo = fake_repo();
        fs::write(repo.root.join("zhao.yml"), "preset: strict\n")
            .expect("should write root config");
        fs::write(repo.project_dir.join("zhao.yml"), "preset: lenient\n")
            .expect("should write project-local config");

        let config = Config::load_for_project(&repo.project_dir).expect("should parse");

        assert_eq!(
            config.severity_for(RuleId::ColumnRemovedWithActiveReferences),
            Severity::Warn
        );
    }

    #[test]
    fn a_single_project_repo_behaves_exactly_like_loading_its_own_zhao_yml() {
        let repo = fake_repo();
        fs::write(
            repo.project_dir.join("zhao.yml"),
            "preset: strict\nrules:\n  column-added: warn\n",
        )
        .expect("should write project config");

        let via_project = Config::load_for_project(&repo.project_dir).expect("should parse");
        let via_direct_path =
            Config::load(&repo.project_dir.join("zhao.yml")).expect("should parse");

        assert_eq!(via_project, via_direct_path);
    }

    #[test]
    fn no_zhao_yml_anywhere_in_the_chain_produces_the_default_config() {
        let repo = fake_repo();

        let config = Config::load_for_project(&repo.project_dir).expect("should parse");

        assert_eq!(config, Config::default());
    }

    #[test]
    fn without_a_git_repo_only_the_project_dirs_own_zhao_yml_is_read() {
        // No `.git` anywhere -- `ancestor_dirs_from_repo_root` should fall
        // back to just the project dir itself, not walk arbitrarily far up
        // the real filesystem the test happens to run on.
        let dir = tempfile::tempdir().expect("should create temp dir");
        let parent_with_no_git = dir.path().join("not_a_repo_root");
        let project_dir = parent_with_no_git.join("dbt_project");
        fs::create_dir_all(&project_dir).expect("should create nested dir");
        fs::write(parent_with_no_git.join("zhao.yml"), "preset: strict\n")
            .expect("should write a decoy config one level up");
        fs::write(project_dir.join("zhao.yml"), "preset: lenient\n")
            .expect("should write the project's own config");

        let config = Config::load_for_project(&project_dir).expect("should parse");

        // Only `lenient` (the project's own file) should apply -- the
        // decoy one level up must be ignored since no `.git` ties it to
        // this project.
        assert_eq!(
            config.severity_for(RuleId::ColumnRemovedWithActiveReferences),
            Severity::Warn
        );
    }

    #[test]
    fn missing_defer_section_leaves_both_fields_unset() {
        let config = Config::load(Path::new("/nonexistent/zhao.yml")).expect("should be ok");
        assert_eq!(config.defer_target(), None);
        assert_eq!(config.defer_state(), None);
    }

    #[test]
    fn defer_target_and_state_are_read_from_zhao_yml() {
        let file =
            write_temp_yaml("defer:\n  target: prod\n  state: artifacts/prod/manifest.json\n");
        let config = Config::load(file.path()).expect("should parse");

        assert_eq!(config.defer_target(), Some("prod"));
        assert_eq!(config.defer_state(), Some("artifacts/prod/manifest.json"));
    }

    #[test]
    fn defer_section_can_set_only_one_of_target_or_state() {
        let file = write_temp_yaml("defer:\n  state: artifacts/prod/manifest.json\n");
        let config = Config::load(file.path()).expect("should parse");

        assert_eq!(config.defer_target(), None);
        assert_eq!(config.defer_state(), Some("artifacts/prod/manifest.json"));
    }

    #[test]
    fn project_local_defer_config_wins_over_root_per_key() {
        let repo = fake_repo();
        fs::write(
            repo.root.join("zhao.yml"),
            "defer:\n  target: prod\n  state: artifacts/root/manifest.json\n",
        )
        .expect("should write root config");
        fs::write(
            repo.project_dir.join("zhao.yml"),
            "defer:\n  state: artifacts/project/manifest.json\n",
        )
        .expect("should write project-local config");

        let config = Config::load_for_project(&repo.project_dir).expect("should parse");

        // Project-local wins for the key it sets.
        assert_eq!(
            config.defer_state(),
            Some("artifacts/project/manifest.json")
        );
        // Root's value still applies for the key the project-local file
        // doesn't touch.
        assert_eq!(config.defer_target(), Some("prod"));
    }

    #[test]
    fn missing_against_leaves_it_unset() {
        let config = Config::load(Path::new("/nonexistent/zhao.yml")).expect("should be ok");
        assert_eq!(config.against(), None);
    }

    #[test]
    fn against_is_read_from_zhao_yml() {
        let file = write_temp_yaml("against: main\n");
        let config = Config::load(file.path()).expect("should parse");

        assert_eq!(config.against(), Some("main"));
    }

    #[test]
    fn project_local_against_wins_over_root() {
        let repo = fake_repo();
        fs::write(repo.root.join("zhao.yml"), "against: main\n").expect("should write root config");
        fs::write(repo.project_dir.join("zhao.yml"), "against: develop\n")
            .expect("should write project-local config");

        let config = Config::load_for_project(&repo.project_dir).expect("should parse");

        assert_eq!(config.against(), Some("develop"));
    }
}
