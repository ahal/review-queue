//! The sync engine: source-agnostic glue between `ReviewSource`, `RepoStore`, and the `Vcs`
//! backends.
//!
//! `rq sync` (this module's `sync()`) only ever tracks metadata and updates/removes *existing*
//! workspaces - it never creates one. Per source, per run:
//! 1. `fetch_queue()` - reviews currently waiting on you.
//! 2. New reviews are recorded with no workspace (see `fetch_local()` for that).
//! 3. Reviews already tracked whose `version` changed get their workspace (if any) updated in
//!    place if clean (`update_workspace`); dirty ones are flagged instead of touched.
//! 4. Reviews still in the queue but otherwise unchanged just get `in_queue` refreshed.
//!
//! Then, once per source, for tracked reviews that weren't in this run's queue (you acted on
//! them - approved, requested changes - so they dropped out): `fetch_status()` tells us whether
//! the review itself resolved (landed/closed/abandoned/merged). A resolved review with no
//! workspace, or one whose workspace directory is already gone from disk, is just dropped. A
//! resolved review with a clean workspace has the workspace removed too. Either way, if dirty,
//! it's kept - and re-checked on every later sync, so it clears out on its own once the local
//! changes are gone. This is the only workspace-removal path; there's no separate prune step.
//!
//! `fetch_local()` is the on-demand counterpart - resolving a canonical repo (asking before
//! cloning one, unless told otherwise) and creating a workspace for a single already-tracked
//! review. It's what `rq fetch` and the fetch hotkey in `rq list`'s TUI call; `sync()` never
//! calls it itself.

use std::collections::{BTreeSet, HashMap};

use anyhow::{Context, Result};

use crate::config::{Config, VcsKind};
use crate::paths::Paths;
use crate::repo::{OnMissing, RepoStore};
use crate::source::{Lifecycle, Review, ReviewSource};
use crate::state::{ReviewEntry, ReviewKey, State, Status, Workspace};
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
    only_source: Option<&str>,
    dry_run: bool,
) -> Result<SyncReport> {
    paths.ensure_dirs()?;
    let mut state = State::load(&paths.state_file())?;
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
            if let Err(e) =
                sync_one(source.as_ref(), &review, &mut state, dry_run, &mut report).await
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
    }
    Ok(report)
}

async fn sync_one(
    source: &dyn ReviewSource,
    review: &Review,
    state: &mut State,
    dry_run: bool,
    report: &mut SyncReport,
) -> Result<()> {
    match state.get(&review.key).cloned() {
        None => {
            add_new(review, state, dry_run, report);
            Ok(())
        }
        Some(entry) => update_existing(source, review, entry, state, dry_run, report).await,
    }
}

fn add_new(review: &Review, state: &mut State, dry_run: bool, report: &mut SyncReport) {
    report.added.push(review.key.clone());
    if dry_run {
        return;
    }
    state.insert(ReviewEntry {
        key: review.key.clone(),
        title: review.title.clone(),
        author: review.author.clone(),
        url: review.url.clone(),
        repo: review.repo.clone(),
        kind: review.kind.clone(),
        version: review.version.clone(),
        in_queue: true,
        resolved: false,
        last_synced: chrono::Utc::now(),
        workspace: None,
        diff_stat: review.diff_stat.clone(),
    });
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
    entry.repo = review.repo.clone();
    entry.kind = review.kind.clone();
    entry.in_queue = true;
    entry.resolved = false;
    entry.diff_stat = review.diff_stat.clone();

    let Some(mut ws) = entry.workspace.clone() else {
        // Not fetched locally - nothing on disk to update, just keep the tracked metadata
        // (including `version`) current for a later `rq fetch`.
        entry.version = review.version.clone();
        entry.last_synced = chrono::Utc::now();
        if !dry_run {
            state.insert(entry);
        }
        return Ok(());
    };

    if entry.version == review.version {
        entry.last_synced = chrono::Utc::now();
        if !dry_run {
            state.insert(entry);
        }
        return Ok(());
    }

    let vcs = vcs_for(ws.vcs);
    if vcs
        .is_dirty(&ws.workspace_path, &ws.head_id)
        .unwrap_or(true)
    {
        ws.status = Status::Dirty;
        entry.workspace = Some(ws);
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

    let checkout = source.checkout_spec(review, &ws.repo_path).await?;
    match vcs.update_workspace(
        &ws.repo_path,
        &ws.workspace_path,
        &checkout,
        &review.key.slug(),
        &review.version,
    ) {
        Ok(head) => {
            entry.version = review.version.clone();
            ws.head_id = head;
            ws.status = Status::Ready;
            entry.workspace = Some(ws);
            entry.last_synced = chrono::Utc::now();
            state.insert(entry);
            report.updated.push(review.key.clone());
        }
        Err(e) => {
            ws.status = Status::ApplyFailed;
            entry.workspace = Some(ws);
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
            entry.resolved = false;
            if !dry_run {
                state.insert(entry);
            }
        }
        Lifecycle::Resolved => {
            entry.resolved = true;
            let Some(ws) = entry.workspace.clone() else {
                // Nothing local to preserve for inspection - just forget it.
                if !dry_run {
                    state.remove(key);
                }
                report.removed.push(key.clone());
                return Ok(());
            };

            if !ws.workspace_path.exists() {
                // Already gone from disk (e.g. removed by hand) - nothing left to clean up,
                // just stop tracking it rather than flagging it as dirty forever.
                if !dry_run {
                    state.remove(key);
                }
                report.removed.push(key.clone());
                return Ok(());
            }

            let vcs = vcs_for(ws.vcs);
            if vcs
                .is_dirty(&ws.workspace_path, &ws.head_id)
                .unwrap_or(true)
            {
                report.flagged.push((
                    key.clone(),
                    "resolved but has local changes; workspace kept".into(),
                ));
                if !dry_run {
                    state.insert(entry);
                }
            } else {
                if !dry_run {
                    vcs.remove_workspace(&ws.repo_path, &ws.workspace_path, &key.slug())?;
                    state.remove(key);
                }
                report.removed.push(key.clone());
            }
        }
    }
    Ok(())
}

/// Resolve a canonical repo (asking before cloning one, unless `on_missing` says otherwise) and
/// create a workspace for `key`, a review already tracked by a prior `sync()`. A no-op that
/// returns the existing path if `key` already has a workspace. This is the on-demand counterpart
/// to `sync()`'s deliberate refusal to create workspaces on its own - see the module docs.
///
/// Under `OnMissing::Ask`, a repo with no local checkout surfaces as a `repo::NeedsClone` error
/// (`downcast_ref` it) rather than cloning - callers should confirm with the user and retry with
/// `OnMissing::Clone` if they agree.
pub async fn fetch_local(
    sources: &[Box<dyn ReviewSource>],
    paths: &Paths,
    config: &Config,
    key: &ReviewKey,
    on_missing: OnMissing,
) -> Result<std::path::PathBuf> {
    paths.ensure_dirs()?;
    let mut state = State::load(&paths.state_file())?;
    let mut entry = state
        .get(key)
        .cloned()
        .with_context(|| format!("`{key}` isn't tracked; run `rq sync` first"))?;

    if let Some(ws) = &entry.workspace {
        return Ok(ws.workspace_path.clone());
    }

    let source = sources
        .iter()
        .find(|s| s.name() == key.source)
        .with_context(|| format!("no configured source named `{}`", key.source))?;

    let review = Review {
        key: entry.key.clone(),
        title: entry.title.clone(),
        author: entry.author.clone(),
        url: entry.url.clone(),
        repo: entry.repo.clone(),
        version: entry.version.clone(),
        kind: entry.kind.clone(),
        diff_stat: entry.diff_stat.clone(),
    };

    let mut repo_store = RepoStore::load(paths, config)?;
    let canon = repo_store.resolve(&review.repo, on_missing)?;
    let checkout = source.checkout_spec(&review, &canon.path).await?;
    let vcs = vcs_for(canon.vcs);
    let ws_path = paths.workspace_dir(&canon.name, &key.slug());
    if let Some(parent) = ws_path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let outcome = vcs.add_workspace(
        &canon.path,
        &ws_path,
        &checkout,
        &key.slug(),
        &review.version,
    );
    let (status, head_id) = match &outcome {
        Ok(head) => (Status::Ready, head.clone()),
        // Recorded anyway (with the workspace left in place, per the vcs backends' own
        // contract) so `rq list`/`rq path` can point at it for inspection.
        Err(_) => (Status::ApplyFailed, String::new()),
    };
    entry.workspace = Some(Workspace {
        repo_path: canon.path.clone(),
        vcs: canon.vcs,
        workspace_path: ws_path.clone(),
        head_id,
        status,
    });
    state.insert(entry);
    state.save(&paths.state_file())?;
    repo_store.save()?;
    outcome.map(|_| ws_path)
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
            .with_overrides(Some(tmp.join("data")))
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
            "additions": 1,
            "deletions": 0,
            "changed_files": 1,
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
    async fn tracks_new_review_without_creating_a_workspace() {
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
        let sources: Vec<Box<dyn ReviewSource>> = vec![github_source(&server)];

        let report = sync(&sources, &paths, None, false).await.unwrap();

        assert_eq!(report.added, vec![ReviewKey::new("gh", "moz/proj/1")]);
        let state = State::load(&paths.state_file()).unwrap();
        let entry = state.get(&ReviewKey::new("gh", "moz/proj/1")).unwrap();
        assert!(entry.in_queue);
        assert!(!entry.resolved);
        assert_eq!(entry.version, sha);
        assert_eq!(
            entry.diff_stat.as_deref(),
            Some("1 file changed, 1 insertion(+)"),
            "diff_stat should be fetched as part of the same sync that adds the review"
        );
        assert!(
            entry.workspace.is_none(),
            "sync must not create a workspace on its own"
        );

        // Nothing was cloned or registered either - that only happens on `fetch_local`.
        assert!(!paths.repos_file().exists());
        assert!(!paths.repos_dir().exists());
    }

    #[tokio::test]
    async fn fetch_local_creates_workspace_on_demand() {
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

        sync(&sources, &paths, None, false).await.unwrap();

        let key = ReviewKey::new("gh", "moz/proj/1");
        let ws_path = fetch_local(&sources, &paths, &config, &key, OnMissing::Clone)
            .await
            .unwrap();

        assert!(ws_path.join("pr.txt").exists());
        let state = State::load(&paths.state_file()).unwrap();
        let entry = state.get(&key).unwrap();
        let ws = entry.workspace.as_ref().unwrap();
        assert_eq!(ws.status, Status::Ready);
        assert_eq!(ws.head_id, sha);
        assert_eq!(ws.workspace_path, ws_path);

        // The canonical repo was cloned (tool-managed - not found in any workdir) and
        // registered, so a second fetch would reuse it instead of cloning again.
        assert!(ws.repo_path.join(".git").exists());
        assert!(paths.repos_file().exists());

        // Calling it again is a no-op that just returns the existing path.
        let ws_path2 = fetch_local(&sources, &paths, &config, &key, OnMissing::Clone)
            .await
            .unwrap();
        assert_eq!(ws_path2, ws_path);
    }

    #[tokio::test]
    async fn resolved_review_without_a_workspace_is_dropped_with_no_git_calls() {
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
        let key = ReviewKey::new("gh", "moz/proj/1");

        sync(&[github_source(&server)], &paths, None, false)
            .await
            .unwrap();
        assert!(
            State::load(&paths.state_file())
                .unwrap()
                .get(&key)
                .unwrap()
                .workspace
                .is_none()
        );
        drop(server);

        // Never fetched locally, and now merged - nothing to check for dirtiness, so it's just
        // dropped from tracking.
        let server = MockServer::start().await;
        mount_search(&server, false).await;
        mount_pull(
            &server,
            pull_json("moz", "proj", &repos.upstream, &repos.fork, &sha, "closed"),
        )
        .await;

        let report = sync(&[github_source(&server)], &paths, None, false)
            .await
            .unwrap();

        assert_eq!(report.removed, vec![key.clone()]);
        assert!(
            State::load(&paths.state_file())
                .unwrap()
                .get(&key)
                .is_none()
        );
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
        let key = ReviewKey::new("gh", "moz/proj/1");

        let sources: Vec<Box<dyn ReviewSource>> = vec![github_source(&server)];
        sync(&sources, &paths, None, false).await.unwrap();
        let ws_path = fetch_local(&sources, &paths, &config, &key, OnMissing::Clone)
            .await
            .unwrap();
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

        let report = sync(&[github_source(&server)], &paths, None, false)
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
        let key = ReviewKey::new("gh", "moz/proj/1");
        let sources: Vec<Box<dyn ReviewSource>> = vec![github_source(&server)];
        sync(&sources, &paths, None, false).await.unwrap();
        fetch_local(&sources, &paths, &config, &key, OnMissing::Clone)
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

        let report = sync(&[github_source(&server)], &paths, None, false)
            .await
            .unwrap();

        assert!(report.removed.is_empty());
        let entry = State::load(&paths.state_file()).unwrap();
        let entry = entry.get(&key).unwrap();
        assert!(!entry.in_queue);
        assert!(entry.workspace.as_ref().unwrap().workspace_path.exists());
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
        let key = ReviewKey::new("gh", "moz/proj/1");
        let sources: Vec<Box<dyn ReviewSource>> = vec![github_source(&server)];
        sync(&sources, &paths, None, false).await.unwrap();
        let ws_path = fetch_local(&sources, &paths, &config, &key, OnMissing::Clone)
            .await
            .unwrap();
        drop(server);

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

        let report = sync(&[github_source(&server)], &paths, None, false)
            .await
            .unwrap();

        assert!(report.updated.is_empty());
        assert_eq!(report.flagged.len(), 1);
        let entry = State::load(&paths.state_file()).unwrap();
        let entry = entry.get(&key).unwrap();
        assert_eq!(entry.workspace.as_ref().unwrap().status, Status::Dirty);
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

        let report = sync(&[github_source(&server)], &paths, None, true)
            .await
            .unwrap();

        assert_eq!(report.added, vec![ReviewKey::new("gh", "moz/proj/1")]);
        assert!(
            !paths.state_file().exists(),
            "dry-run must not write state.json"
        );
        // `sync` never resolves/clones a canonical repo itself - only `fetch_local` does - so
        // this holds regardless of `dry_run`, but is worth pinning down for the dry-run path too.
        assert!(
            !paths.repos_file().exists(),
            "dry-run must not touch the repo registry"
        );
        assert!(
            !paths.repos_dir().exists(),
            "dry-run must not clone the canonical repo"
        );
    }
}
