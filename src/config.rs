//! `config.toml` schema: review sources and user-owned canonical repos.

use std::path::PathBuf;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub data_dir: Option<PathBuf>,
    #[serde(default)]
    pub repo_cache_dir: Option<PathBuf>,

    #[serde(rename = "source", default)]
    pub sources: Vec<SourceConfig>,
    #[serde(rename = "repo", default)]
    pub repos: Vec<RepoConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum SourceConfig {
    /// Mozilla's Phabricator, via the `moz-phab` CLI - not a general-purpose Phabricator source.
    /// See `crate::source::moz_phab` for why.
    #[serde(rename = "moz-phab")]
    MozPhab(MozPhabConfig),
    Github(GithubConfig),
}

impl SourceConfig {
    pub fn name(&self) -> &str {
        match self {
            SourceConfig::MozPhab(c) => &c.name,
            SourceConfig::Github(c) => &c.name,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MozPhabConfig {
    pub name: String,
    pub url: String,
    #[serde(default)]
    pub token: Option<String>,
    #[serde(default)]
    pub token_cmd: Option<String>,
    #[serde(default = "default_true")]
    pub include_groups: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GithubConfig {
    pub name: String,
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

/// A user-owned canonical repo: an existing checkout the tool should create
/// worktrees/workspaces from, rather than cloning its own copy.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RepoConfig {
    /// Clone URLs that all refer to this repo (origin, mirrors, etc). Matched after
    /// normalization; see `crate::repo::normalize_url`.
    pub urls: Vec<String>,
    pub path: PathBuf,
    /// Auto-detected from `path` (`.jj` wins over `.git`) when unset.
    #[serde(default)]
    pub vcs: Option<VcsKind>,
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
        config.repo_cache_dir = config.repo_cache_dir.map(|p| expand_tilde(&p));
        for repo in &mut config.repos {
            repo.path = expand_tilde(&repo.path);
        }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_example_config() {
        let text = r#"
[[source]]
type = "moz-phab"
name = "moz"
url = "https://phabricator.services.mozilla.com"

[[source]]
type = "github"
name = "github"
ignore_repos = ["mozilla/some-noisy-repo"]

[[repo]]
urls = ["https://github.com/mozilla-firefox/firefox"]
path = "~/dev/firefox"
"#;
        let cfg: Config = toml::from_str(text).unwrap();
        assert_eq!(cfg.sources.len(), 2);
        assert_eq!(cfg.sources[0].name(), "moz");
        assert_eq!(cfg.sources[1].name(), "github");
        assert_eq!(cfg.repos.len(), 1);
        assert_eq!(
            cfg.repos[0].urls,
            vec!["https://github.com/mozilla-firefox/firefox"]
        );
        assert!(cfg.repos[0].vcs.is_none());
    }

    #[test]
    fn missing_file_is_empty_config() {
        let cfg = Config::load(std::path::Path::new("/nonexistent/config.toml")).unwrap();
        assert!(cfg.sources.is_empty());
        assert!(cfg.repos.is_empty());
    }

    #[test]
    fn load_expands_tilde_in_repo_and_data_dir_paths() {
        let tmp = tempfile::tempdir().unwrap();
        let config_path = tmp.path().join("config.toml");
        std::fs::write(
            &config_path,
            r#"
data_dir = "~/rq-data"

[[repo]]
urls = ["https://example.com/o/r"]
path = "~/dev/firefox"
"#,
        )
        .unwrap();

        let cfg = Config::load(&config_path).unwrap();
        let home = directories::BaseDirs::new()
            .unwrap()
            .home_dir()
            .to_path_buf();

        assert_eq!(cfg.data_dir, Some(home.join("rq-data")));
        assert_eq!(cfg.repos[0].path, home.join("dev/firefox"));
        assert!(
            !cfg.repos[0].path.starts_with("~"),
            "the literal `~` component must be gone"
        );
    }
}
