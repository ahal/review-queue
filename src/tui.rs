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
//! can take a while. Once confirmed, `begin_fetch` runs it on a background thread so the event
//! loop stays responsive: a spinner ticks via `Cursive::cb_sink`, and `Cancel` stays clickable
//! throughout. The underlying `git`/`jj` calls have no cancellation points of their own, so
//! cancelling doesn't interrupt them - it just detaches from the operation, which keeps running;
//! `finish_fetch` still runs when it completes, and removes whatever workspace it created instead
//! of opening a shell into it.

use std::collections::BTreeSet;
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
    fetching: BTreeSet<ReviewKey>,
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
        fetching: BTreeSet::new(),
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

fn row_label(e: &ReviewEntry, key_w: usize, author_w: usize, expanded: bool) -> StyledString {
    let marker = if expanded { '\u{25be}' } else { '\u{25b8}' };

    let mut out = StyledString::styled(format!("{marker} "), Effect::Dim);
    if e.workspace.is_some() {
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

fn build_rows(
    entries: &[ReviewEntry],
    key_w: usize,
    author_w: usize,
    expanded: &BTreeSet<ReviewKey>,
) -> Vec<(StyledString, Row)> {
    let mut rows = Vec::new();
    for e in entries {
        let is_expanded = expanded.contains(&e.key);
        rows.push((
            row_label(e, key_w, author_w, is_expanded),
            Row::Entry(e.key.clone()),
        ));
        if is_expanded {
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
    if ctx.fetching.contains(&key) {
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

/// Per-fetch view names, namespaced by review key - two reviews can be fetching at once (the TUI
/// stays responsive while a fetch runs), so a bare `"fetch_dialog"`/`"fetch_spinner"` would let
/// one's dialog get mistaken for the other's.
fn fetch_dialog_name(key: &ReviewKey) -> String {
    format!("fetch_dialog::{key}")
}
fn fetch_spinner_name(key: &ReviewKey) -> String {
    format!("fetch_spinner::{key}")
}

/// Confirms before fetching a review that has no local worktree yet, since `fetch_local` may
/// clone a repo or run a source's checkout command - either can take a while. `Proceed` hands off
/// to `begin_fetch`; `Cancel` also flips `cancelled`, though it's a no-op at this point since
/// nothing is running yet - the same flag is threaded through to `finish_fetch` in case the user
/// cancels again once the fetch is actually in flight.
fn prompt_confirm_fetch(s: &mut Cursive, key: ReviewKey, on_missing: OnMissing) {
    let cancelled = Arc::new(AtomicBool::new(false));
    let cancel_flag = cancelled.clone();
    let proceed_key = key.clone();
    let dialog = Dialog::text(format!("No local worktree for `{key}` yet.\nFetch it now?"))
        .title("Fetch review?")
        .button("Cancel", move |s| {
            cancel_flag.store(true, Ordering::SeqCst);
            s.pop_layer();
        })
        .button("Proceed", move |s| {
            begin_fetch(s, proceed_key.clone(), on_missing, cancelled.clone());
        })
        .with_name(fetch_dialog_name(&key));
    s.add_layer(dialog);
}

/// Kicks off `sync::fetch_local` on a background thread and shows progress while it runs. If
/// `s` has a `fetch_dialog` layer already up (the `prompt_confirm_fetch` dialog, `Proceed` just
/// pressed), it's reused: `Proceed` is disabled - greyed out and unclickable - rather than
/// removed, and `Cancel` (already wired to `cancelled`) is left alone so it keeps working exactly
/// as it did before the fetch started. Otherwise (e.g. after confirming a clone from
/// `prompt_clone`, which has no preceding fetch dialog to repurpose) a fresh one is built with
/// just `Cancel`.
fn begin_fetch(s: &mut Cursive, key: ReviewKey, on_missing: OnMissing, cancelled: Arc<AtomicBool>) {
    let dialog_name = fetch_dialog_name(&key);
    let spinner_name = fetch_spinner_name(&key);
    let reused = s
        .call_on_name(&dialog_name, |d: &mut Dialog| {
            if let Some(btn) = d.buttons_mut().nth(1) {
                btn.disable();
            }
            d.set_content(TextView::new(spinner_line(&key, 0)).with_name(spinner_name.clone()));
        })
        .is_some();
    if !reused {
        let cancel_flag = cancelled.clone();
        let dialog = Dialog::around(TextView::new(spinner_line(&key, 0)).with_name(spinner_name))
            .title("Fetching...")
            .button("Cancel", move |s| {
                cancel_flag.store(true, Ordering::SeqCst);
                s.pop_layer();
            })
            .with_name(dialog_name);
        s.add_layer(dialog);
    }

    let Some(ctx) = s.user_data::<Ctx>() else {
        return;
    };
    ctx.fetching.insert(key.clone());
    let sources = ctx.sources.clone();
    let paths = ctx.paths.clone();
    let config = ctx.config.clone();
    let handle = ctx.handle.clone();

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

fn spinner_line(key: &ReviewKey, frame: usize) -> String {
    format!(
        "{} fetching `{key}`...",
        SPINNER_FRAMES[frame % SPINNER_FRAMES.len()]
    )
}

/// Ticks `key`'s spinner text view roughly every 120ms until `done` is set, so `begin_fetch`'s
/// background fetch has some visible sign of life instead of a frozen-looking dialog.
fn spawn_spinner_ticker(s: &Cursive, key: ReviewKey, done: Arc<AtomicBool>) {
    let cb_sink = s.cb_sink().clone();
    let spinner_name = fetch_spinner_name(&key);
    std::thread::spawn(move || {
        let mut frame = 0usize;
        loop {
            std::thread::sleep(Duration::from_millis(120));
            if done.load(Ordering::SeqCst) {
                break;
            }
            frame = frame.wrapping_add(1);
            let tick_key = key.clone();
            let name = spinner_name.clone();
            if cb_sink
                .send(Box::new(move |s| {
                    s.call_on_name(&name, |v: &mut TextView| {
                        v.set_content(spinner_line(&tick_key, frame));
                    });
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
    // By name and not `pop_layer()` - the user is free to open other dialogs (e.g. `d`) while a
    // fetch runs in the background, which would otherwise end up on top of this one and get
    // popped by mistake instead of it.
    if let Some(pos) = s
        .screen_mut()
        .find_layer_from_name(&fetch_dialog_name(&key))
    {
        s.screen_mut().remove_layer(pos);
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
        begin_fetch(
            s,
            yes_key.clone(),
            OnMissing::Clone,
            Arc::new(AtomicBool::new(false)),
        );
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
        begin_fetch(
            s,
            always_key.clone(),
            OnMissing::Clone,
            Arc::new(AtomicBool::new(false)),
        );
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
            sources: Arc::new(Vec::new()),
            handle: tokio::runtime::Handle::current(),
            all: true,
            key_w: 5,
            author_w: 6,
            expanded: BTreeSet::new(),
            pending_shell: None,
            fetching: BTreeSet::new(),
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
