# Codex terminal output investigation

Date: 2026-09-05. Investigation only; no executor, daemon configuration, or
command-output rendering changes have been enabled.

## Desired behavior

Execute each agent command once. Show terminal styling and line updates in
Harness, using the user's theme, while returning readable plain text to Codex.
Keep built-in Codex tools, approval/sandbox behavior, cancellation, process
continuation, and Harness's transcript selection. Do not require the model to
add color flags to commands.

## Evidence and version boundaries

- Installed CLI: `codex-cli 0.153.4`. Its generated experimental App Server
  schema includes `environment/add` parameters (`environmentId`,
  `execServerUrl`) and `turn/start.environments`.
- Schema generated without starting a daemon or model turn. Reproduce with
  `codex app-server generate-json-schema --experimental --out <scratch-directory>`.
- The inspected implementation snapshot declares version **0.153.0**, not
  0.153.4. Source paths below are relative to the official Codex repository's
  `codex-rs/` directory. Implementation conclusions below are source findings,
  not proof of every installed patch-level detail. The later stop/continue
  investigation in the root handoff separately inspected exact 0.153.4 code.
- [Official App Server documentation](https://learn.chatgpt.com/docs/app-server)
  documents standalone PTY commands and client-executed dynamic tools. It does
  not establish a supported rich-user-output/plain-model-output configuration
  switch for built-in shell execution.

## Where output currently goes

1. `core/src/unified_exec/process_manager.rs` applies `NO_COLOR=1`, `TERM=dumb`,
   and an empty `COLORTERM`. The built-in `exec_command` arguments default
   `tty` to false (`core/src/tools/handlers/unified_exec.rs`).
2. The process output is both streamed as command events and collected for
   tool responses. App Server's event mapping does UTF-8 conversion, not a
   terminal-to-plain-text projection.
3. `core/src/tools/context.rs` stores `ExecCommandToolOutput.raw_output` and
   converts/truncates it for the model response. There is no independent
   terminal-rendered text projection in that path.
4. Harness appends command deltas in `crates/harness_protocol/src/lib.rs`.
   `normalize_transcript_line_endings` in the app converts bare carriage returns
   to newlines. Command output is plain text, without an ANSI/terminal parser.

Prior probes on this installation found: Cargo automatic color produced no ANSI
with pipes, explicit color produced ANSI that survived transport, and requesting
a PTY alone still left Cargo automatic color uncolored. Consequently an ANSI
renderer alone cannot restore styling which the command never emitted.

Shell environment configuration is not a robust complete fix. The unified-exec
environment overrides the initial configured environment. Explicit overrides
can be replayed after a shell snapshot in some launch paths; that is conditional
on the shell/snapshot path, and still does not separate UI bytes from model text.

## Integration options

### Renderer only

Useful for ANSI already present, but insufficient for automatic coloring. A
stateful terminal parser is needed for carriage-return progress, erase commands,
and escape sequences split across streaming chunks. A regex removing escapes
does not handle terminal state. Keep raw input separate from rendered text and
derive Vim/copy geometry from the rendered projection.

### Change Codex's execution/output boundary

The direct implementation route is an opt-in execution policy in Codex that
preserves raw terminal output for client events and produces terminal-derived
plain text for model responses, including later `write_stdin` polls. This would
require an upstream change or maintaining a Codex patch, not just Harness code.
PTY behavior must remain deliberate: it changes buffering, stdout/stderr
combination, and interaction, not only colors.

### Experimental execution-server adapter

There is a possible route that retains the built-in tools without modifying
their model-facing names. The installed schema can select an execution server.
The source's `open_session_with_prepared_exec_env` routes selected remote
environments to `backend.start(ExecParams)`. The protocol carries command,
environment, PTY choice, sandbox/network intent, process handles, and sequenced
byte chunks (`exec-server-protocol/src/protocol.rs`).

Inference, not an implemented or verified solution: an adapter could retain a
raw terminal stream for Harness and return a plain-text projection to Codex.
This needs a second data channel and reliable correlation to transcript items;
the normal command events would otherwise contain only the cleaned stream.
It also affects environment discovery, filesystem/config access, shell snapshots,
permissions, and process lifetime. This is not a drop-in color switch.

### Standalone command APIs or a dynamic terminal tool

Follow-up: **the built-in shell tools can actually be disabled** with
`features.shell_tool = false`. This is documented in the
[configuration reference](https://learn.chatgpt.com/docs/config-file/config-reference).
`codex --disable shell_tool features list` on the installed 0.153.4 CLI reports
`shell_tool` false while `unified_exec` stays true; this command does not save
configuration. The source test
`disabling_shell_tools_disables_command_tools_for_all_environments` verifies
that disabling the feature removes both `exec_command` and `write_stdin` from
the visible and registered tool sets. Disabling `unified_exec` alone is not
equivalent: the ordinary shell implementation can remain available.

However, the registry unconditionally rejects external tools named
`exec_command` or `shell_command` in the default namespace, even without a
built-in collision. A separately named/namespaced terminal tool can coexist
with the built-ins disabled. This is a real disable-and-replace arrangement,
not a same-name override or an instruction to the model to ignore another tool.
App Server accepts per-thread configuration overrides, so a future experiment
need not change the user's global shell-tool setting.

`command/exec` supports a PTY and streaming under the server's sandbox, but
does not intercept the built-in agent shell. `process/spawn` is explicitly
outside Codex's sandbox and is not an equivalent substitute. A dynamic tool
could own both outputs and replace the disabled shell tools, but changes the
tool surface the model uses. Harness would need to own process tracking,
approval integration, cancellation, continuation, raw-output persistence, and
the mapping into its existing command cards. Merely registering the tool does
not automatically inherit those built-in behaviors.

Post-tool hooks are not an established output-replacement solution here. The
inspected source explicitly tests `updatedMCPToolOutput` as unsupported, and
post-tool context does not replace the built-in shell's model result.

## Recommended next experiment

Before integrating any of these into real threads, use an isolated, model-free
execution-server prototype to determine whether the adapter route is worth its
complexity. Start with recorded protocol requests and harmless fixture commands.
It must prove:

- One process execution, with raw ANSI retained and readable text returned.
- Correct progress-line replacement and split UTF-8/escape sequences across
  arbitrary chunk boundaries; bounded storage and explicit truncation behavior.
- Correct correlation, ordering, continuation/polling, exit status, and Stop.
- Sandbox and network restrictions preserved, failing closed when unsupported.
- No global configuration changes, live daemon restart, or rewritten commands.

If preserving the execution contract requires a broad replacement backend,
prefer a narrowly scoped upstream output-policy change. Do not silently deploy
an experimental adapter for a cosmetic improvement.
