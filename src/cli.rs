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
    /// Fetch review queues and create/update/remove workspaces.
    Sync {
        /// Only sync the named source (as configured, e.g. "moz" or "github").
        #[arg(long)]
        source: Option<String>,
        #[arg(long)]
        dry_run: bool,
    },
    /// List tracked reviews.
    List {
        #[arg(long)]
        json: bool,
        /// Also show reviews that are out of your queue but not yet resolved.
        #[arg(long)]
        all: bool,
    },
    /// Print a review's workspace path, e.g. `cd $(rq path D12345)`.
    Path {
        /// A review id, or unique prefix of one (e.g. "D123" or "moz/D123").
        id: String,
    },
    /// Remove workspaces for resolved reviews.
    Prune {
        /// Also remove dirty workspaces.
        #[arg(long)]
        force: bool,
        /// Remove specific reviews' workspaces even if still open.
        ids: Vec<String>,
    },
    /// Check auth and tool availability for each configured source.
    Doctor,
    /// Manage canonical repos.
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
    /// Register a user-owned canonical repo.
    Add { url: String, path: PathBuf },
    /// Delete a tool-managed clone (refuses if it still has workspaces).
    Rm { url: String },
}

#[derive(Debug, Clone, clap::ValueEnum)]
pub enum Shell {
    Bash,
    Zsh,
    Fish,
}
