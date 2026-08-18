# Codex Harness

Codex Harness is a standalone, keyboard-first native client for
`codex app-server`. It reuses Zed's real GPUI `Editor`, Vim implementation,
theme tokens, and icon system without booting Zed's workspace, project UI,
tabs, login shell, or IDE chrome.

The product has two coordinated, full-width projections of one transcript:

- **Rich** is the default reading surface: proportional Markdown, Zed-native
  cards and icons, diff styling, images, tool activity, and interactive request
  controls.
- **Text** is a real Zed `Editor`/`Buffer` driven by Zed's Vim engine: character
  motions, text objects, visual selection, registers, yank, search, and fast
  keyboard navigation over the complete selectable history.

Both retain the same semantic item, use one history scrollbar at a time, and
keep a real modal composer below the history. Product acceptance is defined in
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
gh repo clone sumeet/codex-harness
cd codex-harness
./script/build-standalone.sh
./script/run-standalone.sh
```

Builds and launches are deliberately separate; launching never invokes Cargo:

```sh
./script/build-standalone.sh
./script/run-standalone.sh
```

The build wrapper serializes Rust compilation, refuses to build when less than
4 GiB of memory is available, and caps an individual compiler process. For an
optimized non-LTO test build:

```sh
HARNESS_PROFILE=release-fast ./script/build-standalone.sh
HARNESS_PROFILE=release-fast ./script/run-standalone.sh
```

Replay fixtures do not require a live App Server and are useful for UI QA:

```sh
./script/run-standalone.sh --replay 12
./script/run-standalone.sh --replay 10000
```

## Controls worth knowing

- `Ctrl-W H/J/K/L` moves between the task rail, composer, and transcript where
  the current focus makes that direction meaningful; `Ctrl-B` toggles the rail.
- `Ctrl-N` starts a fresh task. `Ctrl-Enter` sends from the composer.
- Rich uses `j`/`k`, `gg`/`G`, `/`, disclosures, and blockwise selection/yank.
  `Shift-V` enters Text at the same semantic item.
- Text is Zed's modal Editor. Its normal/visual motions, registers, yank,
  `/ ? n N`, `* # g* g#`, jumplist, and `:` palette operate on real Buffer
  text. `:rich`, `:text`, `:compose`, `:tasks`, `:new`, and `:stop` are Harness
  aliases. The status-bar `RICH`/`TEXT` control switches modes with the mouse.
- `Enter` on an interactive request in Text focuses its shared form/approval
  surface; `Escape` returns to the transcript.

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

Normal Text-mode streaming updates edit only dirty transcript items. Rich
headers, diff styling, and search highlights are viewport-bounded; underlying
message text remains real selectable Buffer text. Approvals, permissions,
request-user-input forms, MCP forms, and image previews are stable shared GPUI
entities: Rich renders them inline, while Text anchors the same entities as
supplemental Editor blocks. Neither projection introduces a nested vertical
history scrollbar.

## Honest status

Rich and Text are intentional product modes, not old/new implementations.
Current work is validating mode-to-mode semantic position and tail-follow
transfer, consolidating every interactive surface across both projections,
and polishing long diffs, tools, reasoning, images, and the always-visible
composer in real windows. Full Ex command semantics, settings controls, a fresh
Text-mode visual pass, and live-turn/request/reconnect endurance testing remain
active work.

The stripped standalone graph no longer pulls Zed Workspace, Search, Picker,
or command-palette UI through Vim. The fork still contains upstream Zed source
because Harness deliberately reuses and extracts its Editor/Vim behavior rather
than rewriting it.

## Validation

Fast focused checks:

```sh
CARGO_BUILD_JOBS=1 cargo test --offline -p harness_protocol
CARGO_BUILD_JOBS=1 cargo test --offline -p harness_editor
CARGO_BUILD_JOBS=1 cargo test --offline -p harness_app --bin harness
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
