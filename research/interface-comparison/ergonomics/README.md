# Queue and tool-header ergonomics

Native GPUI checks at 1280×900 and 700×900, on a separate Weston/Xvfb display.
The fixture has no App Server connection and never executes its sample commands.
Typography uses the appearance pilot's saved reading/code families and sizes.

- [Wide overview](wide.png): shared command icon/text columns, full-size exit
  labels, actual search queries, and distinct open-page/find actions.
- [Expanded command](command-expanded.png): the icon stays aligned with the
  first line; explicit continuation lines keep the command-text indentation.
- [Expanded prompt](prompt-expanded.png): full paragraphs, blank lines, and the
  final line remain readable; individual prompts expand independently.
- [Narrow prompt](narrow-prompt.png): wrapping and all original text at 700px.
- [Narrow expanded search](narrow-search.png): both complete queries remain
  accessible when the compact header is truncated.

Native interactions checked expanding/collapsing each queued prompt, multiple
expanded prompts and queue scrolling. Clipboard comparisons checked that Copy
text reproduces each original prompt exactly, including blank lines,
indentation, accented characters, and Japanese text. Unit coverage checks
refresh after external edits, reordering/removal, full attachment coverage,
search/open/find classification, and missing versus explicitly empty results.
Result counts describe the entries supplied with that web activity, not cited
sources or independently verified domains. Missing result data shows no count.

These screenshots were captured from binary SHA-256
`db2d61c5ed03a18f3bc884b768c117e08281da867750e151bff60bafd0b2e130`.
A subsequent callback-only change reads a prompt when clicked instead of
cloning its potentially large attachments on every render; it does not change
the depicted layout.
The final binary, SHA-256
`b1f2ece11f5a509623de8aae2fd3fce38f7bff74659b092f4f04378ccc5d3c89`,
was rechecked at 700px: opening the prompt and copying its text still passed
the exact clipboard comparison.

Regression run: 220 app tests and 120 protocol tests passed; one app network
test intentionally ignored. The test command keeps desktop-version discovery
and display access disabled:

```sh
env -u DISPLAY -u WAYLAND_DISPLAY HARNESS_CHATGPT_DESKTOP_VERSION=0.0.0-test cargo test -q -j1 --profile release-fast -p harness_app -p harness_protocol -- --test-threads=1
```

Reproduce after building with one Cargo worker:

```sh
HARNESS_BUILD_JOBS=1 ./script/build-standalone.sh
python3 research/interface-comparison/ergonomics/verify.py --weston-root /path/to/extracted/weston
python3 research/interface-comparison/ergonomics/verify.py --width 700 --weston-root /path/to/extracted/weston
```

The checker prints its isolated state directory and accepts `capture NAME`,
`click X Y`, `scroll X Y up|down COUNT`, `key KEY`, `check-copy INDEX`, and `quit`.
Click Copy text before `check-copy`; zero-based indices refer to fixture prompts.
Exiting stops only the checker's own app, compositor, and display processes.
