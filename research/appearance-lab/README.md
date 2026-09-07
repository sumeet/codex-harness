# Appearance lab

Open [the A/B viewer](pilot/viewer.html). It works directly from disk, without a
server or account. Try **Less chroma**, then **Fewer syntax groups**, switching
between A and B. Try **Intermediate warm paper** against its light reference.
These are actual native Harness captures, not an HTML approximation of the app.

This is a working pilot of **authored starting points + controlled local
changes**, not a replacement theme system or a validated reduction of the
catalog. Existing themes and live preferences are unchanged. Generated variants
are installed only into temporary preview configurations.

## What the first pass found

- The native registry exposed **206 selectable themes: 143 dark, 63 light** on
  this installation. [The audit](catalog-audit.json) records resolved role
  fingerprints, background coordinates, syntax assignments, and limited contrast
  checks. It found one identical exported projection: `Dracula Light (Alucard`
  and `Dracula Light (Alucard)`. This does **not** mean every other appearance is
  meaningfully distinct, or that these two are equivalent in every possible UI.
- Fenced transcript code was created with **no language registry**. Rust blocks
  were monochrome even though Bash tool commands used syntax colors. The app now
  passes its existing theme-aware registry to Markdown. The native captures
  verify Rust and JSON syntax coloring. This is a concrete instance of the app
  hiding imported theme distinctions, not evidence that those themes lack them.
  Unsupported languages still fall back to plain text; no new grammars were
  bundled. The registry already observes live theme changes.
- One Dark has 18 distinct explicit syntax colors in the export. The sparse
  variant has 6, with the same reading background and diagnostic colors. The
  low-chroma variant mostly retains distinct assignments. These are different
  operations, not two labels for one “quietness” slider.
- The warm experiment's reading plane is `#aba494`, approximately OKLab
  lightness 0.720 and relative luminance 0.373. Those numbers are different
  quantities. It remains dark text on a lighter surface; it does not cross the
  polarity boundary or demonstrate that every middle-luminance background works.
  Its normal-surface repair changes 51 foreground/capture entries, explicitly
  recorded in [the manifest](pilot/manifest.json). It passes the limited nominal
  checks, not an accessibility certification or sustained-use evaluation.

## Pilot contents

| Reference | Why it is in the pilot |
| --- | --- |
| One Dark / One Light | Controlled dark/light pair and transformation starting points |
| Gruvbox Dark | Warm dark environment and earthy syntax |
| Everforest Dark Medium (regular) | Cool dark surfaces with green and warm syntax accents |
| Nord Dark | Cool surfaces with restrained syntax relationships |
| GitHub Light | Neutral light surfaces and different syntax assignments |
| Ayu Light | Warm accents on a light reading plane |
| Rosé Pine Dawn | Warm light surfaces and softer color relationships |

These are deliberately chosen examples, **not eight proven aesthetic clusters**.
In particular, this pilot is weak on strongly saturated backgrounds, blur,
mixed-polarity regions, and outline-led interfaces. Nothing has been merged,
deleted, or assigned to a family based on names alone.

The four transformations are independently inspectable:

| Change | Operation | Preserved by the operation |
| --- | --- | --- |
| Flatter surfaces | Move explicit opaque neutral surfaces toward the reading plane in OKLab | Syntax, foregrounds, diagnostic colors |
| Less chroma | Scale syntax chroma within gamut at approximately fixed perceptual lightness | Surfaces, role assignments, typography, diagnostics |
| Fewer syntax groups | Retain keyword/string/comment families; neutralize other explicit syntax colors | Surfaces, typography, diagnostics |
| Intermediate warm paper | Retint and darken One Light surfaces; repair foregrounds against selected normal surfaces | Light polarity and relative surface offsets, within bounds; **not** original foregrounds |

The “flatter” operation is not a complete outline/filled-control redesign. The
native app derives tool surfaces from the editor and surface roles; it does not
expose every imported token as an independent visible region. Sidebar and menu
coverage need additional fixtures before making stronger claims about chrome.

## Try it in the actual app

From the repository root, after building:

```sh
python3 research/appearance-lab/lab.py preview research/appearance-lab/pilot quiet
python3 research/appearance-lab/lab.py preview research/appearance-lab/pilot warm-paper --scene states
```

This intentionally opens a **separate** native fixture window, with temporary
XDG config/data/state/cache directories. It does not restart the live app or
submit fixture commands to a model. Close the preview window when done. Temporary
state and logs are retained and their location is printed.

The screenshots use IBM Plex Sans Condensed 20/400 and VictorMono Nerd Font 17,
at a 1280×900 logical viewport and 1× capture scale. Install those fonts before
regenerating; a font fallback invalidates a matched typography comparison.
Preview captures are for color and layout, not a verdict on your live display's
text rasterization. The A/B viewer offers fit, original logical size, and
physical-pixel modes.

## Generate another experiment

The parameters are real generator inputs, currently exposed by the lab command
rather than production settings. Choose a **new** output directory:

```sh
HARNESS_BUILD_JOBS=1 ./script/build-standalone.sh
python3 research/appearance-lab/lab.py prepare research/appearance-lab/round-two --preferences research/appearance-lab/pilot/preferences.json --flatness 0.5 --chroma 0.6 --warm-lightness 0.8
python3 research/appearance-lab/lab.py resolve research/appearance-lab/round-two
python3 research/appearance-lab/lab.py preview research/appearance-lab/round-two quiet
```

`prepare` refuses an existing directory. `resolve` updates only generated
resolved-color checks and the viewer within that prepared directory. `preview`
loads the selected generated pack into a new temporary configuration. The source
reference themes are never rewritten. Surface flatness and syntax chroma range
from 0 to 1; the warm-paper experiment is restricted to lightness 0.68–0.94 on
its light branch. These bounds are prototype policies, not natural dimensions of
appearance. Arbitrary combinations have not been validated.

On Linux, capture all 12 entries × 2 scenes without touching the live display:

```sh
python3 research/appearance-lab/lab.py capture research/appearance-lab/round-two
```

Capture requires Xvfb, Weston with the X11 backend and kiosk shell, and
ImageMagick's `import`. `--weston-root /path/to/extracted-package` can use an
uninstalled Weston package. It creates and stops its own display and compositor.
The capture directory must be empty; preserve old captures in a different
directory before recapturing. Capture startup currently uses a short fixed
settling delay: inspect the result before treating a new machine's run as valid.
`capture-record.json` records the binary and fixture hashes, viewport, and fonts.

To export the currently installed catalog, choose a new output path:

```sh
env -u DISPLAY -u WAYLAND_DISPLAY target/release-fast/harness --export-appearance-catalog /tmp/harness-colors-round-two.json
python3 research/appearance-lab/lab.py audit /tmp/harness-colors-round-two.json /tmp/harness-audit-round-two.json
```

The export exits before GUI/session initialization, loads themes using native
default resolution, and refuses an existing output file. Build first: older
binaries do not understand this flag. External load errors fail the export
rather than silently auditing a partial collection.

## What the checks do not establish

The audit uses resolved sRGB colors and several actual Harness surface
derivations. It checks ordinary prose, muted prose, editor foreground, selected
status colors, explicit syntax colors, and a few selection/diff composites.
Transparent backgrounds without an opaque backdrop are reported as unknown.

It does **not** enumerate all nested syntax backgrounds, selected syntax,
hover/focus/inactive states, font-weight/size-dependent readability, color vision
differences, semantic distinguishability, wide-gamut displays, blur, or actual
native blending under every platform. A syntax color may intentionally be muted
or not occur in these fixtures. Counts below 4.5:1 are not a theme-quality score.
Some dark references and their pinned transformations fail checked pairs; the
generator does not silently “fix” the authored references.

Equality fingerprints include resolved ThemeColors, exported syntax channels,
Harness surfaces, selected status roles, first-player colors, and window
background mode. They exclude names and random runtime IDs, but also omit some
theme information such as other players. Equality is limited to that projection.
Raw color distance is not a proxy for valued aesthetic differences.

## Next decision

Use the viewer to identify which changes are genuinely useful and which valued
details are lost. Then add references that challenge this model—especially
different surface structures and colored environments—rather than merely more
hues of One Dark. Fit those appearances with an explicit exception budget and
reserve held-out references. Compare the result against a good visual gallery
of unmodified themes before deciding generation deserves a production UI.

If another research pass is useful, share this directory plus the original
research report and ask for critique of **the actual mapping, captures, lost
details, and constraints**. Do not ask it to declare these eight references a
complete taxonomy or to merge the other themes without reconstruction evidence.

## Verification and provenance

```sh
python3 research/appearance-lab/lab.py test
node research/appearance-lab/verify-viewer.mjs research/appearance-lab/pilot
env -u DISPLAY -u WAYLAND_DISPLAY HARNESS_CHATGPT_DESKTOP_VERSION=0.0.0-test cargo test -j1 --profile release-fast -p harness_app -p harness_editor -- --test-threads=1
```

The Python tests cover contrast reference values, compositing bounds, OKLab
roundtrips/gamut mapping, identity and pinned roles, and quantized warm-branch
foreground checks. The viewer check uses an isolated headless Chromium profile
and verifies all 24 images, A/B controls, paired experiments, scenes, sizing, and
narrow layout. The app export has a Rust test for resolved surfaces and retained
syntax typography. Native before/after captures verified the missing Markdown
registry connection. The test version environment variable prevents an installed
desktop wrapper's `--version` probe from opening a desktop window during tests.

Color operations use [Ottosson's OKLab matrices](https://bottosson.github.io/posts/oklab/)
and the [WCAG contrast calculation](https://www.w3.org/WAI/WCAG22/Understanding/contrast-minimum.html).
These provide coordinates and defined constraints, not an aesthetic preference
model. The generator and all fixtures are local; no research service is called.

Reference data is copied from existing bundled themes. Preserve the original
licenses when sharing a generated pack: [One](../../assets/themes/one/LICENSE),
[Gruvbox](../../assets/themes/gruvbox/LICENSE),
[Everforest](../../assets/themes/everforest/LICENSE),
[Nord](../../assets/themes/nord/LICENSE),
[GitHub](../../assets/themes/github-theme/LICENSE),
[Ayu](../../assets/themes/ayu/LICENSE), and
[Rosé Pine](../../assets/themes/rose-pine/LICENSE). The generated pack's attribution
does not replace those notices.
