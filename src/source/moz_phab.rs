//! Mozilla's Phabricator, via the `moz-phab` CLI. Not a general-purpose Phabricator source -
//! see the module name. `moz-phab` is the tool every Mozilla contributor already uses to submit
//! and apply patches; delegating the actual checkout to it, rather than reimplementing diff
//! fetching and application ourselves, was a deliberate pivot after an earlier from-scratch
//! implementation turned out to duplicate work `moz-phab` already does more robustly. Everything
//! below the "Checkout" heading was verified empirically against the real, live Mozilla
//! Phabricator and a real `moz-phab` install (not just read from its source), specifically
//! because the earlier from-scratch approach had exactly this kind of thing wrong.
//!
//! Auth: POST form-encoded to `{url}/api/{method}` with `api.token=...`. Nested params flatten
//! to `constraints[members][0]=...` (see `flatten_params`). Token comes from, in order: config
//! `token`, `token_cmd`, then `~/.arcrc` (`hosts["{url}/api/"].token`, which `moz-phab` already
//! writes, so it usually works with no setup). The same resolved token is passed to `moz-phab`
//! itself via `MOZPHAB_PHABRICATOR_API_TOKEN`, so both halves always agree on identity rather
//! than `moz-phab` silently falling back to its own (possibly different) `~/.arcrc` lookup.
//!
//! Queue: `user.whoami` -> `project.search {constraints:{members:[me]}}` (my groups, unless
//! `include_groups` is off) -> `differential.revision.search {queryKey:"active",
//! attachments:{reviewers:true}}`, paginating on `cursor.after`. Bucketed locally (mirroring
//! Phabricator's own `DifferentialRevisionRequiredActionResultBucket`, which Conduit doesn't
//! expose - this part is transcribed from myqonly's real, working
//! `addon/services/phabricator-service.mjs`):
//! - skip if `fields.authorPHID` is me
//! - skip if my (or my group's) reviewer entry has `status == "resigned"`
//! - require `fields.status.value == "needs-review"`
//! - require a reviewer entry of mine (or a group I'm in) with status in
//!   `blocking|rejected|rejected-older|added|commented` (`accepted` is deliberately excluded -
//!   nothing left to do)
//!
//! Author PHIDs are resolved to usernames with one batched `user.search
//! {constraints:{phids:[...]}}` call over every distinct author in the page, rather than showing
//! raw PHIDs in `rq list`/`rq path`.
//!
//! Every per-revision Conduit lookup after the initial queue fetch is batched across *all*
//! actionable revisions rather than issued once per revision - a queue of N revisions used to cost
//! roughly `N + N * stack_depth` extra calls (one `diffusion.repository.search` per revision, plus
//! a `edge.search`/`differential.revision.search` pair per stack hop per revision), which is easy
//! to trip Phabricator's rate limit on. Now:
//! - Repos: one `diffusion.repository.search {constraints:{phids:[...]}}` over every distinct
//!   `repositoryPHID` among actionable revisions (`repo_refs_for`).
//! - Stacks: `resolve_stacks` walks every actionable revision's parent chain in lockstep, one
//!   `edge.search` + `differential.revision.search` pair *per depth level* covering every stack
//!   still in flight, instead of per revision. A 20-revision queue with 3-deep stacks now costs
//!   ~1 (repos) + 3*2 (stack levels) calls instead of ~20 + 20*(1+2*3).
//! - Diffstats: `diff_stats_for` resolves every actionable revision's `diffPHID` through one
//!   batched `differential.diff.search` call, then fetches each resulting diff's raw text via
//!   `differential.getrawdiff` - unlike the above, this one *is* one call per revision, since
//!   `getrawdiff` has no batch form; see `diff_stats_for`'s own doc comment for why (and for why
//!   it's `getrawdiff`, not the more obvious-looking `differential.querydiffs`).
//!
//! `fetch_queue` fetches each `Review`'s diffstat itself (via `diff_stats_for`, above) rather than
//! `rq list`'s TUI fetching it lazily on expand, so the TUI never blocks on Conduit.
//!
//! `version` is the comma-joined *`dateModified`* of every revision in the stack (walked via
//! `edge.search`, same as before), not diff ids - `moz-phab` re-resolves the live diff/base
//! itself on every invocation regardless of what we pass it, so this only needs to answer "has
//! anything about this stack changed since we last synced," and `dateModified` answers that with
//! zero extra Conduit calls beyond the stack walk we're already doing (`resolve_stacks`' own
//! `differential.revision.search` calls already return it).
//!
//! # Checkout
//!
//! `checkout_spec` returns `Checkout::ExternalCommand { program: "moz-phab", args: ["patch",
//! "D<id>", "--apply-to", "base", "--yes", "--name", <key>], env }`. `moz-phab patch` handles
//! everything the earlier implementation did by hand, and does it better:
//! - Walks the full dependency stack itself (confirmed live: patching a revision automatically
//!   discovered and applied its parent too, unprompted).
//! - Resolves the actual base commit itself, including a fallback our own `fields.refs`-based
//!   base lookup never had: if the recorded base isn't a public/landed commit (e.g. it belongs
//!   to another unlanded stack, or needs a git-cinnabar hg<->git translation), it walks forward
//!   to the latest landed ancestor and applies there instead ("Base revision ... is not public
//!   ... Applying the patch at ... instead" - observed live against `mozilla-central`).
//! - `--yes` fully suppresses its interactive "patch the full stack?" prompt.
//! - Exits 1 on failure (a real patch-doesn't-apply case was observed live), leaving the
//!   workspace checked out at the resolved base with nothing applied - which is exactly the
//!   `Status::ApplyFailed` "leave it for inspection" contract the `Vcs` backends already have.
//!
//! **Requires `repository.callsign` in `.arcconfig`, not just `phabricator.uri`** - confirmed
//! live: `moz-phab patch` fails outright with "Failed to determine the Phabricator callsign for
//! this repository" without it, even though static analysis of `moz-phab`'s own source suggested
//! the callsign was only needed by `submit`. `checkout_spec` looks the callsign up itself
//! (`diffusion.repository.search`) and writes both fields to `.git/.arcconfig` - never the repo
//! root's own `.arcconfig`, which might be absent, tracked, or (as seen live in a real non-central
//! repo) present but missing the callsign. `.git/.arcconfig` is checked first and never touches
//! tracked working-tree content; every canonical repo here is colocated with a real `.git` by
//! construction (tool-managed clones always are; jj canonical repos always are too, by our own
//! design choice), so this path needs no per-VCS fallback.
//!
//! **Known limitation, not yet handled**: `moz-phab` defaults to the remote named `origin` and
//! warns ("Multiple remotes found. Defaulting to 'origin'.") if a repo has more than one. Every
//! tool-managed clone only ever has one remote, so this never bites the common path - but a
//! discovered workdir checkout with multiple/nonstandard remotes (common for Mozilla developers,
//! e.g. a `central`/`try` naming scheme instead of `origin`) may need `git.remote` set in their
//! own `~/.moz-phab-config`, which this tool doesn't configure on their behalf.
//!
//! Whether `moz-phab` itself is installed and runnable is never checked by this module - only
//! `checkout_spec`'s `ExternalCommand` actually shells out to it, and testing this module
//! shouldn't require `moz-phab` to be on `PATH`.
//!
//! `fetch_status`: `differential.revision.search {constraints:{ids:[...]}}`. `status.value` of
//! `published` (landed) or `abandoned` is `Lifecycle::Resolved`; a revision id not found in the
//! response at all is also treated as `Resolved`, so a review that's vanished (e.g. lost access)
//! doesn't linger forever.

use std::collections::HashMap;
use std::path::Path;

use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};

use crate::config::MozPhabConfig;
use crate::source::diffstat::{FileChange, format_diffstat};
use crate::source::{Checkout, Lifecycle, RepoRef, Review, ReviewKey, ReviewKind, ReviewSource};

/// This source's hardcoded id-namespace prefix - see [`crate::state::ReviewKey`].
pub const NAME: &str = "phab";

pub struct MozPhabSource {
    cfg: MozPhabConfig,
    client: reqwest::Client,
    token: Option<String>,
}

impl MozPhabSource {
    pub async fn new(cfg: MozPhabConfig) -> Result<Self> {
        let token = resolve_token(&cfg).await?;
        Ok(Self {
            cfg,
            client: reqwest::Client::new(),
            token,
        })
    }

    #[cfg(test)]
    pub(crate) fn for_test(cfg: MozPhabConfig, token: Option<String>) -> Self {
        Self {
            cfg,
            client: reqwest::Client::new(),
            token,
        }
    }

    async fn call<T: DeserializeOwned>(&self, method: &str, params: Value) -> Result<T> {
        let token = self
            .token
            .as_ref()
            .context("moz-phab source has no token")?;
        let mut form = flatten_params(&params);
        form.push(("api.token".to_string(), token.clone()));

        let url = format!("{}/api/{method}", self.cfg.url.trim_end_matches('/'));
        let resp = self
            .client
            .post(&url)
            .form(&form)
            .send()
            .await
            .with_context(|| format!("POST {url}"))?;
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            bail!("POST {url} failed: {status} {body}");
        }

        let raw: ConduitEnvelope = resp
            .json()
            .await
            .with_context(|| format!("parsing response from {url}"))?;
        if let Some(code) = raw.error_code {
            bail!(
                "{method} failed ({code}): {}",
                raw.error_info.unwrap_or_default()
            );
        }
        serde_json::from_value(raw.result)
            .with_context(|| format!("parsing `result` from {method}"))
    }

    /// Fetch every page of a `*.search` method, following `cursor.after`.
    async fn search_all<T: DeserializeOwned>(
        &self,
        method: &str,
        mut params: Value,
    ) -> Result<Vec<T>> {
        let mut out = Vec::new();
        let mut after: Option<String> = None;
        for _ in 0..10 {
            if let Some(a) = &after {
                params["after"] = json!(a);
            }
            let page: SearchResult<T> = self.call(method, params.clone()).await?;
            let done = page.cursor.after.is_none();
            out.extend(page.data);
            if done {
                break;
            }
            after = page.cursor.after;
        }
        Ok(out)
    }

    /// Batch stack resolution: walks every seed's parent chain in lockstep, one `edge.search` +
    /// `differential.revision.search` pair *per depth level* covering every stack still in
    /// flight, rather than one pair per stack. See the module docs for why this matters (it's
    /// what keeps `fetch_queue`'s Conduit call count independent of queue size). Each seed is
    /// keyed by its own `revision_id` in the returned map. Stops each chain at (and excludes) its
    /// first closed ancestor - its content is already part of the base. Used only for `version`
    /// tracking now; `moz-phab patch` does its own, more capable stack walk for the actual
    /// checkout.
    async fn resolve_stacks(
        &self,
        seeds: Vec<StackMember>,
    ) -> Result<HashMap<u64, Vec<StackMember>>> {
        let mut chains: HashMap<u64, Vec<StackMember>> = HashMap::new();
        let mut current_phid: HashMap<u64, String> = HashMap::new();
        for seed in seeds {
            current_phid.insert(seed.revision_id, seed.revision_phid.clone());
            chains.insert(seed.revision_id, vec![seed]);
        }

        for _ in 0..50 {
            // safety valve against an unexpected cycle
            if current_phid.is_empty() {
                break;
            }
            let phids: Vec<String> = current_phid
                .values()
                .cloned()
                .collect::<std::collections::BTreeSet<_>>()
                .into_iter()
                .collect();
            let edges: Vec<EdgeItem> = self
                .search_all(
                    "edge.search",
                    json!({"sourcePHIDs": phids, "types": ["revision.parent"]}),
                )
                .await?;
            let mut parent_of: HashMap<String, String> = HashMap::new();
            for edge in edges {
                parent_of
                    .entry(edge.source_phid)
                    .or_insert(edge.destination_phid);
            }

            let parent_phids: Vec<String> = current_phid
                .values()
                .filter_map(|phid| parent_of.get(phid).cloned())
                .collect::<std::collections::BTreeSet<_>>()
                .into_iter()
                .collect();
            let parents: HashMap<String, RevisionItem> = if parent_phids.is_empty() {
                HashMap::new()
            } else {
                self.search_all::<RevisionItem>(
                    "differential.revision.search",
                    json!({"constraints": {"phids": parent_phids}}),
                )
                .await?
                .into_iter()
                .map(|r| (r.phid.clone(), r))
                .collect()
            };

            let mut next_current = HashMap::new();
            for (revision_id, phid) in current_phid {
                let Some(parent_phid) = parent_of.get(&phid) else {
                    continue; // no parent edge; this stack is done
                };
                let Some(parent) = parents.get(parent_phid) else {
                    continue; // parent vanished; stop here
                };
                if is_closed(&parent.fields.status.value) {
                    continue; // landed/abandoned ancestor already part of the base
                }
                chains.get_mut(&revision_id).unwrap().push(StackMember {
                    revision_id: parent.id,
                    revision_phid: parent.phid.clone(),
                    date_modified: parent.fields.date_modified.unwrap_or(0),
                });
                next_current.insert(revision_id, parent.phid.clone());
            }
            current_phid = next_current;
        }

        for chain in chains.values_mut() {
            chain.reverse();
        }
        Ok(chains)
    }

    /// Diffstat for every candidate with an active diff, keyed by revision id - fetched as part
    /// of the same `fetch_queue()` call that builds the `Review`s themselves, not lazily by the
    /// TUI. Best-effort throughout: any failure just means affected revision(s) get no diffstat,
    /// never a failed queue fetch.
    ///
    /// `differential.querydiffs` (the obvious-looking Conduit call for this - a diff's per-file
    /// `changes` with line counts) doesn't work against Mozilla's Phabricator: it's missing from
    /// `moz-phab`'s own `IDEMPOTENT_CONDUIT_METHODS` allowlist, and unlike every other method this
    /// module calls, `moz-phab`'s source never uses it - confirmed by reading `mozphab/conduit.py`
    /// rather than by hitting the API live, since this tool has no Phabricator credentials of its
    /// own to test against. What `moz-phab patch` uses instead to get diff content is
    /// `differential.getrawdiff {diffID}` (a single diff id, not batchable) - a raw unified diff
    /// text, the same format `Checkout::Patches`' `Patch::diff` already is elsewhere in this
    /// codebase - which `parse_unified_diff` below turns into the same per-file stats
    /// `querydiffs` would have. Every candidate's `diffPHID` is still resolved to a numeric diff
    /// id via one batched `differential.diff.search` call first (that part *is* confirmed real,
    /// via `mozphab/conduit.py`'s `get_diffs`); only the raw-diff fetch itself is one call per
    /// revision, run sequentially (mirrors `moz-phab patch`'s own per-diff `getrawdiff` calls,
    /// just without its `ThreadPoolExecutor` concurrency).
    async fn diff_stats_for<'a>(
        &self,
        revisions: impl IntoIterator<Item = &'a RevisionItem>,
    ) -> HashMap<u64, String> {
        self.diff_stats_for_inner(revisions).await.unwrap_or_default()
    }

    async fn diff_stats_for_inner<'a>(
        &self,
        revisions: impl IntoIterator<Item = &'a RevisionItem>,
    ) -> Result<HashMap<u64, String>> {
        let phid_to_rev: HashMap<String, u64> = revisions
            .into_iter()
            .filter_map(|r| r.fields.diff_phid.clone().map(|phid| (phid, r.id)))
            .collect();
        if phid_to_rev.is_empty() {
            return Ok(HashMap::new());
        }

        // Sorted for a deterministic request body, same as `author_phids`/`repo_phids` above.
        let phids: Vec<String> = phid_to_rev
            .keys()
            .cloned()
            .collect::<std::collections::BTreeSet<_>>()
            .into_iter()
            .collect();
        let diffs: Vec<DiffItem> = self
            .search_all(
                "differential.diff.search",
                json!({"constraints": {"phids": phids}}),
            )
            .await?;
        let diff_id_to_rev: HashMap<u64, u64> = diffs
            .into_iter()
            .filter_map(|d| phid_to_rev.get(&d.phid).map(|&rev_id| (d.id, rev_id)))
            .collect();

        let mut out = HashMap::new();
        for (diff_id, revision_id) in diff_id_to_rev {
            let Ok(raw) = self
                .call::<String>("differential.getrawdiff", json!({"diffID": diff_id}))
                .await
            else {
                continue; // one revision's diff failing to fetch shouldn't cost the others
            };
            out.insert(revision_id, format_diffstat(&parse_unified_diff(&raw)));
        }
        Ok(out)
    }

    #[cfg(test)]
    async fn resolve_stack(&self, revision_id: u64) -> Result<Vec<StackMember>> {
        let start: Vec<RevisionItem> = self
            .search_all(
                "differential.revision.search",
                json!({"constraints": {"ids": [revision_id]}}),
            )
            .await?;
        let start = start
            .into_iter()
            .next()
            .with_context(|| format!("revision D{revision_id} not found"))?;
        let seed = StackMember {
            revision_id: start.id,
            revision_phid: start.phid,
            date_modified: start.fields.date_modified.unwrap_or(0),
        };
        let mut chains = self.resolve_stacks(vec![seed]).await?;
        Ok(chains.remove(&revision_id).unwrap_or_default())
    }

    /// Resolve author PHIDs to usernames for display (`rq list`/`rq path` show `moz-phab`-style
    /// usernames, not raw PHIDs). Falls back to the PHID itself for any that don't resolve,
    /// rather than failing the whole queue fetch over a display nicety.
    async fn resolve_usernames(&self, phids: &[String]) -> Result<HashMap<String, String>> {
        if phids.is_empty() {
            return Ok(HashMap::new());
        }
        let users: Vec<UserItem> = self
            .search_all("user.search", json!({"constraints": {"phids": phids}}))
            .await?;
        Ok(users
            .into_iter()
            .map(|u| (u.phid, u.fields.username))
            .collect())
    }

    /// One batched `diffusion.repository.search {constraints:{phids:[...]}}` over every distinct
    /// repo among the given phids, keyed by repo phid in the result - mirrors `resolve_usernames`.
    async fn repo_refs_for(&self, repository_phids: &[String]) -> Result<HashMap<String, RepoRef>> {
        if repository_phids.is_empty() {
            return Ok(HashMap::new());
        }
        let repos: Vec<RepoItem> = self
            .search_all(
                "diffusion.repository.search",
                json!({"constraints": {"phids": repository_phids}, "attachments": {"uris": true}}),
            )
            .await?;
        let mut out = HashMap::new();
        for repo in repos {
            let urls: Vec<String> = repo
                .attachments
                .and_then(|a| a.uris)
                .map(|u| {
                    u.uris
                        .into_iter()
                        .map(|item| item.fields.uri.effective)
                        .collect()
                })
                .unwrap_or_default();
            if urls.is_empty() {
                bail!("repository {} has no URIs", repo.phid);
            }
            out.insert(
                repo.phid.clone(),
                RepoRef {
                    urls,
                    display_name: repo.fields.short_name.unwrap_or(repo.phid),
                },
            );
        }
        Ok(out)
    }

    /// Look up the revision's repo and write `.git/.arcconfig` in the canonical repo so
    /// `moz-phab` can find the right Phabricator instance/repo. See the module docs for why this
    /// is required (a live-verified correction to what `moz-phab`'s own source suggested) and why
    /// `.git/.arcconfig` specifically (checked first, never touches tracked content).
    async fn ensure_arcconfig(&self, canonical_repo: &Path, revision_id: u64) -> Result<()> {
        let revisions: Vec<RevisionItem> = self
            .search_all(
                "differential.revision.search",
                json!({"constraints": {"ids": [revision_id]}}),
            )
            .await?;
        let rev = revisions
            .into_iter()
            .next()
            .with_context(|| format!("revision D{revision_id} not found"))?;
        let repository_phid = rev
            .fields
            .repository_phid
            .with_context(|| format!("D{revision_id} has no repository attached"))?;

        let repos: Vec<RepoItem> = self
            .search_all(
                "diffusion.repository.search",
                json!({"constraints": {"phids": [repository_phid]}}),
            )
            .await?;
        let repo = repos
            .into_iter()
            .next()
            .with_context(|| format!("repository {repository_phid} not found"))?;
        let callsign = repo
            .fields
            .callsign
            .with_context(|| format!("repository {repository_phid} has no Phabricator callsign; moz-phab can't patch into it"))?;

        let git_dir = canonical_repo.join(".git");
        if !git_dir.is_dir() {
            bail!(
                "expected a colocated `.git` at {} for moz-phab to use",
                canonical_repo.display()
            );
        }
        let arcconfig_path = git_dir.join(".arcconfig");
        let contents = serde_json::to_string_pretty(
            &json!({"phabricator.uri": self.cfg.url, "repository.callsign": callsign}),
        )?;
        std::fs::write(&arcconfig_path, contents)
            .with_context(|| format!("writing {}", arcconfig_path.display()))?;
        Ok(())
    }
}

#[async_trait]
impl ReviewSource for MozPhabSource {
    fn name(&self) -> &str {
        NAME
    }

    async fn fetch_queue(&self) -> Result<Vec<Review>> {
        let who: WhoAmI = self.call("user.whoami", json!({})).await?;
        let my_phid = who.phid;

        let mut mine_phids = vec![my_phid.clone()];
        let mut group_names: HashMap<String, String> = HashMap::new();
        if self.cfg.include_groups {
            let projects: Vec<ProjectItem> = self
                .search_all(
                    "project.search",
                    json!({"constraints": {"members": [my_phid]}}),
                )
                .await?;
            for p in projects {
                group_names.insert(p.phid.clone(), p.fields.name);
                mine_phids.push(p.phid);
            }
        }

        let revisions: Vec<RevisionItem> = self
            .search_all(
                "differential.revision.search",
                json!({"queryKey": "active", "attachments": {"reviewers": true}}),
            )
            .await?;

        let author_phids: Vec<String> = revisions
            .iter()
            .map(|r| r.fields.author_phid.clone())
            .collect::<std::collections::BTreeSet<_>>()
            .into_iter()
            .collect();
        let usernames = self.resolve_usernames(&author_phids).await?;

        // Filter down to actionable revisions before doing any per-revision Conduit lookups, then
        // batch those lookups (repos, stacks) across all of them at once - see the module docs.
        let mut candidates = Vec::new();
        for rev in revisions {
            let Some(kind) = bucket_revision(&my_phid, &mine_phids, &group_names, &rev) else {
                continue;
            };
            let Some(repository_phid) = rev.fields.repository_phid.clone() else {
                continue; // no repo attached; nothing for us to check out
            };
            candidates.push((rev, kind, repository_phid));
        }

        let repo_phids: Vec<String> = candidates
            .iter()
            .map(|(_, _, phid)| phid.clone())
            .collect::<std::collections::BTreeSet<_>>()
            .into_iter()
            .collect();
        let repos = self.repo_refs_for(&repo_phids).await?;

        let seeds: Vec<StackMember> = candidates
            .iter()
            .map(|(rev, _, _)| StackMember {
                revision_id: rev.id,
                revision_phid: rev.phid.clone(),
                date_modified: rev.fields.date_modified.unwrap_or(0),
            })
            .collect();
        let mut stacks = self.resolve_stacks(seeds).await?;
        let diff_stats = self
            .diff_stats_for(candidates.iter().map(|(rev, _, _)| rev))
            .await;

        let mut reviews = Vec::new();
        for (rev, kind, repository_phid) in &candidates {
            let repo = repos
                .get(repository_phid)
                .cloned()
                .with_context(|| format!("repository {repository_phid} not found"))?;
            let stack = stacks.remove(&rev.id).unwrap_or_default();
            let version = stack
                .iter()
                .map(|m| m.date_modified.to_string())
                .collect::<Vec<_>>()
                .join(",");

            reviews.push(Review {
                key: ReviewKey::new(NAME, format!("D{}", rev.id)),
                title: rev.fields.title.clone(),
                author: usernames
                    .get(&rev.fields.author_phid)
                    .cloned()
                    .unwrap_or_else(|| rev.fields.author_phid.clone()),
                url: format!("{}/D{}", self.cfg.url.trim_end_matches('/'), rev.id),
                repo,
                version,
                kind: kind.clone(),
                diff_stat: diff_stats.get(&rev.id).cloned(),
            });
        }
        Ok(reviews)
    }

    async fn checkout_spec(&self, review: &Review, canonical_repo: &Path) -> Result<Checkout> {
        let revision_id = parse_id(&review.key.id)?;
        self.ensure_arcconfig(canonical_repo, revision_id).await?;

        let mut env = Vec::new();
        if let Some(token) = &self.token {
            env.push(("MOZPHAB_PHABRICATOR_API_TOKEN".to_string(), token.clone()));
        }
        Ok(Checkout::ExternalCommand {
            program: "moz-phab".to_string(),
            args: vec![
                "patch".to_string(),
                format!("D{revision_id}"),
                "--apply-to".to_string(),
                "base".to_string(),
                "--yes".to_string(),
                "--name".to_string(),
                review.key.slug().replace('/', "-"),
            ],
            env,
        })
    }

    async fn fetch_status(&self, ids: &[String]) -> Result<Vec<(String, Lifecycle)>> {
        let numeric_ids: Vec<u64> = ids.iter().map(|s| parse_id(s)).collect::<Result<_>>()?;
        let revisions: Vec<RevisionItem> = self
            .search_all(
                "differential.revision.search",
                json!({"constraints": {"ids": numeric_ids}}),
            )
            .await?;
        let by_id: HashMap<u64, RevisionItem> = revisions.into_iter().map(|r| (r.id, r)).collect();

        let mut out = Vec::with_capacity(ids.len());
        for id_str in ids {
            let id = parse_id(id_str)?;
            let lifecycle = match by_id.get(&id) {
                Some(r) if is_closed(&r.fields.status.value) => Lifecycle::Resolved,
                Some(_) => Lifecycle::Open,
                // Vanished entirely (e.g. lost access) - don't track it forever.
                None => Lifecycle::Resolved,
            };
            out.push((id_str.clone(), lifecycle));
        }
        Ok(out)
    }
}

fn is_closed(status: &str) -> bool {
    matches!(status, "published" | "abandoned")
}

fn is_actionable(status: &str) -> bool {
    matches!(
        status,
        "blocking" | "rejected" | "rejected-older" | "added" | "commented"
    )
}

/// The bucketing decision itself, pulled out of `fetch_queue` so it's directly unit-testable
/// without any network mocking - this is the part transcribed from myqonly's own
/// `_bucketRevisions`, and the highest-risk logic in this source (get it wrong and reviews
/// either go missing or show up when there's nothing to do).
///
/// `mine_phids` is `[my_phid, ...my group phids]`; `group_names` maps a group phid to its
/// display name, used only when the actionable match came from a group rather than me directly.
fn bucket_revision(
    my_phid: &str,
    mine_phids: &[String],
    group_names: &HashMap<String, String>,
    rev: &RevisionItem,
) -> Option<ReviewKind> {
    if rev.fields.author_phid == my_phid {
        return None;
    }
    let reviewers = rev
        .attachments
        .as_ref()
        .and_then(|a| a.reviewers.as_ref())?;
    let mine: Vec<&ReviewerEntry> = reviewers
        .reviewers
        .iter()
        .filter(|r| mine_phids.contains(&r.reviewer_phid))
        .collect();
    if mine.is_empty() || mine.iter().any(|r| r.status == "resigned") {
        return None;
    }
    if rev.fields.status.value != "needs-review" {
        return None;
    }
    let matched = mine.iter().find(|r| is_actionable(&r.status))?;
    Some(if matched.reviewer_phid == my_phid {
        ReviewKind::Direct
    } else {
        ReviewKind::Group(
            group_names
                .get(&matched.reviewer_phid)
                .cloned()
                .unwrap_or_else(|| matched.reviewer_phid.clone()),
        )
    })
}

/// `token`, then `token_cmd`, then `~/.arcrc` (`hosts["{url}/api/"].token`).
async fn resolve_token(cfg: &MozPhabConfig) -> Result<Option<String>> {
    if let Some(t) = &cfg.token {
        return Ok(Some(t.clone()));
    }
    if let Some(cmd) = &cfg.token_cmd {
        let out = tokio::process::Command::new("sh")
            .arg("-c")
            .arg(cmd)
            .output()
            .await
            .with_context(|| format!("running token_cmd `{cmd}`"))?;
        if !out.status.success() {
            bail!(
                "token_cmd `{cmd}` failed: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        let token = String::from_utf8_lossy(&out.stdout).trim().to_string();
        if !token.is_empty() {
            return Ok(Some(token));
        }
    }
    if let Some(base_dirs) = directories::BaseDirs::new() {
        let arcrc_path = base_dirs.home_dir().join(".arcrc");
        if let Ok(text) = std::fs::read_to_string(&arcrc_path)
            && let Ok(value) = serde_json::from_str::<Value>(&text)
        {
            let key = format!("{}/api/", cfg.url.trim_end_matches('/'));
            if let Some(token) = value
                .get("hosts")
                .and_then(|h| h.get(&key))
                .and_then(|h| h.get("token"))
                .and_then(|t| t.as_str())
            {
                return Ok(Some(token.to_string()));
            }
        }
    }
    Ok(None)
}

/// `"D12345"` -> `12345`.
fn parse_id(id: &str) -> Result<u64> {
    id.strip_prefix('D')
        .with_context(|| format!("bad moz-phab review id `{id}`"))?
        .parse()
        .with_context(|| format!("bad moz-phab review id `{id}`"))
}

/// Flatten a JSON object into Conduit's bracket-notation form fields, e.g.
/// `{"constraints": {"members": ["X"]}}` -> `[("constraints[members][0]", "X")]`.
fn flatten_params(value: &Value) -> Vec<(String, String)> {
    let mut out = Vec::new();
    if let Value::Object(map) = value {
        for (k, v) in map {
            flatten_into(k, v, &mut out);
        }
    }
    out
}

fn flatten_into(prefix: &str, value: &Value, out: &mut Vec<(String, String)>) {
    match value {
        Value::Null => {}
        Value::Bool(b) => out.push((prefix.to_string(), b.to_string())),
        Value::Number(n) => out.push((prefix.to_string(), n.to_string())),
        Value::String(s) => out.push((prefix.to_string(), s.clone())),
        Value::Array(items) => {
            for (i, item) in items.iter().enumerate() {
                flatten_into(&format!("{prefix}[{i}]"), item, out);
            }
        }
        Value::Object(map) => {
            for (k, v) in map {
                flatten_into(&format!("{prefix}[{k}]"), v, out);
            }
        }
    }
}

// `revision_id`/`revision_phid` are read by tests (asserting exactly which revisions ended up in
// the stack) but not by `fetch_queue` itself, which only needs `date_modified` now that the
// stack walk no longer feeds checkout building.
#[allow(dead_code)]
struct StackMember {
    revision_id: u64,
    revision_phid: String,
    date_modified: i64,
}

#[derive(Deserialize)]
struct ConduitEnvelope {
    result: Value,
    error_code: Option<String>,
    error_info: Option<String>,
}

#[derive(Deserialize)]
struct SearchResult<T> {
    data: Vec<T>,
    cursor: SearchCursor,
}

#[derive(Deserialize)]
struct SearchCursor {
    after: Option<String>,
}

#[derive(Debug, Deserialize)]
struct WhoAmI {
    phid: String,
}

#[derive(Deserialize)]
struct ProjectItem {
    phid: String,
    fields: ProjectFields,
}

#[derive(Deserialize)]
struct ProjectFields {
    name: String,
}

#[derive(Deserialize)]
struct UserItem {
    phid: String,
    fields: UserFields,
}

#[derive(Deserialize)]
struct UserFields {
    username: String,
}

#[derive(Deserialize)]
struct RevisionItem {
    id: u64,
    phid: String,
    fields: RevisionFields,
    attachments: Option<RevisionAttachments>,
}

#[derive(Deserialize)]
struct RevisionFields {
    title: String,
    #[serde(rename = "authorPHID")]
    author_phid: String,
    status: StatusField,
    #[serde(rename = "repositoryPHID")]
    repository_phid: Option<String>,
    #[serde(rename = "dateModified")]
    date_modified: Option<i64>,
    #[serde(rename = "diffPHID")]
    diff_phid: Option<String>,
}

#[derive(Deserialize)]
struct StatusField {
    value: String,
}

#[derive(Deserialize)]
struct RevisionAttachments {
    reviewers: Option<ReviewersAttachment>,
}

#[derive(Deserialize)]
struct ReviewersAttachment {
    reviewers: Vec<ReviewerEntry>,
}

#[derive(Deserialize)]
struct ReviewerEntry {
    #[serde(rename = "reviewerPHID")]
    reviewer_phid: String,
    status: String,
}

#[derive(Deserialize)]
struct EdgeItem {
    #[serde(rename = "sourcePHID")]
    source_phid: String,
    #[serde(rename = "destinationPHID")]
    destination_phid: String,
}

/// A `differential.diff.search` result - `id` is what `differential.getrawdiff` (which takes a
/// diff id, not a PHID) needs; `phid` maps a result back to the revision that requested it, since
/// `diff_stats_for` batches this search across every candidate's `diffPHID` at once.
#[derive(Deserialize)]
struct DiffItem {
    id: u64,
    phid: String,
}

/// Per-file `+`/`-` counts from a unified diff's raw text - the same format `Checkout::Patches`'
/// `Patch::diff` field already is elsewhere in this codebase (applied via `git apply`/`patch
/// -p1`), so this is a safe assumption for whatever `differential.getrawdiff` hands back. Only
/// lines within a hunk that start with `+`/`-` count, not the `+++`/`---` file headers. A rename
/// with no content change still gets a zero-count entry (from `diff --git` starting a new
/// section) so it's at least listed.
fn parse_unified_diff(diff: &str) -> Vec<FileChange> {
    let mut changes = Vec::new();
    let mut current: Option<FileChange> = None;

    for line in diff.lines() {
        if let Some(rest) = line.strip_prefix("diff --git a/") {
            if let Some(c) = current.take() {
                changes.push(c);
            }
            let path = rest
                .rsplit_once(" b/")
                .map_or(rest, |(_, new_path)| new_path)
                .to_string();
            current = Some(FileChange {
                path,
                additions: 0,
                deletions: 0,
            });
        } else if line.starts_with("+++") || line.starts_with("---") {
            continue; // file headers, not hunk content
        } else if let Some(c) = current.as_mut() {
            if line.starts_with('+') {
                c.additions += 1;
            } else if line.starts_with('-') {
                c.deletions += 1;
            }
        }
    }
    if let Some(c) = current.take() {
        changes.push(c);
    }
    changes
}

#[derive(Deserialize)]
struct RepoItem {
    phid: String,
    fields: RepoFields,
    attachments: Option<RepoAttachments>,
}

#[derive(Deserialize)]
struct RepoFields {
    #[serde(rename = "shortName")]
    short_name: Option<String>,
    callsign: Option<String>,
}

#[derive(Deserialize)]
struct RepoAttachments {
    uris: Option<UrisAttachment>,
}

#[derive(Deserialize)]
struct UrisAttachment {
    uris: Vec<UriItem>,
}

#[derive(Deserialize)]
struct UriItem {
    fields: UriItemFields,
}

#[derive(Deserialize)]
struct UriItemFields {
    uri: UriValue,
}

#[derive(Deserialize)]
struct UriValue {
    effective: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;
    use wiremock::matchers::{body_string_contains, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    const ME: &str = "PHID-USER-me";
    const GROUP: &str = "PHID-PROJ-reviewers";

    fn cfg(url: &str) -> MozPhabConfig {
        MozPhabConfig {
            url: url.into(),
            token: Some("t".into()),
            token_cmd: None,
            include_groups: true,
        }
    }

    fn revision(author: &str, status: &str, reviewers: Vec<(&str, &str)>) -> RevisionItem {
        RevisionItem {
            id: 1,
            phid: "PHID-DREV-1".into(),
            fields: RevisionFields {
                title: "Fix the thing".into(),
                author_phid: author.into(),
                status: StatusField {
                    value: status.into(),
                },
                repository_phid: Some("PHID-REPO-1".into()),
                date_modified: Some(1700000000),
                diff_phid: None,
            },
            attachments: Some(RevisionAttachments {
                reviewers: Some(ReviewersAttachment {
                    reviewers: reviewers
                        .into_iter()
                        .map(|(phid, status)| ReviewerEntry {
                            reviewer_phid: phid.into(),
                            status: status.into(),
                        })
                        .collect(),
                }),
            }),
        }
    }

    fn mine() -> Vec<String> {
        vec![ME.to_string(), GROUP.to_string()]
    }

    fn groups() -> HashMap<String, String> {
        HashMap::from([(GROUP.to_string(), "reviewers".to_string())])
    }

    #[test]
    fn flatten_params_produces_bracket_notation() {
        let params = json!({
            "queryKey": "active",
            "constraints": {"members": ["PHID-1"], "ids": [1, 2]},
            "attachments": {"reviewers": true},
        });
        let pairs = flatten_params(&params);
        assert!(pairs.contains(&("queryKey".to_string(), "active".to_string())));
        assert!(pairs.contains(&("constraints[members][0]".to_string(), "PHID-1".to_string())));
        assert!(pairs.contains(&("constraints[ids][0]".to_string(), "1".to_string())));
        assert!(pairs.contains(&("constraints[ids][1]".to_string(), "2".to_string())));
        assert!(pairs.contains(&("attachments[reviewers]".to_string(), "true".to_string())));
    }

    #[test]
    fn parse_id_round_trip() {
        assert_eq!(parse_id("D12345").unwrap(), 12345);
        assert!(parse_id("12345").is_err());
        assert!(parse_id("Dabc").is_err());
    }

    #[test]
    fn bucket_skips_own_revision() {
        let rev = revision(ME, "needs-review", vec![("PHID-USER-other", "added")]);
        assert_eq!(bucket_revision(ME, &mine(), &groups(), &rev), None);
    }

    #[test]
    fn bucket_skips_when_resigned() {
        let rev = revision("PHID-USER-author", "needs-review", vec![(ME, "resigned")]);
        assert_eq!(bucket_revision(ME, &mine(), &groups(), &rev), None);
    }

    #[test]
    fn bucket_skips_non_needs_review_status() {
        let rev = revision("PHID-USER-author", "changes-planned", vec![(ME, "added")]);
        assert_eq!(bucket_revision(ME, &mine(), &groups(), &rev), None);
    }

    #[test]
    fn bucket_excludes_accepted() {
        let rev = revision("PHID-USER-author", "needs-review", vec![(ME, "accepted")]);
        assert_eq!(
            bucket_revision(ME, &mine(), &groups(), &rev),
            None,
            "accepted has nothing left to do"
        );
    }

    #[test]
    fn bucket_skips_when_no_reviewer_entry_of_mine() {
        let rev = revision(
            "PHID-USER-author",
            "needs-review",
            vec![("PHID-USER-other", "added")],
        );
        assert_eq!(bucket_revision(ME, &mine(), &groups(), &rev), None);
    }

    #[test]
    fn bucket_includes_all_actionable_statuses_direct() {
        for status in [
            "blocking",
            "rejected",
            "rejected-older",
            "added",
            "commented",
        ] {
            let rev = revision("PHID-USER-author", "needs-review", vec![(ME, status)]);
            assert_eq!(
                bucket_revision(ME, &mine(), &groups(), &rev),
                Some(ReviewKind::Direct),
                "status `{status}` should be actionable"
            );
        }
    }

    #[test]
    fn bucket_includes_group_match_with_group_name() {
        let rev = revision("PHID-USER-author", "needs-review", vec![(GROUP, "added")]);
        assert_eq!(
            bucket_revision(ME, &mine(), &groups(), &rev),
            Some(ReviewKind::Group("reviewers".to_string()))
        );
    }

    #[test]
    fn bucket_ignores_group_match_when_groups_not_passed() {
        // Simulates `include_groups = false`: caller only passes `[my_phid]` as `mine_phids`.
        let rev = revision("PHID-USER-author", "needs-review", vec![(GROUP, "added")]);
        assert_eq!(
            bucket_revision(ME, &[ME.to_string()], &groups(), &rev),
            None
        );
    }

    #[test]
    fn bucket_skips_when_no_reviewers_attachment() {
        let mut rev = revision("PHID-USER-author", "needs-review", vec![(ME, "added")]);
        rev.attachments = None;
        assert_eq!(bucket_revision(ME, &mine(), &groups(), &rev), None);
    }

    fn search_response(items: &[Value]) -> Value {
        json!({"result": {"data": items, "cursor": {"after": null}}, "error_code": null, "error_info": null})
    }

    fn call_response(result: Value) -> Value {
        json!({"result": result, "error_code": null, "error_info": null})
    }

    #[tokio::test]
    async fn fetch_status_maps_statuses_and_missing_ids() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/differential.revision.search"))
            .respond_with(ResponseTemplate::new(200).set_body_json(search_response(&[
                json!({"id": 1, "phid": "PHID-DREV-1", "fields": {"title": "x", "authorPHID": "a", "status": {"value": "published"}, "repositoryPHID": null, "dateModified": 1}}),
                json!({"id": 2, "phid": "PHID-DREV-2", "fields": {"title": "x", "authorPHID": "a", "status": {"value": "needs-review"}, "repositoryPHID": null, "dateModified": 1}}),
                json!({"id": 3, "phid": "PHID-DREV-3", "fields": {"title": "x", "authorPHID": "a", "status": {"value": "abandoned"}, "repositoryPHID": null, "dateModified": 1}}),
            ])))
            .mount(&server)
            .await;

        let src = MozPhabSource::for_test(cfg(&server.uri()), Some("t".into()));
        let ids = vec![
            "D1".to_string(),
            "D2".to_string(),
            "D3".to_string(),
            "D4".to_string(),
        ];
        let statuses = src.fetch_status(&ids).await.unwrap();

        assert_eq!(statuses[0], ("D1".to_string(), Lifecycle::Resolved));
        assert_eq!(statuses[1], ("D2".to_string(), Lifecycle::Open));
        assert_eq!(statuses[2], ("D3".to_string(), Lifecycle::Resolved));
        assert_eq!(
            statuses[3],
            ("D4".to_string(), Lifecycle::Resolved),
            "an id missing from the response should count as resolved"
        );
    }

    #[tokio::test]
    async fn call_surfaces_conduit_errors() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/user.whoami"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "result": null, "error_code": "ERR-INVALID-AUTH", "error_info": "Bad token",
            })))
            .mount(&server)
            .await;

        let src = MozPhabSource::for_test(cfg(&server.uri()), Some("t".into()));
        let err = src
            .call::<WhoAmI>("user.whoami", json!({}))
            .await
            .unwrap_err();
        assert!(
            err.to_string().contains("ERR-INVALID-AUTH"),
            "unexpected error: {err}"
        );
    }

    #[tokio::test]
    async fn resolve_stack_stops_at_a_closed_ancestor() {
        let server = MockServer::start().await;
        // D2 (the review we're checking out) depends on D1, which has already landed.
        Mock::given(method("POST"))
            .and(path("/api/differential.revision.search"))
            .and(body_string_contains("constraints%5Bids%5D%5B0%5D=2"))
            .respond_with(ResponseTemplate::new(200).set_body_json(search_response(&[json!({
                "id": 2, "phid": "PHID-DREV-2", "fields": {"title": "child", "authorPHID": "a", "status": {"value": "needs-review"}, "repositoryPHID": null, "dateModified": 200},
            })])))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/api/edge.search"))
            .respond_with(ResponseTemplate::new(200).set_body_json(search_response(&[
                json!({"sourcePHID": "PHID-DREV-2", "destinationPHID": "PHID-DREV-1"}),
            ])))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/api/differential.revision.search"))
            .and(body_string_contains("constraints%5Bphids%5D%5B0%5D=PHID-DREV-1"))
            .respond_with(ResponseTemplate::new(200).set_body_json(search_response(&[json!({
                "id": 1, "phid": "PHID-DREV-1", "fields": {"title": "parent", "authorPHID": "a", "status": {"value": "published"}, "repositoryPHID": null, "dateModified": 100},
            })])))
            .mount(&server)
            .await;

        let src = MozPhabSource::for_test(cfg(&server.uri()), Some("t".into()));
        let stack = src.resolve_stack(2).await.unwrap();

        assert_eq!(
            stack.len(),
            1,
            "the landed parent must not be included in the stack"
        );
        assert_eq!(stack[0].revision_id, 2);
        assert_eq!(stack[0].date_modified, 200);
    }

    #[tokio::test]
    async fn fetch_queue_end_to_end() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/user.whoami"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(call_response(json!({"phid": ME, "userName": "ahal"}))),
            )
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/api/project.search"))
            .respond_with(ResponseTemplate::new(200).set_body_json(search_response(&[])))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/api/differential.revision.search"))
            .and(body_string_contains("queryKey=active"))
            .respond_with(ResponseTemplate::new(200).set_body_json(search_response(&[json!({
                "id": 1, "phid": "PHID-DREV-1",
                "fields": {"title": "Fix the thing", "authorPHID": "PHID-USER-other", "status": {"value": "needs-review"}, "repositoryPHID": "PHID-REPO-1", "dateModified": 1700000000},
                "attachments": {"reviewers": {"reviewers": [{"reviewerPHID": ME, "status": "added"}]}},
            })])))
            .mount(&server)
            .await;
        // resolve_stacks: no parent edge for D1's stack.
        Mock::given(method("POST"))
            .and(path("/api/edge.search"))
            .respond_with(ResponseTemplate::new(200).set_body_json(search_response(&[])))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/api/diffusion.repository.search"))
            .respond_with(ResponseTemplate::new(200).set_body_json(search_response(&[json!({
                "phid": "PHID-REPO-1", "fields": {"shortName": "proj", "callsign": "PROJ"},
                "attachments": {"uris": {"uris": [{"fields": {"uri": {"effective": "https://phab.example.com/source/proj.git"}}}]}},
            })])))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/api/user.search"))
            .respond_with(ResponseTemplate::new(200).set_body_json(search_response(&[
                json!({"phid": "PHID-USER-other", "fields": {"username": "alice"}}),
            ])))
            .mount(&server)
            .await;

        let src = MozPhabSource::for_test(cfg(&server.uri()), Some("t".into()));
        let reviews = src.fetch_queue().await.unwrap();

        assert_eq!(reviews.len(), 1);
        let r = &reviews[0];
        assert_eq!(r.key, ReviewKey::new("phab", "D1"));
        assert_eq!(r.title, "Fix the thing");
        assert_eq!(
            r.author, "alice",
            "author PHID should resolve to a username"
        );
        assert_eq!(r.version, "1700000000");
        assert_eq!(r.kind, ReviewKind::Direct);
        assert_eq!(
            r.repo.urls,
            vec!["https://phab.example.com/source/proj.git".to_string()]
        );
    }

    /// Regression guard for the N+1 pattern that used to trip Phabricator's rate limit: two
    /// actionable revisions sharing a repo and each with a one-level-deep stack must resolve with
    /// exactly one `diffusion.repository.search` and one `edge.search` call between them, not one
    /// each. `.expect(1)` fails the test (on `MockServer` teardown) if either is called twice.
    #[tokio::test]
    async fn fetch_queue_batches_repo_and_stack_lookups_across_revisions() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/user.whoami"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(call_response(json!({"phid": ME, "userName": "ahal"}))),
            )
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/api/project.search"))
            .respond_with(ResponseTemplate::new(200).set_body_json(search_response(&[])))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/api/differential.revision.search"))
            .and(body_string_contains("queryKey=active"))
            .respond_with(ResponseTemplate::new(200).set_body_json(search_response(&[
                json!({
                    "id": 1, "phid": "PHID-DREV-1",
                    "fields": {"title": "D1", "authorPHID": "PHID-USER-other", "status": {"value": "needs-review"}, "repositoryPHID": "PHID-REPO-1", "dateModified": 1},
                    "attachments": {"reviewers": {"reviewers": [{"reviewerPHID": ME, "status": "added"}]}},
                }),
                json!({
                    "id": 2, "phid": "PHID-DREV-2",
                    "fields": {"title": "D2", "authorPHID": "PHID-USER-other", "status": {"value": "needs-review"}, "repositoryPHID": "PHID-REPO-1", "dateModified": 2},
                    "attachments": {"reviewers": {"reviewers": [{"reviewerPHID": ME, "status": "added"}]}},
                }),
            ])))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/api/edge.search"))
            .respond_with(ResponseTemplate::new(200).set_body_json(search_response(&[])))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/api/diffusion.repository.search"))
            .respond_with(ResponseTemplate::new(200).set_body_json(search_response(&[json!({
                "phid": "PHID-REPO-1", "fields": {"shortName": "proj", "callsign": "PROJ"},
                "attachments": {"uris": {"uris": [{"fields": {"uri": {"effective": "https://phab.example.com/source/proj.git"}}}]}},
            })])))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/api/user.search"))
            .respond_with(ResponseTemplate::new(200).set_body_json(search_response(&[
                json!({"phid": "PHID-USER-other", "fields": {"username": "alice"}}),
            ])))
            .mount(&server)
            .await;

        let src = MozPhabSource::for_test(cfg(&server.uri()), Some("t".into()));
        let mut reviews = src.fetch_queue().await.unwrap();
        reviews.sort_by(|a, b| a.key.id.cmp(&b.key.id));

        assert_eq!(reviews.len(), 2);
        assert_eq!(reviews[0].key, ReviewKey::new("phab", "D1"));
        assert_eq!(reviews[1].key, ReviewKey::new("phab", "D2"));
        for r in &reviews {
            assert_eq!(
                r.repo.urls,
                vec!["https://phab.example.com/source/proj.git".to_string()]
            );
        }
    }

    #[tokio::test]
    async fn checkout_spec_writes_arcconfig_and_builds_moz_phab_command() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/differential.revision.search"))
            .respond_with(ResponseTemplate::new(200).set_body_json(search_response(&[json!({
                "id": 1, "phid": "PHID-DREV-1",
                "fields": {"title": "x", "authorPHID": "a", "status": {"value": "needs-review"}, "repositoryPHID": "PHID-REPO-1", "dateModified": 1},
            })])))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/api/diffusion.repository.search"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(search_response(&[json!({
                    "phid": "PHID-REPO-1", "fields": {"shortName": "proj", "callsign": "PROJ"},
                })])),
            )
            .mount(&server)
            .await;

        let canon = TempDir::new().unwrap();
        std::fs::create_dir(canon.path().join(".git")).unwrap();

        let src = MozPhabSource::for_test(cfg(&server.uri()), Some("secret-token".into()));
        let review = Review {
            key: ReviewKey::new("phab", "D1"),
            title: "x".into(),
            author: "a".into(),
            url: "https://phab.example.com/D1".into(),
            repo: RepoRef {
                urls: vec![],
                display_name: "repo".into(),
            },
            version: "1".into(),
            kind: ReviewKind::Direct,
            diff_stat: None,
        };
        let checkout = src.checkout_spec(&review, canon.path()).await.unwrap();

        let arcconfig: Value = serde_json::from_str(
            &std::fs::read_to_string(canon.path().join(".git/.arcconfig")).unwrap(),
        )
        .unwrap();
        assert_eq!(arcconfig["phabricator.uri"], server.uri());
        assert_eq!(arcconfig["repository.callsign"], "PROJ");

        match checkout {
            Checkout::ExternalCommand { program, args, env } => {
                assert_eq!(program, "moz-phab");
                assert_eq!(
                    args,
                    vec![
                        "patch",
                        "D1",
                        "--apply-to",
                        "base",
                        "--yes",
                        "--name",
                        "phab-D1"
                    ]
                );
                assert!(env.contains(&(
                    "MOZPHAB_PHABRICATOR_API_TOKEN".to_string(),
                    "secret-token".to_string()
                )));
            }
            _ => panic!("expected ExternalCommand"),
        }
    }

    #[tokio::test]
    async fn checkout_spec_errors_without_a_callsign() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/differential.revision.search"))
            .respond_with(ResponseTemplate::new(200).set_body_json(search_response(&[json!({
                "id": 1, "phid": "PHID-DREV-1",
                "fields": {"title": "x", "authorPHID": "a", "status": {"value": "needs-review"}, "repositoryPHID": "PHID-REPO-1", "dateModified": 1},
            })])))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/api/diffusion.repository.search"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(search_response(&[json!({
                    "phid": "PHID-REPO-1", "fields": {"shortName": "proj", "callsign": null},
                })])),
            )
            .mount(&server)
            .await;

        let canon = TempDir::new().unwrap();
        std::fs::create_dir(canon.path().join(".git")).unwrap();

        let src = MozPhabSource::for_test(cfg(&server.uri()), Some("t".into()));
        let review = Review {
            key: ReviewKey::new("phab", "D1"),
            title: "x".into(),
            author: "a".into(),
            url: "https://phab.example.com/D1".into(),
            repo: RepoRef {
                urls: vec![],
                display_name: "repo".into(),
            },
            version: "1".into(),
            kind: ReviewKind::Direct,
            diff_stat: None,
        };
        let err = src.checkout_spec(&review, canon.path()).await.unwrap_err();
        assert!(
            err.to_string().contains("callsign"),
            "unexpected error: {err}"
        );
    }

    fn revision_with_diff(id: u64, diff_phid: Option<&str>) -> RevisionItem {
        RevisionItem {
            id,
            phid: format!("PHID-DREV-{id}"),
            fields: RevisionFields {
                title: "x".into(),
                author_phid: "a".into(),
                status: StatusField {
                    value: "needs-review".into(),
                },
                repository_phid: None,
                date_modified: Some(1),
                diff_phid: diff_phid.map(String::from),
            },
            attachments: None,
        }
    }

    #[tokio::test]
    async fn diff_stats_for_batches_diff_search_then_fetches_each_raw_diff() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/differential.diff.search"))
            .and(body_string_contains("constraints%5Bphids%5D%5B0%5D=PHID-DIFF-1"))
            .and(body_string_contains("constraints%5Bphids%5D%5B1%5D=PHID-DIFF-2"))
            .respond_with(ResponseTemplate::new(200).set_body_json(search_response(&[
                json!({"id": 42, "phid": "PHID-DIFF-1"}),
                json!({"id": 43, "phid": "PHID-DIFF-2"}),
            ])))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/api/differential.getrawdiff"))
            .and(body_string_contains("diffID=42"))
            .respond_with(ResponseTemplate::new(200).set_body_json(call_response(json!(
                "diff --git a/src/main.rs b/src/main.rs\n--- a/src/main.rs\n+++ b/src/main.rs\n@@ -1,1 +1,3 @@\n+one\n+two\n-old\n"
            ))))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/api/differential.getrawdiff"))
            .and(body_string_contains("diffID=43"))
            .respond_with(ResponseTemplate::new(200).set_body_json(call_response(json!(
                "diff --git a/old.rs b/old.rs\n--- a/old.rs\n+++ /dev/null\n@@ -1,1 +0,0 @@\n-gone\n"
            ))))
            .mount(&server)
            .await;

        let src = MozPhabSource::for_test(cfg(&server.uri()), Some("t".into()));
        let revisions = [
            revision_with_diff(1, Some("PHID-DIFF-1")),
            revision_with_diff(2, Some("PHID-DIFF-2")),
        ];
        let stats = src.diff_stats_for(revisions.iter()).await;

        assert!(stats[&1].contains("src/main.rs"));
        assert!(stats[&1].ends_with("1 file changed, 2 insertions(+), 1 deletion(-)"));
        assert!(stats[&2].contains("old.rs"));
        assert!(stats[&2].ends_with("1 file changed, 1 deletion(-)"));
    }

    #[tokio::test]
    async fn diff_stats_for_skips_revisions_with_no_active_diff() {
        let server = MockServer::start().await;
        // No mocks registered for diff.search/getrawdiff - a revision with no `diffPHID` must
        // never trigger either call.
        let src = MozPhabSource::for_test(cfg(&server.uri()), Some("t".into()));
        let revisions = [revision_with_diff(1, None)];

        let stats = src.diff_stats_for(revisions.iter()).await;

        assert!(stats.is_empty());
    }

    #[tokio::test]
    async fn diff_stats_for_skips_just_the_revision_whose_raw_diff_fetch_fails() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/differential.diff.search"))
            .respond_with(ResponseTemplate::new(200).set_body_json(search_response(&[
                json!({"id": 42, "phid": "PHID-DIFF-1"}),
                json!({"id": 43, "phid": "PHID-DIFF-2"}),
            ])))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/api/differential.getrawdiff"))
            .and(body_string_contains("diffID=42"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/api/differential.getrawdiff"))
            .and(body_string_contains("diffID=43"))
            .respond_with(ResponseTemplate::new(200).set_body_json(call_response(json!(
                "diff --git a/ok.rs b/ok.rs\n--- a/ok.rs\n+++ b/ok.rs\n@@ -0,0 +1,1 @@\n+ok\n"
            ))))
            .mount(&server)
            .await;

        let src = MozPhabSource::for_test(cfg(&server.uri()), Some("t".into()));
        let revisions = [
            revision_with_diff(1, Some("PHID-DIFF-1")),
            revision_with_diff(2, Some("PHID-DIFF-2")),
        ];
        let stats = src.diff_stats_for(revisions.iter()).await;

        assert!(!stats.contains_key(&1), "the failing revision should just be skipped");
        assert!(stats[&2].contains("ok.rs"));
    }

    #[tokio::test]
    async fn diff_stats_for_is_best_effort_on_conduit_failure() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/differential.diff.search"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;

        let src = MozPhabSource::for_test(cfg(&server.uri()), Some("t".into()));
        let revisions = [revision_with_diff(1, Some("PHID-DIFF-1"))];

        let stats = src.diff_stats_for(revisions.iter()).await;

        assert!(stats.is_empty(), "a Conduit failure should not panic or propagate");
    }

    #[test]
    fn parse_unified_diff_counts_added_and_removed_lines_per_file() {
        let diff = "diff --git a/a.rs b/a.rs\n\
             --- a/a.rs\n\
             +++ b/a.rs\n\
             @@ -1,2 +1,3 @@\n\
             +one\n\
             +two\n\
             -old\n\
             diff --git a/b.rs b/b.rs\n\
             --- a/b.rs\n\
             +++ b/b.rs\n\
             @@ -1,1 +1,1 @@\n\
             -bye\n";

        let changes = parse_unified_diff(diff);

        assert_eq!(changes.len(), 2);
        assert_eq!(changes[0].path, "a.rs");
        assert_eq!(changes[0].additions, 2);
        assert_eq!(changes[0].deletions, 1);
        assert_eq!(changes[1].path, "b.rs");
        assert_eq!(changes[1].additions, 0);
        assert_eq!(changes[1].deletions, 1);
    }

    #[test]
    fn parse_unified_diff_uses_the_new_path_from_the_diff_git_header() {
        let diff = "diff --git a/old-name.rs b/new-name.rs\n\
             --- a/old-name.rs\n\
             +++ b/new-name.rs\n\
             @@ -1,1 +1,1 @@\n\
             +x\n";

        let changes = parse_unified_diff(diff);

        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].path, "new-name.rs");
    }

    #[test]
    fn parse_unified_diff_on_empty_text_finds_no_files() {
        assert!(parse_unified_diff("").is_empty());
    }
}
