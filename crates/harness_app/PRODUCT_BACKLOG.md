# Harness product backlog

This is the durable product backlog for the standalone Harness client. It is
intentionally separate from `DESIGN.md`: that file describes the stable design
contract, while this file records unfinished work and product decisions that
must survive conversation compaction and hand-offs.

## Working protocol

- New product requests are recorded here before they displace the active slice.
- A checked item requires implementation plus proportionate test or live
  evidence; an investigation or green unit test alone does not close a
  user-reported interaction defect.
- The root agent owns priorities, integration, visual acceptance, and this
  ledger. Child agents own bounded, non-overlapping slices and report their
  evidence back to the root.
- Shared-worktree edits are parallel only when their file ownership is
  disjoint. Builds, commits, pushes, and overlapping UI edits are serialized.
- Coordination infrastructure enables this backlog; it does not replace or
  reprioritize the product backlog by itself.

## In flight

- Reconcile the verified protocol, compaction, and compact-queue slices and
  push a private checkpoint.
- Verify the live child-thread registry and collapsed completed-task history in
  a restarted Harness without allowing inherited prompt previews to reappear.
- Define and apply one systematic lightness/density hierarchy across narrative,
  routine activity stacks, and semantic artifacts instead of tuning each card
  independently.

## Now

### Coordination spine and durable work

- [x] Extend the typed App Server model with parent thread, ancestor/source,
  role/nickname, lifecycle status, fork origin, and direct-input capability.
- [x] Maintain a child-thread registry keyed by thread ID; reconcile it from
  descendant lists and lifecycle notifications rather than inferring the tree
  solely from transcript activity rows.
- [x] Render child sessions as nested, Zed-styled sidebar rows with live status,
  parent breadcrumbs, and independently openable transcripts.
- [ ] Expose safe delegate, follow-up, wait, interrupt, and fork workflows while
  keeping supervised child agents distinct from independent thread forks.
- [ ] Build a workspace-wide durable task ledger, distinct from the live
  subagent hierarchy: capture every user submission in an inbox, promote or
  merge explicit tasks, attach multiple execution attempts, and retain outcome,
  status, acceptance criteria, dependencies, material updates, artifacts,
  checkpoints, and source thread/turn identity across restart and compaction.
- [ ] Replace Harness-owned `app-server --listen stdio://` with Codex's managed
  daemon plus a reconnectable local transport. Closing the window must leave
  the daemon PID and active turns untouched; reopening must resume the saved
  thread, replay pending interaction, reload Queue and hierarchy, and reconcile
  missed output without duplicate transcript rows.
- [ ] Keep cached-history reconnects thin: attach with `excludeTurns: true`,
  retain the local semantic transcript, reload Queue/hierarchy/settings, and
  never automatically fall back to a full multi-GB `thread/read`. Detect an
  abnormal repeated attach death and remain explicitly cached/read-only rather
  than entering an OOM restart loop.
- [ ] Report App Server child exit status/signal instead of flattening every
  stdout EOF to `app-server closed stdout`; distinguish a clean shutdown,
  crash, and SIGKILL, and mention likely OOM only when OS/cgroup evidence exists.
- [ ] Add a versioned transactional workspace-state store for the selected
  root/child, composer draft, private durable attachment copies, optimistic
  outbound journal, partial request answers, and practical scroll/Vim/fold
  anchors. Flush atomically on mutation/shutdown and recover from an interrupted
  write without corrupting prior state.
- [ ] Expose the resolved Codex binary path plus installed and running App
  Server versions; refuse or prominently gate a stale managed daemon, detect an
  available update, and provide a controlled restart. Never restart while any
  thread is active or awaiting interaction without explicit user override.
- [ ] Surface multi-agent capacity and allocation failures truthfully enough to
  distinguish an App Server/protocol limitation from the outer Codex
  orchestration runtime and from exhausted local child slots.
- [ ] Paginate both root and spawned-child thread listings instead of treating
  the first bounded page as a complete hierarchy; preserve visible descendants
  while later pages are loading and make truncation explicit on failure.

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
- [ ] Make Markdown inline code consume the complete configured code typography
  role, including size. GPUI currently lays out a `StyledText` line at one
  uniform size, so Markdown's inline-code size refinement is discarded while
  family and weight survive. Add geometry-correct mixed-size text runs rather
  than splitting prose into flex fragments; preserve wrapping, baseline/line
  height, source hit-testing, Vim cursor bounds, and selection.
- [ ] Apply three deliberate visual weights: nearly chrome-free narrative,
  separator-stacked routine activity, and one unnested surface for semantic
  artifacts. Preserve comfortable reading sizes; achieve density by removing
  repeated labels, backgrounds, padding, and borders rather than shrinking text.
- [ ] Reserve strong chroma for meaningful state or interaction. Keep routine
  identity rows muted; animate active state subtly, keep success quiet, and use
  error color only for actionable failure.
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
  steered at the active turn's next safe input boundary, used to interrupt the
  active turn and start a new one, delegated, or forked.
- [ ] Add a global queue pause without conflating it with stopping the active
  turn.
- [ ] Show queued/held messages in the transcript exactly once when they are
  accepted, started, or steered; reconcile with server IDs rather than fuzzy
  text matching.
- [ ] When the reader is away from the live tail, keep one slim sticky proxy for
  newly submitted or queued input directly above the composer without moving
  the transcript. Reference the same logical item, expose its available queue/
  interrupt/cancel actions, and dissolve the proxy only when the natural
  transcript row becomes visible.
- [ ] Paint newly submitted input immediately at a legible muted opacity rather
  than fading from invisible; transition to normal emphasis on server
  acknowledgement without remounting, flicker, or duplicate rows.
- [ ] Prefer server-owned durable pending state so Queue/Later works across
  machines. If App Server cannot represent held drafts, use an explicit,
  versioned sidecar rather than hidden text or overloaded IDs.
- [ ] Make queued rows compact: no dedicated tall header or redundant per-row
  queue icon, inline 22–24 px image thumbnails only when an image exists,
  prompt-first width, and separators instead of cards. Keep the steering,
  interrupt-and-run, edit, and remove actions icon-only with unambiguous custom
  glyphs, accessible labels, and explicit tooltips; do not single out one row
  with an arbitrary bordered action.
- [ ] Track one outbound item by `clientUserMessageId` through submitting,
  queued/start/steer acknowledgement, model incorporation, and
  completed/cancelled states. Queue IDs, server item IDs, and turn IDs are
  aliases with separate meanings; App Server acceptance must not be painted as
  model consumption, and stale RPC callbacks must not regress newer authority.

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
- [ ] Default implementation workers to a bounded explicit task packet with no
  inherited transcript. Make full-history forks an intentional opt-in with a
  size warning; a large parent must not be copied into every child rollout.

### Composer and sending

- [ ] Verify the whole live workflow: Enter submits; Shift-Enter, Alt-Enter,
  and Ctrl-J insert newlines; Ctrl-V pastes text or images; stop/queue/steer
  transitions are truthful; streaming follows the tail when appropriate.
- [ ] Keep the composer caret and final line visible at every height.
- [ ] Show attachments compactly, render submitted images once, and reconcile
  optimistic user messages using protocol identity.
- [ ] Distinguish ordinary queued input, non-interrupting **Steer** delivery at
  the active turn's next safe input boundary, and **Interrupt & run**, which
  terminates that turn and starts the queued input as a new turn.

### Vim transcript correctness

- [ ] Make click, C-w k/j, j/k, gg/G, visual selection, and cursor visibility
  reliable in long, virtualized rich transcripts.
- [ ] Remove compensating cursor/offset workarounds; all visible selectable
  glyphs must own source geometry, and ornaments must not pretend to be text.
- [ ] Keep native Vim search semantics: incremental search, persistent
  highlight, smartcase, n/N, and `:noh[lsearch]`.
- [ ] Make structured tool, diff, Markdown, rule, list-marker, and image regions
  obey the same navigation projection invariants.
- [ ] Support ordinary mouse drag selection across narrative and structured
  regions without scroll jumps or losing the native Vim cursor.

### Scrolling and performance

- [ ] Replace the detached far-right latest arrow with one tail-aware sticky
  affordance aligned to the transcript activity/content gutter above the
  composer. At the live tail, show only the ordinary inline activity marker.
  While scrolled away, show activity plus a down arrow during streaming, then
  an unread dot plus the arrow after completion; click and `G` both return to
  and follow the live tail, and the proxy disappears once there.
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
- [ ] Preserve the reader's viewport while streaming when follow-tail is
  paused; `G` and the tail affordance must reach the actual live insertion
  point rather than the last materialized virtual row.

### Tool calls, diffs, and structured content

- [ ] Give Bash command and output distinct selectable regions; trim trailing
  blank output, syntax-highlight the command, wrap long commands, and render a
  literal selectable `$ ` without `/usr/bin/bash -lc` ceremony when the
  protocol structure proves that wrapper is synthetic.
- [ ] Preserve the same shell syntax-highlight spans in compact one-line
  command activity rows; compactness may clip/wrap evidence but must not reduce
  a command to unstructured label text.
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
- [ ] Define a truthful compact/expanded contract per tool kind: a compact
  identity row, a bounded high-value preview, exact evidence immediately
  expandable, and raw protocol only as diagnostics. Never replace evidence
  with an incomplete generic summary or `Unknown`.
- [ ] Reconcile command completion in place even when later assistant prose has
  already streamed; keep success quiet, active state visible, and show the
  nonzero exit code on failure.
- [ ] Stack consecutive low-level command, search, and read activity into one
  consistently aligned surface with hairline row separators. Expanding one row
  reveals its body beneath that row without changing the stack's outer gutter;
  semantic diffs, images, approvals, and user-input requests remain independent
  blocks.
- [ ] Give plans and reasoning lightweight document treatment and verify their
  current compact layouts in a real long transcript.

### App Server and settings

- [ ] Make task history feel immediate through snapshot caching, staged loading,
  incremental projection, and measurement of App Server versus client cost.
- [ ] Instrument cached restore and live attach as separate phases. Avoid making
  `thread/resume` rescan a multi-GB rollout merely to establish live authority;
  if the installed App Server cannot attach by cursor/checkpoint, continue in a
  new live thread with an explicit semantic handoff instead of hiding minutes of
  work behind `Connecting live…`.
- [ ] Expose Codex Fast mode as an explicit selectable and deselectable task/turn
  setting, display the App Server's resolved `serviceTier`, and send `default`
  when Fast is off rather than inferring it from model or reasoning effort.
- [ ] Show at most one context-compaction landmark for each actual compaction;
  it must never replace, suppress, or multiply the activity and transcript
  content that follows it.
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
- [ ] Support right-click/open-session-in-new-window and validate long-running
  multi-window use without writer conflicts or tiny-window-only iteration.

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
