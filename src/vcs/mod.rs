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

    /// True if the workspace has local modifications, or its head isn't `expected_head` (the
    /// stack tip) or one of its ancestors - either case means `sync` must not touch it silently.
    /// Sitting on an earlier patch of the stack (see `position`) is not dirty.
    fn is_dirty(&self, ws: &Path, expected_head: &str) -> Result<bool>;

    /// Move a clean workspace to `commit`, one of the patches reachable from the stack tip. Never
    /// rewrites any of the stack's commits, so the tip stays valid for `is_dirty`.
    fn position(&self, ws: &Path, commit: &str) -> Result<()>;

    /// Up to `limit` commits reachable from `tip` (newest first) as `(commit id, message)`, so a
    /// caller can find the one carrying a particular review's patch.
    fn commits(&self, ws: &Path, tip: &str, limit: usize) -> Result<Vec<(String, String)>>;

    /// Remove the worktree/workspace and every ref/bookmark `add_workspace`/`update_workspace`
    /// pinned for it under `name`. Callers only ever call this on a workspace `is_dirty` has
    /// already confirmed is clean - there's no way to force-remove a dirty one.
    fn remove_workspace(&self, repo: &Path, ws: &Path, name: &str) -> Result<()>;
}

/// Parse `commits()` output: records separated by `\x1e`, fields by `\x1f`.
pub(crate) fn parse_commit_records(raw: &str) -> Vec<(String, String)> {
    raw.split('\x1e')
        .filter_map(|rec| {
            let (id, msg) = rec.trim_start_matches('\n').split_once('\x1f')?;
            Some((id.trim().to_string(), msg.to_string()))
        })
        .collect()
}

/// Render a child process's captured output as plain text. Progress spinners (like moz-phab's)
/// redraw in place with `\r`, `\b` and ANSI escapes, which is just garbage once captured: apply
/// the overwrites, drop escape sequences and other control characters.
pub(crate) fn clean_output(raw: &[u8]) -> String {
    let text = String::from_utf8_lossy(raw);
    let mut lines = Vec::new();
    let mut line: Vec<char> = Vec::new();
    let mut col: usize = 0;
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\n' => {
                lines.push(std::mem::take(&mut line).into_iter().collect::<String>());
                col = 0;
            }
            '\r' => col = 0,
            '\u{8}' => col = col.saturating_sub(1),
            '\u{1b}' => {
                // CSI sequence: ESC [ params final-byte (0x40..=0x7e).
                if chars.peek() == Some(&'[') {
                    chars.next();
                    for n in chars.by_ref() {
                        if ('\u{40}'..='\u{7e}').contains(&n) {
                            break;
                        }
                    }
                }
            }
            c if c.is_control() || c == '\u{fffd}' => {}
            c => {
                if col < line.len() {
                    line[col] = c;
                } else {
                    line.push(c);
                }
                col += 1;
            }
        }
    }
    lines.push(line.into_iter().collect());
    lines
        .iter()
        .map(|l| l.trim_end())
        .collect::<Vec<_>>()
        .join("\n")
        .trim()
        .to_string()
}

#[cfg(test)]
mod clean_output_tests {
    use super::clean_output;

    #[test]
    fn applies_overwrites_and_strips_escapes() {
        let raw = b"Starting up..  -\x08\\\x08|\nFetching\rDone!!!!\n\x1b[31mred\x1b[0m\n";
        assert_eq!(clean_output(raw), "Starting up..  |\nDone!!!!\nred");
    }
}
