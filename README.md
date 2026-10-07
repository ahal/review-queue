# review-queue (`rq`)

Lists the code reviews waiting on you from GitHub and Mozilla Phabricator, and checks them
out as git worktrees or jj workspaces.

## Install

```sh
cargo install review-queue
```

## Usage

```
rq                      # same as `rq show`
rq show [--all] [--plain] [--json]
rq fetch <id>           # create a workspace for a review
rq sync [--source gh|phab] [--dry-run]
rq path <id>            # print a review's workspace path
rq repo list|rm <url>   # manage canonical repos
rq shell-init <bash|zsh|fish>
```

Ids look like `moz/D12345` or `gh/owner/repo/42`. A unique prefix (`D123`) is enough.

`rq show` opens a TUI on a terminal and prints a table otherwise. Keys: `j`/`k` or arrows to
move, `enter` to open the review locally in a subshell, `o` for the browser, `d` to delete
its workspace, `q` to quit.

`rq sync` updates existing workspaces and removes them once their review resolves. It never
creates one. Use `rq fetch` or `enter` in the TUI for that.

`rq shell-init <shell>` prints an `rqcd` function that picks a review with `fzf` and `cd`s
into its workspace (needs `jq` and `fzf`):

```sh
eval "$(rq shell-init zsh)"
```

## Configuration

`$XDG_CONFIG_HOME/review-queue/config.toml`, or pass `--config`. All keys are optional.

```toml
# Existing checkouts under here are used as canonical repos instead of cloning.
workdir = "~/dev"
# auto_clone = true         # clone without asking when no checkout is found
# data_dir = "~/.local/share/review-queue"

# Replaces $SHELL when opening a review. Runs via `sh -c` in the workspace with
# RQ_REVIEW, RQ_SOURCE, RQ_ID and RQ_WORKSPACE set.
# open_command = "nvim +DiffviewOpen"
# open_command_wait = true  # false for commands that return immediately, e.g. a tmux switch

[source.github]
# api_url = "https://github.example.com/api/v3"   # GitHub Enterprise
# token = "..."
# token_cmd = "pass show github/token"
ignore_repos = ["mozilla/some-noisy-repo"]
# ignore_authors = []
# ignore_teams = []
# include_drafts = false

[source.moz-phab]
url = "https://phabricator.services.mozilla.com"
# token = "..."
# token_cmd = "..."
# include_groups = true
```

GitHub tokens are tried in this order: `token`, `token_cmd`, `$GITHUB_TOKEN`, `gh auth token`.
The Phabricator source needs [`moz-phab`](https://github.com/mozilla-conduit/review) installed.

State, managed clones and workspaces live in `$XDG_DATA_HOME/review-queue/`.

## License

MPL-2.0
