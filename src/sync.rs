//! The sync engine: source-agnostic glue between `ReviewSource`, `RepoStore`, and the `Vcs`
//! backends. `rq sync` runs this once per invocation (see the design plan for why sync is
//! on-demand rather than a daemon).
//!
//! Per source, per run:
//! 1. `fetch_queue()` - reviews currently waiting on you.
//! 2. New reviews get a workspace (`add_workspace`).
//! 3. Reviews already tracked whose `version` changed get updated in place if clean
//!    (`update_workspace`); dirty ones are flagged instead of touched.
//! 4. Reviews still in the queue but otherwise unchanged just get `in_queue` refreshed.
//!
//! Then, once per source, for tracked reviews that weren't in this run's queue (you acted on
//! them - approved, requested changes - so they dropped out): `fetch_status()` tells us whether
//! the review itself resolved (landed/closed/abandoned/merged). Resolved and clean -> the
//! workspace is removed. Resolved and dirty, or still open -> kept, `in_queue = false`.
//!
//! A `sync.lock` (`fd-lock`, guarding overlapping cron runs) and the standalone `rq prune`
//! command are deferred; this module only implements what a single `rq sync` run needs.

use std::collections::{BTreeSet, HashMap};

use anyhow::{Context, Result};

use crate::config::{Config, VcsKind};
use crate::paths::Paths;
use crate::repo::RepoStore;
use crate::source::{Lifecycle, Review, ReviewSource};
use crate::state::{ReviewEntry, ReviewKey, State, Status};
use crate::vcs::Vcs;
use crate::vcs::git::GitVcs;
use crate::vcs::jj::JjVcs;

#[derive(Debug, Default)]
pub struct SyncReport {
    pub added: Vec<ReviewKey>,
    pub updated: Vec<ReviewKey>,
    pub removed: Vec<ReviewKey>,
    /// Review + human-readable reason it was flagged instead of touched (dirty, apply failure).
    pub flagged: Vec<(ReviewKey, String)>,
    pub errors: Vec<(ReviewKey, String)>,
}

fn vcs_for(kind: VcsKind) -> Box<dyn Vcs> {
    match kind {
        VcsKind::Git => Box::new(GitVcs),
        VcsKind::Jj => Box::new(JjVcs),
    }
}

pub async fn sync(
    sources: &[Box<dyn ReviewSource>],
    paths: &Paths,
    config: &Config,
    only_source: Option<&str>,
    dry_run: bool,
) -> Result<SyncReport> {
    paths.ensure_dirs()?;
    let mut state = State::load(&paths.state_file())?;
    let mut repo_store = RepoStore::load(paths, config)?;
    let mut report = SyncReport::default();
    let mut seen: BTreeSet<ReviewKey> = BTreeSet::new();

    for source in sources {
        if only_source.is_some_and(|o| o != source.name()) {
            continue;
        }
        let queue = source
            .fetch_queue()
            .await
            .with_context(|| format!("fetching queue from `{}`", source.name()))?;
        for review in queue {
            seen.insert(review.key.clone());
            if let Err(e) = sync_one(
                source.as_ref(),
                &review,
                paths,
                &mut repo_store,
                &mut state,
                dry_run,
                &mut report,
            )
            .await
            {
                report.errors.push((review.key.clone(), e.to_string()));
            }
        }
    }

    let sources_by_name: HashMap<&str, &Box<dyn ReviewSource>> =
        sources.iter().map(|s| (s.name(), s)).collect();
    let mut ids_by_source: HashMap<String, Vec<String>> = HashMap::new();
    for entry in state.iter() {
        if seen.contains(&entry.key) {
            continue;
        }
        if only_source.is_some_and(|o| o != entry.key.source) {
            continue;
        }
        ids_by_source
            .entry(entry.key.source.clone())
            .or_default()
            .push(entry.key.id.clone());
    }

    for (source_name, ids) in ids_by_source {
        let Some(source) = sources_by_name.get(source_name.as_str()) else {
            continue;
        };
        let statuses = source
            .fetch_status(&ids)
            .await
            .with_context(|| format!("checking status from `{source_name}`"))?;
        for (id, lifecycle) in statuses {
            let key = ReviewKey::new(source_name.clone(), id);
            handle_out_of_queue(&key, lifecycle, &mut state, dry_run, &mut report)?;
        }
    }

    if !dry_run {
        state.save(&paths.state_file())?;
        repo_store.save()?;
    }
    Ok(report)
}

async fn sync_one(
    source: &dyn ReviewSource,
    review: &Review,
    paths: &Paths,
    repo_store: &mut RepoStore,
    state: &mut State,
    dry_run: bool,
    report: &mut SyncReport,
) -> Result<()> {
    match state.get(&review.key).cloned() {
        None => add_new(source, review, paths, repo_store, state, dry_run, report).await,
        Some(entry) => update_existing(source, review, entry, state, dry_run, report).await,
    }
}

async fn add_new(
    source: &dyn ReviewSource,
    review: &Review,
    paths: &Paths,
    repo_store: &mut RepoStore,
    state: &mut State,
    dry_run: bool,
    report: &mut SyncReport,
) -> Result<()> {
    if dry_run {
        // `repo_store.resolve()` can clone a fresh canonical repo - a real side effect that a
        // dry run must not have, even though it would also happen for a real sync of this review.
        report.added.push(review.key.clone());
        return Ok(());
    }

    let canon = repo_store.resolve(&review.repo)?;
    let checkout = source.checkout_spec(review, &canon.path).await?;
    let vcs = vcs_for(canon.vcs);
    let ws = paths.workspace_dir(&review.key.source, &review.key.id);
    if let Some(parent) = ws.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let base_entry = |status: Status, head_id: String| ReviewEntry {
        key: review.key.clone(),
        title: review.title.clone(),
        author: review.author.clone(),
        url: review.url.clone(),
        repo_path: canon.path.clone(),
        vcs: canon.vcs,
        workspace_path: ws.clone(),
        version: review.version.clone(),
        head_id,
        in_queue: true,
        status,
        last_synced: chrono::Utc::now(),
    };

    match vcs.add_workspace(
        &canon.path,
        &ws,
        &checkout,
        &review.key.slug(),
        &review.version,
    ) {
        Ok(head) => {
            state.insert(base_entry(Status::Ready, head));
            report.added.push(review.key.clone());
        }
        Err(e) => {
            // Recorded anyway (with the workspace left in place, per the vcs backends' own
            // contract) so `rq list`/`rq path` can point at it for inspection.
            state.insert(base_entry(Status::ApplyFailed, String::new()));
            report.errors.push((review.key.clone(), e.to_string()));
        }
    }
    Ok(())
}

async fn update_existing(
    source: &dyn ReviewSource,
    review: &Review,
    mut entry: ReviewEntry,
    state: &mut State,
    dry_run: bool,
    report: &mut SyncReport,
) -> Result<()> {
    entry.title = review.title.clone();
    entry.author = review.author.clone();
    entry.url = review.url.clone();
    entry.in_queue = true;

    if entry.version == review.version {
        entry.last_synced = chrono::Utc::now();
        if !dry_run {
            state.insert(entry);
        }
        return Ok(());
    }

    let vcs = vcs_for(entry.vcs);
    if vcs
        .is_dirty(&entry.workspace_path, &entry.head_id)
        .unwrap_or(true)
    {
        entry.status = Status::Dirty;
        entry.last_synced = chrono::Utc::now();
        report.flagged.push((
            review.key.clone(),
            "local changes; not updated to the new version".into(),
        ));
        if !dry_run {
            state.insert(entry);
        }
        return Ok(());
    }

    if dry_run {
        report.updated.push(review.key.clone());
        return Ok(());
    }

    let checkout = source.checkout_spec(review, &entry.repo_path).await?;
    match vcs.update_workspace(
        &entry.repo_path,
        &entry.workspace_path,
        &checkout,
        &review.key.slug(),
        &review.version,
    ) {
        Ok(head) => {
            entry.version = review.version.clone();
            entry.head_id = head;
            entry.status = Status::Ready;
            entry.last_synced = chrono::Utc::now();
            state.insert(entry);
            report.updated.push(review.key.clone());
        }
        Err(e) => {
            entry.status = Status::ApplyFailed;
            entry.last_synced = chrono::Utc::now();
            state.insert(entry);
            report.errors.push((review.key.clone(), e.to_string()));
        }
    }
    Ok(())
}

fn handle_out_of_queue(
    key: &ReviewKey,
    lifecycle: Lifecycle,
    state: &mut State,
    dry_run: bool,
    report: &mut SyncReport,
) -> Result<()> {
    let Some(mut entry) = state.get(key).cloned() else {
        return Ok(());
    };
    entry.in_queue = false;
    entry.last_synced = chrono::Utc::now();

    match lifecycle {
        Lifecycle::Open => {
            if !dry_run {
                state.insert(entry);
            }
        }
        Lifecycle::Resolved => {
            let vcs = vcs_for(entry.vcs);
            if vcs
                .is_dirty(&entry.workspace_path, &entry.head_id)
                .unwrap_or(true)
            {
                entry.status = Status::Resolved;
                report.flagged.push((
                    key.clone(),
                    "resolved but has local changes; workspace kept".into(),
                ));
                if !dry_run {
                    state.insert(entry);
                }
            } else {
                if !dry_run {
                    vcs.remove_workspace(
                        &entry.repo_path,
                        &entry.workspace_path,
                        &key.slug(),
                        false,
                    )?;
                    state.remove(key);
                }
                report.removed.push(key.clone());
            }
        }
    }
    Ok(())
}

#[derive(Debug, Default)]
pub struct PruneReport {
    pub removed: Vec<ReviewKey>,
    /// Dirty and not force-removed - left in place.
    pub kept_dirty: Vec<ReviewKey>,
}

/// Remove workspaces: every `Status::Resolved` one if `ids` is empty, or specifically the
/// (unique-prefix-resolved) reviews named in `ids` regardless of their status. A dirty workspace
/// is left in place (reported via `kept_dirty`) unless `force` is set.
pub fn prune(paths: &Paths, force: bool, ids: &[String]) -> Result<PruneReport> {
    let mut state = State::load(&paths.state_file())?;
    let mut report = PruneReport::default();

    let targets: Vec<ReviewKey> = if ids.is_empty() {
        state
            .iter()
            .filter(|e| e.status == Status::Resolved)
            .map(|e| e.key.clone())
            .collect()
    } else {
        let mut keys = Vec::new();
        for id in ids {
            match state.find_by_prefix(id).as_slice() {
                [] => anyhow::bail!("no tracked review matches `{id}`"),
                [entry] => keys.push(entry.key.clone()),
                many => {
                    let slugs: Vec<_> = many.iter().map(|e| e.key.slug()).collect();
                    anyhow::bail!("`{id}` matches multiple reviews: {}", slugs.join(", "));
                }
            }
        }
        keys
    };

    for key in targets {
        let entry = state
            .get(&key)
            .expect("key was just resolved from this state")
            .clone();

        if !entry.workspace_path.exists() {
            state.remove(&key);
            report.removed.push(key);
            continue;
        }

        let vcs = vcs_for(entry.vcs);
        let dirty = vcs
            .is_dirty(&entry.workspace_path, &entry.head_id)
            .unwrap_or(true);
        if dirty && !force {
            report.kept_dirty.push(key);
            continue;
        }

        vcs.remove_workspace(&entry.repo_path, &entry.workspace_path, &key.slug(), force)?;
        state.remove(&key);
        report.removed.push(key);
    }

    state.save(&paths.state_file())?;
    Ok(report)
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::process::Command;

    use tempfile::TempDir;
    use wiremock::matchers::{method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;
    use crate::config::GithubConfig;
    use crate::source::Checkout;
    use crate::source::github::GithubSource;

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

    fn git_rev_parse(dir: &Path, rev: &str) -> String {
        let out = Command::new("git")
            .current_dir(dir)
            .args(["rev-parse", rev])
            .output()
            .unwrap();
        assert!(out.status.success());
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    /// Real GitHub exposes every PR's head as `refs/pull/N/head` on the *base* repo, which is
    /// what lets the git backend fetch a fork's commit without ever adding the fork as a remote.
    /// A plain local git fixture doesn't do this automatically, so tests have to fake it.
    fn expose_pr_ref(upstream: &Path, fork: &Path, branch: &str, number: u32) {
        git(
            upstream,
            &[
                "fetch",
                "-q",
                fork.to_str().unwrap(),
                &format!("{branch}:refs/pull/{number}/head"),
            ],
        );
    }

    fn paths_in(tmp: &Path) -> Paths {
        Paths::discover()
            .unwrap()
            .with_overrides(Some(tmp.join("data")), Some(tmp.join("cache")))
    }

    fn pull_json(
        owner: &str,
        repo: &str,
        upstream: &Path,
        fork: &Path,
        sha: &str,
        state: &str,
    ) -> serde_json::Value {
        serde_json::json!({
            "number": 1,
            "title": "Fix the thing",
            "html_url": format!("https://github.com/{owner}/{repo}/pull/1"),
            "state": state,
            "user": {"login": "contributor"},
            "head": {"ref": "feature", "sha": sha, "repo": {
                "clone_url": fork.to_string_lossy(),
                "full_name": format!("contributor/{repo}"),
                "owner": {"login": "contributor"},
            }},
            "base": {"ref": "main", "sha": "unused", "repo": {
                "clone_url": upstream.to_string_lossy(),
                "full_name": format!("{owner}/{repo}"),
                "owner": {"login": owner},
            }},
        })
    }

    async fn mount_search(server: &MockServer, has_item: bool) {
        let body = if has_item {
            serde_json::json!({"items": [{"html_url": "https://github.com/moz/proj/pull/1"}]})
        } else {
            serde_json::json!({"items": []})
        };
        Mock::given(method("GET"))
            .and(path("/search/issues"))
            .and(query_param("page", "1"))
            .respond_with(ResponseTemplate::new(200).set_body_json(body))
            .mount(server)
            .await;
    }

    async fn mount_pull(server: &MockServer, body: serde_json::Value) {
        Mock::given(method("GET"))
            .and(path("/repos/moz/proj/pulls/1"))
            .respond_with(ResponseTemplate::new(200).set_body_json(body))
            .mount(server)
            .await;
    }

    fn github_source(server: &MockServer) -> Box<dyn ReviewSource> {
        Box::new(GithubSource::for_test(
            GithubConfig {
                name: "gh".into(),
                api_url: None,
                token: Some("t".into()),
                token_cmd: None,
                ignore_repos: vec![],
                ignore_authors: vec![],
                ignore_teams: vec![],
                include_drafts: false,
            },
            Some("t".into()),
            server.uri(),
            100,
        ))
    }

    /// An "upstream" (PR base) and "fork" (PR head) repo pair, each a real local git repo.
    struct Repos {
        _tmp: TempDir,
        upstream: std::path::PathBuf,
        fork: std::path::PathBuf,
    }

    fn make_repos() -> Repos {
        let tmp = TempDir::new().unwrap();
        let upstream = tmp.path().join("upstream");
        std::fs::create_dir(&upstream).unwrap();
        git(&upstream, &["init", "-q", "-b", "main"]);
        git(&upstream, &["config", "user.name", "test"]);
        git(&upstream, &["config", "user.email", "test@example.com"]);
        git(&upstream, &["commit", "-q", "--allow-empty", "-m", "base"]);

        let fork = tmp.path().join("fork");
        git(
            tmp.path(),
            &[
                "clone",
                "-q",
                upstream.to_str().unwrap(),
                fork.to_str().unwrap(),
            ],
        );
        git(&fork, &["checkout", "-q", "-b", "feature"]);
        std::fs::write(fork.join("pr.txt"), "pr change\n").unwrap();
        git(&fork, &["add", "pr.txt"]);
        git(&fork, &["commit", "-q", "-m", "pr change"]);
        expose_pr_ref(&upstream, &fork, "feature", 1);

        Repos {
            _tmp: tmp,
            upstream,
            fork,
        }
    }

    #[tokio::test]
    async fn adds_a_new_review_end_to_end() {
        let repos = make_repos();
        let sha = git_rev_parse(&repos.fork, "HEAD");
        let server = MockServer::start().await;
        mount_search(&server, true).await;
        mount_pull(
            &server,
            pull_json("moz", "proj", &repos.upstream, &repos.fork, &sha, "open"),
        )
        .await;

        let work_tmp = TempDir::new().unwrap();
        let paths = paths_in(work_tmp.path());
        let config = Config::default();
        let sources: Vec<Box<dyn ReviewSource>> = vec![github_source(&server)];

        let report = sync(&sources, &paths, &config, None, false).await.unwrap();

        assert_eq!(report.added, vec![ReviewKey::new("gh", "moz/proj/1")]);
        let state = State::load(&paths.state_file()).unwrap();
        let entry = state.get(&ReviewKey::new("gh", "moz/proj/1")).unwrap();
        assert_eq!(entry.status, Status::Ready);
        assert!(entry.in_queue);
        assert_eq!(entry.head_id, sha);
        assert!(entry.workspace_path.join("pr.txt").exists());

        // The canonical repo was cloned (tool-managed - no `[[repo]]` config entry) and
        // registered, so a second sync would reuse it instead of cloning again.
        assert!(entry.repo_path.join(".git").exists());
        assert!(paths.repos_file().exists());
    }

    #[tokio::test]
    async fn removes_workspace_once_resolved_and_clean() {
        let repos = make_repos();
        let sha = git_rev_parse(&repos.fork, "HEAD");
        let server = MockServer::start().await;
        mount_search(&server, true).await;
        mount_pull(
            &server,
            pull_json("moz", "proj", &repos.upstream, &repos.fork, &sha, "open"),
        )
        .await;

        let work_tmp = TempDir::new().unwrap();
        let paths = paths_in(work_tmp.path());
        let config = Config::default();

        sync(&[github_source(&server)], &paths, &config, None, false)
            .await
            .unwrap();
        let ws_path = State::load(&paths.state_file())
            .unwrap()
            .get(&ReviewKey::new("gh", "moz/proj/1"))
            .unwrap()
            .workspace_path
            .clone();
        assert!(ws_path.exists());
        drop(server);

        // Second sync: the PR is no longer in the queue (merged) and `fetch_status` reports it
        // closed, with the workspace still clean.
        let server = MockServer::start().await;
        mount_search(&server, false).await;
        mount_pull(
            &server,
            pull_json("moz", "proj", &repos.upstream, &repos.fork, &sha, "closed"),
        )
        .await;

        let report = sync(&[github_source(&server)], &paths, &config, None, false)
            .await
            .unwrap();

        assert_eq!(report.removed, vec![ReviewKey::new("gh", "moz/proj/1")]);
        assert!(!ws_path.exists());
        assert!(
            State::load(&paths.state_file())
                .unwrap()
                .get(&ReviewKey::new("gh", "moz/proj/1"))
                .is_none()
        );
    }

    #[tokio::test]
    async fn keeps_out_of_queue_workspace_when_still_open() {
        let repos = make_repos();
        let sha = git_rev_parse(&repos.fork, "HEAD");
        let server = MockServer::start().await;
        mount_search(&server, true).await;
        mount_pull(
            &server,
            pull_json("moz", "proj", &repos.upstream, &repos.fork, &sha, "open"),
        )
        .await;

        let work_tmp = TempDir::new().unwrap();
        let paths = paths_in(work_tmp.path());
        let config = Config::default();
        sync(&[github_source(&server)], &paths, &config, None, false)
            .await
            .unwrap();
        drop(server);

        // Second sync: changes requested, so the PR drops out of the queue, but it's still open.
        let server = MockServer::start().await;
        mount_search(&server, false).await;
        mount_pull(
            &server,
            pull_json("moz", "proj", &repos.upstream, &repos.fork, &sha, "open"),
        )
        .await;

        let report = sync(&[github_source(&server)], &paths, &config, None, false)
            .await
            .unwrap();

        assert!(report.removed.is_empty());
        let entry = State::load(&paths.state_file()).unwrap();
        let entry = entry.get(&ReviewKey::new("gh", "moz/proj/1")).unwrap();
        assert!(!entry.in_queue);
        assert!(entry.workspace_path.exists());
    }

    #[tokio::test]
    async fn dirty_workspace_is_flagged_not_updated() {
        let repos = make_repos();
        let sha1 = git_rev_parse(&repos.fork, "HEAD");
        let server = MockServer::start().await;
        mount_search(&server, true).await;
        mount_pull(
            &server,
            pull_json("moz", "proj", &repos.upstream, &repos.fork, &sha1, "open"),
        )
        .await;

        let work_tmp = TempDir::new().unwrap();
        let paths = paths_in(work_tmp.path());
        let config = Config::default();
        sync(&[github_source(&server)], &paths, &config, None, false)
            .await
            .unwrap();
        drop(server);

        let ws_path = State::load(&paths.state_file())
            .unwrap()
            .get(&ReviewKey::new("gh", "moz/proj/1"))
            .unwrap()
            .workspace_path
            .clone();
        std::fs::write(ws_path.join("untracked.txt"), "local edit\n").unwrap();

        // The PR gets a new commit pushed.
        std::fs::write(repos.fork.join("pr2.txt"), "more change\n").unwrap();
        git(&repos.fork, &["add", "pr2.txt"]);
        git(&repos.fork, &["commit", "-q", "-m", "pr change 2"]);
        let sha2 = git_rev_parse(&repos.fork, "HEAD");
        assert_ne!(sha1, sha2);
        expose_pr_ref(&repos.upstream, &repos.fork, "feature", 1);

        let server = MockServer::start().await;
        mount_search(&server, true).await;
        mount_pull(
            &server,
            pull_json("moz", "proj", &repos.upstream, &repos.fork, &sha2, "open"),
        )
        .await;

        let report = sync(&[github_source(&server)], &paths, &config, None, false)
            .await
            .unwrap();

        assert!(report.updated.is_empty());
        assert_eq!(report.flagged.len(), 1);
        let entry = State::load(&paths.state_file()).unwrap();
        let entry = entry.get(&ReviewKey::new("gh", "moz/proj/1")).unwrap();
        assert_eq!(entry.status, Status::Dirty);
        assert_eq!(
            entry.version, sha1,
            "dirty workspace must not be moved to the new version"
        );
        assert!(
            ws_path.join("untracked.txt").exists(),
            "local edit must survive untouched"
        );
    }

    #[tokio::test]
    async fn dry_run_makes_no_changes() {
        let repos = make_repos();
        let sha = git_rev_parse(&repos.fork, "HEAD");
        let server = MockServer::start().await;
        mount_search(&server, true).await;
        mount_pull(
            &server,
            pull_json("moz", "proj", &repos.upstream, &repos.fork, &sha, "open"),
        )
        .await;

        let work_tmp = TempDir::new().unwrap();
        let paths = paths_in(work_tmp.path());
        let config = Config::default();

        let report = sync(&[github_source(&server)], &paths, &config, None, true)
            .await
            .unwrap();

        assert_eq!(report.added, vec![ReviewKey::new("gh", "moz/proj/1")]);
        assert!(
            !paths.state_file().exists(),
            "dry-run must not write state.json"
        );
        // Regression: `add_new` used to resolve (and potentially clone) the canonical repo
        // before checking `dry_run`, so a dry run had the very real side effect of actually
        // cloning it.
        assert!(
            !paths.repos_file().exists(),
            "dry-run must not touch the repo registry"
        );
        assert!(
            !paths.repo_cache_dir().exists(),
            "dry-run must not clone the canonical repo"
        );
    }

    /// A minimal canonical git repo (upstream + a clone) for `prune` tests, which don't need a
    /// `ReviewSource` at all - just a real workspace and a matching `state.json` entry.
    fn small_canon(tmp: &std::path::Path) -> std::path::PathBuf {
        let upstream = tmp.join("upstream");
        std::fs::create_dir(&upstream).unwrap();
        git(&upstream, &["init", "-q", "-b", "main"]);
        git(&upstream, &["config", "user.name", "test"]);
        git(&upstream, &["config", "user.email", "test@example.com"]);
        git(&upstream, &["commit", "-q", "--allow-empty", "-m", "base"]);

        let canon = tmp.join("canon");
        git(
            tmp,
            &[
                "clone",
                "-q",
                upstream.to_str().unwrap(),
                canon.to_str().unwrap(),
            ],
        );
        git(&canon, &["config", "user.name", "test"]);
        git(&canon, &["config", "user.email", "test@example.com"]);
        canon
    }

    fn entry_for(
        key: ReviewKey,
        canon: &std::path::Path,
        ws: &std::path::Path,
        head: &str,
        status: Status,
    ) -> ReviewEntry {
        ReviewEntry {
            key,
            title: "x".into(),
            author: "a".into(),
            url: "https://example.com".into(),
            repo_path: canon.to_path_buf(),
            vcs: crate::config::VcsKind::Git,
            workspace_path: ws.to_path_buf(),
            version: "1".into(),
            head_id: head.to_string(),
            in_queue: false,
            status,
            last_synced: chrono::Utc::now(),
        }
    }

    #[test]
    fn prune_removes_resolved_and_keeps_dirty_unless_forced() {
        let tmp = TempDir::new().unwrap();
        let paths = paths_in(tmp.path());
        paths.ensure_dirs().unwrap();
        let canon = small_canon(tmp.path());
        let vcs = GitVcs;
        let checkout = Checkout::Patches {
            base: None,
            patches: vec![],
        };

        let ws_clean = paths.workspace_dir("moz", "D1");
        let head_clean = vcs
            .add_workspace(&canon, &ws_clean, &checkout, "moz/D1", "1")
            .unwrap();
        let ws_dirty = paths.workspace_dir("moz", "D2");
        let head_dirty = vcs
            .add_workspace(&canon, &ws_dirty, &checkout, "moz/D2", "1")
            .unwrap();
        std::fs::write(ws_dirty.join("local.txt"), "uncommitted\n").unwrap();

        let mut state = State::default();
        state.insert(entry_for(
            ReviewKey::new("moz", "D1"),
            &canon,
            &ws_clean,
            &head_clean,
            Status::Resolved,
        ));
        state.insert(entry_for(
            ReviewKey::new("moz", "D2"),
            &canon,
            &ws_dirty,
            &head_dirty,
            Status::Resolved,
        ));
        state.save(&paths.state_file()).unwrap();

        let report = prune(&paths, false, &[]).unwrap();

        assert_eq!(report.removed, vec![ReviewKey::new("moz", "D1")]);
        assert_eq!(report.kept_dirty, vec![ReviewKey::new("moz", "D2")]);
        assert!(!ws_clean.exists());
        assert!(
            ws_dirty.exists(),
            "dirty workspace must survive without --force"
        );

        let report2 = prune(&paths, true, &[]).unwrap();
        assert_eq!(report2.removed, vec![ReviewKey::new("moz", "D2")]);
        assert!(!ws_dirty.exists(), "--force removes dirty workspaces too");
    }

    #[test]
    fn prune_only_touches_open_reviews_when_explicit_ids_given() {
        let tmp = TempDir::new().unwrap();
        let paths = paths_in(tmp.path());
        paths.ensure_dirs().unwrap();
        let canon = small_canon(tmp.path());
        let vcs = GitVcs;
        let checkout = Checkout::Patches {
            base: None,
            patches: vec![],
        };

        let ws = paths.workspace_dir("moz", "D3");
        let head = vcs
            .add_workspace(&canon, &ws, &checkout, "moz/D3", "1")
            .unwrap();

        let mut state = State::default();
        // Still open (not Resolved) - a plain `prune` with no ids would never touch this.
        state.insert(entry_for(
            ReviewKey::new("moz", "D3"),
            &canon,
            &ws,
            &head,
            Status::Ready,
        ));
        state.save(&paths.state_file()).unwrap();

        let untouched = prune(&paths, false, &[]).unwrap();
        assert!(untouched.removed.is_empty());
        assert!(ws.exists());

        let explicit = prune(&paths, false, &["D3".to_string()]).unwrap();
        assert_eq!(explicit.removed, vec![ReviewKey::new("moz", "D3")]);
        assert!(!ws.exists());
    }

    #[test]
    fn prune_removes_state_entry_for_an_already_missing_workspace() {
        let tmp = TempDir::new().unwrap();
        let paths = paths_in(tmp.path());
        paths.ensure_dirs().unwrap();
        let canon = small_canon(tmp.path());

        let mut state = State::default();
        state.insert(entry_for(
            ReviewKey::new("moz", "D4"),
            &canon,
            &paths.workspace_dir("moz", "D4"), // never actually created on disk
            "deadbeef",
            Status::Resolved,
        ));
        state.save(&paths.state_file()).unwrap();

        let report = prune(&paths, false, &[]).unwrap();
        assert_eq!(report.removed, vec![ReviewKey::new("moz", "D4")]);
        assert!(
            State::load(&paths.state_file())
                .unwrap()
                .get(&ReviewKey::new("moz", "D4"))
                .is_none()
        );
    }
}
