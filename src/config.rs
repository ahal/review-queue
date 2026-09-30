//! `config.toml` schema: review sources and where canonical repos live.

use std::path::PathBuf;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub data_dir: Option<PathBuf>,
    /// Directory rq scans for existing checkouts to use as canonical repos (see `crate::workdir`)
    /// instead of cloning its own copy.
    #[serde(default)]
    pub workdir: Option<PathBuf>,
    /// Clone into the data dir without asking, whenever a review's repo isn't found in
    /// `workdir`. Set by answering "always" to the clone prompt (see `set_auto_clone`).
    #[serde(default)]
    pub auto_clone: bool,
    /// Shell command to run in a review's workspace instead of an interactive `$SHELL` when it's
    /// opened locally from the TUI. Run via `sh -c` with the workspace as cwd; the review is
    /// described by `RQ_REVIEW` (`source/id`), `RQ_SOURCE`, `RQ_ID` and `RQ_WORKSPACE`.
    #[serde(default)]
    pub open_command: Option<String>,
    /// Whether rq suspends its TUI until `open_command` exits (the default). Set to `false` for
    /// commands that hand off elsewhere and return - e.g. switching a tmux client - so rq keeps
    /// running, with the command's stdio detached. Ignored without `open_command`: the fallback
    /// `$SHELL` needs the terminal.
    #[serde(default = "default_true")]
    pub open_command_wait: bool,

    #[serde(default)]
    pub source: SourcesConfig,
}

/// Each source is a fixed, hardcoded slot (`[source.github]`, `[source.moz-phab]`) rather than a
/// list - there's exactly one of each kind, and its id-namespace prefix (see
/// `crate::source::github::NAME`/`crate::source::moz_phab::NAME`) is likewise hardcoded, not
/// user-configurable.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SourcesConfig {
    #[serde(default)]
    pub github: Option<GithubConfig>,
    /// Mozilla's Phabricator, via the `moz-phab` CLI - not a general-purpose Phabricator source.
    /// See `crate::source::moz_phab` for why.
    #[serde(rename = "moz-phab", default)]
    pub moz_phab: Option<MozPhabConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MozPhabConfig {
    pub url: String,
    #[serde(default)]
    pub token: Option<String>,
    #[serde(default)]
    pub token_cmd: Option<String>,
    #[serde(default = "default_true")]
    pub include_groups: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct GithubConfig {
    /// REST API base URL. Defaults to `https://api.github.com`; set this for GitHub Enterprise
    /// (typically `https://{host}/api/v3`).
    #[serde(default)]
    pub api_url: Option<String>,
    #[serde(default)]
    pub token: Option<String>,
    #[serde(default)]
    pub token_cmd: Option<String>,
    #[serde(default)]
    pub ignore_repos: Vec<String>,
    #[serde(default)]
    pub ignore_authors: Vec<String>,
    #[serde(default)]
    pub ignore_teams: Vec<String>,
    #[serde(default)]
    pub include_drafts: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum VcsKind {
    Git,
    Jj,
}

fn default_true() -> bool {
    true
}

impl Default for Config {
    fn default() -> Self {
        // Go through serde so the field defaults (e.g. `open_command_wait`) live in one place.
        toml::from_str("").expect("an empty config is valid")
    }
}

impl Config {
    pub fn load(path: &std::path::Path) -> Result<Self> {
        if !path.exists() {
            return Ok(Self::default());
        }
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("reading config {}", path.display()))?;
        let mut config: Config =
            toml::from_str(&text).with_context(|| format!("parsing config {}", path.display()))?;

        config.data_dir = config.data_dir.map(|p| expand_tilde(&p));
        config.workdir = config.workdir.map(|p| expand_tilde(&p));
        Ok(config)
    }
}

/// `PathBuf`'s `Deserialize` treats `~` as a literal path component, so config paths need this
/// run explicitly - nothing does it for free the way a shell would.
fn expand_tilde(path: &std::path::Path) -> PathBuf {
    match path.strip_prefix("~") {
        Ok(rest) => match directories::BaseDirs::new() {
            Some(base_dirs) => base_dirs.home_dir().join(rest),
            None => path.to_path_buf(),
        },
        Err(_) => path.to_path_buf(),
    }
}

/// Record a "always clone without asking" answer to the clone prompt. Prepended (rather than
/// appended, like nothing else in this file does) because appending a bare `key = value` after
/// one of `config.toml`'s `[source.*]` tables would parse as belonging to that table instead of
/// to the top-level document.
pub fn set_auto_clone(path: &std::path::Path) -> Result<()> {
    let existing = std::fs::read_to_string(path).unwrap_or_default();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    std::fs::write(path, format!("auto_clone = true\n{existing}"))
        .with_context(|| format!("writing {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_example_config() {
        let text = r#"
workdir = "~/dev"

[source.moz-phab]
url = "https://phabricator.services.mozilla.com"

[source.github]
ignore_repos = ["mozilla/some-noisy-repo"]
"#;
        let cfg: Config = toml::from_str(text).unwrap();
        assert_eq!(
            cfg.source.moz_phab.unwrap().url,
            "https://phabricator.services.mozilla.com"
        );
        assert_eq!(
            cfg.source.github.unwrap().ignore_repos,
            vec!["mozilla/some-noisy-repo".to_string()]
        );
        assert_eq!(cfg.workdir, Some(PathBuf::from("~/dev")));
        assert!(!cfg.auto_clone);
        assert!(cfg.open_command.is_none());
        assert!(cfg.open_command_wait, "blocking is the default");
    }

    #[test]
    fn open_command_wait_can_be_disabled() {
        let cfg: Config = toml::from_str("open_command_wait = false").unwrap();
        assert!(!cfg.open_command_wait);
    }

    #[test]
    fn parses_open_command() {
        let cfg: Config = toml::from_str(r#"open_command = "nvim +DiffviewOpen""#).unwrap();
        assert_eq!(cfg.open_command.as_deref(), Some("nvim +DiffviewOpen"));
    }

    #[test]
    fn missing_file_is_empty_config() {
        let cfg = Config::load(std::path::Path::new("/nonexistent/config.toml")).unwrap();
        assert!(cfg.source.github.is_none());
        assert!(cfg.source.moz_phab.is_none());
        assert!(cfg.workdir.is_none());
    }

    #[test]
    fn load_expands_tilde_in_data_dir_and_workdir_paths() {
        let tmp = tempfile::tempdir().unwrap();
        let config_path = tmp.path().join("config.toml");
        std::fs::write(
            &config_path,
            r#"
data_dir = "~/rq-data"
workdir = "~/dev"
"#,
        )
        .unwrap();

        let cfg = Config::load(&config_path).unwrap();
        let home = directories::BaseDirs::new()
            .unwrap()
            .home_dir()
            .to_path_buf();

        assert_eq!(cfg.data_dir, Some(home.join("rq-data")));
        assert_eq!(cfg.workdir, Some(home.join("dev")));
        assert!(
            !cfg.workdir.unwrap().starts_with("~"),
            "the literal `~` component must be gone"
        );
    }

    #[test]
    fn set_auto_clone_prepends_and_survives_a_reload() {
        let tmp = tempfile::tempdir().unwrap();
        let config_path = tmp.path().join("config.toml");
        std::fs::write(
            &config_path,
            r#"
[source.github]
"#,
        )
        .unwrap();

        set_auto_clone(&config_path).unwrap();

        let cfg = Config::load(&config_path).unwrap();
        assert!(cfg.auto_clone);
        assert!(cfg.source.github.is_some(), "existing sources must survive");
    }

    #[test]
    fn set_auto_clone_creates_a_missing_config_file() {
        let tmp = tempfile::tempdir().unwrap();
        let config_path = tmp.path().join("nested").join("config.toml");

        set_auto_clone(&config_path).unwrap();

        assert!(Config::load(&config_path).unwrap().auto_clone);
    }
}
