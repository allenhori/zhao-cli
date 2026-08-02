//! Project configuration (`zhao.yml`): a named Preset plus any per-Rule
//! Severity overrides layered on top. Preferred over CLI flags so a
//! team's policy is versioned and reviewable in the project itself,
//! rather than hidden in a CI script.
//!
//! `zhao.yml` is optional -- a project with none behaves identically to
//! zhao's hardcoded v1 defaults (see [`crate::rules::RuleId::default_severity`]).
//!
//! Uses `serde_yaml`, which is archived upstream (no longer actively
//! developed) but remains functionally complete and widely used; YAML
//! itself isn't a moving target, so this is a deliberate, accepted
//! dependency choice rather than an oversight.

use crate::rules::{RuleId, Severity};
use serde::Deserialize;
use std::collections::HashMap;
use std::fs;
use std::path::Path;

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

/// zhao's project configuration: a [`Preset`] plus any per-Rule
/// [`Severity`] overrides layered on top.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    preset: Preset,
    overrides: HashMap<RuleId, Severity>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            preset: Preset::Default,
            overrides: HashMap::new(),
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

    /// Loads `zhao.yml` from the given path. Returns [`Config::default`]
    /// (unchanged v1 defaults) if the file doesn't exist -- `zhao.yml` is
    /// optional, not mandatory.
    pub fn load(path: &Path) -> Result<Config, ConfigError> {
        if !path.exists() {
            return Ok(Config::default());
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
        parsed.into_config(path)
    }
}

#[derive(Debug, Default, Deserialize)]
struct RawConfig {
    #[serde(default)]
    preset: Option<String>,
    #[serde(default)]
    rules: HashMap<String, String>,
}

impl RawConfig {
    fn into_config(self, path: &Path) -> Result<Config, ConfigError> {
        let preset = match self.preset {
            None => Preset::default(),
            Some(name) => {
                Preset::from_config_name(&name).ok_or_else(|| ConfigError::UnknownPreset {
                    path: path.display().to_string(),
                    name: name.clone(),
                })?
            }
        };

        let mut overrides = HashMap::new();
        for (rule_name, severity_name) in self.rules {
            let rule =
                RuleId::from_config_name(&rule_name).ok_or_else(|| ConfigError::UnknownRule {
                    path: path.display().to_string(),
                    name: rule_name.clone(),
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

        Ok(Config { preset, overrides })
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
    #[error("{path}: unknown rule {name:?} (see the Rule catalog for valid names)")]
    UnknownRule {
        /// The file the unknown rule was declared in.
        path: String,
        /// The unrecognized rule name.
        name: String,
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
}
