# Claude ACP work-in-progress checkpoint

Preserved on 2026-09-05 for cross-machine continuity. This branch is based on
`0ef7866b7b`, behind the main product branch `harness/main`.

The only unfinished changes are workspace registration and
`crates/claude_acp_client/Cargo.toml`. The declared library file
`src/claude_acp_client.rs` does not exist. There is no client implementation,
no composer/tool integration, and no provider test result. As preserved, this
branch is not buildable; do not merge its workspace registration into main.

The user's intended feature is to connect Claude Code through ACP and reuse
Harness's existing composer, rich transcript, Vim, tool cards, approvals,
queue/lifecycle surfaces where the provider contract supports them. Existing
Claude session continuation matters: do not assume an Agent SDK wrapper is
equivalent to attaching a native Claude Code session. Research the installed
CLI/ACP adapter's actual capabilities before choosing the boundary.

For current product context and build instructions, start with HANDOFF.md on
`harness/main`. Begin any real integration from that newer code, carrying over
only the scaffold that is still useful. This commit preserves work; it does
not claim the requested feature was implemented.
