#!/usr/bin/env python3
"""Publish complete package artifacts as a GitHub release after all builds pass."""
import argparse
import hashlib
import json
import pathlib
import re
import shutil
import subprocess
import tempfile


def collect_assets(directory, tag):
    if not re.fullmatch(r"v(?:0|[1-9][0-9]*)\.(?:0|[1-9][0-9]*)\.(?:0|[1-9][0-9]*)", tag):
        raise ValueError("Expected a stable vX.Y.Z tag")
    version = re.escape(tag[1:])
    patterns = {
        "deb": rf"p4ui_{version}-[^_]+_[^.]+\.deb",
        "rpm": rf"p4ui-{version}-.+\.(?:x86_64|aarch64)\.rpm",
        "srpm": rf"p4ui-{version}-.+\.src\.rpm",
        "arch": rf"p4ui-{version}-.+-(?:x86_64|aarch64)\.pkg\.tar\.zst",
        "source": rf"p4ui-{version}\.tar\.gz",
        "arch_debug": rf"p4ui-debug-{version}-.+-(?:x86_64|aarch64)\.pkg\.tar\.zst",
    }
    found, names = {}, set()
    for path in sorted(directory.rglob("*")):
        if not path.is_file():
            continue
        if path.is_symlink():
            raise ValueError(f"Release asset must be a regular file: {path}")
        kind = next((kind for kind, pattern in patterns.items() if re.fullmatch(pattern, path.name)), None)
        if kind is None:
            raise ValueError(f"Unexpected asset or mismatched package version: {path.name}")
        if path.name in names:
            raise ValueError(f"Duplicate release asset name: {path.name}")
        names.add(path.name)
        found.setdefault(kind, []).append(path)
    missing = {"deb", "rpm", "srpm", "arch", "source"} - set(found)
    if missing:
        raise ValueError("Missing release artifacts: " + ", ".join(sorted(missing)))
    return sorted((path for paths in found.values() for path in paths), key=lambda p: p.name)


def gh(*args, check=True):
    result = subprocess.run(["gh", *args], text=True, capture_output=True)
    if check and result.returncode:
        raise RuntimeError(result.stderr.strip() or "GitHub CLI request failed")
    return result


def verify_published_release(tag, directory, repo):
    """Verify original published bytes; rebuilding can change package timestamps."""
    directory.mkdir()
    gh("release", "download", tag, "--repo", repo, "--dir", str(directory))
    checksum_file = directory / "SHA256SUMS"
    if not checksum_file.is_file():
        raise ValueError("Published release is incomplete: SHA256SUMS is missing; assets left unchanged")
    lines = checksum_file.read_text().splitlines()
    checksum_file.unlink()  # Only the temporary local copy; collect_assets checks packages below.
    assets = collect_assets(directory, tag)
    expected = {}
    for line in lines:
        match = re.fullmatch(r"([0-9a-f]{64})  ([^\r\n]+)", line)
        if match is None or match[2] in expected:
            raise ValueError("Published SHA256SUMS is malformed or has duplicate entries; assets left unchanged")
        expected[match[2]] = match[1]
    if set(expected) != {path.name for path in assets}:
        raise ValueError("Published package assets and SHA256SUMS do not match; assets left unchanged")
    for path in assets:
        with path.open("rb") as data:
            digest = hashlib.file_digest(data, "sha256").hexdigest()
        if digest != expected[path.name]:
            raise ValueError(f"Published checksum failed for {path.name}; assets left unchanged")


def publish(tag, commit, directory, repo, dry_run=False):
    if not re.fullmatch(r"[0-9a-f]{40}", commit):
        raise ValueError("Expected the validated full Git commit SHA")
    if not re.fullmatch(r"[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+", repo):
        raise ValueError("Expected OWNER/REPO")
    assets = collect_assets(directory, tag)
    with tempfile.TemporaryDirectory(prefix="p4ui-release-") as temp:
        stage = pathlib.Path(temp)
        for asset in assets:
            shutil.copy2(asset, stage / asset.name)
        staged = sorted(stage.iterdir())
        checksums = stage / "SHA256SUMS"
        with checksums.open("w") as output:
            for path in staged:
                with path.open("rb") as data:
                    digest = hashlib.file_digest(data, "sha256").hexdigest()
                output.write(f"{digest}  {path.name}\n")
        if dry_run:
            print(f"Would publish {tag} at {commit} to {repo}: " + ", ".join(p.name for p in staged) + ", SHA256SUMS")
            return
        remote = gh("api", f"repos/{repo}/commits/{tag}", "--jq", ".sha").stdout.strip()
        if remote != commit:
            raise ValueError("The remote release tag no longer matches the built commit")
        existing = gh("release", "view", tag, "--repo", repo, "--json", "isDraft,url", check=False)
        if existing.returncode == 0 and not json.loads(existing.stdout)["isDraft"]:
            verify_published_release(tag, stage / "published", repo)
            print(f"Release {tag} is already published and verified complete; leaving published assets unchanged")
            return
        if existing.returncode:
            gh("release", "create", tag, "--repo", repo, "--verify-tag", "--target", commit,
               "--draft", "--title", f"p4ui {tag}", "--generate-notes")
        # A failed upload leaves a draft; a retry can replace draft assets safely.
        gh("release", "upload", tag, *map(str, [*staged, checksums]), "--repo", repo, "--clobber")
        gh("release", "edit", tag, "--repo", repo, "--verify-tag", "--draft=false")
        print(f"Published https://github.com/{repo}/releases/tag/{tag}")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("tag")
    parser.add_argument("commit")
    parser.add_argument("artifacts", type=pathlib.Path)
    parser.add_argument("--repo", required=True)
    parser.add_argument("--dry-run", action="store_true", help="Validate files and compute checksums without contacting GitHub")
    args = parser.parse_args()
    publish(args.tag, args.commit, args.artifacts, args.repo, args.dry_run)


if __name__ == "__main__":
    main()
