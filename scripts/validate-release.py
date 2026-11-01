#!/usr/bin/env python3
"""Validate package versions at an exact commit before building or publishing."""
import argparse
import os
import pathlib
import re
import subprocess
import tomllib

STABLE_VERSION = r"(?:0|[1-9][0-9]*)\.(?:0|[1-9][0-9]*)\.(?:0|[1-9][0-9]*)"


def release_mode(root, tag):
    if not tag:
        return "auto"
    ref = f"refs/tags/{tag}"
    kind = subprocess.check_output(["git", "cat-file", "-t", ref], cwd=root, text=True).strip()
    if kind != "tag":
        return "auto"
    message = subprocess.check_output(["git", "for-each-ref", "--format=%(contents)", ref], cwd=root, text=True)
    markers = [line for line in message.splitlines() if line.startswith("p4ui-release-mode:")]
    if markers and markers != ["p4ui-release-mode: managed"]:
        raise ValueError("Invalid or duplicate release mode marker")
    return "managed" if markers else "auto"


def validate_versions(files, tag=""):
    version = tomllib.loads(files["Cargo.toml"])["package"]["version"]
    if not re.fullmatch(STABLE_VERSION, version):
        raise ValueError("Release packaging requires a stable X.Y.Z Cargo version")
    if tag and tag != f"v{version}":
        raise ValueError(f"Tag {tag} does not match Cargo version {version}")
    locked = [p["version"] for p in tomllib.loads(files["Cargo.lock"])["package"] if p["name"] == "p4ui"]
    changelog = re.match(r"p4ui \(([^)]+)\)", files["debian/changelog"])
    rpm = re.search(r"^Version:\s*(\S+)\s*$", files["packaging/rpm/p4ui.spec"], re.MULTILINE)
    if locked != [version]:
        raise ValueError("Cargo.lock p4ui version must match Cargo.toml")
    if changelog is None or changelog[1].rsplit("-", 1)[0] != version:
        raise ValueError("Debian changelog version must match Cargo.toml")
    if rpm is None or rpm[1] != version:
        raise ValueError("RPM spec Version must match Cargo.toml")
    return version


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--tag", default="", help="Existing vX.Y.Z tag; empty means build HEAD only")
    args = parser.parse_args()
    if args.tag and not re.fullmatch("v" + STABLE_VERSION, args.tag):
        parser.error("release tag must be vX.Y.Z (prerelease tags are not supported yet)")
    root = pathlib.Path(__file__).resolve().parents[1]
    ref = f"refs/tags/{args.tag}" if args.tag else "HEAD"
    commit = subprocess.check_output(["git", "rev-parse", "--verify", "--end-of-options", ref + "^{commit}"], cwd=root, text=True).strip()
    files = {name: subprocess.check_output(["git", "show", f"{commit}:{name}"], cwd=root, text=True)
             for name in ("Cargo.toml", "Cargo.lock", "debian/changelog", "packaging/rpm/p4ui.spec")}
    version = validate_versions(files, args.tag)
    mode = release_mode(root, args.tag)
    if os.environ.get("GITHUB_OUTPUT"):
        with open(os.environ["GITHUB_OUTPUT"], "a") as output:
            output.write(f"source_sha={commit}\ntag={args.tag}\nversion={version}\nrelease_mode={mode}\n")
    print(f"Validated p4ui {version} at {commit}" + (f" for tag {args.tag}" if args.tag else " (build only)"))


if __name__ == "__main__":
    main()
