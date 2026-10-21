//! Loading configuration from disk.
//!
//! The file format is decided by extension rather than sniffed, so a mistake
//! is reported rather than silently half-parsed. Unknown fields are rejected:
//! a typo in a key name would otherwise look like a setting that does nothing.

use std::path::{Path, PathBuf};

use vetinari_core::application::config::{Config, ConfigError, ParseContext, RawConfig};

/// Where configuration lives, relative to a config directory.
pub const APP_DIR: &str = "vetinari";

/// Why a configuration file could not be loaded.
#[derive(Debug, thiserror::Error)]
pub enum LoadError {
    /// The file could not be read.
    #[error("could not read {}: {source}", path.display())]
    Unreadable {
        /// The file.
        path: PathBuf,
        /// Why.
        source: std::io::Error,
    },
    /// The extension did not say which format the file is in.
    #[error("{} is not a .toml, .yaml, or .yml file", path.display())]
    UnknownFormat {
        /// The file.
        path: PathBuf,
    },
    /// The file was not valid TOML or YAML.
    #[error("{} is malformed: {detail}", path.display())]
    Syntax {
        /// The file.
        path: PathBuf,
        /// What the parser said.
        detail: String,
    },
    /// The file parsed but says impossible things.
    #[error("{} is invalid: {problems}", path.display())]
    Invalid {
        /// The file.
        path: PathBuf,
        /// Everything wrong with it.
        problems: ConfigError,
    },
}

/// A configuration, and where it came from.
#[derive(Debug)]
pub struct LoadedConfig {
    /// The configuration.
    pub config: Config,
    /// The file it was read from, or `None` for built-in defaults.
    pub path: Option<PathBuf>,
}

/// The files Vetinari looks for, in order.
#[must_use]
pub fn candidate_paths(config_dir: &Path) -> Vec<PathBuf> {
    let base = config_dir.join(APP_DIR);
    ["config.toml", "config.yaml", "config.yml"]
        .iter()
        .map(|name| base.join(name))
        .collect()
}

/// The first candidate that exists.
#[must_use]
pub fn find_config(candidates: &[PathBuf]) -> Option<PathBuf> {
    candidates.iter().find(|path| path.is_file()).cloned()
}

/// Load one configuration file.
pub fn load_file(path: &Path, context: &ParseContext) -> Result<Config, LoadError> {
    let text = std::fs::read_to_string(path).map_err(|source| LoadError::Unreadable {
        path: path.to_path_buf(),
        source,
    })?;

    let raw: RawConfig = match path.extension().and_then(|ext| ext.to_str()) {
        Some("toml") => toml::from_str(&text).map_err(|error| LoadError::Syntax {
            path: path.to_path_buf(),
            detail: error.to_string(),
        })?,
        Some("yaml" | "yml") => serde_yaml::from_str(&text).map_err(|error| LoadError::Syntax {
            path: path.to_path_buf(),
            detail: error.to_string(),
        })?,
        _ => {
            return Err(LoadError::UnknownFormat {
                path: path.to_path_buf(),
            });
        }
    };

    Config::parse(raw, context).map_err(|problems| LoadError::Invalid {
        path: path.to_path_buf(),
        problems,
    })
}

/// Load the first candidate that exists, or the built-in defaults.
pub fn load_or_default(
    candidates: &[PathBuf],
    context: &ParseContext,
) -> Result<LoadedConfig, LoadError> {
    match find_config(candidates) {
        Some(path) => {
            let config = load_file(&path, context)?;
            Ok(LoadedConfig {
                config,
                path: Some(path),
            })
        }
        None => {
            let config = Config::parse(RawConfig::default(), context)
                .expect("the built-in defaults are valid");
            Ok(LoadedConfig { config, path: None })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(dir: &Path, name: &str, body: &str) -> PathBuf {
        let path = dir.join(name);
        std::fs::write(&path, body).expect("write");
        path
    }

    #[test]
    fn a_toml_file_is_loaded() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = write(
            dir.path(),
            "config.toml",
            r#"
            [general]
            profile = "home"
            "#,
        );

        let config = load_file(&path, &ParseContext::default()).expect("load");

        assert_eq!(config.profiles.active().id().as_str(), "home");
    }

    #[test]
    fn a_yaml_file_is_loaded_too() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = write(
            dir.path(),
            "config.yaml",
            "general:\n  profile: home\n  tick_rate_ms: 500\n",
        );

        let config = load_file(&path, &ParseContext::default()).expect("load");

        assert_eq!(config.profiles.active().id().as_str(), "home");
        assert_eq!(config.general.tick_rate_ms, 500);
    }

    #[test]
    fn an_unknown_extension_is_refused_rather_than_guessed_at() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = write(dir.path(), "config.ini", "[general]\n");

        let error = load_file(&path, &ParseContext::default()).expect_err("unknown format");

        assert!(matches!(error, LoadError::UnknownFormat { .. }));
    }

    #[test]
    fn a_syntax_error_names_the_file_and_the_problem() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = write(dir.path(), "config.toml", "[general\nprofile = ");

        let error = load_file(&path, &ParseContext::default()).expect_err("bad syntax");

        let message = error.to_string();
        assert!(message.contains("config.toml"), "got {message}");
        assert!(matches!(error, LoadError::Syntax { .. }));
    }

    #[test]
    fn a_typo_in_a_field_name_is_rejected_rather_than_ignored() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = write(dir.path(), "config.toml", "[general]\nprofil = \"home\"\n");

        let error = load_file(&path, &ParseContext::default()).expect_err("unknown field");

        assert!(error.to_string().contains("profil"), "got {error}");
    }

    #[test]
    fn a_semantic_problem_is_reported_with_every_other_one() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = write(
            dir.path(),
            "config.toml",
            r#"
            [general]
            profile = "mars"

            [keymap.normal]
            "SPC z" = "summon-dragon"
            "#,
        );

        let error = load_file(&path, &ParseContext::default()).expect_err("invalid");

        match error {
            LoadError::Invalid { problems, .. } => assert_eq!(problems.problems().len(), 2),
            other => panic!("expected invalid config, got {other:?}"),
        }
    }

    #[test]
    fn a_missing_file_is_absence_rather_than_an_error() {
        let dir = tempfile::tempdir().expect("tempdir");
        let found = find_config(&[dir.path().join("config.toml")]);
        assert!(found.is_none());
    }

    #[test]
    fn the_first_candidate_that_exists_wins() {
        let dir = tempfile::tempdir().expect("tempdir");
        let second = write(dir.path(), "config.yaml", "general: {}\n");
        let candidates = vec![dir.path().join("config.toml"), second.clone()];

        assert_eq!(find_config(&candidates), Some(second));
    }

    #[test]
    fn candidate_paths_cover_toml_and_yaml_in_the_config_directory() {
        let candidates = candidate_paths(Path::new("/home/dev/.config"));
        let names: Vec<_> = candidates
            .iter()
            .map(|path| {
                path.strip_prefix("/home/dev/.config/vetinari")
                    .expect("prefix")
            })
            .map(|path| path.display().to_string())
            .collect();
        assert_eq!(names, vec!["config.toml", "config.yaml", "config.yml"]);
    }

    #[test]
    fn loading_without_a_file_yields_the_defaults() {
        let dir = tempfile::tempdir().expect("tempdir");
        let loaded = load_or_default(&[dir.path().join("nothing.toml")], &ParseContext::default())
            .expect("defaults");
        assert!(loaded.path.is_none());
        assert_eq!(loaded.config.profiles.len(), 2);
    }

    #[test]
    fn loading_reports_which_file_was_used() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = write(dir.path(), "config.toml", "");
        let loaded =
            load_or_default(std::slice::from_ref(&path), &ParseContext::default()).expect("load");
        assert_eq!(loaded.path, Some(path));
    }
}
