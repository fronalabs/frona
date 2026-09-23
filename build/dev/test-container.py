#!/usr/bin/env python3
"""Launcher checks, including optional provider parsing; no containers are started."""

import json
import os
from pathlib import Path
import re
import shutil
import subprocess
import sys
import tempfile
import unittest


BUILD_DIR = Path(__file__).resolve().parents[1]
FORWARDED_KEYS = ("CARGO_BUILD_JOBS", "RUST_TEST_THREADS")


class CargoEnvironmentTests(unittest.TestCase):
    def test_launcher_preserves_environment(self):
        with tempfile.TemporaryDirectory(prefix="frona-launcher-test-") as directory:
            root = Path(directory)
            build = root / "build"
            build.mkdir()
            (build / "dev").mkdir()
            launcher = build / "container.sh"
            shutil.copyfile(BUILD_DIR / "container.sh", launcher)
            bin_dir = root / "bin"
            bin_dir.mkdir()
            for runtime in ("docker", "podman"):
                executable = bin_dir / runtime
                executable.write_text(
                    f"#!{sys.executable}\n"
                    "import json, os, sys\n"
                    "print(json.dumps({'args': sys.argv[1:], 'cwd': os.getcwd(), 'env': "
                    "{key: os.environ.get(key) for key in "
                    f"{FORWARDED_KEYS!r}" + "}}))\n"
                )
                executable.chmod(0o700)

            for runtime in ("docker", "podman"):
                for jobs in (None, "8", "16", "default"):
                    for threads in (None, "16", "32"):
                        with self.subTest(runtime=runtime, jobs=jobs, threads=threads):
                            env = dict(os.environ)
                            env.update(
                                PATH=f"{bin_dir}{os.pathsep}{env.get('PATH', '')}",
                                CONTAINER_RUNTIME=runtime,
                                CONTAINER_BUILD_JOBS="1",
                                KACHE_SHARED_DIR=str(root / "shared-cache"),
                                KACHE_LOCAL_DIR=str(root / "local-cache"),
                            )
                            for key, value in zip(FORWARDED_KEYS, (jobs, threads)):
                                env.pop(key, None)
                                if value is not None:
                                    env[key] = value
                            result = subprocess.run(
                                ["bash", str(launcher), "dev", "config"],
                                cwd=root.parent, env=env, check=True, capture_output=True, text=True,
                                timeout=10,
                            )
                            actual = json.loads(result.stdout)
                            self.assertEqual(actual["env"], dict(zip(FORWARDED_KEYS, (jobs, threads))))
                            self.assertEqual(actual["cwd"], str(root))
                            expected = ["compose", "-f", str(build / "docker-compose.yml")]
                            if runtime == "podman":
                                expected += ["-f", str(build / "dev" / "docker-compose.podman.yml")]
                            self.assertEqual(actual["args"], expected + ["--profile", "dev", "config"])


    @unittest.skipUnless(
        shutil.which("podman-compose") and shutil.which("podman"),
        "podman-compose and podman are not installed",
    )
    def test_installed_provider_opens_both_compose_files(self):
        provider = shutil.which("podman-compose")
        podman = shutil.which("podman")
        with tempfile.TemporaryDirectory(prefix="frona-compose-test-") as directory:
            root = Path(directory)
            build = root / "build"
            build.mkdir()
            (build / "dev").mkdir()
            launcher = build / "container.sh"
            shutil.copyfile(BUILD_DIR / "container.sh", launcher)
            (build / "docker-compose.yml").write_text(
                "services:\n  frona-dev:\n    image: busybox:latest\n"
            )
            shutil.copyfile(
                BUILD_DIR / "dev" / "docker-compose.podman.yml",
                build / "dev" / "docker-compose.podman.yml",
            )
            bin_dir = root / "bin"
            bin_dir.mkdir()
            runtime = bin_dir / "podman"
            runtime.write_text(
                f"#!{sys.executable}\n"
                "import os, sys\n"
                f"if sys.argv[1] != 'compose': os.execv({podman!r}, [{podman!r}, *sys.argv[1:]])\n"
                f"os.execv({provider!r}, [{provider!r}, *sys.argv[2:]])\n"
            )
            runtime.chmod(0o700)
            env = dict(os.environ)
            for key in tuple(env):
                if key.startswith("COMPOSE_"):
                    env.pop(key)
            env.update(
                PATH=f"{bin_dir}{os.pathsep}{env.get('PATH', '')}",
                CONTAINER_RUNTIME="podman",
                CONTAINER_BUILD_JOBS="1",
                KACHE_SHARED_DIR=str(root / "shared-cache"),
                KACHE_LOCAL_DIR=str(root / "local-cache"),
            )
            result = subprocess.run(
                ["bash", str(launcher), "dev", "config"],
                cwd=root.parent, env=env, capture_output=True, text=True, timeout=10,
            )
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertIn("frona-dev:", result.stdout)
            self.assertIn("keep-id", result.stdout)

    def test_compose_uses_bare_environment_keys(self):
        # This checks our declaration, not the external Compose implementation.
        source = (BUILD_DIR / "docker-compose.yml").read_text()
        service = re.search(r"^  frona-dev:\n(.*?)(?=^  \S|\Z)", source, re.M | re.S)
        self.assertIsNotNone(service)
        for key in FORWARDED_KEYS:
            entries = re.findall(rf"^\s+-\s+({key}\b[^\n]*)", service.group(1), re.M)
            self.assertEqual(entries, [key])


class ParallelBuildTests(unittest.TestCase):
    def test_up_builds_missing_images_with_parallel_jobs(self):
        cases = [
            # args, image probe exit status, build exit status, expected builds, exit
            ([], 1, 0, 1, 0),
            ([], 0, 0, 0, 0),
            (["--build"], 0, 0, 1, 0),
            (["--no-build"], 1, 0, 0, 0),
            (["--help"], 1, 0, 0, 0),
            ([], 1, 9, 1, 9),
            ([], 125, 0, 0, 125),
        ]
        with tempfile.TemporaryDirectory(prefix="frona-parallel-test-") as directory:
            root = Path(directory)
            build = root / "build"
            build.mkdir()
            launcher = build / "container.sh"
            shutil.copyfile(BUILD_DIR / "container.sh", launcher)
            bin_dir = root / "bin"
            bin_dir.mkdir()
            runtime = bin_dir / "podman"
            runtime.write_text(
                f"#!{sys.executable}\n"
                "import json, os, sys\n"
                "args = sys.argv[1:]\n"
                "with open(os.environ['FRONA_TEST_CALLS'], 'a') as log:\n"
                "    log.write(json.dumps(args) + '\\n')\n"
                "if args[:2] == ['image', 'exists']:\n"
                "    sys.exit(int(os.environ['FRONA_TEST_IMAGE_STATUS']))\n"
                "if args[0] == 'build':\n"
                "    sys.exit(int(os.environ['FRONA_TEST_BUILD_STATUS']))\n"
            )
            runtime.chmod(0o700)
            for index, (args, image_status, build_status, count, status) in enumerate(cases):
                with self.subTest(args=args, image_status=image_status, build_status=build_status):
                    calls_file = root / f"calls-{index}.jsonl"
                    env = dict(os.environ)
                    env.update(
                        PATH=f"{bin_dir}{os.pathsep}{env.get('PATH', '')}",
                        CONTAINER_RUNTIME="podman", CONTAINER_BUILD_JOBS="64",
                        KACHE_SHARED_DIR=str(root / "shared"),
                        KACHE_LOCAL_DIR=str(root / "local"),
                        FRONA_TEST_CALLS=str(calls_file),
                        FRONA_TEST_IMAGE_STATUS=str(image_status),
                        FRONA_TEST_BUILD_STATUS=str(build_status),
                    )
                    result = subprocess.run(
                        ["bash", str(launcher), "dev", *args], env=env,
                        capture_output=True, text=True, timeout=10,
                    )
                    calls = [json.loads(line) for line in calls_file.read_text().splitlines()]
                    builds = [call for call in calls if call[0] == "build"]
                    self.assertEqual(len(builds), count, calls)
                    self.assertEqual(result.returncode, status, result.stderr)
                    for call in builds:
                        self.assertEqual(call[call.index("--jobs") + 1], "64")
                        self.assertEqual(call[call.index("--target") + 1], "dev")
                    compose = [call for call in calls if call[0] == "compose"]
                    if status:
                        self.assertEqual(compose, [], calls)
                    elif "--help" not in args:
                        self.assertIn("--no-build", compose[0])
                        self.assertNotIn("--build", compose[0])
                        if builds:
                            self.assertLess(calls.index(builds[0]), calls.index(compose[0]))


class ContainerLifecycleTests(unittest.TestCase):
    def test_foreground_cleanup_and_detached_passthrough(self):
        import signal
        import time

        with tempfile.TemporaryDirectory(prefix="frona-lifecycle-test-") as directory:
            root = Path(directory)
            build = root / "build"
            build.mkdir()
            (build / "dev").mkdir()
            launcher = build / "container.sh"
            shutil.copyfile(BUILD_DIR / "container.sh", launcher)
            bin_dir = root / "bin"
            bin_dir.mkdir()
            runtime = bin_dir / "podman"
            runtime.write_text(
                f"#!{sys.executable}\n"
                "import json, os, signal, sys, time\n"
                "from pathlib import Path\n"
                "args = sys.argv[1:]\n"
                "with open(os.environ['FRONA_TEST_CALLS'], 'a') as log:\n"
                "    log.write(json.dumps(args) + '\\n')\n"
                "if 'up' in args and not any(a in args for a in ('-d', '--detach', '--no-start')):\n"
                "    if os.environ['FRONA_TEST_MODE'] == 'failure': sys.exit(7)\n"
                "    signal.signal(signal.SIGINT, lambda *_: sys.exit(130))\n"
                "    signal.signal(signal.SIGTERM, lambda *_: sys.exit(143))\n"
                "    Path(os.environ['FRONA_TEST_READY']).touch()\n"
                "    while True: time.sleep(.05)\n"
            )
            runtime.chmod(0o700)
            cases = [
                ([], signal.SIGINT, 130),
                ([], signal.SIGTERM, 143),
                ([], None, 7),
                (["-d"], None, 0),
                (["--detach"], None, 0),
                (["--no-start"], None, 0),
            ]
            for index, (args, stop_signal, expected_exit) in enumerate(cases):
                with self.subTest(args=args, signal=stop_signal):
                    calls_file = root / f"calls-{index}.jsonl"
                    ready = root / f"ready-{index}"
                    env = dict(os.environ)
                    env.update(
                        PATH=f"{bin_dir}{os.pathsep}{env.get('PATH', '')}",
                        CONTAINER_RUNTIME="podman",
                        CONTAINER_BUILD_JOBS="1",
                        KACHE_SHARED_DIR=str(root / "shared-cache"),
                        KACHE_LOCAL_DIR=str(root / "local-cache"),
                        FRONA_TEST_CALLS=str(calls_file),
                        FRONA_TEST_READY=str(ready),
                        FRONA_TEST_MODE="failure" if expected_exit == 7 else "running",
                    )
                    process = subprocess.Popen(
                        ["bash", str(launcher), "dev", *args], cwd=root.parent,
                        env=env, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                        text=True, start_new_session=True,
                    )
                    try:
                        if stop_signal is not None:
                            deadline = time.monotonic() + 5
                            while not ready.exists() and time.monotonic() < deadline:
                                if process.poll() is not None:
                                    break
                                time.sleep(.02)
                            self.assertTrue(ready.exists(), "Foreground runtime never started")
                            os.killpg(process.pid, stop_signal)
                        _, stderr = process.communicate(timeout=5)
                        self.assertEqual(process.returncode, expected_exit, stderr)
                        calls = [json.loads(line) for line in calls_file.read_text().splitlines()]
                        actions = [call[call.index("--profile") + 2] for call in calls if call[0] == "compose"]
                        self.assertEqual(actions, ["up"] if args else ["up", "down"])
                        if not args:
                            self.assertEqual(calls[-1][-3:], ["down", "--timeout", "10"])
                            self.assertNotIn("--volumes", calls[-1])
                    finally:
                        if process.poll() is None:
                            os.killpg(process.pid, signal.SIGKILL)
                            process.communicate()


if __name__ == "__main__":
    unittest.main()
