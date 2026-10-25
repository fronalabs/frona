#!/usr/bin/env python3
"""Check workspace version forwarding and verified installation without containers."""

import hashlib
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tarfile
import tempfile
import unittest


BUILD_DIR = Path(__file__).resolve().parents[1]


class KacheVersionTests(unittest.TestCase):
    def test_launcher_selects_version_only_when_building(self):
        cases = [
            # profile, args, image exists, workspace version, override, expected version, exit
            ("dev", ["build"], True, "1.2.2", None, "1.2.2", 0),
            ("dev", ["--build", "-d"], True, "1.3.0", None, "1.3.0", 0),
            ("dev", ["-d"], False, "1.3.0", None, "1.3.0", 0),
            ("dev", ["build"], True, None, "1.2.2", "1.2.2", 0),
            ("dev", ["build"], True, None, None, None, 1),
            ("dev", ["build"], True, "garbage", None, None, 1),
            ("dev", ["build"], True, "1.2.2", "../../bad", None, 1),
            ("dev", ["--no-build", "-d"], True, None, None, None, 0),
            ("dev", ["down"], True, None, None, None, 0),
            ("dev", ["config"], True, None, None, None, 0),
            ("dev", ["--help"], True, None, None, None, 0),
            ("prod", ["build"], True, None, None, None, 0),
        ]
        for runtime in ("podman", "docker"):
            runtime_cases = cases + ([
                ("dev", ["-d"], True, None, None, None, 0),
            ] if runtime == "podman" else [])
            for profile, args, exists, version, override, expected, status in runtime_cases:
                with self.subTest(runtime=runtime, profile=profile, args=args, version=version, override=override):
                    with tempfile.TemporaryDirectory(prefix="frona-kache-launcher-") as directory:
                        root = Path(directory)
                        (root / "build").mkdir()
                        shutil.copyfile(BUILD_DIR / "container.sh", root / "build/container.sh")
                        bin_dir = root / "bin"
                        bin_dir.mkdir()
                        kache = bin_dir / "kache"
                        kache.write_text(
                            f"#!{sys.executable}\nimport sys\nfrom pathlib import Path\n"
                            "assert sys.argv[1:] == ['--version']\n"
                            f"Path({str(root / 'queried')!r}).touch()\n"
                            + ("sys.exit(127)\n" if version is None else f"print('kache {version}')\n")
                        )
                        kache.chmod(0o700)
                        executable = bin_dir / runtime
                        executable.write_text(
                            f"#!{sys.executable}\nimport json, os, sys\n"
                            f"with open({str(root / 'calls')!r}, 'a') as log:\n"
                            "    log.write(json.dumps({'args': sys.argv[1:], 'version': os.getenv('KACHE_VERSION')}) + '\\n')\n"
                            f"if sys.argv[1:3] == ['image', 'exists']: sys.exit({0 if exists else 1})\n"
                        )
                        executable.chmod(0o700)
                        env = dict(os.environ, PATH=f"{bin_dir}:{os.environ['PATH']}",
                                   CONTAINER_RUNTIME=runtime, KACHE_SHARED_DIR=str(root / "shared"),
                                   KACHE_LOCAL_DIR=str(root / "local"), CONTAINER_BUILD_JOBS="1")
                        env.pop("KACHE_VERSION", None)
                        if override is not None:
                            env["KACHE_VERSION"] = override
                        result = subprocess.run(
                            ["bash", str(root / "build/container.sh"), profile, *args],
                            env=env, capture_output=True, text=True, timeout=5,
                        )
                        self.assertEqual(result.returncode, status, result.stderr)
                        calls = [json.loads(line) for line in (root / "calls").read_text().splitlines()] if (root / "calls").exists() else []
                        builds = [c for c in calls if c["args"][0] == "build"]
                        if status:
                            self.assertFalse(builds)
                            self.assertFalse(any(c["args"][0] == "compose" for c in calls))
                        elif expected:
                            if runtime == "podman":
                                self.assertEqual(len(builds), 1)
                                self.assertIn(f"KACHE_VERSION={expected}", builds[0]["args"])
                            else:
                                self.assertEqual(calls[-1]["version"], expected)
                        if override is not None or expected is None and not status:
                            self.assertFalse((root / "queried").exists())


class KacheInstallerTests(unittest.TestCase):
    def test_install_verifies_release_checksum(self):
        for architecture in ("x86_64", "aarch64"):
            for corrupt in (False, True):
                with self.subTest(architecture=architecture, corrupt=corrupt), tempfile.TemporaryDirectory(
                    prefix="frona-kache-install-"
                ) as directory:
                    root = Path(directory)
                    bin_dir = root / "bin"
                    bin_dir.mkdir()
                    destination = root / "installed"
                    destination.mkdir()
                    binary = root / "kache"
                    binary.write_text("#!/bin/sh\nprintf 'kache 1.2.2\\n'\n")
                    binary.chmod(0o700)
                    archive = root / f"kache-{architecture}-unknown-linux-musl.tar.gz"
                    with tarfile.open(archive, "w:gz") as tar:
                        tar.add(binary, arcname="kache")
                    checksum = "0" * 64 if corrupt else hashlib.sha256(archive.read_bytes()).hexdigest()
                    Path(str(archive) + ".sha256").write_text(f"{checksum}  {archive.name}\n")
                    curl = bin_dir / "curl"
                    curl.write_text(
                        f"#!{sys.executable}\nimport shutil, sys\nfrom pathlib import Path\n"
                        "url = sys.argv[sys.argv.index('-o') - 1]\n"
                        "assert url.startswith('https://github.com/kunobi-ninja/kache/releases/download/v1.2.2/')\n"
                        f"shutil.copyfile(Path({str(root)!r}) / url.rsplit('/', 1)[1], sys.argv[-1])\n"
                    )
                    curl.chmod(0o700)
                    uname = bin_dir / "uname"
                    uname.write_text(f"#!/bin/sh\nprintf '{architecture}\\n'\n")
                    uname.chmod(0o700)
                    result = subprocess.run(
                        ["sh", str(BUILD_DIR / "dev/install-kache.sh"), str(destination)],
                        env=dict(os.environ, PATH=f"{bin_dir}:{os.environ['PATH']}", KACHE_VERSION="1.2.2"),
                        capture_output=True, text=True, timeout=5,
                    )
                    if corrupt:
                        self.assertNotEqual(result.returncode, 0)
                        self.assertFalse((destination / "kache").exists())
                    else:
                        self.assertEqual(result.returncode, 0, result.stderr)
                        self.assertIn("kache 1.2.2", result.stdout)


if __name__ == "__main__":
    unittest.main()
