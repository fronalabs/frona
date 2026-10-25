#!/usr/bin/env python3
"""Exercise optional registry caching through the real launcher with a fake runtime."""
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import unittest

LAUNCHER = Path(__file__).resolve().parents[1] / "container.sh"


class RegistryCacheTests(unittest.TestCase):
    def launch(self, args, cache, runtime="podman", image_status=1):
        with tempfile.TemporaryDirectory(prefix="frona-cache-test-") as directory:
            root = Path(directory)
            (root / "build").mkdir()
            shutil.copyfile(LAUNCHER, root / "build/container.sh")
            binary = root / runtime
            binary.write_text(
                f"#!{sys.executable}\n"
                "import json, os, sys\n"
                "with open(os.environ['TEST_CALLS'], 'a') as log:\n"
                "    log.write(json.dumps(sys.argv[1:]) + '\\n')\n"
                "if sys.argv[1:3] == ['image', 'exists']:\n"
                "    sys.exit(int(os.environ['TEST_IMAGE_STATUS']))\n"
            )
            binary.chmod(0o700)
            calls = root / "calls.jsonl"
            env = dict(os.environ, PATH=f"{root}:{os.environ['PATH']}",
                       CONTAINER_RUNTIME=runtime, KACHE_VERSION="1.2.2",
                       KACHE_SHARED_DIR=str(root / "shared"),
                       KACHE_LOCAL_DIR=str(root / "local"), CONTAINER_BUILD_JOBS="2",
                       TEST_CALLS=str(calls), TEST_IMAGE_STATUS=str(image_status))
            env.pop("CONTAINER_CACHE_REGISTRY", None)
            if cache is not None:
                env["CONTAINER_CACHE_REGISTRY"] = cache
            result = subprocess.run(["bash", str(root / "build/container.sh"), *args],
                                    env=env, capture_output=True, text=True, timeout=10)
            self.assertEqual(result.returncode, 0, result.stderr)
            return [json.loads(line) for line in calls.read_text().splitlines()]

    def test_optional_cache_for_explicit_and_automatic_builds(self):
        for profile in ("dev", "prod"):
            for action in (["build"], ["--build", "-d"], ["-d"]):
                for cache in (None, "", "127.0.0.1:5050/build-cache"):
                    with self.subTest(profile=profile, action=action, cache=cache):
                        calls = self.launch([profile, *action], cache)
                        builds = [call for call in calls if call[0] == "build"]
                        self.assertEqual(len(builds), 1, calls)
                        build = builds[0]
                        self.assertIn("--layers", build)
                        self.assertEqual(build[-1], ".")
                        for option in ("--cache-from", "--cache-to"):
                            if cache:
                                self.assertEqual(build.count(option), 1)
                                self.assertEqual(build[build.index(option) + 1], cache)
                            else:
                                self.assertNotIn(option, build)
                        for call in calls:
                            if call[0] != "build":
                                self.assertNotIn("--cache-from", call)
                                self.assertNotIn("--cache-to", call)

    def test_nonbuilding_commands_and_existing_images_do_not_contact_cache(self):
        for args, status in ((["dev", "-d"], 0),
                             (["dev", "--no-build", "-d"], 1),
                             (["dev", "config"], 1),
                             (["dev", "--help"], 1)):
            with self.subTest(args=args):
                calls = self.launch(args, "127.0.0.1:5050/build-cache", image_status=status)
                self.assertFalse(any(call[0] == "build" for call in calls), calls)
                self.assertFalse(any("--cache-from" in call or "--cache-to" in call for call in calls))

    def test_docker_compose_is_unchanged(self):
        calls = self.launch(["dev", "build"], "127.0.0.1:5050/build-cache", runtime="docker")
        self.assertEqual(len(calls), 1)
        self.assertEqual(calls[0][0], "compose")
        self.assertNotIn("--cache-from", calls[0])
        self.assertNotIn("--cache-to", calls[0])


if __name__ == "__main__":
    unittest.main()
