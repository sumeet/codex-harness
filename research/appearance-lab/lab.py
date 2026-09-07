#!/usr/bin/env python3
"""Offline, reversible appearance experiments using Harness's native renderer."""

import argparse
import copy
import hashlib
import json
import math
import os
from pathlib import Path
import select
import shutil
import subprocess
import sys
import tempfile
import time
import unittest


ROOT = Path(__file__).resolve().parents[2]
LAB = Path(__file__).resolve().parent
BINARY = ROOT / "target/release-fast/harness"
SURFACES = (
    "background", "surface.background", "elevated_surface.background", "panel.background",
    "editor.background", "editor.gutter.background", "editor.subheader.background",
    "status_bar.background", "title_bar.background", "title_bar.inactive_background",
    "toolbar.background", "tab_bar.background", "tab.active_background",
    "tab.inactive_background", "terminal.background", "element.background",
    "element.hover", "element.active", "element.selected", "element.disabled",
)
REFERENCES = (
    ("one-dark", "One Dark", "one/one.json", "Cool, layered baseline"),
    ("one-light", "One Light", "one/one.json", "Light counterpart; useful controlled pair"),
    ("gruvbox", "Gruvbox Dark", "gruvbox/gruvbox.json", "Warm dark environment and earthy syntax"),
    ("everforest", "Everforest Dark Medium (regular)", "everforest/everforest-regular.json", "Cool dark surfaces with green and warm syntax accents"),
    ("nord", "Nord Dark", "nord/nord.json", "Cool, restrained syntax relationships"),
    ("github", "GitHub Light", "github-theme/github_theme.json", "Neutral light surfaces, different syntax assignment"),
    ("ayu", "Ayu Light", "ayu/ayu.json", "Warm accents on a light reading plane"),
    ("rose-pine", "Rosé Pine Dawn", "rose-pine/rose-pine-dawn.json", "Warm light surfaces and softer color relationships"),
)


def read_json(path):
    return json.loads(Path(path).read_text())


def write_json(path, data):
    Path(path).write_text(json.dumps(data, ensure_ascii=False, indent=2) + "\n")


def rgba(value):
    if isinstance(value, (list, tuple)) and len(value) == 4:
        channels = tuple(float(channel) for channel in value)
    elif isinstance(value, str) and value.startswith("#"):
        digits = value[1:]
        if len(digits) in (3, 4):
            digits = "".join(digit * 2 for digit in digits)
        if len(digits) == 6:
            digits += "ff"
        if len(digits) != 8:
            raise ValueError(f"Unsupported color: {value}")
        channels = tuple(int(digits[index:index + 2], 16) / 255 for index in range(0, 8, 2))
    else:
        raise ValueError(f"Unsupported color: {value}")
    if not all(math.isfinite(channel) and -1e-6 <= channel <= 1 + 1e-6 for channel in channels):
        raise ValueError(f"Invalid color channels: {value}")
    return tuple(min(1, max(0, channel)) for channel in channels)


def hex_color(value):
    return "#" + "".join(f"{round(channel * 255):02x}" for channel in rgba(value))


def linear(channel):
    return channel / 12.92 if channel <= 0.04045 else ((channel + 0.055) / 1.055) ** 2.4


def encoded(channel):
    return channel * 12.92 if channel <= 0.0031308 else 1.055 * channel ** (1 / 2.4) - 0.055


def luminance(color):
    red, green, blue, _ = rgba(color)
    return 0.2126 * linear(red) + 0.7152 * linear(green) + 0.0722 * linear(blue)


def over(foreground, background):
    foreground, background = rgba(foreground), rgba(background)
    if background[3] < 0.999999:
        raise ValueError("A translucent backdrop needs a specified opaque underlying surface")
    alpha = foreground[3]
    return tuple(foreground[index] * alpha + background[index] * (1 - alpha) for index in range(3)) + (1,)


def contrast(foreground, background):
    background = rgba(background)
    foreground = over(foreground, background)
    first, second = sorted((luminance(foreground), luminance(background)))
    return (second + 0.05) / (first + 0.05)


def oklab(color):
    red, green, blue = (linear(channel) for channel in rgba(color)[:3])
    # Ottosson's 2021-01-25 matrices, public-domain reference implementation.
    first = (0.4122214708 * red + 0.5363325363 * green + 0.0514459929 * blue) ** (1 / 3)
    second = (0.2119034982 * red + 0.6806995451 * green + 0.1073969566 * blue) ** (1 / 3)
    third = (0.0883024619 * red + 0.2817188376 * green + 0.6299787005 * blue) ** (1 / 3)
    return (
        0.2104542553 * first + 0.7936177850 * second - 0.0040720468 * third,
        1.9779984951 * first - 2.4285922050 * second + 0.4505937099 * third,
        0.0259040371 * first + 0.7827717662 * second - 0.8086757660 * third,
    )


def from_oklab(lightness, green_red, blue_yellow):
    def convert(factor):
        first = (lightness + 0.3963377774 * green_red * factor + 0.2158037573 * blue_yellow * factor) ** 3
        second = (lightness - 0.1055613458 * green_red * factor - 0.0638541728 * blue_yellow * factor) ** 3
        third = (lightness - 0.0894841775 * green_red * factor - 1.2914855480 * blue_yellow * factor) ** 3
        return (
            4.0767416621 * first - 3.3077115913 * second + 0.2309699292 * third,
            -1.2684380046 * first + 2.6097574011 * second - 0.3413193965 * third,
            -0.0041960863 * first - 0.7034186147 * second + 1.7076147010 * third,
        )

    if not 0 <= lightness <= 1:
        raise ValueError("Lightness must stay within one feasible branch")
    low, high = 0.0, 1.0
    for _ in range(24):
        factor = (low + high) / 2
        if all(-1e-9 <= channel <= 1 + 1e-9 for channel in convert(factor)):
            low = factor
        else:
            high = factor
    return tuple(min(1, max(0, encoded(channel))) for channel in convert(low)) + (1,)


def fingerprint(value):
    return hashlib.sha256(json.dumps(value, sort_keys=True, separators=(",", ":")).encode()).hexdigest()


def projection(theme):
    return {key: theme[key] for key in ("appearance", "colors", "syntax", "harness", "status", "local_player", "window_background")}


def checks(theme):
    colors, surfaces = theme["colors"], theme["harness"]
    background = surfaces["transcript"]
    pairs = [("prose", colors["text"], background), ("muted prose", colors["text_muted"], background),
             ("editor foreground", colors["editor_foreground"], colors["editor_background"]),
             ("rail text", colors["text"], surfaces["rail"])]
    for role, foreground in theme["status"].items():
        if not role.endswith("_background"):
            pairs.append((f"status/{role}", foreground, background))
    for role, highlight in theme["syntax"].items():
        if highlight["color"] is not None:
            pairs.append((f"syntax/{role}", highlight["color"], colors["editor_background"]))
    selection = (theme.get("local_player") or {}).get("selection")
    if selection is not None and rgba(background)[3] >= 0.999999:
        selected_background = over(selection, background)
        pairs.append(("selected prose", colors["text"], selected_background))
        for role in ("diff_added_surface", "diff_deleted_surface"):
            diff_background = over(surfaces[role], background)
            selected_diff = over(selection, diff_background)
            pairs.append((f"selected/{role}/foreground", colors["editor_foreground"], selected_diff))
    results = []
    for role, foreground, backdrop in pairs:
        try:
            ratio = contrast(foreground, backdrop)
            results.append({"role": role, "ratio": round(ratio, 3), "below_4_5": ratio < 4.5})
        except ValueError as error:
            results.append({"role": role, "unknown": str(error)})
    return results


def audit(catalog):
    groups = {}
    rows = []
    for theme in catalog["themes"]:
        identity = fingerprint(projection(theme))
        groups.setdefault(identity, []).append(theme["name"])
        background = theme["harness"]["transcript"]
        lightness, green_red, blue_yellow = oklab(background)
        observations = checks(theme)
        rows.append({
            "name": theme["name"], "appearance": theme["appearance"],
            "background": hex_color(background), "background_luminance": round(luminance(background), 5),
            "background_oklab_lightness": round(lightness, 5),
            "background_oklab_chroma": round(math.hypot(green_red, blue_yellow), 5),
            "syntax_capture_count": len(theme["syntax"]),
            "syntax_distinct_explicit_colors": len({tuple(value["color"]) for value in theme["syntax"].values() if value["color"]}),
            "projection_fingerprint": identity,
            "below_target": [item for item in observations if item.get("below_4_5")],
            "unknown_pairs": [item for item in observations if "unknown" in item],
        })
    return {
        "scope": catalog["scope"], "theme_count": len(rows),
        "appearance_counts": {mode: sum(row["appearance"] == mode for row in rows) for mode in ("dark", "light")},
        "equal_exported_projection_groups": [names for names in groups.values() if len(names) > 1],
        "warning": "Equality is only within the exported role projection. Contrast checks are an incomplete nominal-color screen, not accessibility certification or a reason to delete an authored theme.",
        "themes": rows,
    }


def load_reference(path, name):
    family = read_json(ROOT / "assets/themes" / path)
    return copy.deepcopy(next(theme for theme in family["themes"] if theme["name"] == name))


def transform(base, surface_flatness=0.0, syntax_chroma=1.0, sparse=False):
    if not 0 <= surface_flatness <= 1 or not 0 <= syntax_chroma <= 1:
        raise ValueError("Local transformation settings must be between zero and one")
    result = copy.deepcopy(base)
    style = result["style"]
    if surface_flatness:
        reading = oklab(style["editor.background"])
        for role in SURFACES:
            if style.get(role) and rgba(style[role])[3] == 1:
                original = oklab(style[role])
                mixed = tuple(left * (1 - surface_flatness) + right * surface_flatness for left, right in zip(original, reading))
                style[role] = hex_color(from_oklab(*mixed))
    for role, highlight in style.get("syntax", {}).items():
        if not highlight.get("color"):
            continue
        if sparse and role.split(".")[0] not in {"keyword", "string", "comment"}:
            highlight["color"] = style["editor.foreground"]
        elif syntax_chroma != 1:
            lightness, green_red, blue_yellow = oklab(highlight["color"])
            alpha = rgba(highlight["color"])[3]
            converted = from_oklab(lightness, green_red * syntax_chroma, blue_yellow * syntax_chroma)
            highlight["color"] = hex_color(converted[:3] + (alpha,))
    return result


def warm_reading_plane(base, lightness=0.72):
    if base["appearance"] != "light" or not 0.68 <= lightness <= 0.94:
        raise ValueError("Warm-paper experiment is limited to a light branch, L=0.68..0.94")
    result = copy.deepcopy(base)
    style = result["style"]
    original_lightness = oklab(style["editor.background"])[0]
    hue = math.radians(85)
    for role in SURFACES:
        if style.get(role) and rgba(style[role])[3] == 1:
            offset = oklab(style[role])[0] - original_lightness
            style[role] = hex_color(from_oklab(min(0.97, max(0.55, lightness + offset)), 0.024 * math.cos(hue), 0.024 * math.sin(hue)))
    backgrounds = [style[role] for role in ("editor.background", "surface.background", "panel.background")]
    repairs = []

    def repair(color, role):
        if min(contrast(color, backdrop) for backdrop in backgrounds) >= 4.5:
            return color
        start, green_red, blue_yellow = oklab(color)
        for step in range(1, 201):
            candidate = hex_color(from_oklab(start * (1 - step / 200), green_red, blue_yellow))
            if min(contrast(candidate, backdrop) for backdrop in backgrounds) >= 4.5:
                repairs.append(role)
                return candidate
        raise ValueError(f"Cannot meet the declared contrast floor for {role}")

    for role in ("text", "text.muted", "text.placeholder", "text.accent", "editor.foreground", "icon", "icon.muted", "icon.accent", "error", "warning", "success", "info", "hint"):
        if style.get(role):
            style[role] = repair(style[role], role)
    for role, highlight in style.get("syntax", {}).items():
        if highlight.get("color"):
            highlight["color"] = repair(highlight["color"], f"syntax/{role}")
    return result, repairs


def prepare(output, preferences, flatness=1.0, chroma=0.25, warm_lightness=0.72):
    output.mkdir(parents=True, exist_ok=False)
    themes, entries = [], []
    for identity, name, source, reason in REFERENCES:
        theme = load_reference(source, name)
        themes.append(theme)
        entries.append({"id": identity, "name": name, "kind": "authored reference", "reason": reason, "source": f"assets/themes/{source}", "changed_roles": []})
    baseline = themes[0]
    variants = [
        ("flat", "Lab · Flatter surfaces", transform(baseline, surface_flatness=flatness), "Same syntax, foregrounds, and status colors; neutral surfaces converge on the reading plane.", {"surface_flatness": flatness}),
        ("quiet", "Lab · Lower syntax chroma", transform(baseline, syntax_chroma=chroma), "Same syntax assignments and approximate lightness; chroma is reduced in OKLab.", {"syntax_chroma": chroma}),
        ("sparse", "Lab · Fewer syntax groups", transform(baseline, sparse=True), "Keywords, strings, and comments keep their colors; other syntax becomes editor foreground. This intentionally changes assignments.", {"sparse": True}),
    ]
    warm, repairs = warm_reading_plane(themes[1], warm_lightness)
    variants.append(("warm-paper", "Lab · Intermediate warm paper", warm, "Light-branch stress case, not a dark-to-light interpolation. Foregrounds are explicitly adjusted to a 4.5:1 normal-surface target; nested states still need separate checks.", {"lightness": warm_lightness, "repairs": repairs}))
    for identity, name, theme, reason, parameters in variants:
        base = themes[1] if identity == "warm-paper" else baseline
        theme["name"] = name
        themes.append(theme)
        changed = [role for role in sorted(set(base["style"]) | set(theme["style"])) if base["style"].get(role) != theme["style"].get(role)]
        entries.append({"id": identity, "name": name, "kind": "experimental transformation", "reason": reason, "base": base["name"], "parameters": parameters, "changed_roles": changed})
    write_json(output / "themes.json", {"name": "Harness Appearance Lab", "author": "Original theme authors; experimental transformations by Harness", "themes": themes})
    write_json(output / "preferences.json", preferences)
    write_json(output / "profile.json", {"sidebar_open": False})
    write_json(output / "manifest.json", {"schema_version": 1, "entries": entries, "preferences": preferences, "capture": {"width": 1280, "height": 900, "scale": 1}, "scope": "Pilot exemplars and local transformations, not a validated reduction of the full catalog."})
    return entries


def isolated_environment(prepared):
    state = Path(tempfile.mkdtemp(prefix="harness-appearance-preview-"))
    for directory in ("config/harness/themes", "data", "state", "cache"):
        (state / directory).mkdir(parents=True, exist_ok=True)
    write_json(state / "config/harness/themes/lab.json", read_json(prepared / "themes.json"))
    write_json(state / "config/harness/preferences.json", read_json(prepared / "preferences.json"))
    environment = dict(os.environ)
    for suffix in ("CONFIG", "DATA", "STATE", "CACHE"):
        environment[f"XDG_{suffix}_HOME"] = str(state / suffix.lower())
    for key in ("HARNESS_OPEN_THREAD", "HARNESS_REPLAY_COUNT", "HARNESS_COMPARISON_FIXTURE", "HARNESS_COMPARISON_PROFILE", "HARNESS_THEME"):
        environment.pop(key, None)
    return environment, state


def resolve(prepared):
    environment, state = isolated_environment(prepared)
    environment.pop("DISPLAY", None)
    environment.pop("WAYLAND_DISPLAY", None)
    destination = state / "resolved.json"
    subprocess.run([str(BINARY), "--export-appearance-catalog", str(destination)], env=environment, cwd=ROOT, check=True, timeout=20)
    catalog = read_json(destination)
    manifest = read_json(prepared / "manifest.json")
    selected_names = {entry["name"] for entry in manifest["entries"]}
    selected = [theme for theme in catalog["themes"] if theme["name"] in selected_names]
    if len(selected) != len(selected_names):
        raise ValueError("Not all pilot themes reached the native registry")
    catalog["themes"] = selected
    write_json(prepared / "resolved-colors.json", catalog)
    write_json(prepared / "checks.json", audit(catalog))
    viewer_data = json.dumps({"manifest": manifest, "checks": read_json(prepared / "checks.json")}, ensure_ascii=False).replace("<", "\\u003c")
    template = (LAB / "viewer-template.html").read_text()
    (prepared / "viewer.html").write_text(template.replace("__LAB_DATA__", viewer_data))
    print(f"Resolved {len(selected)} pilot themes through the native registry")


def stop_process(process):
    if process is not None and process.poll() is None:
        process.terminate()
        try:
            process.wait(timeout=5)
        except subprocess.TimeoutExpired:
            process.kill()
            process.wait(timeout=5)


def capture(prepared, weston_root=None):
    for executable in ("Xvfb", "import"):
        if not shutil.which(executable):
            raise ValueError(f"Native capture requires {executable}")
    weston = str(weston_root / "usr/bin/weston") if weston_root else shutil.which("weston")
    if not weston or not Path(weston).is_file():
        raise ValueError("Native capture requires Weston (or --weston-root for an extracted package)")
    manifest = read_json(prepared / "manifest.json")
    destination = prepared / "captures"
    if destination.exists() and any(destination.iterdir()):
        raise ValueError("Capture directory is not empty; preserve it and prepare a new experiment")
    destination.mkdir(exist_ok=True)
    environment, state = isolated_environment(prepared)
    runtime = state / "runtime"
    runtime.mkdir(mode=0o700)
    environment["XDG_RUNTIME_DIR"] = str(runtime)
    environment["WAYLAND_DISPLAY"] = "appearance-lab"
    environment["RUST_LOG"] = "warn"
    server = compositor = application = None
    try:
        with (state / "display.log").open("w") as display_log:
            server = subprocess.Popen(["Xvfb", "-displayfd", "1", "-screen", "0", "1280x900x24", "-nolisten", "tcp"], stdout=subprocess.PIPE, stderr=display_log, text=True)
            if not select.select([server.stdout], [], [], 10)[0]:
                raise ValueError("Isolated display did not become ready")
            display_number = server.stdout.readline().strip()
            if not display_number.isdigit():
                raise ValueError("Isolated display did not return a display number")
            environment["DISPLAY"] = f":{display_number}"
            shell = "kiosk-shell.so"
            if weston_root:
                libraries = weston_root / "usr/lib"
                backends = list(libraries.glob("libweston-*/x11-backend.so"))
                if len(backends) != 1:
                    raise ValueError("Expected exactly one extracted Weston X11 backend")
                environment["LD_LIBRARY_PATH"] = f"{libraries}:{libraries / 'weston'}"
                environment["WESTON_MODULE_MAP"] = f"x11-backend.so={backends[0]}"
                shell = str(libraries / "weston/kiosk-shell.so")
            compositor = subprocess.Popen([weston, "--backend=x11", "--renderer=pixman", f"--shell={shell}", "--width=1280", "--height=900", "--socket=appearance-lab", "--no-config", "--idle-time=0"], env=environment, stdout=display_log, stderr=subprocess.STDOUT)
            deadline = time.monotonic() + 15
            while not (runtime / "appearance-lab").exists():
                if compositor.poll() is not None or time.monotonic() > deadline:
                    raise ValueError(f"Isolated compositor failed; inspect {state / 'display.log'}")
                time.sleep(0.1)
            window = "root"
            for entry in manifest["entries"]:
                environment["HARNESS_THEME"] = entry["name"]
                for scene in ("reading", "states"):
                    log_path = state / f"{entry['id']}-{scene}.log"
                    with log_path.open("w") as application_log:
                        application = subprocess.Popen([str(BINARY), "--comparison-fixture", str(LAB / f"{scene}.json"), "--comparison-profile", str(prepared / "profile.json")], env=environment, cwd=ROOT, stdout=application_log, stderr=subprocess.STDOUT)
                        time.sleep(2.5)
                        if application.poll() is not None:
                            raise ValueError(f"Fixture stopped early; inspect {log_path}")
                        subprocess.run(["import", "-window", window, str(destination / f"{entry['id']}-{scene}.png")], env=environment, check=True, timeout=10)
                        stop_process(application)
                        application = None
                    log = log_path.read_text()
                    if "theme not found" in log or "failed to initialize Harness settings" in log:
                        raise ValueError(f"Fixture did not load its theme; inspect {log_path}")
                print(f"Captured {entry['id']}: reading + states", flush=True)
            write_json(prepared / "capture-record.json", {
                "binary_sha256": hashlib.sha256(BINARY.read_bytes()).hexdigest(),
                "fixture_sha256": {scene: hashlib.sha256((LAB / f"{scene}.json").read_bytes()).hexdigest() for scene in ("reading", "states")},
                "viewport": manifest["capture"], "preferences": manifest["preferences"],
                "renderer": "Native GPUI, isolated Weston/Xvfb, 1x scale; compare color/layout, not live-display rasterization",
                "entry_count": len(manifest["entries"]),
            })
            print(f"Native captures: {destination}\nIsolated logs: {state}")
    finally:
        stop_process(application)
        stop_process(compositor)
        stop_process(server)


class LabTests(unittest.TestCase):
    def test_contrast_reference_values(self):
        self.assertAlmostEqual(contrast("#ffffff", "#000000"), 21)
        self.assertAlmostEqual(contrast("#000000", "#000000"), 1)
        self.assertEqual(hex_color(over("#ffffff80", "#000000")), "#808080ff")
        with self.assertRaises(ValueError):
            contrast("#ffffff", "#00000080")

    def test_oklab_roundtrip_and_gamut(self):
        for color in ("#000000", "#ffffff", "#ff0000", "#238765", "#4488ff"):
            actual = from_oklab(*oklab(color))
            for left, right in zip(actual, rgba(color)):
                self.assertAlmostEqual(left, right, places=5)
        self.assertTrue(all(0 <= channel <= 1 for channel in from_oklab(0.8, 0.5, -0.5)))

    def test_transform_pins_and_identity(self):
        base = load_reference("one/one.json", "One Dark")
        self.assertEqual(transform(base), base)
        flat = transform(base, surface_flatness=1)
        self.assertEqual(flat["style"]["syntax"], base["style"]["syntax"])
        self.assertEqual(flat["style"]["error"], base["style"]["error"])
        self.assertEqual(base, load_reference("one/one.json", "One Dark"))
        quiet = transform(base, syntax_chroma=0.25)
        self.assertEqual(quiet["style"]["editor.background"], base["style"]["editor.background"])
        self.assertEqual(quiet["style"]["players"], base["style"]["players"])
        sparse = transform(base, sparse=True)
        self.assertEqual(sparse["style"]["syntax"]["function"]["color"], base["style"]["editor.foreground"])
        with self.assertRaises(ValueError):
            transform(base, syntax_chroma=-1)

    def test_warm_branch_checks_final_quantized_colors(self):
        theme, repairs = warm_reading_plane(load_reference("one/one.json", "One Light"))
        self.assertTrue(repairs)
        for foreground in [theme["style"]["text"], theme["style"]["text.muted"]] + [value["color"] for value in theme["style"]["syntax"].values() if value.get("color")]:
            self.assertGreaterEqual(contrast(foreground, theme["style"]["editor.background"]), 4.5)
        with self.assertRaises(ValueError):
            warm_reading_plane(load_reference("one/one.json", "One Dark"))


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    subcommands = parser.add_subparsers(dest="command", required=True)
    audit_parser = subcommands.add_parser("audit")
    audit_parser.add_argument("catalog", type=Path)
    audit_parser.add_argument("output", type=Path)
    prepare_parser = subcommands.add_parser("prepare")
    prepare_parser.add_argument("output", type=Path, help="A NEW directory for generated artifacts")
    prepare_parser.add_argument("--preferences", type=Path, required=True)
    prepare_parser.add_argument("--flatness", type=float, default=1.0)
    prepare_parser.add_argument("--chroma", type=float, default=0.25)
    prepare_parser.add_argument("--warm-lightness", type=float, default=0.72)
    preview_parser = subcommands.add_parser("preview")
    preview_parser.add_argument("prepared", type=Path)
    preview_parser.add_argument("identity", help="Entry ID from manifest.json")
    preview_parser.add_argument("--scene", choices=("reading", "states"), default="reading")
    resolve_parser = subcommands.add_parser("resolve")
    resolve_parser.add_argument("prepared", type=Path)
    capture_parser = subcommands.add_parser("capture")
    capture_parser.add_argument("prepared", type=Path)
    capture_parser.add_argument("--weston-root", type=Path)
    subcommands.add_parser("test")
    arguments = parser.parse_args()
    if arguments.command == "test":
        result = unittest.TextTestRunner(verbosity=2).run(unittest.defaultTestLoader.loadTestsFromTestCase(LabTests))
        return 0 if result.wasSuccessful() else 1
    if arguments.command == "audit":
        if arguments.output.exists():
            raise ValueError("Audit output already exists; choose a new path")
        result = audit(read_json(arguments.catalog))
        write_json(arguments.output, result)
        print(json.dumps({key: result[key] for key in ("theme_count", "appearance_counts", "equal_exported_projection_groups")}, indent=2))
    elif arguments.command == "prepare":
        entries = prepare(arguments.output, read_json(arguments.preferences), arguments.flatness, arguments.chroma, arguments.warm_lightness)
        print(f"Prepared {len(entries)} entries in {arguments.output}")
    elif arguments.command == "preview":
        manifest = read_json(arguments.prepared / "manifest.json")
        entry = next((entry for entry in manifest["entries"] if entry["id"] == arguments.identity), None)
        if entry is None:
            raise ValueError("Unknown entry ID; inspect manifest.json")
        environment, state = isolated_environment(arguments.prepared)
        environment["HARNESS_THEME"] = entry["name"]
        print(f"Isolated preview state: {state}", flush=True)
        return subprocess.call([str(BINARY), "--comparison-fixture", str(LAB / f"{arguments.scene}.json"), "--comparison-profile", str(arguments.prepared / "profile.json")], env=environment, cwd=ROOT)
    elif arguments.command == "resolve":
        resolve(arguments.prepared)
    elif arguments.command == "capture":
        capture(arguments.prepared, arguments.weston_root)
    return 0


if __name__ == "__main__":
    try:
        sys.exit(main())
    except (ValueError, OSError, KeyError, StopIteration, subprocess.SubprocessError) as error:
        print(f"Appearance lab: {error}", file=sys.stderr)
        sys.exit(1)
