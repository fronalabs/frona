#!/usr/bin/env python3
"""Verify extracted archives and renamed consumers, without publishing anything.

Run: python3 crates/frona-harness/verify_package.py --offline
The three unpublished Frona support archives receive explicit test-local overrides.
No dependency is resolved from the checkout after archive extraction.
"""
import argparse
import json
import os
from pathlib import Path
import shutil
import subprocess
import tarfile
import tempfile
import tomllib

PACKAGES = ("frona-derive", "frona-text", "frona-model-catalog", "frona-harness")
RECIPES = ("basic", "tools", "refinement", "manual", "budgets",
           "workspace", "progress", "jsonl")


def feature_flags(features):
    return ["--no-default-features", *(["--features", features] if features else [])]


def verify_graph(cargo, consumer, temp, host, features):
    flags = feature_flags(features)
    graph = json.loads(cargo(["metadata", "--format-version", "1", "--filter-platform", host, *flags], consumer, True).stdout)
    for package in graph["packages"]:
        assert not (package.get("source") or "").startswith("git+"), package["name"]
        assert package["name"] not in {"frona-server", "presage", "frona-browser"}, package["name"]
        assert not package["name"].startswith("surrealdb"), package["name"]
        if package.get("source") is None:
            assert Path(package["manifest_path"]).resolve().is_relative_to(temp), package["manifest_path"]
    tree = cargo(["tree", "--edges", "normal,build", "--prefix", "none", *flags], consumer, True).stdout
    names = {line.split()[0] for line in tree.splitlines() if line.strip()}
    assert not any(name.startswith("surrealdb") for name in names), tree
    print(f"Verified isolated normal/build graph: {features or 'default'}", flush=True)



def verify_fresh_resolution(cargo, consumer, temp, host):
    """Resolve a new consumer independently of the archive's supplied lockfile."""
    fresh = temp / "fresh-consumer"
    shutil.copytree(consumer, fresh, ignore=shutil.ignore_patterns("Cargo.lock"))
    verify_graph(cargo, fresh, temp, host, "")
    cargo(["check", "--bin", "basic", *feature_flags("")], fresh)
    graph = json.loads(cargo(["metadata", "--format-version", "1", "--filter-platform", host, *feature_flags("")], fresh, True).stdout)
    tokio_versions = [p["version"] for p in graph["packages"] if p["name"] == "tokio"]
    print(f"Fresh lockfile resolution and default build passed (Tokio {', '.join(tokio_versions)}).", flush=True)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--offline", action="store_true")
    args = parser.parse_args()
    root = Path(__file__).resolve().parents[2]
    version = tomllib.loads((root / "Cargo.toml").read_text())["workspace"]["package"]["version"]
    flags = ["--offline"] if args.offline else []
    env = os.environ.copy()
    env.setdefault("CARGO_TARGET_DIR", str(root / "target"))
    # Match the checkout artifact profile without loading its Cargo configuration.
    env.setdefault("CARGO_PROFILE_DEV_DEBUG", "line-tables-only")
    env.setdefault("CARGO_PROFILE_TEST_DEBUG", "line-tables-only")

    def cargo(arguments, cwd, capture=False):
        print("cargo " + " ".join(arguments), flush=True)
        return subprocess.run(["cargo", arguments[0], *flags, *arguments[1:]], cwd=cwd, env=env,
                              check=True, text=True, stdout=subprocess.PIPE if capture else None)

    print("Support packages are unpublished prerequisites; using only their extracted archives as explicit test overrides.", flush=True)
    with tempfile.TemporaryDirectory(prefix="frona-package-") as directory:
        temp = Path(directory).resolve()
        selection = [part for name in PACKAGES for part in ("-p", name)]
        cargo(["package", "--allow-dirty", "--no-verify", "--target-dir", str(temp / "artifacts"), *selection], root)
        for name in PACKAGES:
            with tarfile.open(temp / "artifacts" / "package" / f"{name}-{version}.crate") as archive:
                archive.extractall(temp, filter="data")
        packaged = temp / f"frona-harness-{version}"
        for name in PACKAGES:
            assert (temp / f"{name}-{version}" / "LICENSE").is_file(), name
        assert (temp / f"frona-model-catalog-{version}" / "resources/modelparams-schema.json").is_file()
        assert not (packaged / "resources").exists(), "application resources leaked into SDK"
        assert not (packaged / "build.rs").exists(), "application build script leaked into SDK"
        assert not (packaged / "src/sdk").exists(), "SDK compatibility namespace leaked into archive"
        package_manifest = tomllib.loads((packaged / "Cargo.toml").read_text())
        assert set(package_manifest["features"]) == {"default", "jsonl"}, package_manifest["features"]
        for name in PACKAGES[:-1]:
            dependency = package_manifest["dependencies"][name]
            assert "version" in dependency and "path" not in dependency, dependency

        consumer = temp / "consumer"
        (consumer / "tests/helpers").mkdir(parents=True)
        (consumer / "src/bin/support").mkdir(parents=True)
        manifest = '''[package]
name = "frona-packaged-consumer"
version = "0.0.0"
edition = "2024"
[workspace]
[lib]
name = "frona_standalone_consumer"
[features]
default = []
jsonl = ["sdk/jsonl"]
[dependencies]
sdk = { package = "frona-harness", path = "../HARNESS" }
frona-derive = "=VERSION"
tokio = { version = "1", features = ["full"] }
tokio-util = { version = "0.7", features = ["rt"] }
serde = { version = "1", features = ["derive"] }
serde_json = "1"
schemars = "1"
rig-core = "0.46"
async-trait = "0.1"
tempfile = "3"
'''.replace("HARNESS", packaged.name).replace("VERSION", version)
        for recipe in RECIPES:
            manifest += f'\n[[bin]]\nname = "{recipe}"\npath = "src/bin/{recipe}.rs"\n'
            if recipe == "jsonl":
                manifest += 'required-features = ["jsonl"]\n'
        manifest += '\n[patch.crates-io]\n'
        patch_flags = []
        for name in PACKAGES[:-1]:
            path = temp / f"{name}-{version}"
            manifest += f'{name} = {{ path = "../{path.name}" }}\n'
            patch_flags += ["--config", f'patch.crates-io.{name}.path={json.dumps(str(path))}']
        (consumer / "Cargo.toml").write_text(manifest)
        shutil.copy(packaged / "Cargo.lock", consumer / "Cargo.lock")

        def copy_rust(source, target, suffix=""):
            content = (packaged / source).read_text().replace("frona_harness::", "sdk::")
            (consumer / target).write_text(content + suffix)

        for recipe in RECIPES:
            copy_rust(f"examples/{recipe}.rs", f"src/bin/{recipe}.rs")
        copy_rust("examples/support/mod.rs", "src/bin/support/mod.rs")
        copy_rust("tests/portable_dtos.rs", "tests/portable_dtos.rs")
        copy_rust("tests/helpers/ownership.rs", "src/lib.rs")
        copy_rust("tests/ownership.rs", "tests/ownership.rs")
        for helper in ("mod.rs", "execution.rs", "ownership.rs"):
            copy_rust(f"tests/helpers/{helper}", f"tests/helpers/{helper}")
        annotated = (packaged / "tests/annotated_tools.rs").read_text()
        annotated = annotated.replace("frona_harness::", "sdk::").replace("../examples/support/mod.rs", "../src/bin/support/mod.rs")
        (consumer / "tests/annotated.rs").write_text(annotated)
        renamed = (packaged / "tests/renamed.rs").read_text().replace("use frona_harness as sdk;", "")
        (consumer / "tests/renamed.rs").write_text(renamed)
        host = subprocess.check_output(["rustc", "-vV"], text=True).split("host: ")[1].splitlines()[0]
        for features in ("", "jsonl"):
            verify_graph(cargo, consumer, temp, host, features)
            cargo(["test", *feature_flags(features)], consumer)
        for recipe in RECIPES:
            cargo(["run", "--bin", recipe, *feature_flags("jsonl" if recipe == "jsonl" else "")], consumer)
        cargo(["check", "--examples", "--features", "jsonl", *patch_flags], packaged)
        verify_fresh_resolution(cargo, consumer, temp, host)
        print("Package verification passed: default/JSONL without SurrealDB, renamed annotations, portable DTOs, actual ownership witnesses, eight recipes, JSONL recovery, and archive-only support assets.", flush=True)


if __name__ == "__main__":
    main()
