//! The sync engine: source-agnostic glue between `ReviewSource`, `RepoStore`, and the `Vcs`
//! backends.
//!
//! Workspaces are per *stack*, not per review: reviews whose `ancestors` chain them together (a
//! Phabricator stack) share one workspace, built from the stack's tip, and a lone review is just a
//! stack of one. See `stacks` for how reviews are grouped.
//!
//! `rq sync` (this module's `sync()`) only ever tracks metadata and updates/removes *existing*
//! workspaces - it never creates one. Per source, per run:
//! 1. `fetch_queue()` - reviews currently waiting on you.
//! 2. New reviews are recorded with no workspace (see `fetch_local()` for that); tracked ones get
//!    their metadata (including `version` and `ancestors`) refreshed.
//!
//! Then, once per source, for tracked reviews that weren't in this run's queue (you acted on
//! them - approved, requested changes - so they dropped out): `fetch_status()` tells us whether
//! the review itself resolved (landed/closed/abandoned/merged). A resolved review that isn't part
//! of a workspace is just dropped.
//!
//! Finally each existing workspace is reconciled against its stack: reviews newly chained onto a
//! checked-out stack join its workspace; a workspace whose tip or tip `version` changed is
//! updated in place if clean (`update_workspace`), or flagged if dirty; one whose reviews have
//! all resolved is removed if clean (dirty ones are kept, and re-checked on every later sync, so
//! they clear out on their own once the local changes are gone). This is the only
//! workspace-removal path; there's no separate prune step.
//!
//! `fetch_local()` is the on-demand counterpart - resolving a canonical repo (asking before
//! cloning one, unless told otherwise) and creating a workspace for a single already-tracked
//! review's stack, or, if it already has one, moving it to that review's patch. It's what `rq
//! fetch` and the fetch hotkey in `rq show`'s TUI call; `sync()` never calls it itself.

use std::collections::{BTreeSet, HashMap};
use std::path::PathBuf;

use anyhow::{Context, Result, bail};

use crate::config::{Config, VcsKind};
use crate::paths::Paths;
use crate::repo::{OnMissing, RepoStore};
use crate::source::{Lifecycle, Review, ReviewSource};
use crate::stacks::{self, Stack};
use crate::state::{ReviewEntry, ReviewKey, State, Status, Workspace};
use crate::vcs::Vcs;
use crate::vcs::git::GitVcs;
use crate::vcs::jj::JjVcs;

/// How far back from a stack's tip to look for a member's commit; comfortably deeper than any
/// realistic stack.
const STACK_SCAN_DEPTH: usize = 200;

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
            record_review(&review, &mut state, &mut report);
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
            handle_out_of_queue(&key, lifecycle, &mut state, &mut report);
        }
    }

    reconcile_workspaces(sources, only_source, &mut state, dry_run, &mut report).await;

    if !dry_run {
        state.save(&paths.state_file())?;
    }
    Ok(report)
}

/// Record a review from this run's queue: new ones are added with no workspace, tracked ones
/// have their metadata refreshed. Never touches a workspace - see `reconcile_workspaces`.
fn record_review(review: &Review, state: &mut State, report: &mut SyncReport) {
    let mut entry = match state.get(&review.key).cloned() {
        Some(entry) => entry,
        None => {
            report.added.push(review.key.clone());
            ReviewEntry {
                key: review.key.clone(),
                title: String::new(),
                author: String::new(),
                url: String::new(),
                repo: review.repo.clone(),
                kind: review.kind.clone(),
                version: String::new(),
                in_queue: true,
                resolved: false,
                last_synced: chrono::Utc::now(),
                stack_id: None,
                ancestors: Vec::new(),
                diff_stat: None,
                description: None,
            }
        }
    };
    entry.title = review.title.clone();
    entry.author = review.author.clone();
    entry.url = review.url.clone();
    entry.repo = review.repo.clone();
    entry.kind = review.kind.clone();
    entry.version = review.version.clone();
    entry.ancestors = review.ancestors.clone();
    entry.in_queue = true;
    entry.resolved = false;
    entry.diff_stat = review.diff_stat.clone();
    entry.description = review.description.clone();
    entry.last_synced = chrono::Utc::now();
    state.insert(entry);
}

fn handle_out_of_queue(
    key: &ReviewKey,
    lifecycle: Lifecycle,
    state: &mut State,
    report: &mut SyncReport,
) {
    let Some(mut entry) = state.get(key).cloned() else {
        return;
    };
    entry.in_queue = false;
    entry.last_synced = chrono::Utc::now();

    match lifecycle {
        Lifecycle::Open => {
            entry.resolved = false;
            state.insert(entry);
        }
        Lifecycle::Resolved => {
            entry.resolved = true;
            let has_workspace = state.workspace_of(&entry).is_some();
            if has_workspace {
                // Kept while its stack's workspace lives; `reconcile_workspaces` drops it along
                // with the workspace.
                state.insert(entry);
            } else {
                // Nothing local to preserve for inspection - just forget it.
                state.remove(key);
                report.removed.push(key.clone());
            }
        }
    }
}

/// Make every review in a stack that's checked out share that stack's workspace. Also settles
/// the case of one stack whose members were fetched separately (older per-review workspaces, or
/// a review re-parented onto another's stack): the tip's workspace wins and the rest are left
/// orphaned for `reconcile_workspaces` to clean up.
fn attach_stack_ids(state: &mut State, stacks: &[Stack]) {
    for stack in stacks {
        let existing = |key: &ReviewKey| state.get(key).and_then(|e| e.stack_id.clone());
        let chosen = existing(&stack.tip).or_else(|| stack.members.iter().find_map(existing));
        let Some(id) = chosen else {
            continue;
        };
        for key in &stack.members {
            if let Some(mut entry) = state.get(key).cloned()
                && entry.stack_id.as_deref() != Some(id.as_str())
            {
                entry.stack_id = Some(id.clone());
                state.insert(entry);
            }
        }
    }
}

fn review_from_entry(entry: &ReviewEntry) -> Review {
    Review {
        key: entry.key.clone(),
        title: entry.title.clone(),
        author: entry.author.clone(),
        url: entry.url.clone(),
        repo: entry.repo.clone(),
        version: entry.version.clone(),
        kind: entry.kind.clone(),
        diff_stat: entry.diff_stat.clone(),
        description: entry.description.clone(),
        ancestors: entry.ancestors.clone(),
    }
}

/// Bring each existing workspace in line with its stack: rebuild it if the stack's tip or tip
/// version changed, remove it once every review in it has resolved. See the module docs.
async fn reconcile_workspaces(
    sources: &[Box<dyn ReviewSource>],
    only_source: Option<&str>,
    state: &mut State,
    dry_run: bool,
    report: &mut SyncReport,
) {
    let stacks = stacks::group(state.iter());
    attach_stack_ids(state, &stacks);

    let ids: Vec<String> = state.workspaces().map(|(id, _)| id.clone()).collect();
    for id in ids {
        let Some(mut ws) = state.workspace(&id).cloned() else {
            continue;
        };
        if only_source.is_some_and(|o| o != ws.tip.source) {
            continue;
        }
        let members = state.members_of(&id);
        let live: Vec<&ReviewKey> = members
            .iter()
            .filter(|k| state.get(k).is_some_and(|e| !e.resolved))
            .collect();

        if live.is_empty() {
            remove_finished_workspace(&id, &ws, &members, state, dry_run, report);
            continue;
        }

        // The stack this workspace belongs to; prefer the one still containing its old tip.
        let stack = stacks::stack_containing(&stacks, &ws.tip)
            .or_else(|| stacks::stack_containing(&stacks, live[0]));
        let Some(stack) = stack else {
            continue;
        };
        let Some(tip) = state.get(&stack.tip).cloned() else {
            continue;
        };
        if ws.tip == tip.key && ws.version == tip.version {
            continue;
        }

        let vcs = vcs_for(ws.vcs);
        if vcs
            .is_dirty(&ws.workspace_path, &ws.head_id)
            .unwrap_or(true)
        {
            ws.status = Status::Dirty;
            state.insert_workspace(id, ws);
            report.flagged.push((
                tip.key.clone(),
                "local changes; not updated to the new version".into(),
            ));
            continue;
        }
        if dry_run {
            report.updated.push(tip.key.clone());
            continue;
        }

        let Some(source) = sources.iter().find(|s| s.name() == tip.key.source) else {
            continue;
        };
        let review = review_from_entry(&tip);
        let checkout = match source.checkout_spec(&review, &ws.repo_path).await {
            Ok(checkout) => checkout,
            Err(e) => {
                report.errors.push((tip.key.clone(), e.to_string()));
                continue;
            }
        };
        match vcs.update_workspace(
            &ws.repo_path,
            &ws.workspace_path,
            &checkout,
            &id,
            &tip.version,
        ) {
            Ok(head) => {
                ws.head_id = head;
                ws.tip = tip.key.clone();
                ws.version = tip.version.clone();
                ws.status = Status::Ready;
                state.insert_workspace(id, ws);
                report.updated.push(tip.key.clone());
            }
            Err(e) => {
                ws.status = Status::ApplyFailed;
                state.insert_workspace(id, ws);
                report.errors.push((tip.key.clone(), e.to_string()));
            }
        }
    }
}

/// Every review in this workspace has resolved (or none remain): remove the workspace and its
/// reviews if it's clean or already gone from disk; keep it, flagged, if it has local changes.
fn remove_finished_workspace(
    id: &str,
    ws: &Workspace,
    members: &[ReviewKey],
    state: &mut State,
    dry_run: bool,
    report: &mut SyncReport,
) {
    let vcs = vcs_for(ws.vcs);
    // Already gone from disk (e.g. removed by hand) - nothing left to clean up, just stop
    // tracking it rather than flagging it as dirty forever.
    let gone = !ws.workspace_path.exists();
    if !gone
        && vcs
            .is_dirty(&ws.workspace_path, &ws.head_id)
            .unwrap_or(true)
    {
        report.flagged.push((
            ws.tip.clone(),
            "resolved but has local changes; workspace kept".into(),
        ));
        return;
    }
    if !dry_run {
        if !gone && let Err(e) = vcs.remove_workspace(&ws.repo_path, &ws.workspace_path, id) {
            report.errors.push((ws.tip.clone(), e.to_string()));
            return;
        }
        state.remove_workspace(id);
        for key in members {
            state.remove(key);
        }
    }
    report.removed.extend(members.iter().cloned());
}

/// Resolve a canonical repo (asking before cloning one, unless `on_missing` says otherwise) and
/// make sure `key`, a review already tracked by a prior `sync()`, has a workspace - the one for
/// its whole stack - then move that workspace to `key`'s own patch. If the stack is already
/// checked out this is just the move (skipped when the workspace has local changes, which are
/// never disturbed); an `ApplyFailed` workspace is instead cleaned up and rebuilt, since handing
/// back a broken/empty path again would just repeat the failure silently. This is the on-demand
/// counterpart to `sync()`'s deliberate refusal to create workspaces on its own - see the module
/// docs.
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
    let entry = state
        .get(key)
        .cloned()
        .with_context(|| format!("`{key}` isn't tracked; run `rq sync` first"))?;

    let stacks = stacks::group(state.iter());
    attach_stack_ids(&mut state, &stacks);
    let stack = stacks::stack_containing(&stacks, key)
        .cloned()
        .unwrap_or_else(|| Stack {
            tip: key.clone(),
            members: vec![key.clone()],
        });
    let stack_id = state
        .get(key)
        .and_then(|e| e.stack_id.clone())
        .unwrap_or_else(|| stack.members[0].slug());

    let source = sources
        .iter()
        .find(|s| s.name() == key.source)
        .with_context(|| format!("no configured source named `{}`", key.source))?;

    if let Some(ws) = state.workspace(&stack_id).cloned() {
        if ws.status != Status::ApplyFailed {
            position_at(source.as_ref(), &ws, key);
            state.save(&paths.state_file())?;
            return Ok(ws.workspace_path);
        }
        // Retry instead of handing back the broken path: best-effort clean up whatever the
        // failed attempt left registered/on-disk first, since re-adding a workspace at the same
        // name/path would otherwise fail again for that reason alone.
        let stale_vcs = vcs_for(ws.vcs);
        let _ = stale_vcs.remove_workspace(&ws.repo_path, &ws.workspace_path, &stack_id);
        let _ = std::fs::remove_dir_all(&ws.workspace_path);
        state.remove_workspace(&stack_id);
    }

    let tip = state
        .get(&stack.tip)
        .cloned()
        .unwrap_or_else(|| entry.clone());
    let tip_source = sources
        .iter()
        .find(|s| s.name() == tip.key.source)
        .with_context(|| format!("no configured source named `{}`", tip.key.source))?;
    let review = review_from_entry(&tip);

    let mut repo_store = RepoStore::load(paths, config)?;
    let canon = repo_store.resolve(&review.repo, on_missing)?;
    let checkout = tip_source.checkout_spec(&review, &canon.path).await?;
    let vcs = vcs_for(canon.vcs);
    let ws_path = paths.workspace_dir(&stack_id);
    if let Some(parent) = ws_path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let outcome = vcs.add_workspace(&canon.path, &ws_path, &checkout, &stack_id, &review.version);
    let (status, head_id) = match &outcome {
        Ok(head) => (Status::Ready, head.clone()),
        // Recorded anyway (with the workspace left in place, per the vcs backends' own
        // contract) so `rq show`/`rq path` can point at it for inspection.
        Err(_) => (Status::ApplyFailed, String::new()),
    };
    let ws = Workspace {
        repo_path: canon.path.clone(),
        vcs: canon.vcs,
        workspace_path: ws_path.clone(),
        head_id,
        status,
        tip: tip.key.clone(),
        version: tip.version.clone(),
    };
    state.insert_workspace(stack_id.clone(), ws.clone());
    for member in &stack.members {
        if let Some(mut e) = state.get(member).cloned() {
            e.stack_id = Some(stack_id.clone());
            state.insert(e);
        }
    }
    if outcome.is_ok() {
        position_at(source.as_ref(), &ws, key);
    }
    state.save(&paths.state_file())?;
    repo_store.save()?;
    outcome.map(|_| ws_path)
}

/// Best-effort: move a stack's workspace to `key`'s patch. Never disturbs a workspace with local
/// changes, and does nothing if `key`'s commit can't be found (e.g. a patch added to the stack
/// since the workspace was built, which the next sync will pick up) - the caller still gets the
/// workspace either way.
fn position_at(source: &dyn ReviewSource, ws: &Workspace, key: &ReviewKey) {
    let vcs = vcs_for(ws.vcs);
    if vcs
        .is_dirty(&ws.workspace_path, &ws.head_id)
        .unwrap_or(true)
    {
        return;
    }
    let target = vcs
        .commits(&ws.workspace_path, &ws.head_id, STACK_SCAN_DEPTH)
        .ok()
        .and_then(|commits| {
            commits
                .into_iter()
                .find(|(_, message)| source.is_commit_for(key, message))
                .map(|(id, _)| id)
        })
        .or_else(|| (*key == ws.tip).then(|| ws.head_id.clone()));
    if let Some(commit) = target {
        let _ = vcs.position(&ws.workspace_path, &commit);
    }
}

/// Returned by `remove_workspace` when the workspace has local changes and `force` wasn't set.
/// Callers should confirm with the user and retry with `force: true` if they agree to discard
/// them.
#[derive(Debug, thiserror::Error)]
#[error("workspace at {} has local changes", path.display())]
pub struct WorkspaceDirty {
    pub path: PathBuf,
}

/// Delete the local workspace of a review's stack on demand - the `d` hotkey in `rq show`'s TUI.
/// Acts regardless of `resolved`/`in_queue`, unlike `sync()`'s own resolved-and-clean removal
/// path, and on every review in the stack, since they share the one workspace.
///
/// A dirty workspace is left untouched and surfaces as `WorkspaceDirty` (`downcast_ref` it)
/// unless `force` is set, in which case whatever the backend's own `remove_workspace` can't clean
/// up on a dirty worktree (it assumes a clean one) is finished off with a raw directory removal.
pub fn remove_workspace(paths: &Paths, key: &ReviewKey, force: bool) -> Result<()> {
    let mut state = State::load(&paths.state_file())?;
    let entry = state
        .get(key)
        .cloned()
        .with_context(|| format!("`{key}` isn't tracked"))?;
    let (Some(stack_id), Some(ws)) = (entry.stack_id.clone(), state.workspace_of(&entry).cloned())
    else {
        bail!("`{key}` has no local workspace");
    };

    if ws.workspace_path.exists() {
        let vcs = vcs_for(ws.vcs);
        if !force
            && vcs
                .is_dirty(&ws.workspace_path, &ws.head_id)
                .unwrap_or(true)
        {
            return Err(WorkspaceDirty {
                path: ws.workspace_path.clone(),
            }
            .into());
        }
        if vcs
            .remove_workspace(&ws.repo_path, &ws.workspace_path, &stack_id)
            .is_err()
        {
            if !force {
                bail!(
                    "failed to remove workspace at {}",
                    ws.workspace_path.display()
                );
            }
            std::fs::remove_dir_all(&ws.workspace_path)
                .with_context(|| format!("removing {}", ws.workspace_path.display()))?;
        }
    }

    state.remove_workspace(&stack_id);
    state.for_each_entry_mut(|e| {
        if e.stack_id.as_deref() == Some(stack_id.as_str()) {
            e.stack_id = None;
        }
    });
    state.save(&paths.state_file())
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
            state.workspace_of(entry).is_none(),
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
        let ws = state.workspace_for(&key).unwrap();
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
    async fn fetch_local_retries_an_apply_failed_workspace_instead_of_returning_it() {
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
        let key = ReviewKey::new("gh", "moz/proj/1");

        sync(&sources, &paths, None, false).await.unwrap();
        let ws_path = fetch_local(&sources, &paths, &config, &key, OnMissing::Clone)
            .await
            .unwrap();

        // Simulate a previous attempt that left a real, registered worktree behind but got
        // marked failed (e.g. a later step errored) - a naive retry would otherwise leave this
        // stuck forever, since `git worktree add`/`jj workspace add` refuse to reuse the path.
        let mut state = State::load(&paths.state_file()).unwrap();
        let stack_id = state.get(&key).unwrap().stack_id.clone().unwrap();
        let mut ws = state.workspace(&stack_id).cloned().unwrap();
        ws.status = Status::ApplyFailed;
        ws.head_id = String::new();
        state.insert_workspace(stack_id, ws);
        state.save(&paths.state_file()).unwrap();

        let retried_path = fetch_local(&sources, &paths, &config, &key, OnMissing::Clone)
            .await
            .unwrap();

        assert_eq!(retried_path, ws_path);
        assert!(retried_path.join("pr.txt").exists());
        let state = State::load(&paths.state_file()).unwrap();
        let ws = state.workspace_for(&key).unwrap();
        assert_eq!(ws.status, Status::Ready);
        assert_eq!(ws.head_id, sha);
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
                .stack_id
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
        let state = State::load(&paths.state_file()).unwrap();
        let entry = state.get(&key).unwrap();
        assert!(!entry.in_queue);
        assert!(state.workspace_of(entry).unwrap().workspace_path.exists());
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
        let state = State::load(&paths.state_file()).unwrap();
        let ws = state.workspace_for(&key).unwrap();
        assert_eq!(ws.status, Status::Dirty);
        assert_eq!(
            ws.version, sha1,
            "dirty workspace must not be moved to the new version"
        );
        assert!(
            ws_path.join("untracked.txt").exists(),
            "local edit must survive untouched"
        );
    }

    #[tokio::test]
    async fn remove_workspace_deletes_a_clean_one() {
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

        remove_workspace(&paths, &key, false).unwrap();

        assert!(!ws_path.exists());
        let state = State::load(&paths.state_file()).unwrap();
        assert!(
            state.workspace_for(&key).is_none(),
            "review stays tracked, just without a workspace"
        );
        assert!(state.get(&key).is_some());
    }

    #[tokio::test]
    async fn remove_workspace_refuses_a_dirty_one_without_force() {
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
        std::fs::write(ws_path.join("untracked.txt"), "local edit\n").unwrap();

        let err = remove_workspace(&paths, &key, false).unwrap_err();

        assert!(err.downcast_ref::<WorkspaceDirty>().is_some());
        assert!(ws_path.exists(), "must not touch a dirty workspace");
        let state = State::load(&paths.state_file()).unwrap();
        assert!(state.workspace_for(&key).is_some());
    }

    #[tokio::test]
    async fn remove_workspace_with_force_discards_local_changes() {
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
        std::fs::write(ws_path.join("untracked.txt"), "local edit\n").unwrap();

        remove_workspace(&paths, &key, true).unwrap();

        assert!(!ws_path.exists());
        let state = State::load(&paths.state_file()).unwrap();
        assert!(state.workspace_for(&key).is_none());
    }

    #[test]
    fn remove_workspace_errors_when_review_has_none() {
        let work_tmp = TempDir::new().unwrap();
        let paths = paths_in(work_tmp.path());
        let key = ReviewKey::new("gh", "moz/proj/1");
        let mut state = State::default();
        state.insert(ReviewEntry {
            key: key.clone(),
            title: "Fix the thing".into(),
            author: "someone".into(),
            url: "https://example.com/1".into(),
            repo: crate::source::RepoRef {
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
        });
        state.save(&paths.state_file()).unwrap();

        let err = remove_workspace(&paths, &key, false).unwrap_err();

        assert!(err.downcast_ref::<WorkspaceDirty>().is_none());
        assert!(format!("{err:#}").contains("no local workspace"));
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

    /// A source whose reviews form a Phabricator-style stack, checked out by applying one small
    /// patch per review (`D<n>` adds `<n>.txt`), each commit carrying a `Differential Revision:`
    /// trailer like `moz-phab patch` leaves.
    struct StackSource {
        upstream: std::path::PathBuf,
        queue: std::sync::Mutex<Vec<Review>>,
        lifecycle: std::sync::Mutex<Lifecycle>,
    }

    impl StackSource {
        fn new(upstream: &Path) -> Self {
            Self {
                upstream: upstream.to_path_buf(),
                queue: Default::default(),
                lifecycle: std::sync::Mutex::new(Lifecycle::Open),
            }
        }

        fn review(&self, n: u32, ancestors: &[u32], version: &str) -> Review {
            Review {
                key: ReviewKey::new("stk", format!("D{n}")),
                title: format!("Review {n}"),
                author: "alice".into(),
                url: format!("https://phab.example.com/D{n}"),
                repo: crate::source::RepoRef {
                    urls: vec![self.upstream.to_string_lossy().to_string()],
                    display_name: "proj".into(),
                },
                version: version.into(),
                kind: crate::source::ReviewKind::Direct,
                diff_stat: None,
                description: None,
                ancestors: ancestors
                    .iter()
                    .map(|a| ReviewKey::new("stk", format!("D{a}")))
                    .collect(),
            }
        }

        fn set_queue(&self, reviews: Vec<Review>) {
            *self.queue.lock().unwrap() = reviews;
        }
    }

    fn patch_for(id: &str) -> crate::source::Patch {
        let file = format!("{id}.txt");
        crate::source::Patch {
            title: format!("patch {id}"),
            author: "Patch Author <patch@example.com>".into(),
            message: format!("patch {id}\n\nDifferential Revision: https://phab.example.com/{id}"),
            diff: format!(
                "diff --git a/{file} b/{file}\nnew file mode 100644\nindex 0000000..1111111\n--- /dev/null\n+++ b/{file}\n@@ -0,0 +1 @@\n+{id}\n"
            ),
        }
    }

    #[async_trait::async_trait]
    impl ReviewSource for StackSource {
        fn name(&self) -> &str {
            "stk"
        }

        async fn fetch_queue(&self) -> Result<Vec<Review>> {
            Ok(self.queue.lock().unwrap().clone())
        }

        async fn checkout_spec(
            &self,
            review: &Review,
            _canonical_repo: &Path,
        ) -> Result<crate::source::Checkout> {
            let patches = review
                .ancestors
                .iter()
                .chain(std::iter::once(&review.key))
                .map(|k| patch_for(&k.id))
                .collect();
            Ok(crate::source::Checkout::Patches {
                base: None,
                patches,
            })
        }

        async fn fetch_status(&self, ids: &[String]) -> Result<Vec<(String, Lifecycle)>> {
            let lifecycle = *self.lifecycle.lock().unwrap();
            Ok(ids.iter().map(|id| (id.clone(), lifecycle)).collect())
        }

        fn is_commit_for(&self, review: &ReviewKey, message: &str) -> bool {
            message
                .lines()
                .any(|l| l.ends_with(&format!("/{}", review.id)))
        }
    }

    fn stack_fixture() -> (Repos, TempDir, Paths, Vec<Box<dyn ReviewSource>>) {
        let repos = make_repos();
        let work_tmp = TempDir::new().unwrap();
        let paths = paths_in(work_tmp.path());
        let source = StackSource::new(&repos.upstream);
        source.set_queue(vec![
            source.review(1, &[], "1"),
            source.review(2, &[1], "1"),
        ]);
        (repos, work_tmp, paths, vec![Box::new(source)])
    }

    fn stk(n: u32) -> ReviewKey {
        ReviewKey::new("stk", format!("D{n}"))
    }

    #[tokio::test]
    async fn stacked_reviews_share_one_workspace_positioned_at_the_requested_patch() {
        let (_repos, _tmp, paths, sources) = stack_fixture();
        let config = Config::default();
        sync(&sources, &paths, None, false).await.unwrap();

        let ws_path = fetch_local(&sources, &paths, &config, &stk(1), OnMissing::Clone)
            .await
            .unwrap();
        // Opened on D1's patch: D2's file (on top) isn't checked out.
        assert!(ws_path.join("D1.txt").exists());
        assert!(!ws_path.join("D2.txt").exists());

        let state = State::load(&paths.state_file()).unwrap();
        assert_eq!(state.workspaces().count(), 1);
        assert_eq!(
            state.get(&stk(1)).unwrap().stack_id,
            state.get(&stk(2)).unwrap().stack_id,
            "both reviews point at the one workspace"
        );
        assert_eq!(state.workspace_for(&stk(2)).unwrap().tip, stk(2));

        // Asking for the top review reuses the workspace and moves to the tip.
        let again = fetch_local(&sources, &paths, &config, &stk(2), OnMissing::Clone)
            .await
            .unwrap();
        assert_eq!(again, ws_path);
        assert!(ws_path.join("D2.txt").exists());
        assert_eq!(
            State::load(&paths.state_file())
                .unwrap()
                .workspaces()
                .count(),
            1
        );
    }

    #[tokio::test]
    async fn local_changes_stop_fetch_local_from_moving_the_workspace() {
        let (_repos, _tmp, paths, sources) = stack_fixture();
        let config = Config::default();
        sync(&sources, &paths, None, false).await.unwrap();
        let ws_path = fetch_local(&sources, &paths, &config, &stk(2), OnMissing::Clone)
            .await
            .unwrap();
        std::fs::write(ws_path.join("scratch.txt"), "review notes\n").unwrap();

        fetch_local(&sources, &paths, &config, &stk(1), OnMissing::Clone)
            .await
            .unwrap();

        assert!(ws_path.join("D2.txt").exists(), "must stay on the tip");
        assert!(ws_path.join("scratch.txt").exists());
    }

    #[tokio::test]
    async fn a_new_patch_on_top_rebuilds_the_workspace_with_the_new_tip() {
        let (repos, _tmp, paths, sources) = stack_fixture();
        let config = Config::default();
        sync(&sources, &paths, None, false).await.unwrap();
        let ws_path = fetch_local(&sources, &paths, &config, &stk(1), OnMissing::Clone)
            .await
            .unwrap();

        let next = StackSource::new(&repos.upstream);
        next.set_queue(vec![
            next.review(1, &[], "1"),
            next.review(2, &[1], "1"),
            next.review(3, &[1, 2], "1"),
        ]);
        let sources: Vec<Box<dyn ReviewSource>> = vec![Box::new(next)];
        let report = sync(&sources, &paths, None, false).await.unwrap();

        assert_eq!(report.updated, vec![stk(3)]);
        let state = State::load(&paths.state_file()).unwrap();
        let ws = state.workspace_for(&stk(3)).unwrap();
        assert_eq!(ws.tip, stk(3));
        assert_eq!(ws.status, Status::Ready);
        assert_eq!(state.workspaces().count(), 1, "still the one workspace");
        assert!(ws_path.join("D3.txt").exists());
    }

    #[tokio::test]
    async fn a_changed_patch_mid_stack_updates_the_workspace_via_the_tip_version() {
        let (repos, _tmp, paths, sources) = stack_fixture();
        let config = Config::default();
        sync(&sources, &paths, None, false).await.unwrap();
        fetch_local(&sources, &paths, &config, &stk(2), OnMissing::Clone)
            .await
            .unwrap();

        // Phabricator folds every ancestor's modification time into the tip's version.
        let next = StackSource::new(&repos.upstream);
        next.set_queue(vec![next.review(1, &[], "2"), next.review(2, &[1], "2")]);
        let sources: Vec<Box<dyn ReviewSource>> = vec![Box::new(next)];
        let report = sync(&sources, &paths, None, false).await.unwrap();

        assert_eq!(report.updated, vec![stk(2)]);
        let state = State::load(&paths.state_file()).unwrap();
        assert_eq!(state.workspace_for(&stk(2)).unwrap().version, "2");
    }

    #[tokio::test]
    async fn workspace_is_removed_only_once_every_review_in_the_stack_resolves() {
        let (repos, _tmp, paths, sources) = stack_fixture();
        let config = Config::default();
        sync(&sources, &paths, None, false).await.unwrap();
        let ws_path = fetch_local(&sources, &paths, &config, &stk(2), OnMissing::Clone)
            .await
            .unwrap();

        // D1 lands and D2 is still open (and now stands alone): the workspace stays.
        let next = StackSource::new(&repos.upstream);
        next.set_queue(vec![next.review(2, &[], "3")]);
        let sources: Vec<Box<dyn ReviewSource>> = vec![Box::new(next)];
        let report = sync(&sources, &paths, None, false).await.unwrap();
        assert_eq!(report.updated, vec![stk(2)]);
        assert!(ws_path.exists());
        // Rebuilt on the landed base, so only D2's own patch remains.
        assert!(ws_path.join("D2.txt").exists());
        assert!(!ws_path.join("D1.txt").exists());

        // Now everything resolves.
        let done = StackSource::new(&repos.upstream);
        *done.lifecycle.lock().unwrap() = Lifecycle::Resolved;
        let sources: Vec<Box<dyn ReviewSource>> = vec![Box::new(done)];
        let report = sync(&sources, &paths, None, false).await.unwrap();

        assert!(report.removed.contains(&stk(2)));
        assert!(!ws_path.exists());
        let state = State::load(&paths.state_file()).unwrap();
        assert_eq!(state.workspaces().count(), 0);
        assert!(state.get(&stk(2)).is_none());
    }

    #[tokio::test]
    async fn remove_workspace_takes_the_whole_stack_with_it() {
        let (_repos, _tmp, paths, sources) = stack_fixture();
        let config = Config::default();
        sync(&sources, &paths, None, false).await.unwrap();
        let ws_path = fetch_local(&sources, &paths, &config, &stk(1), OnMissing::Clone)
            .await
            .unwrap();

        remove_workspace(&paths, &stk(1), false).unwrap();

        assert!(!ws_path.exists());
        let state = State::load(&paths.state_file()).unwrap();
        assert!(state.workspace_for(&stk(2)).is_none());
        assert!(state.get(&stk(2)).unwrap().stack_id.is_none());
    }
}
