# Harness checkpoint — 2026-09-05

Read this before resuming work on another machine. The product branch is
`harness/main` in `sumeet/codex-harness`, not the older default `main` branch.
The development checkout's `checkpoint` remote is that private repository;
its `origin` points to upstream Zed. Never confuse those push destinations.

## Start on another machine

```sh
git clone --branch harness/main git@github.com:sumeet/codex-harness.git
cd codex-harness
HARNESS_BUILD_JOBS=1 ./script/run-standalone.sh
```

Install the pinned Rust toolchain, Zed's Linux build prerequisites, CMake,
the fonts you want, and an authenticated Codex CLI. Latest CLI verified for
this checkpoint: **0.153.4**. Builds default to `target/release-fast/harness`.
Use one Cargo worker: previous parallel builds exhausted memory and killed
interactive applications. Do not run several Rust builds concurrently.

The repository carries implementation and design/research context, not your
live sessions or credentials. Authenticate Codex separately. Harness's
preferences/themes, drafts/attachments, selected session, and transcript
caches are local application state, not Git data. Likewise, ChatGPT needs a
working browser runtime and authenticated session on the new machine. Do not
put cookies, `.codex` logs/auth, browser profiles, or private transcripts in Git.

## Current product and priorities

Harness is a compact native GPUI client with a shared rich/Vim transcript and
real modal composer. Codex uses App Server; ChatGPT has a separate transport
and catalog but reuses transcript/editor surfaces. Claude is **not integrated**.
The old `</>` raw-text view, `--text`, and text-view aliases are removed.

The user wants excellent aesthetics without sacrificing useful information:

- Preserve their own fonts, especially condensed faces. Do not substitute
  another font to make an experiment look better. Compare at identical theme,
  reading/code family, size, weight, viewport, and physical display scale.
- Regular/400 previously felt too bold. Background contrast and subpixel vs
  grayscale are separate hypotheses, not proven universal explanations.
- Mixed-size inline code now uses the actual configured code font size. This
  improvement was explicitly accepted. Do not regress the corresponding
  wrapping, hit-testing, or Vim geometry.
- Compact tools should not become repeated heavy cards. No decorative left
  rails. Horizontal scrolling works without a scrollbar overlay obscuring the
  last output line. Failures retain their dotted boundary and useful output.
- Transcript, hidden Vim editor, and composer share a comfortable gutter:
  18 logical px wide, 10 narrow. Removing tool-local padding is not permission
  to shrink the whole transcript gutter. Inspect real windows after changes.
- Current tool rhythm is Balanced: 4 px total between collapsed rows, 10 px
  at prose/tool boundaries, unchanged prose paragraph/font metrics. See the
  archived comparisons below; this is a targeted composition change, not a
  wholesale Delta renderer transplant.

Keyboard: `Ctrl-Shift-S` toggles the sidebar globally in Harness. Composer
`Ctrl-V` pastes only in Insert mode/Vim-off; `Ctrl-Shift-V` or `Shift-Insert`
explicitly pastes in any composer mode. Native Vim yanks remain in registers
unless `+`/`*` is requested. Escape dismisses local Vim/overlay state first,
then stops Codex (`Esc Esc` from Insert).

## This checkpoint's stop/continue change

Implementation checkpoint: `c9339f330c` (also includes the previously accepted
keyboard, gutter, tool-scrollbar, and rhythm refinements).

Ordinary interruption is a single muted, selectable **Stopped** line. It has
no eye icon, warning badge, box, disclosure, or explanatory boilerplate.
Older saved stop records normalize to the same presentation. Real failures
and interruption records carrying error details retain their information.

The latest idle stop can offer **Continue** once Codex is attached and ready.
It calls `turn/start` with `input: []`: a new model generation from existing
context, not a resumed process and not an injected user message. Draft text
and images are untouched. No automatic resubmit. Busy/queued/restoring,
read-only, approval, and settings-update states prevent the action. A later
turn retires an old Continue action even if that later turn emits no items.
Callbacks are generation/thread guarded and do not resurrect turns whose
lifecycle notifications already arrived.

Validation command:

```sh
cargo test -j1 -p harness_app -p harness_editor -p harness_protocol -p codex_app_server_client --quiet
HARNESS_BUILD_JOBS=1 ./script/build-standalone.sh
```

415 tests passed (217 app, 58 editor, 116 protocol, 24 client); one app network
test is intentionally ignored. Native fixture QA covers the quiet marker and
real error card with the current theme/fonts. Continue's empty-input request
and lifecycle logic are covered without submitting work to the real account;
the first user-triggered live Continue remains manual acceptance.

## Reference-client evidence and waiting behavior

Inspected exact Codex tag `rust-v0.153.4` and official Linux desktop package
**26.901.51231**, downloaded from the official `latest/chatgpt_amd64.deb`
endpoint. The already installed desktop was older, **26.825.51511**. Inspection
did not install, launch, authenticate, or restart either client.

- CLI `tui/src/status_indicator_widget.rs` has Working/activity text plus
  elapsed time. `chatwidget/streaming.rs` uses backend retry messages; daemon
  reconnect is handled separately in `chatwidget/reconnect.rs`.
- CLI `chatwidget/input_restore.rs` retains an interruption notice. It is
  terminal text, not a requirement to render an expanded GUI error card.
- Desktop `local-conversation-turn-68e11fb7f7fc.js` maps a locally interrupted
  turn to a `worked-for` item with `status: stopped`.
  `subagent-activity-chip-group-f888d636e4fc.js` supplies stopped activity
  summaries; `app-initial-9e28b0395ba3.js` handles `willRetry` stream errors as
  reconnect messages. `local-conversation-thread-250162591057.js` separately
  shows loading/reconnecting-to-client status above the composer.
- Backend integration test
  [turn_start_with_empty_input_runs_model_request](https://github.com/openai/codex/blob/rust-v0.153.4/codex-rs/app-server/tests/suite/v2/turn_start.rs)
  verifies a model request with no synthesized user message. This is stronger
  evidence for Continue than guessing from a desktop button label. The
  desktop's exact general-purpose Continue interaction was not established.
- [App Server documentation](https://learn.chatgpt.com/docs/app-server)
  distinguishes reopening a thread from starting/interruption of turns.

No speculative sending/accepted/receiving overhaul was added. Harness already
shows actual transport-retry status near activity. A prior silent turn reached
the daemon/model request, produced no assistant items for about 4m11s, and was
interrupted. A new connection then replied in about 9s. A stale connection is
plausible, not proven; neither effective fast-tier service nor the upstream
root cause was established. Future elapsed-silence diagnostics should be
truthful and never auto-resubmit possibly accepted work.

## Durable research and remaining work

- [Comparison viewer and fixtures](research/interface-comparison/README.md):
  archived native Harness/Zed/Delta/web screenshots, the controlled Harness
  background/rasterization profiles, and the newer tool-rhythm experiments.
  These are historical evidence, not screenshots of the current build.
- [Terminal output investigation](crates/harness_app/terminal-output-investigation.md):
  ANSI-only rendering cannot restore color the executor suppresses. Preserve
  one execution, sandbox/approvals, cancellation, and model-readable output.
  Execution-server or upstream output-policy work is investigation, not shipped.
- Separate `harness/claude-acp` branch: **manifest scaffolding only**, based on
  older commit `0ef7866b7b`, scaffold checkpoint `272b6450d9`; no implementation
  source, no working provider.
  It is intentionally not merged. See that branch's `CLAUDE_ACP_WIP.md`.
  The user's goal is authentic Claude session continuation and the same
  composer/tools/Vim surfaces, not an assumed equivalent restricted SDK path.
- ChatGPT portability, browser discovery/packaging, live restore/reconnect
  endurance, and long-history interaction need continued acceptance testing.
- `DESIGN.md` and `PRODUCT_BACKLOG.md` contain older aspirations, including a
  durable task ledger. Do not treat every statement there as shipped behavior.

Other old local worktrees (scroll, typography, upstream integration, fast-dev)
are experiments, not additional product branches to merge blindly. Their
existence does not imply unfinished changes belong in this checkpoint. Large
downloaded binaries, reverse-engineering scratch space, signed-in browser
profiles, and raw captures outside the curated viewer were not copied.

Do not restart the user's live daemon/app, send fixture prompts to a real
account, change authentication/configuration, or use focus-stealing GUI tests
as incidental validation. Use an isolated fixture process and one build at a
time. Repeated green unit tests are not a substitute for the user's reported
Vim geometry or visual acceptance.
