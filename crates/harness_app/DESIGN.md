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
