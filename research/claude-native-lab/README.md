# Native Claude bridge experiment — 2026-09-06

Latest implementation: [thread opening and terminal handoff](SUPERVISOR-AUDIT.md#thread-opening-and-terminal-handoff--2026-09-06).
Latest existing-session investigation: [sessions running before setup](SUPERVISOR-AUDIT.md#sessions-running-before-setup--2026-09-07).
Architectural assessment: [coherent lifecycle investigation](SUPERVISOR-AUDIT.md#coherent-lifecycle-investigation--2026-09-06).
New and reopened conversations now both use native supervision. Creation has a
durable profile-scoped operation record and a short-lived detached coordinator;
the native supervisor remains the conversation owner. Handoff aliases form one
sidebar conversation. Missing-history recovery, update compatibility and complete
product parity remain separate limitations, not consequences of a second UI mode.

A working local socket bridge into **unmodified native interactive Claude Code
2.1.263**. The default launcher now uses Bun's startup preload mechanism instead
of rewriting Claude's executable. The original, hash-pinned binary patch is
retained as an explicit fallback experiment.

**Harness now has an experimental native Claude workspace using this adapter.** Native
prompts, streaming, permissions, reconnect/interruption, Artifact read, and
restart/resume have worked with the stock executable. It still requires a startup
preload: hot-attaching our local bridge to an arbitrary already-running process
is not implemented. Stopping a stock session and resuming it with the adapter was
tested successfully. The installed CLI was not changed.

## Try the Harness integration

The footer now composes Claude's own permission, model, and effort menus. Models
and effort choices come from the native catalog. Changes call the native command
handlers/permission transition on the verified binary, apply only to this session,
and broadcast the authoritative settings to other Harness windows. Busy sessions
and unanswered dialogs disable changes; native gates still reject unavailable
modes, including bypass without the launch capability. The new `sessionSettings`
capability is absent on already-running old adapters: a frontend relaunch alone
cannot add it, and Harness does not silently restart those workers. Newly spawned
worker/standby processes use the current binary's packaged assets through the
existing stable launcher. Assigning a new conversation to an already-warm standby
does not reload its preload. September 7 read-only checks confirmed this on all
four reachable real workers: each lacked `sessionSettings` and had started with
the older package, despite a newer package already existing on disk. A verified,
user-facing upgrade for existing connections remains the next lifecycle task;
neither refreshing setup nor the experimental old-module `reload` is that upgrade.

Native settings coverage: `settings_workflow.test.mjs` with
`HARNESS_NATIVE_TEST_BINARY` set to the verified executable. Add
`HARNESS_CLAUDE_CONTROLS_GUI=1 HARNESS_RESUME_GUI=1` to the existing packaged
`resume_workflow.test.mjs` invocation to exercise model switching, hidden test
history, and the provider-tab new-window action through the real UI. These tests
use private profiles without credentials or model prompts.

Build with `HARNESS_BUILD_JOBS=1 ./script/build-standalone.sh`, then use Harness's
normal new-build Relaunch prompt. Select **Claude**, enable native connections
once, and use **New Claude session** to choose a project, or select an existing
conversation. Send ordinary prompts through Harness's existing composer.
The permanent worker-discovery footer and terminal-launch button are removed.
Ordinary questions now use the shared question form; unimplemented interactions
are explicitly labeled as gaps. The goal is to finish those interactions in
Harness, not require a separate terminal mode. Setup remains an explicit,
backed-up settings change; real profiles are never changed by tests.
Question replies require the updated adapter's `checkedDialogReplies` capability;
relaunching the frontend alone doesn't update an already-running worker. Such a
worker remains connected, but the new question controls stay disabled. The
existing explicit integration update applies to future workers; no test or build
silently changes the real user's settings or restarts their sessions.

`claude_native.rs` owns private connections and translation into
`harness_protocol::TranscriptItem`; `claude_creation.rs` and `claude_resume.rs`
coordinate native lifecycle operations. The earlier detached PTY host remains
available only for compatibility with already-created managed hosts.
`claude_workspace.rs` manages the provider state and actions. The transcript is
the same `render_transcript_list_item` / `render_item` path as Codex and ChatGPT,
including Markdown, code, Bash cards, file/diff rows, web cards, selection,
scrolling, search, and Vim. The [transcript/protocol audit](TRANSCRIPT-AUDIT.md)
records mappings, regression coverage, and the remaining preview gaps.
Verified Write/Edit and Bash permissions use the existing `RequestSurface`, with allow-once
and deny; unknown dialogs stay visible as unfinished integration work.

### Existing native conversations and direct discovery

The Claude sidebar now combines managed Harness hosts, saved top-level native
conversations from `CLAUDE_CONFIG_DIR/projects` (default `~/.claude/projects`),
and the native `claude agents --json --all` catalog. `harness --claude-list`
exercises the same discovery path without opening a window. It does not wake
jobs, resume conversations, or send prompts.

`claude_sessions.rs` reads bounded metadata for the list and loads the selected
conversation's selected parent chain, including native parallel-response siblings,
into the shared transcript renderer. Parallel tool results point to the assistant
chunk that invoked them rather than the preceding record. Selected response IDs
and matching tool-use IDs bind recovered results; unrelated branches are not
concatenated into the history. It excludes
subagent sidechains, permits retained fork ancestors, ignores an unfinished final
write, and refuses malformed complete records or missing ancestors. Saved
history alone cannot enable stale approvals or sending; selecting a thread now
opens and verifies its native worker before enabling the composer.
Configuration-directory namespacing prevents cross-profile conversation ID
collisions. Duplicate transcript claims produce a warning rather than choosing
one arbitrarily. This is a saved-message reader, not a native resume engine.

Native-supervised workers with a verified adapter endpoint connect directly from
Rust, without the old managed-catalog fixture or byte relay. Discovery checks
private endpoint paths, native job/PID identity, boot/start time, executable and
workspace, socket peer credentials, conversation UUID, adapter epoch, and event
cursor. A changed or missing event cursor requires a fresh snapshot. Native
actions require both a conversation ID and an epoch. Refreshing a native
conversation re-queries discovery instead of reconnecting to a stale PID.

Endpoint lookup defaults to
`$XDG_RUNTIME_DIR/harness-claude-<profile-hash>/jobs`, keyed by the canonical Claude
configuration directory; the explicit `HARNESS_CLAUDE_ENDPOINT_ROOT` override
supports the older isolated lab deployments and QA. Merely updating Harness does
not instrument an ordinary terminal or supervisor. Such sessions remain read-only
in Harness until handed off; passive listing leaves stopped sessions stopped. The existing
**+ / New Claude session** path now creates a native-supervised conversation,
as described below. Selecting saved history uses that same native supervisor;
there is no separate Continue mode. Automatic handoff of an ordinary terminal
remains unsupported. The diagnostic `--claude-job-terminal` command retains stock
`attach` semantics, including possible wake; it is not a no-wake guarantee and is
not a normal frontend action.

### Open an existing conversation

Select a saved Claude conversation. With native connections configured, Harness starts the stopped
conversation through `claude --bg --resume FULL_UUID` and connects to its verified
worker. No new prompt or composer draft is sent by this action. The UI is quiet
once verification succeeds; discovery alone never means connected. A worker already
connected through Harness is reused, not restarted. Failures expose **Retry opening**;
refreshing the list and reconnect monitoring never launch a worker or resend a prompt.

An ordinary terminal is never seized or restarted automatically. Once the native
background service is using the configured Harness launcher, run `/bg` in that
terminal, then select its conversation in Harness. **If Claude was running before
setup, installing the hook alone is not sufficient:** `/bg` may reuse an old,
uninstrumented standby. Service refresh without replacing workers is now tested,
including an attached terminal and running shell, but not yet part of automatic
setup/opening. An existing uninstrumented background worker needs its own explicit
migration; native respawn disconnects its attached terminal. Do not restart all
sessions as a setup shortcut. See the existing-session investigation above.
Native Claude 2.1.263
assigns a new conversation UUID during this handoff and writes a `continued-in`
record to the original transcript. Harness follows that explicit chain and
verifies the source histories against the destination, retaining any stronger
existing destination verification. Source verification stops at the explicit
handoff marker and permits only the known subsequent `/background` bookkeeping;
later conversational changes are refused. Missing, conflicting, cyclic, or unverified
handoffs are refused rather than reopening the original history. The selected
conversation has a stable logical identity while its live endpoint changes.
Unambiguous chains coalesce into one sidebar row; raw physical records remain
available to verification and diagnostics. Cycles, converging roots, and missing
destinations produce warnings instead of silently hiding history. Drafts already
saved under different aliases remain separate and selectable, never merged or
overwritten merely to collapse the rows.

### Create and recover a native conversation

`harness --claude-new PROJECT` and the New Claude button use the same production
creation path. Private records live at
`CLAUDE_CONFIG_DIR/harness-adapter/creations/REQUEST_UUID`. An immutable empty
settings file supplies a unique marker: Claude 2.1.263 persists its exact
`--settings` path in the job's `respawnFlags`. Matching does not depend on a title,
a caller-specified native UUID (which native `--bg` ignores), or catalog differences.
The empty file does not override native workspace, permission or Remote Control
policy. Job identity and the live adapter snapshot are checked before completion.

A detached `harness --claude-create-worker REQUEST_DIRECTORY` finishes the
operation after frontend closure. Its per-request lock serializes competing
coordinators. State is persisted before dispatch; an uncertain dispatch is never
repeated without a unique native job match. Known pre-dispatch failures can be
retried. An unresolved start appears as a normal project-named conversation row.
Selecting it reconciles the original request; failure information appears in the
main pane, with technical details collapsed. There is no separate startup control
panel in the sidebar. `harness --claude-create-recover REQUEST_DIRECTORY` remains
available for diagnostics. Recovery never means resending the composer draft.

Creation does not establish that an absent transcript will always mean an empty
conversation later. Normal Harness opening therefore still refuses a stopped
conversation with missing history. That recovery remains unfinished; a terminal
escape hatch is not the intended product solution.

`creation_workflow.test.mjs` exercises production creation, loss of its caller,
concurrent recovery, unresolved-request visibility, native terminal delegation,
and two frontend windows against a disposable profile without model requests.

### Saved-history verification

`claude_resume.rs` serializes competing Harness continuations with a native-profile
file lock. A second opener waits up to 45 seconds and rechecks the first operation's
result instead of immediately demanding a retry. Before launch it requires complete,
parseable saved history, checks live
native owner records, rechecks the saved file digest, and persists the expected
semantic history digest/count. After launch it requires the same conversation UUID
and ordered message UUIDs/roles/contents, then binds its verification to the actual
PID, Linux process start time, boot ID, and adapter epoch. Normal Harness connections
and actions reject a pending or changed-worker verification record. These records
live under `<Claude configuration>/harness-adapter/continuations`, independent of a
frontend's XDG preferences and lifetime. This is a Harness coordination mechanism,
not an atomic lock enforced on arbitrary third-party Claude launches.

Only the narrow resume arguments are used: adding options to an existing native
job can cause Claude to create a copy. A detected copy is an error, not silently
presented as the selected conversation. No possibly pre-existing worker is killed
to recover from a failed attempt. If launch outcome is uncertain, Retry opening can
reverify an existing worker but does not automatically launch a second one. If no
worker exists, native inspection/recovery is still needed; there is no universal
automatic crash recovery or resume-anything guarantee. Histories transformed by
native compaction/normalization beyond the verified representation may be refused
rather than approximately matched. Refresh remains read-only and never wakes a job.

Diagnostic equivalent (this explicitly starts a stopped conversation):

```sh
target/release-fast/harness --claude-continue FULL_CONVERSATION_UUID
```

`--claude-check-history FULL_CONVERSATION_UUID` checks an existing worker against
its saved continuation proof without launching, sending, or marking it verified.
For a failed proof from the former parent-only reader, reconciliation requires
the byte-exact original source checksum, reconstruction of the old proof, and a
strict match of the corrected message UUIDs/order/contents against the worker.
An unchanged original prefix may still exist after native resume appends records.
Missing/changed source or unexplained differences remain failures. Normal thread
opening publishes the repaired proof only after the usual owner/epoch checks.

The executable regression uses a fully separate Claude profile, no credentials,
and no model request. It checks first adoption, stopped-job wake, duplicate client
requests, failed/retried verification, and missing-history refusal:

```sh
HARNESS_SETUP_BINARY=target/release-fast/harness \
HARNESS_NATIVE_TEST_BINARY=/EXACT/VERIFIED/CLAUDE \
node --test research/claude-native-lab/resume_workflow.test.mjs
```

Add `HARNESS_RESUME_PARALLEL_FIXTURE=1` to cover parallel-tool restoration and
recovery of an already-pending parent-only proof without restarting the worker.
`HARNESS_RESUME_REPAIR_GUI=1` additionally selects that failed conversation in an
isolated real Harness window and checks that recovery preserves its unsent draft.

Add `HARNESS_RESUME_GUI=1` to the regression command to exercise isolated real
Harness windows on a private Xvfb display. This requires Bubblewrap, Xvfb,
xcompmgr, xdotool, ImageMagick, and the Mesa software EGL driver. The GUI probe
puts Xvfb and GUI-driving tools in separate network namespaces, hides host `/tmp`,
and shares only the disposable fixture and its private X11 socket directory.
Harness itself uses XCB's absolute socket-path support to connect directly to
that private display. It retains host process visibility so native PID/executable
verification and supervisor lifetime remain representative of normal use.
There is no unsandboxed X-server/tool fallback. An unavailable sandbox is a test failure.
The old bare `Xvfb -displayfd` launcher was unsafe: it could remove the user's
live Xwayland `X0` pathname. Do not reuse that invocation on the host, even with
separate XDG directories. `gui_probe.test.mjs` verifies mount/network isolation,
simultaneous displays, and preservation of desktop sockets after teardown.
The GUI probe uses independent frontend configurations, a shared disposable
native profile, exact process/window targeting,
and zero model requests. It checks single-click opening, saved draft retention,
two-frontends/one-worker, and worker lifetime after both windows close. The terminal
handoff regression is `terminal_handoff.test.mjs`, using the same environment flags.
Its synthetic history starts in a stock **unpreloaded** terminal; native `/bg`
carries the tested settings/model/MCP/add-directory flags and in-place editing
policy into the instrumented supervisor. This is not proof of every native feature.

### Packaged native connection setup

Opening a stopped saved conversation or creating a conversation asks for setup
consent when needed, then continues that same action after setup succeeds. Cancel
leaves the conversation, draft, settings and native workers alone. Merely browsing
the catalog does not trigger setup or launch anything. A typed pre-launch error
distinguishes missing consent from an uncertain startup: only the former triggers
this flow; arbitrary launch failures are never automatically retried.

The optional **Claude settings…** action can refresh the integration or disable it.
Setup adds a `processWrapper` launcher to Claude's user settings, with a backup.
Setup itself never restarts, resumes or stops a worker; the requesting conversation
action performs its normal verified opening afterward. A service that predates any
Harness configuration may still require a deliberate handover. Existing workers
and already-started warm spares are not retroactively instrumented or upgraded.

`claude_setup.rs` installs immutable, content-addressed assets and a private
launcher under `<Claude configuration>/harness-adapter`. The launcher dispatches
through the current Harness installation path, including after a same-path Harness
upgrade. For each future worker, that dispatcher selects the current binary's
immutable adapter package instead of the supervisor's cached old package. It does
not rewrite settings or old assets. A bounded setup lock serializes package creation;
consent is reread under that lock, so a worker waiting behind a disable operation
cannot use its earlier consent snapshot. Compatibility still checks the actual
native executable. Cached launchers respect disabling the integration in settings.
It uses Claude's documented
[process-wrapper seam](https://code.claude.com/docs/en/corporate-launcher), not an
executable patch. The in-process controller adapter is still private and hash-gated.
Node.js and an executable Harness at the configured path are required for the hook;
moving Harness requires updating setup. If Harness has been removed, the shell
launcher delegates directly to the original Claude command.

Setup preserves unrelated settings, saves exact private backups, serializes other
Harness installers for the same native profile, and refuses an existing unrelated
launcher or Bun hook. It checks for settings changes before writing. An unrelated
editor that ignores the setup lock can still race the final settings replacement;
this is not a general cross-application settings transaction. Existing managed
policy may also override user settings. No corporate-wrapper composition is implied.
Disabling retains old assets because a running supervisor can still reference them.

For a worker launch, the dispatcher checks the **actual executable being spawned**
within a two-second compatibility budget and then `exec`s it, preserving the PID,
arguments and inherited environment. Unsupported builds, missing adapter assets,
or conflicting Bun hooks skip Harness instrumentation and leave native startup
alone, recording an adapter diagnostic in the private runtime directory. A failure
to execute Claude itself is still a native launch error. This fallback does not
claim that arbitrary corruption of the launcher/Harness executable is recoverable.
No global setting has been enabled automatically on this development machine.

For diagnostics without opening a window:

```sh
target/release-fast/harness --claude-setup status
# Stages and verifies assets, without registering a hook or editing settings:
target/release-fast/harness --claude-setup prepare
# These explicitly change Claude user settings, with backups:
target/release-fast/harness --claude-setup enable
target/release-fast/harness --claude-setup disable
```

The executable-level setup regression runs against a disposable native profile:

```sh
HARNESS_SETUP_BINARY=target/release-fast/harness node --test research/claude-native-lab/packaged_setup.test.mjs
```

`resume_workflow.test.mjs` also supports `HARNESS_RESUME_GUI=1` with either
`HARNESS_RESUME_ONBOARDING=1` (keyboard cancel/reopen/approve in the actual UI) or
`HARNESS_LEGACY_LAUNCHER_FIXTURE=1` (a cached old launcher whose package deliberately
has no adapter assets). Set `HARNESS_NATIVE_TEST_BINARY` to a verified stock Claude
executable. Both modes use a disposable profile, synthetic saved history, and no
model request. The older-launcher mode verifies that settings remain byte-identical
and the new worker exposes the current checked-dialog capability. The packaged
setup test holds the installer lock, waits until a worker has opened it, disables
setup, then releases the lock; it verifies that the waiting worker skips the
adapter. This regression fails against the earlier check-before-lock dispatcher.

The app embeds our adapter assets; users do not need a checkout or two lab
commands. Node.js is needed for the read-only binary compatibility check. On
Linux, the latest plain-semver native executable under
`~/.local/share/claude/versions` is selected, or `HARNESS_CLAUDE_BINARY` can name
an exact executable. Unverified builds cannot receive Harness control; no binary
patch is applied. New creation refuses them; the optional native-supervisor
startup hook delegates to stock Claude without instrumentation.
Native settings/plugins remain native. The legacy managed launcher disabled
Remote Control through an overlay; native-supervised creation preserves the
user's native policy. The local adapter does not require hosted Remote Control.

### Historical managed-host lifecycle hardening

This describes already-created legacy hosts, not the current New Claude path.

New hosts publish their private, atomic catalog record **before** a child can
start. Spawn/check failures remain diagnosable; corrupt or unsafe records produce
warnings without hiding healthy records. Listing never recreates a missing
runtime directory. Records are ordered by creation time with a stable tie-break.

A process-held file lock identifies the lifetime of each new managed host. A
duplicate launch of that host is rejected without altering its state; after a
crash the lock releases even if old socket/status files remain. Reusing a used
runtime is refused rather than silently creating a new Claude conversation.
Legacy hosts without the lock require a live socket probe, not a socket pathname.

The sidebar exposes starting, checking, setup, available, stopped, unavailable,
and needs-attention states. **Reconnect** only connects to the selected host:
it never spawns/resumes Claude or resends prompts. A failing available host gets
bounded automatic retries; native trust/login can wait for the user. Host loss
also disables stale approvals and sending while preserving the visible history
and draft. Compatibility checks and initial socket handshakes have timeouts.

Terminal setup happens before native launch where possible; later startup errors
terminate and reap only that launch's child. Adapter status publication is atomic,
and initialization/discovery failures now appear explicitly instead of being
mistaken for endless setup.

Live lifecycle QA killed only a fresh, idle managed host (no model prompt was
submitted). The native child exited, the lifetime lock released, and the UI
reported Stopped despite the leftover attached marker. The unsent composer draft
survived; clicking Reconnect and Send did not launch a replacement or consume it.
Available/Stopped/Unavailable labels were visually checked in the narrow sidebar
with long project paths. Same-host manual reconnect now keeps the visible history
while waiting for a new snapshot, and empty-state guidance distinguishes a stopped
host from native trust/login setup. This verifies detection and safe refusal,
not automatic recovery of a crashed conversation.

This lock is **per managed host**, not a global lock over every native Claude
conversation. It does not make arbitrary terminal-session adoption or native
`/resume` safe against another writer. The separate stopped-session continuation
path above uses native supervision and explicit history verification. Unknown Claude builds are still refused, with the exact path
and hash reported; there is no silent downgrade to an older build.

The native host is a separate `harness --claude-host` process. Closing or
relaunching the frontend disconnects its client, not Claude. Session discovery
records live under `$XDG_DATA_HOME/harness/claude` (the standard local data
directory by default); per-session runtime assets and sockets use private
`/tmp/harness-claude-<uuid>` directories. `harness --claude-new <project>` now invokes
native-supervised creation instead. No credentials or native source are copied
into the repository.

Current limits: text submission only; native model/mode/slash commands,
AskUserQuestion, other permission kinds, specialized Artifact presentation, and
other unmapped dialogs use Native terminal. A stopped host is not automatically
resumed by merely refreshing, nor is an arbitrary existing terminal process hot-attached.
Selecting a stopped saved conversation explicitly opens and verifies it.
Normal native `/resume` remains available in Claude, but must not be used while
another process owns that conversation. Do not claim full product parity or an
upstream-supported, update-independent app-server contract.
For native-supervisor jobs, unmapped dialogs use **Open in Claude terminal…**,
with explicit confirmation of Claude's possible wake/control-transfer behavior.

## Concurrent clients and native ownership

The [concurrency audit](CONCURRENCY-AUDIT.md) records a live distinction:
two stock interactive Claudes could resume the same disposable conversation,
but stock `--resume` refused while the native background supervisor owned it.
The follow-up [supervisor experiment](SUPERVISOR-AUDIT.md) now verifies adapter
startup through Claude's documented process wrapper, warm-standby promotion,
native terminal attach, eight socket-client processes, native respawn, and two
actual Harness frontends sharing a composer roundtrip. No binary patch was needed.
An idle SIGKILL did not automatically recover within 30 seconds; explicit native
respawn and subsequent manual reconnect worked. Competing respawns retained one
observed owner and saved message IDs. That original GUI QA used private fixture
records and a byte relay. The current Rust discovery and stopped-session
continuation and creation paths supersede that plumbing. Upgrade compatibility
and the failures below remain separate work.

The next discovery/setup pass preserved unwrapped workers across native
`--keep-workers` supervisor handover, then migrated just one job while the other
kept its PID. `supervisor_catalog.mjs` now demonstrates direct endpoint binding
from the native JSON listing, with process-start/boot/UUID/epoch validation and
no fixture relay. Rust now implements that direct binding in the app. A missing-history fault
also showed that native `attach` can wake an empty conversation under the same
UUID, even after an assistant reply was saved. The explicit continuation path
above now checks history continuity; read-only reconnect still never wakes. The private test
transcript was restored byte-for-byte, and normal resume restored its marker.
See the follow-up section of [SUPERVISOR-AUDIT.md](SUPERVISOR-AUDIT.md).

The original eight-process synthetic socket probe reproduced duplicate admission
after a post-queue exception and lost deduplication on adapter replacement.
The durable-admission pass now fixes both: retries retain their UUID, and accepted
or uncertain native-profile records prevent a second admission after restart.
Run `node research/claude-native-lab/probe_concurrency.mjs` to exercise those
regressions without Claude or credentials. Full-snapshot fan-out cost and complete
shared-draft revision handling remain limitations; the fixes do not establish
exactly-once execution or unlimited client scaling.

## Latest findings: alternate approaches and packaging

| Route | Evidence on this machine | Decision |
| --- | --- | --- |
| Stock executable + preload + native stores | Two runs of all four live checks passed; existing Artifact read; stopped stock conversation resumed with history; native `/clear` changed the socket's session identity | Most promising local integration route; default lab launcher |
| Remote Control OAuth subscriber + HTTP ingress | Connected to an untouched stock terminal, received a reply, and denied a real Write through the WebSocket | Viable alternative with hosted routing/private protocol; tested ingress is peer-origin, not ordinary user input |
| Native per-session messaging socket | Live process metadata and installed handler inspection: peer messages, delivery/idle notices, rename, Artifact handoff | Useful local peer channel, not an observed general transcript/control API; no wire-level probe performed |
| Native background supervisor / attach | Stock resume refusal, process-wrapper preload in cold and warm workers, native attach, respawn, two Harness frontends, competing restarts | Preferred lifecycle direction; production discovery/configuration and private semantic compatibility still need work |
| SDK / ACP / `--sdk-url` | Installed CLI sends this through the noninteractive runner; SDK URL handling is not a free localhost TUI bridge | Does not meet the requirement to attach to the existing native TUI owner |
| Channels, Hooks, JSONL | Documented ingress/callback/persistence surfaces | Still possible for limited use, but not selected as a multi-source reconstruction architecture |
| Binary patch / debugger injection | Prior binary patch worked; generic React DevTools registration alone produced no callbacks | Keep the pinned patch as fallback; no reason to begin with debugger injection while startup preload works |

Remote Control was tested separately, not combined with the local adapter to
reconstruct state. Its tested API prompt was visibly wrapped by native Claude as
coming from another Claude session. The authenticated permission-denial response
did work. We did not spoof first-party client identity or try to bypass that
origin distinction. The probe does not establish RC's complete feature coverage.
RC also delivered an empty `result` event before the assistant message containing
the final text. A client cannot treat that result payload as the whole answer or
immediately discard subsequent events.

### What the no-file-patch route actually does

1. Read the installed ELF's embedded module index without changing the file.
2. Locate the small Ink registry module by structural markers and exported getter,
   not a hard-coded chunk filename or minified function name.
3. Start **that same stock executable** with a process-local
   `BUN_OPTIONS=--preload ...` module. Consume those environment flags so tools'
   child processes do not inherit the adapter bootstrap.
4. Import the embedded registry, inspect the mounted React tree for the native
   session controller by its object capabilities, and validate its ownership of
   the turn, queue, and dialogs. Hook positions and component names are not used.
5. Attach the existing bridge to those canonical native objects; observe layout
   commits to pick up session-controller/identity changes.

This is **runtime instrumentation of private APIs**, not a supported Claude
extension contract. The adapter still wraps `turn.applyEvent`. Avoid describing
it as having no internal coupling merely because the executable bytes are intact.
Bun documents startup options for standalone executables; their success in this
Claude build was also checked empirically. [Bun standalone executables](https://bun.com/docs/bundler/executables)

### Update policy and the cross-version result

The same preload mechanism ran under Bun 1.4.1 in Claude 2.1.263 and Bun 1.4.0 in
the installed 2.1.243-musl build. Structural discovery found the Ink registry in
both, and a read-only probe found native session controllers in both.

**The current controller adapter does not support 2.1.243.** That build lacks
the `subscribeLayout` callback used by this adapter; native scope/turn APIs also
differ. The normal launcher refuses its unverified hash. An explicit isolated
`--probe-unverified` run returned `unsupported` without exposing a control socket.
The separate glibc 2.1.243 executable exited 139 even for plain `--version`, with
and without preloading; that is a baseline binary/runtime failure, not evidence
that our preload broke an otherwise working executable.

The packaging is consequently deterministic and fail-closed, **not maintenance
free**. For a new release: run discovery, use a fresh disposable compatibility
probe, inspect any contract mismatch, run the live suite and restart/reset tests,
then add the tested hash. Never turn discovery success into an automatic claim
of semantic compatibility. A version-specific adapter may be necessary even when
no executable patch is necessary. Only the observed 2.1.263 hash is enabled by
default today.

## Original patch experiment: what was established

| Capability | Evidence |
| --- | --- |
| Same native owner | Socket and terminal both operated native PID 912274, session `ded81f32-2b7f-4c5d-9123-dec5db87c07a` |
| Ordinary native tool registry | `Artifact` present without enabling it through an SDK-specific environment override |
| Prompt injection | Native message queue admitted a UUID-tagged prompt; normal terminal displayed it and its response |
| Streaming | Native `turn.applyEvent` exposed text and tool-input deltas, assistant blocks, and structured tool results |
| File permissions | Deny prevented the file; allow with modified input wrote exactly the host-approved replacement |
| Two-interface permissions | Socket answers dismissed the terminal dialog; a terminal answer resolved the socket-visible request |
| Native terminal input | Typing a prompt in the TUI produced user/assistant records observable through the socket |
| AskUserQuestion | Native question descriptor, answer submission, and structured answered-tool result captured |
| Reconnect and interrupt | Disconnect preserved the native turn and its pending permission; reconnect retained PID/session/dialog; interrupt cleared it without writing |
| Retry deduplication | Retrying a submission UUID produced one native user message; conflicting reuse was rejected |
| Artifact creation/update | One private counter published, then updated under the same Artifact ID; version and update metadata captured |

The automated live suite and offline tests are in this directory. The curated
[evidence record](evidence.json) identifies the runs and their limitations. Raw
captures are private temporary files on the test machine, not repository assets.
Native test sessions were stopped cleanly after verification. No lab
server is left running; the launcher creates a fresh session when needed.

### Artifact result

With explicit user approval, native Claude published a tiny counter with no
project/personal data, external assets, or tracking, then changed its button and
increment from one to two:

[Private test counter](https://claude.ai/code/artifact/af38d499-0cec-47b0-bc50-8b26ed69a39d)

The native result exposed `artifact_id`, `url`, `audience: "owner"`, version,
`updated`, contract, and live-subscription status. Both publish permissions were
approved using the native source-hash-pinned input. Creation returned
`updated: false`; the second publish returned `updated: true` with the same ID and
a new version. Native Artifact read also returned the first revision's HTML.

This establishes that the feature remains available and yields useful structured
results. It does **not** establish a complete Artifact editor protocol, embedded
rendering in Harness, or coverage of every live Artifact interaction. The hosted
page was not visually inspected; automatic browser opening was disabled.

## The native boundary shared by both loaders

This investigation used the actual installed ELF, not the earlier research
transcript's reconstructed historical source. Its Bun container includes source
and cached bytecode for 1,818 modules. The native REPL module exposes canonical
transcript, turn, stream, dialog, and message-queue handles at the `QKe` bridge
hook. That hook runs in the normal interactive REPL even with Remote Control off.
The newer preload loader reaches the same underlying native controller from the
mounted UI tree instead of injecting an import at that hook.

```text
native Claude process (one session owner)
  ├─ native terminal UI
  ├─ canonical transcript / turn / dialogs / queue
  │    └─ our small adapter → private Unix socket → client
  └─ existing Remote Control projection (disabled in these tests)
```

The adapter subscribes to those native stores and taps `turn.applyEvent`; input
uses `messageQueue.enqueueReportingAdmission`. Dialog answers go through the
native `dialogStore.answer`/`dismiss` path, and interruption uses `turn.cancel`.
It does not tail JSONL, reconcile separate hook streams, scrape terminal output,
resume a second session owner, or proxy Anthropic's Remote Control transport.
The PTY was used for ground-truth checks and the ordinary native terminal only.

This is still an adapter around several internal handles, not proof of one
universal public event bus. Not every product feature necessarily lives in these
stores. Native state remains authoritative; the client keeps no replacement
conversation and performs no tool execution itself.

### Optional binary-patch fallback

`patch_bridge.mjs` accepts exactly this Linux x86-64 build:

```text
Claude version: 2.1.263
Original SHA256: b9c407e36847bcb24b953b1390f240c840ae6c99e10a76475d2fadc5d5c4adca
Patched  SHA256: 333c83debd83564cf31cd99e3d5f4eee2256557dd771df528b870b2910978abf
```

It verifies the whole-file hash and unique REPL anchor, inserts 634 bytes that
import our adapter, and reuses that module's reserved bytecode allocation for
the modified source. It clears only that module's bytecode/cache pointers. Other
modules and ELF offsets remain in place. Merely changing source without dealing
with cached bytecode is not a reliable patch strategy.

The patcher writes a **new**, private executable and refuses overwrite. No
proprietary extracted source, executable, credentials, or account configuration
is included here. An update requires inspecting the new build and porting/testing
the patch; do not change the accepted hash just to defeat its version check.

## Run it

Requires Linux x86-64, Node (tested v26.7.0), and an authenticated exact Claude build.
Use the real ELF path, not the `claude` shell wrapper. From the repository root:

```sh
node --test research/claude-native-lab/bridge.test.mjs research/claude-native-lab/preload.test.mjs
node research/claude-native-lab/launch.mjs /home/sumeet/.local/share/claude/versions/2.1.263
```

The launcher prints a new private `/tmp/harness-claude-lab.*` directory, socket
path, discovery/hash results, and workspace, then opens normal native Claude in that terminal.
Accept trust only for this disposable workspace. There is no automatic prompt.
`--prepare-only` performs discovery/preparation without launching Claude. No copy
is made in the default `stock-preload` mode. Add `--patch` to explicitly run the
old pinned-copy experiment. `preload-status.json` records attachment or a failed
compatibility probe.

It uses normal CLI authentication without reading/copying credentials into the
adapter. Project/settings sources and MCP connections are excluded for the lab;
manual permissions, Remote Control off, no Chrome, and no browser auto-open are
requested explicitly for fresh sessions. Native resume may restore a conversation's
previous Remote Control setting; inspect `snapshot.remoteControl` rather than
assuming the startup setting overrides persisted state. This is **not an OS security sandbox**: an approved tool
still executes with the user's normal account. Only use synthetic lab prompts.

In a second terminal, replace the example socket with the printed path:

```sh
node research/claude-native-lab/client.mjs /tmp/harness-claude-lab.EXAMPLE/native.sock snapshot
node research/claude-native-lab/client.mjs /tmp/harness-claude-lab.EXAMPLE/native.sock tools
node research/claude-native-lab/client.mjs /tmp/harness-claude-lab.EXAMPLE/native.sock prompt '{"text":"Reply HELLO. Do not use tools."}'
```

`prompt` returns queue admission, **not** completion. Watch the normal terminal,
use `snapshot`, or use `run_prompt.mjs SOCKET NEW_PRIVATE_CAPTURE_PATH PROMPT`
to watch until completion or a pending permission. That helper leaves a pending
permission for you to answer; it does not silently approve anything.

Explicitly authenticated automated checks:

```sh
node research/claude-native-lab/verify_live.mjs --live /tmp/harness-claude-lab.EXAMPLE/native.sock /tmp/harness-claude-lab.EXAMPLE/workspace
```

This sends four prompts, denies/changes/intercepts narrowly scoped test writes,
checks streaming and deduplication, and disconnects/reconnects at a pending
permission before interruption. It prints the report location. It does not
publish Artifacts, launch agents, or change project files. The native session
must be idle. Do not simultaneously type or submit other prompts during a run.

Exiting a socket client does not stop Claude. Type `/exit` in this launcher's
native terminal when finished. This original `launch.mjs` route does not provide
a detached daemon; the separate [supervisor experiment](SUPERVISOR-AUDIT.md)
uses native background jobs. Temporary binaries/captures are retained, as is Claude's normal test
conversation history. Resume only a known stopped test session, never a UUID
still owned by another running Claude process.

The launcher has a restricted resume path for its own stopped labs:

```sh
node research/claude-native-lab/launch.mjs --resume-lab /tmp/harness-claude-lab.PREVIOUS /home/sumeet/.local/share/claude/versions/2.1.263
```

It refuses a live recorded PID and a matching live native session record. This
is not a distributed lock or protection against a concurrent third-party launch;
general session adoption and crash-safe single-owner coordination remain work.
The lab keeps normal Claude conversation persistence. No transcript is imported
into a separate SDK runner. A tested resume retained all 26 pre-exit message IDs,
used the same conversation UUID under a new PID, and correctly recalled an earlier
marker. A separate previously uninstrumented stock session also resumed with its
prior history and accepted a local socket prompt.

### Reproduce the alternative Remote Control probe

`probe_remote.mjs` uses a separately installed `ws` module (tested 8.18.3) and
normal existing CLI OAuth credentials. It neither copies nor prints credentials.
Start a **disposable stock** native session with `--rc` in a lab workspace, then:

```sh
node research/claude-native-lab/probe_remote.mjs --live session_YOUR_DISPOSABLE_ID /tmp/harness-claude-lab.EXAMPLE/workspace /PRIVATE_TOOLS/node_modules/ws/wrapper.mjs
```

This sends two authenticated model prompts, waits for a distinctive reply, and
denies only its specifically named Write fixture. It does not approve tools.
Private captures and a summary report go into the lab directory. A timeout leaves
any pending native operation for explicit terminal inspection; there is no blind
retry. Do not point it at an existing work conversation. Remote Control sends
test conversation activity through Anthropic's service. Its undocumented client
surface is an additional compatibility dependency, not a supported public API.
See [official Remote Control documentation](https://code.claude.com/docs/en/remote-control)
and the [Conductor protocol implementation](https://github.com/rmindgh/Conductor/blob/master/docs/protocol.md).

## Protocol and important limits

NDJSON over a Unix socket in an owner-only 0700 directory; socket mode 0600.
The trust boundary is the local OS user, not an independently authenticated RPC
principal. Anyone with access to that socket can submit prompts and approve tools.
No TCP listener, browser endpoint, token export, or generic JavaScript evaluation
method is provided.

Requests use `{ "id": 1, "method": "snapshot" }`; replies echo `id` and contain
`result` or `error`. Unsolicited events carry `event`, `epoch`, `sequence`, and
`data`. The initial `hello` includes protocol `harness-native-lab/0`, PID, and
session ID. `snapshot` returns the native transcript, turn, stream, pending
dialogs, and Remote Control state. Maps/Sets use explicit `$map`/`$set` wrappers.

Core methods are `snapshot`, `prompt`, `submission_status`, `interrupt`, `dialog_reply`, `tools`, and
`tool_schema`. Introspection and idle-only adapter reload are lab conveniences,
not a stable production API. Reload changes the adapter epoch and clears its
in-memory cache; it does not restart native Claude or erase durable send records.

- Prompts require an explicit UUID. The adapter writes the full request before
  native admission and an immutable outcome afterward, under the private native
  profile's `harness-adapter/submissions`. Duplicate IDs do not enqueue again,
  including after adapter reload or process restart. An incomplete operation
  remains uncertain. An exact native user-message UUID and text can establish
  acceptance later; absence never authorizes replay. A receipt proves admission,
  not execution or completion of a model turn.
- Harness saves the pending UUID with its composer draft before sending and
  consumes the draft only after a matching acceptance receipt. An edited draft
  checks the older send's status instead of substituting new text under its ID.
  Draft-file writes are locked and atomically replaced; send-journal merges
  preserve unresolved IDs and consumed-ID markers against stale snapshots.
  This is not yet a server-authoritative shared-draft system or a solution to
  all concurrent draft edits/mixed-version frontend writes.
- Sending requires snapshot capability `durableSubmissionDeduplication: 1`.
  Existing workers need the updated adapter; Harness does not restart them or
  modify the real user's hook as an incidental test. Old connections remain
  readable but cannot silently fall back to unsafe send behavior.
- Events are not a durable journal. Reconnect recovers current canonical state
  through `snapshot`, not every missed streaming delta. Production needs an
  explicit snapshot/event cursor contract and crash-safe admission handling.
- Full transcript snapshots are emitted on changes. This is intentionally not
  a scalable long-history transport. Clients retain only 1,024 recent events;
  optional captures retain the raw stream. Responses over 16 MiB are rejected.
- The bridge is plaintext-prompt-only with slash-command processing disabled.
  Native terminal `/clear` and stopped-session startup `--resume` were tested;
  these are not new RPC commands. Images, attachments, model/effort control, compaction,
  workflows/teams, foreground/background agents, MCP elicitation, and permission
  policy variants have not been covered by this experiment.
- Errors disconnect slow/malformed clients. This is basic prototype hygiene,
  not a security audit or proof that unknown native event variants serialize.
- Normal model/tool traffic still uses Anthropic. “Local bridge” means no hosted
  Remote Control hop; it does not mean offline inference or locally hosted Artifacts.

## Failures that changed the implementation

1. An off-the-shelf extractor's ELF-offset fallback misread this build and
   exhausted its Node heap. The bounded `.bun` section reader here extracted the
   installed container successfully; the downloaded extractor is not a dependency.
2. Resolving `dialogTransport.reply` directly completed tools but left terminal
   dialogs open. A real terminal-input test caught this. Answers now go through
   the native dialog store, and regression tests model the native host wiring.
3. A timing-based reconnect test using `sleep 30` failed because native Claude
   rejected that standalone command. It was not a disconnect-induced cancellation.
   No guard was bypassed; the final test waits at a native permission instead,
   asserting the turn is active both before disconnect and after reconnect.
4. A transient idle notification is not turn completion. Live tests require an
   advanced native `lastQueryCompletionTime`, not merely `isLoading: false`.
5. A query-string cachebuster did not reload the external module in this Bun
  runtime. Lab reload uses a new private module pathname instead.
6. Native session identity can change without a new process or controller object.
   The preload rechecks identity on native layout commits. `/clear` was observed
   on the existing socket as a new session with an empty transcript, and a fresh
   prompt completed successfully afterward.
7. The first packaged RC check timed out because it expected the reply text in
   `result.result`. The raw capture and native terminal showed a successful turn:
   RC sent an empty result, then the assistant text. The probe now requires both
   completion evidence and the actual assistant marker.

## What to build next

The result justifies a narrowly scoped **native Claude provider** experiment in
Harness. Keep native events and feature payloads intact in a separate transport;
adapt common transcript/tool/approval surfaces without pretending Claude speaks
Codex's protocol. Expose unsupported capabilities honestly and retain the native
terminal for operations not yet mapped.

Before normal use, establish process ownership outside the frontend, durable
admission/reconnect semantics, efficient transcript deltas, explicit dialog
schemas, and version compatibility checks. Test the remaining feature matrix,
especially background work and Artifact rendering. Keep stock-session hot attach
without restart as a separate unsolved requirement. A restart of a stopped stock
conversation is now proven, but arbitrary future releases are not. Background
supervisor migration also needs explicit testing: consumed preload environment
does not automatically follow a process into a different supervisor-owned worker.

The next implementation should use the preload route behind a capability-gated
provider, retain the stock terminal as an escape hatch, and avoid making the UI
own the native process lifetime. Keep Remote Control as a distinct optional
transport, not a required supplemental event source. The user-facing promise
should be “one supported startup/restart, unchanged installed Claude, tested build
compatibility,” not yet “attach to anything and never maintain the adapter.”
