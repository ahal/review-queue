//! The `ReviewSource` abstraction: sources say *what* to check out, the `vcs` module decides
//! *how*. See the design plan for the GitHub/moz-phab implementations; this module currently
//! only has the shared types and trait.

use std::path::Path;

use anyhow::Result;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};

pub mod diffstat;
pub mod github;
pub mod moz_phab;

/// `{source}/{id}` identity for a review; re-exported here since sources produce it.
pub use crate::state::ReviewKey;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lifecycle {
    Open,
    /// Landed/closed/abandoned/merged: safe to remove the workspace once clean.
    Resolved,
}

/// Serialized into `state.json` (as part of `ReviewEntry`) so a review's kind survives between
/// `sync` and a later on-demand `rq fetch`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ReviewKind {
    Direct,
    Group(String),
}

/// A repo referenced by a review, before it's resolved to a canonical local repo. Serialized into
/// `state.json` (as part of `ReviewEntry`) so `rq fetch`/the TUI can resolve it later without
/// re-querying the source.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RepoRef {
    /// Candidate clone URLs (mirrors, ssh/https variants) - matched against discovered workdir
    /// repos and the tool-managed clone registry after normalization.
    pub urls: Vec<String>,
    pub display_name: String,
}

#[derive(Debug, Clone)]
pub struct Review {
    pub key: ReviewKey,
    pub title: String,
    pub author: String,
    pub url: String,
    pub repo: RepoRef,
    /// Diff ID / head SHA (or comma-joined stack of diff IDs for Phabricator). Used to detect
    /// updates that require re-syncing the workspace.
    pub version: String,
    pub kind: ReviewKind,
    /// Diffstat (same summary format `git diff --stat`/`jj diff --stat` print), fetched
    /// best-effort as part of the same `fetch_queue()` call that built this `Review` - `None` if
    /// the source couldn't get one (no active diff, a Conduit/API hiccup, etc). Carried straight
    /// into `state.json`'s `ReviewEntry` so `rq show`'s TUI can show it with no further network
    /// calls of its own.
    pub diff_stat: Option<String>,
    /// The PR description / Phabricator revision summary (the commit message body - the title is
    /// carried separately in `title`). Carried into `state.json` alongside `diff_stat` so the TUI
    /// can show it with no network calls of its own.
    pub description: Option<String>,
    /// This review's ancestors within its stack, bottom-most first, excluding itself and any
    /// already-landed ones (those are part of the base). Empty for a review that isn't stacked.
    pub ancestors: Vec<ReviewKey>,
}

#[derive(Debug, Clone)]
pub struct Patch {
    pub title: String,
    /// `"Name <email>"`, ready to pass straight to `git commit --author`/`jj commit --author`.
    pub author: String,
    pub message: String,
    pub diff: String,
}

#[derive(Debug, Clone)]
pub enum Checkout {
    /// e.g. GitHub: fetch `refspec` from the canonical repo's own origin, expect it to resolve
    /// to `commit`. The git backend only ever needs `refspec`/`commit`; `fork`, when present,
    /// is used solely by the jj backend, which tracks the PR head's own remote+branch rather
    /// than importing an anonymous ref (see `vcs::jj`).
    Ref {
        refspec: String,
        commit: String,
        fork: Option<ForkRef>,
    },
    /// Apply `patches` bottom-to-top on top of `base` (or the canonical repo's default branch
    /// tip if `base` is unset). No current source produces this - `moz_phab` delegates the whole
    /// apply step to the `moz-phab` CLI instead (see `ExternalCommand`) - but it's kept as a
    /// generic extension point for a future source that fetches raw diffs itself.
    Patches {
        base: Option<String>,
        patches: Vec<Patch>,
    },
    /// Delegate the entire checkout to an external command, run with the given `env` added and
    /// `cwd` set to the workspace. The workspace starts at the canonical repo's default branch
    /// tip (same starting point as `Patches` with `base: None`) - the command is expected to
    /// move it wherever it needs to itself (`moz-phab patch --apply-to base` resolves and checks
    /// out the revision's actual base on its own, including cases a source's own Conduit calls
    /// can't cheaply replicate, like an unlanded base needing a git-cinnabar hg/git translation -
    /// confirmed against real Mozilla Phabricator, not just moz-phab's source). A nonzero exit
    /// leaves the workspace in place for inspection, same as a failed `Patches` application.
    ExternalCommand {
        program: String,
        args: Vec<String>,
        env: Vec<(String, String)>,
    },
}

/// The PR head's own remote+branch, e.g. `https://github.com/alice/firefox` + `feature-x`.
/// `None` when the source fork has been deleted, in which case the jj backend falls back to a
/// raw fetch-and-import of `refspec`/`commit` instead.
#[derive(Debug, Clone)]
pub struct ForkRef {
    /// Name to register the remote under in the canonical jj repo. Callers should use `"origin"`
    /// instead of registering a redundant remote when this normalizes to the canonical repo's
    /// own origin URL (a same-repo branch PR).
    pub remote_name: String,
    pub remote_url: String,
    pub branch: String,
}

#[async_trait]
pub trait ReviewSource: Send + Sync {
    /// Instance name from config, e.g. "moz" - distinguishes multiple configured instances of
    /// the same source type and forms the `source` half of a `ReviewKey`.
    fn name(&self) -> &str;

    async fn fetch_queue(&self) -> Result<Vec<Review>>;

    /// `canonical_repo` is the already-resolved local repo this review's workspace will be built
    /// from - most sources ignore it, but one that needs to prepare local repo state first (e.g.
    /// `moz_phab` writing `.git/.arcconfig` so `moz-phab` can find the right Phabricator
    /// instance) needs it before it can build the `Checkout`.
    async fn checkout_spec(&self, review: &Review, canonical_repo: &Path) -> Result<Checkout>;

    /// Refresh the lifecycle of reviews that are no longer in the queue (e.g. changes were
    /// requested, or the reviewer was removed) so `sync` knows whether to keep the workspace.
    async fn fetch_status(&self, ids: &[String]) -> Result<Vec<(String, Lifecycle)>>;

    /// Whether `message` (a commit message from a workspace built for a stack containing
    /// `review`) is the commit that carries `review`'s patch. Lets a stack workspace be
    /// positioned at one member's patch. Sources that don't stack reviews never need this.
    fn is_commit_for(&self, _review: &ReviewKey, _message: &str) -> bool {
        false
    }
}
