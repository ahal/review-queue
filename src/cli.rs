use std::path::PathBuf;

use clap::{Parser, Subcommand};

#[derive(Parser, Debug)]
#[command(name = "rq", version, about = "Track and check out your code reviews")]
pub struct Cli {
    /// Path to config.toml (default: XDG config dir)
    #[arg(long, global = true)]
    pub config: Option<PathBuf>,

    #[command(subcommand)]
    pub command: Command,
}

#[derive(Subcommand, Debug)]
pub enum Command {
    /// Fetch review queues and update existing workspaces, removing one once its review resolves
    /// and is clean. Never creates a new workspace - use `rq fetch` (or the fetch hotkey in `rq
    /// list`) for that.
    Sync {
        /// Only sync the named source (as configured, e.g. "moz" or "github").
        #[arg(long)]
        source: Option<String>,
        #[arg(long)]
        dry_run: bool,
    },
    /// List tracked reviews. Opens an interactive TUI when stdout is a terminal (arrow keys or
    /// j/k to move, enter to open the review in your browser, `f` to fetch it locally, `q` to
    /// quit); prints a plain table otherwise, or with `--plain`.
    List {
        #[arg(long)]
        json: bool,
        /// Also show reviews that are out of your queue but not yet resolved.
        #[arg(long)]
        all: bool,
        /// Print a plain table instead of the interactive TUI, even on a terminal.
        #[arg(long)]
        plain: bool,
    },
    /// Print a review's workspace path, e.g. `cd $(rq path D12345)`.
    Path {
        /// A review id, or unique prefix of one (e.g. "D123" or "moz/D123").
        id: String,
    },
    /// Fetch a review locally: resolve its canonical repo (asking before cloning one, unless
    /// `auto_clone` is set) and create a worktree/workspace for it. `rq sync` never does this on
    /// its own; use this (or the fetch hotkey in `rq list`) for reviews that actually warrant
    /// checking out.
    Fetch {
        /// A review id, or unique prefix of one (e.g. "D123" or "moz/D123").
        id: String,
    },
    /// Manage canonical repos (discovered by scanning `workdir`, or tool-managed clones).
    Repo {
        #[command(subcommand)]
        command: RepoCommand,
    },
    /// Print a shell function (`rqcd`) for sourcing in your shell rc.
    ShellInit { shell: Shell },
}

#[derive(Subcommand, Debug)]
pub enum RepoCommand {
    /// List canonical repos and how many live workspaces each has.
    List,
    /// Delete a tool-managed clone (refuses for a discovered repo, or if it still has
    /// workspaces).
    Rm { url: String },
}

#[derive(Debug, Clone, clap::ValueEnum)]
pub enum Shell {
    Bash,
    Zsh,
    Fish,
}
