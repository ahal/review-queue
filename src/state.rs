//! `state.json`: the single source of truth for which reviews have a workspace,
//! where it lives, and whether it's safe for `sync` to touch it.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::config::VcsKind;
use crate::source::{RepoRef, ReviewKind};

/// `{source}/{id}`, e.g. `phab/D12345` or `gh/mozilla/taskgraph/123`. `source` is the configured
/// source's name (used to look up its `ReviewSource` impl); `id` is that source's own review id.
/// Used as the state map key and, via `Paths::workspace_dir`, the workspace directory name - the
/// full slug (not just `id`) is used there so ids can't collide across sources sharing the same
/// canonical repo.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct ReviewKey {
    pub source: String,
    pub id: String,
}

impl ReviewKey {
    pub fn new(source: impl Into<String>, id: impl Into<String>) -> Self {
        Self {
            source: source.into(),
            id: id.into(),
        }
    }

    /// Directory-safe join used for state.json keys and workspace subpaths.
    pub fn slug(&self) -> String {
        format!("{}/{}", self.source, self.id)
    }
}

impl std::fmt::Display for ReviewKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.slug())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    /// Checked out cleanly and matches the latest version from the source.
    Ready,
    /// A patch in the stack failed to apply; the workspace is left as-is for inspection.
    ApplyFailed,
    /// Local modifications (or a HEAD that doesn't match state) prevented an update or removal.
    Dirty,
}

/// A local worktree/workspace fetched for a stack of reviews (a lone review is a stack of one) -
/// absent until the user explicitly asks for one (`rq fetch`, or the open-locally hotkey in `rq
/// show`'s TUI), since `sync` no longer creates these on its own. Stored in `State::workspaces`,
/// keyed by a stable stack id; member reviews point at it via `ReviewEntry::stack_id`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Workspace {
    /// Canonical repo this workspace was created from (may be a tool-managed clone or a
    /// discovered workdir checkout). Kept even if a later scan points elsewhere.
    pub repo_path: PathBuf,
    pub vcs: VcsKind,
    pub workspace_path: PathBuf,
    /// The stack tip's commit id, i.e. what the workspace was built to. The user may have moved
    /// to an earlier patch in the stack since (see `Vcs::position`) - that isn't "dirty".
    pub head_id: String,
    pub status: Status,
    /// The stack's top-most review as of the last build. A different tip (a new patch landed on
    /// top of the stack) means the workspace needs rebuilding.
    pub tip: ReviewKey,
    /// The tip's `version` as of the last build; a change means some patch in the stack changed.
    pub version: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReviewEntry {
    pub key: ReviewKey,
    pub title: String,
    pub author: String,
    pub url: String,
    /// The review's repo, as reported by its source - kept even without a workspace so `rq
    /// fetch`/the TUI can resolve a canonical repo later without re-querying the source.
    pub repo: RepoRef,
    pub kind: ReviewKind,
    /// Diff ID / head SHA (or comma-joined stack of diff IDs for Phabricator) last seen from the
    /// source - independent of whether a workspace has caught up to it.
    pub version: String,
    /// True while the review is waiting on you; false once you've acted (e.g. requested
    /// changes) but the review itself hasn't resolved yet.
    pub in_queue: bool,
    /// The review itself landed/closed/merged/abandoned (as opposed to just dropping out of your
    /// queue). A resolved review with no workspace is dropped on the next sync; one with a
    /// workspace has that workspace removed too, even if it has local changes.
    pub resolved: bool,
    pub last_synced: chrono::DateTime<chrono::Utc>,
    /// Id of the `State::workspaces` entry this review's checked out in, if it's been fetched.
    /// Every member of a stack shares one.
    #[serde(default)]
    pub stack_id: Option<String>,
    /// The review's ancestors within its stack, bottom-most first, excluding itself and any
    /// already-landed ones. Ancestors needn't be tracked themselves (you may not be a reviewer
    /// on them).
    #[serde(default)]
    pub ancestors: Vec<ReviewKey>,
    /// Diffstat fetched from the source as of `last_synced` (same summary format `git diff
    /// --stat`/`jj diff --stat` print) - `None` if the source couldn't produce one. Refreshed on
    /// every `rq sync`, independent of whether a local workspace exists.
    pub diff_stat: Option<String>,
    /// PR description / revision summary as of `last_synced`, refreshed on every `rq sync`. `None`
    /// for entries synced before this was tracked, or if the source has none.
    #[serde(default)]
    pub description: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct State {
    reviews: BTreeMap<String, ReviewEntry>,
    #[serde(default)]
    workspaces: BTreeMap<String, Workspace>,
}

impl State {
    pub fn load(path: &Path) -> Result<Self> {
        if !path.exists() {
            return Ok(Self::default());
        }
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("reading state {}", path.display()))?;
        if text.trim().is_empty() {
            return Ok(Self::default());
        }
        let mut value: serde_json::Value = serde_json::from_str(&text)
            .with_context(|| format!("parsing state {}", path.display()))?;
        migrate_per_review_workspaces(&mut value);
        serde_json::from_value(value).with_context(|| format!("parsing state {}", path.display()))
    }

    /// Atomic write: temp file in the same directory, then rename over the target.
    pub fn save(&self, path: &Path) -> Result<()> {
        let dir = path
            .parent()
            .context("state path has no parent directory")?;
        std::fs::create_dir_all(dir)?;
        let tmp = dir.join(format!(
            ".{}.tmp",
            path.file_name().unwrap().to_string_lossy()
        ));
        let text = serde_json::to_string_pretty(self)?;
        std::fs::write(&tmp, text).with_context(|| format!("writing {}", tmp.display()))?;
        std::fs::rename(&tmp, path)
            .with_context(|| format!("renaming {} to {}", tmp.display(), path.display()))?;
        Ok(())
    }

    pub fn get(&self, key: &ReviewKey) -> Option<&ReviewEntry> {
        self.reviews.get(&key.slug())
    }

    pub fn insert(&mut self, entry: ReviewEntry) {
        self.reviews.insert(entry.key.slug(), entry);
    }

    pub fn remove(&mut self, key: &ReviewKey) -> Option<ReviewEntry> {
        self.reviews.remove(&key.slug())
    }

    pub fn iter(&self) -> impl Iterator<Item = &ReviewEntry> {
        self.reviews.values()
    }

    pub fn workspace(&self, stack_id: &str) -> Option<&Workspace> {
        self.workspaces.get(stack_id)
    }

    /// The workspace `entry` is checked out in, if any.
    pub fn workspace_of(&self, entry: &ReviewEntry) -> Option<&Workspace> {
        entry.stack_id.as_deref().and_then(|id| self.workspace(id))
    }

    pub fn workspace_for(&self, key: &ReviewKey) -> Option<&Workspace> {
        self.get(key).and_then(|e| self.workspace_of(e))
    }

    pub fn workspaces(&self) -> impl Iterator<Item = (&String, &Workspace)> {
        self.workspaces.iter()
    }

    pub fn insert_workspace(&mut self, stack_id: String, ws: Workspace) {
        self.workspaces.insert(stack_id, ws);
    }

    pub fn remove_workspace(&mut self, stack_id: &str) -> Option<Workspace> {
        self.workspaces.remove(stack_id)
    }

    /// Every tracked review checked out in `stack_id`'s workspace.
    pub fn members_of(&self, stack_id: &str) -> Vec<ReviewKey> {
        self.reviews
            .values()
            .filter(|e| e.stack_id.as_deref() == Some(stack_id))
            .map(|e| e.key.clone())
            .collect()
    }

    /// Apply `f` to every entry (used to detach/attach stack membership in bulk).
    pub fn for_each_entry_mut(&mut self, mut f: impl FnMut(&mut ReviewEntry)) {
        self.reviews.values_mut().for_each(&mut f);
    }

    /// Matches from the front of `id` (`"D123"` -> `"D12345"`), from the front of `id`'s last
    /// `/`-delimited segment (GitHub ids are `owner/repo/number`, so `"123"` -> `".../123"` -
    /// otherwise there'd be no way to find a PR by the number you'd actually remember, since it
    /// sits at the end, not the start), or an exact `source/id` slug.
    pub fn find_by_prefix<'a>(&'a self, prefix: &str) -> Vec<&'a ReviewEntry> {
        self.reviews
            .values()
            .filter(|e| {
                let id = &e.key.id;
                id.starts_with(prefix)
                    || id
                        .rsplit('/')
                        .next()
                        .is_some_and(|last| last.starts_with(prefix))
                    || e.key.slug() == prefix
            })
            .collect()
    }
}

/// Older `state.json` files kept a `workspace` on every review; workspaces now live in a
/// top-level map shared by a stack's members. Each old one becomes a one-review stack.
fn migrate_per_review_workspaces(value: &mut serde_json::Value) {
    use serde_json::Value;

    let Some(reviews) = value.get_mut("reviews").and_then(Value::as_object_mut) else {
        return;
    };
    let mut migrated = serde_json::Map::new();
    for (slug, entry) in reviews.iter_mut() {
        let Some(entry) = entry.as_object_mut() else {
            continue;
        };
        let Some(Value::Object(mut ws)) = entry.remove("workspace") else {
            continue;
        };
        ws.insert(
            "tip".into(),
            entry.get("key").cloned().unwrap_or(Value::Null),
        );
        ws.insert(
            "version".into(),
            entry.get("version").cloned().unwrap_or(Value::Null),
        );
        entry.insert("stack_id".into(), Value::String(slug.clone()));
        migrated.insert(slug.clone(), Value::Object(ws));
    }
    if migrated.is_empty() {
        return;
    }
    if let Some(root) = value.as_object_mut() {
        let existing = root
            .entry("workspaces")
            .or_insert_with(|| Value::Object(Default::default()));
        if let Some(existing) = existing.as_object_mut() {
            existing.extend(migrated);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn sample_entry(id: &str) -> ReviewEntry {
        sample_entry_for("moz", id)
    }

    fn sample_entry_for(source: &str, id: &str) -> ReviewEntry {
        ReviewEntry {
            key: ReviewKey::new(source, id),
            title: "Fix the thing".into(),
            author: "someone".into(),
            url: format!("https://phabricator.services.mozilla.com/{id}"),
            repo: RepoRef {
                urls: vec!["https://example.com/o/r".into()],
                display_name: "o/r".into(),
            },
            kind: ReviewKind::Direct,
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
    fn round_trips_through_disk() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("state.json");

        let mut state = State::default();
        state.insert(sample_entry("D1"));
        state.save(&path).unwrap();

        let loaded = State::load(&path).unwrap();
        assert_eq!(
            loaded.get(&ReviewKey::new("moz", "D1")).unwrap().title,
            "Fix the thing"
        );
    }

    #[test]
    fn migrates_per_review_workspaces_into_the_shared_map() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("state.json");
        let mut entry = serde_json::to_value(sample_entry("D1")).unwrap();
        entry.as_object_mut().unwrap().remove("stack_id");
        entry.as_object_mut().unwrap().remove("ancestors");
        entry.as_object_mut().unwrap().insert(
            "workspace".into(),
            serde_json::json!({
                "repo_path": "/tmp/repo",
                "vcs": "git",
                "workspace_path": "/tmp/ws/D1",
                "head_id": "abc123",
                "status": "ready",
            }),
        );
        let old = serde_json::json!({"reviews": {"moz/D1": entry}});
        std::fs::write(&path, old.to_string()).unwrap();

        let state = State::load(&path).unwrap();
        let ws = state.workspace_for(&ReviewKey::new("moz", "D1")).unwrap();
        assert_eq!(ws.head_id, "abc123");
        assert_eq!(ws.tip, ReviewKey::new("moz", "D1"));
        assert_eq!(ws.version, "1");
    }

    #[test]
    fn missing_file_is_empty_state() {
        let state = State::load(Path::new("/nonexistent/state.json")).unwrap();
        assert_eq!(state.iter().count(), 0);
    }

    #[test]
    fn find_by_prefix_matches_id_or_full_slug() {
        let mut state = State::default();
        state.insert(sample_entry("D12345"));
        state.insert(sample_entry("D999"));

        assert_eq!(state.find_by_prefix("D123").len(), 1);
        assert_eq!(state.find_by_prefix("moz/D999").len(), 1);
        assert_eq!(state.find_by_prefix("D").len(), 2);
        assert_eq!(state.find_by_prefix("nope").len(), 0);
    }

    #[test]
    fn find_by_prefix_matches_the_trailing_segment_of_a_multi_part_id() {
        // GitHub ids are "owner/repo/number" - the part someone actually remembers (the PR
        // number) sits at the end, not the start, so a plain `id.starts_with(prefix)` could
        // never find it.
        let mut state = State::default();
        state.insert(sample_entry_for("gh", "mozilla/gecko-dev/123"));
        state.insert(sample_entry_for("gh", "mozilla/other-repo/456"));

        let by_number = state.find_by_prefix("123");
        assert_eq!(by_number.len(), 1);
        assert_eq!(by_number[0].key.id, "mozilla/gecko-dev/123");

        // A prefix of the number should work too, not just the exact number.
        assert_eq!(state.find_by_prefix("12").len(), 1);

        // Matching still starts from a segment boundary - "23" is a suffix of "123" but not a
        // prefix of its own segment, so it must not match.
        assert_eq!(state.find_by_prefix("23").len(), 0);

        // The existing owner/repo-starts-with behavior still works alongside the new rule.
        assert_eq!(state.find_by_prefix("mozilla/gecko-dev").len(), 1);
    }
}
