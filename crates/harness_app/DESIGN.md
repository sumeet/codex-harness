# Harness interaction contract

Harness is one keyboard-first Codex session window. It is not an IDE shell and it does not expose
Zed workspaces, ACP, account state, extensions, or a general-purpose code editor.

## Reading surface

- The transcript uses the full width available after the task rail; it is never a narrow centered
  chat column.
- User and Codex prose read as a document. Repeated rounded chat cards are avoided.
- Reasoning and plans are annotated document sections.
- Commands, diffs, file changes, tools, images, approvals, and subagents use purpose-built
  structured blocks.
- App Server frames are retained in a bounded runtime diagnostic journal. Semantic deltas update a
  stable block; telemetry, routine lifecycle noise, unknown notifications, and unknown item types
  do not become conversation rows. Diagnostic retention is intentionally not described as
  permanent or lossless because the journal is bounded and is not persisted with the transcript.
- The task rail and transcript are virtualized independently.

### Visual weight and density

Density comes from removing repeated chrome, not from shrinking readable text.
Every transcript item belongs to one of three presentation levels:

- **Narrative** is nearly chrome-free. Assistant prose is unboxed; user input
  uses only a quiet raised surface. Attribution labels are omitted when the
  speaker is already unambiguous.
- **Routine activity** such as commands, searches, reads, and generic tool
  calls forms consecutive activity stacks. A stack has one outer surface and
  radius, line-height identity rows, and hairline separators. Expanding one row
  inserts its evidence directly beneath that row without changing the stack's
  outer gutter.
- **Semantic artifacts** such as diffs, images, and approvals receive one
  unnested surface when their structure benefits from it. Plans and reasoning
  remain lightweight structured narrative rather than ordinary tool cards.

Routine identity rows use a quiet semantic icon, a readable title, and status
only when status adds information. Strong color communicates active,
action-required, or failed state; it does not merely announce a tool category.
Success is quiet.

A compact command row shows the raw shell command with the same syntax spans as
its expanded form. It uses Zed's terminal icon and has no redundant `Command`,
`Unknown`, or decorative prompt byte. The one-line compact form may clip; the
expanded form exposes the complete command and output on the same left/right
baseline. Failure shows `exit N`; successful completion adds no checkmark.

Queue actions name their actual delivery policy. **Steer** uses Harness's
native steering-wheel glyph and delivers guidance to the active turn at its
next safe input boundary without interrupting it. **Interrupt & run** ends the
active turn and starts the queued item as a new turn. A queued row needs no
decorative queue icon: its location and status already communicate that state,
and uncommon lifecycle choices must not rely on mystery glyphs alone.

### Parent and child tasks

Interactive threads remain the stable top-level task order. Spawned Codex
threads form a typed hierarchy beneath their parent using App Server source and
parent metadata, never transcript-title inference. Child inspection is
observational: opening a read-only child must not reject, cancel, or otherwise
answer a request belonging to its parent. If a parent needs user input while a
child is visible, Harness preserves the request and returns attention to the
parent rather than silently declining it.

The live child registry refreshes independently from thread opening and the
ordinary root list. Collaboration activity can schedule a debounced hierarchy
refresh while the parent is still running; one cancellable task slot must never
be shared by navigation and discovery. Historical spawn, steer/send, wait, and
completion events retain their event-local meaning. Current child status lives
in the hierarchy row and is not retroactively copied into old transcript
events.

### Process and workspace continuity

Closing a Harness window disconnects a view; it does not terminate Codex work.
Harness connects through Codex's managed App Server daemon rather than owning a
stdio server whose lifetime is tied to the window. Startup must preflight the
PATH CLI, managed CLI, and running App Server versions and must never silently
attach to an incompatible or stale daemon. Restarting the daemon is a separate,
explicit lifecycle action and is not implied by closing Harness.

Codex owns durable thread history, active-turn recovery, pending server
requests, the runnable Queue, and parent/child thread metadata. Harness owns a
versioned transactional workspace store for state that Codex cannot reconstruct:
the selected root and inspected child, composer text, durable private copies of
unsent attachments, optimistic outbound identity, partial request answers, and
practical viewport/Vim/expansion anchors. Startup restores the local shell
immediately, resumes the selected thread, reloads its server queue and child
hierarchy, then reconciles optimistic rows by stable IDs. Shutdown atomically
flushes local state and disconnects only the UI transport.

Ordinary window closure must preserve a running turn. A daemon crash or machine
reboot may interrupt in-memory computation; after that failure Harness restores
all durable state and labels the unfinished turn truthfully rather than
pretending it is still running.

### Durable tasks and execution attempts

The live parent/child sidebar answers which sessions are executing and where
their transcripts live. It is not the product backlog. Harness also maintains a
workspace-wide task ledger across threads. A task has stable identity, an
intended outcome and acceptance criteria, explicit status and priority,
dependencies, its originating prompt/thread, meaningful updates, artifacts or
checkpoints, and zero or more execution attempts.

A subagent thread is one execution attempt attached to a task. Spawning a child
does not define the task, and completing a child does not by itself prove the
task is complete. Every user submission first enters a durable inbox so no
request disappears during compaction, restart, reassignment, or interruption;
the coordinator or user can then promote, merge, supersede, delegate, or resolve
it explicitly. The task view summarizes outcome, state, and latest material
change; raw agent and tool activity remains available one level deeper.

## Keyboard model

There are five explicit focus modes: tasks, transcript, composer, search, and request input.

- `Ctrl-h`, `Ctrl-k`, and `Ctrl-j` move to tasks, transcript, and composer.
- Transcript `j/k`, `gg/G`, and `Ctrl-u/Ctrl-d` navigate blocks and pages.
- `v` starts a blockwise visual selection. `y` copies one block or the visual range.
- `Enter` or `za` folds the selected structured block. `r` toggles its raw protocol payload.
- `/` searches the transcript; `n/N` move between matches.
- `i`, `a`, or `o` returns to the real Zed/Vim composer.
- On a `request_user_input` block, `Enter` enters request mode. `j/k` chooses a question, `h/l`
  chooses an option, `Enter` selects it, `i` focuses a free-form or masked field, and `Ctrl-Enter`
  submits the schema-correct answer map.
- The composer uses Zed's editor and Vim engine. `Ctrl-Enter` sends and `Ctrl-w k` returns to the
  transcript.

## Streaming and performance

- UI updates are coalesced to one batch per 16 ms frame.
- Streaming mutates existing semantic blocks instead of appending a row per token.
- Replay mode is the acceptance environment. A 10,000-block session must open, jump to either end,
  select, fold, and copy without building 10,000 GPUI element trees.
- Live transport is attached only after the replayed screen is visually and behaviorally correct.
