//! Discovering canonical repos by scanning `Config::workdir` instead of requiring them to be
//! cloned by rq or listed by hand.
//!
//! `scan()` walks the tree recursively, stopping at the first repo found in each branch (a repo's
//! own subdirectories - submodules, vendored trees, nested worktrees - are never canonical repos
//! themselves). A directory is a repo if it has a `.jj/repo` directory (jj, colocated or not -
//! checked first so a colocated git+jj repo counts as jj) or a `.git` directory (git). A
//! `.jj/repo` *file* (a secondary `jj workspace add` workspace) or a `.git` *file* (a git
//! worktree) means "not canonical, don't descend either" - the real repo lives elsewhere and
//! will be found from its own position in the tree.
//!
//! Every remote is read and normalized (`crate::repo::normalize_url`), so any URL form
//! (`https://`, `git@host:`, `ssh://`) matches. A repo with no remotes at all can't be matched
//! against anything a review reports, so it's dropped rather than cached. `name` - the id a
//! discovered repo's workspaces live under (`Paths::repo_dir`) - is `origin`'s normalized URL, or
//! the first remote if there's no `origin`, so it stays stable regardless of which alias a review
//! happens to report.
//!
//! The scan result is cached (`WorkdirCache`) so it isn't re-run on every `rq fetch`; see
//! `RepoStore::resolve` for the rescan-on-miss policy.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::config::VcsKind;
use crate::repo::normalize_url;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiscoveredRepo {
    pub path: PathBuf,
    pub vcs: VcsKind,
    /// `Paths::repo_dir` id for this repo - `origin`'s normalized URL, or the first remote's.
    pub name: String,
    /// Every remote's normalized URL (including `name`'s), for matching a review's repo against.
    pub remotes: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkdirCache {
    pub workdir: PathBuf,
    pub scanned_at: DateTime<Utc>,
    pub repos: Vec<DiscoveredRepo>,
}

impl WorkdirCache {
    pub fn load(path: &Path) -> Result<Option<Self>> {
        if !path.exists() {
            return Ok(None);
        }
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("reading {}", path.display()))?;
        if text.trim().is_empty() {
            return Ok(None);
        }
        Ok(Some(
            serde_json::from_str(&text).with_context(|| format!("parsing {}", path.display()))?,
        ))
    }

    /// Atomic write: temp file in the same directory, then rename over the target.
    pub fn save(&self, path: &Path) -> Result<()> {
        let dir = path
            .parent()
            .context("workdir cache path has no parent directory")?;
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

    /// The first cached repo (still present on disk) with a remote matching one of
    /// `normalized_urls`.
    pub fn lookup(&self, normalized_urls: &[String]) -> Option<&DiscoveredRepo> {
        self.repos
            .iter()
            .find(|r| r.path.exists() && r.remotes.iter().any(|u| normalized_urls.contains(u)))
    }
}

/// Recursively scan `root` for canonical repos, skipping `skip` (rq's own data dir, so
/// tool-managed clones never get picked up as if they were user checkouts) and any
/// dot-directory. Errors reading a directory (permissions, races) are treated as "nothing here",
/// not fatal - a workdir scan shouldn't abort over one unreadable subdirectory.
pub fn scan(root: &Path, skip: &Path) -> Vec<DiscoveredRepo> {
    let mut out = Vec::new();
    scan_dir(root, skip, &mut out);
    out
}

fn scan_dir(dir: &Path, skip: &Path, out: &mut Vec<DiscoveredRepo>) {
    if dir == skip {
        return;
    }

    let jj_repo = dir.join(".jj").join("repo");
    if jj_repo.is_dir() {
        if let Some(repo) = discovered_jj_repo(dir) {
            out.push(repo);
        }
        return;
    }
    if jj_repo.is_file() {
        return; // secondary `jj workspace add` workspace - the real repo is elsewhere
    }

    let git_dir = dir.join(".git");
    if git_dir.is_dir() {
        if let Some(repo) = discovered_git_repo(dir) {
            out.push(repo);
        }
        return;
    }
    if git_dir.is_file() {
        return; // git worktree - the real repo is elsewhere
    }

    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if file_type.is_symlink() || !file_type.is_dir() {
            continue;
        }
        let path = entry.path();
        if path
            .file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| n.starts_with('.'))
        {
            continue;
        }
        scan_dir(&path, skip, out);
    }
}

fn discovered_git_repo(dir: &Path) -> Option<DiscoveredRepo> {
    let output = Command::new("git")
        .current_dir(dir)
        .args(["config", "--get-regexp", r"^remote\..*\.url$"])
        .output()
        .ok()?;
    // A repo with no remotes at all makes `--get-regexp` exit non-zero (nothing matched).
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let mut remotes = Vec::new();
    for line in text.lines() {
        let Some((key, url)) = line.split_once(' ') else {
            continue;
        };
        let Some(name) = key
            .strip_prefix("remote.")
            .and_then(|s| s.strip_suffix(".url"))
        else {
            continue;
        };
        remotes.push((name.to_string(), url.to_string()));
    }
    build_discovered(dir.to_path_buf(), VcsKind::Git, remotes)
}

fn discovered_jj_repo(dir: &Path) -> Option<DiscoveredRepo> {
    let output = Command::new("jj")
        .current_dir(dir)
        .args(["git", "remote", "list"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let mut remotes = Vec::new();
    for line in text.lines() {
        let mut parts = line.split_whitespace();
        let (Some(name), Some(url)) = (parts.next(), parts.next()) else {
            continue;
        };
        remotes.push((name.to_string(), url.to_string()));
    }
    build_discovered(dir.to_path_buf(), VcsKind::Jj, remotes)
}

fn build_discovered(
    path: PathBuf,
    vcs: VcsKind,
    remotes: Vec<(String, String)>,
) -> Option<DiscoveredRepo> {
    if remotes.is_empty() {
        return None;
    }
    let normalized: Vec<String> = remotes.iter().map(|(_, u)| normalize_url(u)).collect();
    let name = remotes
        .iter()
        .position(|(name, _)| name == "origin")
        .unwrap_or(0);
    Some(DiscoveredRepo {
        path,
        vcs,
        name: normalized[name].clone(),
        remotes: normalized,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;
    use tempfile::TempDir;

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

    fn git_repo_with_remote(dir: &Path, remote_url: &str) {
        std::fs::create_dir_all(dir).unwrap();
        git(dir, &["init", "-q"]);
        git(dir, &["remote", "add", "origin", remote_url]);
    }

    fn jj_available() -> bool {
        Command::new("jj")
            .arg("--version")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    }

    macro_rules! require_jj {
        () => {
            if !jj_available() {
                eprintln!("skipping: `jj` not found on PATH");
                return;
            }
        };
    }

    #[test]
    fn finds_a_nested_repo_and_normalizes_its_remote() {
        let tmp = TempDir::new().unwrap();
        let repo = tmp.path().join("a/b/repo");
        git_repo_with_remote(&repo, "git@github.com:o/r.git");

        let found = scan(tmp.path(), Path::new("/nonexistent"));
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].path, repo);
        assert_eq!(found[0].vcs, VcsKind::Git);
        assert_eq!(found[0].name, "github.com/o/r");
        assert_eq!(found[0].remotes, vec!["github.com/o/r".to_string()]);
    }

    #[test]
    fn does_not_descend_into_a_found_repo() {
        let tmp = TempDir::new().unwrap();
        let outer = tmp.path().join("outer");
        git_repo_with_remote(&outer, "https://example.com/o/outer");
        // A vendored/nested repo inside it must not be reported separately.
        git_repo_with_remote(&outer.join("vendor/nested"), "https://example.com/o/nested");

        let found = scan(tmp.path(), Path::new("/nonexistent"));
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].path, outer);
    }

    #[test]
    fn skips_dot_directories_and_the_data_dir() {
        let tmp = TempDir::new().unwrap();
        std::fs::create_dir_all(tmp.path().join(".cache/repo")).unwrap();
        git_repo_with_remote(
            &tmp.path().join(".cache/repo"),
            "https://example.com/o/hidden",
        );
        let skip_repo = tmp.path().join("data/repos/foo/source");
        git_repo_with_remote(&skip_repo, "https://example.com/o/skip");

        let found = scan(tmp.path(), &tmp.path().join("data/repos"));
        assert!(found.is_empty());
    }

    #[test]
    fn drops_a_repo_with_no_remotes() {
        let tmp = TempDir::new().unwrap();
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        git(&repo, &["init", "-q"]);

        assert!(scan(tmp.path(), Path::new("/nonexistent")).is_empty());
    }

    #[test]
    fn skips_a_git_worktree() {
        let tmp = TempDir::new().unwrap();
        let repo = tmp.path().join("repo");
        git_repo_with_remote(&repo, "https://example.com/o/r");
        git(&repo, &["config", "user.name", "test"]);
        git(&repo, &["config", "user.email", "test@example.com"]);
        git(&repo, &["commit", "-q", "--allow-empty", "-m", "base"]);
        let worktree = tmp.path().join("worktree");
        git(
            &repo,
            &["worktree", "add", "--detach", worktree.to_str().unwrap()],
        );

        let found = scan(tmp.path(), Path::new("/nonexistent"));
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].path, repo);
    }

    #[test]
    fn prefers_origin_as_the_name_over_other_remotes() {
        let tmp = TempDir::new().unwrap();
        let repo = tmp.path().join("repo");
        git_repo_with_remote(&repo, "https://example.com/fork/r");
        git(&repo, &["remote", "add", "upstream", "https://example.com/o/r"]);

        let found = scan(tmp.path(), Path::new("/nonexistent"));
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].name, "example.com/fork/r");
        assert_eq!(
            found[0].remotes.iter().collect::<std::collections::HashSet<_>>(),
            std::collections::HashSet::from([
                &"example.com/fork/r".to_string(),
                &"example.com/o/r".to_string(),
            ])
        );
    }

    #[test]
    fn lookup_ignores_a_cached_entry_whose_path_no_longer_exists() {
        let cache = WorkdirCache {
            workdir: PathBuf::from("/dev"),
            scanned_at: Utc::now(),
            repos: vec![DiscoveredRepo {
                path: PathBuf::from("/nonexistent/gone"),
                vcs: VcsKind::Git,
                name: "example.com/o/r".into(),
                remotes: vec!["example.com/o/r".into()],
            }],
        };
        assert!(cache.lookup(&["example.com/o/r".to_string()]).is_none());
    }

    #[test]
    fn colocated_jj_repo_is_detected_as_jj_not_git() {
        require_jj!();
        let tmp = TempDir::new().unwrap();
        let upstream = tmp.path().join("upstream");
        std::fs::create_dir(&upstream).unwrap();
        git(&upstream, &["init", "-q", "-b", "main"]);
        git(&upstream, &["config", "user.name", "test"]);
        git(&upstream, &["config", "user.email", "test@example.com"]);
        git(&upstream, &["commit", "-q", "--allow-empty", "-m", "base"]);

        let canon = tmp.path().join("canon");
        let out = Command::new("jj")
            .current_dir(tmp.path())
            .args([
                "git",
                "clone",
                upstream.to_str().unwrap(),
                canon.to_str().unwrap(),
            ])
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "jj git clone failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );

        let found = scan(tmp.path(), Path::new("/nonexistent"));
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].path, canon);
        assert_eq!(found[0].vcs, VcsKind::Jj, "colocated repo must count as jj");
        assert_eq!(found[0].name, normalize_url(upstream.to_str().unwrap()));
    }

    #[test]
    fn skips_a_secondary_jj_workspace() {
        require_jj!();
        let tmp = TempDir::new().unwrap();
        let upstream = tmp.path().join("upstream");
        std::fs::create_dir(&upstream).unwrap();
        git(&upstream, &["init", "-q", "-b", "main"]);
        git(&upstream, &["config", "user.name", "test"]);
        git(&upstream, &["config", "user.email", "test@example.com"]);
        git(&upstream, &["commit", "-q", "--allow-empty", "-m", "base"]);

        let canon = tmp.path().join("canon");
        let clone = Command::new("jj")
            .current_dir(tmp.path())
            .args([
                "git",
                "clone",
                upstream.to_str().unwrap(),
                canon.to_str().unwrap(),
            ])
            .output()
            .unwrap();
        assert!(clone.status.success());

        let secondary = tmp.path().join("secondary");
        let add = Command::new("jj")
            .current_dir(&canon)
            .args([
                "workspace",
                "add",
                "--name",
                "secondary",
                secondary.to_str().unwrap(),
            ])
            .output()
            .unwrap();
        assert!(
            add.status.success(),
            "jj workspace add failed: {}",
            String::from_utf8_lossy(&add.stderr)
        );

        let found = scan(tmp.path(), Path::new("/nonexistent"));
        assert_eq!(found.len(), 1, "only the canonical repo, not the workspace");
        assert_eq!(found[0].path, canon);
    }
}
