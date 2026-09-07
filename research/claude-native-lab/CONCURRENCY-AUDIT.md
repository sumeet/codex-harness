# Native Claude ownership and concurrent clients — 2026-09-06

The existing adapter supports several clients in the ordinary case, but this is
not yet evidence of crash-safe, app-server-grade operation. The investigation
found reproducible admission failures and an important distinction in Claude's
own ownership enforcement. No production behavior was changed in this pass.

**Follow-up:** [SUPERVISOR-AUDIT.md](SUPERVISOR-AUDIT.md) now demonstrates the
worker startup/respawn integration proposed below, eight real native socket
clients, two Harness frontends, and competing restarts. The original admission,
shared-draft, and fan-out findings were initially unfixed. The subsequent
[opening/handoff pass](SUPERVISOR-AUDIT.md#thread-opening-and-terminal-handoff--2026-09-06)
fixes the specific post-admission bookkeeping ordering bug: its same-ID retry now
produces one admission and retains an accepted-with-warning result. The historical
two-admission observation below describes the original version. Adapter-restart
durability, uncertainty inside admission itself, shared drafts, and fan-out remain
open; this is not an exactly-once delivery guarantee.

## Live native ownership experiment

Tested the unchanged installed Linux executable, Claude Code 2.1.263, SHA-256
`b9c407e36847bcb24b953b1390f240c840ae6c99e10a76475d2fadc5d5c4adca`.
All work used a new fork of a stopped disposable lab conversation. No model
prompt was submitted, no existing user conversation was resumed, and Remote
Control, MCP servers, browser launching, and auto-updating were disabled for the
test launches. Commands were local `/rename`, `/exit`, and lifecycle operations.

| Situation | Observed result |
| --- | --- |
| Preloaded interactive owner, then stock `--resume` | Both opened the same conversation |
| Two stock interactive owners, neither with preload | Both opened the same conversation |
| Stock `--bg --resume`, then stock interactive `--resume` | Second process exited 1 and directed the user to `claude attach` or `claude stop` |

The fork was `ff157bdc-bc46-43a7-a4c0-46d9cbcf079c`. The preloaded owner was
PID 984159. The two simultaneous **stock** owners were PIDs 984949 and 985342;
both reached the ordinary prompt and had live native session-registry records
with that same UUID, `kind: interactive`, and distinct process-start identities.
Neither stock process had `BUN_OPTIONS` or Harness preload settings in its launch
environment. This demonstrates duplicate ownership without our instrumentation;
it does **not** demonstrate transcript corruption, because no concurrent model
work was attempted.

The first resume attempt, immediately after forking, reported no conversation
found. A local `/rename harness-disposable-ownership-probe` materialized the fork;
the subsequent resume tests succeeded. Do not mistake the initial missing-file
error for ownership enforcement.

After both interactive owners exited, `--bg --resume` continued the same UUID
under native job `ff157bdc`, worker PID 985755. `claude agents --json` reported it
idle. A stock interactive resume then refused to open it. `claude stop ff157bdc`
stopped only this test job. Its newly started, transient supervisor PID 985730
subsequently idle-exited. All six observed test native/host/supervisor PIDs were
confirmed absent afterward; the installed binary hash remained unchanged. The
disposable conversation and stopped-job metadata were retained for inspection.

### What installed-source inspection explains

Private extracted modules remain outside the repository. These names identify
this inspected build, not stable integration contracts:

- `chunk-q53nffg6.js`: `dit` obtains live matching session records; `G4` excludes
  interactive owners when selecting a holder for the resume guard.
- `chunk-t9jcqxnm.js`: normal non-fork resume calls that guard before loading.
- `chunk-vn1198wy.js`: background dispatch checks existing owners and can choose
  to fork rather than continue. It also has respawn conflict checks.
- `chunk-7kc4t68e.js`: the `--session-id` existence helper checks for a transcript
  file; that check is not an exclusive process lock.
- Native session registration includes a process-start fingerprint as well as
  PID. A Harness-side process registry should not use PID alone.

**Consequently, the native supervisor is a materially stronger ownership lead
than launching independent interactive sessions ourselves.** This was a serial
startup/refusal test, not proof of an atomic global lock against simultaneous
background launches, every resume entry point, or restart races.

At the time of this initial ownership test, it was not a completed alternative
transport. Our preload consumes its
startup environment inside the process where it runs. Native job dispatch also
persists an allowlisted environment and respawn flags. Loading the adapter into a
supervisor-owned worker, retaining it across respawn/upgrades, and discovering its
socket had not been demonstrated. The follow-up uses the documented native
process wrapper and a delayed standby bootstrap to establish those cases.
Merely preloading `claude attach` would target
the attaching client, not establish instrumentation of the worker. Do not change
native registry kinds to impersonate a background worker or advertise a working
attach path that does not exist.

## Repeatable multi-client and fault probe

Run from the repository root:

```sh
node research/claude-native-lab/probe_concurrency.mjs
node --test research/claude-native-lab/bridge.test.mjs research/claude-native-lab/preload.test.mjs
```

The probe uses the **real bridge and real Unix sockets**, with synthetic native
stores. Prompt and approval races use eight separate Node client processes. Event
ordering uses eight subscriber instances in one Node process. It needs no Claude
installation, account, or model calls. It reports findings about current behavior;
exit success is not a claim that the reported failures are fixed.

| Check | Observed result |
| --- | --- |
| Eight processes submit the same UUID/text | One queue admission, seven duplicate acknowledgements |
| Eight distinct submissions | All eight admitted once |
| Eight clients race to allow/deny one dialog | One resolution, seven stale replies rejected |
| Broadcast 250 engine events | All eight subscribers received the same sequence order without duplicates |
| Throw in bookkeeping after queue admission, then retry same UUID | **Two admissions** for that UUID |
| Replace adapter instance, retry | Old epoch rejected; retry with new epoch **admitted again** |

The existing 16 offline adapter/preload tests also passed. Neither suite is a
native Bun load test or multi-Harness GUI soak test.

### Admission and restart gap

In `bridge.mjs`, `enqueueReportingAdmission` runs before idle-turn bookkeeping,
and the in-memory deduplication entry is recorded only after that bookkeeping.
Injecting an exception into `markSubmit` reproduces an RPC error after native
admission. A retry then admits the same request again. Moving the ledger write
addresses that particular ordering bug, but does not establish durable delivery.

Adapter replacement discards the in-memory ledger. Epoch rejection prevents an
old bound request from acting on the replacement; it does not determine whether
an earlier request was admitted. A client cannot safely refresh the epoch and
blindly resend an uncertain request. Current Harness reconnect does not do that
automatically, so the probe does not establish a current automatic-retry loop.

A durable journal needs explicit pending/admitted/rejected/uncertain outcomes and
reconciliation against native state. Native queue admission and our disk commit
are not one atomic transaction. Unless native admission offers durable dedupe,
the crash window must remain visibly uncertain rather than promising exactly-once
delivery. Require session/epoch bindings for mutating requests in a hardened
protocol; the prototype currently checks them only when supplied.

### Fan-out cost

The bridge serializes each whole transcript update separately for every client,
synchronously inside the native process. A single synthetic transcript of
1,117,099 JSON bytes took approximately 5.5 ms with one client, 11.8 ms with four,
and 16.4 ms with eight in the first probe run. These are noisy single Node
measurements of synchronous publication, not sustained throughput or Bun timings.

Serialization once per event, transcript deltas, and bounded backpressure are
needed before claiming comfortable long-history multi-client behavior. Existing
slow-client disconnection is useful, but does not remove per-client serialization
cost. Rust projection checks epoch but does not yet enforce the event sequence
cursor; reconnect and gap recovery need dedicated tests.

## Shared Harness persistence is a separate problem

Code inspection of `main.rs` found that `persist_harness_session` and
`persist_composer_drafts` both write a full in-memory snapshot to a fixed
`*.json.tmp` pathname, then rename it over the shared destination. Debouncing is
per frontend entity, not cross-process coordination.

Multiple frontends can therefore overwrite changes based on stale snapshots,
collide on the temporary pathname, or report a rename failure after another
writer moved that pathname. This is independent of Claude and affects the shared
Harness persistence path. It was identified by inspection, not a live destructive
test of the user's drafts. Unique temporary filenames alone would not solve lost
updates; shared state needs transactional mutations/versioning through an owner
or an equivalent coordinated store. Deliberately client-specific UI preferences
should not be confused with shared drafts or submission state.

## Recommended next implementation experiment

1. **Native supervisor compatibility first:** demonstrate adapter startup in one
   native-owned background worker, stock terminal attach, eight Harness clients,
   disconnect/reconnect, and stop/respawn without lost adapter discovery. Race
   competing starts against the same disposable ID and test identity changes.
2. **If that works, reuse its lifecycle.** Keep our private native-state adapter
   narrow. We do not need to invent a second lifecycle authority merely to expose
   the semantic transcript/controls to Harness clients.
3. **Otherwise, keep the managed-host route explicit:** one authoritative owner
   and durable admission/state per conversation, shared by cooperating Harness
   clients. Detect foreign native owners and refuse unsafe adoption. A lock only
   our processes honor cannot prevent an unmodified interactive CLI from opening
   the same conversation after a check; that remains a product boundary.
4. In either route, fix admission uncertainty, transactional shared drafts,
   fan-out/backpressure, and reconnect cursors. Then run native failure and load
   tests, not just synthetic green tests.

The evidence supports hardening this into a useful multi-client integration. It
does not support calling it update-independent or equivalent to a supported
upstream app-server. Native lifecycle reuse may reduce the amount we maintain;
the version-gated private controller adapter still needs compatibility testing.

## Subsequent send-hardening results

The earlier adapter-restart duplicate finding is now covered by durable native
profile records in `bridge.mjs`, not the in-memory cache. The eight-client probe
now asserts that retry under a new adapter epoch returns the saved receipt with
zero additional admissions. Separate process-death tests cover crashes before
native admission, after admission but before receipt, and after receipt. Missing
completion is uncertain and cannot be reclaimed. Exact authoritative native
user-message UUID/text evidence can reconcile it later without a second enqueue.

Frontend IDs are now persisted before requests. The real Harness GUI regression
restarts the frontend, changes the adapter epoch, retries a lost reply, and checks
intentional repeated text as a distinct operation. Draft-file writes use unique
private replacements and an exclusive lock; pending-ID and receipt-marker merges
protect the send journal against stale snapshots. This fixes those journal races,
not arbitrary concurrent draft-text editing or mixed-version writers. Request
records and accepted receipts are native-profile-owned; draft text/references are
still frontend-persisted. Acceptance also does not prove model execution completed
or establish a crash-durable native queue. Those distinctions remain required.

See the latest supervisor audit section and `submission_workflow.test.mjs`,
`submission_ledger.test.mjs`, and `probe_concurrency.mjs` for reproducible scope.
