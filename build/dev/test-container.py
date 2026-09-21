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


if __name__ == "__main__":
    unittest.main()
