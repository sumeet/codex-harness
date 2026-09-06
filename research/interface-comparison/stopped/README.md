# Stop marker native QA — 2026-09-05

Same optimized Harness build at 1280×720 and 640×720 logical pixels, 1.5×
display scale. Base16 Catppuccin Latte, IBM Plex Sans Condensed 17/400 reading,
VictorMono Nerd Font 14/400 code. Fixture-only process on a temporary headless
display; the live app and daemon were not restarted.

The fixture shows ordinary stopped turns as unboxed muted rows, alongside an
unchanged actual-error card. The marker shares the transcript/composer gutter.
Protocol and app projection tests verify its one selectable `Stopped` string.

Continue is intentionally absent in the offline fixture: the actual action is
only offered for a ready, attached, idle Codex thread. This capture therefore
does not claim live-button/end-to-end acceptance. The client empty-input request
and turn lifecycle behavior have automated coverage; first live use remains a
user-triggered check, not a fixture prompt submitted to a real account.

Replay:

```sh
target/release-fast/harness --comparison-fixture research/interface-comparison/stopped/fixture.json --comparison-profile research/interface-comparison/stopped/profile.json
```
