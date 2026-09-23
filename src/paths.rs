//! XDG path resolution for config and data directories.
//!
//! Layout:
//! - config: `$XDG_CONFIG_HOME/review-queue/config.toml`
//! - data: `$XDG_DATA_HOME/review-queue/` — state.json, repos.json, sync.lock, and `repos/`
//!   (every canonical repo, keyed by its normalized URL, holding both the tool-managed clone
//!   under `source/` and the workspaces built from it under `workspaces/{id}`; each source
//!   prefixes its ids, e.g. `phab-D1234`/`pr-owner/repo/42`, so ids never collide across sources
//!   sharing a repo)

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use directories::ProjectDirs;

#[derive(Clone)]
pub struct Paths {
    config_dir: PathBuf,
    data_dir: PathBuf,
}

impl Paths {
    /// Resolve paths from `ProjectDirs`, honoring the `data_dir` override once it's read from
    /// config (see [`Paths::with_overrides`]).
    pub fn discover() -> Result<Self> {
        let dirs = ProjectDirs::from("", "", "review-queue")
            .context("could not determine home directory")?;
        Ok(Self {
            config_dir: dirs.config_dir().to_path_buf(),
            data_dir: dirs.data_dir().to_path_buf(),
        })
    }

    pub fn with_overrides(mut self, data_dir: Option<PathBuf>) -> Self {
        if let Some(d) = data_dir {
            self.data_dir = d;
        }
        self
    }

    pub fn config_file(&self) -> PathBuf {
        self.config_dir.join("config.toml")
    }

    pub fn state_file(&self) -> PathBuf {
        self.data_dir.join("state.json")
    }

    pub fn repos_file(&self) -> PathBuf {
        self.data_dir.join("repos.json")
    }

    pub fn sync_lock_file(&self) -> PathBuf {
        self.data_dir.join("sync.lock")
    }

    /// Root of every canonical repo's directory, keyed by name (its normalized URL).
    pub fn repos_dir(&self) -> PathBuf {
        self.data_dir.join("repos")
    }

    /// A single canonical repo's directory: `repos/{name}/`.
    pub fn repo_dir(&self, name: &str) -> PathBuf {
        self.repos_dir().join(name)
    }

    /// Where a tool-managed canonical clone lives: `repos/{name}/source`.
    pub fn repo_source_dir(&self, name: &str) -> PathBuf {
        self.repo_dir(name).join("source")
    }

    /// Root of the workspaces built from a given canonical repo: `repos/{name}/workspaces/`.
    pub fn repo_workspaces_dir(&self, name: &str) -> PathBuf {
        self.repo_dir(name).join("workspaces")
    }

    /// Where a specific review's workspace lives: `repos/{name}/workspaces/{id}`. `id` is already
    /// unique across sources - see [`crate::state::ReviewKey`].
    pub fn workspace_dir(&self, name: &str, id: &str) -> PathBuf {
        self.repo_workspaces_dir(name).join(id)
    }

    pub fn data_dir(&self) -> &Path {
        &self.data_dir
    }

    /// Create the data and config directories if they don't exist yet.
    pub fn ensure_dirs(&self) -> Result<()> {
        std::fs::create_dir_all(&self.data_dir)
            .with_context(|| format!("creating data dir {}", self.data_dir.display()))?;
        std::fs::create_dir_all(&self.config_dir)
            .with_context(|| format!("creating config dir {}", self.config_dir.display()))?;
        Ok(())
    }
}
