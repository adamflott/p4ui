#!/usr/bin/env python3
"""Checks for source checksum enforcement and bundled license notices (Python 3.11+)."""
import hashlib
import importlib.util
import json
import pathlib
import subprocess
import sys
import tempfile
import unittest
from unittest import mock

ROOT = pathlib.Path(__file__).resolve().parents[1]


def load_script(name):
    spec = importlib.util.spec_from_file_location(name, ROOT / f"scripts/{name}.py")
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


validator = load_script("validate-release")
publisher = load_script("publish-release")


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


class ReleaseTests(unittest.TestCase):
    def files(self):
        return {
            "Cargo.toml": '[package]\nname="p4ui"\nversion="0.1.0"\n',
            "Cargo.lock": '[[package]]\nname="p4ui"\nversion="0.1.0"\n',
            "debian/changelog": 'p4ui (0.1.0-1) unstable; urgency=medium\n',
            "packaging/rpm/p4ui.spec": 'Name: p4ui\nVersion: 0.1.0\n',
        }

    def assets(self, directory):
        names = ['p4ui_0.1.0-1_amd64.deb', 'p4ui-0.1.0-1.fc43.x86_64.rpm',
                 'p4ui-0.1.0-1.fc43.src.rpm', 'p4ui-0.1.0-1-x86_64.pkg.tar.zst',
                 'p4ui-0.1.0.tar.gz']
        for name in names:
            (directory / name).write_bytes(name.encode())
        return names

    def test_mismatched_tag_and_package_versions_are_rejected(self):
        self.assertEqual(validator.validate_versions(self.files(), "v0.1.0"), "0.1.0")
        with self.assertRaises(ValueError):
            validator.validate_versions(self.files(), "v0.2.0")
        for name in ('Cargo.lock', 'debian/changelog', 'packaging/rpm/p4ui.spec'):
            files = self.files()
            files[name] = files[name].replace('0.1.0', '0.2.0')
            with self.assertRaises(ValueError, msg=name):
                validator.validate_versions(files, 'v0.1.0')

    def test_incomplete_wrong_version_or_duplicate_assets_block_publication(self):
        with tempfile.TemporaryDirectory() as temp:
            directory = pathlib.Path(temp)
            with self.assertRaises(ValueError):
                publisher.collect_assets(directory, 'v0.1.0')
            names = self.assets(directory)
            self.assertEqual(len(publisher.collect_assets(directory, 'v0.1.0')), 5)
            with self.assertRaises(ValueError):
                publisher.collect_assets(directory, 'v0.2.0')
            nested = directory / 'duplicate'
            nested.mkdir()
            (nested / names[0]).write_bytes(b'duplicate')
            with self.assertRaises(ValueError):
                publisher.collect_assets(directory, 'v0.1.0')

    def test_older_arch_debug_package_is_optional_not_a_replacement_for_main(self):
        with tempfile.TemporaryDirectory() as temp:
            directory = pathlib.Path(temp)
            names = self.assets(directory)
            (directory / 'p4ui-debug-0.1.0-1-x86_64.pkg.tar.zst').write_bytes(b'older tag debug package')
            self.assertEqual(len(publisher.collect_assets(directory, 'v0.1.0')), 6)
            (directory / names[3]).unlink()
            with self.assertRaisesRegex(ValueError, 'arch'):
                publisher.collect_assets(directory, 'v0.1.0')

    def test_failed_upload_leaves_a_draft_and_never_publishes(self):
        with tempfile.TemporaryDirectory() as temp:
            directory = pathlib.Path(temp)
            self.assets(directory)
            def gh(*args, **kwargs):
                if args[0] == 'api':
                    return subprocess.CompletedProcess(args, 0, 'a' * 40, '')
                if args[:2] == ('release', 'view'):
                    return subprocess.CompletedProcess(args, 1, '', 'not found')
                if args[:2] == ('release', 'upload'):
                    raise RuntimeError('upload interrupted')
                return subprocess.CompletedProcess(args, 0, '', '')
            with mock.patch.object(publisher, 'gh', side_effect=gh) as calls:
                with self.assertRaisesRegex(RuntimeError, 'upload interrupted'):
                    publisher.publish('v0.1.0', 'a' * 40, directory, 'owner/repo')
                commands = [call.args for call in calls.call_args_list]
                self.assertTrue(any(args[:2] == ('release', 'create') and '--draft' in args for args in commands))
                self.assertFalse(any(args[:2] == ('release', 'edit') for args in commands))

    def test_success_uploads_checksums_before_publishing(self):
        with tempfile.TemporaryDirectory() as temp:
            directory = pathlib.Path(temp)
            names = self.assets(directory)
            def gh(*args, **kwargs):
                if args[0] == 'api':
                    return subprocess.CompletedProcess(args, 0, 'a' * 40, '')
                if args[:2] == ('release', 'view'):
                    return subprocess.CompletedProcess(args, 0, json.dumps({'isDraft': True}), '')
                if args[:2] == ('release', 'upload'):
                    checksum_file = next(pathlib.Path(arg) for arg in args if arg.endswith('/SHA256SUMS'))
                    checksums = checksum_file.read_text().splitlines()
                    self.assertEqual(len(checksums), len(names))
                    for line in checksums:
                        digest, name = line.split('  ', 1)
                        self.assertEqual(digest, hashlib.sha256((directory / name).read_bytes()).hexdigest())
                return subprocess.CompletedProcess(args, 0, '', '')
            with mock.patch.object(publisher, 'gh', side_effect=gh) as calls:
                publisher.publish('v0.1.0', 'a' * 40, directory, 'owner/repo')
                self.assertEqual(calls.call_args_list[-1].args[:3], ('release', 'edit', 'v0.1.0'))
                self.assertIn('--draft=false', calls.call_args_list[-1].args)

    def published_gh(self, directory, mode="complete"):
        def gh(*args, **kwargs):
            if args[0] == 'api':
                return subprocess.CompletedProcess(args, 0, 'a' * 40, '')
            if args[:2] == ('release', 'view'):
                return subprocess.CompletedProcess(args, 0, json.dumps({'isDraft': False}), '')
            self.assertEqual(args[:2], ('release', 'download'), "Public release must never be mutated")
            destination = pathlib.Path(args[args.index('--dir') + 1])
            manifest = []
            downloaded = []
            for path in sorted(directory.iterdir()):
                if mode == 'missing_package' and path.name.endswith('.pkg.tar.zst'):
                    continue
                # Released packages may differ from a fresh rebuild. Verify the
                # original published checksums, rather than the new build's bytes.
                data = b'original released bytes: ' + path.read_bytes()
                target = destination / path.name
                target.write_bytes(data)
                downloaded.append(target)
                manifest.append(f"{hashlib.sha256(data).hexdigest()}  {path.name}")
            if mode == 'missing_entry':
                manifest.pop()
            elif mode == 'duplicate':
                manifest.append(manifest[0])
            elif mode == 'malformed':
                manifest[0] = 'invalid checksum entry'
            elif mode == 'corrupt':
                downloaded[0].write_bytes(b'corrupt')
            elif mode == 'unexpected':
                (destination / 'unexpected.txt').write_text('unexpected')
            if mode != 'missing_checksums':
                (destination / 'SHA256SUMS').write_text('\n'.join(manifest) + '\n')
            return subprocess.CompletedProcess(args, 0, '', '')
        return gh

    def test_complete_published_release_is_verified_without_replacing_its_bytes(self):
        with tempfile.TemporaryDirectory() as temp:
            directory = pathlib.Path(temp)
            self.assets(directory)
            with mock.patch.object(publisher, 'gh', side_effect=self.published_gh(directory)) as calls:
                publisher.publish('v0.1.0', 'a' * 40, directory, 'owner/repo')
                self.assertEqual(calls.call_count, 3)

    def test_incomplete_or_corrupt_public_release_fails_without_mutation(self):
        for mode in ('missing_checksums', 'missing_package', 'missing_entry', 'duplicate',
                     'malformed', 'corrupt', 'unexpected'):
            with self.subTest(mode=mode), tempfile.TemporaryDirectory() as temp:
                directory = pathlib.Path(temp)
                self.assets(directory)
                with mock.patch.object(publisher, 'gh', side_effect=self.published_gh(directory, mode)) as calls:
                    with self.assertRaises(ValueError):
                        publisher.publish('v0.1.0', 'a' * 40, directory, 'owner/repo')
                    self.assertEqual(calls.call_count, 3)

    def test_moved_tags_are_never_published(self):
        with tempfile.TemporaryDirectory() as temp:
            directory = pathlib.Path(temp)
            self.assets(directory)
            with mock.patch.object(publisher, 'gh', return_value=subprocess.CompletedProcess([], 0, 'b' * 40, '')) as calls:
                with self.assertRaisesRegex(ValueError, 'no longer matches'):
                    publisher.publish('v0.1.0', 'a' * 40, directory, 'owner/repo')
                self.assertEqual(calls.call_count, 1)

    def test_dry_run_never_contacts_github(self):
        with tempfile.TemporaryDirectory() as temp:
            directory = pathlib.Path(temp)
            self.assets(directory)
            with mock.patch.object(publisher, 'gh', side_effect=AssertionError('Unexpected GitHub access')):
                publisher.publish('v0.1.0', 'a' * 40, directory, 'owner/repo', dry_run=True)


if __name__ == "__main__":
    unittest.main()
