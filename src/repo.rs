//! Canonical repo resolution: every review repo maps to exactly one canonical local repo, which
//! `sync` never deletes - only the worktrees/workspaces created from it. A canonical repo
//! is either discovered (an existing checkout found by scanning `Config::workdir`, see
//! `crate::workdir`) or tool-managed (cloned once under `Paths::repo_dir()` and recorded
//! in `repos.json`).
//!
//! Lookup order: the workdir scan cache, then the `repos.json` registry, then - if `workdir` is
//! set - a rescan (throttled, see `RepoStore::needs_rescan`) in case the repo was just cloned or
//! is newly discoverable. Still missing after all that: `OnMissing` decides whether to clone
//! (always plain git - only a discovered checkout can be jj) or hand back `NeedsClone` for the
//! caller to ask about first.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::config::{Config, VcsKind};
use crate::paths::Paths;
use crate::source::RepoRef;
use crate::state::State;
use crate::workdir::{self, WorkdirCache};

/// A resolved local repo that review workspaces are built from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CanonicalRepo {
    pub path: PathBuf,
    pub vcs: VcsKind,
    /// Identifies this repo's directory under `Paths::repos_dir()` - its normalized URL. Stable
    /// across aliases (origin vs mirror) so a review resolving through a different alias still
    /// finds the same clone.
    pub name: String,
}

/// What `RepoStore::resolve` should do when a review's repo isn't found locally.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OnMissing {
    /// Return `NeedsClone` instead, so the caller can confirm with the user first.
    Ask,
    /// Clone into the data dir without asking (`Config::auto_clone`, or a context - like a
    /// non-interactive `rq sync` - that can't ask).
    Clone,
}

/// Returned by `RepoStore::resolve` under `OnMissing::Ask` when a review's repo has no local
/// checkout. Callers should `downcast_ref` for this to offer the clone prompt, and re-resolve
/// with `OnMissing::Clone` if the user agrees.
#[derive(Debug, thiserror::Error)]
#[error("no local checkout of `{url}` found (would clone into {})", dest.display())]
pub struct NeedsClone {
    pub url: String,
    pub dest: PathBuf,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct RegistryEntry {
    path: PathBuf,
    origin_url: String,
    created: chrono::DateTime<chrono::Utc>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct Registry {
    /// Keyed by `normalize_url(origin_url)`.
    repos: BTreeMap<String, RegistryEntry>,
}

impl Registry {
    fn load(path: &Path) -> Result<Self> {
        if !path.exists() {
            return Ok(Self::default());
        }
        let text =
            std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        if text.trim().is_empty() {
            return Ok(Self::default());
        }
        serde_json::from_str(&text).with_context(|| format!("parsing {}", path.display()))
    }

    fn save(&self, path: &Path) -> Result<()> {
        let dir = path
            .parent()
            .context("registry path has no parent directory")?;
        std::fs::create_dir_all(dir)?;
        let tmp = dir.join(format!(
            ".{}.tmp",
            path.file_name().unwrap().to_string_lossy()
        ));
        std::fs::write(&tmp, serde_json::to_string_pretty(self)?)
            .with_context(|| format!("writing {}", tmp.display()))?;
        std::fs::rename(&tmp, path)
            .with_context(|| format!("renaming {} to {}", tmp.display(), path.display()))?;
        Ok(())
    }
}

/// A rescan is skipped if the cache is younger than this - covers the ask/confirm/retry round
/// trip and bursts of `rq fetch` calls, without letting a real miss go undetected for long.
const RESCAN_THROTTLE: chrono::Duration = chrono::Duration::seconds(60);

pub struct RepoStore {
    registry_path: PathBuf,
    registry: Registry,
    repos_dir: PathBuf,
    workdir: Option<PathBuf>,
    workdir_cache_path: PathBuf,
    workdir_cache: Option<WorkdirCache>,
}

impl RepoStore {
    pub fn load(paths: &Paths, config: &Config) -> Result<Self> {
        let workdir_cache_path = paths.workdir_cache_file();
        Ok(Self {
            registry_path: paths.repos_file(),
            registry: Registry::load(&paths.repos_file())?,
            repos_dir: paths.repos_dir(),
            workdir: config.workdir.clone(),
            workdir_cache: WorkdirCache::load(&workdir_cache_path)?,
            workdir_cache_path,
        })
    }

    /// Resolve `repo_ref` to a canonical local repo. See the module docs for the lookup order.
    pub fn resolve(&mut self, repo_ref: &RepoRef, on_missing: OnMissing) -> Result<CanonicalRepo> {
        let normalized: Vec<String> = repo_ref.urls.iter().map(|u| normalize_url(u)).collect();

        if let Some(canon) = self.lookup_workdir(&normalized) {
            return Ok(canon);
        }

        for n in &normalized {
            if let Some(entry) = self.registry.repos.get(n)
                && entry.path.exists()
            {
                return Ok(CanonicalRepo {
                    path: entry.path.clone(),
                    vcs: VcsKind::Git,
                    name: n.clone(),
                });
            }
        }

        self.rescan_if_needed()?;
        if let Some(canon) = self.lookup_workdir(&normalized) {
            return Ok(canon);
        }

        let url = repo_ref
            .urls
            .first()
            .context("review's repo has no candidate URLs to clone")?;
        let name = normalize_url(url);
        let dest = self.repos_dir.join(&name);

        if on_missing == OnMissing::Ask {
            return Err(NeedsClone {
                url: url.clone(),
                dest,
            }
            .into());
        }

        if dest.exists() {
            // Self-heal a registry that fell out of sync with the filesystem - e.g. a prior run
            // cloned this but was interrupted before it could save `repos.json`. Trust an
            // existing directory at the expected cache path over attempting (and failing) to
            // clone into it again.
            tracing::warn!(
                "{} already exists but wasn't registered; reusing it as-is",
                dest.display()
            );
        } else {
            clone_repo(url, &dest)?;
        }

        self.registry.repos.insert(
            name.clone(),
            RegistryEntry {
                path: dest.clone(),
                origin_url: url.clone(),
                created: chrono::Utc::now(),
            },
        );
        Ok(CanonicalRepo {
            path: dest,
            vcs: VcsKind::Git,
            name,
        })
    }

    /// Looks up `normalized` in the workdir cache - but only if it was scanned for the
    /// currently-configured `workdir`. A cache left over from a since-changed (or removed)
    /// `workdir` setting is treated as absent rather than trusted, even though the directories it
    /// recorded may still exist on disk.
    fn lookup_workdir(&self, normalized: &[String]) -> Option<CanonicalRepo> {
        let cache = self.workdir_cache.as_ref()?;
        if Some(&cache.workdir) != self.workdir.as_ref() {
            return None;
        }
        let repo = cache.lookup(normalized)?;
        Some(CanonicalRepo {
            path: repo.path.clone(),
            vcs: repo.vcs,
            name: repo.name.clone(),
        })
    }

    /// Scan `workdir` if it's never been scanned, or the cache is stale - the same throttled
    /// policy `resolve` uses on a miss. Exposed so `rq repo list` reflects the workdir without
    /// requiring an `rq fetch` to have triggered a scan first.
    pub fn rescan_if_needed(&mut self) -> Result<()> {
        if self.workdir.is_some() && self.needs_rescan() {
            self.rescan()?;
        }
        Ok(())
    }

    fn needs_rescan(&self) -> bool {
        match &self.workdir_cache {
            None => true,
            Some(cache) => {
                Some(&cache.workdir) != self.workdir.as_ref()
                    || chrono::Utc::now().signed_duration_since(cache.scanned_at) >= RESCAN_THROTTLE
            }
        }
    }

    fn rescan(&mut self) -> Result<()> {
        let Some(workdir) = self.workdir.clone() else {
            return Ok(());
        };
        let repos = workdir::scan(&workdir, &self.repos_dir);
        let cache = WorkdirCache {
            workdir,
            scanned_at: chrono::Utc::now(),
            repos,
        };
        cache.save(&self.workdir_cache_path)?;
        self.workdir_cache = Some(cache);
        Ok(())
    }

    pub fn save(&self) -> Result<()> {
        self.registry.save(&self.registry_path)
    }

    /// Every canonical repo (discovered and tool-managed), with how many tracked workspaces
    /// currently point at it - for `rq repo list`.
    pub fn list(&self, state: &State) -> Vec<RepoListEntry> {
        let mut out = Vec::new();
        if let Some(cache) = &self.workdir_cache {
            for r in &cache.repos {
                let workspace_count = state
                    .workspaces()
                    .filter(|(_, w)| w.repo_path == r.path)
                    .count();
                out.push(RepoListEntry {
                    url: r.name.clone(),
                    path: r.path.clone(),
                    kind: RepoKind::Discovered,
                    workspace_count,
                });
            }
        }
        for (url, entry) in &self.registry.repos {
            let workspace_count = state
                .workspaces()
                .filter(|(_, w)| w.repo_path == entry.path)
                .count();
            out.push(RepoListEntry {
                url: url.clone(),
                path: entry.path.clone(),
                kind: RepoKind::ToolManaged,
                workspace_count,
            });
        }
        out
    }

    /// Delete a tool-managed clone and forget it. Refuses if `url` is a discovered repo instead
    /// (rq never deletes checkouts it didn't create), or if any tracked workspace still points
    /// at it.
    pub fn remove(&mut self, url: &str, state: &State) -> Result<()> {
        let normalized = normalize_url(url);
        if self
            .workdir_cache
            .as_ref()
            .is_some_and(|cache| cache.repos.iter().any(|r| r.remotes.contains(&normalized)))
        {
            bail!(
                "`{url}` is a discovered repo in your workdir; rq never deletes those - remove the checkout yourself if you want it forgotten"
            );
        }
        let Some(entry) = self.registry.repos.get(&normalized).cloned() else {
            bail!("no tool-managed clone registered for `{url}` (see `rq repo list`)");
        };
        if state.workspaces().any(|(_, w)| w.repo_path == entry.path) {
            bail!(
                "{} still has workspaces using it; wait for `rq sync` to clean up resolved \
                 ones, or remove them by hand first",
                entry.path.display()
            );
        }
        std::fs::remove_dir_all(&entry.path)
            .with_context(|| format!("removing {}", entry.path.display()))?;
        self.registry.repos.remove(&normalized);
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RepoKind {
    Discovered,
    ToolManaged,
}

#[derive(Debug, Clone)]
pub struct RepoListEntry {
    pub url: String,
    pub path: PathBuf,
    pub kind: RepoKind,
    pub workspace_count: usize,
}

fn clone_repo(url: &str, dest: &Path) -> Result<()> {
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    let output = std::process::Command::new("git")
        .args(["clone", "--filter=blob:none", url, &dest.to_string_lossy()])
        .output()
        .with_context(|| format!("cloning {url}"))?;
    if !output.status.success() {
        bail!(
            "cloning {url} into {} failed: {}",
            dest.display(),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(())
}

/// Normalize a clone URL so `git@github.com:o/r.git`, `https://github.com/o/r`, and
/// `ssh://git@github.com/o/r` all compare equal, as `host/owner/repo`.
pub fn normalize_url(url: &str) -> String {
    let url = url.trim().trim_end_matches(".git");

    // scp-like syntax: git@host:owner/repo
    if let Some((host_part, path_part)) = url.split_once(':')
        && !host_part.contains('/')
        && host_part.contains('@')
    {
        let host = host_part.rsplit('@').next().unwrap_or(host_part);
        return format!("{host}/{}", path_part.trim_matches('/'));
    }

    // URL syntax: scheme://[user@]host/path
    if let Some(idx) = url.find("://") {
        let rest = &url[idx + 3..];
        let rest = rest.split_once('@').map(|(_, h)| h).unwrap_or(rest);
        return rest.trim_matches('/').to_string();
    }

    url.trim_matches('/').to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::paths::Paths;
    use crate::workdir::DiscoveredRepo;
    use std::process::Command;
    use tempfile::TempDir;

    #[test]
    fn normalizes_equivalent_github_urls() {
        let expected = "github.com/mozilla-firefox/firefox";
        assert_eq!(
            normalize_url("https://github.com/mozilla-firefox/firefox"),
            expected
        );
        assert_eq!(
            normalize_url("https://github.com/mozilla-firefox/firefox.git"),
            expected
        );
        assert_eq!(
            normalize_url("git@github.com:mozilla-firefox/firefox.git"),
            expected
        );
        assert_eq!(
            normalize_url("ssh://git@github.com/mozilla-firefox/firefox"),
            expected
        );
    }

    fn git(dir: &Path, args: &[&str]) {
        let out = Command::new("git")
            .current_dir(dir)
            .args(args)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    fn paths_in(tmp: &Path) -> Paths {
        Paths::discover()
            .unwrap()
            .with_overrides(Some(tmp.join("data")))
    }

    fn repo_ref(urls: &[&str]) -> RepoRef {
        RepoRef {
            urls: urls.iter().map(|s| s.to_string()).collect(),
            display_name: "test/repo".into(),
        }
    }

    fn config_with_workdir(workdir: &Path) -> Config {
        Config {
            workdir: Some(workdir.to_path_buf()),
            ..Default::default()
        }
    }

    #[test]
    fn resolves_a_discovered_repo_without_cloning() {
        let tmp = TempDir::new().unwrap();
        let owned = tmp.path().join("dev/owned");
        std::fs::create_dir_all(&owned).unwrap();
        git(&owned, &["init", "-q"]);
        git(
            &owned,
            &["remote", "add", "origin", "git@github.com:o/r.git"],
        );

        let config = config_with_workdir(&tmp.path().join("dev"));
        let mut store = RepoStore::load(&paths_in(tmp.path()), &config).unwrap();
        let canon = store
            .resolve(&repo_ref(&["https://github.com/o/r"]), OnMissing::Ask)
            .unwrap();

        assert_eq!(canon.path, owned);
        assert_eq!(canon.vcs, VcsKind::Git);
        assert!(
            !tmp.path().join("data").join("repos").exists(),
            "should not have cloned anything"
        );
    }

    #[test]
    fn a_cache_miss_triggers_a_rescan_that_picks_up_a_newly_cloned_repo() {
        let tmp = TempDir::new().unwrap();
        let dev = tmp.path().join("dev");
        std::fs::create_dir_all(&dev).unwrap();

        let config = config_with_workdir(&dev);
        let paths = paths_in(tmp.path());
        let mut store = RepoStore::load(&paths, &config).unwrap();

        // First lookup: nothing in `dev` yet, and no config to clone from - `Ask` reports
        // `NeedsClone` (this also seeds the workdir cache as "empty, just scanned").
        let err = store
            .resolve(&repo_ref(&["https://example.com/o/r"]), OnMissing::Ask)
            .unwrap_err();
        assert!(err.downcast_ref::<NeedsClone>().is_some());

        // The repo shows up in `dev` after that (e.g. the user cloned it by hand). A fresh
        // `RepoStore` (as a later `rq fetch` would construct) still has the stale cache on disk,
        // but it's older than the throttle window, so resolving rescans and finds it.
        let repo = dev.join("owned");
        std::fs::create_dir_all(&repo).unwrap();
        git(&repo, &["init", "-q"]);
        git(
            &repo,
            &["remote", "add", "origin", "https://example.com/o/r"],
        );
        backdate_cache(&paths);

        let mut store2 = RepoStore::load(&paths, &config).unwrap();
        let canon = store2
            .resolve(&repo_ref(&["https://example.com/o/r"]), OnMissing::Ask)
            .unwrap();
        assert_eq!(canon.path, repo);
    }

    /// Force the on-disk workdir cache's `scanned_at` far enough into the past to clear
    /// `RESCAN_THROTTLE`, simulating "it's been a while since the last scan" without sleeping.
    fn backdate_cache(paths: &Paths) {
        let mut cache = WorkdirCache::load(&paths.workdir_cache_file())
            .unwrap()
            .unwrap();
        cache.scanned_at -= RESCAN_THROTTLE * 2;
        cache.save(&paths.workdir_cache_file()).unwrap();
    }

    #[test]
    fn a_cached_path_that_no_longer_exists_is_treated_as_a_miss() {
        let tmp = TempDir::new().unwrap();
        let paths = paths_in(tmp.path());
        let dev = tmp.path().join("dev");
        std::fs::create_dir_all(&dev).unwrap();
        let config = config_with_workdir(&dev);

        let cache = WorkdirCache {
            workdir: dev.clone(),
            scanned_at: chrono::Utc::now(),
            repos: vec![DiscoveredRepo {
                path: dev.join("gone"),
                vcs: VcsKind::Git,
                name: "example.com/o/r".into(),
                remotes: vec!["example.com/o/r".into()],
            }],
        };
        cache.save(&paths.workdir_cache_file()).unwrap();

        let mut store = RepoStore::load(&paths, &config).unwrap();
        let err = store
            .resolve(&repo_ref(&["https://example.com/o/r"]), OnMissing::Ask)
            .unwrap_err();
        assert!(err.downcast_ref::<NeedsClone>().is_some());
    }

    /// Regression test: a cache scanned under a workdir the user has since changed (or removed
    /// from config) must not be trusted, even though the directory it recorded still exists on
    /// disk - otherwise turning `workdir` off doesn't actually stop rq from using it.
    #[test]
    fn a_cache_from_a_different_workdir_is_treated_as_a_miss() {
        let tmp = TempDir::new().unwrap();
        let paths = paths_in(tmp.path());
        let old_dev = tmp.path().join("old-dev");
        let repo = old_dev.join("owned");
        std::fs::create_dir_all(&repo).unwrap();
        git(&repo, &["init", "-q"]);
        git(
            &repo,
            &["remote", "add", "origin", "https://example.com/o/r"],
        );

        let cache = WorkdirCache {
            workdir: old_dev,
            scanned_at: chrono::Utc::now(),
            repos: vec![DiscoveredRepo {
                path: repo,
                vcs: VcsKind::Git,
                name: "example.com/o/r".into(),
                remotes: vec!["example.com/o/r".into()],
            }],
        };
        cache.save(&paths.workdir_cache_file()).unwrap();

        // No `workdir` configured now (e.g. the user removed it from config.toml).
        let config = Config::default();
        let mut store = RepoStore::load(&paths, &config).unwrap();
        let err = store
            .resolve(&repo_ref(&["https://example.com/o/r"]), OnMissing::Ask)
            .unwrap_err();
        assert!(
            err.downcast_ref::<NeedsClone>().is_some(),
            "must not serve a repo cached under a different workdir"
        );
    }

    #[test]
    fn ask_reports_needs_clone_without_touching_the_filesystem() {
        let tmp = TempDir::new().unwrap();
        let paths = paths_in(tmp.path());
        let config = Config::default();
        let mut store = RepoStore::load(&paths, &config).unwrap();

        let err = store
            .resolve(&repo_ref(&["https://example.com/o/r"]), OnMissing::Ask)
            .unwrap_err();
        let needs_clone = err.downcast_ref::<NeedsClone>().unwrap();
        assert_eq!(needs_clone.url, "https://example.com/o/r");
        assert!(!needs_clone.dest.exists());
        assert!(!paths.repos_file().exists());
    }

    #[test]
    fn clones_and_reuses_a_tool_managed_repo() {
        let tmp = TempDir::new().unwrap();
        let upstream = tmp.path().join("upstream");
        std::fs::create_dir(&upstream).unwrap();
        git(&upstream, &["init", "-q", "-b", "main"]);
        git(&upstream, &["config", "user.name", "test"]);
        git(&upstream, &["config", "user.email", "test@example.com"]);
        git(&upstream, &["commit", "-q", "--allow-empty", "-m", "base"]);

        let paths = paths_in(tmp.path());
        let config = Config::default();
        let url = upstream.to_string_lossy().to_string();

        let mut store = RepoStore::load(&paths, &config).unwrap();
        let canon1 = store.resolve(&repo_ref(&[&url]), OnMissing::Clone).unwrap();
        assert_eq!(canon1.vcs, VcsKind::Git);
        assert!(canon1.path.join(".git").exists());
        store.save().unwrap();

        // A fresh store (simulating a later `rq sync` run) reuses the registered clone rather
        // than cloning again.
        let mut store2 = RepoStore::load(&paths, &config).unwrap();
        let canon2 = store2
            .resolve(&repo_ref(&[&url]), OnMissing::Clone)
            .unwrap();
        assert_eq!(canon2.path, canon1.path);

        // Reusing it via a differently-formed but equivalent URL also hits the same registry
        // entry, not a second clone.
        let canon3 = store2
            .resolve(&repo_ref(&[&format!("{url}.git")]), OnMissing::Clone)
            .unwrap();
        assert_eq!(canon3.path, canon1.path);
    }

    /// Regression test: a prior run that cloned a repo but crashed before saving `repos.json`
    /// left the registry unaware of a clone that already exists on disk. `resolve()` must reuse
    /// it rather than trying (and failing) to clone into the same non-empty directory again.
    #[test]
    fn resolve_self_heals_an_unregistered_but_already_cloned_directory() {
        let tmp = TempDir::new().unwrap();
        let upstream = tmp.path().join("upstream");
        std::fs::create_dir(&upstream).unwrap();
        git(&upstream, &["init", "-q", "-b", "main"]);
        git(&upstream, &["config", "user.name", "test"]);
        git(&upstream, &["config", "user.email", "test@example.com"]);
        git(&upstream, &["commit", "-q", "--allow-empty", "-m", "base"]);

        let paths = paths_in(tmp.path());
        let config = Config::default();
        let url = upstream.to_string_lossy().to_string();

        // Simulate the interrupted-prior-run scenario directly: clone to the exact path
        // `resolve()` would use, but never register it.
        let dest = paths.repo_dir(&normalize_url(&url));
        std::fs::create_dir_all(dest.parent().unwrap()).unwrap();
        git(tmp.path(), &["clone", "-q", &url, dest.to_str().unwrap()]);
        assert!(!paths.repos_file().exists());

        let mut store = RepoStore::load(&paths, &config).unwrap();
        let canon = store.resolve(&repo_ref(&[&url]), OnMissing::Clone).unwrap();

        assert_eq!(canon.path, dest);
        assert!(canon.path.join(".git").exists());
    }

    fn state_using(repo_path: &Path) -> crate::state::State {
        let mut state = crate::state::State::default();
        let mut entry = entry_using();
        entry.stack_id = Some("phab/D1".into());
        state.insert(entry);
        state.insert_workspace(
            "phab/D1".into(),
            crate::state::Workspace {
                repo_path: repo_path.to_path_buf(),
                vcs: VcsKind::Git,
                workspace_path: PathBuf::from("/tmp/ws/D1"),
                head_id: "abc".into(),
                status: crate::state::Status::Ready,
                tip: crate::state::ReviewKey::new("phab", "D1"),
                version: "1".into(),
            },
        );
        state
    }

    fn entry_using() -> crate::state::ReviewEntry {
        crate::state::ReviewEntry {
            key: crate::state::ReviewKey::new("phab", "D1"),
            title: "x".into(),
            author: "a".into(),
            url: "https://example.com/D1".into(),
            repo: RepoRef {
                urls: vec!["https://example.com/o/r".into()],
                display_name: "o/r".into(),
            },
            kind: crate::source::ReviewKind::Direct,
            version: "1".into(),
            in_queue: true,
            resolved: false,
            last_synced: chrono::Utc::now(),
            stack_id: None,
            ancestors: Vec::new(),
            diff_stat: None,
            description: None,
        }
    }

    #[test]
    fn list_reports_discovered_and_registry_repos_with_workspace_counts() {
        let tmp = TempDir::new().unwrap();
        let owned = tmp.path().join("dev/owned");
        std::fs::create_dir_all(&owned).unwrap();
        git(&owned, &["init", "-q"]);
        git(
            &owned,
            &["remote", "add", "origin", "https://example.com/o/r"],
        );

        let config = config_with_workdir(&tmp.path().join("dev"));
        let paths = paths_in(tmp.path());
        let mut store = RepoStore::load(&paths, &config).unwrap();
        // Force a scan so `list()` has something to report.
        store.rescan().unwrap();

        let state = state_using(&owned);

        let repos = store.list(&state);
        assert_eq!(repos.len(), 1);
        assert_eq!(repos[0].kind, RepoKind::Discovered);
        assert_eq!(repos[0].workspace_count, 1);
    }

    #[test]
    fn remove_refuses_for_a_discovered_repo() {
        let tmp = TempDir::new().unwrap();
        let owned = tmp.path().join("dev/owned");
        std::fs::create_dir_all(&owned).unwrap();
        git(&owned, &["init", "-q"]);
        git(
            &owned,
            &["remote", "add", "origin", "https://example.com/o/r"],
        );

        let config = config_with_workdir(&tmp.path().join("dev"));
        let paths = paths_in(tmp.path());
        let mut store = RepoStore::load(&paths, &config).unwrap();
        store.rescan().unwrap();
        let state = crate::state::State::default();

        let err = store.remove("https://example.com/o/r", &state).unwrap_err();
        assert!(
            err.to_string().contains("workdir"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn remove_refuses_while_workspaces_exist() {
        let tmp = TempDir::new().unwrap();
        let upstream = tmp.path().join("upstream");
        std::fs::create_dir(&upstream).unwrap();
        git(&upstream, &["init", "-q", "-b", "main"]);
        git(&upstream, &["config", "user.name", "test"]);
        git(&upstream, &["config", "user.email", "test@example.com"]);
        git(&upstream, &["commit", "-q", "--allow-empty", "-m", "base"]);
        let url = upstream.to_string_lossy().to_string();

        let paths = paths_in(tmp.path());
        let config = Config::default();
        let mut store = RepoStore::load(&paths, &config).unwrap();
        let canon = store.resolve(&repo_ref(&[&url]), OnMissing::Clone).unwrap();

        let state = state_using(&canon.path);

        let err = store.remove(&url, &state).unwrap_err();
        assert!(
            err.to_string().contains("still has workspaces"),
            "unexpected error: {err}"
        );
        assert!(canon.path.exists(), "must not delete while still in use");
    }

    #[test]
    fn remove_deletes_an_unused_tool_managed_clone() {
        let tmp = TempDir::new().unwrap();
        let upstream = tmp.path().join("upstream");
        std::fs::create_dir(&upstream).unwrap();
        git(&upstream, &["init", "-q", "-b", "main"]);
        git(&upstream, &["config", "user.name", "test"]);
        git(&upstream, &["config", "user.email", "test@example.com"]);
        git(&upstream, &["commit", "-q", "--allow-empty", "-m", "base"]);
        let url = upstream.to_string_lossy().to_string();

        let paths = paths_in(tmp.path());
        let config = Config::default();
        let mut store = RepoStore::load(&paths, &config).unwrap();
        let canon = store.resolve(&repo_ref(&[&url]), OnMissing::Clone).unwrap();
        store.save().unwrap();

        let state = crate::state::State::default();
        store.remove(&url, &state).unwrap();
        store.save().unwrap();

        assert!(!canon.path.exists());
        let mut reloaded = RepoStore::load(&paths, &config).unwrap();
        // A removed clone is forgotten, not just deleted - resolving again clones fresh.
        let canon2 = reloaded
            .resolve(&repo_ref(&[&url]), OnMissing::Clone)
            .unwrap();
        assert!(canon2.path.exists());
    }
}
