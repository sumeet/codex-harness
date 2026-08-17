# Codex Harness

Codex Harness is a standalone, keyboard-first native client for
`codex app-server`. It reuses Zed's real GPUI `Editor`, Vim implementation,
theme tokens, and icon system without booting Zed's workspace, project UI,
tabs, login shell, or IDE chrome.

The product target is one full-width transcript that looks like a rich GUI
timeline while behaving like a native Vim buffer: character motions, visual
selection, yank, search, folds, fast scrolling, and a real modal composer below
the history.

This is an active development checkpoint, not a finished release. The current
tree includes the standalone app, direct App Server protocol client, segmented
incremental transcript model, rich Editor decorations and supplemental views,
and the ongoing extraction of Zed's command-palette behavior.

## Build

Install the normal Zed Linux build prerequisites, Rust through `rustup`, and a
recent CMake. The repository pins its Rust toolchain. The `codex` executable
must be installed and authenticated because Harness starts `codex app-server`
locally.

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

Normal streaming updates edit only dirty transcript items. Rich headers, diff
styling, and search highlights are viewport-bounded; underlying message text
remains real selectable Buffer text. Interactive approvals, permissions,
request-user-input forms, MCP forms, and image previews mount as supplemental
Editor blocks and share the transcript's single scrollbar.

## Honest status

The Editor-backed transcript is the migration target, but the old rich list is
still temporarily reachable as a comparison fixture. Before deleting it we are
validating streaming follow-tail behavior, mouse/keyboard focus transitions,
image parity, and the always-visible composer in a real tall window. Native
folds, final `:` palette UI, additional media surfaces, settings controls, and
live/reopen endurance testing remain active work.

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

