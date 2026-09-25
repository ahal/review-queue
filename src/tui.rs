//! `rq show`'s interactive TUI: a `cursive` (crossterm backend) `SelectView` of tracked reviews.
//! Up/down and `j`/`k` move the cursor between *reviews*, skipping over an expanded review's
//! diffstat lines rather than stepping into them (see `move_selection`) - arrow keys are
//! intercepted via an `OnEventView` since `SelectView`'s own built-in handling would otherwise
//! land on those lines like any other row. Left/right (and `h`/`l`) expand/collapse the selected
//! review's diffstat (fetched by `rq sync` and read straight out of `state.json` - no network
//! calls here). `o` opens the review in a browser. Enter opens it locally: fetching it on demand
//! if needed (the same `sync::fetch_local` a plain `rq fetch <id>` runs), then suspending the TUI
//! to drop the user into a subshell in its worktree, resuming once they exit it - see
//! `run_event_loop` for why that means tearing down and recreating the whole backend rather than
//! just toggling raw mode. See `crate::sync`'s module docs for why `rq sync` itself never creates
//! workspaces.

use std::collections::BTreeSet;
use std::path::PathBuf;

use anyhow::Result;
use cursive::event::Key;
use cursive::traits::*;
use cursive::views::{Dialog, LinearLayout, OnEventView, SelectView, TextView};
use cursive::Cursive;

use crate::config::{self, Config};
use crate::paths::Paths;
use crate::repo::{NeedsClone, OnMissing};
use crate::source::ReviewSource;
use crate::state::{ReviewEntry, ReviewKey, State};
use crate::sync;

const HELP: &str =
    "↑/↓ move   ←/→ expand/collapse   enter open locally   o open in browser   r reload   q quit";

struct Ctx {
    paths: Paths,
    config: Config,
    config_path: PathBuf,
    sources: Vec<Box<dyn ReviewSource>>,
    handle: tokio::runtime::Handle,
    all: bool,
    key_w: usize,
    author_w: usize,
    /// Reviews currently expanded to show their diffstat - any number at once, independently.
    expanded: BTreeSet<ReviewKey>,
    /// Set by `do_open_locally` on a successful fetch; drained by `run_event_loop`, which is the
    /// only place actually allowed to touch the terminal/backend to suspend into a subshell.
    pending_shell: Option<(ReviewKey, PathBuf)>,
}

/// A row in the `reviews` `SelectView`: either a review itself, or one of the diffstat lines
/// shown underneath it while expanded. Both carry the owning review's key so opening/collapsing
/// act on the right review regardless of which line the cursor happens to sit on.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Row {
    Entry(ReviewKey),
    Detail(ReviewKey),
}

impl Row {
    fn key(&self) -> &ReviewKey {
        match self {
            Row::Entry(k) | Row::Detail(k) => k,
        }
    }
}

/// Run the TUI until the user quits. Blocks the current thread; call from within a Tokio runtime
/// (needed for the open-locally hotkey, which drives `sync::fetch_local` to completion
/// synchronously).
pub fn run(
    paths: Paths,
    config: Config,
    config_path: PathBuf,
    sources: Vec<Box<dyn ReviewSource>>,
    all: bool,
) -> Result<()> {
    let entries = load_rows(&paths, all)?;
    if entries.is_empty() {
        println!("No reviews tracked yet. Run `rq sync` first.");
        return Ok(());
    }
    let (key_w, author_w) = column_widths(&entries);

    let mut siv = Cursive::new();
    siv.set_user_data(Ctx {
        paths,
        config,
        config_path,
        sources,
        handle: tokio::runtime::Handle::current(),
        all,
        key_w,
        author_w,
        expanded: BTreeSet::new(),
        pending_shell: None,
    });

    let mut select = SelectView::<Row>::new();
    for (label, row) in build_rows(&entries, key_w, author_w, &BTreeSet::new()) {
        select.add_item(label, row);
    }
    select.set_on_submit(|s, row: &Row| open_locally_selected_key(s, row.key().clone()));

    // `OnEventView` intercepts the arrow keys before `SelectView`'s own built-in handling sees
    // them, so they go through `move_selection`/`set_expanded` too instead of stopping on a
    // diffstat line or falling through to `SelectView`'s own (unwanted) left/right handling.
    let select = OnEventView::new(select.with_name("reviews"))
        .on_event(Key::Down, |s| move_selection(s, true))
        .on_event(Key::Up, |s| move_selection(s, false))
        .on_event(Key::Right, |s| set_expanded(s, true))
        .on_event(Key::Left, |s| set_expanded(s, false));

    let layout = LinearLayout::vertical()
        .child(TextView::new(header_line(key_w, author_w)))
        .child(select.scrollable().full_height())
        .child(TextView::new("").with_name("status"))
        .child(TextView::new(HELP));
    siv.add_fullscreen_layer(layout);

    siv.add_global_callback('q', |s| s.quit());
    siv.add_global_callback(Key::Esc, |s| s.quit());
    siv.add_global_callback('o', open_in_browser_selected);
    siv.add_global_callback('r', reload);
    siv.add_global_callback('j', |s| move_selection(s, true));
    siv.add_global_callback('k', |s| move_selection(s, false));
    siv.add_global_callback('l', |s| set_expanded(s, true));
    siv.add_global_callback('h', |s| set_expanded(s, false));

    run_event_loop(siv)
}

/// Drives the event loop by hand instead of the usual `siv.run()`, so an open-locally request can
/// tear the whole backend down before dropping the user into a subshell, then build a fresh one
/// on return. `Cursive`'s screen diffing has no way to know the subshell scribbled all over the
/// terminal, so patching the existing backend back to raw/alternate-screen mode leaves stale
/// content behind wherever the next frame doesn't happen to differ from the last one drawn before
/// suspending. Recreating the backend (and with it, a `CursiveRunner` with a blank diff buffer)
/// sidesteps that entirely - the next `refresh()` is indistinguishable from a fresh start, so it
/// draws every cell instead of only the ones it thinks changed.
fn run_event_loop(mut siv: Cursive) -> Result<()> {
    enum Outcome {
        Quit,
        OpenShell(ReviewKey, PathBuf),
    }

    loop {
        let backend = cursive::backends::crossterm::Backend::init()?;
        let outcome = {
            let mut runner = siv.runner(backend);
            runner.refresh();
            loop {
                runner.step();
                if let Some((key, path)) =
                    runner.user_data::<Ctx>().and_then(|ctx| ctx.pending_shell.take())
                {
                    break Outcome::OpenShell(key, path);
                }
                if !runner.is_running() {
                    break Outcome::Quit;
                }
            }
            // `runner` drops here, tearing the backend fully down (leaves the alternate screen,
            // disables raw mode, shows the cursor) before we touch the terminal for anything else.
        };

        match outcome {
            Outcome::Quit => return Ok(()),
            Outcome::OpenShell(key, path) => {
                let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_string());
                let result = std::process::Command::new(&shell).current_dir(&path).status();
                match result {
                    Ok(status) if status.success() => {
                        set_status(&mut siv, format!("back from {key} ({})", path.display()))
                    }
                    Ok(status) => set_status(
                        &mut siv,
                        format!("`{shell}` exited with {status} in {}", path.display()),
                    ),
                    Err(e) => set_status(&mut siv, format!("failed to launch `{shell}`: {e}")),
                }
            }
        }
    }
}

fn load_rows(paths: &Paths, all: bool) -> Result<Vec<ReviewEntry>> {
    let state = State::load(&paths.state_file())?;
    Ok(state
        .iter()
        .filter(|e| all || e.in_queue)
        .cloned()
        .collect())
}

fn column_widths(entries: &[ReviewEntry]) -> (usize, usize) {
    let key_w = entries
        .iter()
        .map(|e| e.key.slug().len())
        .max()
        .unwrap_or(3)
        .max(3);
    let author_w = entries
        .iter()
        .map(|e| e.author.chars().count())
        .max()
        .unwrap_or(6)
        .clamp(6, 20);
    (key_w, author_w)
}

fn truncate(s: &str, width: usize) -> String {
    if s.chars().count() <= width {
        format!("{s:<width$}")
    } else {
        let head: String = s.chars().take(width.saturating_sub(1)).collect();
        format!("{head}\u{2026}")
    }
}

fn header_line(key_w: usize, author_w: usize) -> String {
    format!(
        "  {:<key_w$}  {:<3}  {:<9}  {:<author_w$}  TITLE",
        "KEY", "GOT", "STATUS", "AUTHOR"
    )
}

fn row_label(e: &ReviewEntry, key_w: usize, author_w: usize, expanded: bool) -> String {
    let marker = if expanded { '\u{25be}' } else { '\u{25b8}' };
    let fetched = if e.workspace.is_some() { "yes" } else { "no" };
    let status = match &e.workspace {
        Some(ws) => format!("{:?}", ws.status),
        None if e.resolved => "resolved".to_string(),
        None if e.in_queue => "queued".to_string(),
        None => "waiting".to_string(),
    };
    format!(
        "{marker} {:<key_w$}  {fetched:<3}  {status:<9}  {}  {}",
        e.key.slug(),
        truncate(&e.author, author_w),
        e.title,
    )
}

/// The diffstat lines shown under an expanded review - `rq sync` fetches this from the review's
/// source and stores it on the `ReviewEntry`, so this is a plain read with no network call.
fn detail_lines(e: &ReviewEntry) -> Vec<String> {
    match &e.diff_stat {
        None => vec!["      (no diffstat - run `rq sync`)".to_string()],
        Some(stat) if stat.trim().is_empty() => vec!["      (no changes)".to_string()],
        Some(stat) => stat.lines().map(|l| format!("      {l}")).collect(),
    }
}

fn build_rows(
    entries: &[ReviewEntry],
    key_w: usize,
    author_w: usize,
    expanded: &BTreeSet<ReviewKey>,
) -> Vec<(String, Row)> {
    let mut rows = Vec::new();
    for e in entries {
        let is_expanded = expanded.contains(&e.key);
        rows.push((
            row_label(e, key_w, author_w, is_expanded),
            Row::Entry(e.key.clone()),
        ));
        if is_expanded {
            for line in detail_lines(e) {
                rows.push((line, Row::Detail(e.key.clone())));
            }
        }
    }
    rows
}

fn set_status(s: &mut Cursive, msg: impl Into<String>) {
    s.call_on_name("status", |v: &mut TextView| v.set_content(msg.into()));
}

/// Moves to the next/previous review, skipping over any diffstat detail lines in between so the
/// cursor only ever lands on a `Row::Entry` - backs both the arrow keys and `j`/`k`.
fn move_selection(s: &mut Cursive, down: bool) {
    s.call_on_name("reviews", |v: &mut SelectView<Row>| {
        loop {
            let before = v.selected_id();
            let _ = if down { v.select_down(1) } else { v.select_up(1) };
            if v.selected_id() == before {
                break; // hit the top/bottom of the list; nowhere left to go
            }
            if !matches!(v.selection().as_deref(), Some(Row::Detail(_))) {
                break;
            }
        }
    });
}

fn selected_key(s: &mut Cursive) -> Option<ReviewKey> {
    s.call_on_name("reviews", |v: &mut SelectView<Row>| v.selection())
        .flatten()
        .map(|row| row.key().clone())
}

/// `set_expanded(s, true/false)` backs left/right and the `l`/`h` vim-style bindings - directional
/// rather than a toggle, so repeating one is idempotent instead of flipping back and forth.
fn set_expanded(s: &mut Cursive, expand: bool) {
    let Some(key) = selected_key(s) else {
        return;
    };
    if let Some(ctx) = s.user_data::<Ctx>() {
        if expand {
            ctx.expanded.insert(key);
        } else {
            ctx.expanded.remove(&key);
        }
    }
    reload(s);
}

fn open_in_browser_selected(s: &mut Cursive) {
    let Some(key) = selected_key(s) else {
        return;
    };
    open_in_browser(s, &key);
}

fn open_in_browser(s: &mut Cursive, key: &ReviewKey) {
    let url = s.user_data::<Ctx>().and_then(|ctx| {
        State::load(&ctx.paths.state_file())
            .ok()
            .and_then(|st| st.get(key).map(|e| e.url.clone()))
    });
    match url {
        Some(url) => match open::that(&url) {
            Ok(()) => set_status(s, format!("opened {url}")),
            Err(e) => set_status(s, format!("failed to open {url}: {e}")),
        },
        None => set_status(s, format!("`{key}` is no longer tracked")),
    }
}

fn open_locally_selected_key(s: &mut Cursive, key: ReviewKey) {
    let on_missing = s
        .user_data::<Ctx>()
        .map(|ctx| {
            if ctx.config.auto_clone {
                OnMissing::Clone
            } else {
                OnMissing::Ask
            }
        })
        .unwrap_or(OnMissing::Ask);
    do_open_locally(s, key, on_missing);
}

fn do_open_locally(s: &mut Cursive, key: ReviewKey, on_missing: OnMissing) {
    let outcome = s.user_data::<Ctx>().map(|ctx| {
        tokio::task::block_in_place(|| {
            ctx.handle.clone().block_on(sync::fetch_local(
                &ctx.sources,
                &ctx.paths,
                &ctx.config,
                &key,
                on_missing,
            ))
        })
    });

    match outcome {
        // Either way `fetch_local` may have written a workspace to state.json (even a failed
        // apply is recorded, left in place for inspection) - reload to reflect that. The actual
        // subshell only gets launched by `run_event_loop`, which alone is allowed to tear down
        // the backend.
        Some(Ok(path)) => {
            if let Some(ctx) = s.user_data::<Ctx>() {
                ctx.pending_shell = Some((key, path));
            }
            reload(s);
        }
        Some(Err(e)) => match e.downcast::<NeedsClone>() {
            Ok(needs_clone) => prompt_clone(s, key, needs_clone.url, needs_clone.dest),
            Err(e) => {
                set_status(s, format!("error fetching {key}: {e:#}"));
                reload(s);
            }
        },
        None => {}
    }
}

/// Raw stdin can't be read while cursive owns the screen, so the CLI's `Y/n/always` prompt
/// becomes a dialog here instead.
fn prompt_clone(s: &mut Cursive, key: ReviewKey, url: String, dest: PathBuf) {
    let yes_key = key.clone();
    let always_key = key.clone();
    let dialog = Dialog::text(format!(
        "No local checkout of `{url}` found.\nClone into {}?",
        dest.display()
    ))
    .title("Clone repo?")
    .button("No", |s| {
        s.pop_layer();
        set_status(s, "skipped - not cloned");
    })
    .button("Yes", move |s| {
        s.pop_layer();
        do_open_locally(s, yes_key.clone(), OnMissing::Clone);
    })
    .button("Always", move |s| {
        s.pop_layer();
        let saved = s.user_data::<Ctx>().map(|ctx| {
            let result = config::set_auto_clone(&ctx.config_path);
            if result.is_ok() {
                ctx.config.auto_clone = true;
            }
            result
        });
        if let Some(Err(e)) = saved {
            set_status(s, format!("failed to save auto_clone: {e:#}"));
            return;
        }
        do_open_locally(s, always_key.clone(), OnMissing::Clone);
    });
    s.add_layer(dialog);
}

fn reload(s: &mut Cursive) {
    let loaded = s.user_data::<Ctx>().map(|ctx| {
        (
            load_rows(&ctx.paths, ctx.all),
            ctx.key_w,
            ctx.author_w,
            ctx.expanded.clone(),
        )
    });
    let Some((loaded, key_w, author_w, expanded)) = loaded else {
        return;
    };
    let entries = match loaded {
        Ok(entries) => entries,
        Err(e) => {
            set_status(s, format!("reload failed: {e:#}"));
            return;
        }
    };

    s.call_on_name("reviews", |v: &mut SelectView<Row>| {
        let selected = v.selection().map(|row| row.key().clone());
        v.clear();
        for (label, row) in build_rows(&entries, key_w, author_w, &expanded) {
            v.add_item(label, row);
        }
        if let Some(selected) = selected
            && let Some(idx) = (0..v.len())
                .find(|&i| v.get_item(i).is_some_and(|(_, row)| *row.key() == selected))
        {
            v.set_selection(idx);
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::source::{RepoRef, ReviewKind};

    fn entry(id: &str, diff_stat: Option<&str>) -> ReviewEntry {
        ReviewEntry {
            key: ReviewKey::new("moz", id),
            title: "Fix the thing".into(),
            author: "someone".into(),
            url: format!("https://example.com/{id}"),
            repo: RepoRef {
                urls: vec!["https://example.com/o/r".into()],
                display_name: "o/r".into(),
            },
            kind: ReviewKind::Direct,
            version: "1".into(),
            in_queue: true,
            resolved: false,
            last_synced: chrono::Utc::now(),
            workspace: None,
            diff_stat: diff_stat.map(String::from),
        }
    }

    #[test]
    fn collapsed_entries_produce_one_row_each() {
        let entries = vec![entry("D1", None), entry("D2", None)];
        let rows = build_rows(&entries, 5, 6, &BTreeSet::new());
        assert_eq!(rows.len(), 2);
        assert!(matches!(&rows[0].1, Row::Entry(k) if k.id == "D1"));
        assert!(matches!(&rows[1].1, Row::Entry(k) if k.id == "D2"));
    }

    #[test]
    fn expanding_a_review_inserts_detail_rows_owned_by_its_key() {
        let entries = vec![
            entry("D1", Some("a.rs | 1 +\nb.rs | 2 ++")),
            entry("D2", None),
        ];
        let expanded = ReviewKey::new("moz", "D1");
        let rows = build_rows(&entries, 5, 6, &BTreeSet::from([expanded.clone()]));

        // D1's entry row, its two detail lines, then D2's entry row.
        assert_eq!(rows.len(), 4);
        assert!(matches!(&rows[0].1, Row::Entry(k) if *k == expanded));
        for (_, row) in &rows[1..3] {
            assert_eq!(row, &Row::Detail(expanded.clone()));
        }
        assert!(matches!(&rows[3].1, Row::Entry(k) if k.id == "D2"));
    }

    #[test]
    fn multiple_reviews_can_be_expanded_at_once() {
        let entries = vec![
            entry("D1", Some("a.rs | 1 +")),
            entry("D2", Some("b.rs | 2 ++")),
        ];
        let d1 = ReviewKey::new("moz", "D1");
        let d2 = ReviewKey::new("moz", "D2");
        let rows = build_rows(&entries, 5, 6, &BTreeSet::from([d1.clone(), d2.clone()]));

        // D1's entry + its detail line, then D2's entry + its detail line - expanding D2 must
        // not have collapsed D1.
        assert_eq!(rows.len(), 4);
        assert!(matches!(&rows[0].1, Row::Entry(k) if *k == d1));
        assert_eq!(rows[1].1, Row::Detail(d1));
        assert!(matches!(&rows[2].1, Row::Entry(k) if *k == d2));
        assert_eq!(rows[3].1, Row::Detail(d2));
    }

    #[tokio::test]
    async fn set_expanded_can_expand_multiple_reviews_independently() {
        let tmp = tempfile::tempdir().unwrap();
        let mut siv = cursive::dummy();
        siv.set_user_data(ctx_with_state(
            tmp.path(),
            vec![entry("D1", None), entry("D2", None)],
        ));
        let mut select = SelectView::<Row>::new();
        select.add_item("D1", Row::Entry(ReviewKey::new("moz", "D1")));
        select.add_item("D2", Row::Entry(ReviewKey::new("moz", "D2")));
        siv.add_layer(select.with_name("reviews"));

        // Expand D1, then move to D2 (skipping over D1's now-visible detail line, same as any
        // other navigation) and expand it too - must not collapse D1.
        set_expanded(&mut siv, true);
        move_selection(&mut siv, true);
        set_expanded(&mut siv, true);

        let ctx = siv.user_data::<Ctx>().unwrap();
        assert_eq!(
            ctx.expanded,
            BTreeSet::from([ReviewKey::new("moz", "D1"), ReviewKey::new("moz", "D2")]),
            "expanding D2 must not collapse the already-expanded D1"
        );

        // Collapse D1 - must not touch D2.
        move_selection(&mut siv, false);
        set_expanded(&mut siv, false);

        let ctx = siv.user_data::<Ctx>().unwrap();
        assert_eq!(
            ctx.expanded,
            BTreeSet::from([ReviewKey::new("moz", "D2")]),
            "collapsing D1 must not touch D2"
        );
    }

    #[test]
    fn detail_lines_with_no_diff_stat_say_to_run_sync() {
        let e = entry("D1", None);
        let lines = detail_lines(&e);
        assert_eq!(lines.len(), 1);
        assert!(lines[0].contains("run `rq sync`"));
    }

    #[test]
    fn detail_lines_with_an_empty_diff_stat_say_no_changes() {
        let e = entry("D1", Some(""));
        let lines = detail_lines(&e);
        assert_eq!(lines.len(), 1);
        assert!(lines[0].contains("no changes"));
    }

    #[test]
    fn detail_lines_render_each_line_of_the_stored_diff_stat() {
        let e = entry(
            "D1",
            Some("a.rs | 1 +\nb.rs | 2 ++\n2 files changed, 3 insertions(+)"),
        );
        let lines = detail_lines(&e);
        assert_eq!(lines.len(), 3);
        assert!(lines[0].contains("a.rs | 1 +"));
        assert!(lines[1].contains("b.rs | 2 ++"));
        assert!(lines[2].contains("2 files changed"));
    }

    fn selected_row(siv: &mut Cursive) -> Row {
        (*siv
            .call_on_name("reviews", |v: &mut SelectView<Row>| v.selection())
            .flatten()
            .unwrap())
        .clone()
    }

    #[test]
    fn move_selection_skips_detail_rows_in_both_directions() {
        let mut siv = cursive::dummy();
        let entries = vec![
            entry("D1", Some("a.rs | 1 +\nb.rs | 2 ++")),
            entry("D2", None),
        ];
        let mut select = SelectView::<Row>::new();
        let expanded = ReviewKey::new("moz", "D1");
        for (label, row) in build_rows(&entries, 5, 6, &BTreeSet::from([expanded])) {
            select.add_item(label, row);
        }
        siv.add_layer(select.with_name("reviews"));

        // Starts on D1's entry row; moving down must skip both of its detail lines and land
        // directly on D2, not stop partway through the diffstat.
        move_selection(&mut siv, true);
        assert!(matches!(selected_row(&mut siv), Row::Entry(k) if k.id == "D2"));

        // And back up, skipping the same detail lines in the other direction.
        move_selection(&mut siv, false);
        assert!(matches!(selected_row(&mut siv), Row::Entry(k) if k.id == "D1"));
    }

    #[test]
    fn move_selection_stops_at_the_last_row_even_if_it_is_a_detail_line() {
        let mut siv = cursive::dummy();
        let entries = vec![entry("D1", Some("a.rs | 1 +"))];
        let mut select = SelectView::<Row>::new();
        let expanded = ReviewKey::new("moz", "D1");
        for (label, row) in build_rows(&entries, 5, 6, &BTreeSet::from([expanded])) {
            select.add_item(label, row);
        }
        siv.add_layer(select.with_name("reviews"));

        move_selection(&mut siv, true); // land on the one detail row
        assert!(matches!(selected_row(&mut siv), Row::Detail(_)));

        // No review below it to skip forward to - must not hang looping at the boundary.
        move_selection(&mut siv, true);
        assert!(matches!(selected_row(&mut siv), Row::Detail(_)));
    }

    /// A `Ctx` whose `paths` point at a fresh tempdir seeded with `entries` in `state.json`, so
    /// `reload` (which `set_expanded` calls) has real, stable rows to rebuild the `reviews` view
    /// from instead of reading whatever's on the real machine's disk.
    fn ctx_with_state(tmp: &std::path::Path, entries: Vec<ReviewEntry>) -> Ctx {
        let paths = crate::paths::Paths::discover()
            .unwrap()
            .with_overrides(Some(tmp.join("data")));
        let mut state = crate::state::State::default();
        for e in entries {
            state.insert(e);
        }
        state.save(&paths.state_file()).unwrap();
        Ctx {
            paths,
            config: Config::default(),
            config_path: PathBuf::new(),
            sources: Vec::new(),
            handle: tokio::runtime::Handle::current(),
            all: true,
            key_w: 5,
            author_w: 6,
            expanded: BTreeSet::new(),
            pending_shell: None,
        }
    }

    #[tokio::test]
    async fn set_expanded_is_directional_not_a_toggle() {
        let tmp = tempfile::tempdir().unwrap();
        let mut siv = cursive::dummy();
        siv.set_user_data(ctx_with_state(tmp.path(), vec![entry("D1", None)]));
        let mut select = SelectView::<Row>::new();
        select.add_item("D1", Row::Entry(ReviewKey::new("moz", "D1")));
        siv.add_layer(select.with_name("reviews"));

        // Pressing `l` twice must stay expanded, not toggle back to collapsed.
        set_expanded(&mut siv, true);
        set_expanded(&mut siv, true);
        assert!(
            siv.user_data::<Ctx>()
                .unwrap()
                .expanded
                .contains(&ReviewKey::new("moz", "D1"))
        );

        // Pressing `h` twice must stay collapsed.
        set_expanded(&mut siv, false);
        set_expanded(&mut siv, false);
        assert!(
            !siv.user_data::<Ctx>()
                .unwrap()
                .expanded
                .contains(&ReviewKey::new("moz", "D1"))
        );
    }

    #[tokio::test]
    async fn set_expanded_targets_whichever_review_owns_the_selected_row() {
        let tmp = tempfile::tempdir().unwrap();
        let mut siv = cursive::dummy();
        siv.set_user_data(ctx_with_state(
            tmp.path(),
            vec![entry("D1", Some("a.rs | 1 +")), entry("D2", None)],
        ));
        let mut select = SelectView::<Row>::new();
        // Selection starts on D1's lone detail line, not its entry row.
        select.add_item("D1", Row::Entry(ReviewKey::new("moz", "D1")));
        select.add_item("  a.rs | 1 +", Row::Detail(ReviewKey::new("moz", "D1")));
        select.add_item("D2", Row::Entry(ReviewKey::new("moz", "D2")));
        select.set_selection(1);
        siv.add_layer(select.with_name("reviews"));

        set_expanded(&mut siv, true);

        assert_eq!(
            siv.user_data::<Ctx>().unwrap().expanded,
            BTreeSet::from([ReviewKey::new("moz", "D1")]),
            "the detail row's owning key (D1) should be expanded, not D2"
        );
    }
}
