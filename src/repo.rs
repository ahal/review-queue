//! Canonical repo resolution: every review repo maps to exactly one canonical local repo, which
//! `sync`/`prune` never delete - only the worktrees/workspaces created from it. A canonical repo
//! is either user-owned (an explicit `[[repo]]` config entry pointing at an existing checkout)
//! or tool-managed (cloned once under `Paths::repo_cache_dir()` and recorded in `repos.json`).
//!
//! Lookup order: config `[[repo]]`, then the `repos.json` registry, then clone a new one (always
//! plain git - only a pre-existing user-owned checkout can be jj) and register it.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::config::{Config, RepoConfig, VcsKind};
use crate::paths::Paths;
use crate::source::RepoRef;
use crate::state::State;

/// A resolved local repo that review workspaces are built from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CanonicalRepo {
    pub path: PathBuf,
    pub vcs: VcsKind,
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

pub struct RepoStore {
    registry_path: PathBuf,
    registry: Registry,
    config_repos: Vec<RepoConfig>,
    cache_dir: PathBuf,
}

impl RepoStore {
    pub fn load(paths: &Paths, config: &Config) -> Result<Self> {
        Ok(Self {
            registry_path: paths.repos_file(),
            registry: Registry::load(&paths.repos_file())?,
            config_repos: config.repos.clone(),
            cache_dir: paths.repo_cache_dir(),
        })
    }

    /// Resolve `repo_ref` to a canonical local repo: a matching `[[repo]]` config entry, a
    /// previously registered tool-managed clone, or - failing both - a fresh clone that gets
    /// registered for next time.
    pub fn resolve(&mut self, repo_ref: &RepoRef) -> Result<CanonicalRepo> {
        let normalized: Vec<String> = repo_ref.urls.iter().map(|u| normalize_url(u)).collect();

        for rc in &self.config_repos {
            let rc_normalized: Vec<String> = rc.urls.iter().map(|u| normalize_url(u)).collect();
            if normalized.iter().any(|n| rc_normalized.contains(n)) {
                let vcs = rc.vcs.unwrap_or_else(|| detect_vcs(&rc.path));
                return Ok(CanonicalRepo {
                    path: rc.path.clone(),
                    vcs,
                });
            }
        }

        for n in &normalized {
            if let Some(entry) = self.registry.repos.get(n)
                && entry.path.exists()
            {
                return Ok(CanonicalRepo {
                    path: entry.path.clone(),
                    vcs: VcsKind::Git,
                });
            }
        }

        let url = repo_ref
            .urls
            .first()
            .context("review's repo has no candidate URLs to clone")?;
        let dest = self.cache_dir.join(normalize_url(url));
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
            normalize_url(url),
            RegistryEntry {
                path: dest.clone(),
                origin_url: url.clone(),
                created: chrono::Utc::now(),
            },
        );
        Ok(CanonicalRepo {
            path: dest,
            vcs: VcsKind::Git,
        })
    }

    pub fn save(&self) -> Result<()> {
        self.registry.save(&self.registry_path)
    }

    /// Every canonical repo (user-owned and tool-managed), with how many tracked workspaces
    /// currently point at it - for `rq repo list`.
    pub fn list(&self, state: &State) -> Vec<RepoListEntry> {
        let mut out = Vec::new();
        for rc in &self.config_repos {
            let workspace_count = state.iter().filter(|e| e.repo_path == rc.path).count();
            out.push(RepoListEntry {
                url: rc.urls.first().cloned().unwrap_or_default(),
                path: rc.path.clone(),
                user_owned: true,
                workspace_count,
            });
        }
        for (url, entry) in &self.registry.repos {
            let workspace_count = state.iter().filter(|e| e.repo_path == entry.path).count();
            out.push(RepoListEntry {
                url: url.clone(),
                path: entry.path.clone(),
                user_owned: false,
                workspace_count,
            });
        }
        out
    }

    /// Delete a tool-managed clone and forget it. Refuses if `url` is a user-owned `[[repo]]`
    /// entry instead (those are removed by editing `config.toml`, never by this tool), or if any
    /// tracked workspace still points at it.
    pub fn remove(&mut self, url: &str, state: &State) -> Result<()> {
        let normalized = normalize_url(url);
        if self
            .config_repos
            .iter()
            .any(|rc| rc.urls.iter().any(|u| normalize_url(u) == normalized))
        {
            bail!("`{url}` is a user-owned `[[repo]]` entry; remove it from config.toml instead");
        }
        let Some(entry) = self.registry.repos.get(&normalized).cloned() else {
            bail!("no tool-managed clone registered for `{url}` (see `rq repo list`)");
        };
        if state.iter().any(|e| e.repo_path == entry.path) {
            bail!(
                "{} still has workspaces using it; run `rq prune` or remove them first",
                entry.path.display()
            );
        }
        std::fs::remove_dir_all(&entry.path)
            .with_context(|| format!("removing {}", entry.path.display()))?;
        self.registry.repos.remove(&normalized);
        Ok(())
    }
}

#[derive(Debug, Clone)]
pub struct RepoListEntry {
    pub url: String,
    pub path: PathBuf,
    pub user_owned: bool,
    pub workspace_count: usize,
}

fn detect_vcs(path: &Path) -> VcsKind {
    if path.join(".jj").exists() {
        VcsKind::Jj
    } else {
        VcsKind::Git
    }
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
            .with_overrides(Some(tmp.join("data")), Some(tmp.join("cache")))
    }

    fn repo_ref(urls: &[&str]) -> RepoRef {
        RepoRef {
            urls: urls.iter().map(|s| s.to_string()).collect(),
            display_name: "test/repo".into(),
        }
    }

    #[test]
    fn resolves_user_owned_config_repo_without_cloning() {
        let tmp = TempDir::new().unwrap();
        let owned = tmp.path().join("owned");
        std::fs::create_dir(&owned).unwrap();
        git(&owned, &["init", "-q"]);

        let config = Config {
            repos: vec![RepoConfig {
                urls: vec!["https://github.com/o/r".into()],
                path: owned.clone(),
                vcs: None,
            }],
            ..Default::default()
        };
        let mut store = RepoStore::load(&paths_in(tmp.path()), &config).unwrap();
        let canon = store
            .resolve(&repo_ref(&["git@github.com:o/r.git"]))
            .unwrap();

        assert_eq!(canon.path, owned);
        assert_eq!(canon.vcs, VcsKind::Git);
        assert!(
            !tmp.path().join("cache").exists(),
            "should not have cloned anything"
        );
    }

    #[test]
    fn detects_jj_over_git_when_both_present() {
        let tmp = TempDir::new().unwrap();
        let owned = tmp.path().join("owned");
        std::fs::create_dir_all(owned.join(".git")).unwrap();
        std::fs::create_dir_all(owned.join(".jj")).unwrap();

        let config = Config {
            repos: vec![RepoConfig {
                urls: vec!["https://example.com/o/r".into()],
                path: owned.clone(),
                vcs: None,
            }],
            ..Default::default()
        };
        let mut store = RepoStore::load(&paths_in(tmp.path()), &config).unwrap();
        let canon = store
            .resolve(&repo_ref(&["https://example.com/o/r"]))
            .unwrap();

        assert_eq!(canon.vcs, VcsKind::Jj);
    }

    #[test]
    fn explicit_vcs_override_wins_over_autodetection() {
        let tmp = TempDir::new().unwrap();
        let owned = tmp.path().join("owned");
        std::fs::create_dir_all(owned.join(".jj")).unwrap();

        let config = Config {
            repos: vec![RepoConfig {
                urls: vec!["https://example.com/o/r".into()],
                path: owned.clone(),
                vcs: Some(VcsKind::Git),
            }],
            ..Default::default()
        };
        let mut store = RepoStore::load(&paths_in(tmp.path()), &config).unwrap();
        let canon = store
            .resolve(&repo_ref(&["https://example.com/o/r"]))
            .unwrap();

        assert_eq!(
            canon.vcs,
            VcsKind::Git,
            "explicit config override should beat .jj autodetection"
        );
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
        let canon1 = store.resolve(&repo_ref(&[&url])).unwrap();
        assert_eq!(canon1.vcs, VcsKind::Git);
        assert!(canon1.path.join(".git").exists());
        store.save().unwrap();

        // A fresh store (simulating a later `rq sync` run) reuses the registered clone rather
        // than cloning again.
        let mut store2 = RepoStore::load(&paths, &config).unwrap();
        let canon2 = store2.resolve(&repo_ref(&[&url])).unwrap();
        assert_eq!(canon2.path, canon1.path);

        // Reusing it via a differently-formed but equivalent URL also hits the same registry
        // entry, not a second clone.
        let canon3 = store2.resolve(&repo_ref(&[&format!("{url}.git")])).unwrap();
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
        let dest = paths.repo_cache_dir().join(normalize_url(&url));
        std::fs::create_dir_all(dest.parent().unwrap()).unwrap();
        git(tmp.path(), &["clone", "-q", &url, dest.to_str().unwrap()]);
        assert!(!paths.repos_file().exists());

        let mut store = RepoStore::load(&paths, &config).unwrap();
        let canon = store.resolve(&repo_ref(&[&url])).unwrap();

        assert_eq!(canon.path, dest);
        assert!(canon.path.join(".git").exists());
    }

    fn entry_using(repo_path: &Path) -> crate::state::ReviewEntry {
        crate::state::ReviewEntry {
            key: crate::state::ReviewKey::new("moz", "D1"),
            title: "x".into(),
            author: "a".into(),
            url: "https://example.com/D1".into(),
            repo_path: repo_path.to_path_buf(),
            vcs: VcsKind::Git,
            workspace_path: PathBuf::from("/tmp/ws/D1"),
            version: "1".into(),
            head_id: "abc".into(),
            in_queue: true,
            status: crate::state::Status::Ready,
            last_synced: chrono::Utc::now(),
        }
    }

    #[test]
    fn list_reports_config_and_registry_repos_with_workspace_counts() {
        let tmp = TempDir::new().unwrap();
        let owned = tmp.path().join("owned");
        std::fs::create_dir(&owned).unwrap();
        git(&owned, &["init", "-q"]);

        let config = Config {
            repos: vec![RepoConfig {
                urls: vec!["https://example.com/o/r".into()],
                path: owned.clone(),
                vcs: None,
            }],
            ..Default::default()
        };
        let store = RepoStore::load(&paths_in(tmp.path()), &config).unwrap();

        let mut state = crate::state::State::default();
        state.insert(entry_using(&owned));

        let repos = store.list(&state);
        assert_eq!(repos.len(), 1);
        assert!(repos[0].user_owned);
        assert_eq!(repos[0].workspace_count, 1);
    }

    #[test]
    fn remove_refuses_for_user_owned_repo() {
        let tmp = TempDir::new().unwrap();
        let owned = tmp.path().join("owned");
        std::fs::create_dir(&owned).unwrap();
        let config = Config {
            repos: vec![RepoConfig {
                urls: vec!["https://example.com/o/r".into()],
                path: owned,
                vcs: None,
            }],
            ..Default::default()
        };
        let mut store = RepoStore::load(&paths_in(tmp.path()), &config).unwrap();
        let state = crate::state::State::default();

        let err = store.remove("https://example.com/o/r", &state).unwrap_err();
        assert!(
            err.to_string().contains("config.toml"),
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
        let canon = store.resolve(&repo_ref(&[&url])).unwrap();

        let mut state = crate::state::State::default();
        state.insert(entry_using(&canon.path));

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
        let canon = store.resolve(&repo_ref(&[&url])).unwrap();
        store.save().unwrap();

        let state = crate::state::State::default();
        store.remove(&url, &state).unwrap();
        store.save().unwrap();

        assert!(!canon.path.exists());
        let mut reloaded = RepoStore::load(&paths, &config).unwrap();
        // A removed clone is forgotten, not just deleted - resolving again clones fresh.
        let canon2 = reloaded.resolve(&repo_ref(&[&url])).unwrap();
        assert!(canon2.path.exists());
    }
}
