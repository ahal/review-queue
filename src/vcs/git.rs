//! Git backend.
//!
//! - `add_workspace`: `git worktree add --detach <ws> <base|commit>`, then for
//!   `Checkout::Patches`, apply each patch bottom-to-top with `git apply --index --3way` and
//!   commit it.
//! - `update_workspace`: `git checkout --detach` onto the new base, then re-apply patches.
//!   Every `add_workspace`/`update_workspace` call pins its resulting head under
//!   `refs/review-queue/<name>/<version>` (`git update-ref`, never overwritten), so a prior
//!   version stays reachable once a later call moves the workspace on.
//! - `is_dirty`: `git status --porcelain` is non-empty, or HEAD isn't the expected id (the stack
//!   tip) or one of its ancestors.
//! - `position`: `git checkout --detach <commit>`, to sit on one patch of a stack.
//! - `remove_workspace`: `git worktree remove`, plus every `refs/review-queue/<name>/*` ref.
//!
//! A failed patch application leaves the workspace in place (the caller should record
//! `Status::ApplyFailed`) rather than being silently cleaned up.

use std::io::Write;
use std::path::Path;
use std::process::Command;

use anyhow::{Context, Result, bail};

use crate::source::{Checkout, Patch};
use crate::vcs::Vcs;

pub struct GitVcs;

impl GitVcs {
    fn run(&self, dir: &Path, args: &[&str]) -> Result<String> {
        let output = Command::new("git")
            .current_dir(dir)
            .args(args)
            .output()
            .with_context(|| format!("running `git {}` in {}", args.join(" "), dir.display()))?;
        if !output.status.success() {
            bail!(
                "`git {}` in {} failed: {}",
                args.join(" "),
                dir.display(),
                String::from_utf8_lossy(&output.stderr).trim(),
            );
        }
        Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
    }

    fn commit_exists(&self, repo: &Path, rev: &str) -> bool {
        // Expected to fail (and print to stderr) whenever the commit isn't present yet, so
        // stdio is captured rather than inherited to avoid spamming callers with normal misses.
        Command::new("git")
            .current_dir(repo)
            .args(["cat-file", "-e", &format!("{rev}^{{commit}}")])
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    }

    /// Make sure `commit` is present in `repo`'s object database, fetching it from `refspec` if
    /// not. Tries fetching `commit` directly first (GitHub allows fetching arbitrary reachable
    /// SHAs), falling back to `refspec` - an advertised ref, which every server must allow -
    /// since not every git host permits the direct-by-SHA fetch.
    fn ensure_ref_commit(&self, repo: &Path, refspec: &str, commit: &str) -> Result<()> {
        if self.commit_exists(repo, commit) {
            return Ok(());
        }
        if self.run(repo, &["fetch", "origin", commit]).is_err() {
            self.run(repo, &["fetch", "origin", refspec])
                .with_context(|| format!("fetching `{refspec}` into {}", repo.display()))?;
        }
        if !self.commit_exists(repo, commit) {
            bail!(
                "commit `{commit}` not found in {} after fetching `{refspec}`",
                repo.display()
            );
        }
        Ok(())
    }

    /// The canonical repo's default branch tip, used as the base when `Checkout::Patches` omits
    /// one. Prefers the recorded remote HEAD symref; falls back to whatever the repo currently
    /// has checked out.
    fn default_branch_commit(&self, repo: &Path) -> Result<String> {
        if let Ok(sym) = self.run(repo, &["symbolic-ref", "refs/remotes/origin/HEAD"]) {
            return self.run(repo, &["rev-parse", &sym]);
        }
        self.run(repo, &["rev-parse", "HEAD"])
    }

    /// Resolve a `Checkout::Patches` base to a commit id present locally, fetching it if a base
    /// was given but isn't present yet.
    fn resolve_base(&self, repo: &Path, base: &Option<String>) -> Result<String> {
        match base {
            Some(b) => {
                self.ensure_commit(repo, b)?;
                self.run(repo, &["rev-parse", b])
            }
            None => self.default_branch_commit(repo),
        }
    }

    fn apply_patch(&self, ws: &Path, patch: &Patch) -> Result<()> {
        let mut file = tempfile::NamedTempFile::new().context("creating temp patch file")?;
        file.write_all(patch.diff.as_bytes())
            .context("writing temp patch file")?;
        let patch_path = file.path().to_string_lossy().to_string();

        self.run(ws, &["apply", "--index", "--3way", &patch_path])
            .with_context(|| format!("applying patch `{}`", patch.title))?;

        // -c sets the committer identity so a from-scratch checkout doesn't need global git
        // config; --author preserves the patch's real author.
        self.run(
            ws,
            &[
                "-c",
                "user.name=review-queue",
                "-c",
                "user.email=review-queue@localhost",
                "commit",
                "--author",
                &patch.author,
                "-m",
                &patch.message,
            ],
        )
        .with_context(|| format!("committing patch `{}`", patch.title))?;
        Ok(())
    }

    fn apply_checkout(&self, ws: &Path, checkout: &Checkout) -> Result<()> {
        match checkout {
            Checkout::Ref { .. } => {} // nothing to apply; already at `commit`
            Checkout::Patches { patches, .. } => {
                for patch in patches {
                    self.apply_patch(ws, patch)?;
                }
            }
            Checkout::ExternalCommand { program, args, env } => {
                self.run_external(ws, program, args, env)?
            }
        }
        Ok(())
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
                String::from_utf8_lossy(&output.stderr).trim(),
                String::from_utf8_lossy(&output.stdout).trim(),
            );
        }
        Ok(())
    }

    /// The commit to start a worktree/checkout from, before `apply_checkout` runs.
    /// `ExternalCommand` gets the same "default branch tip" starting point as an unset
    /// `Patches` base - the command is expected to move the workspace wherever it actually
    /// needs to be itself.
    fn resolve_target(&self, repo: &Path, checkout: &Checkout) -> Result<String> {
        match checkout {
            Checkout::Ref {
                refspec, commit, ..
            } => {
                self.ensure_ref_commit(repo, refspec, commit)?;
                Ok(commit.clone())
            }
            Checkout::Patches { base, .. } => self.resolve_base(repo, base),
            Checkout::ExternalCommand { .. } => self.resolve_base(repo, &None),
        }
    }

    fn pin_ref(&self, repo: &Path, name: &str, version: &str, head: &str) -> Result<()> {
        self.run(
            repo,
            &[
                "update-ref",
                &format!("refs/review-queue/{name}/{version}"),
                head,
            ],
        )?;
        Ok(())
    }
}

impl Vcs for GitVcs {
    fn ensure_commit(&self, repo: &Path, refspec_or_sha: &str) -> Result<()> {
        if self.commit_exists(repo, refspec_or_sha) {
            return Ok(());
        }
        self.run(repo, &["fetch", "origin", refspec_or_sha])
            .with_context(|| format!("fetching `{refspec_or_sha}` into {}", repo.display()))?;
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
        let ws_str = ws.to_string_lossy().to_string();
        let base = self.resolve_target(repo, checkout)?;

        // A failed earlier attempt whose directory was since deleted leaves a registered-but-
        // missing worktree, which makes `worktree add` at the same path refuse forever.
        self.run(repo, &["worktree", "prune"])?;
        self.run(repo, &["worktree", "add", "--detach", &ws_str, &base])
            .with_context(|| format!("creating worktree at {}", ws.display()))?;

        self.apply_checkout(ws, checkout)?;

        let head = self.run(ws, &["rev-parse", "HEAD"])?;
        self.pin_ref(repo, name, version, &head)?;
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
        let base = self.resolve_target(repo, checkout)?;

        self.run(ws, &["checkout", "--detach", &base])
            .with_context(|| format!("checking out {base} in {}", ws.display()))?;

        self.apply_checkout(ws, checkout)?;

        let head = self.run(ws, &["rev-parse", "HEAD"])?;
        self.pin_ref(repo, name, version, &head)?;
        Ok(head)
    }

    fn is_dirty(&self, ws: &Path, expected_head: &str) -> Result<bool> {
        let status = self.run(ws, &["status", "--porcelain"])?;
        if !status.is_empty() {
            return Ok(true);
        }
        let head = self.run(ws, &["rev-parse", "HEAD"])?;
        if head == expected_head {
            return Ok(false);
        }
        // Positioned on an earlier patch of the stack is fine; anything else isn't.
        let is_ancestor = Command::new("git")
            .current_dir(ws)
            .args(["merge-base", "--is-ancestor", &head, expected_head])
            .output()
            .with_context(|| format!("running `git merge-base` in {}", ws.display()))?
            .status
            .success();
        Ok(!is_ancestor)
    }

    fn position(&self, ws: &Path, commit: &str) -> Result<()> {
        self.run(ws, &["checkout", "--detach", commit])
            .with_context(|| format!("checking out {commit} in {}", ws.display()))?;
        Ok(())
    }

    fn commits(&self, ws: &Path, tip: &str, limit: usize) -> Result<Vec<(String, String)>> {
        let raw = self.run(
            ws,
            &[
                "log",
                "-n",
                &limit.to_string(),
                "--format=%H%x1f%B%x1e",
                tip,
            ],
        )?;
        Ok(super::parse_commit_records(&raw))
    }

    fn remove_workspace(&self, repo: &Path, ws: &Path, name: &str) -> Result<()> {
        let ws_str = ws.to_string_lossy().to_string();
        self.run(repo, &["worktree", "remove", &ws_str])
            .with_context(|| format!("removing worktree at {}", ws.display()))?;

        let refs = self.run(
            repo,
            &[
                "for-each-ref",
                "--format=%(refname)",
                &format!("refs/review-queue/{name}/"),
            ],
        )?;
        for r in refs.lines().filter(|l| !l.is_empty()) {
            self.run(repo, &["update-ref", "-d", r])?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

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

    fn init_repo(dir: &Path) {
        git(dir, &["init", "-q", "-b", "main"]);
        git(dir, &["config", "user.name", "test"]);
        git(dir, &["config", "user.email", "test@example.com"]);
    }

    fn commit_file(dir: &Path, name: &str, contents: &str, message: &str) -> String {
        fs::write(dir.join(name), contents).unwrap();
        git(dir, &["add", name]);
        git(dir, &["commit", "-q", "-m", message]);
        rev_parse(dir, "HEAD")
    }

    fn rev_parse(dir: &Path, rev: &str) -> String {
        let out = Command::new("git")
            .current_dir(dir)
            .args(["rev-parse", rev])
            .output()
            .unwrap();
        assert!(out.status.success());
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    /// Produce a unified diff for a single new file, applicable with `git apply` regardless of
    /// what's currently checked out (a real Phabricator raw diff looks like this).
    fn add_file_patch(title: &str, filename: &str, contents: &str) -> Patch {
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
            author: "Patch Author <patch@example.com>".into(),
            message: title.into(),
            diff,
        }
    }

    /// A canonical repo cloned from a fresh upstream with one commit, plus the upstream dir
    /// (kept around so tests can push more refs into it and re-fetch).
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
        init_repo(&upstream);
        let base = commit_file(&upstream, "README.md", "hello\n", "base");

        let canon = tmp.path().join("canon");
        git(
            tmp.path(),
            &[
                "clone",
                "-q",
                upstream.to_str().unwrap(),
                canon.to_str().unwrap(),
            ],
        );
        git(&canon, &["config", "user.name", "test"]);
        git(&canon, &["config", "user.email", "test@example.com"]);

        Fixture {
            _tmp: tmp,
            upstream,
            canon,
            base,
        }
    }

    #[test]
    fn add_workspace_from_ref() {
        let f = fixture();
        // Simulate a GitHub PR ref: a branch in "upstream" exposed as refs/pull/1/head.
        let branch_head = commit_file(&f.upstream, "pr.txt", "pr change\n", "pr change");
        git(
            &f.upstream,
            &["update-ref", "refs/pull/1/head", &branch_head],
        );

        let vcs = GitVcs;
        let ws = f._tmp.path().join("ws");
        let checkout = Checkout::Ref {
            refspec: "refs/pull/1/head".into(),
            commit: branch_head.clone(),
            fork: None,
        };
        let head = vcs
            .add_workspace(&f.canon, &ws, &checkout, "github/123", "v1")
            .unwrap();

        assert_eq!(head, branch_head);
        assert_eq!(rev_parse(&ws, "HEAD"), branch_head);
        assert!(ws.join("pr.txt").exists());
        assert_eq!(
            rev_parse(&f.canon, "refs/review-queue/github/123/v1"),
            branch_head
        );
    }

    #[test]
    fn add_workspace_from_patch_stack() {
        let f = fixture();
        let patch1 = add_file_patch("add a", "a.txt", "aaa\n");
        let patch2 = add_file_patch("add b", "b.txt", "bbb\n");
        let checkout = Checkout::Patches {
            base: Some(f.base.clone()),
            patches: vec![patch1, patch2],
        };

        let vcs = GitVcs;
        let ws = f._tmp.path().join("ws");
        let head = vcs
            .add_workspace(&f.canon, &ws, &checkout, "moz/D1", "1")
            .unwrap();

        assert_eq!(rev_parse(&ws, "HEAD"), head);
        assert!(ws.join("a.txt").exists());
        assert!(ws.join("b.txt").exists());
        // Two new commits over base.
        assert_eq!(rev_parse(&ws, "HEAD~2"), f.base);
        assert_eq!(rev_parse(&f.canon, "refs/review-queue/moz/D1/1"), head);
    }

    #[test]
    fn position_moves_within_a_stack_without_counting_as_dirty() {
        let f = fixture();
        let checkout = Checkout::Patches {
            base: Some(f.base.clone()),
            patches: vec![
                add_file_patch("add a", "a.txt", "aaa\n"),
                add_file_patch("add b", "b.txt", "bbb\n"),
            ],
        };
        let vcs = GitVcs;
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
    }

    #[test]
    fn a_commit_outside_the_stack_is_dirty() {
        let f = fixture();
        let checkout = Checkout::Patches {
            base: Some(f.base.clone()),
            patches: vec![add_file_patch("add a", "a.txt", "aaa\n")],
        };
        let vcs = GitVcs;
        let ws = f._tmp.path().join("ws");
        let tip = vcs
            .add_workspace(&f.canon, &ws, &checkout, "moz/D1", "1")
            .unwrap();

        git(&ws, &["config", "user.name", "test"]);
        git(&ws, &["config", "user.email", "test@example.com"]);
        fs::write(ws.join("mine.txt"), "x\n").unwrap();
        git(&ws, &["add", "mine.txt"]);
        git(&ws, &["commit", "-q", "-m", "local"]);

        assert!(vcs.is_dirty(&ws, &tip).unwrap());
    }

    #[test]
    fn add_workspace_from_patch_stack_with_no_base_uses_default_branch() {
        let f = fixture();
        git(&f.upstream, &["symbolic-ref", "HEAD", "refs/heads/main"]);
        git(&f.canon, &["fetch", "-q"]);
        git(
            &f.canon,
            &[
                "symbolic-ref",
                "refs/remotes/origin/HEAD",
                "refs/remotes/origin/main",
            ],
        );

        let checkout = Checkout::Patches {
            base: None,
            patches: vec![add_file_patch("add a", "a.txt", "aaa\n")],
        };
        let vcs = GitVcs;
        let ws = f._tmp.path().join("ws");
        let head = vcs
            .add_workspace(&f.canon, &ws, &checkout, "moz/D2", "1")
            .unwrap();

        assert_eq!(rev_parse(&ws, "HEAD~1"), f.base);
        assert_eq!(rev_parse(&f.canon, "refs/review-queue/moz/D2/1"), head);
    }

    #[test]
    fn apply_failure_leaves_workspace_for_inspection() {
        let f = fixture();
        let mut bad = add_file_patch("conflict", "README.md", "will not apply\n");
        // README.md already exists at base, so a "new file" patch for it must fail to apply.
        bad.diff = bad.diff.replace("new file mode 100644\n", "");

        let checkout = Checkout::Patches {
            base: Some(f.base.clone()),
            patches: vec![bad],
        };
        let vcs = GitVcs;
        let ws = f._tmp.path().join("ws");
        let result = vcs.add_workspace(&f.canon, &ws, &checkout, "moz/D3", "1");

        assert!(result.is_err());
        assert!(
            ws.exists(),
            "workspace should be left in place for inspection, not cleaned up"
        );
    }

    #[test]
    fn update_workspace_keeps_old_head_reachable() {
        let f = fixture();
        let vcs = GitVcs;
        let ws = f._tmp.path().join("ws");

        let checkout_v1 = Checkout::Patches {
            base: Some(f.base.clone()),
            patches: vec![add_file_patch("add a", "a.txt", "aaa\n")],
        };
        let head_v1 = vcs
            .add_workspace(&f.canon, &ws, &checkout_v1, "moz/D4", "1")
            .unwrap();

        let checkout_v2 = Checkout::Patches {
            base: Some(f.base.clone()),
            patches: vec![
                add_file_patch("add a", "a.txt", "aaa\n"),
                add_file_patch("add b", "b.txt", "bbb\n"),
            ],
        };
        let head_v2 = vcs
            .update_workspace(&f.canon, &ws, &checkout_v2, "moz/D4", "2")
            .unwrap();

        assert_ne!(head_v1, head_v2);
        assert_eq!(rev_parse(&ws, "HEAD"), head_v2);
        assert!(ws.join("b.txt").exists());
        // The old head is still reachable via its pinned ref, even though the workspace moved on.
        assert_eq!(rev_parse(&f.canon, "refs/review-queue/moz/D4/1"), head_v1);
        assert_eq!(rev_parse(&f.canon, "refs/review-queue/moz/D4/2"), head_v2);
    }

    #[test]
    fn is_dirty_detects_local_changes_and_head_mismatch() {
        let f = fixture();
        let vcs = GitVcs;
        let ws = f._tmp.path().join("ws");
        let checkout = Checkout::Patches {
            base: Some(f.base.clone()),
            patches: vec![],
        };
        let head = vcs
            .add_workspace(&f.canon, &ws, &checkout, "moz/D5", "1")
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
    fn remove_workspace_cleans_up_worktree_and_refs_without_touching_canon() {
        let f = fixture();
        let vcs = GitVcs;
        let ws = f._tmp.path().join("ws");
        let checkout = Checkout::Patches {
            base: Some(f.base.clone()),
            patches: vec![],
        };
        vcs.add_workspace(&f.canon, &ws, &checkout, "moz/D6", "1")
            .unwrap();

        let canon_head_before = rev_parse(&f.canon, "HEAD");

        vcs.remove_workspace(&f.canon, &ws, "moz/D6").unwrap();

        assert!(!ws.exists());
        let refs = vcs
            .run(&f.canon, &["for-each-ref", "refs/review-queue/moz/D6/"])
            .unwrap();
        assert!(
            refs.is_empty(),
            "expected all pinned refs to be deleted, found: {refs}"
        );
        assert_eq!(
            rev_parse(&f.canon, "HEAD"),
            canon_head_before,
            "canonical repo must be untouched"
        );
    }

    #[test]
    fn add_workspace_with_external_command_runs_it_with_env_and_captures_head() {
        let f = fixture();
        let vcs = GitVcs;
        let ws = f._tmp.path().join("ws");
        let checkout = Checkout::ExternalCommand {
            program: "sh".into(),
            args: vec![
                "-c".into(),
                "echo \"$RQ_TEST_VAR\" > ext.txt && git add ext.txt && git commit -q -m ext".into(),
            ],
            env: vec![("RQ_TEST_VAR".into(), "hello-from-env".into())],
        };

        let head = vcs
            .add_workspace(&f.canon, &ws, &checkout, "moz/D7", "1")
            .unwrap();

        assert_eq!(rev_parse(&ws, "HEAD"), head);
        assert_eq!(
            fs::read_to_string(ws.join("ext.txt")).unwrap().trim(),
            "hello-from-env"
        );
        assert_eq!(rev_parse(&f.canon, "refs/review-queue/moz/D7/1"), head);
    }

    #[test]
    fn external_command_failure_leaves_workspace_for_inspection() {
        let f = fixture();
        let vcs = GitVcs;
        let ws = f._tmp.path().join("ws");
        let checkout = Checkout::ExternalCommand {
            program: "sh".into(),
            args: vec!["-c".into(), "exit 7".into()],
            env: vec![],
        };

        let err = vcs
            .add_workspace(&f.canon, &ws, &checkout, "moz/D8", "1")
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
