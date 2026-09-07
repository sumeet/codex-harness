# Native Claude → shared Harness transcript audit

Updated 2026-09-07, installed Claude 2.1.263, stock executable with the local preload.
This audits the **frontend adapter**, not a claim that native Claude lacks a
feature when Harness has not mapped it. The transport supplies native messages,
structured `toolUseResult` metadata, stream events, and native dialog descriptors.

## Findings and changes

The initial projection only specialized Bash. Every other tool was a generic
name plus input JSON, and results discarded the envelope's `toolUseResult`.
Consequently a native Write had enough information for a diff but displayed just
“Write.” Sharing `render_item` did not by itself establish semantic parity.

| Shared surface | Native mapping after this pass | Evidence / remaining boundary |
| --- | --- | --- |
| User / assistant prose | Correct role, ordinary Markdown/code renderer | Live composer submission and rendered response; string and block-array regression cases |
| Reasoning / text streaming | Separate thinking/text blocks; commit deduplication by API block index | Native commits each API block independently; regression covers thinking committed while answer streams |
| Streaming tool input | Partial JSON retained, complete input gains semantic title, stable tool-use identity | Regression test; incomplete JSON is not treated as executable approved input |
| Command | Bash command/output, failed status, explicit exit code when available, interruption/background-admission distinction | Live `exit 7`: error block plus `toolUseResult: "Error: Exit code 7"`; do not parse arbitrary stdout as an exit code |
| File change | Write/Edit target in header; confirmed creation or actual native patch hunks use shared file/diff rows | Live Write create and Edit at the same disposable path; original line positions and no-newline markers retained |
| Unfinished / denied file change | Labeled proposal, failure retained, no fabricated applied patch | Regression tests, live file approval; updated approval input does not get replaced by the original model proposal |
| Read / search / generic tools | Target/query in header where known; input, result, structured native details retained | Live Read and ToolSearch. Grep/Glob are absent from this installed native registry; their conventional shapes have synthetic tests, not live certification |
| Web | WebSearch query and recognized links; WebFetch URL and response/error in shared web cards | Installed input schema and synthetic result-shape tests; live network search was not exercised in this pass |
| Subagent / workflow | Recognized invocation uses shared Subagent activity card; native result preserved | Invocation projection only. Nested agent streaming, ordering, teams, and background completion are **not certified** |
| Plan / review | Native mode tools remain tool activities; no invented Codex review or plan-state events | Mode-specific interactions remain unimplemented in Harness |
| Compaction / errors | Native compaction boundary and system/API errors visible; timing bookkeeping omitted | Synthetic regression tests; not a live compaction/retry test |
| Image / document / Artifact | Explicit attachment fallback or generic tool details; unknown content blocks retained | No embedded Artifact editor, image submission, or attachment parity claimed |
| Approval | Verified Write/Edit and Bash permission kinds use common RequestSurface, exact input, allow once/deny | Live Write approved through Harness; native Bash reply contract tested with bounded command; malformed/unknown kinds get no guessed controls |
| Questions | Ordinary AskUserQuestion choice dialogs use the existing RequestSurface, including multiple selections and custom text | Native capture plus installed 2.1.263 schema establish question-text keys and comma-separated multi-select answers. Rich previews/new question kinds remain unsupported, not silently simplified. Replies require the checkedDialogReplies adapter capability. |

Parallel results are matched by `tool_use_id`, not order. Envelope metadata is
associated only when exactly one result block identifies its owner. An orphaned
result remains visible instead of silently disappearing. A missing result is
labeled unavailable, not presumed interrupted or successful. Native metadata is
kept in `raw.nativeToolResult`; display summaries are not its replacement.

## Protocol and ownership boundary

| Surface | Handling | Limits |
| --- | --- | --- |
| Snapshot / transcript | Authoritative full-message replacement, native session/epoch identity, stable tool IDs | Mid-answer reconnect may wait for a committed text block to recover pre-disconnect text; no durable frontend event journal |
| Turn / stream | Native activity flags; engine text/thinking/tool-input deltas | Native `stream` store progress, usage telemetry, and in-progress tool-ID operations are not fully projected |
| Dialog changes | Authoritative open-dialog snapshot; resolved requests disappear. Changed questions clear the old form; the bridge compares the full expected descriptor before answering | Elicitation, Artifact/source-hash approvals and other unknown dialog contracts remain unimplemented. Closing another client's question never causes a synthetic answer. |
| Prompt admission | Durable native-profile UUID claims and receipts, exact transcript reconciliation, no blind resend; draft retained on uncertainty | Receipt establishes admission, not completion across a native crash. Slash commands and attachments are not enabled by this endpoint. |
| Interrupt / permission reply | Same native owner; expected session, epoch and pending dialog ID; updated adapters also compare expected descriptors | No background-task cancellation UI or guessed approval persistence |
| Disconnect / frontend restart | Passive reconnect doesn't launch. Selecting a stopped saved conversation uses serialized native-supervised resume and full-history verification | Arbitrary running terminals require native /bg handoff; Harness never seizes them. Missing or uncertain history remains a visible failure. |
| Model / effort / session controls | Unimplemented in Harness | Removing the terminal escape hatch doesn't establish control parity |
| Unknown data | Unknown content/system messages and orphan tool results remain inspectable; unknown dialogs visible without controls | Internal attachment context and unrelated engine bookkeeping are intentionally omitted. Nested progress needs a dedicated mapping rather than dumping every internal event into the transcript |

## Validation standard

The Rust tests in `claude_native.rs` cover mapping, status, correlation, partial
commits, native metadata, and fail-closed permission shapes. Shared renderer
tests remain in `main.rs`. Live tests use only disposable fixtures and the
existing authenticated native process; no real user thread is resumed or
mutated. GUI tests must use `gui_probe.mjs` with mount/network-isolated Xvfb and
an explicit private XCB socket for Harness. Never launch a host Xvfb, including
an automatically allocated or supposedly unused numbered display.

Remaining high-value work: nested/background-agent event fixtures, live web and
MCP result coverage, streaming recovery/usage, rich questions and Artifact
presentation, and attachments. These are explicit preview gaps, not reasons to
fork the renderer or represent all Claude activity as generic tool cards.

The product target is one conversation workflow: select a thread and work in
Harness. Native worker discovery text is not connection status. The permanent
discovery footer and terminal-launch button have been removed; successful
connections are quiet and failures retain retry/error UI. This is not merely a
rename of terminal-only flows: every unsupported interaction above is unfinished
integration work. Routine use must not depend on a terminal escape hatch.

First saved-thread opening or creation now requests setup consent in place and
continues the original action after approval. Cancellation neither launches nor
mutates settings. An older supervisor launcher loads the current Harness package
for future workers without changing settings or restarting existing workers.
This does not upgrade an already-running adapter, instrument a worker that predates
setup, or make an unverified Claude release compatible.
