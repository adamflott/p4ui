#!/usr/bin/env python3
"""Checks for source checksum enforcement and bundled license notices (Python 3.11+)."""
import hashlib
import pathlib
import subprocess
import sys
import tempfile
import unittest

ROOT = pathlib.Path(__file__).resolve().parents[1]


class PackagingTests(unittest.TestCase):
    def test_arch_checksum_tracks_archive_and_refuses_overwrite(self):
        with tempfile.TemporaryDirectory() as temp:
            directory = pathlib.Path(temp)
            source = directory / "p4ui-0.1.0.tar.gz"
            source.write_bytes(b"a source archive")
            destination = directory / "arch"
            command = [sys.executable, str(ROOT / "scripts/prepare-arch.py"), str(source), str(destination)]
            subprocess.run(command, check=True, capture_output=True)
            pkgbuild = (destination / "PKGBUILD").read_text()
            self.assertIn(hashlib.sha256(source.read_bytes()).hexdigest(), pkgbuild)
            self.assertNotIn("SKIP", pkgbuild)
            self.assertNotIn("@VERSION@", pkgbuild)
            self.assertNotEqual(subprocess.run(command, capture_output=True).returncode, 0)
            # Reject paths that would inject a package version into the shell recipe.
            invalid = directory / "p4ui-0.1.0;bad.tar.gz"
            invalid.write_bytes(b"invalid")
            result = subprocess.run([*command[:2], str(invalid), str(directory / "invalid")], capture_output=True)
            self.assertNotEqual(result.returncode, 0)
            self.assertFalse((directory / "invalid").exists())

    def test_notices_preserve_upstream_license_and_notice(self):
        with tempfile.TemporaryDirectory() as temp:
            crate = pathlib.Path(temp) / "example-1.0.0"
            crate.mkdir()
            (crate / "Cargo.toml").write_text('[package]\nname="example"\nversion="1.0.0"\nlicense="MIT OR Apache-2.0"\n')
            (crate / "LICENSE-MIT").write_text("Copyright example author\nMIT terms preserved\n")
            (crate / "NOTICE").write_text("Upstream attribution\n")
            result = subprocess.run([sys.executable, str(ROOT / "scripts/third-party-notices.py"), temp],
                                    text=True, capture_output=True, check=True)
            for expected in ("example 1.0.0", "MIT OR Apache-2.0", "Copyright example author",
                             "MIT terms preserved", "Upstream attribution"):
                self.assertIn(expected, result.stdout)


if __name__ == "__main__":
    unittest.main()
