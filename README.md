# dch

A terminal-based agentic coding assistant written in Rust, built on the
[`loopctl`](https://crates.io/crates/loopctl) agent-loop library.

## Features

- **Interactive TUI** — live streaming output, Markdown rendering with
  syntax highlighting, configurable themes.
- **Single-run mode** — run one task non-interactively (as an argument
  or piped on stdin), exit with a meaningful shell status, optionally
  write a JSON completion report for CI and orchestration.
- **Built-in tools** — file reads and paginated viewing, batched
  multi-file reads (`ReadFiles`) under one byte budget, single and atomic
  multi-file edits, writes behind a linter gate, `bash` with
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
that is not a terminal selects it too. A bare invocation on a
terminal opens the fullscreen TUI. A `TASK` composes with `--resume`
or `--continue` (continue a saved session headless); neither combines
with `--list-sessions`.

Resume a saved session — the restored conversation seeds the display
and the agent's context (tool blocks carry their retained command
and output, so the model remembers what its tools returned),
further auto-saves keep writing the same session file, and the run
uses the current configuration's model and provider (the model the
session was saved under is provenance for `--list-sessions`;
`--model` overrides the configuration):

```bash
dch --resume 01234567-89ab-cdef-0123-456789abcdef   # in the TUI
dch "keep going" --resume 01234567-89ab-cdef-0123-456789abcdef
```

Continue the most recently saved session — the `--resume` id hunt,
skipped:

```bash
dch --continue
```

List saved sessions (newest first) and exit. The ten most recent
show by default; pass a count or `all` for more:

```bash
dch --list-sessions          # the 10 most recent
dch --list-sessions 30       # the 30 most recent
dch --list-sessions all      # everything
```

The MSGS column counts your submitted prompts — tool calls, replies,
error rows, and steered echo rows (a steer the run read, a hook's
reminder) all stay out. CONTEXT is the engine's context-size estimate
at the moment of the last save — the figure a running TUI's context
bar fills against and a headless run's save records from the loop
itself, and approximately the context a resume re-loads — not the
session's lifetime token spend.

A missing session id warns and starts fresh; a corrupt file warns
loudly, starts fresh, and is never modified; a session file that
cannot be read at all exits non-zero. Files the restored transcript
shows being read are guarded for writes, but the first `Write` to one
requires a fresh `read` — the transcript cannot carry what the model
originally saw, so a stale overwrite of externally-changed files is
refused until the current bytes are read.

Common options:

| Flag | Description |
| --- | --- |
| `-m, --model MODEL` | Run a configured model: with `[[models]]` entries, selects one along with its endpoint, protocol, and credential |
| `--resume SESSION_ID` | Resume a saved session by id |
| `--list-sessions` | Print saved sessions and exit |
| `--theme NAME` | Override the configured theme |
| `--config PATH` | Use an alternate config file |
| `--unsafe-paths` | Let file tools reach paths outside the working directory |
| `--reasoning-effort LEVEL` | Override the configured thinking budget for this run |
| `--no-redact` | Disable secrets redaction for this run |
| `--done-file PATH` | Write a JSON completion status to PATH (single-run mode) |
| `-v, --verbose` / `-q, --quiet` | Adjust verbosity |

Themes: `default` (near-black, derived from OpenCode's dark palette),
`transparent`, `dracula`, `nord`, `tokyo_night`,
`gruvbox_dark`, `gruvbox_light`, `solarized_dark`, `solarized_light`,
`catppuccin_latte`, `catppuccin_frappe`, `catppuccin_macchiato`,
`catppuccin_mocha`, `one_dark`, `monokai`, `github_dark`,
`github_light`, `ayu_dark`, `rose_pine`, `kanagawa_wave`, `dark_plus`,
`autumn_night`.
The transparent theme defers to the terminal's own colors, so the
terminal's background (transparency included) shows through; hairline
rules in the terminal's own dim tone mark the composer's top and
bottom, de-emphasized text (thinking, timestamps, system rows)
renders in a fixed mid gray so it greys out on every terminal, and
only the scrollbar and the success/warning/error indicators carry
colors of their own. An unknown name falls back to the default.

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
| `Shift+Enter`, `Ctrl+J`, or `\`+`Enter` | Newline (Shift+Enter on terminals with key-modifier reporting — kitty, wezterm, foot, ghostty, alacritty, xterm, and recent gnome-terminal; iTerm2 folds Shift+Enter into a plain one unless a profile key mapping sends `[13;2u` for it; macOS Terminal.app reports no modifiers at all — there Ctrl+J and the backslash form are the newline keys) |
| `Up` / `Down` | Move the caret through the composer's rendered rows; history recall from the top and bottom rows |
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
| Mouse wheel | Scroll the transcript, one line per event |
| Mouse drag | Select characters; the highlight stays after release and the text is copied to the clipboard. Dragging on the pane's top or bottom line scrolls the transcript with the selection |
| Mouse click (composer) | Place the caret where the press landed, clamped to the text |
| Mouse click (tool row) | Expand a tool call's block to its full command and output — pretty-printed input, then the (redacted) output; click any row of the open block to fold it back. Long output scrolls with the transcript. The last 256 tool calls stay expandable |
| `Shift` + arrows | Move the selection head — extend or shrink the current selection; the view follows and each step refreshes the clipboard copy |
| `Home` | Snap to the oldest line (only while the input is empty) |
| `End` | Snap to the newest line (only while the input is empty) |

The mouse and selection rows need `mouse_capture = true` (the
default): with capture off the terminal owns selection, a selection
can never start, the `Shift` + arrows rows do nothing, and the
click-to-expand rows below them have no path — expanding is
mouse-only.

Anywhere:

| Key | Action |
| --- | --- |
| `F2` | Cycle tool-line verbosity (Quiet → Normal → Verbose; initial mode from `display.verbosity`) |
| `Ctrl-C` `Ctrl-C` | While a run is in flight — through a permission ask too, which passes the chord but owns Esc — the double press is one action: the fire cancels the run exactly as `Esc Esc` cancels it (same stop, same `Agent cancelled` record) and terminates the program on the same press. The first press only arms and says so; a stray key between the presses stands the whole action down. Text in the composer clears first wherever it stands, run or not — clearing is not consenting, and a paste never feeds an exit it was not offered to — so the ladder is always clear, then arm, then fire. At rest, with an empty composer, first press arms, second press quits. The interrupted prompt waits again only when the exit's flush caught it before its run could record anything; a prompt with its `Agent cancelled` record beside it was stopped deliberately and stays stopped — the queue a resume opens with is the truth, never a phantom. The prompts still waiting when the exit fires were never sent at all, and do not vanish with the process: each is written into the transcript as an unclaimed row, and the resume opens with them waiting in the section again |
| `Esc` `Esc` | Cancel the run in flight — only that turn, and the prompts waiting behind it keep their place in the queue and run next (see [Waiting prompts](#waiting-prompts)). One press arms and says so on the notice row, the next fires, any other key between stands it down. Permission asks, questions, and the waiting section own Esc first — under them, close the sheet or release the section and the cancel is two presses away |
| `Ctrl-Shift-C` | Copy the selected transcript text (where the terminal reports the modifier) |
| `Ctrl-S` | Send the composer now: to the turn in flight as guidance when a run is running, as an ordinary prompt when one is not |
| `Ctrl-T` | Walk the plan section's postures — capped at four rows, the whole list, hidden. The plan keeps updating underneath |

While a permission prompt is on screen (see
[Permissions](#permissions)), the prompt owns the keyboard:

| Key | Action |
| --- | --- |
| `y` | Allow the pending tool call |
| `Enter` | Swallowed — a queued submit press never lands as an approval |
| `n` / `Esc` | Deny it |
| `Ctrl-C` | Serves the exit ladder as ever — a draft clears first; it cancels nothing, and the run's cancel is Esc's double press, which Esc here cannot serve (this sheet owns it) |

### Waiting prompts

A prompt submitted while a run holds the agent waits in the queue, and
the section above the composer shows the one the queue is standing on,
numbered `[2/3]` — where it stands in the queue out of how many wait.
The section stands whenever anything waits, not only under a run:
prompts a resume owes — the backlog the exit wrote into the transcript
because no turn ever spent it — wait with no run up, in the section,
readable and editable like any waiting prompt. They wait for you: none
of them runs on its own, so fix a wording with `e`, aim one at your
next run with `s`, or take one out with `d`.
Nothing enters the transcript until a prompt is actually sent — the
one exception is the exit's unclaimed rows above — the user
row a prompt gets is written at the moment its turn starts, with the
wording it goes out with.
So an edited prompt reads as its edit, and a dropped prompt never
appears at all. A cancelled turn is the other case again: its prompt
keeps the echo row written at its claim — the record of the words that
went out — and the prompts still waiting keep their place and run
next.

`Ctrl+Q` takes the keyboard from anywhere (except while a permission or
question sheet owns it) and gives it to the section:

| Key | Action |
| --- | --- |
| `Ctrl+Q` | Focus the waiting section — or, with a prompt open in the composer, save it and focus the section. Does nothing while nothing waits |
| `Left` / `Right` | Walk the waiting prompts, wrapping at both ends. The queue's order is untouched: the walk moves what is shown, not what runs next |
| `e` | Lift the shown prompt's text into the composer to edit it. Refused, with the reason on the notice row, while the composer holds a draft of its own |
| `Enter` | While a prompt is out for editing: save the text back into the prompt it came from, in its original slot, and clear the composer. A blank save is refused rather than emptying the prompt |
| `Esc` | With a prompt out for editing: discard the draft and leave the prompt exactly as it was. With the section focused: hand the keyboard back |
| `s` | Send the shown prompt to the turn already running, as guidance it reads at its next boundary — the prompt leaves the queue as it goes, so it cannot also run later. Asks first: `y` sends, any other key stands down. Refused, with the reason on the notice row, while nothing is running |
| `Delete` / `d` | Take the shown prompt out of the queue for good — it never runs. Asks first: `y` drops, any other key stands down. Dropping the last waiting prompt releases the focus |

The section swallows everything else while it holds the keys, so a letter
typed while reading the backlog reaches neither the composer nor the
prompt on screen. While the section holds the keys — or stands one of
its asks — the composer greys out: its edge column and text fall to
the dim register and its caret hides, saying where the keys are going
without a word of prose. The run's own chords pass through — `Ctrl-C` still
cancels the run in flight (the waiting prompts are untouched — they
run next),
`Ctrl-S` still steers it, and `PageUp`/`PageDown` still scroll the
transcript you are reading for context. Saving an edit does not touch
the input history `Up` recalls: that stays the list of prompts actually
submitted.

An edit saved into a prompt the run has already claimed has no slot left to
return to, so it goes out as a turn of its own — and the turn that went out
with the superseded wording is taken out of the conversation the model
reads, so the correction replaces that turn rather than answering it. That
cut is possible only while the superseded turn is the newest one: once a
later answer has been built on it, the correction runs beside the
conversation as it stands. Either way the transcript keeps what happened —
what is pruned is what the model is shown. And a correction that a cancel
catches before its turn never re-enters the waiting section — the driver
holds it and claims it as the very next turn, the section's own "this
runs next" made good by the hand that owns the ordering, because the
corrected words are the only copy of what was typed.

### The plan

While the run's todo list holds unfinished tasks, it stands pinned
above the composer — no key, no overlay: a `TODO` badge (the waiting
section's pill, over the secondary accent) with the done fraction
beside it (`2/5 done`), then one checkbox row per task (◐ the task in
progress and newly filed rows in the accent; ☑ finished rows uniform
in the dim register, the words struck through and the box plain), and
an ellipsis naming the tasks past the section's four
rows — and the ellipsis names the chord that lifts the cap: `Ctrl-T`
walks the section from capped to the whole list to hidden, the plan
updating underneath all three. Hidden keeps the badge row
standing in the section's place — the `TODO` pill with `2/9 · ctrl+t`
beside it — so a folded plan is out of sight without being out of
mind. It reads at a glance where the run is;
the transcript's own todo blocks hold every update in full for the
detail. The plan stays up after its run lands —
the final state, done or not, is there to read in the quiet after the
run — and the next prompt's claim retires it once every entry stands
completed. A run that ends with work still open carries its plan into the
next run instead, where the rows keep updating under the new prompt, so
the section always shows the plan the session actually stands on — never
one it has finished leaving behind. A steer delivers
mid-run without claiming, so the plan keeps updating through its own
run.

### Steering a running turn

`Enter` is patient. `Ctrl+S` is not: with a run in flight it hands the
composer's text to that run as guidance, and the run reads it at its next
turn boundary — never inside a request already sent or a tool call already
running, which is the same timing law the rest of the loop keeps. The
guidance joins the conversation the model continues, so it is still there
after the turn: the model reads it on the later turns too, and the session
saved to disk holds it.

A prompt already queued patiently can join the running turn too: focus the
waiting section (`Ctrl+Q`), walk to it (`Left`/`Right`), press `s` — it
leaves the queue and reaches the model inside the turn, which is what you
want when the turn has made the point urgent.

Two edges, both settled the same way — the words are the user's:

- **Nothing is running, or the run ended that instant.** The offer is
  refused and the text takes the ordinary path, queueing or opening a turn
  like any other submit. A chord never eats what was typed.
- **The run ends before it reads the guidance.** The intent to send still
  stands, so it is sent: the text returns to the waiting section with its
  turn promised again, joins the transcript when that turn claims it, and
  the notice row says why. Until its own turn comes it can still be edited
  (`e`) or taken out (`d`) like any other waiting prompt — and if the advice
  has gone stale with the turn it was meant for, `d` is one key.
- **The session exits before the run reads the guidance.** The steered row
  keeps the words visible in the transcript, but they do not re-enter the
  waiting section: guidance aimed at a turn that died with the exit is
  advice for a conversation that no longer stands, and resurrecting it to
  run on resume would put words into a session their author never aimed
  there. Lift them from the transcript and re-send them if you still want
  them.

A prompt lifted out of the waiting section for editing (`e`) is steered
from the composer like any other text, and its waiting slot is retired as
it goes — the same words do not both reach the model now and run again
later.

## Permissions

How aggressively the agent may act is a mode, set by
`[runner] permission_mode` (default `auto`) or per-run with
`--permission-mode <auto|plan|accept-edits|interactive>`:

| Mode | Behavior |
| --- | --- |
| `auto` | Runs everything without confirmation |
| `plan` | Read-only tools run; writes, shell, network, and meta tools are blocked |
| `accept-edits` | Reads and file edits run; shell, network, and meta tools prompt |
| `interactive` | Every tool prompts, reads included |

In the TUI a prompt renders as a sheet over the prompt box,
answered with `y`/`n` (see
the key tables above); a headless run denies whatever it cannot ask
about, so `plan` and `accept-edits` stay meaningful there too. Unknown
tool names fail closed: only `auto` runs them without confirmation —
every other mode asks or blocks.

## Configuration

Configuration lives in `~/.dch/config.toml`; a
`~/.dch/config.local.toml`, when present, is loaded instead of it —
a complete replacement, not a field-level merge. Every field has a
default — an unconfigured `dch` works
against a local Ollama server. [`example.config`](./example.config)
documents every option with its possible values and defaults.

```toml
[api]
api_type = "ollama"               # ollama | openai | anthropic | gemini | deepseek |
                                  # grok | azure | moonshot | zai | bedrock | self_hosted
base_url = "http://localhost:11434/v1"   # OpenAI-compatible servers: include /v1
model = "qwen3.8:27b"
# api_key = "…"                   # optional; provider env vars also work
# fallback_model = "…"            # takes over when the primary fails repeatedly
# max_tokens = 32000              # output cap per model response
# context_window = 200000         # unset = discover from the provider
#                                 # (per-request share on local servers;
#                                 #  falls back to 200000); explicit wins

# [[models]]                      # more models, each selectable with --model
# model = "glm-4.6"               # required; also the name --model selects
# base_url = "https://api.z.ai/api/anthropic"
# api_type = "anthropic"          # its own endpoint, protocol, and credential;
#                                 # everything else inherits from [api]

[runner]
max_turns = 200
turn_timeout_secs = 300            # per-model-call streaming budget; ≥120 s, and the first-token wait is separately capped at 120 s
role = "general"                  # general | coding | refactor | debug | review | docs | tests
# permission_mode = "auto"        # auto | plan | accept_edits | interactive (see Permissions)

[display]
theme = "default"
# diff_layout = "inline"          # how edit diffs render in the
                                  # conversation: inline stacks removals
                                  # above insertions as full-width solid
                                  # bands; side-by-side pairs each removed
                                  # line with its replacement. Narrow
                                  # terminals fall back to inline
# mouse_capture = true            # the wheel scrolls dch; false hands
                                  # selection back to the terminal
verbosity = "normal"              # quiet | normal | verbose

# [[mcp.servers]]                 # external tool servers, connected over stdio
# name = "docs"
# command = "npx"                 # executable (resolved on PATH or absolute)
# args = ["-y", "@example/docs-server"]
```

`[[models]]` entries are the models you switch between, one block each.
**The top entry is the default run** — reorder the list to change it —
and `dch --model NAME` picks any other by name, ignoring case, taking
that entry's endpoint, protocol, and credential with it. An entry states
what it needs and inherits the rest from `[api]`; name an optional
setting `""` to drop an inherited one. A name matching no entry stops at
startup and lists the configured ones. With no entries at all, `[api]`
is the whole configuration and `--model` just renames the model, as it
always has.

`mouse_capture = true` (the default) gives the app the wheel. Set it to
`false` to keep click-drag text selection with the terminal instead —
the two are mutually exclusive on terminals without a modifier bypass;
keyboard scrolling (PageUp/PageDown) still works either way.

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
attachment, roles, fallback models, permission gating (mode ×
category enforcement with the TUI approval overlay and
`--permission-mode`), signal handling.

## License

Dual-licensed under **MIT OR Apache-2.0**, at your choice.
