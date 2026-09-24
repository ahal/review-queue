//! `rq list`'s interactive TUI: a `cursive` (crossterm backend) `SelectView` of tracked reviews.
//! Up/down (built into `SelectView`) moves the cursor, enter opens the review in a browser, `f`
//! fetches it locally on demand - the same `sync::fetch_local` a plain `rq fetch <id>` runs. See
//! `crate::sync`'s module docs for why `rq sync` itself never creates workspaces.

use std::path::PathBuf;

use anyhow::Result;
use cursive::event::Key;
use cursive::traits::*;
use cursive::views::{Dialog, LinearLayout, SelectView, TextView};
use cursive::{Cursive, CursiveExt};

use crate::config::{self, Config};
use crate::paths::Paths;
use crate::repo::{NeedsClone, OnMissing};
use crate::source::ReviewSource;
use crate::state::{ReviewEntry, ReviewKey, State};
use crate::sync;

const HELP: &str = "↑/↓ move   enter open in browser   f fetch locally   r reload   q quit";

struct Ctx {
    paths: Paths,
    config: Config,
    config_path: PathBuf,
    sources: Vec<Box<dyn ReviewSource>>,
    handle: tokio::runtime::Handle,
    all: bool,
    key_w: usize,
    author_w: usize,
}

/// Run the TUI until the user quits. Blocks the current thread; call from within a Tokio runtime
/// (needed for the fetch hotkey, which drives `sync::fetch_local` to completion synchronously).
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
    });

    let mut select = SelectView::<ReviewKey>::new();
    for e in &entries {
        select.add_item(row_label(e, key_w, author_w), e.key.clone());
    }
    select.set_on_submit(open_in_browser);

    let layout = LinearLayout::vertical()
        .child(TextView::new(header_line(key_w, author_w)))
        .child(select.with_name("reviews").scrollable().full_height())
        .child(TextView::new("").with_name("status"))
        .child(TextView::new(HELP));
    siv.add_fullscreen_layer(layout);

    siv.add_global_callback('q', |s| s.quit());
    siv.add_global_callback(Key::Esc, |s| s.quit());
    siv.add_global_callback('f', fetch_selected);
    siv.add_global_callback('r', reload);
    siv.add_global_callback('j', |s| move_selection(s, true));
    siv.add_global_callback('k', |s| move_selection(s, false));

    siv.run();
    Ok(())
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
        "{:<key_w$}  {:<3}  {:<9}  {:<author_w$}  TITLE",
        "KEY", "GOT", "STATUS", "AUTHOR"
    )
}

fn row_label(e: &ReviewEntry, key_w: usize, author_w: usize) -> String {
    let fetched = if e.workspace.is_some() { "yes" } else { "no" };
    let status = match &e.workspace {
        Some(ws) => format!("{:?}", ws.status),
        None if e.resolved => "resolved".to_string(),
        None if e.in_queue => "queued".to_string(),
        None => "waiting".to_string(),
    };
    format!(
        "{:<key_w$}  {fetched:<3}  {status:<9}  {}  {}",
        e.key.slug(),
        truncate(&e.author, author_w),
        e.title,
    )
}

fn set_status(s: &mut Cursive, msg: impl Into<String>) {
    s.call_on_name("status", |v: &mut TextView| v.set_content(msg.into()));
}

fn move_selection(s: &mut Cursive, down: bool) {
    s.call_on_name("reviews", |v: &mut SelectView<ReviewKey>| {
        let _ = if down {
            v.select_down(1)
        } else {
            v.select_up(1)
        };
    });
}

fn selected_key(s: &mut Cursive) -> Option<ReviewKey> {
    s.call_on_name("reviews", |v: &mut SelectView<ReviewKey>| v.selection())
        .flatten()
        .map(|k| (*k).clone())
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

fn fetch_selected(s: &mut Cursive) {
    let Some(key) = selected_key(s) else {
        return;
    };
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
    do_fetch(s, key, on_missing);
}

fn do_fetch(s: &mut Cursive, key: ReviewKey, on_missing: OnMissing) {
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
        // apply is recorded, left in place for inspection) - reload to reflect that.
        Some(Ok(path)) => {
            set_status(s, format!("fetched {key} -> {}", path.display()));
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
        do_fetch(s, yes_key.clone(), OnMissing::Clone);
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
        do_fetch(s, always_key.clone(), OnMissing::Clone);
    });
    s.add_layer(dialog);
}

fn reload(s: &mut Cursive) {
    let loaded = s
        .user_data::<Ctx>()
        .map(|ctx| (load_rows(&ctx.paths, ctx.all), ctx.key_w, ctx.author_w));
    let Some((loaded, key_w, author_w)) = loaded else {
        return;
    };
    let entries = match loaded {
        Ok(entries) => entries,
        Err(e) => {
            set_status(s, format!("reload failed: {e:#}"));
            return;
        }
    };

    s.call_on_name("reviews", |v: &mut SelectView<ReviewKey>| {
        let selected = v.selection();
        v.clear();
        for e in &entries {
            v.add_item(row_label(e, key_w, author_w), e.key.clone());
        }
        if let Some(selected) = selected
            && let Some(idx) = entries.iter().position(|e| e.key == *selected)
        {
            v.set_selection(idx);
        }
    });
}
