# dch

A terminal-based agentic coding assistant written in Rust, built on the
[`loopctl`](https://crates.io/crates/loopctl) agent-loop library.

## Features

- **Interactive TUI** — live streaming output, Markdown rendering with
  syntax highlighting, configurable themes.
- **Single-run mode** — run one task non-interactively (as an argument
  or piped on stdin), exit with a meaningful shell status, optionally
  write a JSON completion report for CI and orchestration.
- **Built-in tools** — file reads and paginated viewing, single and
  atomic multi-file edits, writes behind a linter gate, `bash` with
  timeouts and background jobs, `glob` / `grep` / `tree` /
  code-search, and per-session todos.
- **Providers** — Ollama (local, the default), OpenAI, Anthropic,
  Google Gemini, AWS Bedrock, DeepSeek, Grok, Azure OpenAI,
  Moonshot, Z.AI; any OpenAI-compatible endpoint works via
  `base_url`. An optional
  `fallback_model` takes over when the primary fails repeatedly.
- **MCP** — external tool servers configured as `[[mcp.servers]]` are
  connected over stdio at startup and join the tool registry.
- **Roles** — `general` (default), `coding`, `refactor`, `debug`,
  `review`, `docs`, `tests`; each tunes the system prompt, set in
  config without rebuilding.
- **Containment** — file tools are confined to the working directory
  unless `--unsafe-paths` is given for the run.

## Workspace layout

```text
crates/
├── dch/         # Binary crate — CLI entrypoint, TUI/single-run dispatch, signals
├── dch-loop/    # Agent-loop wiring: runner, provider, prompt, MCP, console observer
├── dch-tools/   # Tool implementations (read, write, edit, bash, grep, todo, …)
├── dch-tui/     # Terminal UI (ratatui) — app loop, markdown pipeline, theming
└── dch-config/  # Configuration loading (TOML)
xtask/           # Workspace automation (crate-boundary checks)
```

The dependency graph is strictly layered and enforced in CI
(`make boundary`): `dch-tools` depends on no workspace crate,
`dch-tui` on `dch-config` only, `dch-loop` on `dch-config` +
`dch-tools`, and the `dch` binary on all three.

## Installation

Requires Rust **1.98** (edition 2024).

```bash
git clone https://github.com/dch-labs/dch
cd dch
cargo build --release
target/release/dch                 # interactive TUI
target/release/dch "say hello"     # single task, non-interactive
```

## Usage

Interactive session (a terminal with no task argument and no piped
stdin):

```bash
dch
```

Single task, non-interactive — the result goes to stdout:

```bash
dch "fix the failing test in crates/dch-tools"
echo "summarize this repo" | dch
```

A `TASK` argument selects single-run mode; with no argument, a stdin
that is not a terminal selects it too. Only a bare invocation on a
terminal launches the TUI. A `TASK` composes with `--resume` (continue
a saved session headless); neither combines with `--list-sessions`.

Resume a saved session — the restored conversation seeds the display
and the agent's context, further auto-saves keep writing the same
session file, and the saved model applies unless `--model` overrides
it:

```bash
dch --resume 01234567-89ab-cdef-0123-456789abcdef   # in the TUI
dch "keep going" --resume 01234567-89ab-cdef-0123-456789abcdef
```

List saved sessions (newest first) and exit:

```bash
dch --list-sessions
```

A missing session id warns and starts fresh; a corrupt file warns
loudly, starts fresh, and is never modified; a session file that
cannot be read at all exits non-zero. Files the restored transcript
shows being read are guarded for writes, but the first `Write` to one
requires a fresh `Read` — the transcript cannot carry what the model
originally saw, so a stale overwrite of externally-changed files is
refused until the current bytes are read.

Common options:

| Flag | Description |
| --- | --- |
| `-m, --model MODEL` | Override the configured model for this run |
| `--resume SESSION_ID` | Resume a saved session by id |
| `--list-sessions` | Print saved sessions and exit |
| `--theme NAME` | Override the configured theme |
| `--config PATH` | Use an alternate config file |
| `--unsafe-paths` | Let file tools reach paths outside the working directory |
| `--done-file PATH` | Write a JSON completion status to PATH (single-run mode) |
| `-v, --verbose` / `-q, --quiet` | Adjust verbosity |

Themes: `dracula` (default), `nord`, `tokyo_night`, `gruvbox_dark`,
`gruvbox_light`, `solarized_dark`, `solarized_light`,
`catppuccin_latte`, `catppuccin_frappe`, `catppuccin_macchiato`,
`catppuccin_mocha`, `one_dark`, `monokai`, `github_dark`,
`github_light`, `ayu_dark`, `rose_pine`, `kanagawa_wave`, `dark_plus`.
An unknown name falls back to the default.

### Exit codes and the done-file

Single-run mode exits `0` on success, `1` on a construction or usage
failure, `2` when the run itself fails, and `130` when cancelled by an
interrupt. With `--done-file PATH`, a JSON status (`success`,
`message`, `turns`, `tools_used`) is written on every terminal path —
including cancellation — so a polling orchestrator never hangs.

### Keys

Input (always insert mode):

| Key | Action |
| --- | --- |
| `Enter`, `Ctrl+M` | Submit (Ctrl+M is the same byte as Enter on legacy terminals; enhanced ones report the modifier, and both submit) |
| `Shift+Enter`, `Ctrl+J`, or `\`+`Enter` | Newline (Shift+Enter on terminals with key-modifier reporting — kitty, wezterm, foot, ghostty, alacritty, xterm, and recent gnome-terminal; macOS Terminal.app reports no modifiers, where Ctrl+J and the backslash form are the newline keys) |
| `Up` / `Down` | History recall; cursor motion in a multi-line buffer |
| `Ctrl-P` / `Ctrl-N` | History recall (always) |
| `Left` / `Right`, `Home` / `End`, `Ctrl-A` / `Ctrl-E` | Move by character; to line start/end |
| `Alt+Left` / `Alt+Right`, `Ctrl-W` | Move / delete by word |
| `Ctrl-U` / `Ctrl-K` | Delete to line start / end |
| `Tab` | Indent (four spaces) |
| paste | Inserts whole, newlines included — never submits |

Conversation:

| Key | Action |
| --- | --- |
| `Up` / `Down` | Scroll one line (only while the input is empty with nothing recallable) |
| `PageUp` / `PageDown` | Scroll ten lines |
| mouse wheel | Scroll three lines per event |
| `End` | Snap to the newest line (only while the input is empty) |

Anywhere:

| Key | Action |
| --- | --- |
| `F2` | Cycle tool-line verbosity (Quiet → Normal → Verbose; initial mode from `display.verbosity`) |
| `Esc`, `Ctrl-C`, `Ctrl-D` | Quit (cancels the in-flight run; queued submissions are not started) |

## Configuration

Configuration lives in `~/.dch/config.toml`; a
`~/.dch/config.local.toml`, when present, overrides it field by
field. Every field has a default — an unconfigured `dch` works
against a local Ollama server.

```toml
[api]
api_type = "ollama"               # ollama | openai | anthropic | gemini | deepseek |
                                  # grok | azure | moonshot | zai | bedrock
base_url = "http://localhost:11434/v1"   # OpenAI-compatible servers: include /v1
model = "qwen3.8:27b"
# api_key = "…"                   # optional; provider env vars also work
# fallback_model = "…"            # takes over when the primary fails repeatedly
# max_tokens = 32000              # output cap per model response
# context_window = 200000         # window used for accounting and compaction

[runner]
max_turns = 200
role = "general"                  # general | coding | refactor | debug | review | docs | tests

[display]
theme = "dracula"
verbosity = "normal"              # quiet | normal | verbose

# [[mcp.servers]]                 # external tool servers, connected over stdio
# name = "docs"
# command = "npx"                 # executable (resolved on PATH or absolute)
# args = ["-y", "@example/docs-server"]
```

See the `dch-config` crate docs (`make docs`) for the full schema.

## Development

Prefer the `Makefile` — every target maps 1:1 to a CI job:

```bash
make build       # cargo build --all-features
make check       # cargo check across the workspace
make test        # unit + integration + doctests
make clippy      # clippy --all-targets --all-features -- -D warnings
make fmt         # check formatting (CI-equivalent)
make lint        # auto-format (write)
make docs        # rustdoc with -D warnings
make boundary    # enforce crate-boundary rules (xtask)
make nodefault   # prove the workspace compiles without default features
make ci          # full local CI gate
make run ARGS="…"
```

The lint policy is deliberately strict: `clippy::pedantic` at warn
level with `-D warnings` in CI, and hard denials on `unwrap_used`,
`panic`, `todo`, `indexing_slicing`, arithmetic side effects, and
more. Errors are typed (`thiserror`); logs are structured
(`tracing`); every public API is documented (`cargo doc` runs with
`-D warnings`).

## Status

Pre-1.0. Working: the interactive TUI with multi-line input,
history, paste, and mouse-wheel scrolling, session auto-save (every
completed turn is persisted under `~/.dch/sessions/`), session resume
and listing (`--resume` / `--list-sessions`), single-run
mode with exit codes and done-files, the tool set above, MCP
attachment, roles, fallback models, signal handling. Not implemented
yet: permission prompting (tools run without
confirmation — treat `bash` accordingly).

## License

Dual-licensed under **MIT OR Apache-2.0**, at your choice.
