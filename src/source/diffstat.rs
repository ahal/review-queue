//! Renders a `git diff --stat`/`jj diff --stat`-style summary from per-file line counts, for
//! sources that report changed files via their own API (Phabricator's `differential.querydiffs`)
//! or only an aggregate count (GitHub's PR object) rather than a local git/jj diff.

/// One changed file's line counts, as reported by a source's API.
pub struct FileChange {
    pub path: String,
    pub additions: u64,
    pub deletions: u64,
}

/// Widest a single file's +/- bar is allowed to get, regardless of how large its own change
/// count is - matches `git diff --stat`'s own scaling behavior, just with a fixed cap instead of
/// scaling to the terminal width (there's no terminal column to size against here).
const MAX_BAR_WIDTH: usize = 24;

/// Cap on how many per-file lines are rendered - large PRs/revisions (generated code, vendored
/// dependency bumps) can touch hundreds of files, which would otherwise blow out the TUI's detail
/// panel; the rest are folded into a single "N more files not shown" line instead.
const MAX_FILES_SHOWN: usize = 10;

/// Render `changes` the way `git diff --stat` would: one `path | N +++---` line per file (up to
/// [`MAX_FILES_SHOWN`], widest bar capped at [`MAX_BAR_WIDTH`], `+`/`-` split proportionally to
/// that file's own additions/deletions), then a "N more files not shown" line if any were
/// omitted, followed by a `N files changed, X insertions(+), Y deletions(-)` summary line (always
/// counting every file, shown or not). Returns an empty string for no changes.
pub fn format_diffstat(changes: &[FileChange]) -> String {
    if changes.is_empty() {
        return String::new();
    }

    let shown = &changes[..changes.len().min(MAX_FILES_SHOWN)];
    let name_w = shown.iter().map(|c| c.path.chars().count()).max().unwrap_or(0);
    let max_total = shown
        .iter()
        .map(|c| c.additions + c.deletions)
        .max()
        .unwrap_or(0)
        .max(1);

    let mut total_add = 0u64;
    let mut total_del = 0u64;
    let mut lines = Vec::with_capacity(shown.len() + 2);
    for c in changes {
        total_add += c.additions;
        total_del += c.deletions;
    }
    for c in shown {
        let total = c.additions + c.deletions;

        let bar_len = ((total as f64 / max_total as f64) * MAX_BAR_WIDTH as f64).round() as usize;
        let bar_len = if total > 0 { bar_len.max(1) } else { 0 };
        let plus_len = (bar_len as u64 * c.additions).checked_div(total).unwrap_or(0) as usize;
        let minus_len = bar_len.saturating_sub(plus_len);

        lines.push(format!(
            "{:<name_w$} | {total:>4} {}{}",
            c.path,
            "+".repeat(plus_len),
            "-".repeat(minus_len),
        ));
    }
    let hidden = changes.len() - shown.len();
    if hidden > 0 {
        lines.push(format!(
            "{hidden} more file{} not shown",
            if hidden == 1 { "" } else { "s" }
        ));
    }
    lines.push(summary_line(changes.len() as u64, total_add, total_del));

    lines.join("\n")
}

/// Just the `N files changed, X insertions(+), Y deletions(-)` line, for sources that only expose
/// aggregate counts (GitHub's PR object) rather than a per-file breakdown.
pub fn format_summary(file_count: u64, additions: u64, deletions: u64) -> String {
    summary_line(file_count, additions, deletions)
}

fn summary_line(file_count: u64, total_add: u64, total_del: u64) -> String {
    let mut summary = format!(
        "{file_count} file{} changed",
        if file_count == 1 { "" } else { "s" }
    );
    if total_add > 0 {
        summary.push_str(&format!(
            ", {total_add} insertion{}(+)",
            if total_add == 1 { "" } else { "s" }
        ));
    }
    if total_del > 0 {
        summary.push_str(&format!(
            ", {total_del} deletion{}(-)",
            if total_del == 1 { "" } else { "s" }
        ));
    }
    summary
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_changes_render_nothing() {
        assert_eq!(format_diffstat(&[]), "");
    }

    #[test]
    fn single_file_matches_git_style_summary() {
        let changes = [FileChange {
            path: "src/main.rs".into(),
            additions: 3,
            deletions: 1,
        }];
        let out = format_diffstat(&changes);
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines.len(), 2);
        assert!(lines[0].starts_with("src/main.rs |    4 "));
        assert_eq!(lines[1], "1 file changed, 3 insertions(+), 1 deletion(-)");
    }

    #[test]
    fn omits_the_clause_for_a_side_with_no_changes() {
        let added_only = [FileChange {
            path: "a.rs".into(),
            additions: 2,
            deletions: 0,
        }];
        assert_eq!(
            format_diffstat(&added_only).lines().last().unwrap(),
            "1 file changed, 2 insertions(+)"
        );

        let removed_only = [FileChange {
            path: "a.rs".into(),
            additions: 0,
            deletions: 5,
        }];
        assert_eq!(
            format_diffstat(&removed_only).lines().last().unwrap(),
            "1 file changed, 5 deletions(-)"
        );
    }

    #[test]
    fn bars_scale_relative_to_the_largest_change_and_never_exceed_the_cap() {
        let changes = [
            FileChange {
                path: "big.rs".into(),
                additions: 100,
                deletions: 0,
            },
            FileChange {
                path: "small.rs".into(),
                additions: 1,
                deletions: 0,
            },
        ];
        let out = format_diffstat(&changes);
        let lines: Vec<&str> = out.lines().collect();
        let big_bar = lines[0].split_whitespace().last().unwrap();
        let small_bar = lines[1].split_whitespace().last().unwrap();
        assert_eq!(big_bar.len(), MAX_BAR_WIDTH);
        assert!(small_bar.len() < big_bar.len());
    }

    #[test]
    fn caps_file_lines_and_reports_the_rest_as_hidden() {
        let changes: Vec<FileChange> = (0..12)
            .map(|i| FileChange {
                path: format!("f{i}.rs"),
                additions: 1,
                deletions: 0,
            })
            .collect();
        let out = format_diffstat(&changes);
        let lines: Vec<&str> = out.lines().collect();

        // 10 file lines + "2 more files not shown" + the summary line.
        assert_eq!(lines.len(), 12);
        assert_eq!(lines[10], "2 more files not shown");
        assert_eq!(lines[11], "12 files changed, 12 insertions(+)");
    }

    #[test]
    fn does_not_add_a_hidden_line_when_under_the_cap() {
        let changes = [FileChange {
            path: "a.rs".into(),
            additions: 1,
            deletions: 0,
        }];
        let out = format_diffstat(&changes);
        assert!(!out.contains("not shown"));
    }

    #[test]
    fn summary_pluralizes_a_single_file_and_change() {
        let changes = [FileChange {
            path: "a.rs".into(),
            additions: 1,
            deletions: 0,
        }];
        assert_eq!(
            format_diffstat(&changes).lines().last().unwrap(),
            "1 file changed, 1 insertion(+)"
        );
    }

    #[test]
    fn format_summary_matches_format_diffstats_own_summary_line() {
        assert_eq!(
            format_summary(2, 3, 1),
            "2 files changed, 3 insertions(+), 1 deletion(-)"
        );
        assert_eq!(format_summary(1, 1, 0), "1 file changed, 1 insertion(+)");
        assert_eq!(format_summary(0, 0, 0), "0 files changed");
    }
}
