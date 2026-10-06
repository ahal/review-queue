//! Jj backend. Shells out only to the `jj` CLI (never raw `git`) plus the standalone `patch(1)`
//! tool for applying diffs - a secondary `jj workspace add`'d directory has no `.git` of its
//! own (only the canonical repo's own default workspace is colocated with git), so `git apply`
//! isn't available there. `jj commit` snapshots the working copy directly, so `patch(1)` editing
//! files on disk is enough; no index of any kind needs updating.
//!
//! - `Checkout::Ref` with a `fork`: `jj git remote add <remote_name> <remote_url>` (skipped if
//!   `jj git remote list` already has it - `add` isn't idempotent), then `jj git fetch -b
//!   <branch> --remote <remote_name>`, giving the revset `<branch>@<remote_name>`.
//! - `Checkout::Ref` with no `fork` (the PR's source fork was deleted): unsupported for now -
//!   there's no remote/branch left to track and no raw-git fallback per the above. Callers
//!   should suggest the git backend for that review instead.
//! - `Checkout::Patches`: `base` must already be reachable (fetched as part of some branch's
//!   history) - the jj backend has no by-SHA fetch to fall back on; `None` resolves to jj's own
//!   `trunk()` revset alias.
//! - `add_workspace`/`update_workspace`: `jj workspace add --name rq-<name> -r <revset> <ws>` /
//!   `jj new <revset>` (run from inside `ws`), then each patch is applied with `patch -p1` and
//!   `jj commit -m`. jj stamps a commit's author when it's *created* as an empty node, not when
//!   `jj commit` later finalizes it - so authors are pre-stamped one step ahead via `--config
//!   user.name=...`/`user.email=...` on whichever call creates the commit that will receive that
//!   patch (the initial `workspace add`/`new` for the first patch, the previous patch's `commit`
//!   for the rest). Verified with a local spike before relying on it.
//! - Every `add_workspace`/`update_workspace` call pins its resulting head under a
//!   `review-queue/<name>/<version>` bookmark (`jj bookmark create`/`set`). This isn't optional
//!   bookkeeping the way it might seem: a spike showed that without it, a force-pushed PR branch
//!   causes `jj git fetch` to abandon the old head on re-fetch (`git.abandon-unreachable-commits`
//!   defaults to `true`), breaking interdiffing. The pin keeps it reachable regardless of what
//!   the remote branch does next.
//! - `is_dirty`: `@` isn't empty, or `@-` isn't the expected id (the stack tip) or one of its
//!   ancestors.
//! - `position`: `jj new <commit>` (never `edit`, which would rewrite the stack).
//! - `remove_workspace`: `jj workspace forget`, `rm -rf`, and delete every
//!   `review-queue/<name>/*` bookmark.

use std::io::Write;
use std::path::Path;
use std::process::Command;

use anyhow::{Context, Result, bail};

use crate::source::{Checkout, Patch};
use crate::vcs::Vcs;

pub struct JjVcs;

impl JjVcs {
    fn run(&self, dir: &Path, args: &[&str]) -> Result<String> {
        let output = Command::new("jj")
            .current_dir(dir)
            .args(args)
            .output()
            .with_context(|| format!("running `jj {}` in {}", args.join(" "), dir.display()))?;
        if !output.status.success() {
            bail!(
                "`jj {}` in {} failed: {}",
                args.join(" "),
                dir.display(),
                String::from_utf8_lossy(&output.stderr).trim(),
            );
        }
        Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
    }

    fn revision_exists(&self, repo: &Path, rev: &str) -> bool {
        Command::new("jj")
            .current_dir(repo)
            .args(["log", "-r", rev, "--no-graph", "-T", ""])
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    }

    fn remote_exists(&self, repo: &Path, name: &str) -> Result<bool> {
        let out = self.run(repo, &["git", "remote", "list"])?;
        Ok(out
            .lines()
            .any(|l| l.split_whitespace().next() == Some(name)))
    }

    fn ensure_remote(&self, repo: &Path, name: &str, url: &str) -> Result<()> {
        if !self.remote_exists(repo, name)? {
            self.run(repo, &["git", "remote", "add", name, url])
                .with_context(|| format!("adding jj git remote `{name}`"))?;
        }
        Ok(())
    }

    /// Resolve a `Checkout` to a jj revset expression, fetching/registering remotes as needed.
    /// Doesn't touch the working copy - callers combine this with `workspace add -r`/`jj new`.
    fn resolve_revset(&self, repo: &Path, checkout: &Checkout) -> Result<String> {
        match checkout {
            Checkout::Ref {
                fork: Some(fork), ..
            } => {
                self.ensure_remote(repo, &fork.remote_name, &fork.remote_url)?;
                self.run(
                    repo,
                    &[
                        "git",
                        "fetch",
                        "-b",
                        &fork.branch,
                        "--remote",
                        &fork.remote_name,
                    ],
                )
                .with_context(|| {
                    format!("fetching `{}` from `{}`", fork.branch, fork.remote_name)
                })?;
                Ok(format!("{}@{}", fork.branch, fork.remote_name))
            }
            Checkout::Ref {
                commit, fork: None, ..
            } => bail!(
                "can't check out `{commit}` with the jj backend: its source fork is gone, so there's \
                 no remote/branch left to track and no raw-git fallback; use the git backend for this \
                 review instead"
            ),
            Checkout::Patches {
                base: Some(base), ..
            } => {
                if !self.revision_exists(repo, base) {
                    bail!(
                        "commit `{base}` isn't reachable in the canonical repo; the jj backend can only \
                         check out commits already fetched via a tracked branch"
                    );
                }
                Ok(base.clone())
            }
            Checkout::Patches { base: None, .. } => Ok("trunk()".to_string()),
            Checkout::ExternalCommand { .. } => Ok("trunk()".to_string()),
        }
    }

    /// Apply `diff` to the tree at `ws` with the standalone `patch(1)` tool.
    fn apply_diff(&self, ws: &Path, diff: &str) -> Result<()> {
        let mut file = tempfile::NamedTempFile::new().context("creating temp patch file")?;
        file.write_all(diff.as_bytes())
            .context("writing temp patch file")?;

        let output = Command::new("patch")
            .current_dir(ws)
            .args(["-p1", "-i"])
            .arg(file.path())
            .output()
            .context("running `patch`")?;
        if !output.status.success() {
            bail!(
                "`patch -p1` in {} failed: {}",
                ws.display(),
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
        Ok(())
    }

    /// `--config user.name=...`/`user.email=...` args that stamp `patch`'s author onto whatever
    /// commit is created by the jj invocation they're attached to.
    fn author_config_args(patch: &Patch) -> Result<Vec<String>> {
        let (name, email) = split_author(&patch.author)?;
        // Debug-formatting a &str produces a quoted, escaped literal, which is also valid TOML
        // string syntax for the common case - good enough without a real TOML writer here.
        Ok(vec![
            "--config".into(),
            format!("user.name={name:?}"),
            "--config".into(),
            format!("user.email={email:?}"),
        ])
    }

    /// Apply `patches` bottom-to-top on top of the current (empty) `@` in `ws`, leaving a fresh
    /// empty `@` on top. See the module docs for why authorship is pre-stamped one step ahead.
    fn apply_patches(&self, ws: &Path, patches: &[Patch]) -> Result<()> {
        for (i, patch) in patches.iter().enumerate() {
            self.apply_diff(ws, &patch.diff)?;

            let mut args = match patches.get(i + 1) {
                Some(next) => Self::author_config_args(next)?,
                None => Vec::new(),
            };
            args.push("commit".into());
            args.push("-m".into());
            args.push(patch.message.clone());
            let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();

            self.run(ws, &arg_refs)
                .with_context(|| format!("committing patch `{}`", patch.title))?;
        }
        Ok(())
    }

    fn patches_of(checkout: &Checkout) -> &[Patch] {
        match checkout {
            Checkout::Patches { patches, .. } => patches,
            Checkout::Ref { .. } | Checkout::ExternalCommand { .. } => &[],
        }
    }

    /// Apply whatever `checkout` needs beyond the initial `workspace add -r`/`jj new`: patches
    /// for `Patches`, an external command for `ExternalCommand`, nothing for `Ref`.
    fn apply_checkout(&self, ws: &Path, checkout: &Checkout) -> Result<()> {
        match checkout {
            Checkout::Ref { .. } => Ok(()),
            Checkout::Patches { patches, .. } => self.apply_patches(ws, patches),
            Checkout::ExternalCommand { program, args, env } => {
                self.run_external(ws, program, args, env)
            }
        }
    }

    fn run_external(
        &self,
        ws: &Path,
        program: &str,
        args: &[String],
        env: &[(String, String)],
    ) -> Result<()> {
        let output = Command::new(program)
            .args(args)
            .current_dir(ws)
            .envs(env.iter().map(|(k, v)| (k.as_str(), v.as_str())))
            .output()
            .with_context(|| format!("running `{program}`"))?;
        if !output.status.success() {
            bail!(
                "`{program} {}` in {} failed: {}\n{}",
                args.join(" "),
                ws.display(),
                crate::vcs::clean_output(&output.stderr),
                crate::vcs::clean_output(&output.stdout),
            );
        }
        Ok(())
    }

    fn head_id(&self, ws: &Path) -> Result<String> {
        self.run(ws, &["log", "-r", "@-", "--no-graph", "-T", "commit_id"])
    }

    fn pin(&self, repo: &Path, name: &str, version: &str, head: &str) -> Result<()> {
        // A stack's version can be a comma-joined list of its patches' versions, and jj parses
        // bookmark names as revset symbols, so anything outside this set is a syntax error.
        let version: String = version
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() || "._-".contains(c) { c } else { '-' })
            .collect();
        let bookmark = format!("review-queue/{name}/{version}");
        // `create` errors if the bookmark already exists (e.g. a retried sync); `set` handles
        // that case instead, so this call is idempotent either way.
        if self
            .run(repo, &["bookmark", "create", "-r", head, &bookmark])
            .is_err()
        {
            self.run(repo, &["bookmark", "set", "-r", head, &bookmark])
                .with_context(|| format!("pinning `{bookmark}`"))?;
        }
        Ok(())
    }

    fn workspace_name(name: &str) -> String {
        format!("rq-{}", name.replace('/', "-"))
    }
}

/// Split `"Name <email>"` into its parts.
fn split_author(author: &str) -> Result<(&str, &str)> {
    let (name, rest) = author
        .split_once('<')
        .with_context(|| format!("author `{author}` isn't in `Name <email>` form"))?;
    let email = rest
        .strip_suffix('>')
        .with_context(|| format!("author `{author}` isn't in `Name <email>` form"))?;
    Ok((name.trim(), email.trim()))
}

impl Vcs for JjVcs {
    fn ensure_commit(&self, repo: &Path, refspec_or_sha: &str) -> Result<()> {
        if self.revision_exists(repo, refspec_or_sha) {
            return Ok(());
        }
        // Best effort: there's no generic by-refspec fetch in jj, only tracked branches (see
        // `resolve_revset`), so all we can do is refresh whatever's already configured.
        self.run(repo, &["git", "fetch"])?;
        if !self.revision_exists(repo, refspec_or_sha) {
            bail!(
                "`{refspec_or_sha}` not found in {} after fetching",
                repo.display()
            );
        }
        Ok(())
    }

    fn add_workspace(
        &self,
        repo: &Path,
        ws: &Path,
        checkout: &Checkout,
        name: &str,
        version: &str,
    ) -> Result<String> {
        let revset = self.resolve_revset(repo, checkout)?;
        let patches = Self::patches_of(checkout);
        let ws_str = ws.to_string_lossy().to_string();

        let mut args = match patches.first() {
            Some(first) => Self::author_config_args(first)?,
            None => Vec::new(),
        };
        args.extend([
            "workspace".into(),
            "add".into(),
            "--name".into(),
            Self::workspace_name(name),
            "-r".into(),
            revset,
            ws_str,
        ]);
        let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
        self.run(repo, &arg_refs)
            .with_context(|| format!("creating jj workspace at {}", ws.display()))?;

        self.apply_checkout(ws, checkout)?;

        let head = self.head_id(ws)?;
        self.pin(repo, name, version, &head)?;
        Ok(head)
    }

    fn update_workspace(
        &self,
        repo: &Path,
        ws: &Path,
        checkout: &Checkout,
        name: &str,
        version: &str,
    ) -> Result<String> {
        let revset = self.resolve_revset(repo, checkout)?;
        let patches = Self::patches_of(checkout);

        let mut args = match patches.first() {
            Some(first) => Self::author_config_args(first)?,
            None => Vec::new(),
        };
        args.push("new".into());
        args.push(revset);
        let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
        self.run(ws, &arg_refs)
            .with_context(|| format!("updating jj workspace at {}", ws.display()))?;

        self.apply_checkout(ws, checkout)?;

        let head = self.head_id(ws)?;
        self.pin(repo, name, version, &head)?;
        Ok(head)
    }

    fn is_dirty(&self, ws: &Path, expected_head: &str) -> Result<bool> {
        let at_state = self.run(
            ws,
            &[
                "log",
                "-r",
                "@",
                "--no-graph",
                "-T",
                "if(empty, \"clean\", \"dirty\")",
            ],
        )?;
        if at_state != "clean" {
            return Ok(true);
        }
        let parent = self.run(ws, &["log", "-r", "@-", "--no-graph", "-T", "commit_id"])?;
        if parent == expected_head {
            return Ok(false);
        }
        // Positioned on an earlier patch of the stack is fine; anything else isn't.
        let in_stack = self.run(
            ws,
            &[
                "log",
                "-r",
                &format!("@- & ::{expected_head}"),
                "--no-graph",
                "-T",
                "commit_id",
            ],
        )?;
        Ok(in_stack.is_empty())
    }

    fn position(&self, ws: &Path, commit: &str) -> Result<()> {
        // `new`, not `edit`: editing a mid-stack commit would rewrite it and everything above,
        // moving the tip out from under `is_dirty`.
        self.run(ws, &["new", commit])
            .with_context(|| format!("moving to {commit} in {}", ws.display()))?;
        Ok(())
    }

    fn commits(&self, ws: &Path, tip: &str, limit: usize) -> Result<Vec<(String, String)>> {
        let raw = self.run(
            ws,
            &[
                "log",
                "-r",
                &format!("::{tip}"),
                "--limit",
                &limit.to_string(),
                "--no-graph",
                "-T",
                "commit_id ++ \"\\x1f\" ++ description ++ \"\\x1e\"",
            ],
        )?;
        Ok(super::parse_commit_records(&raw))
    }

    fn remove_workspace(&self, repo: &Path, ws: &Path, name: &str) -> Result<()> {
        let ws_name = Self::workspace_name(name);
        self.run(repo, &["workspace", "forget", &ws_name])
            .with_context(|| format!("forgetting jj workspace `{ws_name}`"))?;
        if ws.exists() {
            std::fs::remove_dir_all(ws).with_context(|| format!("removing {}", ws.display()))?;
        }

        let prefix = format!("review-queue/{name}/");
        let listing = self.run(repo, &["bookmark", "list"])?;
        for line in listing.lines() {
            if line.starts_with(char::is_whitespace) {
                continue; // indented lines are per-remote tracking info, not bookmark names
            }
            let Some((bookmark_name, _)) = line.split_once(':') else {
                continue;
            };
            if bookmark_name.starts_with(&prefix) {
                self.run(repo, &["bookmark", "delete", bookmark_name])?;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    fn jj_available() -> bool {
        Command::new("jj")
            .arg("--version")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    }

    macro_rules! require_jj {
        () => {
            if !jj_available() {
                eprintln!("skipping: `jj` not found on PATH");
                return;
            }
        };
    }

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

    fn jj_out(dir: &Path, args: &[&str]) -> String {
        let out = Command::new("jj")
            .current_dir(dir)
            .args(args)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "jj {args:?} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    fn commit_id(dir: &Path, revset: &str) -> String {
        jj_out(dir, &["log", "-r", revset, "--no-graph", "-T", "commit_id"])
    }

    fn author_of(dir: &Path, revset: &str) -> String {
        jj_out(
            dir,
            &[
                "log",
                "-r",
                revset,
                "--no-graph",
                "-T",
                "author.name() ++ \" <\" ++ author.email() ++ \">\"",
            ],
        )
    }

    /// Produce a unified diff for a single new file, applicable with `patch -p1` regardless of
    /// what's currently checked out (a real Phabricator raw diff looks like this).
    fn add_file_patch(title: &str, filename: &str, contents: &str, author: &str) -> Patch {
        let diff = format!(
            "diff --git a/{filename} b/{filename}\nnew file mode 100644\nindex 0000000..1111111\n--- /dev/null\n+++ b/{filename}\n@@ -0,0 +1,{n} @@\n{body}",
            n = contents.lines().count(),
            body = contents
                .lines()
                .map(|l| format!("+{l}\n"))
                .collect::<String>(),
        );
        Patch {
            title: title.into(),
            author: author.into(),
            message: title.into(),
            diff,
        }
    }

    /// A canonical jj repo cloned (colocated, the jj default) from a fresh upstream with one
    /// commit, plus the upstream dir so tests can add branches/forks off it.
    struct Fixture {
        _tmp: TempDir,
        upstream: std::path::PathBuf,
        canon: std::path::PathBuf,
        base: String,
    }

    fn fixture() -> Fixture {
        let tmp = TempDir::new().unwrap();
        let upstream = tmp.path().join("upstream");
        fs::create_dir(&upstream).unwrap();
        git(&upstream, &["init", "-q", "-b", "main"]);
        git(&upstream, &["config", "user.name", "test"]);
        git(&upstream, &["config", "user.email", "test@example.com"]);
        fs::write(upstream.join("README.md"), "hello\n").unwrap();
        git(&upstream, &["add", "README.md"]);
        git(&upstream, &["commit", "-q", "-m", "base"]);

        let canon = tmp.path().join("canon");
        let out = Command::new("jj")
            .current_dir(tmp.path())
            .args([
                "git",
                "clone",
                upstream.to_str().unwrap(),
                canon.to_str().unwrap(),
            ])
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "jj git clone failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );

        let base = commit_id(&canon, "trunk()");
        Fixture {
            _tmp: tmp,
            upstream,
            canon,
            base,
        }
    }

    /// Clone `upstream` into a sibling "fork" dir, add `branch` with one commit, and return its
    /// path and commit id - simulating a GitHub PR from a contributor's fork.
    fn make_fork(
        tmp: &Path,
        upstream: &Path,
        branch: &str,
        filename: &str,
        contents: &str,
    ) -> (std::path::PathBuf, String) {
        let fork = tmp.join("fork");
        git(
            tmp,
            &[
                "clone",
                "-q",
                upstream.to_str().unwrap(),
                fork.to_str().unwrap(),
            ],
        );
        git(&fork, &["checkout", "-q", "-b", branch]);
        fs::write(fork.join(filename), contents).unwrap();
        git(&fork, &["add", filename]);
        git(&fork, &["commit", "-q", "-m", "pr change"]);
        let sha = git_rev_parse(&fork, "HEAD");
        (fork, sha)
    }

    /// Add a second branch (a second PR) to an existing fork clone.
    fn add_branch_to_fork(fork: &Path, branch: &str, filename: &str, contents: &str) -> String {
        git(fork, &["checkout", "-q", "main"]);
        git(fork, &["checkout", "-q", "-b", branch]);
        fs::write(fork.join(filename), contents).unwrap();
        git(fork, &["add", filename]);
        git(fork, &["commit", "-q", "-m", "second pr change"]);
        git_rev_parse(fork, "HEAD")
    }

    #[test]
    fn second_ref_from_same_fork_owner_reuses_the_jj_remote() {
        require_jj!();
        let f = fixture();
        let (fork, sha1) = make_fork(
            f._tmp.path(),
            &f.upstream,
            "feature-1",
            "pr1.txt",
            "pr one\n",
        );
        let sha2 = add_branch_to_fork(&fork, "feature-2", "pr2.txt", "pr two\n");

        let fork_ref = |branch: &str| crate::source::ForkRef {
            remote_name: "alice".into(),
            remote_url: fork.to_string_lossy().to_string(),
            branch: branch.into(),
        };

        let vcs = JjVcs;
        let ws1 = f._tmp.path().join("ws1");
        let checkout1 = Checkout::Ref {
            refspec: "refs/pull/1/head".into(),
            commit: sha1.clone(),
            fork: Some(fork_ref("feature-1")),
        };
        let head1 = vcs
            .add_workspace(&f.canon, &ws1, &checkout1, "github/1", "v1")
            .unwrap();
        assert_eq!(head1, sha1);

        // `jj git remote add` errors on a duplicate name; this must not blow up the second time.
        let ws2 = f._tmp.path().join("ws2");
        let checkout2 = Checkout::Ref {
            refspec: "refs/pull/2/head".into(),
            commit: sha2.clone(),
            fork: Some(fork_ref("feature-2")),
        };
        let head2 = vcs
            .add_workspace(&f.canon, &ws2, &checkout2, "github/2", "v1")
            .unwrap();
        assert_eq!(head2, sha2);

        assert_eq!(
            vcs.run(&f.canon, &["git", "remote", "list"])
                .unwrap()
                .lines()
                .count(),
            2,
            "should have origin + one alice remote, not two"
        );
    }

    #[test]
    fn add_workspace_from_ref_with_fork() {
        require_jj!();
        let f = fixture();
        let (fork, sha) = make_fork(
            f._tmp.path(),
            &f.upstream,
            "feature",
            "pr.txt",
            "pr change\n",
        );

        let vcs = JjVcs;
        let ws = f._tmp.path().join("ws");
        let checkout = Checkout::Ref {
            refspec: "refs/pull/1/head".into(),
            commit: sha.clone(),
            fork: Some(crate::source::ForkRef {
                remote_name: "alice".into(),
                remote_url: fork.to_string_lossy().to_string(),
                branch: "feature".into(),
            }),
        };
        let head = vcs
            .add_workspace(&f.canon, &ws, &checkout, "github/1", "v1")
            .unwrap();

        assert_eq!(head, sha);
        assert!(ws.join("pr.txt").exists());
        assert_eq!(commit_id(&f.canon, "review-queue/github/1/v1"), sha);
    }

    #[test]
    fn add_workspace_from_ref_with_no_fork_is_an_error() {
        require_jj!();
        let f = fixture();
        let vcs = JjVcs;
        let ws = f._tmp.path().join("ws");
        let checkout = Checkout::Ref {
            refspec: "refs/pull/1/head".into(),
            commit: "deadbeef".into(),
            fork: None,
        };

        let err = vcs
            .add_workspace(&f.canon, &ws, &checkout, "github/2", "v1")
            .unwrap_err();
        assert!(
            err.to_string().contains("deadbeef"),
            "error should name the commit: {err}"
        );
    }

    #[test]
    fn add_workspace_from_patch_stack_attributes_each_author() {
        require_jj!();
        let f = fixture();
        let patch1 = add_file_patch("add a", "a.txt", "aaa\n", "Author One <one@example.com>");
        let patch2 = add_file_patch("add b", "b.txt", "bbb\n", "Author Two <two@example.com>");
        let checkout = Checkout::Patches {
            base: Some(f.base.clone()),
            patches: vec![patch1, patch2],
        };

        let vcs = JjVcs;
        let ws = f._tmp.path().join("ws");
        let head = vcs
            .add_workspace(&f.canon, &ws, &checkout, "moz/D1", "1")
            .unwrap();

        assert!(ws.join("a.txt").exists());
        assert!(ws.join("b.txt").exists());
        assert_eq!(
            commit_id(&ws, "@---"),
            f.base,
            "two patch commits should sit over base"
        );
        assert_eq!(author_of(&ws, "@--"), "Author One <one@example.com>");
        assert_eq!(author_of(&ws, "@-"), "Author Two <two@example.com>");
        assert_eq!(commit_id(&f.canon, "review-queue/moz/D1/1"), head);
    }

    #[test]
    fn position_moves_within_a_stack_without_counting_as_dirty() {
        require_jj!();
        let f = fixture();
        let checkout = Checkout::Patches {
            base: Some(f.base.clone()),
            patches: vec![
                add_file_patch("add a", "a.txt", "aaa\n", "Author One <one@example.com>"),
                add_file_patch("add b", "b.txt", "bbb\n", "Author Two <two@example.com>"),
            ],
        };
        let vcs = JjVcs;
        let ws = f._tmp.path().join("ws");
        let tip = vcs
            .add_workspace(&f.canon, &ws, &checkout, "moz/D1", "1")
            .unwrap();

        let commits = vcs.commits(&ws, &tip, 10).unwrap();
        assert_eq!(commits[0].0, tip);
        assert_eq!(commits[0].1.trim(), "add b");
        assert_eq!(commits[1].1.trim(), "add a");

        vcs.position(&ws, &commits[1].0).unwrap();
        assert!(!ws.join("b.txt").exists());
        assert!(!vcs.is_dirty(&ws, &tip).unwrap());

        vcs.position(&ws, &tip).unwrap();
        assert!(ws.join("b.txt").exists());
        assert!(!vcs.is_dirty(&ws, &tip).unwrap());
    }

    #[test]
    fn a_commit_outside_the_stack_is_dirty() {
        require_jj!();
        let f = fixture();
        let checkout = Checkout::Patches {
            base: Some(f.base.clone()),
            patches: vec![add_file_patch(
                "add a",
                "a.txt",
                "aaa\n",
                "Author <a@example.com>",
            )],
        };
        let vcs = JjVcs;
        let ws = f._tmp.path().join("ws");
        let tip = vcs
            .add_workspace(&f.canon, &ws, &checkout, "moz/D1", "1")
            .unwrap();

        // Build on top of the base instead of the stack.
        vcs.position(&ws, &f.base).unwrap();
        fs::write(ws.join("mine.txt"), "x\n").unwrap();
        assert!(vcs.is_dirty(&ws, &tip).unwrap(), "uncommitted edit");
        jj_out(&ws, &["commit", "-m", "local"]);
        assert!(vcs.is_dirty(&ws, &tip).unwrap(), "commit outside the stack");
    }

    #[test]
    fn add_workspace_from_patch_stack_with_no_base_uses_trunk() {
        require_jj!();
        let f = fixture();
        let checkout = Checkout::Patches {
            base: None,
            patches: vec![add_file_patch(
                "add a",
                "a.txt",
                "aaa\n",
                "Author <a@example.com>",
            )],
        };

        let vcs = JjVcs;
        let ws = f._tmp.path().join("ws");
        vcs.add_workspace(&f.canon, &ws, &checkout, "moz/D2", "1")
            .unwrap();

        assert_eq!(commit_id(&ws, "@--"), f.base);
    }

    #[test]
    fn add_workspace_from_patches_with_unreachable_base_is_an_error() {
        require_jj!();
        let f = fixture();
        let checkout = Checkout::Patches {
            // Not jj's root commit (all zeros) or any real commit - just an unreachable-looking id.
            base: Some("1111111111111111111111111111111111111111".into()),
            patches: vec![],
        };
        let vcs = JjVcs;
        let ws = f._tmp.path().join("ws");

        let err = vcs
            .add_workspace(&f.canon, &ws, &checkout, "moz/D3", "1")
            .unwrap_err();
        assert!(
            err.to_string().contains("isn't reachable"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn apply_failure_leaves_workspace_for_inspection() {
        require_jj!();
        let f = fixture();
        let bad = Patch {
            title: "conflict".into(),
            author: "Author <a@example.com>".into(),
            message: "conflict".into(),
            diff: "diff --git a/README.md b/README.md\n--- a/README.md\n+++ b/README.md\n@@ -1,1 +1,1 @@\n-this is not what's there\n+changed\n".into(),
        };
        let checkout = Checkout::Patches {
            base: Some(f.base.clone()),
            patches: vec![bad],
        };
        let vcs = JjVcs;
        let ws = f._tmp.path().join("ws");
        let result = vcs.add_workspace(&f.canon, &ws, &checkout, "moz/D4", "1");

        assert!(result.is_err());
        assert!(
            ws.exists(),
            "workspace should be left in place for inspection, not cleaned up"
        );
    }

    #[test]
    fn update_workspace_keeps_old_head_reachable() {
        require_jj!();
        let f = fixture();
        let vcs = JjVcs;
        let ws = f._tmp.path().join("ws");

        let checkout_v1 = Checkout::Patches {
            base: Some(f.base.clone()),
            patches: vec![add_file_patch(
                "add a",
                "a.txt",
                "aaa\n",
                "Author <a@example.com>",
            )],
        };
        let head_v1 = vcs
            .add_workspace(&f.canon, &ws, &checkout_v1, "moz/D5", "1")
            .unwrap();

        let checkout_v2 = Checkout::Patches {
            base: Some(f.base.clone()),
            patches: vec![
                add_file_patch("add a", "a.txt", "aaa\n", "Author <a@example.com>"),
                add_file_patch("add b", "b.txt", "bbb\n", "Author <a@example.com>"),
            ],
        };
        let head_v2 = vcs
            .update_workspace(&f.canon, &ws, &checkout_v2, "moz/D5", "2")
            .unwrap();

        assert_ne!(head_v1, head_v2);
        assert!(ws.join("b.txt").exists());
        // The old head is still reachable via its pinned bookmark, even though the workspace
        // moved on - this is the behavior the force-push spike showed doesn't happen for free.
        assert_eq!(commit_id(&f.canon, "review-queue/moz/D5/1"), head_v1);
        assert_eq!(commit_id(&f.canon, "review-queue/moz/D5/2"), head_v2);
    }

    #[test]
    fn is_dirty_detects_local_changes_and_head_mismatch() {
        require_jj!();
        let f = fixture();
        let vcs = JjVcs;
        let ws = f._tmp.path().join("ws");
        let checkout = Checkout::Patches {
            base: Some(f.base.clone()),
            patches: vec![],
        };
        let head = vcs
            .add_workspace(&f.canon, &ws, &checkout, "moz/D6", "1")
            .unwrap();

        assert!(!vcs.is_dirty(&ws, &head).unwrap());

        fs::write(ws.join("untracked.txt"), "oops\n").unwrap();
        assert!(vcs.is_dirty(&ws, &head).unwrap());

        fs::remove_file(ws.join("untracked.txt")).unwrap();
        assert!(!vcs.is_dirty(&ws, &head).unwrap());
        assert!(
            vcs.is_dirty(&ws, "0000000000000000000000000000000000000000")
                .unwrap()
        );
    }

    #[test]
    fn remove_workspace_cleans_up_workspace_and_bookmarks_without_touching_canon() {
        require_jj!();
        let f = fixture();
        let vcs = JjVcs;
        let ws = f._tmp.path().join("ws");
        let checkout = Checkout::Patches {
            base: Some(f.base.clone()),
            patches: vec![],
        };
        vcs.add_workspace(&f.canon, &ws, &checkout, "moz/D7", "1")
            .unwrap();

        let trunk_before = commit_id(&f.canon, "trunk()");

        vcs.remove_workspace(&f.canon, &ws, "moz/D7").unwrap();

        assert!(!ws.exists());
        let bookmarks = jj_out(&f.canon, &["bookmark", "list"]);
        assert!(
            !bookmarks.contains("review-queue/moz/D7"),
            "expected pinned bookmarks to be deleted, found: {bookmarks}"
        );
        let workspaces = jj_out(&f.canon, &["workspace", "list"]);
        assert!(
            !workspaces.contains("rq-moz-D7"),
            "expected workspace to be forgotten, found: {workspaces}"
        );
        assert_eq!(
            commit_id(&f.canon, "trunk()"),
            trunk_before,
            "canonical repo must be untouched"
        );
    }

    #[test]
    fn add_workspace_with_external_command_runs_it_with_env_and_captures_head() {
        require_jj!();
        let f = fixture();
        let vcs = JjVcs;
        let ws = f._tmp.path().join("ws");
        // A stand-in for `moz-phab patch`, which does exactly this on the jj path: modify the
        // tree, `jj describe` to finalize the commit holding those changes, then `jj new` to
        // leave a fresh empty `@` on top - the same "always ends empty" contract our own patch
        // application already follows, which is what makes `head_id` (`@-`) correct afterward.
        let checkout = Checkout::ExternalCommand {
            program: "sh".into(),
            args: vec![
                "-c".into(),
                "echo \"$RQ_TEST_VAR\" > ext.txt && jj describe -m ext --quiet && jj new --quiet"
                    .into(),
            ],
            env: vec![("RQ_TEST_VAR".into(), "hello-from-env".into())],
        };

        let head = vcs
            .add_workspace(&f.canon, &ws, &checkout, "moz/D8", "1")
            .unwrap();

        assert_eq!(
            fs::read_to_string(ws.join("ext.txt")).unwrap().trim(),
            "hello-from-env"
        );
        assert_eq!(commit_id(&ws, "@-"), head);
        assert_eq!(commit_id(&f.canon, "review-queue/moz/D8/1"), head);
    }

    #[test]
    fn external_command_failure_leaves_workspace_for_inspection() {
        require_jj!();
        let f = fixture();
        let vcs = JjVcs;
        let ws = f._tmp.path().join("ws");
        let checkout = Checkout::ExternalCommand {
            program: "sh".into(),
            args: vec!["-c".into(), "exit 7".into()],
            env: vec![],
        };

        let err = vcs
            .add_workspace(&f.canon, &ws, &checkout, "moz/D9", "1")
            .unwrap_err();

        assert!(
            ws.exists(),
            "workspace should be left in place for inspection, not cleaned up"
        );
        assert!(
            err.to_string().contains("failed"),
            "unexpected error: {err}"
        );
    }
}
