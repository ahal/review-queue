//! VCS backends (milestone 2): everything shells out to the `git`/`jj` CLIs, since jj has no
//! stable library API and `git worktree`/`git apply` support in gix/git2 is incomplete.
//!
//! Operations on the same canonical repo must be serialized (a per-repo mutex, added when the
//! sync engine lands); different repos and sources can run concurrently.

use std::path::Path;

use anyhow::Result;

use crate::source::Checkout;

pub mod git;
pub mod jj;

pub trait Vcs {
    /// Fetch `refspec_or_sha` into the canonical repo if not already present.
    fn ensure_commit(&self, repo: &Path, refspec_or_sha: &str) -> Result<()>;

    /// Create a new worktree/workspace at `ws` from the canonical `repo`, applying `checkout`.
    /// `name` is the review's key slug (e.g. `moz/D12345`), used to namespace the ref that pins
    /// this `version` so it survives future updates; see `update_workspace`. Returns the
    /// resulting head id.
    fn add_workspace(
        &self,
        repo: &Path,
        ws: &Path,
        checkout: &Checkout,
        name: &str,
        version: &str,
    ) -> Result<String>;

    /// Re-point an existing, clean workspace at a new `checkout`/`version`. The commit the
    /// workspace pointed at before this call remains reachable afterwards (each call pins its
    /// own resulting head under a `name`/`version`-namespaced ref rather than overwriting the
    /// previous one), so old versions stay available for interdiffing until `remove_workspace`
    /// drops them all. Returns the new head id.
    fn update_workspace(
        &self,
        repo: &Path,
        ws: &Path,
        checkout: &Checkout,
        name: &str,
        version: &str,
    ) -> Result<String>;

    /// True if the workspace has local modifications, or its head no longer matches
    /// `expected_head` - either case means `sync`/`prune` must not touch it silently.
    fn is_dirty(&self, ws: &Path, expected_head: &str) -> Result<bool>;

    /// Remove the worktree/workspace and every ref/bookmark `add_workspace`/`update_workspace`
    /// pinned for it under `name`. `force` must be set to remove a workspace `is_dirty` reports
    /// as dirty - callers otherwise get an error rather than silently discarding local changes
    /// (git's own `worktree remove` already refuses a dirty worktree without `--force`; jj's
    /// removal has no such guard, so `force` is a no-op there).
    fn remove_workspace(&self, repo: &Path, ws: &Path, name: &str, force: bool) -> Result<()>;
}
