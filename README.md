# TScope

A terminal multiplexer with context-aware side panels. Splits the screen into
side-by-side shell panes and, for the focused pane, shows live information
about whatever it's currently doing — an SSH session, a Claude Code session,
or a long-running service listening on a port.

Built with [ratatui](https://ratatui.rs) and
[alacritty_terminal](https://crates.io/crates/alacritty_terminal); panes are
real PTYs, not line-buffered subprocesses.

## What it shows

The focused pane gets a context panel above the status line, populated based
on what's running inside it:

- **Claude Code session** — current topic, turn count, last user/assistant
  message, the tool currently in flight (with its target file/command), a
  rolling window of recent tool calls, and the session's git branch. Driven
  by tailing the live JSONL transcript on disk.
- **SSH session** — user, host, resolved IP, when the connection started,
  the remote command (if any), and the most recent line of output. SSH
  invocations get a user-editable alias persisted to the config file.
- **Service** — a process listening on TCP ports gets its PID, port list,
  RSS / virtual memory, CPU%, and the full command line. Discovered via
  `lsof` and sampled with `libproc` on macOS.

When none of those apply, the context panel collapses and the pane uses the
full height.

## Per-pane settings

Each pane can have a custom display name and accent color, keyed by the
pane's initial working directory and persisted to
`~/.config/tscope/config.toml`. Open a pane in the same directory again
and the name/color come back automatically.

## Key bindings

All commands go through a `Ctrl-a` prefix (tmux-style). Press `Ctrl-a`,
release, then press the command key.

| Keys             | Action                                              |
| ---------------- | --------------------------------------------------- |
| `Ctrl-a` `n`     | **new** — spawn a new shell pane                    |
| `Ctrl-a` `x`     | **close** — close the focused pane (last one quits) |
| `Ctrl-a` `h` / `←` | **switch** focus to the previous pane             |
| `Ctrl-a` `l` / `→` | **switch** focus to the next pane                 |
| `Ctrl-a` `1`–`9` | **jump** directly to pane N                         |
| `Ctrl-a` `r`     | **rename** the focused pane's SSH alias             |
| `Ctrl-a` `s`     | open **settings** (name + accent color) for the pane |
| `Ctrl-a` `[`     | enter **copy mode** on the focused pane             |
| `Ctrl-a` `q`     | **quit** TermiUS                                    |
| `Ctrl-a` `a`     | send a literal `Ctrl-a` through to the pane         |
| `Ctrl-q`         | emergency quit (no prefix needed)                   |

Inside the **rename** prompt: type the alias, `Enter` to save (empty clears
it), `Esc` to cancel.

Inside the **settings** modal: `Tab` switches between the Name and Color
fields. On the color field, `←`/`→` or `h`/`l` cycles through the palette.
`Enter` saves, `Esc` cancels.

Anything else is forwarded to the focused pane's PTY, so vim, less, ssh
prompts, etc. all work normally.

## Scrolling and copy mode

Each pane has its own scrollback buffer (10,000 lines). Scroll it with the
**mouse wheel** while hovering over the pane — the other panes stay put.

**Copy mode** (`Ctrl-a [`) is a keyboard-driven selection mode scoped to
the focused pane. The yank goes to the system clipboard via OSC 52, so it
works over SSH as long as the outer terminal supports it (iTerm2, kitty,
WezTerm, Alacritty, recent Terminal.app).

| Keys              | Action                                    |
| ----------------- | ----------------------------------------- |
| `h` `j` `k` `l` / arrows | move the cursor                    |
| `0` / `Home`      | jump to start of line                     |
| `$` / `End`       | jump to end of line                       |
| `g` / `G`         | top / bottom of the viewport              |
| `v` / `Space`     | start (or clear) a selection at cursor    |
| `y` / `Enter`     | yank the selection to the clipboard       |
| `Esc` / `q`       | exit copy mode                            |

If you want to select content that's already scrolled off, wheel-scroll
the pane first, then enter copy mode. Mouse wheel is disabled inside copy
mode so the cursor doesn't drift against content you didn't move.

## Config

Stored at `~/.config/tscope/config.toml`:

```toml
[ssh_aliases]
"me@prod-db-1" = "prod db"

[pane_aliases."/Users/me/code/widget-api"]
name = "widget"
color = "magenta"
```

`ssh_aliases` is keyed by `user@host` (or just `host`) and used to label
SSH panes. `pane_aliases` is keyed by absolute working directory.

## Installation

### Prebuilt binaries (recommended)

Releases are built for macOS (Apple Silicon + Intel) and Linux (x86_64 +
aarch64).

```sh
curl --proto '=https' --tlsv1.2 -LsSf https://github.com/TrejoMF/tscope/releases/latest/download/tscope-installer.sh | sh
```

The installer drops the `tscope` binary into a directory on your `PATH`
(e.g. `~/.local/bin` on Unix). Pin a specific version by replacing
`latest/download` with `download/vX.Y.Z`.

### From crates.io

```sh
cargo install tscope
```

### From source

```sh
git clone https://github.com/TrejoMF/tscope.git
cd tscope
cargo install --path .
```

Or without cloning:

```sh
cargo install --git https://github.com/TrejoMF/tscope.git
```

## Run

After install:

```sh
tscope
```

Or from a checkout:

```sh
cargo run --release
```

Building from source requires Rust 2024 edition (install via
[rustup](https://rustup.rs)). Service detection (port/CPU/memory) is
macOS-only — other platforms still get the multiplexer, SSH context, and
Claude context.
