# Harness quality gates

Harness is not done when a feature compiles or when its happy path appears in a
screenshot. A product checkpoint is good only when the relevant gates below
have been exercised and the result is pleasant enough to use as a primary
agent client.

## Visual language

- Use Zed theme tokens, iconography, spacing rhythm, compact controls, and
  status-bar language. Do not invent a second generic desktop-app aesthetic.
- The transcript uses the available width. It must not become a narrow centered
  reading column.
- Rich prose uses proportional UI typography; monospace is reserved for code,
  commands, terminal output, paths, and the Text projection.
- Message kinds are distinguishable at a glance without turning every item into
  a heavy card. Headers, left rules, status color, spacing, and disclosure are
  restrained and consistent.
- Protocol bookkeeping, duplicate reasoning updates, raw JSON, opaque IDs, and
  lifecycle noise do not pollute the reading timeline. Meaningful state lives in
  semantic cards or compact chrome; exact raw events remain available for
  diagnostics.
- Empty, loading, missing-media, error, and responding states are deliberately
  designed—not accidental blank space or serialized payloads.

## Interaction

- The composer is always directly below the transcript and remains a real Zed
  modal Editor. Focus changes by mouse and keyboard update the visible mode and
  hints correctly.
- There is one vertical history scrollbar. A diff, terminal, image, form, or
  tool card may scroll horizontally when its content requires it, but never
  creates a nested vertical reading surface.
- Rich mode supports fast semantic `j`/`k` navigation, search, disclosure,
  block selection/yank, and predictable focus transfer to requests and the
  composer.
- Text mode is Zed's actual Editor/Vim behavior, not a reimplementation. Normal,
  visual, text-object, register, yank, search, jumplist, and selection behavior
  must operate on real Buffer text.
- Switching Rich/Text preserves the selected semantic item and whether the user
  is following the tail or reading above it. Switching modes never doubles as
  Escape, silently changes modes, or steals a visual selection.
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
- Rich and Text share the same request entity, drafts, cursors, validation,
  response latch, and protocol reply. Persisted historical requests never
  revive as live controls.
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
- Standalone builds remain serialized and memory-bounded; ordinary iteration
  must not require rebuilding or linking the Zed application shell.

## Required verification for a meaningful checkpoint

1. Run focused protocol, Editor, and app tests with one Cargo job.
2. Build through `script/build-standalone.sh`; launching must never invoke
   Cargo.
3. Inspect a full-height Rich replay around prose, reasoning, commands, diffs,
   images, subagents, and requests. Inspect Text mode separately.
4. Exercise Rich/Text switching, mouse and keyboard focus, character and block
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
