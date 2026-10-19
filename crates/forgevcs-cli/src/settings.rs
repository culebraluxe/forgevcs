//! Local configuration: where the service is, and where the bearer token lives.
//!
//! Precedence (highest first): command-line flag, environment variable, `config.toml`, default.
//! The configuration directory defaults to `$XDG_CONFIG_HOME/forgevcs` or `~/.config/forgevcs`.
//! `config.toml` is optional:
//!
//! ```toml
//! base_url = "https://forgevcs.example"
//! # token_file = "token"   # relative to the config directory
//! ```

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::Deserialize;

use crate::args::GlobalOptions;

/// The default local service URL when nothing is configured.
pub const DEFAULT_BASE_URL: &str = "http://127.0.0.1:3000";

/// Environment variables the CLI honours.
pub const BASE_URL_VAR: &str = "FORGEVCS_BASE_URL";
/// Overrides the config directory.
pub const CONFIG_DIR_VAR: &str = "FORGEVCS_CONFIG_DIR";
/// Overrides the token file path.
pub const TOKEN_FILE_VAR: &str = "FORGEVCS_TOKEN_FILE";

/// Resolved local settings for one invocation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Settings {
    /// Service base URL, without a trailing slash.
    pub base_url: String,
    /// Directory holding `token`, `session.json` and the optional `config.toml`.
    pub config_dir: PathBuf,
    /// Bearer token file (mode 0600).
    pub token_file: PathBuf,
}

impl Settings {
    /// Resolve settings from the process environment.
    pub fn resolve(cli: &GlobalOptions) -> Result<Self> {
        Self::resolve_with(cli, |key| std::env::var(key).ok())
    }

    /// Resolve settings with an injectable environment lookup (tests, callers with their own env).
    pub fn resolve_with(cli: &GlobalOptions, env: impl Fn(&str) -> Option<String>) -> Result<Self> {
        let config_dir = match &cli.config_dir {
            Some(path) => path.clone(),
            None => match env(CONFIG_DIR_VAR) {
                Some(path) => PathBuf::from(path),
                None => default_config_dir(&env)?,
            },
        };
        let file = read_file_config(&config_dir)?;

        let base_url = cli
            .base_url
            .clone()
            .or_else(|| env(BASE_URL_VAR))
            .or(file.base_url)
            .unwrap_or_else(|| DEFAULT_BASE_URL.to_owned());
        if base_url.trim().is_empty() {
            anyhow::bail!("base URL is empty");
        }

        let token_file = match &cli.token_file {
            Some(path) => path.clone(),
            None => match env(TOKEN_FILE_VAR) {
                Some(path) => PathBuf::from(path),
                None => match file.token_file {
                    Some(path) => resolve_inside(&config_dir, &path),
                    None => config_dir.join("token"),
                },
            },
        };

        Ok(Self {
            base_url: base_url.trim().trim_end_matches('/').to_owned(),
            config_dir,
            token_file,
        })
    }

    /// The file holding the minted session metadata (principal/email/display/expiry).
    pub fn session_file(&self) -> PathBuf {
        self.config_dir.join("session.json")
    }
}

/// The optional `config.toml` shape. Unknown fields are rejected so a typo is loud.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct FileConfig {
    base_url: Option<String>,
    token_file: Option<String>,
}

fn read_file_config(config_dir: &Path) -> Result<FileConfig> {
    let path = config_dir.join("config.toml");
    match std::fs::read_to_string(&path) {
        Ok(raw) => toml::from_str(&raw).with_context(|| format!("parsing {}", path.display())),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(FileConfig::default()),
        Err(error) => Err(error).with_context(|| format!("reading {}", path.display())),
    }
}

fn default_config_dir(env: &impl Fn(&str) -> Option<String>) -> Result<PathBuf> {
    if let Some(xdg) = env("XDG_CONFIG_HOME").filter(|value| !value.trim().is_empty()) {
        return Ok(PathBuf::from(xdg).join("forgevcs"));
    }
    let home = env("HOME")
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| anyhow::anyhow!("HOME is not set; pass --config-dir"))?;
    Ok(PathBuf::from(home).join(".config").join("forgevcs"))
}

fn resolve_inside(config_dir: &Path, raw: &str) -> PathBuf {
    let path = PathBuf::from(raw);
    if path.is_absolute() {
        path
    } else {
        config_dir.join(path)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    fn env(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let map: std::collections::HashMap<String, String> = pairs
            .iter()
            .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
            .collect();
        move |key| map.get(key).cloned()
    }

    #[test]
    fn defaults_to_localhost_and_the_home_config_dir() {
        let settings =
            Settings::resolve_with(&GlobalOptions::default(), env(&[("HOME", "/home/human")]))
                .expect("settings");
        assert_eq!(settings.base_url, DEFAULT_BASE_URL);
        assert_eq!(
            settings.config_dir,
            PathBuf::from("/home/human/.config/forgevcs")
        );
        assert_eq!(
            settings.token_file,
            PathBuf::from("/home/human/.config/forgevcs/token")
        );
    }

    #[test]
    fn xdg_config_home_and_environment_override_defaults() {
        let settings = Settings::resolve_with(
            &GlobalOptions::default(),
            env(&[
                ("XDG_CONFIG_HOME", "/xdg"),
                (CONFIG_DIR_VAR, "/env/config"),
                (BASE_URL_VAR, "https://env.example/"),
                (TOKEN_FILE_VAR, "/env/token"),
            ]),
        )
        .expect("settings");
        assert_eq!(settings.config_dir, PathBuf::from("/env/config"));
        assert_eq!(settings.base_url, "https://env.example");
        assert_eq!(settings.token_file, PathBuf::from("/env/token"));
    }

    #[test]
    fn cli_flags_beat_environment_and_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(
            dir.path().join("config.toml"),
            "base_url = \"https://file.example\"\ntoken_file = \"file-token\"\n",
        )
        .expect("write config");
        let cli = GlobalOptions {
            base_url: Some("https://cli.example".to_owned()),
            config_dir: Some(dir.path().to_owned()),
            token_file: Some(dir.path().join("cli-token")),
        };
        let settings = Settings::resolve_with(
            &cli,
            env(&[
                (BASE_URL_VAR, "https://env.example"),
                (TOKEN_FILE_VAR, "/env/token"),
            ]),
        )
        .expect("settings");
        assert_eq!(settings.base_url, "https://cli.example");
        assert_eq!(settings.token_file, dir.path().join("cli-token"));
    }

    #[test]
    fn config_file_is_used_when_flags_and_environment_are_absent() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(
            dir.path().join("config.toml"),
            "base_url = \"https://file.example\"\ntoken_file = \"file-token\"\n",
        )
        .expect("write config");
        let cli = GlobalOptions {
            config_dir: Some(dir.path().to_owned()),
            ..GlobalOptions::default()
        };
        let settings = Settings::resolve_with(&cli, env(&[])).expect("settings");
        assert_eq!(settings.base_url, "https://file.example");
        assert_eq!(settings.token_file, dir.path().join("file-token"));
        assert_eq!(settings.session_file(), dir.path().join("session.json"));
    }

    #[test]
    fn typo_in_config_file_is_loud() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(
            dir.path().join("config.toml"),
            "base_uri = \"https://typo.example\"\n",
        )
        .expect("write config");
        let cli = GlobalOptions {
            config_dir: Some(dir.path().to_owned()),
            ..GlobalOptions::default()
        };
        assert!(Settings::resolve_with(&cli, env(&[])).is_err());
    }
}
