# Harness quality gates

Harness is not done when a feature compiles or when its happy path appears in a
screenshot. A product checkpoint is good only when the relevant gates below
have been exercised and the result is pleasant enough to use as a primary
agent client.

## Visual language

- Use Zed theme tokens, iconography, spacing rhythm, and compact control
  language. Do not invent a second generic desktop-app aesthetic or permanent
  chrome that merely explains healthy state.
- The transcript uses the available width. It must not become a narrow centered
  reading column.
- Rich prose uses proportional UI typography; monospace is reserved for code,
  commands, terminal output, paths, and the raw Editor projection.
- Message kinds are distinguishable at a glance without turning every item into
  a heavy card. Headers, status color, spacing, and disclosure are restrained
  and consistent; selection does not add a full-height rule beside an item.
- Protocol bookkeeping, duplicate reasoning updates, raw JSON, opaque IDs, and
  lifecycle noise do not pollute the reading timeline. Meaningful state lives in
  semantic cards or compact chrome; exact raw events remain available for
  diagnostics.
- Empty, loading, missing-media, error, and responding states are deliberately
  designed—not accidental blank space or serialized payloads.

## Interaction

- The composer is always directly below the transcript and remains a real Zed
  modal Editor. Its mode and exceptional state belong inside the composer;
  permanent navigation guides and duplicated transport/view labels do not.
- There is one vertical history scrollbar. A diff, terminal, image, form, or
  tool card may scroll horizontally when its content requires it, but never
  creates a nested vertical reading surface.
- Rich paint is backed by Zed's actual Editor/Vim behavior, not a reimplementation.
  Normal, Visual, Visual Line, Visual Block, text-object, register, yank, search,
  jumplist, and selection behavior operate on real Buffer text while the
  selection is projected onto the proportional Markdown and structured cards.
- The raw Editor projection remains available for diagnostics. Switching
  Rich/raw preserves the selected semantic item and whether the user is
  following the tail or reading above it. Switching never doubles as Escape,
  silently changes Vim mode, or steals a visual selection.
- Streaming follows only while pinned. Any deliberate upward scroll or backward
  Vim motion pauses follow; returning to the bottom or sending a new turn
  re-engages it.

## Streaming and protocol completeness

- Every meaningful App Server item is represented: messages, reasoning/plan,
  commands, file changes and diffs, tools/MCP, subagents, web activity, images,
  reviews, errors, approvals, forms, and terminal status.
- Started, incremental, completed, failed, interrupted, and responding states
  update in place. Streaming must not duplicate prior reasoning steps or rewrite
  stable visible text unnecessarily.
- Every ServerRequest is either rendered as an actionable live surface or
  answered immediately with a method-valid safe response/error. No request may
  become an inert transcript card or hang the turn.
- Rich and raw Editor projections share the same request entity, drafts,
  cursors, validation, response latch, and protocol reply. Persisted historical
  requests never revive as live controls.
- Exact protocol payloads remain in the raw journal even when their semantic UI
  projection is compact or intentionally omitted.

## Performance and stability

- A streaming frame edits only dirty items. Selection, scrolling, rendering,
  and viewport decoration never clone or rescan the whole transcript.
- Native blocks, diff/search highlights, and semantic decoration are bounded to
  the visible region plus a small overscan. A 10,000-item replay must not create
  10,000 GPUI blocks or a perpetual idle frame source.
- UTF-8 byte offsets are validated and clipped before becoming Zed anchors. A
  malformed or stale projection falls back safely instead of panicking.
- Thread load/switch, empty tasks, disappearing files, malformed requests,
  App Server disconnects, and response failures are recoverable and visible.
- Standalone builds remain memory-bounded; the compact dependency graph may use
  a small bounded worker pool. Ordinary iteration must not require rebuilding
  or linking the Zed application shell.

## Required verification for a meaningful checkpoint

1. Run focused protocol, Editor, and app tests with one Cargo job.
2. Build through `script/build-standalone.sh`; launching must never invoke
   Cargo.
3. Inspect a full-height Rich replay around prose, reasoning, commands, diffs,
   images, subagents, and requests. Inspect the raw Editor projection separately.
4. Exercise Rich/raw switching, mouse and keyboard focus, character and block
   selection/yank, `/ ? n N`, composer send, request submit/failure, and thread
   switching.
5. Verify streaming follow-tail both pinned and paused while reading above.
6. Run the 10,000-item replay and compare focused/unfocused CPU, scrolling
   responsiveness, and memory. No dataset-scaled idle work is acceptable.
7. Exercise a real App Server thread through start, streaming tools/diffs, an
   interactive request, completion, reopen, and disconnect/recovery.

Any gate not yet exercised is reported as remaining work. “Done” is reserved
for a checkpoint that passes these checks and has survived a fresh visual and
interaction review without obvious rough edges.

## Checkpoint evidence

### 2026-08-17 — transcript lifecycle and standalone Vim parity

- Focused app tests passed: 22/22. The serialized standalone build completed
  without pulling in the Zed workspace application.
- A full-height Rich replay was inspected at 1920×1080/1.5× scale. The reading
  surface remained full-width, historical requests were visibly inert, and the
  composer stayed compact below the transcript.
- A 10,000-item Rich replay used 382,624 KiB RSS versus 363,420 KiB for the
  120-item replay. Hidden on another workspace, it consumed 0 CPU ticks over
  five seconds. Focused, it consumed 40 ticks over five seconds versus 43 for
  the smaller replay, so no item-count-scaled frame loop was observed.
- Manual smoke testing found no immediate crash, focus trap, or palette/mode
  failure in this checkpoint.
- The real Helium history was opened repeatedly in Rich mode without the
  previously suspected panic. Its active-writer conflict now degrades to an
  explicit read-only state with sending disabled instead of a red error and an
  apparently live composer.
- The probe's private app-server child was forcibly terminated. Harness kept
  the loaded history, retired the stale client, disabled sending, changed its
  status to `OFFLINE`, and rendered one semantic disconnect item without raw
  JSON. No orphaned app-server process or hanging event receiver remained.

### 2026-08-17 — quiet chrome and bounded automatic reconnect

- Focused Editor tests passed 32/32 and app tests passed 23/23. Rich and Text
  were separately launched and inspected full-height at 1920×1080/1.5× scale.
- The transcript title row and separate global status row were removed, giving
  62 logical pixels back to history. The unlabeled thread rail owns its compact
  collapse/view/refresh/new controls, while a collapsed rail leaves one reveal
  affordance. The composer now owns the real Vim mode, exceptional connection
  state, and send/stop control without a duplicated send guide.
- Ordinary `Codex` attribution was removed from Rich messages and from Text's
  native header label. Text retains a small semantic boundary and keeps labels
  for genuinely distinct agents and subagents.
- After forcibly terminating a real Helium probe's direct app-server child,
  Harness spawned a different child through capped 1/2/4-second backoff,
  reinitialized, and reopened exact thread
  `01a00e18-ffe7-7982-be69-640ad4b2668e` without a panic. Manual refresh remains
  available after the retry cap rather than being required for ordinary
  recovery.

Still open: pinned and paused streaming against a real turn; a live
approval/form response including failure recovery; Rich/Text selection and
tail-position transfer under longer real histories; and longer exploratory
multi-window use.

### 2026-08-18 — real streaming pause and semantic tool authority

- A fresh Harness-owned App Server thread
  `01a013e1-79db-7131-8327-adbccf814d71` ran two harmless shell commands that
  emitted 40 lines over roughly eight seconds and 80 lines over roughly sixteen
  seconds. The first turn followed the live tail. During the second turn, Text
  mode moved to the top with `gg`; later output chunks and final completion did
  not snap the viewport away from the reading position.
- The same thread exposed duplicate provider-level `custom_tool_call` wrappers
  beside the typed `commandExecution` item. Raw response items are now retained
  only in the bounded diagnostic journal; typed App Server items are the sole
  transcript presentation authority. A migration filters these obsolete cards
  from version-1 local snapshots, and a rebuilt real-window replay showed one
  syntax-highlighted Command card with its output and no `Tool · Exec` or
  `write_stdin` implementation cards.
- From the paused history, Text mode returned to the exact tail with `G` and
  sent a third turn that streamed 20 numbered lines. The viewport followed the
  appended command output and final `repin test complete.` response through
  completion without moving the transcript selection, proving the automatic
  re-pin half of the follow contract against a real turn.
- Text mode's transcript-specific Editor context was verified in the rebuilt
  real window: one `i` moved focus from the read-only transcript to the
  composer, entered Insert mode after the focus handoff, and the next literal
  character appeared in the composer. The test draft was then removed without
  sending it. The same literal-entry proof passed through `Ctrl-W J`. This uses
  Zed's Editor addon/keymap/Vim stack rather than a host key-event shim.
- Focused protocol tests passed 57/57 and app tests passed 38/38; the standalone
  app binary rebuilt successfully. Only the known warnings from the reduced
  upstream Editor feature slice remain.
- A long real Helium history was paused around a file-diff section before
  switching Rich → Text → Rich; both reading surfaces returned to the same
  semantic area. After Text moved to the exact tail with `G`, Rich also opened
  at the latest response instead of restoring an unrelated viewport.
- Three live `item/commandExecution/requestApproval` requests were exercised
  against harmless HTTPS `curl` commands. The first exposed that an idle
  composer retained focus; the request could be operated only after an
  explicit pane transfer. Harness now auto-focuses only a newly mounted,
  unresolved request belonging to the live thread when the composer is empty
  and still focused. It never steals a draft, a history-reading position, the
  task rail, or an existing request update.
- Two rebuilt-window repetitions proved immediate `h/l` choice movement,
  Enter submission, successful App Server response, command exit 0, compact
  completed-state collapse, and focus return to the transcript. The final
  visual pass removed the duplicated command from the request header, kept one
  syntax-highlighted command line, flattened the nested inner panel, preserved
  the complete outer border, and reduced fixed Text-mode request height.
- Focused app tests passed 40/40 after the request-focus and compact-surface
  changes; the standalone binary rebuilt successfully.

Still open: live response-failure recovery and an editable form response;
measured high-refresh-rate scrolling/input latency; and longer exploratory
multi-window use.

### 2026-08-21 — native Vim selection projected onto Rich paint

- Rich is now backed by one persistent input-only Zed Editor/Vim state machine.
  The raw Editor projection remains diagnostic rather than being required for
  motions, registers, search, Visual, Visual Line, or Visual Block behavior.
- Character selection contrast, whole-line Visual Line geometry, and every
  disjoint Visual Block row are projected onto structured cards. Markdown's
  external selection API now also accepts disjoint source ranges, so a block
  spanning checklist rows paints only the selected columns instead of the
  unrelated text between them. A real 1600x1000 replay verified separate `Fin`
  and `Val` ranges, and the full-height Reasoning/Plan left rule was removed.
- Markdown tests passed 142/142 and Harness app tests passed 75/75. The default
  `release-fast` standalone binary rebuilt successfully.
- A 1,000-item Rich `:perf-j` run on the 60 Hz headless output measured draw
  p50 5.87 ms / p95 8.03 ms / max 9.04 ms, input dispatch p50 4.87 ms / p95
  6.66 ms, and input-to-present p50 11.34 ms / p95 14.75 ms. No draw exceeded
  16.67 ms. The measured ~18 ms presentation cadence is constrained by that
  60 Hz output and is not a 120 Hz acceptance result.
- Wayland touchpad release sampling now preserves the newest delta, separates
  source timestamps from animation time, and tolerates same-dispatch timestamp
  quantization. Gesture and Linux pointer-clock tests pass, but physical upward
  and downward fling feel on the 120 Hz laptop panel remains a manual gate.

Still open: exact mouse placement across every structured Rich fragment; visible
cursor treatment for Markdown source bytes replaced by non-text elements such
as checkboxes; a live editable form and response-failure recovery; physical
120 Hz scrolling validation; and longer multi-window use.
