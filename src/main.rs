use std::io::{IsTerminal, Write};
use std::path::Path;

use anyhow::{Context, Result, bail};
use clap::Parser;
use comfy_table::{Table, presets::UTF8_FULL_CONDENSED};

use review_queue::cli::{Cli, Command, RepoCommand, Shell};
use review_queue::config::{self, Config, SourceConfig};
use review_queue::paths::Paths;
use review_queue::repo::{NeedsClone, OnMissing, RepoKind, RepoStore};
use review_queue::source::ReviewSource;
use review_queue::source::github::GithubSource;
use review_queue::source::moz_phab::MozPhabSource;
use review_queue::state::{ReviewEntry, ReviewKey, State};
use review_queue::sync::{self, SyncReport};
use review_queue::tui;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt::init();
    let cli = Cli::parse();

    let paths = Paths::discover()?;
    let config_path = cli.config.clone().unwrap_or_else(|| paths.config_file());
    let config = Config::load(&config_path)?;
    let paths = paths.with_overrides(config.data_dir.clone());

    match cli.command {
        Command::List { json, all, plain } => {
            list(&paths, &config, &config_path, json, all, plain).await
        }
        Command::Path { id } => path(&paths, &id),
        Command::Fetch { id } => fetch_cmd(&paths, &config, &config_path, &id).await,
        Command::Sync { source, dry_run } => {
            sync_cmd(&paths, &config, source.as_deref(), dry_run).await
        }
        Command::Doctor => doctor_cmd(&paths, &config_path, &config).await,
        Command::Repo { command } => match command {
            RepoCommand::List => repo_list_cmd(&paths, &config),
            RepoCommand::Rm { url } => repo_rm_cmd(&paths, &config, &url),
        },
        Command::ShellInit { shell } => {
            println!("{}", shell_init_script(shell));
            Ok(())
        }
    }
}

/// Build a `ReviewSource` for each configured source.
async fn build_sources(config: &Config) -> Result<Vec<Box<dyn ReviewSource>>> {
    let mut sources: Vec<Box<dyn ReviewSource>> = Vec::new();
    for sc in &config.sources {
        match sc {
            SourceConfig::Github(cfg) => {
                sources.push(Box::new(GithubSource::new(cfg.clone()).await?))
            }
            SourceConfig::MozPhab(cfg) => {
                sources.push(Box::new(MozPhabSource::new(cfg.clone()).await?))
            }
        }
    }
    Ok(sources)
}

/// Hold `sync.lock` for the duration of `f`, refusing to run alongside another `rq sync`/`rq
/// fetch` (e.g. an overlapping cron invocation). Advisory only, per `fd_lock`'s own caveats.
fn acquire_sync_lock(paths: &Paths) -> Result<fd_lock::RwLock<std::fs::File>> {
    paths.ensure_dirs()?;
    let lock_path = paths.sync_lock_file();
    let file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&lock_path)
        .with_context(|| format!("opening {}", lock_path.display()))?;
    Ok(fd_lock::RwLock::new(file))
}

async fn sync_cmd(
    paths: &Paths,
    config: &Config,
    only_source: Option<&str>,
    dry_run: bool,
) -> Result<()> {
    let mut lock = acquire_sync_lock(paths)?;
    let _guard = lock.try_write().map_err(|_| {
        anyhow::anyhow!(
            "another `rq sync` or `rq fetch` is already running (lock: {})",
            paths.sync_lock_file().display()
        )
    })?;

    let sources = build_sources(config).await?;
    if sources.is_empty() {
        bail!("no usable sources configured (see `config.toml`)");
    }
    let report = sync::sync(&sources, paths, only_source, dry_run).await?;
    print_sync_report(&report, dry_run);
    Ok(())
}

async fn fetch_cmd(paths: &Paths, config: &Config, config_path: &Path, id: &str) -> Result<()> {
    let key = resolve_key(paths, id)?;
    let mut lock = acquire_sync_lock(paths)?;
    let _guard = lock.try_write().map_err(|_| {
        anyhow::anyhow!(
            "another `rq sync` or `rq fetch` is already running (lock: {})",
            paths.sync_lock_file().display()
        )
    })?;

    let sources = build_sources(config).await?;
    let mut on_missing = if config.auto_clone {
        OnMissing::Clone
    } else {
        OnMissing::Ask
    };
    loop {
        match sync::fetch_local(&sources, paths, config, &key, on_missing).await {
            Ok(ws_path) => {
                println!("{}", ws_path.display());
                return Ok(());
            }
            Err(e) => {
                let Some(needs_clone) = e.downcast_ref::<NeedsClone>() else {
                    return Err(e);
                };
                on_missing = match prompt_clone_cli(&needs_clone.url, &needs_clone.dest)? {
                    CloneAnswer::Yes => OnMissing::Clone,
                    CloneAnswer::Always => {
                        config::set_auto_clone(config_path)?;
                        OnMissing::Clone
                    }
                    CloneAnswer::No => bail!("not cloning `{}`", needs_clone.url),
                };
            }
        }
    }
}

enum CloneAnswer {
    Yes,
    Always,
    No,
}

/// Ask whether to clone a review's repo into the data dir, since no local checkout was found.
fn prompt_clone_cli(url: &str, dest: &Path) -> Result<CloneAnswer> {
    if !std::io::stdin().is_terminal() {
        bail!(
            "no local checkout of `{url}` found, and stdin isn't a terminal to ask; \
             set `auto_clone = true` in config.toml, or run this interactively"
        );
    }
    print!("No local checkout of `{url}` found. Clone into {}? [Y/n/always] ", dest.display());
    std::io::stdout().flush()?;
    let mut line = String::new();
    std::io::stdin().read_line(&mut line)?;
    Ok(match line.trim().to_lowercase().as_str() {
        "" | "y" | "yes" => CloneAnswer::Yes,
        "always" | "a" => CloneAnswer::Always,
        _ => CloneAnswer::No,
    })
}

/// Resolve a review id/prefix (see `State::find_by_prefix`) to its key, without holding onto the
/// borrowed `State`.
fn resolve_key(paths: &Paths, id: &str) -> Result<ReviewKey> {
    let state = State::load(&paths.state_file())?;
    match state.find_by_prefix(id).as_slice() {
        [] => bail!("no tracked review matches `{id}`"),
        [entry] => Ok(entry.key.clone()),
        many => {
            let keys: Vec<_> = many.iter().map(|e| e.key.slug()).collect();
            bail!("`{id}` matches multiple reviews: {}", keys.join(", "))
        }
    }
}

async fn doctor_cmd(paths: &Paths, config_path: &Path, config: &Config) -> Result<()> {
    let mut ok = true;

    println!("config: {}", config_path.display());
    println!("data dir: {}", paths.data_dir().display());
    match &config.workdir {
        Some(w) => println!("workdir: {}", w.display()),
        None => println!("workdir: not set"),
    }
    println!();

    for tool in ["git", "jj"] {
        match std::process::Command::new(tool).arg("--version").output() {
            Ok(o) if o.status.success() => println!("✓ {tool} is available"),
            _ => {
                println!("✗ {tool} not found on PATH");
                ok = false;
            }
        }
    }

    let needs_moz_phab = config
        .sources
        .iter()
        .any(|s| matches!(s, SourceConfig::MozPhab(_)));
    match std::process::Command::new("moz-phab")
        .arg("version")
        .output()
    {
        Ok(o) if o.status.success() => println!("✓ moz-phab is available"),
        _ if needs_moz_phab => {
            println!("✗ moz-phab not found on PATH (required by your configured moz-phab source)");
            ok = false;
        }
        _ => println!("- moz-phab not found on PATH (fine - no moz-phab source configured)"),
    }
    println!();

    if config.sources.is_empty() {
        println!("✗ no sources configured in {}", config_path.display());
        ok = false;
    }
    for sc in &config.sources {
        let name = sc.name();
        let auth_result = match sc {
            SourceConfig::Github(cfg) => GithubSource::new(cfg.clone()).await?.check_auth().await,
            SourceConfig::MozPhab(cfg) => MozPhabSource::new(cfg.clone()).await?.check_auth().await,
        };
        match auth_result {
            Ok(username) => println!("✓ {name}: authenticated as {username}"),
            Err(e) => {
                println!("✗ {name}: {e}");
                ok = false;
            }
        }
    }

    if !ok {
        std::process::exit(1);
    }
    Ok(())
}

fn repo_list_cmd(paths: &Paths, config: &Config) -> Result<()> {
    let mut store = RepoStore::load(paths, config)?;
    store.rescan_if_needed()?;
    let state = State::load(&paths.state_file())?;
    let repos = store.list(&state);

    if repos.is_empty() {
        println!(
            "No canonical repos yet. Set `workdir` in config.toml, or run `rq fetch`/`rq sync`."
        );
        return Ok(());
    }

    let mut table = Table::new();
    table.load_preset(UTF8_FULL_CONDENSED);
    table.set_header(vec!["URL", "KIND", "PATH", "WORKSPACES"]);
    for r in repos {
        table.add_row(vec![
            r.url,
            match r.kind {
                RepoKind::Discovered => "discovered".to_string(),
                RepoKind::ToolManaged => "tool-managed".to_string(),
            },
            r.path.display().to_string(),
            r.workspace_count.to_string(),
        ]);
    }
    println!("{table}");
    Ok(())
}

fn repo_rm_cmd(paths: &Paths, config: &Config, url: &str) -> Result<()> {
    let mut store = RepoStore::load(paths, config)?;
    let state = State::load(&paths.state_file())?;
    store.remove(url, &state)?;
    store.save()?;
    println!("Removed tool-managed clone for {url}");
    Ok(())
}

fn shell_init_script(shell: Shell) -> &'static str {
    match shell {
        Shell::Bash | Shell::Zsh => {
            r#"# Requires `jq` and `fzf`. Add to your shell rc:
#   eval "$(rq shell-init bash)"   # or: rq shell-init zsh
rqcd() {
  local line
  line=$(rq list --json | jq -r '.[] | [(.key.source + "/" + .key.id), .title] | @tsv' | fzf --delimiter='\t' --with-nth=2 | cut -f1)
  [ -n "$line" ] && cd "$(rq path "$line")"
}"#
        }
        Shell::Fish => {
            r#"# Requires `jq` and `fzf`. Add to your fish config:
#   rq shell-init fish | source
function rqcd
    set -l line (rq list --json | jq -r '.[] | [(.key.source + "/" + .key.id), .title] | @tsv' | fzf --delimiter='\t' --with-nth=2 | cut -f1)
    if test -n "$line"
        cd (rq path $line)
    end
end"#
        }
    }
}

fn print_sync_report(report: &SyncReport, dry_run: bool) {
    let verb = if dry_run { "would add" } else { "added" };
    for key in &report.added {
        println!("{verb} {key}");
    }
    let verb = if dry_run { "would update" } else { "updated" };
    for key in &report.updated {
        println!("{verb} {key}");
    }
    let verb = if dry_run { "would remove" } else { "removed" };
    for key in &report.removed {
        println!("{verb} {key}");
    }
    for (key, reason) in &report.flagged {
        println!("flagged {key}: {reason}");
    }
    for (key, err) in &report.errors {
        eprintln!("error syncing {key}: {err}");
    }
    if report.added.is_empty()
        && report.updated.is_empty()
        && report.removed.is_empty()
        && report.flagged.is_empty()
        && report.errors.is_empty()
    {
        println!("Nothing to do.");
    }
    if !report.errors.is_empty() {
        std::process::exit(1);
    }
}

async fn list(
    paths: &Paths,
    config: &Config,
    config_path: &Path,
    json: bool,
    all: bool,
    plain: bool,
) -> Result<()> {
    if json {
        let state = State::load(&paths.state_file())?;
        let entries: Vec<&ReviewEntry> = state.iter().filter(|e| all || e.in_queue).collect();
        println!("{}", serde_json::to_string_pretty(&entries)?);
        return Ok(());
    }

    if !plain && std::io::stdout().is_terminal() {
        let sources = build_sources(config).await?;
        return tui::run(paths.clone(), config.clone(), config_path.to_path_buf(), sources, all);
    }

    let state = State::load(&paths.state_file())?;
    let entries: Vec<&ReviewEntry> = state.iter().filter(|e| all || e.in_queue).collect();
    if entries.is_empty() {
        println!("No reviews tracked yet. Run `rq sync` first.");
        return Ok(());
    }

    let mut table = Table::new();
    table.load_preset(UTF8_FULL_CONDENSED);
    table.set_header(vec!["KEY", "TITLE", "AUTHOR", "STATUS", "PATH"]);
    for e in entries {
        let (status, path) = match &e.workspace {
            Some(ws) => (
                format!("{:?}", ws.status),
                ws.workspace_path.display().to_string(),
            ),
            None => ("not fetched".to_string(), "-".to_string()),
        };
        table.add_row(vec![
            e.key.slug(),
            e.title.clone(),
            e.author.clone(),
            status,
            path,
        ]);
    }
    println!("{table}");
    Ok(())
}

fn path(paths: &Paths, id: &str) -> Result<()> {
    let state = State::load(&paths.state_file())?;
    let matches = state.find_by_prefix(id);
    match matches.as_slice() {
        [] => bail!("no tracked review matches `{id}`"),
        [entry] => match &entry.workspace {
            Some(ws) => {
                println!("{}", ws.workspace_path.display());
                Ok(())
            }
            None => bail!(
                "`{id}` has no local workspace yet; run `rq fetch {id}` (or press the fetch key in `rq list`)"
            ),
        },
        many => {
            let keys: Vec<_> = many.iter().map(|e| e.key.slug()).collect();
            bail!("`{id}` matches multiple reviews: {}", keys.join(", "))
        }
    }
}
