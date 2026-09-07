# Harness checkpoint — 2026-09-07

Read this before resuming work on another machine. The product branch is
`harness/main` in `sumeet/codex-harness`, not the older default `main` branch.
This checkout's `origin` is `git@github.com:sumeet/codex-harness`, verified
September 7. Remote names differ across clones: inspect `git remote -v` before
pushing, and never send product work to upstream Zed by mistake.

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

## Checkpoint boundary and next task

This is a development checkpoint, not a claim of complete Claude/Codex parity.
The small sidebar cleanup is finished: ordinary Claude rows no longer expose
`Available` versus `Saved`; opening, working, and actionable failures remain.
Unsupported settings menus now wrap their explanation and explicitly say that
updating Harness alone does not update an already-running Claude connection.

**Next: safe upgrades for existing Claude connections.** Read-only snapshots of
all four reachable real workers on September 7 lacked `sessionSettings`. Their
startup preloads came from the older immutable package, even for a recently
assigned conversation; warm spares can outlive the frontend build that added
model/permission controls. A newer package on disk is not proof of a newer live
adapter. These workers were not reloaded, restarted, or sent test prompts.
The current lifecycle does not yet provide a verified user-facing upgrade for
them. Do not add an automatic force-respawn or an “update” button that merely
rewrites startup settings. Preserve native ownership, pending work, drafts, and
message identity; distinguish replacing stale standbys from migrating a live
worker. The existing `reload` experiment reloads its old module path and is not
a validated cross-package/preload upgrade. See `SUPERVISOR-AUDIT.md` in the native
lab for the tested restart/terminal-disconnection limits.

On another computer, authenticate the CLIs separately, build this branch, then
use Claude setup in Harness. The adapter is verified against the specific native
2.1.263 hash in `research/claude-native-lab/discover.mjs`; a different installed
build can be refused. Installed hook settings and active sessions are not part of
this Git checkpoint. The appearance lab's portable synthetic fixtures, captures,
and viewer are included; temporary private native-session evidence is not.

Final verification for this checkpoint: 317 Harness tests, 121 protocol tests,
and 27 App Server client tests passed (three Harness tests remained explicitly
ignored). The gated real-history recovery test also passed separately. All 37
offline adapter tests and four checkpoint-repair tests passed; eleven native/GUI
tests were gated off in the offline run. The explicit native settings workflow
then passed on an isolated 2.1.263 session with no model prompt or credentials
(`/tmp/hsettings-YCZevd`). The normal `target/release-fast/harness` was rebuilt at
16:10:28 Pacific with one Cargo worker and an 8 GiB process limit. The running user
app, real Claude workers, and display server were not restarted. Use Harness's
Relaunch prompt for the new frontend. Git whitespace checks and a credential-pattern
scan of the checkpoint files passed; private runtime history was excluded.

## Current product and priorities

September 7 follow-up to the transcript mismatch: the live Codex 0.153.4
paginated-history writer repeatedly rejected ordinal 14546 while its checkpoint
expected 14547 at rollout byte offset 327803904. The raw JSONL retained the
missing CLI turn and its 21:44:36 UTC final answer. A private database/transcript
copy reproduced the stale App Server projection; correcting only that checkpoint
ordinal let the native writer recover the missing turn's 234 items. Merely
reading or resuming an already loaded thread did not advance the repaired index;
the next normal writer event did, without restarting the App Server. Tests used
fixture-only item injection, never a real-session prompt or mutation.

`script/repair-codex-history.mjs` is a narrowly guarded, manual recovery tool,
not a generic ordinal-decrement retry. Its explicit plan requires the tested CLI
version, exact thread/checkpoint, SHA-256 of the unchanged rollout prefix, matching
session identity, and no materialized records overlapping the proposed ordinal.
Apply rechecks under a SQLite write transaction and saves the original checkpoint
in a private recovery record before changing that one value. It never edits JSONL
or launches Codex. The current plan is private:
`/tmp/harness-codex-history-repair-01a074e7.json`. After the user restored full
access and requested wrap-up, the guarded repair was applied to the live profile.
Its original checkpoint is backed up privately under `~/.codex/harness-history-recovery/`.
The existing writer caught up through ordinal 16003 without a restart, and the
missing CLI final answer was verified in the live index. Do not reapply this old
plan: its exact-checkpoint guard now rejects it. Relaunch/refresh Harness to
reconcile any still-open stale frontend cache; no raw conversation bytes were
rewritten by the repair.

Harness now checks the saved source's bounded, read-only tip against returned
App Server history before declaring catch-up complete. This is a freshness guard,
not another transcript renderer. A persisted verified-history anchor prevents a
newer live suffix from hiding an earlier history gap. Old caches without an anchor
reconcile from the beginning once; live notifications cannot advance the anchor.
Failed verification preserves cached/live messages and drafts and exposes an
incomplete-history error. Private evidence lives in
`/tmp/harness-history-probe.VH8f96`; do not commit its transcripts or databases.
The explicitly gated `recovered_codex_history_restores_the_real_cache_gap` test
feeds the recovered App Server response and the real copied cache through Harness's
transcript model and verifies the missing answer's order. The regular checks passed
315 Harness tests (three ignored, one sandbox-blocked socket test excluded),
121 protocol tests, 27 client tests, and all four repair-script tests.
The normal `target/release-fast/harness` was rebuilt at 15:57:21 Pacific with one
Cargo worker and the 8 GiB process limit. The user application, live Codex writer,
Claude workers, and display server were not restarted or modified.

September 7 follow-up: large Codex history replies could disconnect Harness's
managed WebSocket client. A read-only `thread/turns/list` request for the affected
conversation returned 21,898,547 bytes for four turns, exceeding tungstenite's
default 16 MiB frame limit. Frames now share the bounded 64 MiB message budget;
catch-up requests one turn per page. Reattach resets its retry budget only after
history reconciliation, not immediately after the thin live subscription. A
transport failure's exact reason now reaches pending requests and the log.
Transport tests reproduce the old 21 MiB failure, verify subsequent frames remain
readable after the fix, and retain rejection above the 64 MiB safety budget.

The Claude footer is provider-specific: native permissions, model, and supported
effort controls use the common button/menu primitives, not Codex settings RPCs.
On verified 2.1.263, the startup adapter imports native local model/effort command
handlers and the native validated permission transition. It does not patch the
binary, inject a prompt, or enable hosted Remote Control. Changes are session-only,
carry session/epoch and expected-value checks, and publish state to other clients.
Busy sessions and pending dialogs block these changes. Native policy refusals
remain errors; bypass is unavailable unless the session permits it. This new
`sessionSettings` capability requires a newly started adapter; existing workers
are not silently restarted or reloaded. Native `inherit` versus `default` effort
states must remain distinct (the latter is `/effort auto`).

Claude sidebar project labels share Codex's basename helper. Twenty-three verified
old disposable conversations are hidden by exact, profile-scoped UUIDs in the
user's private `harness-adapter/hidden-conversations.json`; no histories were
deleted. The sidebar can show hidden conversations again, and backend discovery
still includes them. New tests continue to use isolated profiles. Native
`away_summary` events render as proportional “While you were away” recaps with
the original payload preserved. Chat/Codex/Claude tabs share an “Open in New
Window” context menu. Native settings tests use a credential-free private TUI;
the packaged GUI test also changes a native model and opens a second window
without starting another owner or sending a prompt.

Verification for this follow-up: 310 Harness unit tests passed (two ignored),
27 App Server client tests passed, 37 offline adapter tests passed (11 runtime-gated
tests skipped), and the explicit native settings and packaged GUI workflows passed.
Latest GUI evidence: `/tmp/hr-oNZlav/ui`; it checks model/Plan/effort changes,
native readback, exact-ID hiding, and the new window's actual conversation and
settings (not just window count). Native-only settings evidence:
`/tmp/hsettings-r5zaeu`. The normal `target/release-fast/harness` was rebuilt at
14:41:58 Pacific. The three real conversation workers and Xwayland remained
untouched. Their older running adapters do not gain the new settings capability
until restarted; opening a new window is not an adapter upgrade.

The September 7 real-session opening failure (43 expected messages versus 44
restored) was a saved-history reader bug: parallel tool results reference their
originating assistant chunk, so a parent-only traversal dropped a valid sibling.
`claude_sessions.rs` now reconstructs selected parallel responses in native order.
`claude_resume.rs` can repair an already-pending old proof only after recovering
the byte-exact original source, reproducing the old proof, and strictly verifying
the corrected message identities/order/content. No blanket subsequence matching
or verification bypass was added. `--claude-check-history UUID` performs the same
check read-only, without launching or saving verification. It passed against the
user's existing worker with all 44 messages and left the proof unchanged.
The earlier recovery binary was rebuilt at 06:38 Pacific; the running user Harness and
Claude workers were not restarted. Relaunch Harness and reselect the conversation
to apply the correction. Native/GUI regression `/tmp/hr-8CBsfb` verified old-proof
recovery, same-PID reuse, preserved drafts, concurrent reopen, and rejection of
unexplained history changes with no credentials/model requests. All 306 app tests
passed (two ignored), plus 35 offline adapter tests (one native-runtime test skipped).

The user's terminal launcher `/home/sumeet/.local/bin/claude` had an independent
stdin bug: it exec'd Claude from inside a `while read` loop whose stdin was the
installed-version list. The native process inherited that pipe and treated
remaining version paths as a prompt. With explicit user approval on September 7,
the launcher was backed up under
`~/.local/state/harness/launcher-backups/stdin-fix.VMis6V/claude.before` and changed
to select/break inside the loop, then exec after the original stdin is restored.
Do not move that exec back into the redirected loop. The regression
`research/claude-native-lab/terminal_launcher.test.mjs` tests an explicitly provided
launcher using stand-in executables, never actual Claude credentials/model calls.
It reproduces the old input corruption and verifies piped input, TTY ownership,
argv, paths with spaces, version filtering and absent-binary refusal. The actual
repaired launcher also returned `2.1.263 (Claude Code)` for `--version`.
This is a real-machine shell repair, not a change embedded in Harness's binary.
Separate read-only diagnostics found the native daemon's failed OAuth refresh at
06:13:54 Pacific and fresh credentials recognized at 06:14:55 after the user's
login. No authenticated turn was tested, auth locks deleted, credentials changed,
or live workers restarted. Do not assume that launcher repair alone proves all
authentication/Remote Control issues resolved.

Harness is a compact native GPUI client with a shared rich/Vim transcript and
real modal composer. Codex uses App Server; ChatGPT has a separate transport
and catalog but reuses transcript/editor surfaces. Claude now has an experimental
native workspace sharing that same renderer/composer, with native supervision
and the verified stock-executable preload adapter. Legacy detached PTY hosts
remain supported, but new sessions no longer use that launcher. See the native lab README
for launch instructions and explicit capability/adoption limitations.
The Claude sidebar now also discovers existing native saved conversations and
native-supervisor jobs. Saved history alone is non-interactive. Verified worker
endpoints connect directly from Rust; no managed-catalog fixture/relay is needed.
The supervised-worker installer now asks for explicit consent during first
conversation opening/creation, then continues the original action. The optional
**Claude settings…** sidebar action provides refresh/disable. It installs profile-scoped immutable assets and an
exec-preserving launcher, backs up/settings-merges only the native `processWrapper`
key, and provides update/disable without restarting workers. Unsupported builds
skip the optional adapter while native startup continues. The private controller
contract is still version-gated, not an upstream app-server API. Do not change the
real user's hook as incidental validation. Selecting a thread now opens
stopped saved conversations via native `--bg --resume FULL_UUID`, verifies the
restored message identities/order/content, and only then enables the composer.
Per-conversation locks and pending/verified records live in the native profile;
unfinished or changed-worker verification blocks normal Harness action connections.
A live ordinary terminal is refused with guidance to run `/bg` there. Native `/bg`
changes UUIDs and writes `continued-in` to the original transcript; Harness now
follows and verifies that chain, preserving stronger prior checks and the selected
conversation's unsent draft. It does not hot-attach or automatically restart a
terminal. Ordinary terminal catalog rows lack a background job ID: discovery now
handles that schema instead of rejecting live discovery, and native default
ID labels no longer replace saved conversation titles. The + / **New Claude session** path
now uses `claude_creation.rs`: private durable request records, a unique empty
`--settings` file retained in native job metadata, and a short-lived detached
coordinator. The native supervisor owns the conversation. Competing coordinators
reconcile one request; uncertain dispatch never blindly creates a replacement.
The September 7 existing-session pass reproduced that late hook installation
leaves old warm spares uninstrumented. `/bg` alone is not sufficient in that case.
A private native-service refresh kept existing workers, an attached terminal and
its active shell alive without creating another conversation. Selective native
respawn then connected one old worker with verified history, but disconnected
its attached terminal. Neither operation is an automatic migration feature yet.
Do not silently use forceful native respawn on thread opening. Production handoff
verification now excludes only native `/background` bookkeeping appended after
the explicit transfer marker; genuine later work and stronger prior history
checks still block mismatches. See
[the existing-session audit](research/claude-native-lab/SUPERVISOR-AUDIT.md#sessions-running-before-setup--2026-09-07)
for reproduction, successful GUI/active-shell tests, and remaining constraints.
See `claude_resume.rs` and the latest
resume-workflow section in the supervisor audit for conservative recovery limits.
The latest coherent-lifecycle investigation in that audit confirms promptless
native-supervised creation, empty-thread resume, and recovery of one creation
interrupted after native state publication. It also reproduces that `--bg` ignores
caller `--session-id`, identical create retries duplicate sessions, and Harness's
continuation rejects stopped empty threads. Production new-session creation now
uses the verified marker; stopped empty/missing-history recovery remains conservative.
The creation marker does not override native workspace, permission, or RC policy.
The research probe is
`research/claude-native-lab/lifecycle_probe.mjs`; latest evidence is private
`/tmp/hl-dqPmkE/report.json`. Compatibility and submission uncertainty remain open.
The subsequent opening/handoff pass adds real two-window acceptance tests using
`HARNESS_RESUME_GUI=1` with `resume_workflow.test.mjs` and
`terminal_handoff.test.mjs`. Both use isolated profiles and no model prompts.
The opening path and passive reconnect are now distinct internally, not separate
user modes. Native handoff aliases now coalesce into one logical conversation,
while raw Session identities remain explicit. Separate saved alias drafts stay
selectable. Native local-command envelopes render as one card with raw records retained.
The user rejected a diagnostic-heavy startup panel in the sidebar. Failed startup
must be a normal project-named conversation row; selection runs recovery, and a
short main-pane error offers retry plus collapsed technical details. Do not reintroduce
startup records/job IDs/backend instructions as routine sidebar workflow.
`ConversationTarget` separates a real native endpoint from a pending creation,
without inventing a Session for an uncreated native job. Recognized creation
records are linked to their actual conversation instead of duplicating rows.
The earlier **Open in Claude terminal…** escape hatch was rejected as product
drift and is removed from the frontend. The diagnostic CLI retains native attach,
which can wake a job or transfer terminal control; it is not a no-wake guarantee.
Unsupported interactions are unfinished Harness integration work, not a second
user mode or a reason to reintroduce a routine terminal button.
`creation_workflow.test.mjs` verifies creation, caller SIGKILL, concurrent recovery,
two frontend windows, unresolved-creation visibility and native terminal behavior
with isolated profiles and zero model requests. The ordinary opening path still
refuses missing history, including stopped empty conversations. Concurrent saved
thread openers now wait on the first operation and reverify its result.
Latest validation after the sidebar correction: `/tmp/hc-DzRU8w` (creation,
coalesced pending row, collapsed main-pane details), `/tmp/hr-tawpaI` (resume and
competing callers), `/tmp/ht-VX1K7D` (terminal handoff/drafts), `/tmp/hs-Jt7n9x`
(setup). All four executable tests and 286 unit tests passed; two unit tests are
ignored. All 27 offline adapter tests passed. The normal binary was rebuilt;
user Harness PID 1038023 and the real Claude profile were left untouched.
The next pass implements persisted frontend send IDs plus native-profile durable
admission records. `bridge.mjs` claims each UUID before native queue mutation and
saves its outcome afterward. A retry reads the receipt even after reload/restart;
an uncertain claim is never enqueued again. Exact native user-message UUID and
text evidence can reconcile uncertainty, with a separate immutable proof record.
Harness checks accepted=true and the exact receipt UUID before clearing a draft.
Edited drafts query the original pending send rather than replacing its text.
Composer-file locking and journal merges keep stale snapshots from erasing
unresolved IDs or reactivating consumed IDs. Ordinary draft text is still
frontend-owned; full concurrent draft coherence and mixed-version writers remain
open. Do not claim this is a server-authoritative draft system or exactly-once
execution across a native crash (a receipt establishes admission, not completion).
New sends require the durableSubmissionDeduplication snapshot capability; old
adapter connections stay readable but need an explicit hook update and worker
restart before sending. The real user's hook/workers were not changed by tests.
`submission_ledger.test.mjs` covers process-death boundaries, competing processes,
malformed records, and an opt-in actual-native-runtime filesystem probe.
`submission_workflow.test.mjs` uses the real Harness UI with a synthetic legacy
transport to inject lost replies and changed adapter epochs. It verifies saved
IDs across frontend restart, intentional identical sends as distinct IDs, and
uncertain/edited drafts retained. First passing UI evidence: `/tmp/hq-OeEF7x`.
The subsequent Xwayland investigation disproved the earlier GUI display-isolation
assumption: bare `Xvfb -displayfd` can unlink an existing live `X0` socket, including
with a display lock present. The user's repaired `X0 -> X0_` link must not be
modified. `gui_probe.mjs` now requires Bubblewrap mount/network isolation, a
private `/tmp` and X11 socket directory, and no unsandboxed X-server/tool fallback.
Xvfb and driving tools see only the shared disposable fixture writable; the host
root is read-only. Harness itself uses the private absolute XCB socket path while
retaining host process visibility for native-worker identity verification.
`gui_probe.test.mjs` verifies containment before lifecycle GUI tests.
Do not launch automatic-display Xvfb directly on the host. Read the latest
supervisor audit for the reproduction and corrected validation evidence.
The legacy host-status probe also now explicitly releases its temporary flock;
a fork must not inherit a read-only probe's lock and impersonate a host owner.
The creation-lock regression uses a bounded CLOEXEC release wait. All 291 app
unit tests passed in 20 consecutive full runs (two ignored per run), and all 35
offline/runtime adapter tests plus the eight-client concurrency probe passed.
The normal binary was rebuilt with that fix. All eight final executable checks
passed (five packaged workflows plus three display-containment checks); final
fixture paths are recorded at the end of the supervisor audit. The running
user Harness and real Claude configuration were not restarted or changed.
The next product correction removes the permanent discovery/status prose and
terminal button from the sidebar. Actual connected readiness makes the selected
row and composer quiet; disconnected/failed openings keep errors and retry.
The configuration button is labeled Claude settings/Set up Claude; profile hook
changes still require explicit approval. First opening/creation now asks in place;
cancel leaves settings/workers untouched and does not produce a launch failure.
Harness prompts use themed, keyboard-accessible confirmation UI (Escape cancels,
Tab/arrow keys select, Enter confirms; cancellation is selected initially).
Existing cached launchers dispatch through the current Harness binary and now
select its current immutable package for future workers, without rewriting settings
or old assets. They respect disabling setup. Already-running workers and warm
spares are not retroactively upgraded, and native compatibility remains hash-gated.
Ordinary native AskUserQuestion dialogs now use the common RequestSurface with
single/multi-select and custom answers, including keyboard support. Question
text is the native answer-map key; multiple answers join with comma-space.
The frontend retains metadata and permission-updated input, rejects unknown rich
presentation variants, clears edited questions, and sends the descriptor it
actually answered. The bridge compares that descriptor before replying; stale
and already-resolved replies fail across clients. The checkedDialogReplies
capability gates question controls so older workers do not silently bypass this
check. Their installed hook/assets are not silently replaced or hot-reloaded.
See `question_workflow.test.mjs` and `fixtures/question-dialog.json`: real Harness
and the production socket bridge with a synthetic native controller, not a live
model-driven certification. `/tmp/hquestion-jDzGSt` passed two-window resolution,
custom answer precedence, multi-selection, changed-question reset, and reconnect
with keyboard answer/deselection. The send-recovery regression passed again at
`/tmp/hq-jedFwS`, alongside all three display-containment tests. The 36 offline/
runtime adapter tests pass. Native schema/reply evidence comes from the prior
question capture and bounded inspection of installed 2.1.263, not a new remote
research claim. Remaining gaps are in TRANSCRIPT-AUDIT.md: richer dialogs and
permissions, model/mode controls, attachments/Artifact interaction, and nested
agent fidelity. Do not call this Codex-equivalent merely because the scaffolding
or shared renderer is in place.
Final checks for this correction: 296 app unit tests passed (two ignored), 36
offline/runtime adapter checks passed, and six GUI/workflow checks passed.
`/tmp/hr-HtugXq` is the actual stock-native saved-thread resume and two-window
proof, with no model prompt or copied credentials. The normal binary was rebuilt
at 2026-09-07 02:08:48 -0700. The real Claude profile and Xwayland socket/link were
not changed by this pass. No user-facing process was restarted by the agent.
The subsequent setup/update pass rebuilt the normal binary at
2026-09-07 03:43:07 -0700. All 297 app tests passed (two ignored), along with 36
adapter checks and all three display-containment checks. Final executable proofs:
`/tmp/hr-TAMoQs` covers in-flow first setup, Enter/Escape cancellation, reopening,
explicit approval, draft preservation and two frontends reusing one native owner;
`/tmp/hr-bRtzwj` repeats native history/ownership verification through an old launcher
with deliberately missing old assets, while keeping settings byte-identical.
`/tmp/hs-V2EY43` verifies delegation, backup preservation, disable, and the
disable-during-worker-lock race. That race was reproduced against the prior binary
before moving consent validation under the package/setup lock. Additional workflow
regressions passed at `/tmp/hc-K4jScL` (creation), `/tmp/hquestion-7mHCaM` (questions),
`/tmp/hq-5MawuI` (send recovery), and `/tmp/ht-MBozvS` (explicit native terminal handoff).
These are private fixtures, not user threads; no model requests or credential copies
were needed. The user's running apps, Claude profile and Xwayland were left alone.
This closes the tested first-use/cached-launcher paths, not the remaining gaps:
an already-running uninstrumented worker or warm spare cannot gain an adapter just
because settings changed; model/mode controls, attachments and richer native
interactions still require implementation and verification. Do not redirect those
unsupported features to a terminal button or imply that they have been completed.
See `claude_sessions.rs` and the "Existing native conversations and direct
discovery" section in the native lab README before extending lifecycle controls.
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

- The queued-prompt/tool-header batch is implemented in the working tree:
  expandable full prompts with exact-text copy and edit/reorder-safe previews;
  exit labels use the configured code size; command headers share an icon/text
  column across states; web headers expose queries and distinguish open/find
  actions, with returned-result counts instead of ambiguous sources. See
  [native captures and reproduction](research/interface-comparison/ergonomics/README.md).
  No live app or daemon was restarted, and no fixture input was sent to a model.
- Queue pause/hold must be server-owned, not a Harness-local preference: the
  user explicitly rejected local flags next to the shared queue. The unfinished
  `queue_control.rs` approach was removed before any build or deployment.
  Codex 0.153.4's generated experimental protocol exposes queue
  add/list/update/delete/reorder/start, but no pause/hold state or operation.
  Implementation needs an App Server contract and enforcement, including
  persistence across restarts, cross-client updates, and atomic start-versus-
  pause handling. Do not substitute local drafts or another client-side store.
  Failure policy also needs a decision: Harness currently attempts queue
  advancement after a failed active turn, not only after success. No retry or
  advancement policy was changed by the terminal-error presentation fix.
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
- [Native Claude bridge lab](research/claude-native-lab/README.md), 2026-09-06:
  the default launcher now runs the **unmodified installed executable** with
  process-local Bun preload, discovers the embedded Ink registry structurally,
  and finds/validates the native session controller through its mounted React
  tree. No chunk names or hook indices are pinned in discovery. The bridge still
  instruments private native methods; this is not a supported extension API.
  Sixteen offline tests, two four-check preload live runs, existing Artifact
  read, stopped-stock-session resume, packaged resume, and native `/clear` passed.
  Earlier pinned-patch tests also verified AskUserQuestion and private Artifact
  create/update. The explicit `--patch` fallback reproduces the previous hash.
  Version gating is intentional: the 2.1.243-musl preload can run and its registry
  is discoverable, but the current controller adapter rejects its missing layout
  callback. Use `--probe-unverified` only for fresh disposable compatibility tests;
  do not merely update an accepted hash. `--resume-lab` refuses a live owner and
  resumes only previous disposable labs. The Harness workspace now adds detached
  lifetime, session discovery/reconnection, shared transcript/composer, text
  send/stop, shared file/Bash approvals, and a same-process Native terminal button.
  The [transcript/protocol audit](research/claude-native-lab/TRANSCRIPT-AUDIT.md)
  fixes the initial generic Write projection: native result metadata now feeds
  shared file/diff cards; Read/search targets, command failures, block streaming,
  orphan results, and known system errors have regression coverage. The app
  suite passes 249 tests (one live-network test ignored); these are ordinary
  Rust tests plus separate isolated GUI checks, not a full GPUI test suite.
  Managed-host hardening publishes the catalog before spawning, holds a lifetime
  lock, rejects duplicate/reused hosts, preserves healthy records beside corrupt
  ones, bounds compatibility/connection waits, and disables stale controls on
  disconnect. Reconnect never starts another Claude or resends a prompt. Eleven
  lifecycle regression tests passed; a fresh native host, duplicate rejection,
  mixed healthy/corrupt/missing-runtime discovery, and same-host GUI connection
  were checked without submitting a model prompt. Follow-up live QA killed only
  a fresh idle host: its native child exited, the UI detected Stopped despite
  the stale attached marker, an unsent draft survived, and Reconnect/Send did
  not start a new owner or consume the draft. Shared sidebar rows now reserve
  space for metadata status/timestamps while truncating long project paths;
  full paths remain available on hover. Reconnecting the selected host preserves
  its current transcript until a fresh snapshot arrives. The lock is per host,
  not a global native-conversation ownership guard; crash detection is not
  crash/reboot conversation restoration.
  During this QA, repeated app tests were found to launch/focus ChatGPT Desktop:
  the request-header test reached a `codex-desktop --version` probe. That probe
  now reads `resources/codex-linux-build-info.json` without launching a process;
  the top-level `version` file is Electron's version, not the app's. Explicit
  version override/default fallback remain available. Do not restore the GUI
  executable probe or assume Xvfb alone isolates desktop-launch side effects.
  Arbitrary-session hot attach, supervisor migration, and general stopped-session
  adoption UI are not implemented; do not confuse the lab resume check with a
  complete migration feature.
  The subsequent [concurrency audit](research/claude-native-lab/CONCURRENCY-AUDIT.md)
  demonstrated two stock interactive processes opening the same disposable UUID
  without our preload. Stock resume did refuse when native `--bg` owned that UUID:
  native-supervisor lifecycle reuse became a stronger lead. No model prompt was
  submitted in that ownership experiment; its workers and transient supervisor
  were stopped/confirmed exited. The repeatable synthetic `probe_concurrency.mjs`
  uses eight client processes for duplicate submission and approval races. Those
  ordinary cases passed, but fault injection exposed duplicate queue admission
  after a post-admission exception, and adapter replacement loses deduplication.
  These remain unfixed. Full-snapshot serialization scales with client count;
  shared full-snapshot drafts/session JSON writes also lack cross-process
  transactions (inspection finding). Do not claim a durable exactly-once journal,
  global native ownership, or production multi-client soak coverage from these
  probes. All 16 existing offline adapter/preload tests still pass.
  The follow-up [supervisor experiment](research/claude-native-lab/SUPERVISOR-AUDIT.md)
  now demonstrates cold-worker and warm-standby preload through the officially
  documented `CLAUDE_CODE_PROCESS_WRAPPER` / `processWrapper` launch seam. No
  installed binary changes or user settings changes were needed. Stock native
  terminal attach, two four-check live suites, eight independent socket-client
  processes, persisted-history respawn and stale-epoch rejection passed. Two
  isolated real Harness windows shared a composer roundtrip through lab-only
  legacy catalog/socket relay plumbing; the production launch/discovery backend
  and Native terminal button are NOT migrated. Nine model prompts were submitted
  in disposable workspaces. SIGKILL of an idle worker did not auto-recover in
  30 seconds; explicit native respawn restored the same UUID and 32 messages,
  and manual Harness reconnect worked. Two competing respawns returned success,
  with one final owner and preserved saved message UUIDs; 100ms sampling is not
  proof of global atomic ownership. Wrapper rejection of 2.1.243-musl left the
  current worker untouched; a genuine new-version update was not tested.
  All 22 offline tests pass, including six new wrapper tests. Native supervisors,
  jobs, standbys, GUI fixtures, relay, and Xvfb/compositor were stopped; user
  Harness PID 906724 was left running. Prefer native lifecycle reuse, but retain
  the private controller's version gate. Do not install this temporary wrapper
  globally or restart a pre-existing native service without explicit setup.
  Native `--bg --resume` with extra flags created a copy rather than retaining
  the requested UUID; an immediate restart before that copy materialized lost
  its in-memory imported history. Verify actual identity/persistence on adoption.
  Shared drafts, admission uncertainty, fan-out and reconnect cursor work remain.
  The next setup/discovery pass tested native `daemon stop --any --keep-workers`
  with only idle disposable jobs: an unwrapped worker kept PID 996683 across
  supervisor handover, then just that job migrated to the adapter, preserving
  its UUID and history while another job kept PID 997116. Global settings and
  existing corporate-wrapper composition were NOT changed/tested. The new
  `supervisor_catalog.mjs` uses `agents --json --all --cwd`, so dormant jobs remain
  discoverable without a second authoritative catalog. It binds directly to real
  per-worker sockets with native ID/PID, boot/start ticks, executable, UID, cwd,
  protocol, UUID, epoch and post-handshake endpoint checks. New endpoint metadata
  is version 1; old lab endpoints without the process fingerprint are refused.
  This is research code, not yet the Rust catalog/backend. All 26 offline tests
  pass. One tools-free model prompt was submitted; no GUI was launched.
  IMPORTANT: native `attach` is not a universal missing-history guard. With only
  the stopped lab job's transcript temporarily withheld, it opened a new worker
  under the SAME UUID with ZERO messages, both before and after a real assistant
  reply. Installed-source refusal is conditional, not general. Cleanup stopped
  the unexpected worker and restored the original file with a hash check; normal
  restart recovered all 11 messages including the submitted UUID/assistant marker.
  Do not silently wake on reconnect or infer continuity from matching UUIDs.
  Explicit wake needs saved-history preconditions and post-wake continuity checks;
  this guard is not implemented. Evidence: `/tmp/harness-claude-supervisor.2MJ8Bw`
  and the follow-up section in SUPERVISOR-AUDIT.md. Test workers/supervisors were
  stopped; the user's Harness stayed running.
  A separate packaged Remote Control probe also passed reply/permission-denial
  tests against untouched stock Claude. Its tested OAuth HTTP ingress is marked
  peer-origin, and its empty result can arrive before assistant text. Native
  local peer messaging/supervisor interfaces were inspected but do not establish
  a full app-server. Keep these alternatives distinct, not a multi-source event
  reconstruction. All test Claude processes were stopped; proprietary binaries,
  source, raw captures, and credentials are not repository assets. Installation
  and existing user sessions were untouched. Details and exact evidence are in
  the lab README/evidence.json. Remaining: broader native dialog/tool projection,
  stopped-session adoption/reboot recovery, and upgrade compatibility checks.
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
