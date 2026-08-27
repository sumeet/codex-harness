# Harness product backlog

This is the durable product backlog for the standalone Harness client. It is
intentionally separate from `DESIGN.md`: that file describes the stable design
contract, while this file records unfinished work and product decisions that
must survive conversation compaction and hand-offs.

## Now

### Visual system and overall aesthetic

- [x] Define one semantic Harness visual layer on top of Zed theme tokens:
  canvas, transcript, rail, composer, raised surface, quiet divider, focus,
  activity, warning, error, selection, and syntax/diff roles.
- [x] Improve contrast without making text smaller or adding ornamental chrome.
- [x] Replace the bright composer/transcript focus line with a calmer,
  Zed-native separation and focus treatment.
- [x] Attach live activity to the response's actual streaming insertion point;
  use a quiet transcript-tail fallback only before the first text arrives, and
  keep reconnecting state nearby without turning either into a status banner.
- [ ] Normalize typography, padding, borders, radii, identity rows, and density
  across user messages, tool calls, diffs, images, queue rows, and the composer.
- [x] Preserve the core Harness direction: show a lot of information in a
  document-like transcript while keeping its structure easy to scan.
- [x] Add persistent in-app theme selection with immediate live application;
  components remain independent of any particular dark theme.
- [x] Port Zed's composer composition rather than only its buttons: a compact
  auto-height draft surface, one contextual Send/Queue/Stop control, a native
  context ring, and consistent model/effort and permission selectors in a
  quiet editor-surface footer that preserves Harness Vim state.

### Draft destinations and queue/later model

- [ ] Treat composer content as a durable draft whose destination can change
  without retyping: **Send**, **Queue**, **Later**, **Steer**, **Delegate**, or
  **Fork thread**.
- [ ] Keep the default action effortless (Enter/click), and expose alternatives
  from the same send control plus keyboard-accessible commands.
- [ ] Split pending work into two explicit lanes:
  - **Queue**: runnable in order when the active turn finishes.
  - **Later**: held/paused indefinitely and never auto-started.
- [ ] Let every pending item be edited, deleted, reordered, paused/resumed,
  sent now, steered into the active turn, delegated, or forked.
- [ ] Add a global queue pause without conflating it with stopping the active
  turn.
- [ ] Show queued/held messages in the transcript exactly once when they are
  accepted, started, or steered; reconcile with server IDs rather than fuzzy
  text matching.
- [ ] Prefer server-owned durable pending state so Queue/Later works across
  machines. If App Server cannot represent held drafts, use an explicit,
  versioned sidecar rather than hidden text or overloaded IDs.

## Next

### Delegation, forks, and parallel work

- [ ] **Delegate** through Codex's native parent/child agent machinery: show
  child status/activity and preserve communication with the parent.
- [ ] Keep **Fork thread** distinct: it creates an independent conversation
  branch from stored history rather than a supervised child.
- [ ] Offer same-workspace delegation for research/disjoint files and isolated
  worktrees for risky or overlapping implementation.
- [ ] Make concurrent edits and ownership visible enough that parent and child
  agents do not silently invalidate each other's assumptions.

### Composer and sending

- [ ] Verify the whole live workflow: Enter submits; Shift-Enter, Alt-Enter,
  and Ctrl-J insert newlines; Ctrl-V pastes text or images; stop/queue/steer
  transitions are truthful; streaming follows the tail when appropriate.
- [ ] Keep the composer caret and final line visible at every height.
- [ ] Show attachments compactly, render submitted images once, and reconcile
  optimistic user messages using protocol identity.
- [ ] Distinguish queued user input from user steering that should interrupt or
  redirect the current turn.

### Vim transcript correctness

- [ ] Make click, C-w k/j, j/k, gg/G, visual selection, and cursor visibility
  reliable in long, virtualized rich transcripts.
- [ ] Remove compensating cursor/offset workarounds; all visible selectable
  glyphs must own source geometry, and ornaments must not pretend to be text.
- [ ] Keep native Vim search semantics: incremental search, persistent
  highlight, smartcase, n/N, and `:noh[lsearch]`.
- [ ] Make structured tool, diff, Markdown, rule, list-marker, and image regions
  obey the same navigation projection invariants.

### Scrolling and performance

- [ ] Measure event-to-presentation latency and sustain 120 Hz scrolling under
  realistic long-thread load; keep input handling independent of transcript
  layout, parsing, and syntax highlighting.
- [ ] Finish Wayland precision-touchpad momentum using recorded physical traces
  and source/stop/timestamp information rather than guessed deltas.
- [ ] Use `StickyVisibleOwner` nested-scroll latching: freeze the chain at
  gesture start, let the first visibly moving region commit, preserve ownership
  through reversal/momentum, and never retarget under a stationary pointer.
- [ ] Keep thin, discoverable, auto-hiding Zed scrollbars on every scrollable
  surface.
- [ ] Eliminate clipping, delayed first scroll, unexpected child/parent
  handoff, and virtualization stalls; keep deterministic replay tests.
- [ ] Revisit the product tradeoff between nested tool scrolling and a single
  transcript scrollbar with real trace-based usability testing.

### Tool calls, diffs, and structured content

- [ ] Give Bash command and output distinct selectable regions; trim trailing
  blank output, syntax-highlight the command, wrap long commands, and render a
  literal selectable `$ ` without `/usr/bin/bash -lc` ceremony when the
  protocol structure proves that wrapper is synthetic.
- [ ] Use consistent compact headers (or no redundant header) across tool
  cards, file changes, reasoning, plans, web results, and images.
- [ ] Flatten single-file and multi-file edits into the same file-panel format.
- [ ] Normalize diff line height, gutters, marker spacing, header alignment,
  filename/stat presentation, common-indent trimming, borders, and clipping.
- [ ] Render plan status through checkbox/icon state instead of appending
  `(pending)` or `(inProgress)` text.
- [ ] Keep transient retry/reconnect messages near live activity; reserve
  durable error cards for actionable terminal failures.
- [ ] Correct image preview sizing, remove redundant image/tool chrome, and
  make expansion useful without huge empty surfaces.

### App Server and settings

- [ ] Make task history feel immediate through snapshot caching, staged loading,
  incremental projection, and measurement of App Server versus client cost.
- [ ] Show context-compaction landmarks when the protocol exposes them.
- [ ] Expose truthful model, reasoning effort, permissions/access, context
  usage, and project/worktree state without fabricated `Default` choices.
- [ ] Preserve transport fallback/retry as transient liveness state.
- [ ] Investigate a durable App Server representation for queued versus held
  drafts and a client-visible delegation request surface.

### Runtime, builds, and collaboration

- [ ] Keep the optimized hacking build fast with sensible parallelism (eight
  workers on this machine) and ensure launch scripts do not damage the caller's
  terminal.
- [ ] Keep periodic private GitHub checkpoints so work can be inspected and run
  from another machine.
- [ ] Maintain focused replay fixtures and visual acceptance checks for every
  repaired class of rendering/navigation bug instead of relying on ad-hoc live
  testing alone.

## Done (retain as regression requirements)

- [x] Standalone Harness window separated from ordinary Zed IDE chrome.
- [x] Real Zed Editor/Vim engine attached to composer and transcript model.
- [x] Rich transcript supports structured Markdown, tool, diff, and image
  renderers alongside native Vim navigation experiments.
- [x] Composer can submit a prompt, receive a live response, and attach images.
- [x] Basic queued prompt rendering, edit, remove, steer, and send-now actions.
- [x] Ctrl-V composer paste, native rich search transaction, and a transcript
  tail activity item exist; remaining work above is polish and correctness, not
  permission to regress these paths.
