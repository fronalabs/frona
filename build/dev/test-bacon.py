#!/usr/bin/env python3
"""Exercise the installed Bacon and our real jobs without building Frona."""

import json
import os
from pathlib import Path
import shutil
import signal
import subprocess
import sys
import tempfile
import time
import unittest


ROOT = Path(__file__).resolve().parents[2]
BACON = shutil.which("bacon")
CARGO = shutil.which("cargo")


@unittest.skipUnless(BACON and CARGO, "Bacon and Cargo must be on PATH (run in the development image)")
class BaconTests(unittest.TestCase):
    def test_restart_recovery_and_shutdown(self):
        for stop_signal in (signal.SIGINT, signal.SIGTERM):
            with self.subTest(signal=stop_signal), tempfile.TemporaryDirectory(prefix="frona-bacon-test-") as directory:
                root = Path(directory)
                (root / "crates/server/src").mkdir(parents=True)
                (root / "web").mkdir()
                (root / "Cargo.toml").write_text('[workspace]\nmembers = ["crates/server"]\nresolver = "2"\n')
                (root / "crates/server/Cargo.toml").write_text(
                    '[package]\nname = "frona-server"\nversion = "0.1.0"\nedition = "2021"\n'
                )
                source = root / "crates/server/src/main.rs"
                source.write_text("fn main() {}\n")
                subprocess.run([CARGO, "generate-lockfile", "--offline"], cwd=root, check=True, capture_output=True)
                shutil.copyfile(ROOT / "bacon.toml", root / "bacon.toml")
                shutil.copytree(ROOT / "build/dev", root / "build/dev")
                bin_dir = root / "bin"
                bin_dir.mkdir()
                log = root / "events.jsonl"
                # Cargo metadata is real. Build/run are a fixture with a long-lived
                # descendant, so restart checks do not compile or launch Frona.
                cargo = bin_dir / "cargo"
                cargo.write_text(f'''#!{sys.executable}
import json, os, pathlib, signal, subprocess, sys, time
root = pathlib.Path.cwd()
def event(kind):
    with open(root / "events.jsonl", "a") as out:
        out.write(json.dumps({{"kind": kind, "pid": os.getpid()}}) + "\\n")
if sys.argv[1] == "metadata":
    os.execv({CARGO!r}, [{CARGO!r}, *sys.argv[1:]])
if sys.argv[1] == "build":
    event("build-mcp")
    sys.exit(0)
if (root / "fail-build").exists():
    event("build-failed")
    print("error: fixture compilation failed", file=sys.stderr)
    sys.exit(1)
if sys.argv[1] == "child":
    def stop(*args):
        event("child-stopped")
        sys.exit(0)
    signal.signal(signal.SIGTERM, stop)
    event("child-started")
    while True: time.sleep(0.05)
child = subprocess.Popen([sys.executable, __file__, "child"])
def stop(*args):
    child.wait(timeout=5)
    event("server-stopped")
    sys.exit(0)
signal.signal(signal.SIGTERM, stop)
event("server-started")
while True: time.sleep(0.05)
''')
                cargo.chmod(0o755)
                # The container job must retain its MCP copy step, without writing /app.
                copy = bin_dir / "cp"
                copy.write_text(f'''#!{sys.executable}
import json, sys
assert sys.argv[1:] == ["target/debug/mcpctl", "/app/bin/mcpctl"]
with open("events.jsonl", "a") as out:
    out.write(json.dumps({{"kind": "copy-mcp"}}) + "\\n")
''')
                copy.chmod(0o755)
                npm = bin_dir / "npm"
                npm.write_text(f'''#!{sys.executable}
import signal, sys, time
if sys.argv[1] == "install": sys.exit(0)
signal.signal(signal.SIGTERM, lambda *args: sys.exit(0))
while True: time.sleep(0.05)
''')
                npm.chmod(0o755)
                env = dict(os.environ, PATH=f"{bin_dir}:{os.environ['PATH']}")
                # Do not inherit compiler cache, personal Bacon config, or Cargo overrides.
                for key in ("RUSTC_WRAPPER", "BACON_CONFIG", "BACON_PREFS", "CARGO"):
                    env.pop(key, None)
                env["BACON_PREFS"] = str(root / "empty-prefs.toml")
                (root / "empty-prefs.toml").write_text("")

                def events():
                    return [json.loads(line) for line in log.read_text().splitlines()] if log.exists() else []

                def count(kind):
                    return sum(e["kind"] == kind for e in events())

                def wait_for(predicate):
                    deadline = time.monotonic() + 15
                    while time.monotonic() < deadline:
                        if predicate():
                            return
                        time.sleep(0.05)
                    self.fail(f"Timed out; events: {events()}; output: {output.read_text()}")

                output = root / "bacon.log"
                command = ["bash", "build/dev/start.sh"]
                with output.open("w") as out:
                    process = subprocess.Popen(
                        command, cwd=root, env=env,
                        stdout=out, stderr=out, start_new_session=True,
                    )
                    try:
                        wait_for(lambda: count("child-started") == 1)
                        (root / "web/page.tsx").write_text("frontend edit\n")
                        time.sleep(0.4)
                        self.assertEqual(count("server-stopped"), 0)
                        # Include non-Rust crate resources and both root Cargo files.
                        for index, path in enumerate((source, root / "crates/server/data.txt", root / "Cargo.lock", root / "Cargo.toml"), 2):
                            with path.open("a") as changed:
                                changed.write("\n")
                            wait_for(lambda: count("child-started") == index)
                            self.assertEqual(count("child-stopped"), index - 1)
                            self.assertEqual(count("server-stopped"), index - 1)
                        (root / "fail-build").touch()
                        source.write_text("// failed build\n")
                        wait_for(lambda: count("build-failed") == 1)
                        self.assertEqual(count("server-started"), 5)
                        self.assertEqual(count("server-stopped"), 5)
                        (root / "fail-build").unlink()
                        source.write_text("fn main() {}\n")
                        wait_for(lambda: count("child-started") == 6)
                        process.send_signal(stop_signal)
                        process.wait(timeout=15)
                        self.assertEqual(count("child-stopped"), 6)
                        self.assertEqual(count("server-stopped"), 6)
                        self.assertEqual(count("build-mcp"), 7)
                        self.assertEqual(count("copy-mcp"), 7)
                    finally:
                        if process.poll() is None:
                            process.kill()
                            process.wait()
                        # Clean up only PIDs recorded by this isolated fixture if it fails.
                        stopped = {e.get("pid") for e in events() if e["kind"].endswith("stopped")}
                        for event in events():
                            if event["kind"].endswith("started") and event["pid"] not in stopped:
                                try:
                                    os.kill(event["pid"], signal.SIGKILL)
                                except ProcessLookupError:
                                    pass


if __name__ == "__main__":
    unittest.main()
