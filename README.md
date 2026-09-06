# Codex Harness

Codex Harness is a standalone, keyboard-first native client for
`codex app-server`. It reuses Zed's real GPUI `Editor`, Vim implementation,
theme tokens, and icon system without booting Zed's workspace, project UI,
tabs, login shell, or IDE chrome.

The default transcript is a hybrid of Zed's reading and editing surfaces:

- **Rich paint** supplies proportional Markdown, Zed-native cards and icons,
  diff styling, images, tool activity, and interactive request controls.
- A persistent, input-only **Zed `Editor`/`Buffer`** is the transcript's source
  of truth for Vim state: character and line motions, Visual/Visual Line/Visual
  Block selection, registers, yank, search, and keyboard navigation. Its cursor
  and selections are projected onto the rich surface.

The old code-icon/raw transcript view has been removed. Rich paint and the
input-only Editor share one semantic document and one history scrollbar, with
a real modal composer below the history. Start with [HANDOFF.md](HANDOFF.md)
for the current checkpoint, known limitations, and cross-machine setup.
Product acceptance is defined in
[`docs/quality-gates.md`](docs/quality-gates.md); a compiling slice is not by
itself considered finished.

This is an active development checkpoint, not a finished release. The current
tree includes the standalone app, direct App Server protocol client, segmented
incremental transcript model, rich Editor decorations and supplemental views,
and a lightweight standalone host for the extracted Zed command-palette core.

## Build

Install the normal Zed Linux build prerequisites, Rust through `rustup`, and a
recent CMake. The repository pins its Rust toolchain. The `codex` executable
must be installed and authenticated because Harness starts `codex app-server`
locally.

Then clone the compact private checkpoint with an authenticated GitHub CLI and
build it from the repository root:

```sh
git clone --branch harness/main git@github.com:sumeet/codex-harness.git
cd codex-harness
HARNESS_BUILD_JOBS=1 ./script/run-standalone.sh
```

The normal launcher first asks Cargo for an incremental optimized build, then
detaches the GUI and writes its output to
`$XDG_STATE_HOME/harness/logs/harness.log` (or
`~/.local/state/harness/logs/harness.log`):

```sh
./script/run-standalone.sh
```

Cargo's freshness check is normally sub-second when the executable is current,
and prevents the launcher from silently exercising stale code. Set
`HARNESS_SKIP_BUILD=1` only when intentionally launching the last successful
artifact without checking the working tree.

The build wrapper refuses to build when less than 4 GiB of memory is available,
caps an individual compiler process, and uses eight Cargo workers by default.
Its default `release-fast` profile is broadly optimized without release LTO, so
the ordinary run command exercises the interactive build rather than debug
rendering performance:

```sh
./script/run-standalone.sh
```

For assertion-heavy development, opt into `harness-dev`. Only Harness's frame,
input, Editor, text-layout, and Linux GPU-submission path is optimized in that
profile; assertions, overflow checks, limited line information, backtraces, and
incremental compilation remain enabled:

```sh
HARNESS_PROFILE=dev ./script/run-standalone.sh
```

For debugging that intentionally keeps logs and process lifetime attached to
the terminal, opt into foreground mode:

```sh
HARNESS_FOREGROUND=1 ./script/run-standalone.sh
```

Set `HARNESS_BUILD_JOBS` to override the default worker count on a machine with
more or less available memory.

Replay fixtures do not require a live App Server and are useful for UI QA:

```sh
./script/run-standalone.sh --replay 12
./script/run-standalone.sh --replay 120
./script/run-standalone.sh --replay 10000
```

## Controls worth knowing

- `Ctrl-W H/J/K/L` moves between the thread rail, composer, and transcript where
  the current focus makes that direction meaningful; `Ctrl-Shift-S` toggles the
  rail globally in Harness (`Ctrl-B` remains an alias).
- `Ctrl-N` starts a fresh task. `Ctrl-Enter` sends from the composer.
- The rich transcript uses Zed's modal Editor for `j`/`k`, `gg`/`G`, motions,
  Visual (`v`), Visual Line (`V`), Visual Block (`Ctrl-V`), registers, yank,
  `/ ? n N`, and jumplist behavior. `z a` toggles the selected disclosure.
- `:compose`, `:tasks`, `:new`, `:stop`, and `:perf` are Harness aliases.
  `:perf` copies a delta performance report
  that distinguishes input arrival, input dispatch, input-to-present latency,
  and input-present cadence rather than adding permanent profiler chrome. The
  developer alias `:perf-j` runs 240 real Vim `j` inputs paced one per presented
  frame, then copies that run's report.
- In the composer, `Ctrl-V` pastes in Insert mode or with Vim disabled;
  Normal/Visual mode keeps Vim's visual-block behavior. `Ctrl-Shift-V` and
  `Shift-Insert` explicitly paste in all composer modes. Ordinary Vim yanks do
  not overwrite the system clipboard; use `"+y`, `"*y`, or explicit GUI copy.
- Escape lets Vim modes, forms, and overlays dismiss first, then stops active
  Codex work. From Insert mode this normally means `Esc Esc`.
- An idle, ready Codex thread ending in a stopped turn offers **Continue**.
  This starts a new generation with no new user message; it does not restart
  an interrupted process or send the draft.

The composer is a real plaintext Zed Editor with Markdown and fenced-language
syntax highlighting. Markdown punctuation remains visible and editable, and
Vim operations always address the exact source text; it is not a WYSIWYG field.
Colors, emphasis, and common Bash/Rust/JSON fence injections are enabled today.
The transcript Editor also supports proportional prose mixed with monospace
code and structured output. Those alternate font advances participate in its
real wrapping and hit-testing geometry; they are not unselectable UI fragments.

## Current architecture

- `crates/harness_app`: the standalone GPUI window and host-owned rich
  transcript surfaces.
- `crates/harness_protocol`: direct App Server projection, persistence,
  semantic request summaries, segmented document state, and fast protocol
  tests.
- `crates/harness_editor`: the application-facing boundary to Zed's real local
  `Editor` and Vim engine.
- `crates/codex_app_server_client`: App Server process/JSON-RPC client and safe
  infrastructure request handling.
- `crates/command_palette_core`: host-neutral Zed palette matching, history,
  interceptor merge, and confirmation behavior.

Streaming updates edit only dirty transcript items in the persistent Editor.
Rich headers, diff styling, selection paint, and search highlights are
viewport-bounded; underlying message text remains real selectable Buffer text.
Approvals, permissions,
request-user-input forms, MCP forms, and image previews are stable shared GPUI
entities rendered inline. ChatGPT conversations also use the shared rich/Vim
transcript; their transport and model catalog are provider-specific.

## Honest status

Rich paint backed by native Editor/Vim state is the primary product direction.
Current work is maintaining selection and hit-testing across every rich
renderer and
polishing long diffs, tools, reasoning, images, scrolling, and the always-visible
composer in real windows. Full Ex command semantics, settings controls,
live-turn/request endurance testing, and longer exploratory use across real
histories remain active work.

The stripped standalone graph no longer pulls Zed Workspace, Search, Picker,
or command-palette UI through Vim. The fork still contains upstream Zed source
because Harness deliberately reuses and extracts its Editor/Vim behavior rather
than rewriting it.

## Validation

Fast focused checks:

```sh
cargo test -j1 -p harness_app -p harness_editor -p harness_protocol -p codex_app_server_client --quiet
HARNESS_BUILD_JOBS=1 ./script/build-standalone.sh
```

Use `./script/clippy` for repository linting, following the upstream Zed
guidelines in `AGENTS.md`.

## Upstream

The implementation is GPL-compatible and derived from
[Zed](https://github.com/zed-industries/zed). The development checkout retains
the upstream history locally; the private checkpoint repository uses compact
snapshot history so other machines do not need to download all of Zed's Git
history. Add upstream when needed:

```sh
git remote add upstream https://github.com/zed-industries/zed.git
git fetch upstream --filter=blob:none
```
