#!/usr/bin/env python3
"""Collect license declarations and supplied license/notice texts from vendored crates."""
import pathlib
import sys
import tomllib


def main():
    vendor = pathlib.Path(sys.argv[1])
    print("p4ui — third-party dependency notices\n")
    print("The following crates are vendored from the locked Cargo dependency graph.")
    print("This includes optional dependencies for other target platforms. License")
    print("declarations and texts below are copied from their upstream sources.\n")
    for crate in sorted(vendor.iterdir()):
        manifest = crate / "Cargo.toml"
        if not manifest.is_file():
            continue
        package = tomllib.loads(manifest.read_text())["package"]
        print("=" * 72)
        print(f"{package['name']} {package['version']}")
        print(f"License: {package.get('license', 'See license file supplied by upstream')}")
        print(f"Source: {package.get('repository', package.get('homepage', 'https://crates.io/crates/' + package['name']))}\n")
        files = [p for p in crate.iterdir() if p.is_file() and p.name.upper().startswith(
            ("LICENSE", "LICENCE", "COPYING", "NOTICE", "UNLICENSE", "COPYRIGHT"))]
        if package.get("license-file"):
            custom = crate / package["license-file"]
            if custom.is_file() and custom not in files:
                files.append(custom)
        for path in sorted(files):
            print(f"--- {path.name} ---")
            print(path.read_text(errors="replace"))
        if not files:
            print("No separate license text supplied in the crate root; see the upstream source.")
        print()


if __name__ == "__main__":
    main()
