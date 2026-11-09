#!/usr/bin/env python3
"""Run repeatable harness SDK, server compatibility, and package gates.

Use --stage to rerun a failed gate without repeating already-passed suites.
No model credentials, publication, or external services are required.
"""
import argparse
import os
from pathlib import Path
import subprocess
import sys


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--offline", action="store_true")
    parser.add_argument("--stage", choices=("all", "sdk", "server", "package"), default="all")
    args = parser.parse_args()
    root = Path(__file__).resolve().parents[2]
    harness = root / "crates/frona-harness"
    env = os.environ.copy()
    env.setdefault("CARGO_TARGET_DIR", str(root / "target"))

    def cargo(*arguments):
        command = ["cargo", arguments[0], *(["--offline"] if args.offline else []), *arguments[1:]]
        print(" ".join(command), flush=True)
        subprocess.run(command, cwd=root, env=env, check=True)

    if args.stage in ("all", "sdk"):
        subprocess.run(["cargo", "fmt", "--all", "--", "--check"], cwd=root, env=env, check=True)
        # No name filter: root unit tests and every integration target must run.
        for features in ([], ["--features", "jsonl"]):
            cargo("test", "-p", "frona-derive", "-p", "frona-harness", "--locked", *features)
            cargo("test", "--manifest-path", str(harness / "tests/fixtures/standalone/Cargo.toml"), "--locked", *features)
        cargo("test", "--manifest-path", str(harness / "tests/fixtures/annotated-tools/Cargo.toml"), "--locked")
        cargo("clippy", "-p", "frona-harness", "--no-default-features", "--features", "jsonl", "--examples", "--", "-D", "warnings")
    if args.stage in ("all", "server"):
        cargo("test", "-p", "frona-server", "--lib", "memory::pkm::consolidation")
        suites = ("pkm_sync_e2e", "pkm_recovery_e2e", "pkm_reset_e2e", "pkm_service_e2e",
                  "pkm_consolidation_sweep_e2e", "structured_conversation_repair", "structured_inference_tools",
                  "chat_compaction", "conversation_builder", "persistent_tool_dispatch", "task_executor",
                  "runtime_lifecycle", "native_persistence", "schema_compatibility", "harness_execution",
                  "native_file_tools", "inference_storage_seams", "send_message_gating_e2e", "agent_context", "inference_test")
        cargo("test", "-p", "frona-server", *[part for suite in suites for part in ("--test", suite)])
        cargo("clippy", "--workspace", "--", "-D", "warnings")
    if args.stage in ("all", "package"):
        subprocess.run([sys.executable, str(harness / "verify_package.py"), *(["--offline"] if args.offline else [])], cwd=root, env=env, check=True)
    print(f"Harness verification passed: {args.stage}", flush=True)


if __name__ == "__main__":
    main()
