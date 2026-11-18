//! The sync engine: source-agnostic glue between `ReviewSource`, `RepoStore`, and the `Vcs`
//! backends.
//!
//! Workspaces are per *stack*, not per review: reviews whose `ancestors` chain them together (a
//! Phabricator stack) share one workspace, built from the stack's tip, and a lone review is just a
//! stack of one. See `stacks` for how reviews are grouped.
//!
//! `rq sync` (this module's `sync()`) only ever tracks metadata and updates/removes *existing*
//! workspaces. With `auto_workspace` enabled, a separate step then creates workspaces for
//! queued stacks that have none after sync has saved state. Per source, per run:
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
//! updated in place (`update_workspace`); one whose reviews have all resolved is removed.
//!
//! Last, `prune_orphans` sweeps up whatever a removal that didn't finish left behind: workspace
//! directories under `workspaces/` that state doesn't know about, `rq` workspaces still
//! registered in a canonical repo, and `review-queue/<name>/*` pins with no workspace. Then the
//! owner/repo directories removals left empty are deleted.
//!
//! `fetch_local()` is the on-demand counterpart - resolving a canonical repo (asking before
//! cloning one, unless told otherwise) and creating a workspace for a single already-tracked
//! review's stack, or, if it already has one, moving it to that review's patch. It's what `rq
//! fetch` and the fetch hotkey in `rq show`'s TUI call; `sync()` never calls it itself.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

use crate::config::{Config, VcsKind};
use crate::paths::Paths;
use crate::repo::{self, NeedsClone, OnMissing, RepoStore};
use crate::source::{Lifecycle, RateLimited, Review, ReviewSource};
use crate::stacks::{self, Stack};
use crate::state::{ReviewEntry, ReviewKey, State, Status, Workspace};
use crate::vcs::git::GitVcs;
use crate::vcs::jj::JjVcs;
use crate::vcs::{PartialApply, Vcs};

/// How far back from a stack's tip to look for a member's commit; comfortably deeper than any
/// realistic stack.
const STACK_SCAN_DEPTH: usize = 200;

#[derive(Debug, Default)]
pub struct SyncReport {
    pub added: Vec<ReviewKey>,
    /// Workspaces created by `auto_workspace`, or that it would create on a dry run.
    pub fetched: Vec<ReviewKey>,
    /// The bottom-most queued review of each stack that needs a workspace built: what
    /// `auto_workspace` would fetch. Computed from sync's in-memory state, so it's also right
    /// on a dry run, when nothing is saved.
    pub missing_stacks: Vec<ReviewKey>,
    pub updated: Vec<ReviewKey>,
    pub removed: Vec<ReviewKey>,
    /// Review + human-readable reason it was flagged (e.g. a partial apply).
    pub flagged: Vec<(ReviewKey, String)>,
    pub errors: Vec<(ReviewKey, String)>,
    /// Source name + error for a source that failed as a whole; its reviews are left as-is.
    pub source_errors: Vec<(String, String)>,
    /// Leftover workspaces/pins no tracked review owns, removed by `prune_orphans`.
    pub pruned: Vec<String>,
    /// Leftovers `prune_orphans` found but couldn't remove.
    pub prune_flagged: Vec<(String, String)>,
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
    let mut failed_sources: BTreeSet<&str> = BTreeSet::new();
    // Sources that hit a rate limit this run: already reported once, so skip them from here on.
    let mut rate_limited: BTreeSet<String> = BTreeSet::new();

    for source in sources {
        if only_source.is_some_and(|o| o != source.name()) {
            continue;
        }
        tracing::info!("fetching review queue from `{}`", source.name());
        let queue = match source.fetch_queue().await {
            Ok(queue) => {
                tracing::info!(
                    "`{}`: {} reviews in queue",
                    source.name(),
                    queue.reviews.len() + queue.errors.len()
                );
                queue
            }
            Err(e) => {
                // Without its queue there's no telling which of its reviews left it, so don't
                // check any of their statuses either.
                failed_sources.insert(source.name());
                if RateLimited::is(&e) {
                    rate_limited.insert(source.name().to_string());
                }
                report
                    .source_errors
                    .push((source.name().to_string(), format!("fetching queue: {e:#}")));
                continue;
            }
        };
        for review in queue.reviews {
            seen.insert(review.key.clone());
            record_review(&review, &mut state, &mut report);
        }
        // Still in the queue, just not fetchable right now: leave its state as it was rather
        // than treating it as having left the queue.
        for (key, err) in queue.errors {
            seen.insert(key.clone());
            report.errors.push((key, err));
        }
    }

    // Gathered before `reconcile_workspaces` forgets any workspace, so a repo whose last
    // workspace goes this run still gets checked for leftovers.
    let mut repos_to_check: BTreeSet<PathBuf> = state
        .workspaces()
        .map(|(_, ws)| ws.repo_path.clone())
        .collect();
    repos_to_check.extend(repo::tool_managed_paths(paths).unwrap_or_default());

    let sources_by_name: HashMap<&str, &Box<dyn ReviewSource>> =
        sources.iter().map(|s| (s.name(), s)).collect();
    let mut ids_by_source: HashMap<String, Vec<String>> = HashMap::new();
    for entry in state.iter() {
        if seen.contains(&entry.key) {
            continue;
        }
        if only_source.is_some_and(|o| o != entry.key.source)
            || failed_sources.contains(entry.key.source.as_str())
        {
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
        tracing::info!(
            "checking status of {} reviews no longer in the `{source_name}` queue",
            ids.len()
        );
        let statuses = match source.fetch_status(&ids).await {
            Ok(statuses) => statuses,
            Err(e) => {
                if RateLimited::is(&e) {
                    rate_limited.insert(source_name.clone());
                }
                report
                    .source_errors
                    .push((source_name, format!("checking status: {e:#}")));
                continue;
            }
        };
        for (id, lifecycle) in statuses {
            let key = ReviewKey::new(source_name.clone(), id);
            match lifecycle {
                Ok(lifecycle) => handle_out_of_queue(&key, lifecycle, &mut state, &mut report),
                Err(err) => report.errors.push((key, err)),
            }
        }
    }

    tracing::info!("reconciling workspaces");
    reconcile_workspaces(
        sources,
        only_source,
        &mut rate_limited,
        &mut state,
        dry_run,
        &mut report,
    )
    .await;
    tracing::info!("checking for leftover workspaces");
    prune_orphans(
        paths,
        &state,
        repos_to_check,
        only_source,
        dry_run,
        &mut report,
    );
    report.missing_stacks = missing_stacks(&state);

    if !dry_run {
        state.save(&paths.state_file())?;
    }
    Ok(report)
}

/// For each stack with a still-queued review that needs a workspace built, its bottom-most such
/// review. A stack needs one if none of its members has a workspace, or if its workspace failed
/// to apply and the stack has changed since (`reconcile_workspaces` leaves those alone, and
/// retrying an unchanged stack would just fail the same way). Stacks with a member whose
/// workspace the user deleted are skipped, including ones that grew a new review since.
fn missing_stacks(state: &State) -> Vec<ReviewKey> {
    stacks::group(state.iter())
        .into_iter()
        .filter(|stack| {
            !stack
                .members
                .iter()
                .any(|member| state.get(member).is_some_and(|e| e.workspace_dismissed))
        })
        .filter(|stack| {
            let ws = stack
                .members
                .iter()
                .find_map(|member| state.get(member).and_then(|e| state.workspace_of(e)));
            match ws {
                None => true,
                Some(ws) if ws.status == Status::ApplyFailed => state
                    .get(&stack.tip)
                    .is_some_and(|tip| ws.tip != tip.key || ws.version != tip.version),
                Some(_) => false,
            }
        })
        .filter_map(|stack| {
            stack
                .members
                .into_iter()
                .find(|k| state.get(k).is_some_and(|e| e.in_queue && !e.resolved))
        })
        .collect()
}

/// Create a workspace for each of `report.missing_stacks` after `sync` has saved state. The caller
/// must keep holding the sync lock: `fetch_local` loads and saves state independently.
pub async fn auto_workspace(
    sources: &[Box<dyn ReviewSource>],
    paths: &Paths,
    config: &Config,
    report: &mut SyncReport,
    dry_run: bool,
) -> Result<()> {
    if !config.auto_workspace {
        return Ok(());
    }
    if dry_run {
        report.fetched.extend(report.missing_stacks.iter().cloned());
        return Ok(());
    }

    let on_missing = if config.auto_clone {
        OnMissing::Clone
    } else {
        OnMissing::Ask
    };
    for key in report.missing_stacks.clone() {
        tracing::info!("creating workspace for {key}");
        match fetch_local(sources, paths, config, &key, on_missing).await {
            Ok(_) => {
                if let Some(note) = State::load(&paths.state_file())
                    .ok()
                    .and_then(|st| partial_note(&st, &key))
                {
                    report.flagged.push((key.clone(), note));
                }
                report.fetched.push(key);
            }
            Err(e) if e.downcast_ref::<NeedsClone>().is_some() => report.flagged.push((
                key,
                "no local checkout; run `rq fetch` or set auto_clone".into(),
            )),
            Err(e) => report.errors.push((key, format!("{e:#}"))),
        }
    }
    Ok(())
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
                workspace_dismissed: false,
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
    rate_limited: &mut BTreeSet<String>,
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
            tracing::info!("removing workspace for resolved stack {id}");
            remove_finished_workspace(&id, &ws, &members, state, dry_run, report);
            continue;
        }

        // Deleted out from under us: forget it, so the next open rebuilds it from scratch.
        if !ws.workspace_path.is_dir() {
            if !dry_run {
                let _ = vcs_for(ws.vcs).remove_workspace(&ws.repo_path, &ws.workspace_path, &id);
                state.remove_workspace(&id);
            }
            continue;
        }

        // Leave a failed build to `fetch_local` to rebuild.
        if ws.status == Status::ApplyFailed {
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

        if dry_run {
            report.updated.push(tip.key.clone());
            continue;
        }

        // Left as-is; the next sync will pick the update up.
        if rate_limited.contains(&tip.key.source) {
            continue;
        }
        let Some(source) = sources.iter().find(|s| s.name() == tip.key.source) else {
            continue;
        };
        tracing::info!("updating workspace for {} to {}", tip.key, tip.version);
        let vcs = vcs_for(ws.vcs);
        let review = review_from_entry(&tip);
        let checkout = match source.checkout_spec(&review, &ws.repo_path).await {
            Ok(checkout) => checkout,
            Err(e) if RateLimited::is(&e) => {
                rate_limited.insert(tip.key.source.clone());
                report
                    .source_errors
                    .push((tip.key.source.clone(), format!("{e:#}")));
                continue;
            }
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
                ws.unapplied.clear();
                state.insert_workspace(id, ws);
                report.updated.push(tip.key.clone());
            }
            Err(e) => match e.downcast::<PartialApply>() {
                Ok(partial) => {
                    ws.unapplied = unapplied_members(
                        source.as_ref(),
                        vcs.as_ref(),
                        &ws.workspace_path,
                        &partial.head,
                        &stack.members,
                    );
                    ws.head_id = partial.head;
                    ws.tip = tip.key.clone();
                    ws.version = tip.version.clone();
                    ws.status = Status::Partial;
                    let note = partial_note_for(&ws);
                    state.insert_workspace(id, ws);
                    report.flagged.push((tip.key.clone(), note));
                }
                Err(e) => {
                    ws.status = Status::ApplyFailed;
                    state.insert_workspace(id, ws);
                    report.errors.push((tip.key.clone(), e.to_string()));
                }
            },
        }
    }
}

/// Every review in this workspace has resolved (or none remain): remove the workspace and its
/// reviews.
fn remove_finished_workspace(
    id: &str,
    ws: &Workspace,
    members: &[ReviewKey],
    state: &mut State,
    dry_run: bool,
    report: &mut SyncReport,
) {
    let vcs = vcs_for(ws.vcs);
    if !dry_run {
        // Even when the directory is already gone, the parent repo still has the workspace
        // registered; the backend forgets it. Whatever it can't clean up is finished off with a
        // raw removal.
        if vcs
            .remove_workspace(&ws.repo_path, &ws.workspace_path, id)
            .is_err()
            && ws.workspace_path.exists()
            && let Err(e) = std::fs::remove_dir_all(&ws.workspace_path)
        {
            report.errors.push((
                ws.tip.clone(),
                format!("removing {}: {e}", ws.workspace_path.display()),
            ));
            return;
        }
        state.remove_workspace(id);
        for key in members {
            state.remove(key);
        }
    }
    report.removed.extend(members.iter().cloned());
}

/// Which backend made the workspace or repo at `dir`, if it's one at all. jj is checked first,
/// since jj workspaces and repos may be colocated with git.
fn vcs_of_dir(dir: &Path) -> Option<VcsKind> {
    if dir.join(".jj").exists() {
        Some(VcsKind::Jj)
    } else if dir.join(".git").exists() {
        Some(VcsKind::Git)
    } else {
        None
    }
}

/// `path`, with symlinks resolved if it exists, so paths from state and from a vcs compare
/// equal however each was spelled.
fn normalized(path: &Path) -> PathBuf {
    path.canonicalize().unwrap_or_else(|_| path.to_path_buf())
}

/// Every workspace directory under `root` (recursively, not descending into workspaces).
fn workspace_dirs(root: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let Ok(entries) = std::fs::read_dir(root) else {
        return found;
    };
    for entry in entries.flatten() {
        if !entry.file_type().is_ok_and(|t| t.is_dir()) {
            continue;
        }
        let dir = entry.path();
        if vcs_of_dir(&dir).is_some() {
            found.push(dir);
        } else {
            found.extend(workspace_dirs(&dir));
        }
    }
    found
}

/// Delete every directory under `root` left with nothing in it (`root` itself excepted), such
/// as the owner/repo directories of removed workspaces. Never descends into a workspace.
/// Returns whether `root` ended up empty.
fn remove_empty_dirs(root: &Path) -> bool {
    let Ok(entries) = std::fs::read_dir(root) else {
        return false;
    };
    let mut empty = true;
    for entry in entries.flatten() {
        let dir = entry.path();
        if entry.file_type().is_ok_and(|t| t.is_dir())
            && vcs_of_dir(&dir).is_none()
            && remove_empty_dirs(&dir)
            && std::fs::remove_dir(&dir).is_ok()
        {
            continue;
        }
        empty = false;
    }
    empty
}

/// Delete `path`'s ancestors below `root` that are left empty, nearest first.
fn remove_empty_parents(path: &Path, root: &Path) {
    let mut dir = path.parent();
    while let Some(d) = dir {
        if !d.starts_with(root) || d == root || std::fs::remove_dir(d).is_err() {
            break;
        }
        dir = d.parent();
    }
}

/// The stack id an orphaned workspace at `path` was created under: its path below
/// `workspaces/`. Older layouts kept `workspaces/` elsewhere in the data dir, so failing that,
/// whatever follows the last `workspaces` component.
fn orphan_slug(paths: &Paths, path: &Path) -> String {
    if let Ok(rel) = path.strip_prefix(paths.workspaces_dir()) {
        return rel.to_string_lossy().into_owned();
    }
    let components: Vec<_> = path.components().collect();
    let tail = match components
        .iter()
        .rposition(|c| c.as_os_str() == "workspaces")
    {
        Some(i) => components[i + 1..].iter().collect::<PathBuf>(),
        None => path.file_name().map(PathBuf::from).unwrap_or_default(),
    };
    tail.to_string_lossy().into_owned()
}

/// Remove one leftover workspace (or, if `ws` doesn't exist, whatever's still registered or
/// pinned for it in `repo`). Returns whether it was removed.
fn remove_orphan(
    vcs: &dyn Vcs,
    repo: &Path,
    ws: &Path,
    slug: &str,
    label: String,
    dry_run: bool,
    report: &mut SyncReport,
) -> bool {
    if !dry_run {
        tracing::info!("removing leftover {label}");
    }
    if !dry_run
        && let Err(e) = vcs.remove_workspace(repo, ws, slug)
        // e.g. a git worktree whose registration is already gone, which `worktree remove`
        // refuses; any pins left over are picked up by the next sync.
        && (!ws.exists() || std::fs::remove_dir_all(ws).is_err())
    {
        report.prune_flagged.push((label, format!("{e:#}")));
        return false;
    }
    report.pruned.push(label);
    true
}

/// Clean up what no tracked workspace accounts for - see the module docs. `repos` are the
/// canonical repos to check for leftover registrations and pins, on top of those any orphaned
/// workspace directory points at.
fn prune_orphans(
    paths: &Paths,
    state: &State,
    repos: BTreeSet<PathBuf>,
    only_source: Option<&str>,
    dry_run: bool,
    report: &mut SyncReport,
) {
    let known_ids: BTreeSet<&str> = state.workspaces().map(|(id, _)| id.as_str()).collect();
    let known_paths: BTreeSet<PathBuf> = state
        .workspaces()
        .map(|(_, ws)| normalized(&ws.workspace_path))
        .collect();
    let in_scope = |slug: &str| only_source.is_none_or(|o| slug.split('/').next() == Some(o));
    let data_dir = normalized(paths.data_dir());

    let mut repos: BTreeMap<PathBuf, VcsKind> = repos
        .into_iter()
        .filter_map(|r| vcs_of_dir(&r).map(|kind| (r, kind)))
        .collect();
    // Workspace paths already dealt with (removed or flagged), and the names whose pins went
    // with them.
    let mut handled: BTreeSet<PathBuf> = BTreeSet::new();
    let mut handled_names: BTreeSet<String> = BTreeSet::new();

    for dir in workspace_dirs(&paths.workspaces_dir()) {
        let slug = orphan_slug(paths, &dir);
        if known_paths.contains(&normalized(&dir))
            || known_ids.contains(slug.as_str())
            || !in_scope(&slug)
        {
            continue;
        }
        let label = format!("{slug} ({})", dir.display());
        handled.insert(normalized(&dir));
        handled_names.insert(slug.clone());
        let Some(kind) = vcs_of_dir(&dir) else {
            continue;
        };
        let vcs = vcs_for(kind);
        let repo = match vcs.canonical_repo(&dir) {
            Ok(repo) => repo,
            Err(e) => {
                report
                    .prune_flagged
                    .push((label, format!("can't find its repo: {e:#}")));
                continue;
            }
        };
        let repo_kind = vcs_of_dir(&repo).unwrap_or(kind);
        repos.entry(repo.clone()).or_insert(repo_kind);
        remove_orphan(
            vcs_for(repo_kind).as_ref(),
            &repo,
            &dir,
            &slug,
            label,
            dry_run,
            report,
        );
    }

    for (repo, kind) in repos {
        let vcs = vcs_for(kind);
        let residue = match vcs.residue(&repo) {
            Ok(residue) => residue,
            Err(e) => {
                report.prune_flagged.push((
                    repo.display().to_string(),
                    format!("checking for leftovers: {e:#}"),
                ));
                continue;
            }
        };
        for root in residue.workspaces {
            let slug = orphan_slug(paths, &root);
            let norm = normalized(&root);
            // Only ever touch workspaces inside our own data dir.
            if !norm.starts_with(&data_dir)
                || known_paths.contains(&norm)
                || handled.contains(&norm)
                || known_ids.contains(slug.as_str())
                || !in_scope(&slug)
            {
                continue;
            }
            handled.insert(norm);
            handled_names.insert(slug.clone());
            let label = format!(
                "{slug} ({}, registered in {})",
                root.display(),
                repo.display()
            );
            remove_orphan(vcs.as_ref(), &repo, &root, &slug, label, dry_run, report);
        }
        for name in residue.pins {
            if known_ids.contains(name.as_str())
                || handled_names.contains(&name)
                || !in_scope(&name)
            {
                continue;
            }
            let ws = paths.workspace_dir(&name);
            // Something's there that wasn't removed above; leave its pins with it.
            if ws.exists() {
                continue;
            }
            let label = format!("{name} (pins in {})", repo.display());
            remove_orphan(vcs.as_ref(), &repo, &ws, &name, label, dry_run, report);
        }
    }

    if !dry_run {
        remove_empty_dirs(&paths.workspaces_dir());
    }
}

/// Resolve a canonical repo (asking before cloning one, unless `on_missing` says otherwise) and
/// make sure `key`, a review already tracked by a prior `sync()`, has a workspace - the one for
/// its whole stack - then move that workspace to `key`'s own patch. If the stack is already
/// checked out this is just the move; an `ApplyFailed` (or deleted) workspace is instead cleaned
/// up and rebuilt, since handing back a broken/empty path again would just repeat the failure
/// silently. This is the on-demand counterpart to `sync()`'s deliberate refusal to create workspaces on its own -
/// see the module docs.
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
        if ws.status != Status::ApplyFailed && ws.workspace_path.is_dir() {
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
    let (status, head_id, unapplied) = match &outcome {
        Ok(head) => (Status::Ready, head.clone(), Vec::new()),
        // The part of the stack that applied is still worth reviewing.
        Err(e) if let Some(partial) = e.downcast_ref::<PartialApply>() => (
            Status::Partial,
            partial.head.clone(),
            unapplied_members(
                source.as_ref(),
                vcs.as_ref(),
                &ws_path,
                &partial.head,
                &stack.members,
            ),
        ),
        // Recorded anyway (with the workspace left in place, per the vcs backends' own
        // contract) so `rq show`/`rq path` can point at it for inspection.
        Err(_) => (Status::ApplyFailed, String::new(), Vec::new()),
    };
    let ws = Workspace {
        repo_path: canon.path.clone(),
        vcs: canon.vcs,
        workspace_path: ws_path.clone(),
        head_id,
        status,
        tip: tip.key.clone(),
        version: tip.version.clone(),
        unapplied,
    };
    state.insert_workspace(stack_id.clone(), ws.clone());
    for member in &stack.members {
        if let Some(mut e) = state.get(member).cloned() {
            e.stack_id = Some(stack_id.clone());
            e.workspace_dismissed = false;
            state.insert(e);
        }
    }
    if status != Status::ApplyFailed {
        position_at(source.as_ref(), &ws, key);
    }
    state.save(&paths.state_file())?;
    repo_store.save()?;
    match outcome {
        Err(e) if status == Status::ApplyFailed => Err(e),
        _ => Ok(ws_path),
    }
}

/// The members of a stack with no commit reachable from `head` in the workspace at `ws_path`.
fn unapplied_members(
    source: &dyn ReviewSource,
    vcs: &dyn Vcs,
    ws_path: &std::path::Path,
    head: &str,
    members: &[ReviewKey],
) -> Vec<ReviewKey> {
    let Ok(commits) = vcs.commits(ws_path, head, STACK_SCAN_DEPTH) else {
        return Vec::new();
    };
    members
        .iter()
        .filter(|k| !commits.iter().any(|(_, msg)| source.is_commit_for(k, msg)))
        .cloned()
        .collect()
}

/// A warning to show alongside `key`'s workspace if its stack only partially applied.
pub fn partial_note(state: &State, key: &ReviewKey) -> Option<String> {
    state
        .workspace_for(key)
        .filter(|ws| ws.status == Status::Partial)
        .map(partial_note_for)
}

fn partial_note_for(ws: &Workspace) -> String {
    if ws.unapplied.is_empty() {
        return "stack only partially applied".into();
    }
    let missing: Vec<String> = ws.unapplied.iter().map(ReviewKey::slug).collect();
    format!(
        "stack only partially applied; {} didn't apply",
        missing.join(", ")
    )
}

/// Best-effort: move a stack's workspace to `key`'s patch. Does nothing if `key`'s commit can't
/// be found (e.g. a patch added to the stack since the workspace was built, which the next sync
/// will pick up) - the caller still gets the workspace either way.
fn position_at(source: &dyn ReviewSource, ws: &Workspace, key: &ReviewKey) {
    let vcs = vcs_for(ws.vcs);
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

/// Delete the local workspace of a review's stack on demand - the `d` hotkey in `rq show`'s TUI.
/// Acts regardless of `resolved`/`in_queue`, unlike `sync()`'s own resolved-and-clean removal
/// path, and on every review in the stack, since they share the one workspace. Whatever the
/// backend's own `remove_workspace` can't clean up is finished off with a raw directory removal.
/// The members are marked `workspace_dismissed` so `auto_workspace` doesn't recreate it.
pub fn remove_workspace(paths: &Paths, key: &ReviewKey) -> Result<()> {
    let mut state = State::load(&paths.state_file())?;
    let entry = state
        .get(key)
        .cloned()
        .with_context(|| format!("`{key}` isn't tracked"))?;
    let (Some(stack_id), Some(ws)) = (entry.stack_id.clone(), state.workspace_of(&entry).cloned())
    else {
        bail!("`{key}` has no local workspace");
    };

    let vcs = vcs_for(ws.vcs);
    // Even when the directory is already gone, the parent repo still has the workspace
    // registered; the backend forgets it.
    if vcs
        .remove_workspace(&ws.repo_path, &ws.workspace_path, &stack_id)
        .is_err()
        && ws.workspace_path.exists()
    {
        std::fs::remove_dir_all(&ws.workspace_path)
            .with_context(|| format!("removing {}", ws.workspace_path.display()))?;
    }

    remove_empty_parents(&ws.workspace_path, &paths.workspaces_dir());
    state.remove_workspace(&stack_id);
    state.for_each_entry_mut(|e| {
        if e.stack_id.as_deref() == Some(stack_id.as_str()) {
            e.stack_id = None;
            e.workspace_dismissed = true;
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
    use crate::source::Queue;
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

        let mut report = sync(&sources, &paths, None, false).await.unwrap();
        auto_workspace(&sources, &paths, &Config::default(), &mut report, false)
            .await
            .unwrap();
        assert!(report.fetched.is_empty());

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
    async fn inaccessible_pr_is_reported_and_left_untouched() {
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
        drop(server);

        // Still in the queue, then out of it, with the PR itself forbidden both times.
        for in_queue in [true, false] {
            let server = MockServer::start().await;
            mount_search(&server, in_queue).await;
            Mock::given(method("GET"))
                .and(path("/repos/moz/proj/pulls/1"))
                .respond_with(ResponseTemplate::new(403).set_body_string("SAML enforcement"))
                .mount(&server)
                .await;

            let report = sync(&[github_source(&server)], &paths, None, false)
                .await
                .unwrap();

            assert_eq!(report.errors.len(), 1, "in_queue={in_queue}");
            assert_eq!(report.errors[0].0, key);
            assert!(report.errors[0].1.contains("403"), "{}", report.errors[0].1);
            assert!(report.removed.is_empty());
            let state = State::load(&paths.state_file()).unwrap();
            let entry = state.get(&key).unwrap();
            assert!(entry.in_queue, "in_queue={in_queue}");
            assert!(!entry.resolved);
            assert_eq!(entry.version, sha);
        }
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
        assert!(!paths.workspaces_dir().join("gh").exists());
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
    async fn deleted_workspace_is_forgotten_and_rebuilt() {
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

        // Deleted out from under us, then opened again before any sync notices.
        std::fs::remove_dir_all(&ws_path).unwrap();
        let reopened = fetch_local(&sources, &paths, &config, &key, OnMissing::Clone)
            .await
            .unwrap();
        assert_eq!(reopened, ws_path);
        assert!(
            ws_path.is_dir(),
            "fetch_local must rebuild a deleted workspace"
        );

        // Deleted again, then a sync runs: it must forget the workspace.
        std::fs::remove_dir_all(&ws_path).unwrap();
        let report = sync(&sources, &paths, None, false).await.unwrap();
        assert!(report.flagged.is_empty());
        let state = State::load(&paths.state_file()).unwrap();
        assert!(state.workspace_for(&key).is_none());
    }

    #[tokio::test]
    async fn apply_failed_workspace_is_left_for_fetch_local_to_rebuild() {
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

        // Simulate a build that failed after the directory was created.
        let mut state = State::load(&paths.state_file()).unwrap();
        let (id, mut ws) = state
            .workspaces()
            .map(|(id, ws)| (id.clone(), ws.clone()))
            .next()
            .unwrap();
        ws.status = Status::ApplyFailed;
        ws.head_id = String::new();
        state.insert_workspace(id, ws);
        state.save(&paths.state_file()).unwrap();

        // The PR gets a new commit pushed.
        std::fs::write(repos.fork.join("pr2.txt"), "more change\n").unwrap();
        git(&repos.fork, &["add", "pr2.txt"]);
        git(&repos.fork, &["commit", "-q", "-m", "pr change 2"]);
        let sha2 = git_rev_parse(&repos.fork, "HEAD");
        expose_pr_ref(&repos.upstream, &repos.fork, "feature", 1);

        let server = MockServer::start().await;
        mount_search(&server, true).await;
        mount_pull(
            &server,
            pull_json("moz", "proj", &repos.upstream, &repos.fork, &sha2, "open"),
        )
        .await;
        let sources: Vec<Box<dyn ReviewSource>> = vec![github_source(&server)];

        let report = sync(&sources, &paths, None, false).await.unwrap();
        assert!(report.flagged.is_empty());
        let state = State::load(&paths.state_file()).unwrap();
        assert_eq!(
            state.workspace_for(&key).unwrap().status,
            Status::ApplyFailed
        );

        fetch_local(&sources, &paths, &config, &key, OnMissing::Clone)
            .await
            .unwrap();
        let state = State::load(&paths.state_file()).unwrap();
        let ws = state.workspace_for(&key).unwrap();
        assert_eq!(ws.status, Status::Ready);
        assert_eq!(ws.version, sha2);
        assert!(ws_path.is_dir());
    }

    #[tokio::test]
    async fn local_changes_are_discarded_on_update() {
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

        assert_eq!(report.updated, vec![key.clone()]);
        assert!(report.flagged.is_empty());
        let state = State::load(&paths.state_file()).unwrap();
        let ws = state.workspace_for(&key).unwrap();
        assert_eq!(ws.status, Status::Ready);
        assert_eq!(ws.version, sha2);
        assert!(ws_path.join("pr2.txt").exists());
        assert!(!ws_path.join("untracked.txt").exists());
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

        remove_workspace(&paths, &key).unwrap();

        assert!(!ws_path.exists());
        let state = State::load(&paths.state_file()).unwrap();
        assert!(
            state.workspace_for(&key).is_none(),
            "review stays tracked, just without a workspace"
        );
        assert!(state.get(&key).is_some());
    }

    #[tokio::test]
    async fn remove_workspace_discards_local_changes() {
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

        remove_workspace(&paths, &key).unwrap();

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
            workspace_dismissed: false,
            ancestors: Vec::new(),
            diff_stat: None,
            description: None,
        });
        state.save(&paths.state_file()).unwrap();

        let err = remove_workspace(&paths, &key).unwrap_err();

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
        /// When set, `checkout_spec` fails with `RateLimited`.
        rate_limited: std::sync::atomic::AtomicBool,
        checkout_calls: std::sync::Arc<std::sync::atomic::AtomicUsize>,
        /// Ids whose patch doesn't apply.
        broken: std::sync::Mutex<Vec<String>>,
    }

    impl StackSource {
        fn new(upstream: &Path) -> Self {
            Self {
                upstream: upstream.to_path_buf(),
                queue: Default::default(),
                lifecycle: std::sync::Mutex::new(Lifecycle::Open),
                rate_limited: Default::default(),
                checkout_calls: Default::default(),
                broken: Default::default(),
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

    /// A patch that conflicts with D1's: it expects `D1.txt` to hold a line it never has.
    fn broken_patch_for(id: &str) -> crate::source::Patch {
        crate::source::Patch {
            diff: "diff --git a/D1.txt b/D1.txt\n--- a/D1.txt\n+++ b/D1.txt\n@@ -1 +1 @@\n-nope\n+changed\n"
                .into(),
            ..patch_for(id)
        }
    }

    #[async_trait::async_trait]
    impl ReviewSource for StackSource {
        fn name(&self) -> &str {
            "stk"
        }

        async fn fetch_queue(&self) -> Result<Queue> {
            Ok(Queue {
                reviews: self.queue.lock().unwrap().clone(),
                ..Default::default()
            })
        }

        async fn checkout_spec(
            &self,
            review: &Review,
            _canonical_repo: &Path,
        ) -> Result<crate::source::Checkout> {
            use std::sync::atomic::Ordering;
            self.checkout_calls.fetch_add(1, Ordering::Relaxed);
            if self.rate_limited.load(Ordering::Relaxed) {
                return Err(RateLimited { name: "stk".into() }.into());
            }
            let broken = self.broken.lock().unwrap();
            let patches = review
                .ancestors
                .iter()
                .chain(std::iter::once(&review.key))
                .map(|k| {
                    if broken.contains(&k.id) {
                        broken_patch_for(&k.id)
                    } else {
                        patch_for(&k.id)
                    }
                })
                .collect();
            Ok(crate::source::Checkout::Patches {
                base: None,
                patches,
            })
        }

        async fn fetch_status(
            &self,
            ids: &[String],
        ) -> Result<Vec<(String, std::result::Result<Lifecycle, String>)>> {
            let lifecycle = *self.lifecycle.lock().unwrap();
            Ok(ids.iter().map(|id| (id.clone(), Ok(lifecycle))).collect())
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
    async fn failed_source_is_reported_and_other_sources_still_sync() {
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
        let gh_key = ReviewKey::new("gh", "moz/proj/1");
        sync(&[github_source(&server)], &paths, None, false)
            .await
            .unwrap();
        drop(server);

        // Every GitHub request now fails, including the status check for the tracked PR.
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;
        let stack = StackSource::new(&repos.upstream);
        stack.set_queue(vec![stack.review(1, &[], "1")]);
        let sources: Vec<Box<dyn ReviewSource>> = vec![github_source(&server), Box::new(stack)];

        let report = sync(&sources, &paths, None, false).await.unwrap();

        assert_eq!(report.source_errors.len(), 1);
        assert_eq!(report.source_errors[0].0, "gh");
        assert!(
            report.source_errors[0].1.contains("500"),
            "{}",
            report.source_errors[0].1
        );
        assert_eq!(report.added, vec![stk(1)]);
        assert!(report.errors.is_empty());
        let state = State::load(&paths.state_file()).unwrap();
        let entry = state.get(&gh_key).unwrap();
        assert!(entry.in_queue);
        assert!(!entry.resolved);
        assert_eq!(entry.version, sha);
    }

    fn auto_workspace_config() -> Config {
        Config {
            auto_workspace: true,
            auto_clone: true,
            ..Config::default()
        }
    }

    #[tokio::test]
    async fn auto_workspace_fetches_once_and_leaves_a_deleted_workspace_alone() {
        use std::sync::atomic::Ordering;

        let repos = make_repos();
        let tmp = TempDir::new().unwrap();
        let paths = paths_in(tmp.path());
        let source = StackSource::new(&repos.upstream);
        source.set_queue(vec![source.review(1, &[], "1")]);
        let calls = source.checkout_calls.clone();
        let sources: Vec<Box<dyn ReviewSource>> = vec![Box::new(source)];
        let config = auto_workspace_config();
        let mut report = sync(&sources, &paths, None, false).await.unwrap();
        auto_workspace(&sources, &paths, &config, &mut report, false)
            .await
            .unwrap();
        assert_eq!(report.fetched, vec![stk(1)]);
        assert!(report.errors.is_empty(), "{:?}", report.errors);
        let state = State::load(&paths.state_file()).unwrap();
        let ws_path = state.workspace_for(&stk(1)).unwrap().workspace_path.clone();
        assert!(ws_path.join("D1.txt").exists());

        let mut report = sync(&sources, &paths, None, false).await.unwrap();
        auto_workspace(&sources, &paths, &config, &mut report, false)
            .await
            .unwrap();
        assert!(report.fetched.is_empty());
        assert_eq!(calls.load(Ordering::Relaxed), 1);

        remove_workspace(&paths, &stk(1)).unwrap();
        let mut report = sync(&sources, &paths, None, false).await.unwrap();
        auto_workspace(&sources, &paths, &config, &mut report, false)
            .await
            .unwrap();
        assert!(report.fetched.is_empty());
        assert_eq!(calls.load(Ordering::Relaxed), 1);
        assert!(!ws_path.exists());

        // A new revision of the stack doesn't bring it back either.
        let next = StackSource::new(&repos.upstream);
        next.set_queue(vec![next.review(1, &[], "2")]);
        let next_calls = next.checkout_calls.clone();
        let sources: Vec<Box<dyn ReviewSource>> = vec![Box::new(next)];
        let mut report = sync(&sources, &paths, None, false).await.unwrap();
        auto_workspace(&sources, &paths, &config, &mut report, false)
            .await
            .unwrap();
        assert!(report.fetched.is_empty());
        assert_eq!(next_calls.load(Ordering::Relaxed), 0);

        // Fetching it by hand clears the dismissal, so a later deletion is needed to stop sync.
        fetch_local(&sources, &paths, &config, &stk(1), OnMissing::Ask)
            .await
            .unwrap();
        let state = State::load(&paths.state_file()).unwrap();
        assert!(!state.get(&stk(1)).unwrap().workspace_dismissed);
        assert!(ws_path.join("D1.txt").exists());
    }

    #[tokio::test]
    async fn auto_workspace_backfills_previously_tracked_reviews() {
        let (_repos, _tmp, paths, sources) = stack_fixture();
        sync(&sources, &paths, None, false).await.unwrap();
        let mut report = sync(&sources, &paths, None, false).await.unwrap();
        auto_workspace(
            &sources,
            &paths,
            &auto_workspace_config(),
            &mut report,
            false,
        )
        .await
        .unwrap();
        assert_eq!(report.fetched, vec![stk(1)]);
        assert_eq!(
            State::load(&paths.state_file())
                .unwrap()
                .workspaces()
                .count(),
            1
        );
    }

    #[tokio::test]
    async fn auto_workspace_rebuilds_an_apply_failed_workspace_once_its_stack_changes() {
        use std::sync::atomic::Ordering;

        let repos = make_repos();
        let tmp = TempDir::new().unwrap();
        let paths = paths_in(tmp.path());
        let source = StackSource::new(&repos.upstream);
        source.set_queue(vec![source.review(1, &[], "1")]);
        source.broken.lock().unwrap().push("D1".into());
        let calls = source.checkout_calls.clone();
        let sources: Vec<Box<dyn ReviewSource>> = vec![Box::new(source)];
        let config = auto_workspace_config();

        let mut report = sync(&sources, &paths, None, false).await.unwrap();
        auto_workspace(&sources, &paths, &config, &mut report, false)
            .await
            .unwrap();
        assert_eq!(report.errors.len(), 1, "{report:?}");
        let state = State::load(&paths.state_file()).unwrap();
        assert_eq!(
            state.workspace_for(&stk(1)).unwrap().status,
            Status::ApplyFailed
        );

        // Unchanged: retrying would just fail the same way.
        let mut report = sync(&sources, &paths, None, false).await.unwrap();
        auto_workspace(&sources, &paths, &config, &mut report, false)
            .await
            .unwrap();
        assert!(report.fetched.is_empty());
        assert_eq!(calls.load(Ordering::Relaxed), 1);

        let next = StackSource::new(&repos.upstream);
        next.set_queue(vec![next.review(1, &[], "2")]);
        let sources: Vec<Box<dyn ReviewSource>> = vec![Box::new(next)];
        let mut report = sync(&sources, &paths, None, false).await.unwrap();
        auto_workspace(&sources, &paths, &config, &mut report, false)
            .await
            .unwrap();
        assert_eq!(report.fetched, vec![stk(1)], "{report:?}");
        let state = State::load(&paths.state_file()).unwrap();
        let ws = state.workspace_for(&stk(1)).unwrap();
        assert_eq!(ws.status, Status::Ready);
        assert_eq!(ws.version, "2");
        assert!(ws.workspace_path.join("D1.txt").exists());
    }

    #[tokio::test]
    async fn auto_workspace_fetches_three_review_stack_once_at_bottom() {
        let repos = make_repos();
        let tmp = TempDir::new().unwrap();
        let paths = paths_in(tmp.path());
        let source = StackSource::new(&repos.upstream);
        source.set_queue(vec![
            source.review(3, &[1, 2], "1"),
            source.review(2, &[1], "1"),
            source.review(1, &[], "1"),
        ]);
        let calls = source.checkout_calls.clone();
        let sources: Vec<Box<dyn ReviewSource>> = vec![Box::new(source)];
        let mut report = sync(&sources, &paths, None, false).await.unwrap();
        auto_workspace(
            &sources,
            &paths,
            &auto_workspace_config(),
            &mut report,
            false,
        )
        .await
        .unwrap();
        assert_eq!(report.fetched, vec![stk(1)]);
        assert_eq!(calls.load(std::sync::atomic::Ordering::Relaxed), 1);
        let state = State::load(&paths.state_file()).unwrap();
        assert_eq!(state.workspaces().count(), 1);
        let ws = state.workspace_for(&stk(3)).unwrap();
        assert_eq!(ws.tip, stk(3));
        assert!(ws.workspace_path.join("D1.txt").exists());
        assert!(!ws.workspace_path.join("D2.txt").exists());
        assert!(!ws.workspace_path.join("D3.txt").exists());
    }

    #[tokio::test]
    async fn auto_workspace_does_not_fetch_new_member_of_existing_workspace() {
        let (repos, _tmp, paths, sources) = stack_fixture();
        let config = auto_workspace_config();
        let mut report = sync(&sources, &paths, None, false).await.unwrap();
        auto_workspace(&sources, &paths, &config, &mut report, false)
            .await
            .unwrap();
        let next = StackSource::new(&repos.upstream);
        next.set_queue(vec![
            next.review(1, &[], "1"),
            next.review(2, &[1], "1"),
            next.review(3, &[1, 2], "1"),
        ]);
        let calls = next.checkout_calls.clone();
        let sources: Vec<Box<dyn ReviewSource>> = vec![Box::new(next)];
        let mut report = sync(&sources, &paths, None, true).await.unwrap();
        auto_workspace(&sources, &paths, &config, &mut report, true)
            .await
            .unwrap();
        assert_eq!(report.added, vec![stk(3)]);
        assert!(report.fetched.is_empty(), "dry run: {:?}", report.fetched);
        let mut report = sync(&sources, &paths, None, false).await.unwrap();
        assert_eq!(report.added, vec![stk(3)]);
        assert_eq!(calls.load(std::sync::atomic::Ordering::Relaxed), 1);
        auto_workspace(&sources, &paths, &config, &mut report, false)
            .await
            .unwrap();
        assert!(report.fetched.is_empty());
        assert_eq!(calls.load(std::sync::atomic::Ordering::Relaxed), 1);
    }

    #[tokio::test]
    async fn auto_workspace_dry_run_reports_candidates_without_writing_state() {
        let (_repos, _tmp, paths, sources) = stack_fixture();
        let mut report = sync(&sources, &paths, None, true).await.unwrap();
        auto_workspace(
            &sources,
            &paths,
            &auto_workspace_config(),
            &mut report,
            true,
        )
        .await
        .unwrap();
        // One stack of two new reviews: one workspace, at the bottom.
        assert_eq!(report.added.len(), 2);
        assert_eq!(report.fetched, vec![stk(1)]);
        assert!(!paths.state_file().exists());
        assert!(!paths.repos_file().exists());
        assert!(!paths.workspace_dir(&stk(1).slug()).exists());
    }

    #[tokio::test]
    async fn auto_workspace_flags_missing_checkout_without_cloning() {
        let (_repos, _tmp, paths, sources) = stack_fixture();
        let config = Config {
            auto_workspace: true,
            ..Config::default()
        };
        let mut report = sync(&sources, &paths, None, false).await.unwrap();
        auto_workspace(&sources, &paths, &config, &mut report, false)
            .await
            .unwrap();
        assert!(report.errors.is_empty());
        assert!(report.fetched.is_empty());
        assert_eq!(
            report.flagged,
            vec![(
                stk(1),
                "no local checkout; run `rq fetch` or set auto_clone".into()
            )]
        );
        assert_eq!(
            State::load(&paths.state_file())
                .unwrap()
                .workspaces()
                .count(),
            0
        );
        assert!(!paths.repos_dir().exists());
    }

    #[tokio::test]
    async fn auto_workspace_continues_after_errors_and_retries_next_sync() {
        let repos = make_repos();
        let tmp = TempDir::new().unwrap();
        let paths = paths_in(tmp.path());
        let source = StackSource::new(&repos.upstream);
        source.set_queue(vec![source.review(1, &[], "1"), source.review(2, &[], "1")]);
        source
            .rate_limited
            .store(true, std::sync::atomic::Ordering::Relaxed);
        let calls = source.checkout_calls.clone();
        let sources: Vec<Box<dyn ReviewSource>> = vec![Box::new(source)];
        let config = auto_workspace_config();
        let mut report = sync(&sources, &paths, None, false).await.unwrap();
        auto_workspace(&sources, &paths, &config, &mut report, false)
            .await
            .unwrap();
        assert_eq!(report.errors.len(), 2);
        assert_eq!(calls.load(std::sync::atomic::Ordering::Relaxed), 2);
        let mut report = sync(&sources, &paths, None, false).await.unwrap();
        auto_workspace(&sources, &paths, &config, &mut report, false)
            .await
            .unwrap();
        assert_eq!(report.errors.len(), 2);
        assert_eq!(calls.load(std::sync::atomic::Ordering::Relaxed), 4);
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
    async fn fetch_local_moves_the_workspace_despite_local_changes() {
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

        assert!(ws_path.join("D1.txt").exists());
        assert!(!ws_path.join("D2.txt").exists(), "must move to D1");
        assert!(!ws_path.join("scratch.txt").exists());
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
    async fn a_stack_that_applies_partway_is_kept_until_it_changes() {
        let repos = make_repos();
        let work_tmp = TempDir::new().unwrap();
        let paths = paths_in(work_tmp.path());
        let config = Config::default();
        let source = StackSource::new(&repos.upstream);
        source.set_queue(vec![
            source.review(1, &[], "1"),
            source.review(2, &[1], "1"),
            source.review(3, &[1, 2], "1"),
        ]);
        source.broken.lock().unwrap().push("D2".into());
        let checkout_calls = source.checkout_calls.clone();
        let sources: Vec<Box<dyn ReviewSource>> = vec![Box::new(source)];
        sync(&sources, &paths, None, false).await.unwrap();

        let ws_path = fetch_local(&sources, &paths, &config, &stk(3), OnMissing::Clone)
            .await
            .expect("a partially applied stack is still a usable workspace");
        assert!(ws_path.join("D1.txt").exists());
        assert!(!ws_path.join("D3.txt").exists());
        let state = State::load(&paths.state_file()).unwrap();
        let ws = state.workspace_for(&stk(3)).unwrap();
        assert_eq!(ws.status, Status::Partial);
        assert_eq!(ws.unapplied, vec![stk(2), stk(3)]);
        assert_eq!(
            partial_note(&state, &stk(1)).unwrap(),
            "stack only partially applied; stk/D2, stk/D3 didn't apply"
        );

        // Neither reopening nor an unchanged sync retries the same broken patches.
        let calls = checkout_calls.load(std::sync::atomic::Ordering::Relaxed);
        fetch_local(&sources, &paths, &config, &stk(1), OnMissing::Clone)
            .await
            .unwrap();
        let report = sync(&sources, &paths, None, false).await.unwrap();
        assert!(report.flagged.is_empty(), "{:?}", report.flagged);
        assert_eq!(
            checkout_calls.load(std::sync::atomic::Ordering::Relaxed),
            calls
        );

        // The author fixes D2: the next sync rebuilds the whole stack.
        let next = StackSource::new(&repos.upstream);
        next.set_queue(vec![
            next.review(1, &[], "1"),
            next.review(2, &[1], "2"),
            next.review(3, &[1, 2], "2"),
        ]);
        let sources: Vec<Box<dyn ReviewSource>> = vec![Box::new(next)];
        let report = sync(&sources, &paths, None, false).await.unwrap();
        assert_eq!(report.updated, vec![stk(3)]);
        let state = State::load(&paths.state_file()).unwrap();
        let ws = state.workspace_for(&stk(3)).unwrap();
        assert_eq!(ws.status, Status::Ready);
        assert!(ws.unapplied.is_empty());
        assert!(partial_note(&state, &stk(3)).is_none());
    }

    #[tokio::test]
    async fn a_rate_limited_source_is_reported_once_and_skipped() {
        let repos = make_repos();
        let work_tmp = TempDir::new().unwrap();
        let paths = paths_in(work_tmp.path());
        let config = Config::default();
        let source = StackSource::new(&repos.upstream);
        source.set_queue(vec![source.review(1, &[], "1"), source.review(2, &[], "1")]);
        let sources: Vec<Box<dyn ReviewSource>> = vec![Box::new(source)];
        sync(&sources, &paths, None, false).await.unwrap();
        for n in [1, 2] {
            fetch_local(&sources, &paths, &config, &stk(n), OnMissing::Clone)
                .await
                .unwrap();
        }

        // Both workspaces need updating, but the first checkout hits a rate limit.
        let next = StackSource::new(&repos.upstream);
        next.set_queue(vec![next.review(1, &[], "2"), next.review(2, &[], "2")]);
        next.rate_limited
            .store(true, std::sync::atomic::Ordering::Relaxed);
        let checkout_calls = next.checkout_calls.clone();
        let sources: Vec<Box<dyn ReviewSource>> = vec![Box::new(next)];
        let report = sync(&sources, &paths, None, false).await.unwrap();

        assert!(report.errors.is_empty(), "{:?}", report.errors);
        assert_eq!(report.source_errors.len(), 1, "{:?}", report.source_errors);
        assert_eq!(report.source_errors[0].0, "stk");
        assert!(report.updated.is_empty());
        assert_eq!(
            checkout_calls.load(std::sync::atomic::Ordering::Relaxed),
            1,
            "the second workspace shouldn't ask the source again"
        );
        let state = State::load(&paths.state_file()).unwrap();
        for n in [1, 2] {
            assert_eq!(state.workspace_for(&stk(n)).unwrap().version, "1");
        }
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

        remove_workspace(&paths, &stk(1)).unwrap();

        assert!(!ws_path.exists());
        let state = State::load(&paths.state_file()).unwrap();
        assert!(state.workspace_for(&stk(2)).is_none());
        assert!(state.get(&stk(2)).unwrap().stack_id.is_none());
    }

    /// A tracked PR with a workspace, which state then loses track of while everything stays on
    /// disk and in the canonical repo - as an interrupted removal would leave it. Returns the
    /// workspace and canonical repo paths.
    async fn orphaned_workspace(
        repos: &Repos,
        paths: &Paths,
        server: &MockServer,
    ) -> (PathBuf, PathBuf) {
        let sha = git_rev_parse(&repos.fork, "HEAD");
        mount_search(server, true).await;
        mount_pull(
            server,
            pull_json("moz", "proj", &repos.upstream, &repos.fork, &sha, "open"),
        )
        .await;
        let key = ReviewKey::new("gh", "moz/proj/1");
        let sources: Vec<Box<dyn ReviewSource>> = vec![github_source(server)];
        sync(&sources, paths, None, false).await.unwrap();
        let ws_path = fetch_local(&sources, paths, &Config::default(), &key, OnMissing::Clone)
            .await
            .unwrap();

        let mut state = State::load(&paths.state_file()).unwrap();
        let id = state.get(&key).unwrap().stack_id.clone().unwrap();
        let canon = state.remove_workspace(&id).unwrap().repo_path;
        state.for_each_entry_mut(|e| e.stack_id = None);
        state.save(&paths.state_file()).unwrap();
        (ws_path, canon)
    }

    fn worktrees_and_pins(canon: &Path) -> (usize, String) {
        let worktrees = Command::new("git")
            .current_dir(canon)
            .args(["worktree", "list", "--porcelain"])
            .output()
            .unwrap();
        let refs = Command::new("git")
            .current_dir(canon)
            .args(["for-each-ref", "refs/review-queue/"])
            .output()
            .unwrap();
        (
            String::from_utf8_lossy(&worktrees.stdout)
                .lines()
                .filter(|l| l.starts_with("worktree "))
                .count(),
            String::from_utf8_lossy(&refs.stdout).trim().to_string(),
        )
    }

    #[tokio::test]
    async fn orphaned_workspace_directory_is_pruned_with_its_pins() {
        let repos = make_repos();
        let tmp = TempDir::new().unwrap();
        let paths = paths_in(tmp.path());
        let server = MockServer::start().await;
        let (ws_path, canon) = orphaned_workspace(&repos, &paths, &server).await;

        let report = sync(&[github_source(&server)], &paths, None, false)
            .await
            .unwrap();

        assert_eq!(report.pruned.len(), 1, "{report:?}");
        assert!(report.prune_flagged.is_empty(), "{report:?}");
        assert!(!ws_path.exists());
        assert_eq!(worktrees_and_pins(&canon), (1, String::new()));
        // The owner/repo directories it sat in went with it.
        assert!(!paths.workspaces_dir().join("gh").exists());
    }

    #[tokio::test]
    async fn stale_registration_and_pins_without_a_directory_are_pruned() {
        let repos = make_repos();
        let tmp = TempDir::new().unwrap();
        let paths = paths_in(tmp.path());
        let server = MockServer::start().await;
        let (ws_path, canon) = orphaned_workspace(&repos, &paths, &server).await;
        std::fs::remove_dir_all(&ws_path).unwrap();

        let report = sync(&[github_source(&server)], &paths, None, false)
            .await
            .unwrap();

        assert_eq!(report.pruned.len(), 1, "{report:?}");
        assert_eq!(worktrees_and_pins(&canon), (1, String::new()));
        assert!(!paths.workspaces_dir().join("gh").exists());
    }

    #[tokio::test]
    async fn orphaned_workspace_with_local_changes_is_removed() {
        let repos = make_repos();
        let tmp = TempDir::new().unwrap();
        let paths = paths_in(tmp.path());
        let server = MockServer::start().await;
        let (ws_path, canon) = orphaned_workspace(&repos, &paths, &server).await;
        std::fs::write(ws_path.join("notes.txt"), "mine\n").unwrap();

        let report = sync(&[github_source(&server)], &paths, None, false)
            .await
            .unwrap();

        assert_eq!(report.pruned.len(), 1, "{report:?}");
        assert!(report.prune_flagged.is_empty(), "{report:?}");
        assert!(!ws_path.exists());
        assert_eq!(worktrees_and_pins(&canon), (1, String::new()));
    }

    #[tokio::test]
    async fn dry_run_reports_orphans_without_removing_them() {
        let repos = make_repos();
        let tmp = TempDir::new().unwrap();
        let paths = paths_in(tmp.path());
        let server = MockServer::start().await;
        let (ws_path, _canon) = orphaned_workspace(&repos, &paths, &server).await;

        let report = sync(&[github_source(&server)], &paths, None, true)
            .await
            .unwrap();

        assert_eq!(report.pruned.len(), 1, "{report:?}");
        assert!(ws_path.exists());
    }

    #[tokio::test]
    async fn only_source_leaves_other_sources_orphans_alone() {
        let repos = make_repos();
        let tmp = TempDir::new().unwrap();
        let paths = paths_in(tmp.path());
        let server = MockServer::start().await;
        let (ws_path, _canon) = orphaned_workspace(&repos, &paths, &server).await;

        let report = sync(&[github_source(&server)], &paths, Some("phab"), false)
            .await
            .unwrap();

        assert!(report.pruned.is_empty(), "{report:?}");
        assert!(ws_path.exists());
    }

    #[test]
    fn empty_dirs_are_removed_but_workspaces_and_files_are_kept() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        std::fs::create_dir_all(root.join("gh/a/b")).unwrap();
        std::fs::create_dir_all(root.join("gh/c/ws/.git")).unwrap();
        std::fs::create_dir_all(root.join("gh/c/ws/empty")).unwrap();
        std::fs::create_dir_all(root.join("phab/kept")).unwrap();
        std::fs::write(root.join("phab/kept/file"), "").unwrap();

        remove_empty_dirs(root);

        assert!(!root.join("gh/a").exists());
        assert!(root.join("gh/c/ws/empty").exists());
        assert!(root.join("phab/kept/file").exists());
        assert!(root.exists());
    }

    #[test]
    fn orphan_slug_handles_the_old_layout() {
        let tmp = TempDir::new().unwrap();
        let paths = paths_in(tmp.path());
        assert_eq!(
            orphan_slug(&paths, &paths.workspace_dir("phab/D1")),
            "phab/D1"
        );
        assert_eq!(
            orphan_slug(
                &paths,
                &paths
                    .data_dir()
                    .join("repos/github.com/o/r/workspaces/gh/o/r/4")
            ),
            "gh/o/r/4"
        );
    }
}
