#!/usr/bin/env python3
"""Prepare a local Arch build directory with a real SHA-256, never a SKIP checksum."""
import argparse
import hashlib
import pathlib
import re
import shutil


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("archive", type=pathlib.Path)
    parser.add_argument("destination", type=pathlib.Path)
    args = parser.parse_args()
    archive = args.archive.resolve(strict=True)
    match = re.fullmatch(r"p4ui-(\d+\.\d+\.\d+)\.tar\.gz", archive.name)
    if match is None:
        parser.error("archive must be named p4ui-X.Y.Z.tar.gz")
    template = pathlib.Path(__file__).resolve().parents[1] / "packaging/arch/PKGBUILD.in"
    with archive.open("rb") as source:
        checksum = hashlib.file_digest(source, "sha256").hexdigest()
    text = template.read_text().replace("@VERSION@", match[1]).replace("@SHA256@", checksum)
    if args.destination.exists():
        parser.error("destination already exists; choose a new build directory")
    args.destination.mkdir(parents=True)
    shutil.copy2(archive, args.destination / archive.name)
    (args.destination / "PKGBUILD").write_text(text)
    print(f"Arch build prepared in {args.destination} (SHA-256 {checksum})")


if __name__ == "__main__":
    main()
