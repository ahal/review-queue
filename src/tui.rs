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
//! workspaces. `d` deletes the selected review's workspace (`sync::remove_workspace`), confirming
//! first and, if it has local changes, confirming again before discarding them.
//!
//! Fetching a review with no local worktree yet asks to confirm first (`prompt_confirm_fetch`) -
//! `fetch_local` may clone a repo or shell out to a source's checkout command, either of which
//! can take a while. Once confirmed the dialog closes and `begin_fetch` runs it on a background
//! thread so the event loop stays responsive: the review's row shows a spinner (ticked via
//! `Cursive::cb_sink`) where its workspace icon goes, and `d` on that row cancels. The underlying
//! `git`/`jj` calls have no cancellation points of their own, so cancelling doesn't interrupt
//! them - it just detaches from the operation, which keeps running (spinner turns red);
//! `finish_fetch` still runs when it completes, and removes whatever workspace it created instead
//! of opening a shell into it.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use anyhow::Result;
use cursive::Cursive;
use cursive::event::Key;
use cursive::theme::{BaseColor, Color, Effect, Theme};
use cursive::traits::*;
use cursive::utils::markup::StyledString;
use cursive::views::{Dialog, LinearLayout, OnEventView, SelectView, TextView};

use crate::config::{self, Config};
use crate::paths::Paths;
use crate::repo::{NeedsClone, OnMissing};
use crate::source::ReviewSource;
use crate::state::{ReviewEntry, ReviewKey, State};
use crate::sync;

/// Frames for the spinner shown while a review is fetched on a background thread.
const SPINNER_FRAMES: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

const HELP: &str = "↑/↓ move   ←/→ expand/collapse   enter open locally   o open in browser   \
d delete workspace   r reload   q quit";

struct Ctx {
    paths: Paths,
    config: Config,
    config_path: PathBuf,
    sources: Arc<Vec<Box<dyn ReviewSource>>>,
    handle: tokio::runtime::Handle,
    all: bool,
    key_w: usize,
    author_w: usize,
    /// Reviews currently expanded to show their diffstat - any number at once, independently.
    expanded: BTreeSet<ReviewKey>,
    /// Set by `do_open_locally` on a successful fetch; drained by `run_event_loop`, which is the
    /// only place actually allowed to touch the terminal/backend to suspend into a subshell.
    pending_shell: Option<(ReviewKey, PathBuf)>,
    /// Reviews `begin_fetch` currently has a background fetch running for - guards against
    /// pressing `enter` on the same review again (and racing two `fetch_local` calls against the
    /// same workspace path) while its own confirm/progress dialog is already up.
    fetching: BTreeMap<ReviewKey, Fetch>,
}

/// A background fetch `begin_fetch` has running for a review, shown as a spinner in that review's
/// row until it finishes.
struct Fetch {
    /// Flipped by `cancel_fetch`. The underlying `fetch_local` can't be interrupted, so this only
    /// tells `finish_fetch` to clean up instead of opening the workspace.
    cancelled: Arc<AtomicBool>,
    frame: usize,
}

/// What `row_label` needs to draw a fetch's spinner in place of the workspace icon.
#[derive(Debug, Clone, Copy)]
struct Spin {
    frame: usize,
    cancelling: bool,
}

type Spinners = BTreeMap<ReviewKey, Spin>;

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
    // Inherit the terminal's own colors. The palette's `Highlight` styles already use reverse video,
    // so the selected row stays visible without hardcoding any colors.
    siv.set_theme(Theme::terminal_default());
    siv.set_user_data(Ctx {
        paths,
        config,
        config_path,
        sources: Arc::new(sources),
        handle: tokio::runtime::Handle::current(),
        all,
        key_w,
        author_w,
        expanded: BTreeSet::new(),
        pending_shell: None,
        fetching: BTreeMap::new(),
    });

    let mut select = SelectView::<Row>::new();
    for (label, row) in build_rows(
        &entries,
        key_w,
        author_w,
        &BTreeSet::new(),
        terminal_width(),
        &Spinners::new(),
    ) {
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
        .child(select.scrollable().full_height())
        .child(TextView::new("").with_name("status"))
        .child(TextView::new(StyledString::styled(HELP, Effect::Dim)));
    siv.add_fullscreen_layer(layout);

    siv.add_global_callback('q', |s| s.quit());
    siv.add_global_callback(Key::Esc, |s| s.quit());
    siv.add_global_callback('o', open_in_browser_selected);
    siv.add_global_callback('d', delete_workspace_selected);
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
                if let Some((key, path)) = runner
                    .user_data::<Ctx>()
                    .and_then(|ctx| ctx.pending_shell.take())
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
                let result = std::process::Command::new(&shell)
                    .current_dir(&path)
                    .status();
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

fn ansi(c: BaseColor) -> Color {
    Color::Dark(c)
}

fn row_label(
    e: &ReviewEntry,
    key_w: usize,
    author_w: usize,
    expanded: bool,
    spin: Option<Spin>,
) -> StyledString {
    let marker = if expanded { '\u{25be}' } else { '\u{25b8}' };

    let mut out = StyledString::styled(format!("{marker} "), Effect::Dim);
    if let Some(spin) = spin {
        let color = if spin.cancelling {
            BaseColor::Red
        } else {
            BaseColor::Yellow
        };
        out.append_styled(SPINNER_FRAMES[spin.frame % SPINNER_FRAMES.len()], ansi(color));
    } else if e.workspace.is_some() {
        out.append_styled("\u{2913}", ansi(BaseColor::Cyan));
    } else {
        out.append_plain(" ");
    }
    out.append_plain(" ");
    out.append_styled(format!("{:<key_w$}", e.key.slug()), ansi(BaseColor::Cyan));
    out.append_plain("  ");
    out.append_plain(truncate(&e.author, author_w));
    out.append_plain("  ");
    out.append_plain(&e.title);
    out
}

/// Colors a diffstat line: the `+`/`-` graph after a file's `|` becomes green/red, and in the
/// `N insertions(+), M deletions(-)` summary the matching segments get the same treatment.
fn style_detail(line: &str) -> StyledString {
    let mut out = StyledString::new();
    if let Some((name, graph)) = line.split_once('|') {
        out.append_plain(format!("{name}|"));
        if graph.contains("Bin ") {
            out.append_plain(graph);
            return out;
        }
        for c in graph.chars() {
            match c {
                '+' => out.append_styled(c.to_string(), ansi(BaseColor::Green)),
                '-' => out.append_styled(c.to_string(), ansi(BaseColor::Red)),
                _ => out.append_plain(c.to_string()),
            }
        }
    } else {
        for (i, part) in line.split(',').enumerate() {
            if i > 0 {
                out.append_plain(",");
            }
            if part.ends_with("(+)") {
                out.append_styled(part, ansi(BaseColor::Green));
            } else if part.ends_with("(-)") {
                out.append_styled(part, ansi(BaseColor::Red));
            } else {
                out.append_plain(part);
            }
        }
    }
    out
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

/// Indent for the expanded lines under a review (matches the diffstat).
const DETAIL_INDENT: &str = "      ";

/// Width to wrap at when the terminal size can't be read.
const DEFAULT_WIDTH: usize = 100;

/// The terminal's current width. `SelectView` rows are single-line and can't wrap themselves, so
/// expanded text is wrapped up front to fit; a resize is picked up the next time rows are rebuilt.
fn terminal_width() -> usize {
    cursive::backends::crossterm::crossterm::terminal::size()
        .map(|(w, _)| w as usize)
        .unwrap_or(DEFAULT_WIDTH)
}

/// Word-wraps `text` to `width` columns, keeping blank lines and each line's leading indent.
fn wrap_text(text: &str, width: usize) -> Vec<String> {
    let mut lines = Vec::new();
    for raw in text.lines() {
        let raw = raw.trim_end();
        let indent = raw.len() - raw.trim_start().len();
        let mut line = " ".repeat(indent);
        let mut has_word = false;
        for word in raw.split_whitespace() {
            if has_word && line.chars().count() + 1 + word.chars().count() > width {
                lines.push(std::mem::replace(&mut line, " ".repeat(indent)));
                has_word = false;
            }
            if has_word {
                line.push(' ');
            }
            line.push_str(word);
            has_word = true;
        }
        lines.push(line);
    }
    lines
}

/// The full title, then the PR description / commit message, shown under an expanded review.
/// The title is repeated in full since the review's own row truncates it to the terminal width.
/// Wrapped to `width` and each block followed by a blank line.
fn description_lines(e: &ReviewEntry, width: usize) -> Vec<String> {
    let wrap = width.saturating_sub(DETAIL_INDENT.len() + 2).max(20);
    let mut lines = wrap_text(&e.title, wrap);
    lines.push(String::new());
    if let Some(desc) = e.description.as_deref().map(str::trim).filter(|d| !d.is_empty()) {
        lines.extend(wrap_text(desc, wrap));
        lines.push(String::new());
    }
    lines
        .into_iter()
        .map(|l| format!("{DETAIL_INDENT}{l}"))
        .collect()
}

fn build_rows(
    entries: &[ReviewEntry],
    key_w: usize,
    author_w: usize,
    expanded: &BTreeSet<ReviewKey>,
    width: usize,
    spinners: &Spinners,
) -> Vec<(StyledString, Row)> {
    let mut rows = Vec::new();
    for e in entries {
        let is_expanded = expanded.contains(&e.key);
        rows.push((
            row_label(e, key_w, author_w, is_expanded, spinners.get(&e.key).copied()),
            Row::Entry(e.key.clone()),
        ));
        if is_expanded {
            for line in description_lines(e, width) {
                rows.push((StyledString::plain(line), Row::Detail(e.key.clone())));
            }
            for line in detail_lines(e) {
                rows.push((style_detail(&line), Row::Detail(e.key.clone())));
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
            let _ = if down {
                v.select_down(1)
            } else {
                v.select_up(1)
            };
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

fn delete_workspace_selected(s: &mut Cursive) {
    let Some(key) = selected_key(s) else {
        return;
    };
    // Nothing to delete yet while it's still being fetched - `d` cancels it instead.
    if s.user_data::<Ctx>().is_some_and(|ctx| ctx.fetching.contains_key(&key)) {
        cancel_fetch(s, &key);
        return;
    }
    let workspace_path = s.user_data::<Ctx>().and_then(|ctx| {
        State::load(&ctx.paths.state_file())
            .ok()
            .and_then(|st| st.get(&key).and_then(|e| e.workspace.as_ref()).cloned())
            .map(|ws| ws.workspace_path)
    });
    match workspace_path {
        Some(path) => prompt_delete_workspace(s, key, path, false),
        None => set_status(s, format!("`{key}` has no local workspace")),
    }
}

/// Confirms before deleting a workspace - `force` (retried after a `WorkspaceDirty` error) warns
/// that local changes will be discarded instead of just naming the path.
fn prompt_delete_workspace(s: &mut Cursive, key: ReviewKey, path: PathBuf, force: bool) {
    let (title, text) = if force {
        (
            "Discard local changes?",
            format!(
                "{} has local changes.\nDelete it anyway, discarding them?",
                path.display()
            ),
        )
    } else {
        (
            "Delete workspace?",
            format!("Delete workspace at {}?", path.display()),
        )
    };
    let yes_key = key.clone();
    let dialog = Dialog::text(text)
        .title(title)
        .button("No", |s| {
            s.pop_layer();
        })
        .button("Yes", move |s| {
            s.pop_layer();
            do_delete_workspace(s, yes_key.clone(), path.clone(), force);
        });
    s.add_layer(dialog);
}

fn do_delete_workspace(s: &mut Cursive, key: ReviewKey, path: PathBuf, force: bool) {
    let outcome = s
        .user_data::<Ctx>()
        .map(|ctx| sync::remove_workspace(&ctx.paths, &key, force));
    match outcome {
        Some(Ok(())) => {
            set_status(s, format!("deleted workspace for {key}"));
            reload(s);
        }
        Some(Err(e)) => match e.downcast::<sync::WorkspaceDirty>() {
            Ok(_) => prompt_delete_workspace(s, key, path, true),
            Err(e) => set_status(s, format!("failed to delete workspace for {key}: {e:#}")),
        },
        None => {}
    }
}

fn open_locally_selected_key(s: &mut Cursive, key: ReviewKey) {
    let Some(ctx) = s.user_data::<Ctx>() else {
        return;
    };
    let on_missing = if ctx.config.auto_clone {
        OnMissing::Clone
    } else {
        OnMissing::Ask
    };
    if ctx.fetching.contains_key(&key) {
        set_status(s, format!("already fetching {key}"));
        return;
    }
    let has_workspace = State::load(&ctx.paths.state_file())
        .ok()
        .and_then(|st| st.get(&key).map(|e| e.workspace.is_some()))
        .unwrap_or(false);

    if has_workspace {
        // Already fetched - `fetch_local` is a fast, local no-op in this case, so there's
        // nothing worth showing a confirmation or progress dialog for.
        do_open_locally(s, key, on_missing);
    } else {
        prompt_confirm_fetch(s, key, on_missing);
    }
}

/// Confirms before fetching a review that has no local worktree yet, since `fetch_local` may
/// clone a repo or run a source's checkout command - either can take a while. `Proceed` closes
/// the dialog and hands off to `begin_fetch`, which shows progress in the review's own row.
fn prompt_confirm_fetch(s: &mut Cursive, key: ReviewKey, on_missing: OnMissing) {
    let dialog = Dialog::text(format!("No local worktree for `{key}` yet.\nFetch it now?"))
        .title("Fetch review?")
        .button("Cancel", |s| {
            s.pop_layer();
        })
        .button("Proceed", move |s| {
            s.pop_layer();
            begin_fetch(s, key.clone(), on_missing);
        });
    s.add_layer(dialog);
}

/// Kicks off `sync::fetch_local` on a background thread. Progress is a spinner in the review's
/// row where the workspace icon goes (see `row_label`), so the rest of the UI stays usable; `d`
/// on that row cancels (`cancel_fetch`).
fn begin_fetch(s: &mut Cursive, key: ReviewKey, on_missing: OnMissing) {
    let cancelled = Arc::new(AtomicBool::new(false));
    let Some(ctx) = s.user_data::<Ctx>() else {
        return;
    };
    ctx.fetching.insert(
        key.clone(),
        Fetch {
            cancelled: cancelled.clone(),
            frame: 0,
        },
    );
    let sources = ctx.sources.clone();
    let paths = ctx.paths.clone();
    let config = ctx.config.clone();
    let handle = ctx.handle.clone();
    reload(s);

    let done = Arc::new(AtomicBool::new(false));
    spawn_spinner_ticker(s, key.clone(), done.clone());

    let cb_sink = s.cb_sink().clone();
    let worker_key = key.clone();
    std::thread::spawn(move || {
        let result = handle.block_on(sync::fetch_local(
            &sources,
            &paths,
            &config,
            &worker_key,
            on_missing,
        ));
        done.store(true, Ordering::SeqCst);
        let _ = cb_sink.send(Box::new(move |s| {
            finish_fetch(s, worker_key, result, cancelled)
        }));
    });
}

/// Cancels a review's in-flight fetch (`d` on its row, see `delete_workspace_selected`).
/// `fetch_local`'s `git`/`jj` calls have no cancellation points, so the operation keeps running in
/// the background: the spinner turns red until it completes, and `finish_fetch` then removes
/// whatever workspace it created.
fn cancel_fetch(s: &mut Cursive, key: &ReviewKey) {
    let flagged = s
        .user_data::<Ctx>()
        .and_then(|ctx| ctx.fetching.get(key))
        .map(|f| f.cancelled.swap(true, Ordering::SeqCst));
    match flagged {
        Some(false) => {
            set_status(
                s,
                format!("cancelling {key} - it will be cleaned up once the running operation stops"),
            );
            reload(s);
        }
        Some(true) => set_status(s, format!("already cancelling {key}")),
        None => set_status(s, format!("`{key}` isn't being fetched")),
    }
}

/// Advances `key`'s row spinner roughly every 120ms until `done` is set, so `begin_fetch`'s
/// background fetch has some visible sign of life.
fn spawn_spinner_ticker(s: &Cursive, key: ReviewKey, done: Arc<AtomicBool>) {
    let cb_sink = s.cb_sink().clone();
    std::thread::spawn(move || {
        loop {
            std::thread::sleep(Duration::from_millis(120));
            if done.load(Ordering::SeqCst) {
                break;
            }
            let tick_key = key.clone();
            if cb_sink
                .send(Box::new(move |s| {
                    let ticked = s
                        .user_data::<Ctx>()
                        .and_then(|ctx| ctx.fetching.get_mut(&tick_key))
                        .map(|f| f.frame = f.frame.wrapping_add(1))
                        .is_some();
                    if ticked {
                        reload(s);
                    }
                }))
                .is_err()
            {
                break; // UI gone.
            }
        }
    });
}

/// Runs once `begin_fetch`'s background thread finishes, back on the main thread via `cb_sink`.
/// If `cancelled` was flipped in the meantime, the operation itself was never interrupted (see the
/// module docs), so a successful fetch's workspace is removed here instead of being opened;
/// nothing needs cleaning up on a failed one.
fn finish_fetch(
    s: &mut Cursive,
    key: ReviewKey,
    result: Result<PathBuf>,
    cancelled: Arc<AtomicBool>,
) {
    if let Some(ctx) = s.user_data::<Ctx>() {
        ctx.fetching.remove(&key);
    }

    if cancelled.load(Ordering::SeqCst) {
        let status = match &result {
            Ok(_) => match s
                .user_data::<Ctx>()
                .map(|ctx| sync::remove_workspace(&ctx.paths, &key, true))
            {
                Some(Ok(())) => format!("cancelled fetching {key}; workspace cleaned up"),
                Some(Err(e)) => format!("cancelled fetching {key}; cleanup failed: {e:#}"),
                None => format!("cancelled fetching {key}"),
            },
            Err(_) => format!("cancelled fetching {key}"),
        };
        set_status(s, status);
        reload(s);
        return;
    }

    match result {
        Ok(path) => {
            if let Some(ctx) = s.user_data::<Ctx>() {
                ctx.pending_shell = Some((key, path));
            }
            reload(s);
        }
        Err(e) => match e.downcast::<NeedsClone>() {
            Ok(needs_clone) => prompt_clone(s, key, needs_clone.url, needs_clone.dest),
            Err(e) => {
                set_status(s, format!("error fetching {key}: {e:#}"));
                reload(s);
            }
        },
    }
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
        begin_fetch(s, yes_key.clone(), OnMissing::Clone);
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
        begin_fetch(s, always_key.clone(), OnMissing::Clone);
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
            ctx.fetching
                .iter()
                .map(|(k, f)| {
                    let spin = Spin {
                        frame: f.frame,
                        cancelling: f.cancelled.load(Ordering::SeqCst),
                    };
                    (k.clone(), spin)
                })
                .collect::<Spinners>(),
        )
    });
    let Some((loaded, key_w, author_w, expanded, spinners)) = loaded else {
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
        for (label, row) in build_rows(&entries, key_w, author_w, &expanded, terminal_width(), &spinners) {
            v.add_item(label, row);
        }
        if let Some(selected) = selected
            && let Some(idx) =
                (0..v.len()).find(|&i| v.get_item(i).is_some_and(|(_, row)| *row.key() == selected))
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
            description: None,
        }
    }

    #[test]
    fn collapsed_entries_produce_one_row_each() {
        let entries = vec![entry("D1", None), entry("D2", None)];
        let rows = build_rows(&entries, 5, 6, &BTreeSet::new(), 100, &Spinners::new());
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
        let rows = build_rows(&entries, 5, 6, &BTreeSet::from([expanded.clone()]), 100, &Spinners::new());

        // D1's entry row, its title + blank line, its two diffstat lines, then D2's entry row.
        assert_eq!(rows.len(), 6);
        assert!(matches!(&rows[0].1, Row::Entry(k) if *k == expanded));
        for (_, row) in &rows[1..5] {
            assert_eq!(row, &Row::Detail(expanded.clone()));
        }
        assert!(matches!(&rows[5].1, Row::Entry(k) if k.id == "D2"));
    }

    #[test]
    fn multiple_reviews_can_be_expanded_at_once() {
        let entries = vec![
            entry("D1", Some("a.rs | 1 +")),
            entry("D2", Some("b.rs | 2 ++")),
        ];
        let d1 = ReviewKey::new("moz", "D1");
        let d2 = ReviewKey::new("moz", "D2");
        let rows = build_rows(&entries, 5, 6, &BTreeSet::from([d1.clone(), d2.clone()]), 100, &Spinners::new());

        // D1's entry + title/blank + its diffstat line, then the same for D2 - expanding D2 must
        // not have collapsed D1.
        assert_eq!(rows.len(), 8);
        assert!(matches!(&rows[0].1, Row::Entry(k) if *k == d1));
        for (_, row) in &rows[1..4] {
            assert_eq!(row, &Row::Detail(d1.clone()));
        }
        assert!(matches!(&rows[4].1, Row::Entry(k) if *k == d2));
        for (_, row) in &rows[5..8] {
            assert_eq!(row, &Row::Detail(d2.clone()));
        }
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
    fn expanding_a_review_shows_title_and_wrapped_description_before_the_diffstat() {
        let mut e = entry("D1", Some("a.rs | 1 +"));
        e.description = Some(format!("first para\n\n{}", "word ".repeat(60)));
        let lines = description_lines(&e, 100);

        assert_eq!(lines[0], "      Fix the thing");
        assert_eq!(lines[1], "      ");
        assert_eq!(lines[2], "      first para");
        assert_eq!(lines[3], "      ");
        assert!(lines.len() > 6, "long paragraph should wrap: {lines:?}");
        assert!(lines.iter().all(|l| l.chars().count() <= 100));
        assert_eq!(lines.last().unwrap(), "      ");

        let expanded = BTreeSet::from([e.key.clone()]);
        let rows = build_rows(&[e], 5, 6, &expanded, 100, &Spinners::new());
        // entry + title/description lines + diffstat line
        assert_eq!(rows.len(), 1 + lines.len() + 1);
    }

    #[test]
    fn a_long_title_is_wrapped_in_full_to_the_given_width() {
        let mut e = entry("D1", None);
        e.title = "word ".repeat(30);
        let lines = description_lines(&e, 40);
        assert!(lines.len() > 4, "{lines:?}");
        assert!(lines.iter().all(|l| l.chars().count() <= 40));
        assert_eq!(lines[..lines.len() - 1].join(" ").split_whitespace().count(), 30);
    }

    #[test]
    fn a_fetching_row_shows_a_spinner_instead_of_the_workspace_icon() {
        let e = entry("D1", None);
        let plain = row_label(&e, 5, 6, false, None).source().to_string();
        let spin = Spin {
            frame: 0,
            cancelling: false,
        };
        let spinning = row_label(&e, 5, 6, false, Some(spin)).source().to_string();
        assert!(spinning.contains(SPINNER_FRAMES[0]));
        assert!(!plain.contains(SPINNER_FRAMES[0]));
        assert_eq!(plain.chars().count(), spinning.chars().count());
    }

    #[tokio::test]
    async fn cancel_fetch_flags_only_a_review_that_is_being_fetched() {
        let tmp = tempfile::tempdir().unwrap();
        let mut siv = cursive::dummy();
        siv.set_user_data(ctx_with_state(tmp.path(), vec![entry("D1", None)]));
        let key = ReviewKey::new("moz", "D1");
        let flag = Arc::new(AtomicBool::new(false));
        siv.user_data::<Ctx>().unwrap().fetching.insert(
            key.clone(),
            Fetch {
                cancelled: flag.clone(),
                frame: 0,
            },
        );

        cancel_fetch(&mut siv, &ReviewKey::new("moz", "D2"));
        assert!(!flag.load(Ordering::SeqCst));
        cancel_fetch(&mut siv, &key);
        assert!(flag.load(Ordering::SeqCst));
    }

    #[test]
    fn a_review_without_a_description_still_shows_its_title() {
        let mut e = entry("D1", None);
        e.description = Some("  \n".into());
        assert_eq!(description_lines(&e, 100), vec!["      Fix the thing", "      "]);
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
        for (label, row) in build_rows(&entries, 5, 6, &BTreeSet::from([expanded]), 100, &Spinners::new()) {
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
        for (label, row) in build_rows(&entries, 5, 6, &BTreeSet::from([expanded]), 100, &Spinners::new()) {
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
            sources: Arc::new(Vec::new()),
            handle: tokio::runtime::Handle::current(),
            all: true,
            key_w: 5,
            author_w: 6,
            expanded: BTreeSet::new(),
            pending_shell: None,
            fetching: BTreeMap::new(),
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
