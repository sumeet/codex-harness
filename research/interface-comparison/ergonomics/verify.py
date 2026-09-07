"""Inspect the native fixture on an isolated display, without a live Codex connection."""

import argparse
import importlib.util
from pathlib import Path
import select
import subprocess
import time


ROOT = Path(__file__).resolve().parents[3]
HERE = Path(__file__).resolve().parent
specification = importlib.util.spec_from_file_location("appearance_lab", ROOT / "research/appearance-lab/lab.py")
lab = importlib.util.module_from_spec(specification)
specification.loader.exec_module(lab)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--width", type=int, default=1280)
    parser.add_argument("--height", type=int, default=900)
    parser.add_argument("--weston-root", type=Path, required=True)
    arguments = parser.parse_args()
    environment, state = lab.isolated_environment(ROOT / "research/appearance-lab/pilot")
    runtime = state / "runtime"
    runtime.mkdir(mode=0o700)
    environment.update({
        "XDG_RUNTIME_DIR": str(runtime), "WAYLAND_DISPLAY": "ergonomics",
        "HARNESS_THEME": "One Dark", "HARNESS_OPEN_THREAD": "comparison-ergonomics",
        "HARNESS_CHATGPT_DESKTOP_VERSION": "0.0.0-test", "RUST_LOG": "warn",
    })
    libraries = arguments.weston_root / "usr/lib"
    backends = list(libraries.glob("libweston-*/x11-backend.so"))
    if len(backends) != 1:
        raise ValueError("Expected one Weston X11 backend")
    environment["LD_LIBRARY_PATH"] = f"{libraries}:{libraries / 'weston'}"
    environment["WESTON_MODULE_MAP"] = f"x11-backend.so={backends[0]}"
    server = compositor = application = None
    try:
        with (state / "display.log").open("w") as display_log, (state / "app.log").open("w") as app_log:
            server = subprocess.Popen([
                "Xvfb", "-displayfd", "1", "-screen", "0",
                f"{arguments.width}x{arguments.height}x24", "-nolisten", "tcp",
            ], stdout=subprocess.PIPE, stderr=display_log, text=True)
            if not select.select([server.stdout], [], [], 10)[0]:
                raise RuntimeError("Isolated display did not become ready")
            display = server.stdout.readline().strip()
            if not display.isdigit():
                raise RuntimeError("Invalid isolated display number")
            environment["DISPLAY"] = f":{display}"
            compositor = subprocess.Popen([
                str(arguments.weston_root / "usr/bin/weston"), "--backend=x11", "--renderer=pixman",
                f"--shell={libraries / 'weston/kiosk-shell.so'}",
                f"--width={arguments.width}", f"--height={arguments.height}",
                "--socket=ergonomics", "--no-config", "--idle-time=0",
            ], env=environment, stdout=display_log, stderr=subprocess.STDOUT)
            deadline = time.monotonic() + 15
            while not (runtime / "ergonomics").exists():
                if compositor.poll() is not None or time.monotonic() > deadline:
                    raise RuntimeError(f"Compositor failed; inspect {state}")
                time.sleep(0.1)
            application = subprocess.Popen([
                str(lab.BINARY), "--comparison-fixture", str(HERE / "fixture.json"),
                "--comparison-profile", str(HERE / "profile.json"),
            ], env=environment, cwd=ROOT, stdout=app_log, stderr=subprocess.STDOUT)
            time.sleep(3)
            if application.poll() is not None:
                raise RuntimeError(f"Fixture failed; inspect {state}")
            print(f"Isolated fixture ready: {state}", flush=True)
            print("Commands: capture NAME, click X Y, scroll X Y up|down COUNT, key KEY, check-copy INDEX, quit", flush=True)
            while True:
                try:
                    command = input().split()
                except EOFError:
                    break
                if not command:
                    continue
                if command[0] == "quit":
                    break
                if command[0] == "capture" and len(command) == 2:
                    name = command[1]
                    if not name.replace("-", "").isalnum():
                        raise ValueError("Capture names must be alphanumeric")
                    destination = state / f"{name}.png"
                    subprocess.run(["import", "-window", "root", str(destination)], env=environment, check=True, timeout=10)
                    print(destination, flush=True)
                elif command[0] == "click" and len(command) == 3:
                    subprocess.run(["xdotool", "mousemove", command[1], command[2], "click", "1"], env=environment, check=True, timeout=10)
                elif command[0] == "scroll" and len(command) == 5:
                    button = "4" if command[3] == "up" else "5"
                    subprocess.run(["xdotool", "mousemove", command[1], command[2], "click", "--repeat", command[4], "--delay", "80", button], env=environment, check=True, timeout=10)
                elif command[0] == "key" and len(command) == 2:
                    subprocess.run(["xdotool", "key", command[1]], env=environment, check=True, timeout=10)
                elif command[0] == "check-copy" and len(command) == 2:
                    prompt = lab.read_json(HERE / "fixture.json")["queued_prompts"][int(command[1])]
                    expected = "\n".join(block["text"] for block in prompt["input"] if block["type"] == "text")
                    actual = subprocess.run(["wl-paste", "--no-newline"], env=environment, capture_output=True, text=True, check=True, timeout=10).stdout
                    if actual != expected:
                        raise AssertionError(f"Copied text differs from prompt {command[1]}")
                    print(f"Clipboard exactly matches prompt {command[1]}", flush=True)
                else:
                    print("Unrecognized command", flush=True)
                time.sleep(0.3)
    finally:
        lab.stop_process(application)
        lab.stop_process(compositor)
        lab.stop_process(server)


if __name__ == "__main__":
    main()
