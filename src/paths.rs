//! XDG path resolution for config, cache, and data directories.
//!
//! Layout (see the design plan for rationale):
//! - config: `$XDG_CONFIG_HOME/review-queue/config.toml`
//! - tool-managed canonical clones: `$XDG_CACHE_HOME/review-queue/repos/` (never deleted by us,
//!   but safe to nuke by hand since they're recreated on demand)
//! - data: `$XDG_DATA_HOME/review-queue/` — workspaces, state.json, repos.json, sync.lock

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use directories::ProjectDirs;

pub struct Paths {
    config_dir: PathBuf,
    cache_dir: PathBuf,
    data_dir: PathBuf,
}

impl Paths {
    /// Resolve paths from `ProjectDirs`, honoring the `data_dir` / `repo_cache_dir` overrides
    /// once they're read from config (see [`Paths::with_overrides`]).
    pub fn discover() -> Result<Self> {
        let dirs = ProjectDirs::from("", "", "review-queue")
            .context("could not determine home directory")?;
        Ok(Self {
            config_dir: dirs.config_dir().to_path_buf(),
            cache_dir: dirs.cache_dir().to_path_buf(),
            data_dir: dirs.data_dir().to_path_buf(),
        })
    }

    pub fn with_overrides(
        mut self,
        data_dir: Option<PathBuf>,
        repo_cache_dir: Option<PathBuf>,
    ) -> Self {
        if let Some(d) = data_dir {
            self.data_dir = d;
        }
        if let Some(c) = repo_cache_dir {
            self.cache_dir = c;
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

    pub fn workspaces_dir(&self) -> PathBuf {
        self.data_dir.join("workspaces")
    }

    /// Where a specific review's workspace lives: `workspaces/{source}/{id}`.
    pub fn workspace_dir(&self, source: &str, id: &str) -> PathBuf {
        self.workspaces_dir().join(source).join(id)
    }

    /// Root of tool-managed canonical clones.
    pub fn repo_cache_dir(&self) -> PathBuf {
        self.cache_dir.join("repos")
    }

    pub fn data_dir(&self) -> &Path {
        &self.data_dir
    }

    /// Create the data and config directories if they don't exist yet.
    pub fn ensure_dirs(&self) -> Result<()> {
        std::fs::create_dir_all(&self.data_dir)
            .with_context(|| format!("creating data dir {}", self.data_dir.display()))?;
        std::fs::create_dir_all(self.workspaces_dir()).with_context(|| {
            format!(
                "creating workspaces dir {}",
                self.workspaces_dir().display()
            )
        })?;
        std::fs::create_dir_all(&self.config_dir)
            .with_context(|| format!("creating config dir {}", self.config_dir.display()))?;
        Ok(())
    }
}
