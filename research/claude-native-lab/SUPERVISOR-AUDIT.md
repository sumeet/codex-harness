# Native supervisor + local adapter — 2026-09-06

**The native-supervisor route works with our adapter and the actual Harness
frontend.** This is now the preferred lifecycle direction, not just a source-code
lead. It is a tested research integration, **not yet the default Harness launch
backend** and not an upstream-supported semantic app-server.

**Follow-up:** the discovery/setup experiment below also demonstrated preserving
unwrapped workers across a supervisor handover and migrating one job at a time.
It found that native `attach` is **not** a universal missing-history guard.

## The new launch seam

Anthropic documents `CLAUDE_CODE_PROCESS_WRAPPER` / `processWrapper` for native
self-spawns, including supervised workers, warm standbys, respawns, and update
relaunches. The launcher must preserve argv, inherited environment, and PID via
`exec`; Windows does not support this contract. Running services need to pick up
the configuration through a restart. [Official corporate-launcher documentation](https://code.claude.com/docs/en/corporate-launcher)

Our wrapper uses Node's `process.execve`, adding the existing Bun preload only
to workers in the disposable workspace and to standbys awaiting assignment.
The latter attach only after receiving that workspace and native job identity.
It does not modify the installed executable, wrap an Agent SDK runner, or use
Remote Control as a second source of state. The socket still reads and controls
the canonical native controller through the version-gated private adapter.

The documented launcher contract does **not** make controller discovery, native
role flags, or the preload's private API assumptions supported contracts. Only
the previously verified Linux Claude 2.1.263 hash is accepted.

## Live results

Executable: `/home/sumeet/.local/share/claude/versions/2.1.263`, SHA-256
`b9c407e36847bcb24b953b1390f240c840ae6c99e10a76475d2fadc5d5c4adca`.
Private evidence root: `/tmp/harness-claude-supervisor.BXREBU`.

| Test | Result |
| --- | --- |
| Cold native-supervised worker | Adapter attached to native PID 988517; `claude attach 9b7dd5c1` opened the same worker |
| Four live controls on that worker | Permission denial, modified-input approval, streaming/deduplication, and pending-permission reconnect/interruption passed |
| Eight independent client processes during real native activity | All observed 156 events in the same order, without duplicates or gaps in their common interval |
| Warm standby promoted to a worker | Initial bootstrap failed; corrected lazy assignment attached to PID 990031 |
| Same live suite on the promoted standby | All four checks passed; eight client processes each observed 158 ordered events |
| Native respawn with persisted history | 990031 → 990244 → 991236 retained the conversation UUID and 27 messages; adapter epochs changed and stale commands were rejected |
| Two actual Harness windows | Both rendered the same native transcript through the existing shared rendering path |
| Harness composer roundtrip | Window 1 submitted a prompt; window 2 showed it streaming and then `SUPERVISOR_UI_ROUNDTRIP_OK`, from native PID 991236 |
| Forced idle-worker crash | SIGKILL of 991236 did **not** produce automatic recovery within 30 seconds; the transient supervisor subsequently exited |
| Explicit recovery after crash | `claude respawn b6eb99ef` started PID 992948 with the same UUID and 32 messages, including the UI marker; stock terminal attach and manual Harness reconnect worked |
| Two simultaneous native respawn requests | Both returned success; final worker 992947 had the same UUID, all previous user/assistant UUIDs, and 32 messages; at most one live owner was observed in 60 samples |
| Unsupported candidate | Wrapper rejected installed 2.1.243-musl before launch; the running supported worker's PID and epoch were unchanged |
| Offline regressions | All 22 tests passed, including six new wrapper/socket tests |

The competing-respawn test samples native registration every 100 ms. It is not
proof of an atomic global lock or coverage of every simultaneous start, migration,
resume, and shutdown entry point. Eight socket-client processes are not eight GUI
instances or a sustained load test. The GUI test used **two** frontends.

Nine model prompts were intentionally submitted: two runs of four narrowly
scoped live checks and one text-only Harness roundtrip. Tool writes were confined
to the private workspace fixtures. No Artifact was published in this pass.

### Evidence locations

- `jobs/9b7dd5c1/988517/checks-AL4ruW/report.json`
- `jobs/b6eb99ef/990031/checks-1MumPb/report.json`
- `watch-1788739052439.json`, `watch-1788739386464.json`
- `crash-1788739844756.json`: records the failed automatic-recovery observation
- `race-1788740341794.json`: competing respawns and message UUID preservation
- `launches.jsonl`, `assignments.jsonl`: whitelisted lifecycle metadata
- `frontend-two-live.png`, `frontend-two-roundtrip.png`: second Harness receiving
  the prompt submitted by the first
- `frontend-two-after-crash-recovery.png`: bounded reconnect retries exhausted,
  old history still visible
- `frontend-two-manual-reconnect.png`: error cleared after explicit reconnect

These paths are relative to the private evidence root. Raw transcripts, native
source, credentials, and binaries are not repository assets.

## Failures that changed the design

**Preloading only ordinary workers is insufficient.** Claude prewarms a spare
before it has a job or workspace, then claims that already-running process.
The first prototype let that spare run uninstrumented; the first respawn lost
the socket. `supervisor_preload.mjs` now waits for assignment before discovering
the controller. The complete live suite passed on an actual claimed spare.

**Resume flags can change identity.** Adding launch options to
`--bg --resume 9b7dd5c1` made native Claude explicitly create a copy,
`b6eb99ef-b399-4a0f-ab31-cf3335c0235a`, rather than continue the requested UUID.
That initially copied history was in memory but not yet materialized for the
new identity: an immediate respawn loaded zero messages. After running the suite
and persisting this new conversation, subsequent respawns retained its history.
Never present this flags-dependent copy as same-conversation adoption. Verify
the returned native UUID and persistence before offering restart/migration.

**Crash recovery is explicit in the tested idle case.** Documentation that the
wrapper covers crash respawns does not promise that every crash triggers one.
The existing Harness client eventually paused retries, retaining history; a
manual reconnect succeeded after explicit native respawn. It must not silently
launch another owner or replay a possibly admitted prompt when its connection
disappears.

**Existing discovery is still managed-host-specific.** The two UI fixtures used
private legacy catalog records plus a raw-byte Unix-socket relay, because the
current frontend rejects symlink socket paths and has no native-job backend.
This relay only forwarded the single adapter protocol; it did not merge event
sources. It is **test plumbing**, not the recommended production architecture.
The real composer, renderer, permissions protocol, and reconnect code were used;
native supervisor discovery and the Native terminal button were not integrated
through those fixture records. Stock `claude attach` was exercised separately.

## Production boundary and implementation order

1. Add a native-supervisor session backend to Harness's existing provider model:
   native job/conversation identity, canonical worker endpoint, and explicit
   lifecycle state. Do not disguise supervisor jobs as managed hosts or ship the
   fixture relay. Continue using the shared transcript and composer.
2. Make supervisor configuration an explicit, checked installation step. Do not
   overwrite an existing corporate wrapper or restart a user's background jobs
   behind their back. Our experiment used a transient daemon with a private
   configuration; it did not test migrating a pre-existing configured service.
   A production preload must also be installed in a durable, versioned location,
   not this checkout or a temporary directory.
3. Separate attach/reconnect from restart/adoption. Stock terminals can attach to
   the supervised worker without our preload in the attaching client. Arbitrary
   already-running interactive workers still require a controlled restart or
   migration; there is no demonstrated hot attach.
4. Fix the existing admission-uncertainty and shared-draft issues from
   [CONCURRENCY-AUDIT.md](CONCURRENCY-AUDIT.md), then fan-out/backpressure and
   sequence-gap recovery. This experiment does not fix those issues.
5. Validate a genuinely new Claude release before adding its hash. We tested
   rejection of another installed build, **not a live upgrade to a new version**.
   A documented relaunch hook improves packaging, not semantic compatibility.

## Reproduce safely

Use an account/machine with no unrelated native supervisor jobs. Inspect native
daemon state first. The diagnostic launcher does not implement a production
installer or arbitrate an existing service's wrapper configuration. Never run a
global daemon-stop operation to make room for this test without approval.

Prepare a private experiment (no native launch or model prompt):

```sh
node research/claude-native-lab/supervisor_lab.mjs prepare /EXACT/VERIFIED/CLAUDE
node --test research/claude-native-lab/bridge.test.mjs research/claude-native-lab/preload.test.mjs research/claude-native-lab/supervisor.test.mjs
```

Using the printed directory in place of `PRIVATE_DIRECTORY`, start a **new** job:

```sh
node research/claude-native-lab/supervisor_lab.mjs native PRIVATE_DIRECTORY --bg --settings PRIVATE_DIRECTORY/settings.json --setting-sources '' --strict-mcp-config --mcp-config '{"mcpServers":{}}' --permission-mode default --no-chrome --model sonnet --effort low --name harness-supervisor-lab
```

Use `native PRIVATE_DIRECTORY agents --json` to inspect the returned job,
`native PRIVATE_DIRECTORY attach JOB_ID` for initial native folder trust, and
Ctrl-Z to detach the native attach client. `snapshot PRIVATE_DIRECTORY` shows
the adapter endpoint. Ctrl-Z is the detach path verified here; native attached
and ordinary foreground sessions have different exit behavior.

Further diagnostic operations, all against the private directory:

- `watch PRIVATE_DIRECTORY JOB_ID MILLISECONDS`: eight actual client processes;
  run `verify_live.mjs --live SOCKET PRIVATE_DIRECTORY/workspace` separately to
  exercise real model activity. Watch output without activity is not load proof.
- `native PRIVATE_DIRECTORY respawn JOB_ID`: explicit native restart.
- `race-respawn PRIVATE_DIRECTORY JOB_ID`: two competing respawns, idle only.
- `crash-idle-worker PRIVATE_DIRECTORY JOB_ID`: **SIGKILL** an exact verified
  idle test worker and observe for 30 seconds. Does not recover it automatically.
- `harness-fixture PRIVATE_DIRECTORY JOB_ID INDEX`, then
  `relay-fixtures PRIVATE_DIRECTORY JOB_ID`: private GUI test plumbing, not
  supported user setup. Use isolated XDG paths, X display, and desktop bus.
- `native PRIVATE_DIRECTORY stop JOB_ID`: stop only this experiment's job.

Native test jobs, their transient supervisors/standbys, both GUI frontends,
the fixture relay, and the private X server/compositor were stopped after QA.
The user's Harness stayed running. Its configuration, the user's CLI wrapper,
global Claude settings, and installed binary were not changed. Private evidence
and normal disposable Claude conversation history were retained.

## Follow-up: native discovery and non-disruptive setup

Evidence root: `/tmp/harness-claude-supervisor.2MJ8Bw`.

Claude documents `agents --json --all --cwd PATH` for listing sessions including
completed ones, and `daemon stop --any --keep-workers` for stopping an on-demand
supervisor while leaving its workers alive. Session state and worker liveness are
different: a completed session can still have a PID. [Native agent-view documentation](https://code.claude.com/docs/en/agent-view#manage-sessions-from-the-shell)

The listing command returned the previous experiment's stopped/done jobs while
no supervisor was running. We do not need a second authoritative conversation
catalog just to populate Harness's sidebar. Our endpoint records can remain
ephemeral connection metadata keyed by the native job identity. Neither the
listing nor our discovery probe woke the stopped jobs in these checks.

### Staged handover worked

1. Started **uninstrumented** native background job `e866aff0`, worker PID
   996683, in a fresh private workspace. `/proc` confirmed it had neither Bun
   preload options nor the Harness bootstrap environment. A terminal `/rename`
   created local conversation history; no model prompt had yet been sent.
2. Verified that the only live native session was this disposable job, then
   stopped its supervisor using `--keep-workers`. Worker 996683 stayed alive.
3. Started a new supervisor configured with the lab wrapper and a new job,
   `f6c71021`, worker 997116. The new job exposed the adapter; the original job
   stayed at PID 996683 and remained uninstrumented.
4. A second keep-workers handover loaded the updated endpoint metadata bootstrap,
   again preserving both workers. Explicitly restarting **only** `e866aff0`
   produced adapter-enabled PID 998924, the same native conversation UUID, and
   its four messages. Worker 997116 was unchanged.
5. Sent one tools-disabled marker prompt after a later restart through the new
   directly bound connection. After the history fault tests and restoration,
   worker 1000571 loaded the same UUID and 11 messages, including the exact
   submitted user UUID and `HANDOVER_HISTORY_MARKER` reply. Worker 997116 still
   had its original PID throughout these operations.

This shows a staged setup path, **not zero-disruption universal migration**.
Only idle disposable jobs were tested. Existing terminal connections, in-flight
tools, permissions, teams, and workflows across a supervisor handover were not
tested. Unwrapped interactive terminal sessions still require native backgrounding
or another controlled restart; this experiment started with a native background
job. We did not alter global settings or compose an existing corporate wrapper.

### Direct endpoint discovery, without the fixture relay

`supervisor_catalog.mjs` is a read-only research implementation:

```sh
node research/claude-native-lab/supervisor_catalog.mjs PRIVATE_DIRECTORY
```

It asks the stock CLI for native rows, validates our private endpoint metadata,
and connects directly to the worker's Unix socket. It verifies native job ID,
PID, boot ID, Linux process start ticks, executable, workspace, UID, socket type,
protocol, conversation UUID, and adapter epoch. It rechecks identity and endpoint
publication after the handshake to reject a concurrent replacement. It does not
follow the stable symlink or start a relay. The new worker publishes this process
fingerprint in endpoint metadata version 1.

Observed states included an untouched live worker with no adapter, an old
adapter lacking the new metadata version, a fully bound worker, and dormant jobs
with no PID. A stale record cannot establish liveness by itself. Unknown/mismatched
records remain unavailable rather than triggering automatic restart. The trust
boundary remains the local OS user, not an independently authenticated principal.

The probe's `canSend` describes a usable transport, **not proof of history
continuity**; connected results explicitly mark continuity as not checked. This
is not wired into the production Rust backend or sidebar yet. Four additional
offline tests cover process-stat parsing, PID/boot/conversation mismatches,
independent native-state/liveness classification, and symlink rejection. All
26 adapter/bootstrap/catalog tests passed.

### `attach` can wake an empty conversation under the same UUID

This was a consequential failed hypothesis. For stopped job `e866aff0`, we
temporarily renamed **only its own lab transcript** out of native discovery,
then invoked stock `claude attach` in a private PTY:

- With local-command-only history, attach started a new empty worker.
- After a real assistant reply had been saved, the same experiment started
  worker 1000275 with **the same conversation UUID and zero messages**. The
  adapter confirmed it was idle; this was not merely a truncated terminal view.

Both probes timed out because the empty native TUI remained open, not because a
history refusal was returned. Cleanup stopped the unexpected worker, restored
the original transcript, and checked its exact byte hash. A subsequent normal
restart restored the user-message UUID and assistant marker. The initial attempt
to move the file across `/home` and `/tmp` failed with EXDEV before changing it;
the working probe uses a temporary name on the same filesystem. No test history
was deleted or left withheld.

Current installed-source inspection explains the limit: missing-transcript
refusal is conditional, including special fork-handoff/dead-epoch cases; it is
not a universal rule for every stopped job. The history-fault job was originally
created idle with `--bg`, then used conversationally. This is relevant to Harness's
new-session-then-composer workflow. It does not prove the behavior for every
native launch mode or that a model/tool prompt was replayed in this experiment.

Private reports: `missing-history-1788741491995.json` (local commands),
`missing-history-1788741635458.json` (assistant history), `handover-marker.json`,
and `followup-report.json`. Native source remains outside the repository.

**Production implication:** keep ordinary reconnect strictly read-only. Before
an explicit wake/adoption, require saved-history preconditions; after it, verify
conversation identity **and expected history continuity** before enabling sends.
File existence alone is not an atomic guarantee against deletion/races, and UUID
equality alone passed the empty-history fault above. Do not treat `attach` as a
universal safe replacement for `respawn`, or silently resend an uncertain prompt.
The complete guarded-wake implementation remains work, not a claimed fix.

All follow-up workers, terminal clients, standbys, and transient supervisors were
stopped afterward; no GUI was launched. One model prompt was sent, with tools
disabled by its explicit instruction (not an OS sandbox). User Harness PID
906724 and the installed Claude executable were left unchanged.
# Product discovery follow-up — 2026-09-06

The earlier managed-host catalog/byte-relay GUI workaround is no longer needed
for discovery: `claude_sessions.rs` now consumes the native catalog, validates
the endpoint binding and socket peer, and connects directly from Harness. Saved
native conversations are also indexed independently of the supervisor catalog
and opened read-only through the shared transcript renderer. The wrapper remains
an opt-in lab setup; installation/adoption/resume are not shipped yet.

Live QA used only a fresh disposable job `81d835f9`, native UUID
`81d835f9-b35f-4565-9234-12c4db57d9cd`, worker PID 1014021, in
`/tmp/harness-claude-supervisor.xnHaC1`. Harness discovered it without a managed
session record or relay. The actual GUI composer submitted one tool-free prompt
and displayed `HARNESS_DIRECT_UI_OK`. After an explicit stop, the GUI retained
the transcript, refreshed to saved read-only history, and kept an unsent draft;
clicking Send did not append that draft or wake a worker. The native reader smoke
test read 25 local conversation entries (including research fixtures), validated
one direct live connection before stop and zero afterwards, with no catalog
warnings. Screenshots are private under the experiment's `ui/` directory.

All five experiment-owned supervisor/PTY-host/worker/standby PIDs exited. The
user's existing Harness PID 906724 was untouched. No global Claude settings,
installed CLI wrapper, or binary were changed. No existing conversation was
resumed. The native profile retains this disposable test conversation.

## Packaged setup follow-up — 2026-09-06

Harness now embeds a production installer/dispatcher in `claude_setup.rs`. The
sidebar explicitly asks before configuring Claude's user-level `processWrapper`;
it also supports update/disable. The setup lock and immutable packages belong to
the native Claude profile, not a particular frontend's XDG application state.
Runtime endpoint namespaces include a hash of the canonical native profile.
Managed-host launch (+), arbitrary terminal handoff, and guarded wake are separate
from this installer and have not been migrated by it.

Eight Rust setup tests exercise exact settings backups, preservation of unrelated
keys, changed-settings refusal, launcher conflicts, immutable asset validation,
old-package ownership across interrupted updates, private/symlink boundaries,
and concurrent installer exclusion. `packaged_setup.test.mjs` additionally runs
the actual standalone executable in a disposable profile: prepare leaves settings
unchanged, enable/re-enable/disable round-trip, unrelated launchers are untouched,
the exec chain retains PID/argv/environment, and missing/unsupported/conflicting
adapter startup falls back without pre-exec output. Evidence from the first
successful executable run: `/tmp/hs-H41p0l`.

A live test staged the actual package without enabling it in user settings, then
passed its launcher only to a new disposable native supervisor. Cold worker
`8829e681` (PID 1033998, UUID `8829e681-2835-476b-8de9-a8994a211c03`) and assigned
warm standby `44860356` (PID 1033997, UUID
`44860356-aa4d-4520-8884-909e4258efde`) both exposed verified endpoints, in two
different private workspaces. The real Rust catalog/snapshot smoke test validated
two direct connections with no endpoint-root override, managed catalog, or relay.
No model prompt was submitted. After stopping only these jobs, it validated zero
live connections; the experiment's supervisor/PTY hosts/workers/standbys exited.
The user settings SHA-256 before and after was
`acbc5c5fbdb8c0064698abd9bf3e545ac63d2dfe861aeeb3de06ed8d5f947815`.
Private prepared assets remain, but no startup hook is globally enabled.

An isolated real Harness window exercised setup/cancel/enable/manage/disable on
`/tmp/hs-5ZGss8/p`, preserving its sentinel setting. This caught clipped consent
text in GPUI's fallback prompt; constraining and wrapping its text now shows the
whole explanation. Screenshots are under `/tmp/hs-5ZGss8/ui`. The normal Harness
binary was rebuilt, without restarting user Harness PID 906724.
Final verification passed 268 default Harness tests (two explicitly ignored),
all 27 adapter/packaged-setup tests, and the separate live catalog smoke checks.
The isolated GUI/Xvfb/compositor processes were stopped after validation.

Remaining research should target explicit handoff and wake: capture expected
saved-message identity before migration, preserve the previous owner until an
explicit handover, and reject a replacement that has the same UUID but lacks the
expected history. Include concurrent frontends, missing/partially written history,
active permissions/tools, and worker death at each handoff boundary. Ordinary
reconnect must never become an implicit wake or resend. The already documented
post-admission uncertainty/deduplication and new-version compatibility questions
remain; passing installer tests does not resolve those separate contracts.

## Stopped-session continuation in Harness — 2026-09-06

The previously missing user workflow is now implemented for stopped saved native
conversations. **Continue conversation** calls native `--bg --resume FULL_UUID`,
without an initial prompt or extra options, then verifies the actual worker before
enabling the shared composer. A running ordinary terminal is refused rather than
restarted. **New Claude session** is now explicit in the sidebar, and the no-worker
state explains how to continue saved history instead of only describing setup.

Exact installed-source inspection identified `own-options` as a reason an existing
job's resume becomes a copy. Both first adoption of saved history and wake of an
already-known stopped job passed with the narrow command. This was verified with
stock 2.1.263, not inferred solely from its documentation.

The persistent continuation record belongs to the native profile and contains the
expected message count/semantic digest and source-file digest. Normal Harness
connections and actions reject pending verification or a changed PID/start time,
boot, or adapter epoch. Restored UUID alone is insufficient: ordered user/assistant
UUIDs, roles, and content must also match. A per-conversation file lock serializes
Harness continuation attempts. After uncertain launch outcomes, an existing worker
can be reverified without another launch; missing-worker recovery is deliberately
not an automatic retry. These are checks and coordination for Harness clients,
not a global atomic ownership lock imposed on unrelated native terminal launches.

`resume_workflow.test.mjs` ran entirely in separate Claude configuration/runtime
directories, with no copied credentials or model prompts:

- `/tmp/hr-24HGXo`: two-message synthetic saved history, UUID
  `3da749dd-ec33-4860-a63c-1f1f52fe55f5`.
- `/tmp/hr-NkLFqn`: a copy of the previous disposable supervisor lab's selected
  history branch, including real tool activity, with 17 user/assistant messages;
  the isolated copy's UUID is `db10bd9d-0583-4f9b-a525-9430ddc2eb53`. Its original
  profile/conversation was only read, never resumed or modified.

Both passed adoption, same-live-worker reuse, stopped-job continuation, competing
Continue requests leaving one worker, failed history-verification refusal,
same-PID reverification, and missing-transcript refusal without waking a job.
The restored native turn reported zero submissions: continuing never sent a
new prompt. The test fixtures remain only in their private temporary profiles.

An isolated actual Harness window opened the first saved transcript, clicked
Continue, and reached **Connected to native Claude** with the original history.
After typing an unsent draft and stopping only that test worker, a second Continue
restored the conversation and preserved the draft without submitting it. The
normal Rust read-only catalog/snapshot check validated that resulting connection.
Screenshots are under `/tmp/hr-24HGXo/ui`, including `saved.png`, `continued.png`,
`stopped-draft.png`, and `continued-draft.png`.
Closing the test Harness window left worker PID 1067468 alive; the CLI continuation
entry point reverified and reused that same PID. Cleanup then stopped only the
isolated jobs and GUI/Xvfb/compositor processes. User Harness PID 1038023 and the
user's real native sessions/settings were untouched. The normal standalone binary
was rebuilt. Final checks passed 274 Harness tests (two ignored), 27 offline/package
tests, both isolated resume-workflow runs, and the actual GUI acceptance check.

Remaining: active terminal handoff, safe terminal attachment for supervised jobs,
recovery when an uncertain launch has no worker, broader compaction/normalization
coverage, and true simultaneous third-party native launch races. The preexisting
adapter admission-uncertainty and upgrade-compatibility issues are not solved by
this continuation implementation. Do not promote passing fixtures into an
update-independent or arbitrary-session parity claim.

## Coherent lifecycle investigation — 2026-09-06

The user clarified that success means the same coherent thread-selection and
working experience as Harness's existing Codex path, not individually functional
Claude setup/continue/managed-host features. No production lifecycle behavior was
changed in this investigation. The separate new-session and resume backends are
still present; removing their visible buttons is not an architectural fix.

`lifecycle_probe.mjs` tests the unified native-supervisor candidate, with no model
prompts, credentials, or real conversation changes:

```sh
node research/claude-native-lab/lifecycle_probe.mjs \
  /home/sumeet/.local/share/claude/versions/2.1.263 \
  target/release-fast/harness
```

It enables packaged instrumentation only in a new private Claude profile. It
deliberately repeats a create and kills one of its own native launchers, then
stops every live job in that isolated profile. A completed probe records defects
as well as successful capabilities; it is not a production-readiness test.

Latest evidence: `/tmp/hl-dqPmkE/report.json`. Earlier runs are
`/tmp/hl-gOEQPn/report.json` (no caller-death fault) and
`/tmp/hl-drLsYp/report.json` (fault repeated; the earlier reporter incorrectly
labeled a signal-terminated caller's null exit code as zero, while correctly
recording SIGKILL). The final probe fixes that reporting bug and asserts SIGKILL.

| Question | Observed result | Consequence |
| --- | --- | --- |
| Can a new empty thread use native supervision? | Promptless `--bg` produced a live instrumented worker with zero messages/submissions | The older managed-host backend is not required merely to support creating a thread |
| Can two clients connect? | Both saw the same PID and adapter epoch | Empty native threads support the same multi-client transport; no two-window UI test was repeated here |
| Can the caller choose a retry-safe creation UUID? | `--bg` explicitly ignored `--session-id`; repeating identical argv created two UUIDs/workers | Never retry native creation as though it were idempotent |
| Can an empty stopped native thread reopen? | Bare `--bg --resume FULL_UUID` restored the same UUID despite having no transcript file | Missing history is not necessarily lost history; a proven empty thread needs a distinct validation rule |
| Does Harness support that empty-thread lifecycle? | `--claude-continue` refused both stopped and live empty threads with `Choose a conversation with saved history` | Existing continuation policy is incomplete, not a native capability limit |
| What if the create caller dies before acknowledgment? | SIGKILL after native state publication left a discoverable blocked job, no PID, and no stdout acknowledgment | Native publication can survive the caller |
| Can that particular interrupted creation recover? | Reopened the published UUID with `--bg --resume`; snapshot confirmed that UUID, zero messages/submissions | Recovery can reconcile an existing job instead of repeating create, when its identity and empty-history provenance are known |

The final run's empty conversation was
`e0448157-1370-4c85-b4f7-4c9337a4a3fd`; its deliberately duplicated create was
`4fc98a2d-92dc-4a5e-89cf-95b67f7ca57f`. The interrupted creation recovered as
`cd8cd8e9-6706-41a1-834b-ed83cae6bf00`. All test workers, standbys and transient
supervisors exited after cleanup; user Harness PID 1038023 remained running.

This fault is one precisely observed boundary, not crash recovery in general.
The diagnostic learns the native UUID by watching its own isolated job directory.
It does not implement a crash-safe binding from a durable Harness create request
to that UUID under arbitrary external concurrent launches. Names and a catalog
set difference alone are not sufficient proof of that binding. Installed-source
inspection shows native dispatch has its own short ID/nonce and acknowledgment
reconciliation, but those are private interfaces; directly using them would add
another compatibility dependency. Compare that option against a coordinated
CLI launch with reliable identity capture before choosing an implementation.

### Preserving behavior requires more than preserving messages

The [official agent-view documentation](https://code.claude.com/docs/en/agent-view#how-file-edits-are-isolated)
describes background worktree isolation. Native backgrounding also carries
configuration and in-flight work, with documented exceptions and confirmations.
Those behaviors must be included in the integration's acceptance criteria.
Installed 2.1.263 source distinguishes a REPL handoff from a shell dispatch:
the former explicitly sets `bgIsolation: "none"` and interactive lineage; a new
shell dispatch can inherit the default isolation policy. Our empty test workspace
was not a Git repository, so it did not empirically test that policy difference.
Do not silently change user/project isolation settings to make an experiment fit.
Saved-history adoption via CLI is not evidence that original launch-only MCP,
settings, permission grants, and background tasks were all preserved.

The existing offline `probe_concurrency.mjs` was rerun unchanged: eight identical
submission requests still produce one admission normally, but an exception after
admission followed by retry produces two admissions. Adapter replacement still
loses its in-memory deduplication ledger. These remain defects/limitations, not
fixed by the successful empty-thread recovery probe.

Read-only discovery of installed 2.1.243-musl again found its registry, but that
build is unverified. The prior failed controller-compatibility result still
applies. No new version was enabled, installed, or live-tested in this pass.

### Next decision gates

1. One lifecycle API for new, existing-live, and stopped conversations, with the
   native supervisor retaining worker ownership. Distinguish deliberate open/wake
   from passive reconnect; neither operation may implicitly submit work.
2. Durable creation identity and delivery outcomes outside frontend windows.
   Prove recovery at each publication/admission boundary, including a missing
   transcript that used to contain messages versus a known-empty conversation.
3. Native terminal handoff preserving configuration, workspace policy, dialogs,
   and background work. Test a real ordinary terminal with launch overrides, not
   just a copied transcript. Keep explicit consent where native migration changes
   or restarts work; do not disguise that as a harmless connect.
4. A complete two-window journey through create, send, approval, disconnect,
   frontend exit, worker crash and reopen, plus real compacted histories and
   missing feature mappings. The current isolated zero-submission test is only
   the initial lifecycle slice.
5. A compatibility matrix across several native releases. Fail-closed packaging
   protects against unsupported control but does not deliver uninterrupted use
   through updates. Measure maintenance before promising app-server-like stability.

## Thread opening and terminal handoff — 2026-09-06

This pass implements the saved-thread opening path and tests native terminal
handoff through the real Harness binary and GUI. It does not unify new-session
creation or establish crash-safe operation. Tests use isolated Claude profiles,
synthetic saved histories, no copied credentials, and zero model prompts.

`terminal_handoff.test.mjs` launches the stock, unpreloaded 2.1.263 executable in
a real PTY with a two-message fixture, a launch-only settings file containing a
harmless environment sentinel, an extra allowed directory, an empty explicit MCP
configuration plus strict-MCP flag, and `--model haiku`. Opening that still-owned
conversation in Harness must refuse with `/bg` guidance and leave its PID alone.
The test then types native `/bg`, without injecting a model prompt.

The native handoff is **not** same-UUID migration. Native writes a `continued-in`
record to the source JSONL, forks the retained history into a new conversation,
and creates a supervised worker. In the final run, source
`fcb75988-fd0a-4c5b-8425-59645e8c9592` (PID 1098357) became
`ca15b834-2496-45bf-b2bd-d9015d89e251` (PID 1098710). The original message UUIDs and
contents were retained, along with the tested settings/MCP/directory/model flags,
environment sentinel, `interactiveLineage: true`, and `bgIsolation: "none"`.
Native appends local `/background` command records after the redirect; therefore
the redirect need not be the last record and restored history may have a suffix.
The copied target history may initially exist only in worker memory, with no
target JSONL. Do not wake such a stopped target using only the source's history.

Source inspection of the installed build's background command corroborates the
observed `--resume <source path> --fork-session` dispatch and the explicit
continued-in record. Extracted native source remains outside the repository.
This test does not exercise actual MCP tool behavior, in-flight tool handoff,
worktrees inside Git, permissions already granted in a terminal, or artifacts.

Production changes:

- A deliberate sidebar click opens/verifies saved native history and connects;
  there is no second Continue mode. List refresh and reconnect monitoring remain
  passive, and failures offer Retry opening. Neither path submits the draft.
- Native `continued-in` chains are followed only within the selected profile.
  Full bounded reads reject partial/malformed redirects, conflicting destinations,
  missing targets, and cycles. Every source's semantic prefix must match the live
  target (or its complete saved history before a stopped wake).
- A live handoff retains the strongest applicable source, destination, or prior
  pending history requirement. A version-2 verification record can refer to its
  origin file while still binding controls to the destination's actual UUID,
  PID/start time/boot, and adapter epoch. A conflicting prior proof is not erased.
- Logical selection and the composer draft key survive the endpoint UUID change.
  This is not yet global coalescing of both native catalog entries or merging
  independently written drafts against both IDs; both rows can still be visible.
- Ordinary `agents --json --all` terminal entries have no background job ID.
  They are now represented as interactive owners instead of causing a `missing
  field id` parse failure. Background endpoints still require a real job ID.
  A terminal view of a supervised worker does not replace its background binding.
  Default native ID labels no longer replace meaningful saved conversation titles.
- Successful prompt admission is recorded before fallible native bookkeeping.
  A later failure returns accepted-with-warning; matching retries preserve that
  result and do not enqueue twice. This fixes the earlier reproduced fault only.
  Exceptions inside admission and ledger loss across adapter restart remain open.

With `HARNESS_RESUME_GUI=1`, both executable regressions also launch Harness on
private Xvfb displays. `gui_probe.mjs` uses `xdotool search --all --pid ... --name
Harness`; omitting `--all` initially targeted the first window twice. That test
bug was corrected before accepting multi-window evidence. Each frontend has its
own XDG configuration but shares the disposable native profile/worker.

Final evidence:

- `/tmp/hr-3TRuUx/ui`: single-click saved-thread opening, retained unsent draft,
  a second independent frontend selecting and rendering that same conversation,
  and one unchanged worker after both windows close. The regression subsequently
  tests stopped wake, competing open requests, verification failure/recovery,
  and missing-history refusal.
- `/tmp/ht-YcF4ma/report.json` and `ui/handoff-opened.png`: ordinary terminal
  refusal, native handoff, selecting the original sidebar row, verified target
  connection with original selection/draft retained, and refusal of a deliberately
  conflicting prior verification without launching/restarting a worker.
- `/tmp/hs-snd91x`: packaged setup regression against the rebuilt executable.
- 277 Harness tests passed, two explicitly ignored; 27 offline adapter tests
  passed, three opt-in executable tests skipped in that offline run. All three
  executable tests then passed with the explicit verified binary and GUI flag.
  The eight-process concurrency probe now reports one admission for its injected
  post-admission failure/retry; adapter-restart retry remains non-durable.

All test windows, native processes, Xvfb displays, and compositors were cleaned
up. The normal `target/release-fast/harness` was rebuilt; user Harness PID 1038023
was left running, and no real profile was changed. Native new-thread creation,
durable creation identity, crash recovery, alias catalog coalescing, compacted
history coverage, terminal-only feature presentation, and multi-version adapter
support remain necessary before calling this Codex-equivalent everyday use.

## Native creation, logical conversation rows, and startup presentation

The subsequent implementation replaces new managed-host creation with a private
durable creation request and a detached, short-lived coordinator. A unique empty
`--settings` file is retained in native job `respawnFlags`, allowing recovery to
identify the actual job even after the creating Harness process dies. Claude's
supervisor owns the worker. The marker does not change workspace isolation,
permission, or Remote Control policy. Once dispatch is recorded, absence of a
matching job is an uncertain outcome, not permission to repeat native creation.

`creation_workflow.test.mjs` exercises production CLI creation, loss of the
creating frontend, competing recovery callers, two GUI clients, and an ambiguous
request without a published job. Private run `/tmp/hc-OSM4nq/report.json` passed
with zero model requests and no repeated creation. Its terminal test also
establishes a separate policy: explicit **Open in Claude terminal…** calls stock
`attach`, which may wake a stopped job. The UI requires confirmation. Normal
Harness opening still refuses stopped empty/missing-history conversations; the
terminal escape hatch is not described as a safe no-wake attach.

The logical catalog now coalesces explicit native handoff aliases. It retains
separate saved drafts instead of overwriting one with another. Recognized pending
creation records attach to their actual native conversation row; records without
a discovered worker have a logical conversation target, not a fabricated native
Session. When recovery completes, the original creation ID remains a selection
and draft alias. Native `/background` command envelopes are displayed as a single
card while their original records remain available as raw data.

The first failure screenshot exposed a separate startup-request panel containing
backend diagnostics in the sidebar. The user rejected that presentation. It was
real proposed UI in a simulated failure test, not a debug-only panel. The revised
UI has a normal project-named row and short failure status. Selecting it retries
reconciliation of the same request, with a short explanation and collapsed
technical details in the main pane. The ordinary sidebar does not expose request
paths, backend phases, or duplicate recovery controls. Pending requests with an
already-published native job must not become a second conversation row.

These changes do not settle prompt-delivery uncertainty. The current Send path
still allocates a fresh UUID on retry, and adapter admission deduplication remains
in-memory. A lost acknowledgement followed by another Send can duplicate an
already-admitted prompt. This requires durable intent/admission handling, not an
additional automatic retry. Multi-version compatibility and broader native
feature coverage also remain open.

Final executable validation after the sidebar correction:

- `/tmp/hc-DzRU8w/report.json`: creation recovery and duplicate-row regression;
  `ui/failed-conversation-opened.png` and `ui/failed-conversation-details.png`
  were visually inspected. The disclosure works and diagnostics remain out of
  the sidebar.
- `/tmp/hr-tawpaI`: saved-conversation opening, including both competing callers
  succeeding against the same worker after the continuation lock wait.
- `/tmp/ht-VX1K7D`: ordinary terminal handoff, one sidebar conversation, both
  saved drafts retained, and prior verification still enforced.
- `/tmp/hs-Jt7n9x`: packaged setup preserves settings and delegates safely.
- All four executable tests passed without model requests. All 286 Harness
  unit tests passed (two ignored); all 27 offline adapter tests passed.

The normal standalone binary was rebuilt, without restarting user Harness
PID 1038023 or modifying the real Claude profile. Prompt-delivery work was paused
for this user-requested UI correction and remains unfinished as described above.

## Persisted send identity and durable admission receipts

The following hardening pass addresses the reproduced lost-acknowledgement retry
failure. The user does not need to test real conversations before this engineering
work can proceed.

`bridge.mjs` now requires an explicit submission UUID and claims a private durable
record before calling native queue admission. Request text and identity are saved
in the native profile; completion is a separate immutable record. Exclusive
creation serializes competing processes, and file plus directory synchronization
precedes admission. A missing/partial outcome never permits a second enqueue.
Accepted receipts survive adapter reload and process restart. An exception after
queue mutation is uncertain, not a retryable rejection. If the authoritative
native transcript later contains the exact user-message UUID and text, a separate
immutable reconciliation record can confirm acceptance without resubmission.

Harness persists a pending ID with its draft before dispatch. Retry reuses that
ID; edited or cleared drafts query the older send instead of changing its input.
Only a matching accepted receipt consumes the draft. A definite rejection retires
the ID while retaining the draft. Draft publication now uses an exclusive lock,
private atomic replacement, and synchronization. Pending-journal merges retain
unresolved IDs and consumed-ID markers against stale saves; conflicting unresolved
IDs cause refusal before dispatch. Saved receipt markers also prevent a stale
pending snapshot from restoring already-accepted draft text. These protections
are not a complete concurrent draft-text/revision protocol.

The updated adapter advertises `durableSubmissionDeduplication: 1`; new Harness
sends require it. Existing native workers were not restarted or updated by this
test pass. Updating the normal Harness executable alone does not replace code
already loaded into those workers. The backend request/receipt data lives in the
native profile; frontend draft/correlation metadata still lives in Harness's draft
store. Do not claim all drafts are now server-authoritative.

Evidence includes 35 offline/runtime tests, an eight-client-process socket probe,
and real Harness windows driven against an explicitly synthetic managed transport
to inject lost replies. `/tmp/hq-u5tmtq` verifies frontend restart plus changed
adapter epoch, one admission for a retried ID, intentionally repeated text using
a distinct ID, and retention of uncertain/edited drafts. The native runtime file
probe uses the installed 2.1.263 executable's Bun preload before CLI startup, exits
before any model request, and verifies actual durable filesystem operations.
`BUN_BE_BUN=1 --eval` was not supported by this executable; no implementation
depends on that attempted shortcut.

The latest full lifecycle rerun at this checkpoint passed: `/tmp/hc-scgzDM`
(creation), `/tmp/hr-6mcBzQ` (resume), `/tmp/ht-0MGJtC` (terminal handoff), and
`/tmp/hs-num6Cp` (setup). All five executable workflows passed without model
requests; all 290 Harness unit tests passed, with two ignored.

Remaining boundaries matter: acceptance is not proof that a queued request
finished or survived every native-worker crash; complete shared-draft revision
handling and mixed-version frontend writers remain unverified; pending sends
across native `/bg` UUID changes are refused rather than guessed/migrated; native
feature coverage, stopped-empty recovery, and new-version compatibility are still
unfinished. No automatic prompt replay, credential copying, real-profile hook
change, or user-worker restart was added.

## Xwayland incident and corrected GUI-test containment

The user reported a missing `/tmp/.X11-unix/X0` and repaired it with a link to
the still-working `X0_` socket. The old GUI helper launched bare
`Xvfb -displayfd 3`, assuming automatic allocation meant an isolated display.
That assumption was wrong. A reproduction using the installed Xvfb 21.1.24
inside Bubblewrap's private mount/network namespaces showed it calling
`unlink("/tmp/.X11-unix/X0")`, binding a replacement, and unlinking it on exit,
despite a live fake listener and an existing lock. `X0_` survived. The original
test runs were not traced, so attribution is very likely, not a complete
forensic proof. Earlier statements that those GUI tests left the user's display
untouched should not be relied on. The user's socket repair was preserved.

`gui_probe.mjs` now runs Xvfb and all GUI-driving tools through Bubblewrap with
private mount/network namespaces. Host `/tmp` is hidden; only the disposable
fixture and a new private X11 socket directory are writable. Xvfb uses explicit
`:0` within that private directory, never the host display namespace. There is
no unsandboxed server/tool fallback. Failed isolation stops the test. Process
IDs and display paths are recorded in `ui/isolation.jsonl`.

Putting Harness itself in the child user namespace initially broke verification
of native workers outside that namespace: reads of `/proc/<pid>/{exe,cwd}` were
denied, and live rows became unavailable. Do not weaken native identity checking
to accommodate a test sandbox. Harness now connects using XCB's absolute private
socket-path support while retaining normal process visibility. Test driving and
screen capture remain sandboxed. This is display containment, not a claim that
the entire application/native supervisor is security-sandboxed.

The final arrangement passed these executable workflows with zero model requests:

- `/tmp/hq-4EqRI0`: lost send acknowledgement, frontend restart, changed adapter
  epoch, intentional repeat, and retained uncertain/edited/cleared drafts.
- `/tmp/hc-eDLYl1`: creation recovery, two frontend windows, and worker lifetime.
- `/tmp/hr-fpBWNZ`: single-click resume and competing frontend requests.
- `/tmp/ht-cfzEOd`: stock terminal `/bg` handoff, preserved settings/history/drafts.
- `/tmp/hs-FgxnBQ`: packaged setup and safe hook delegation.

`gui_probe.test.mjs` checks mount/network separation before any X server starts,
two simultaneous displays, preservation of the real X0/X0_/lock identities,
post-teardown connection failure rather than desktop fallback, and missing
Bubblewrap refusal. No user Harness, Xwayland, or Claude worker was restarted.

The final send-pass unit rerun also exposed an intermittent test-only startup
lock assertion. Parallel tests can fork while a descriptor is open, retaining
its flock until the child execs. The host-lock fixtures already account for this;
the startup-lock test now uses the same bounded release wait. Production locking
was not weakened or replaced.

Repeated full-suite runs then exposed the same descriptor inheritance in the
legacy host-status probe itself, not only in an assertion: the probe's acquired
lock could outlive the probe through a concurrent fork. `probe_host_lock` now
explicitly unlocks only its own successful temporary acquisition before returning.
A genuinely held ownership lock is never unlocked. A deterministic regression
retains a duplicate descriptor (the same open-file-description relationship as
fork) and verifies that it cannot keep a status probe looking like an owner.

After the probe fix, 20 consecutive full Harness unit runs passed (291 passing,
two ignored per run). All 35 offline/runtime adapter tests and the eight-client
real-socket synthetic probe passed. The latter still measures full-snapshot
fan-out cost; it is not proof that client count can grow without bound.

The normal `target/release-fast/harness` was rebuilt after the lock-probe fix.
The final combined run passed all eight executable checks: three containment
checks plus creation (`/tmp/hc-ZSSJw0`), resume (`/tmp/hr-bEeH8v`), terminal handoff
(`/tmp/ht-ydvZWQ`), send recovery (`/tmp/hq-jjBMJ5`), and packaged setup
(`/tmp/hs-QQfnH1`). The send-recovery transport is deliberately synthetic; native
lifecycle checks use the installed stock 2.1.263 executable. Zero model requests
were sent. The user's X0 symlink and X0_ socket remained unchanged, and user
Harness PID 1038023 was not relaunched.

## Sessions running before setup — 2026-09-07

The earlier terminal test installed the wrapper before starting Claude. It did
not cover enabling Harness with an ordinary terminal AND an unwrapped native
supervisor already running. `terminal_handoff.test.mjs` now covers that ordering
with disposable profiles, without using the real Claude login or successful
model requests. Local `!` shell probes produce a subsequent native authentication
error because the fixture deliberately has no login; these are not model tool
execution or permission tests.

### Reproduced failures

- `/tmp/ht-NWUZkJ`: enabling the hook after startup, then `/bg`, claimed the old
  supervisor's uninstrumented spare. Native history moved but no Harness socket
  appeared. Settings installation alone does not retrofit a running supervisor,
  standby, or conversation. The previous unconditional `/bg` guidance was incomplete.
- `/tmp/ht-cFluX9` and `/tmp/ht-sx3a3b`: a fresh `--bg --resume` client refreshed
  the supervisor but also created another job for an already-running empty
  conversation. Do not use that command as an incidental setup nudge.
- `/tmp/ht-sx3a3b` also exposed a verification race: native writes `continued-in`
  before appending `/background`'s local command caveat/input/output to the old
  transcript. Those final three records are not in the clone. Reading the old
  history immediately used to pass, whereas later opening falsely reported
  different message identities/order/content.

The production handoff verifier now takes the source expectation at its explicit
`continued-in` boundary. It accepts only the exact native local-command suffix,
with source identity, valid UUIDs and linked parents; other post-boundary user or
assistant work is refused, not dropped. The ordinary history reader remains
unchanged. Destination histories, every source in a multi-hop chain, and stronger
prior verification still apply. Unit regressions include partial suffix writes,
wrong destinations, malformed parents, changed commands, later work and conflicts.

### Service refresh without worker replacement

In the private fixture only, after installing the packaged hook:

1. `claude daemon stop --any --keep-workers` stopped the old service.
2. `claude daemon run --origin transient` started a configured native replacement
   without creating any conversation. A connected native client can win this
   startup race itself; the test verifies the actual service/launcher rather than
   requiring our starter's PID. A losing on-demand starter refuses to displace it.
3. Both the existing background worker and the original ordinary terminal kept
   their PIDs. Old spare cleanup was native-supervisor behavior, not a Harness
   process-killing heuristic.
4. `/bg` then produced an instrumented worker. Harness opened the original logical
   conversation and verified the copied UUIDs/roles/content and launch flags.

`/tmp/ht-MN4DJ1` passed that sequence through the real GUI with separate alias
drafts preserved and no accidental submission. Its native shell probe confirmed
the launch-settings environment sentinel survived; `/proc/PID/environ` did not
show it because a warm spare received the setting after process startup. Do not
use `/proc`'s initial environment as proof that the live JS environment lost a
setting. `/tmp/ht-la639B` passed the original setup-before-terminal GUI scenario.

`/tmp/ht-oO6jmQ` and `/tmp/ht-UB4yCG` exercised a stronger service-refresh case:
a terminal was attached to the preexisting uninstrumented background worker and
running a gated shell command. The worker, attached terminal, and shell process
survived; releasing the gate completed that same shell PID. Native's attached
client restarted the configured service before our explicit starter in these
runs. No extra conversation was created. This is not coverage of in-flight
model tools, permissions, subagents, teams, or every supervisor version.

### Selective session migration is a different operation

After the shell finished, `/tmp/ht-UB4yCG` explicitly ran native `respawn` on only
the old uninstrumented worker. It acquired an adapter, retained its native UUID,
all three persisted conversational messages and launch flags, and connected in
Harness. The other instrumented worker kept its PID. **The old attached terminal
client exited.** The CLI implementation in this exact native build invokes its
respawn operation with force enabled; it is not an atomic safe-idle upgrade API.

Consequently, service refresh does not justify automatic session respawn on
thread selection. A production migration workflow still needs explicit consent,
durable identity/history checks, outcome reconciliation without retrying an
uncertain restart, and handling of attached-terminal drafts and active work.
Neither service refresh nor selective migration was wired into automatic UI
opening in this pass. Actual ordinary terminal hot-attach remains unsupported.
No SIGUSR1 was sent: the tested ready terminal had no handler for it, so debugger
activation through that signal must not be assumed safe.

Repeat the strongest private probe using:

```sh
HARNESS_SETUP_BINARY=target/release-fast/harness \
HARNESS_NATIVE_TEST_BINARY=/absolute/path/to/verified/claude \
HARNESS_HANDOFF_LATE_SETUP=1 HARNESS_HANDOFF_EXISTING_WORKER=1 \
HARNESS_HANDOFF_REFRESH_SERVICE=1 HARNESS_HANDOFF_ACTIVE_BYSTANDER=1 \
HARNESS_HANDOFF_MIGRATE_BYSTANDER=1 \
node --test research/claude-native-lab/terminal_handoff.test.mjs
```

`HARNESS_HANDOFF_SHELL_PROBE=1` checks the transferred terminal's live settings;
`HARNESS_RESUME_GUI=1` adds the isolated real frontend. No host X server is
launched: the existing Bubblewrap/private-socket containment remains mandatory.
All test-owned sessions/services are stopped on exit. User settings, existing
sessions, the stock executable, and Xwayland were not changed. Terminal-origin
`<bash-input>` / `<bash-stdout>` records are currently visible as raw text in the
shared transcript; mapping that local-shell envelope is a separate rendering gap.

Final combined validation on the rebuilt normal Harness binary:
`/tmp/ht-nlKZwQ/report.json` passed late setup, native-client service restart,
the still-attached running shell, handoff history/launch settings, both GUI drafts,
conflicting prior verification refusal, and selective bystander migration. That
last operation again disconnected its old terminal while leaving the other worker
unchanged. All 299 Harness unit tests passed (two ignored), and all 36 adapter/
offline/native-runtime tests passed. Normal binary rebuilt September 7 at 04:34
Pacific. The user's running Harness was not relaunched.

## Parallel-tool history verification — 2026-09-07

A real existing conversation failed opening with the message-identity/order/content
guard. Read-only comparison found that every one of the 43 expected messages was
present unchanged in native memory. Native had also restored one saved parallel
tool result that Harness's parent-only reader omitted. The original saved bytes
were still an exact SHA-256-matching prefix of the appended JSONL.

Inspection of the exact installed 2.1.263 executable confirmed why: native tool
results use their originating assistant chunk as `parentUuid`. Native history
restoration recovers assistant chunks by shared API response ID and their result
children, merging them in file order within the response span. A serial parent
walk alone cannot reproduce that history. No proprietary source was copied into
the repository.

The saved reader now recovers those parallel response siblings, binds results to
their actual tool-use IDs, preserves selected ancestor ordering and excludes
sidechains/unrelated responses. New continuation expectations include those
results. Already-pending old proofs are repaired only when the exact original
source checksum is found at a complete JSONL boundary, the old parent-only
projection reproduces the saved proof, and the corrected complete prefix matches
the native snapshot's UUIDs, types, roles and contents. Handoff boundaries and
stronger destination proofs remain enforced. Missing or changed source bytes and
unexplained differences still fail closed.

`--claude-check-history UUID` exposes this verification as a read-only diagnostic:
it never launches, sends, or publishes a verified proof. On the real stuck worker,
the rebuilt binary reported 43 previous expected messages and 44 verified messages;
the worker PID and stored proof were unchanged. The user must reopen with the
updated Harness frontend to publish verification through the usual owner/epoch
checks. No native restart, adapter update or login is needed for this reader fix.

Validation: 306 app unit tests passed, two existing tests ignored; 35 offline
adapter tests passed, one native-runtime test skipped. The isolated actual-native
and GUI regression `/tmp/hr-8CBsfb` restored a synthetic eight-message conversation
with two parallel tool results, seeded a legacy seven-message pending proof, then
selected it in Harness. `ui/parallel-repaired.png` shows the shared transcript,
enabled composer, preserved draft, and no opening error. It verifies same-PID
recovery, zero submitted prompts, later stopped-worker resume, competing openers,
and rejection of an unexplained proof mismatch. An initial GUI test clicked the
instruction line instead of the unavailable conversation row; the corrected
target passed. No credentials were copied or authenticated model calls made.

Repeat with the verified native executable:

```sh
HARNESS_SETUP_BINARY=target/release-fast/harness \
HARNESS_NATIVE_TEST_BINARY=/absolute/path/to/verified/claude \
HARNESS_RESUME_PARALLEL_FIXTURE=1 HARNESS_RESUME_REPAIR_GUI=1 \
node --test research/claude-native-lab/resume_workflow.test.mjs
```
