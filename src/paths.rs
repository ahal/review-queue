//! XDG path resolution for config and data directories.
//!
//! Layout:
//! - config: `$XDG_CONFIG_HOME/review-queue/config.toml`
//! - data: `$XDG_DATA_HOME/review-queue/` — state.json, repos.json, sync.lock, workdir.json (the
//!   `crate::workdir` scan cache), `repos/` (every tool-managed canonical clone, keyed by its
//!   normalized URL - discovered repos live wherever the user's workdir put them instead), and
//!   `workspaces/{id}` (every review's workspace, independent of where its canonical repo lives;
//!   keyed by the full canonical id - see `crate::state::ReviewKey::slug` - so ids can't collide
//!   across sources sharing a repo)

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

    /// The `crate::workdir` scan cache.
    pub fn workdir_cache_file(&self) -> PathBuf {
        self.data_dir.join("workdir.json")
    }

    /// Root of every tool-managed canonical repo's directory, keyed by name (its normalized URL).
    pub fn repos_dir(&self) -> PathBuf {
        self.data_dir.join("repos")
    }

    /// Where a tool-managed canonical clone lives: `repos/{name}`.
    pub fn repo_dir(&self, name: &str) -> PathBuf {
        self.repos_dir().join(name)
    }

    /// Root of every review's workspace: `workspaces/`.
    pub fn workspaces_dir(&self) -> PathBuf {
        self.data_dir.join("workspaces")
    }

    /// Where a specific review's workspace lives: `workspaces/{slug}`. `slug` should be the
    /// review's full canonical id (`ReviewKey::slug`, e.g. `gh/owner/repo/42`) so it can't collide
    /// with another source's workspace built from the same canonical repo.
    pub fn workspace_dir(&self, slug: &str) -> PathBuf {
        self.workspaces_dir().join(slug)
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
