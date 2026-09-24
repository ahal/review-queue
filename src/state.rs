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
/// canonical repo's workspace directory.
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

/// A local worktree/workspace fetched for a review - absent until the user explicitly asks for
/// one (`rq fetch`, or the fetch hotkey in `rq list`'s TUI), since `sync` no longer creates these
/// on its own.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Workspace {
    /// Canonical repo this workspace was created from (may be a tool-managed clone or a
    /// discovered workdir checkout). Kept even if a later scan points elsewhere.
    pub repo_path: PathBuf,
    pub vcs: VcsKind,
    pub workspace_path: PathBuf,
    pub head_id: String,
    pub status: Status,
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
    /// queue). A resolved review with no workspace is dropped on the next sync; one with a clean
    /// workspace has that workspace removed too - dirty workspaces are kept either way.
    pub resolved: bool,
    pub last_synced: chrono::DateTime<chrono::Utc>,
    pub workspace: Option<Workspace>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct State {
    reviews: BTreeMap<String, ReviewEntry>,
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
        serde_json::from_str(&text).with_context(|| format!("parsing state {}", path.display()))
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
            workspace: Some(Workspace {
                repo_path: PathBuf::from("/tmp/repo"),
                vcs: VcsKind::Git,
                workspace_path: PathBuf::from(format!("/tmp/ws/{id}")),
                head_id: "abc123".into(),
                status: Status::Ready,
            }),
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
