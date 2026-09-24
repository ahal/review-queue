//! GitHub review source.
//!
//! Auth: config `token`, then `token_cmd`, then `$GITHUB_TOKEN`, then `gh auth token`. GitHub's
//! search API requires auth to use the `@me` qualifier, so a missing token is a hard error here
//! (unlike myqonly, which runs unauthenticated and accepts the rate limits).
//!
//! Queue: `GET /search/issues?q=review-requested:@me type:pr is:open archived:false`, plus the
//! `ignore_repos`/`ignore_authors`/`ignore_teams`/`include_drafts` filters from
//! [`crate::config::GithubConfig`] folded directly into `q` (mirroring myqonly's approach of
//! letting GitHub's own search syntax do the filtering). Paginated. Each result's `html_url`
//! (`https://github.com/{owner}/{repo}/pull/{number}`) gives an unambiguous review id -
//! `{owner}/{repo}/{number}` - since owner/repo names can't themselves contain `/`. Then `GET
//! /repos/{owner}/{repo}/pulls/{number}` for `head.sha`, `head.ref`, `head.repo.clone_url`,
//! `head.repo.owner.login`, and `base.repo.clone_url`.
//!
//! Diffstat: `review_from_pull` also fetches (paginated) `GET
//! /repos/{owner}/{repo}/pulls/{number}/files` for the per-file `filename`/`additions`/
//! `deletions` breakdown - one extra request per review, paid during `rq sync` rather than by
//! `rq list`'s TUI. If that call fails (rate limit, permissions), it falls back to the
//! aggregate-only `additions`/`deletions`/`changed_files` already on the pull object fetched
//! above (present only on the single-PR fetch, not the search results) rather than showing no
//! diffstat at all.
//!
//! Checkout is `Checkout::Ref { refspec: "refs/pull/{n}/head", commit: head.sha, fork }`.
//! `refspec` is fetched from the base repo (GitHub exposes `refs/pull/N/head` there for any PR),
//! so the git backend never needs a remote for the fork. `fork` is `Some(ForkRef { remote_name:
//! head.repo.owner.login, remote_url: head.repo.clone_url, branch: head.ref })`, used only by
//! the jj backend - `remote_name` becomes `"origin"` instead when the fork URL normalizes to the
//! canonical repo's own origin (a same-repo branch PR). `fork` is `None` when `head.repo` is
//! null (source fork deleted). `checkout_spec`/`fetch_status` independently re-fetch the pull
//! from its id rather than caching data from `fetch_queue`, so they stay correct even if called
//! much later against stale state.
//!
//! `fetch_status`: `GET /repos/{owner}/{repo}/pulls/{n}` for each tracked PR. `state == "closed"`
//! (merged or not) counts as `Lifecycle::Resolved`.

use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use reqwest::Method;
use serde::Deserialize;

use crate::config::GithubConfig;
use crate::repo::normalize_url;
use crate::source::diffstat::{FileChange, format_diffstat, format_summary};
use crate::source::{
    Checkout, ForkRef, Lifecycle, RepoRef, Review, ReviewKey, ReviewKind, ReviewSource,
};

const DEFAULT_API_BASE: &str = "https://api.github.com";
const SEARCH_PER_PAGE: u32 = 100;
/// GitHub caps a PR's file list at 3000 entries across pages; 100/page keeps this well under
/// the pagination loop's own 30-page ceiling even at that cap.
const FILES_PER_PAGE: u32 = 100;

/// This source's hardcoded id-namespace prefix - see [`crate::state::ReviewKey`].
pub const NAME: &str = "gh";

pub struct GithubSource {
    cfg: GithubConfig,
    client: reqwest::Client,
    token: Option<String>,
    base_url: String,
    search_per_page: u32,
}

impl GithubSource {
    pub async fn new(cfg: GithubConfig) -> Result<Self> {
        let token = resolve_token(&cfg).await?;
        let base_url = cfg
            .api_url
            .clone()
            .unwrap_or_else(|| DEFAULT_API_BASE.to_string());
        Ok(Self {
            cfg,
            client: reqwest::Client::new(),
            token,
            base_url,
            search_per_page: SEARCH_PER_PAGE,
        })
    }

    #[cfg(test)]
    pub(crate) fn for_test(
        cfg: GithubConfig,
        token: Option<String>,
        base_url: String,
        search_per_page: u32,
    ) -> Self {
        Self {
            cfg,
            client: reqwest::Client::new(),
            token,
            base_url,
            search_per_page,
        }
    }

    fn build_query(&self) -> String {
        let mut q = String::from("review-requested:@me type:pr is:open archived:false");
        for r in &self.cfg.ignore_repos {
            q.push_str(&format!(" -repo:{r}"));
        }
        for a in &self.cfg.ignore_authors {
            q.push_str(&format!(" -author:{a}"));
        }
        for t in &self.cfg.ignore_teams {
            q.push_str(&format!(" -team-review-requested:{t}"));
        }
        if !self.cfg.include_drafts {
            q.push_str(" draft:false");
        }
        q
    }

    async fn get_json<T: serde::de::DeserializeOwned>(
        &self,
        path: &str,
        query: &[(&str, &str)],
    ) -> Result<T> {
        let url = format!("{}{path}", self.base_url);
        let mut req = self
            .client
            .request(Method::GET, url)
            .query(query)
            .header("Accept", "application/vnd.github+json")
            .header("X-GitHub-Api-Version", "2022-11-28")
            .header("User-Agent", "review-queue");
        if let Some(token) = &self.token {
            req = req.header("Authorization", format!("Bearer {token}"));
        }
        let resp = req.send().await.with_context(|| format!("GET {path}"))?;
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            bail!("GET {path} failed: {status} {body}");
        }
        resp.json::<T>()
            .await
            .with_context(|| format!("parsing response from GET {path}"))
    }

    async fn fetch_pull(&self, owner: &str, repo: &str, number: u64) -> Result<PullRequest> {
        self.get_json(&format!("/repos/{owner}/{repo}/pulls/{number}"), &[])
            .await
    }

    async fn review_from_pull(&self, owner: &str, repo: &str, pr: &PullRequest) -> Result<Review> {
        let base_repo = pr.base.repo.as_ref().context("PR has no base repo")?;
        let diff_stat = match self.file_changes_for(owner, repo, pr.number).await {
            Ok(changes) => format_diffstat(&changes),
            // Fall back to the aggregate-only counts already on `pr` (present only on the
            // single-PR fetch, not the search results) rather than showing no diffstat at all.
            Err(_) => format_summary(pr.changed_files, pr.additions, pr.deletions),
        };
        Ok(Review {
            key: ReviewKey::new(NAME, format!("{owner}/{repo}/{}", pr.number)),
            title: pr.title.clone(),
            author: pr.user.login.clone(),
            url: pr.html_url.clone(),
            repo: RepoRef {
                urls: vec![base_repo.clone_url.clone()],
                display_name: base_repo.full_name.clone(),
            },
            version: pr.head.sha.clone(),
            kind: ReviewKind::Direct,
            diff_stat: Some(diff_stat),
        })
    }

    /// Per-file `filename`/`additions`/`deletions`, paginating `GET
    /// /repos/{owner}/{repo}/pulls/{number}/files`.
    async fn file_changes_for(
        &self,
        owner: &str,
        repo: &str,
        number: u64,
    ) -> Result<Vec<FileChange>> {
        let mut changes = Vec::new();
        let mut page = 1u32;
        loop {
            let per_page_str = FILES_PER_PAGE.to_string();
            let page_str = page.to_string();
            let files: Vec<PullFile> = self
                .get_json(
                    &format!("/repos/{owner}/{repo}/pulls/{number}/files"),
                    &[("per_page", &per_page_str), ("page", &page_str)],
                )
                .await?;
            let count = files.len();
            changes.extend(files.into_iter().map(|f| FileChange {
                path: f.filename,
                additions: f.additions,
                deletions: f.deletions,
            }));
            if count < FILES_PER_PAGE as usize || page >= 30 {
                break;
            }
            page += 1;
        }
        Ok(changes)
    }

    fn fork_of(&self, pr: &PullRequest) -> Result<Option<ForkRef>> {
        let Some(head_repo) = &pr.head.repo else {
            return Ok(None);
        };
        let base_repo = pr.base.repo.as_ref().context("PR has no base repo")?;
        let remote_name =
            if normalize_url(&head_repo.clone_url) == normalize_url(&base_repo.clone_url) {
                "origin".to_string()
            } else {
                head_repo.owner.login.clone()
            };
        Ok(Some(ForkRef {
            remote_name,
            remote_url: head_repo.clone_url.clone(),
            branch: pr.head.ref_field.clone(),
        }))
    }
}

#[async_trait]
impl ReviewSource for GithubSource {
    fn name(&self) -> &str {
        NAME
    }

    async fn fetch_queue(&self) -> Result<Vec<Review>> {
        if self.token.is_none() {
            bail!(
                "GitHub source needs a token to search `@me` - configure `token`/`token_cmd`, \
                 set $GITHUB_TOKEN, or run `gh auth login`"
            );
        }

        let q = self.build_query();
        let mut reviews = Vec::new();
        let mut page = 1u32;
        loop {
            let per_page_str = self.search_per_page.to_string();
            let page_str = page.to_string();
            let resp: SearchResponse = self
                .get_json(
                    "/search/issues",
                    &[("q", &q), ("per_page", &per_page_str), ("page", &page_str)],
                )
                .await?;
            let count = resp.items.len();
            for item in &resp.items {
                let (owner, repo, number) = parse_pr_url(&item.html_url)?;
                let pr = self.fetch_pull(&owner, &repo, number).await?;
                reviews.push(self.review_from_pull(&owner, &repo, &pr).await?);
            }
            if count < self.search_per_page as usize || page >= 10 {
                break;
            }
            page += 1;
        }
        Ok(reviews)
    }

    async fn checkout_spec(
        &self,
        review: &Review,
        _canonical_repo: &std::path::Path,
    ) -> Result<Checkout> {
        let (owner, repo, number) = parse_id(&review.key.id)?;
        let pr = self.fetch_pull(&owner, &repo, number).await?;
        let fork = self.fork_of(&pr)?;
        Ok(Checkout::Ref {
            refspec: format!("refs/pull/{number}/head"),
            commit: pr.head.sha.clone(),
            fork,
        })
    }

    async fn fetch_status(&self, ids: &[String]) -> Result<Vec<(String, Lifecycle)>> {
        let mut out = Vec::with_capacity(ids.len());
        for id in ids {
            let (owner, repo, number) = parse_id(id)?;
            let pr = self.fetch_pull(&owner, &repo, number).await?;
            let lifecycle = if pr.state == "closed" {
                Lifecycle::Resolved
            } else {
                Lifecycle::Open
            };
            out.push((id.clone(), lifecycle));
        }
        Ok(out)
    }
}

/// `token`, then `token_cmd`, then `$GITHUB_TOKEN`, then `gh auth token`.
async fn resolve_token(cfg: &GithubConfig) -> Result<Option<String>> {
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
    if let Ok(t) = std::env::var("GITHUB_TOKEN")
        && !t.is_empty()
    {
        return Ok(Some(t));
    }
    if let Ok(out) = tokio::process::Command::new("gh")
        .args(["auth", "token"])
        .output()
        .await
        && out.status.success()
    {
        let token = String::from_utf8_lossy(&out.stdout).trim().to_string();
        if !token.is_empty() {
            return Ok(Some(token));
        }
    }
    Ok(None)
}

/// `{owner}/{repo}/{number}` - unambiguous since owner/repo names can't contain `/`.
fn parse_id(id: &str) -> Result<(String, String, u64)> {
    let mut parts = id.splitn(3, '/');
    let owner = parts
        .next()
        .with_context(|| format!("bad github review id `{id}`"))?;
    let repo = parts
        .next()
        .with_context(|| format!("bad github review id `{id}`"))?;
    let number: u64 = parts
        .next()
        .with_context(|| format!("bad github review id `{id}`"))?
        .parse()
        .with_context(|| format!("bad github review id `{id}` (PR number)"))?;
    Ok((owner.to_string(), repo.to_string(), number))
}

/// `https://github.com/{owner}/{repo}/pull/{number}` -> `(owner, repo, number)`.
fn parse_pr_url(html_url: &str) -> Result<(String, String, u64)> {
    let rest = html_url
        .strip_prefix("https://github.com/")
        .with_context(|| format!("unexpected PR url `{html_url}`"))?;
    let mut parts = rest.splitn(4, '/');
    let owner = parts
        .next()
        .with_context(|| format!("unexpected PR url `{html_url}`"))?;
    let repo = parts
        .next()
        .with_context(|| format!("unexpected PR url `{html_url}`"))?;
    let _pull_literal = parts.next();
    let number: u64 = parts
        .next()
        .with_context(|| format!("unexpected PR url `{html_url}`"))?
        .parse()
        .with_context(|| format!("unexpected PR url `{html_url}` (PR number)"))?;
    Ok((owner.to_string(), repo.to_string(), number))
}

#[derive(Deserialize)]
struct SearchResponse {
    items: Vec<SearchItem>,
}

#[derive(Deserialize)]
struct SearchItem {
    html_url: String,
}

#[derive(Deserialize)]
struct PullRequest {
    number: u64,
    title: String,
    html_url: String,
    state: String,
    user: GhUser,
    head: PrSide,
    base: PrSide,
    /// Only present on the single-PR fetch (`GET .../pulls/{number}`), not the search results -
    /// see `review_from_pull`.
    additions: u64,
    deletions: u64,
    changed_files: u64,
}

#[derive(Deserialize)]
struct GhUser {
    login: String,
}

#[derive(Deserialize)]
struct PrSide {
    #[serde(rename = "ref")]
    ref_field: String,
    sha: String,
    repo: Option<GhRepo>,
}

#[derive(Deserialize)]
struct GhRepo {
    clone_url: String,
    full_name: String,
    owner: GhUser,
}

#[derive(Deserialize)]
struct PullFile {
    filename: String,
    additions: u64,
    deletions: u64,
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{method, path, path_regex, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn cfg() -> GithubConfig {
        GithubConfig {
            api_url: None,
            token: Some("t".into()),
            token_cmd: None,
            ignore_repos: vec![],
            ignore_authors: vec![],
            ignore_teams: vec![],
            include_drafts: false,
        }
    }

    fn pull_json(
        owner: &str,
        repo: &str,
        number: u64,
        fork_owner: Option<&str>,
    ) -> serde_json::Value {
        let head_repo = match fork_owner {
            Some(fo) => serde_json::json!({
                "clone_url": format!("https://github.com/{fo}/{repo}.git"),
                "full_name": format!("{fo}/{repo}"),
                "owner": {"login": fo},
            }),
            None => serde_json::Value::Null,
        };
        serde_json::json!({
            "number": number,
            "title": "Fix the thing",
            "html_url": format!("https://github.com/{owner}/{repo}/pull/{number}"),
            "state": "open",
            "user": {"login": "author"},
            "head": {"ref": "feature", "sha": "deadbeef", "repo": head_repo},
            "base": {"ref": "main", "sha": "cafef00d", "repo": {
                "clone_url": format!("https://github.com/{owner}/{repo}.git"),
                "full_name": format!("{owner}/{repo}"),
                "owner": {"login": owner},
            }},
            "additions": 3,
            "deletions": 1,
            "changed_files": 2,
        })
    }

    #[tokio::test]
    async fn parses_id_round_trip() {
        assert_eq!(
            parse_id("mozilla/gecko-dev/123").unwrap(),
            ("mozilla".into(), "gecko-dev".into(), 123)
        );
        assert!(parse_id("not-enough-parts").is_err());
        assert!(parse_id("owner/repo/not-a-number").is_err());
    }

    #[tokio::test]
    async fn parses_pr_url() {
        assert_eq!(
            parse_pr_url("https://github.com/mozilla/gecko-dev/pull/123").unwrap(),
            ("mozilla".into(), "gecko-dev".into(), 123)
        );
        assert!(parse_pr_url("https://example.com/not-github").is_err());
    }

    #[tokio::test]
    async fn build_query_folds_in_config_filters() {
        let mut c = cfg();
        c.ignore_repos = vec!["mozilla/noisy".into()];
        c.ignore_authors = vec!["bot".into()];
        c.ignore_teams = vec!["mozilla/reviewers".into()];
        c.include_drafts = true;
        let src = GithubSource::for_test(
            c,
            Some("t".into()),
            DEFAULT_API_BASE.into(),
            SEARCH_PER_PAGE,
        );
        let q = src.build_query();
        assert!(q.contains("review-requested:@me"));
        assert!(q.contains("-repo:mozilla/noisy"));
        assert!(q.contains("-author:bot"));
        assert!(q.contains("-team-review-requested:mozilla/reviewers"));
        assert!(
            !q.contains("draft:false"),
            "include_drafts=true should not add draft:false"
        );
    }

    #[tokio::test]
    async fn fetch_queue_builds_review_from_search_and_pull() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/search/issues"))
            .and(query_param("page", "1"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "items": [{"html_url": "https://github.com/mozilla/gecko-dev/pull/123"}]
            })))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/repos/mozilla/gecko-dev/pulls/123"))
            .respond_with(ResponseTemplate::new(200).set_body_json(pull_json(
                "mozilla",
                "gecko-dev",
                123,
                None,
            )))
            .mount(&server)
            .await;

        let src = GithubSource::for_test(cfg(), Some("t".into()), server.uri(), SEARCH_PER_PAGE);
        let reviews = src.fetch_queue().await.unwrap();

        assert_eq!(reviews.len(), 1);
        let r = &reviews[0];
        assert_eq!(r.key, ReviewKey::new("gh", "mozilla/gecko-dev/123"));
        assert_eq!(r.title, "Fix the thing");
        assert_eq!(r.author, "author");
        assert_eq!(r.version, "deadbeef");
        assert_eq!(
            r.repo.urls,
            vec!["https://github.com/mozilla/gecko-dev.git"]
        );
        // `/files` isn't mocked in this test, so this exercises the fallback path - it happens to
        // match `pull_json`'s aggregate fields exactly, which is what
        // `diff_stat_falls_back_to_the_aggregate_when_the_files_endpoint_fails` asserts on
        // directly; `diff_stat_prefers_the_full_per_file_breakdown` covers the primary path.
        assert_eq!(
            r.diff_stat.as_deref(),
            Some("2 files changed, 3 insertions(+), 1 deletion(-)")
        );
    }

    #[tokio::test]
    async fn diff_stat_prefers_the_full_per_file_breakdown() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/repos/mozilla/gecko-dev/pulls/123/files"))
            .and(query_param("page", "1"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([
                {"filename": "src/main.rs", "additions": 5, "deletions": 2},
            ])))
            .mount(&server)
            .await;

        let src = GithubSource::for_test(cfg(), Some("t".into()), server.uri(), SEARCH_PER_PAGE);
        let review = src
            .review_from_pull(
                "mozilla",
                "gecko-dev",
                &serde_json::from_value(pull_json("mozilla", "gecko-dev", 123, None)).unwrap(),
            )
            .await
            .unwrap();

        let stat = review.diff_stat.unwrap();
        assert!(stat.contains("src/main.rs"));
        // Differs from `pull_json`'s aggregate fields (2 files/3+/1-) - proves this came from
        // `/files`, not the fallback.
        assert!(stat.ends_with("1 file changed, 5 insertions(+), 2 deletions(-)"));
    }

    #[tokio::test]
    async fn diff_stat_falls_back_to_the_aggregate_when_the_files_endpoint_fails() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/repos/mozilla/gecko-dev/pulls/123/files"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;

        let src = GithubSource::for_test(cfg(), Some("t".into()), server.uri(), SEARCH_PER_PAGE);
        let review = src
            .review_from_pull(
                "mozilla",
                "gecko-dev",
                &serde_json::from_value(pull_json("mozilla", "gecko-dev", 123, None)).unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(
            review.diff_stat.as_deref(),
            Some("2 files changed, 3 insertions(+), 1 deletion(-)")
        );
    }

    #[tokio::test]
    async fn fetch_queue_paginates() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/search/issues"))
            .and(query_param("page", "1"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "items": [{"html_url": "https://github.com/mozilla/gecko-dev/pull/1"}]
            })))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/search/issues"))
            .and(query_param("page", "2"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "items": [{"html_url": "https://github.com/mozilla/gecko-dev/pull/2"}]
            })))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/search/issues"))
            .and(query_param("page", "3"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(serde_json::json!({"items": []})),
            )
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path_regex(r"^/repos/mozilla/gecko-dev/pulls/\d+$"))
            .respond_with(|req: &wiremock::Request| {
                let number: u64 = req
                    .url
                    .path_segments()
                    .unwrap()
                    .next_back()
                    .unwrap()
                    .parse()
                    .unwrap();
                ResponseTemplate::new(200).set_body_json(pull_json(
                    "mozilla",
                    "gecko-dev",
                    number,
                    None,
                ))
            })
            .mount(&server)
            .await;

        // per_page=1 so each mocked page has exactly one item, forcing the loop to page 3.
        let src = GithubSource::for_test(cfg(), Some("t".into()), server.uri(), 1);
        let reviews = src.fetch_queue().await.unwrap();
        assert_eq!(reviews.len(), 2);
    }

    #[tokio::test]
    async fn checkout_spec_names_fork_remote_after_owner() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/repos/mozilla/gecko-dev/pulls/123"))
            .respond_with(ResponseTemplate::new(200).set_body_json(pull_json(
                "mozilla",
                "gecko-dev",
                123,
                Some("alice"),
            )))
            .mount(&server)
            .await;

        let src = GithubSource::for_test(cfg(), Some("t".into()), server.uri(), SEARCH_PER_PAGE);
        let review = Review {
            key: ReviewKey::new("gh", "mozilla/gecko-dev/123"),
            title: "x".into(),
            author: "author".into(),
            url: "https://github.com/mozilla/gecko-dev/pull/123".into(),
            repo: RepoRef {
                urls: vec![],
                display_name: "mozilla/gecko-dev".into(),
            },
            version: "deadbeef".into(),
            kind: ReviewKind::Direct,
            diff_stat: None,
        };
        let checkout = src
            .checkout_spec(&review, std::path::Path::new("/tmp/unused"))
            .await
            .unwrap();
        match checkout {
            Checkout::Ref {
                refspec,
                commit,
                fork,
            } => {
                assert_eq!(refspec, "refs/pull/123/head");
                assert_eq!(commit, "deadbeef");
                let fork = fork.unwrap();
                assert_eq!(fork.remote_name, "alice");
                assert_eq!(fork.branch, "feature");
            }
            _ => panic!("expected a Ref checkout"),
        }
    }

    #[tokio::test]
    async fn checkout_spec_uses_origin_for_same_repo_branch_pr() {
        let server = MockServer::start().await;
        // fork_owner == base owner: a branch-on-the-same-repo PR, not an actual fork.
        Mock::given(method("GET"))
            .and(path("/repos/mozilla/gecko-dev/pulls/123"))
            .respond_with(ResponseTemplate::new(200).set_body_json(pull_json(
                "mozilla",
                "gecko-dev",
                123,
                Some("mozilla"),
            )))
            .mount(&server)
            .await;

        let src = GithubSource::for_test(cfg(), Some("t".into()), server.uri(), SEARCH_PER_PAGE);
        let review = Review {
            key: ReviewKey::new("gh", "mozilla/gecko-dev/123"),
            title: "x".into(),
            author: "author".into(),
            url: "https://github.com/mozilla/gecko-dev/pull/123".into(),
            repo: RepoRef {
                urls: vec![],
                display_name: "mozilla/gecko-dev".into(),
            },
            version: "deadbeef".into(),
            kind: ReviewKind::Direct,
            diff_stat: None,
        };
        let Checkout::Ref { fork, .. } = src
            .checkout_spec(&review, std::path::Path::new("/tmp/unused"))
            .await
            .unwrap()
        else {
            panic!("expected Ref")
        };
        assert_eq!(fork.unwrap().remote_name, "origin");
    }

    #[tokio::test]
    async fn checkout_spec_handles_deleted_fork() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/repos/mozilla/gecko-dev/pulls/123"))
            .respond_with(ResponseTemplate::new(200).set_body_json(pull_json(
                "mozilla",
                "gecko-dev",
                123,
                None,
            )))
            .mount(&server)
            .await;

        let src = GithubSource::for_test(cfg(), Some("t".into()), server.uri(), SEARCH_PER_PAGE);
        let review = Review {
            key: ReviewKey::new("gh", "mozilla/gecko-dev/123"),
            title: "x".into(),
            author: "author".into(),
            url: "https://github.com/mozilla/gecko-dev/pull/123".into(),
            repo: RepoRef {
                urls: vec![],
                display_name: "mozilla/gecko-dev".into(),
            },
            version: "deadbeef".into(),
            kind: ReviewKind::Direct,
            diff_stat: None,
        };
        let Checkout::Ref { fork, .. } = src
            .checkout_spec(&review, std::path::Path::new("/tmp/unused"))
            .await
            .unwrap()
        else {
            panic!("expected Ref")
        };
        assert!(fork.is_none());
    }

    #[tokio::test]
    async fn fetch_status_maps_closed_to_resolved() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/repos/mozilla/gecko-dev/pulls/1"))
            .respond_with(ResponseTemplate::new(200).set_body_json(pull_json(
                "mozilla",
                "gecko-dev",
                1,
                None,
            )))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/repos/mozilla/gecko-dev/pulls/2"))
            .respond_with(ResponseTemplate::new(200).set_body_json({
                let mut v = pull_json("mozilla", "gecko-dev", 2, None);
                v["state"] = serde_json::json!("closed");
                v
            }))
            .mount(&server)
            .await;

        let src = GithubSource::for_test(cfg(), Some("t".into()), server.uri(), SEARCH_PER_PAGE);
        let ids = vec![
            "mozilla/gecko-dev/1".to_string(),
            "mozilla/gecko-dev/2".to_string(),
        ];
        let statuses = src.fetch_status(&ids).await.unwrap();

        assert_eq!(
            statuses[0],
            ("mozilla/gecko-dev/1".to_string(), Lifecycle::Open)
        );
        assert_eq!(
            statuses[1],
            ("mozilla/gecko-dev/2".to_string(), Lifecycle::Resolved)
        );
    }

    #[tokio::test]
    async fn fetch_queue_without_token_is_an_error() {
        let src = GithubSource::for_test(cfg(), None, DEFAULT_API_BASE.into(), SEARCH_PER_PAGE);
        let err = src.fetch_queue().await.unwrap_err();
        assert!(
            err.to_string().contains("needs a token"),
            "unexpected error: {err}"
        );
    }

}
