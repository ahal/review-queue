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

`rq show` opens a TUI on a terminal and prints a table otherwise. Use `j`/`k` or arrows to
move, Enter to choose an open target, `o` to open the browser, and `w` to fetch and open a
workspace in `$SHELL`. Use `h`/`l` or left/right to expand/collapse details, `f` to fetch,
`d` to delete a workspace (or cancel a fetch), `r` to reload, `?` for help, and `q`/Esc to quit.
In the target picker, arrows or `j`/`k` move the selection, Enter or `l` runs it, and Esc
closes the picker. Other character keys are ignored; target shortcuts only work in the main list.

`rq sync` updates existing workspaces and removes them once their review resolves. It never
creates one. Use `rq fetch` or a workspace target in the TUI for that. Choosing a target starts
fetching without a workspace confirmation; rq still asks before cloning a missing repository
unless `auto_clone` is enabled.

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

Open targets are configured by id. Browser and workspace are built in and only allow
`description` and `key` overrides. Their default keys are `o` and `w`; set `key = ""` to unbind either.
Custom targets require a command and may have a single character shortcut. The keys
`j k h l q d f r ?` are reserved; shortcuts must be unique. Enter always opens the picker,
ordered browser, workspace, then custom targets by id.

```toml
[targets.browser]
key = "o"

[targets.workspace]
key = "w"

[targets.diffview]
description = "Diffview"         # shown in the picker; defaults to the table id
command = "nvim +DiffviewOpen"   # run via sh -c
key = "v"                        # optional
workspace = true                 # default: fetch first, run in the workspace
wait = true                      # default: suspend TUI until exit; false detaches

[targets.url]
command = "echo $RQ_URL > /tmp/rq-url"
workspace = false                # run immediately in rq's cwd, without fetching
```

Commands receive `RQ_REVIEW` (`source/id`), `RQ_SOURCE`, `RQ_ID`, and `RQ_URL`.
`RQ_WORKSPACE` is set only for targets running in a workspace. Detached commands have null
stdin/stdout/stderr. The workspace built-in always waits for `$SHELL` to exit.
The old `open_command` and `open_command_wait` settings have been replaced by
`[targets.<id>]`; move the command and wait setting into a custom target.

GitHub tokens are tried in this order: `token`, `token_cmd`, `$GITHUB_TOKEN`, `gh auth token`.
The Phabricator source needs [`moz-phab`](https://github.com/mozilla-conduit/review) installed.

State, managed clones and workspaces live in `$XDG_DATA_HOME/review-queue/`.

## License

MPL-2.0
