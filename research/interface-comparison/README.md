# Interface comparison archive and native Harness lab

Curated from the local September 1–5 experiments. These are real native-client
captures of a synthetic transcript, not mockups or the user's live history.
They are historical evidence; current Harness has additional fixes.

Open `viewer.html` directly in a browser to inspect the archived Harness,
Zed, Delta, and plain-Helium captures. Its pixel-exact mode maps one captured
pixel to one physical screen pixel; logical-size mode can resample edges on
displays other than the original 1.5× screen. Do not judge rasterization from
the smaller side-by-side thumbnails.

## Controlled Harness experiment

The first four profiles hold the binary, fixture, Delta One Light theme,
IBM Plex Sans Condensed 16/400, VictorMono Nerd Font 14/400, 1.3125 line height,
and 1270×710 logical viewport at 1.5× scale constant. One axis changes the
canvas between editor and surface theme roles; the other changes actual GPUI
subpixel vs grayscale rendering. Profiles 05–08 vary typography separately.
These separate the background-contrast hypothesis from the renderer hypothesis.

```sh
HARNESS_BUILD_JOBS=1 ./script/build-standalone.sh
bash research/interface-comparison/run-harness-profile --list
bash research/interface-comparison/run-harness-profile 01-ibm400-editor-subpixel.json
```

The launcher uses a fresh temporary XDG directory and the archived theme.
Install the named fonts first; missing fonts can fall back and invalidate the
comparison. Set the window size and display scale yourself. No daemon or model
turn is used. Profile JSON applies only to a fixture launch and does not alter
normal preferences. The temporary directory is printed and retained for QA.

`fixture.json` includes the original comparison contract and illustrative
command/test output. Claims inside its transcript are fixture text, not fresh
verification results. `assets/` contains the extracted Delta theme and a small
reference SVG. The image event is not a guarantee of cross-client image parity.

## Native-policy differences in the four-client captures

The earlier experiments recorded these differences, not a wholesale port:

- Delta 0.4.0 has no reading-weight control; 400 is the honest shared baseline.
  Weight-300 captures are Harness/Zed/web comparisons, not matched Delta 300.
- Zed uses its agent-buffer font for editable prompts and code. Its inspected
  agent Markdown line height was buffer size × 1.75. `zed-matched-400-*`
  prioritizes 21px prose leading using a 12px code font; `zed-code14-400-*`
  keeps 14px code and consequently uses 24.5px prose leading.
- The accountless Delta replay had no worktree, so its patch/image operations
  honestly show native failed states. That is not successful tool-output parity.
- The web page isolates font rasterization; it is not a fourth agent layout.
  The Plex Regular file used in that experiment had SHA-256
  `e65367b51f6bb698128ff0b19ec5f0732fc4adda39b633fd65943bfacb161faf`;
  Light was `c43846acebed745929732fb0d8605fe4b7cfdb62e51a8a090eec012b34ac8059`.

The archived `web/` sources require their named TTF files under `web/fonts/`.
Those installed-font copies are not bundled. Populate them from your own font
installation and check loaded faces before treating a new web render as valid.
The archived browser PNGs work without fonts installed. Full Delta/Zed replay
adapters, patched/downloaded binaries, auth profiles, and exploratory scratch
captures are not included; this archive does not claim one-command native
regeneration of all three clients.

## Tool rhythm, September 5

`rhythm/` preserves a separate, newer experiment at Base16 Unikitty Light,
IBM Plex Sans Condensed 17/400, VictorMono Nerd Font 14/400, 1.5× display scale.
Wide captures are 1920×1080 physical / 1280×720 logical; the narrow expanded
capture is 960×1080 physical / 640×720 logical.

| Variant | Between collapsed tools | Prose/tool boundary |
| --- | ---: | ---: |
| Baseline | 0px | 13px |
| Balanced (current default) | 4px | 10px |
| Relaxed | 8px | 12px |

These are summed outer paddings in logical pixels, excluding unchanged line
boxes. Ordinary prose-to-prose padding remains 16px. `selected.png` verifies
the default without a spacing override. `expanded-wide.png` and
`expanded-narrow.png` check the larger output panels. No font-size, paragraph
spacing, horizontal gutter, or composer-geometry change is hidden in this test.

Replay with your current preferences:

```sh
target/release-fast/harness --comparison-fixture research/interface-comparison/rhythm/fixture.json --comparison-profile research/interface-comparison/rhythm/balanced.json
```

For an exact historic reproduction, isolate configuration and install/select
the recorded theme/fonts instead of overwriting your normal preferences.
