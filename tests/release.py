#!/usr/bin/env python3
"""Release fault-injection tests: private Git remotes, no external publication."""
import contextlib
import datetime
import importlib.util
import io
import json
import os
import pathlib
import re
import subprocess
import tempfile
import unittest
from unittest import mock

ROOT = pathlib.Path(__file__).resolve().parents[1]
spec = importlib.util.spec_from_file_location("release_command", ROOT / "scripts/release.py")
release = importlib.util.module_from_spec(spec)
spec.loader.exec_module(release)
CURRENT = release.helper("validate-release").validate_versions(
    {name: (ROOT / name).read_text() for name in release.VERSION_FILES})
major, minor, patch = release.version_tuple(CURRENT)
NEXT = f"{major}.{minor}.{patch + 1}"


def metadata_fixture(root):
    for name in (*release.VERSION_FILES, *release.DOCUMENTS, "debian/control"):
        path = root / name
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text((ROOT / name).read_text())
    manifest = root / "Cargo.toml"
    manifest.write_text(manifest.read_text().replace("adamflott/p4ui", "example/p4ui"))
    changelog = root / "CHANGELOG.md"
    changelog.write_text(re.sub(r"(^## Unreleased\n).*?(?=^## |\Z)",
                               r"\1\n- A tested change.\n\n", changelog.read_text(),
                               count=1, flags=re.MULTILINE | re.DOTALL))


class GitFixture:
    def __init__(self, directory):
        self.root = directory / "checkout"
        self.remote = directory / "origin.git"
        self.root.mkdir()
        metadata_fixture(self.root)
        self.git("init", "--initial-branch=main")
        self.git("config", "user.name", "Release test")
        self.git("config", "user.email", "release@example.invalid")
        self.git("config", "commit.gpgsign", "false")
        self.git("config", "tag.gpgsign", "false")
        self.git("add", ".")
        self.git("commit", "-m", "Source")
        self.base = self.git("rev-parse", "HEAD")
        release.run(["git", "init", "--bare", "--initial-branch=main", self.remote], directory)
        self.git("remote", "add", "origin", self.remote)
        self.git("tag", "-a", f"v{CURRENT}", "-m", f"p4ui {CURRENT}")
        self.git("push", "--set-upstream", "origin", "main", "--tags")

    def git(self, *args):
        return release.run(["git", *args], self.root).stdout.strip()

    def remote_head(self):
        return self.git("--git-dir", self.remote, "rev-parse", "main")

    def advance_remote(self):
        parent = self.remote_head()
        commit = self.git("commit-tree", f"{parent}^{{tree}}", "-p", parent, "-m", "Concurrent work")
        self.git("push", "origin", f"{commit}:refs/heads/main")


class FakeGitHub:
    def __init__(self, fixture):
        self.fixture = fixture
        self.entry = None
        self.publications = 0
        self.verifications = 0
        self.calls = []
        self.failed_workflow = False
        self.corrupt_assets = False
        self.advance_during_verification = False
        self.asset_generation = 1
        self.attempt = 1
        self.change_assets = False
        self.rerun_workflow = False

    def context(self, tag):
        return {"nameWithOwner": "example/p4ui", "viewerPermission": "ADMIN",
                "defaultBranchRef": {"name": "main", "target": {"oid": self.fixture.remote_head()}},
                "latestRelease": {"tagName": f"v{CURRENT}", "isPrerelease": False},
                "release": dict(self.entry) if self.entry else None}

    def wait(self, workflow, sha, branch, required, *, tag_run=False):
        self.calls.append((workflow, sha, branch, tag_run))
        if tag_run:
            if self.failed_workflow:
                raise release.ReleaseError("Candidate CI failed")
            release.require(self.fixture.git("rev-parse", f"{branch}^{{commit}}") == sha,
                                         "Wrong workflow source")
            if self.entry is None:
                self.entry = {"id": "release-id", "tagName": branch, "isDraft": True,
                              "isPrerelease": False, "url": "https://github.com/example/p4ui/releases/1"}
        return {"databaseId": 1, "attempt": self.attempt,
                "url": "https://github.com/example/p4ui/actions/runs/1"}

    def asset_snapshot(self, tag):
        return self.asset_generation

    def verify_assets(self, tag):
        self.verifications += 1
        if self.corrupt_assets:
            raise release.ReleaseError("Checksum failed")
        if self.advance_during_verification:
            self.fixture.advance_remote()
        if self.change_assets:
            self.asset_generation += 1
        if self.rerun_workflow:
            self.attempt += 1

    def publish(self, tag):
        release.require(self.verifications > 0, "Verification must precede publication")
        release.require(self.entry and self.entry["tagName"] == tag, "Wrong release")
        self.publications += 1
        self.entry["isDraft"] = False


class ReleaseTests(unittest.TestCase):
    def setUp(self):
        self.environment = mock.patch.dict(os.environ, {
            "GIT_CONFIG_GLOBAL": os.devnull, "GIT_CONFIG_NOSYSTEM": "1",
            "GIT_AUTHOR_NAME": "Release test", "GIT_AUTHOR_EMAIL": "release@example.invalid",
            "GIT_COMMITTER_NAME": "Release test", "GIT_COMMITTER_EMAIL": "release@example.invalid",
        })
        self.environment.start()
        self.addCleanup(self.environment.stop)
        self.temporary = tempfile.TemporaryDirectory(prefix="p4ui-release-test-")
        self.addCleanup(self.temporary.cleanup)
        self.fixture = GitFixture(pathlib.Path(self.temporary.name))
        self.github = FakeGitHub(self.fixture)
        canonical = release.github_repository
        self.remote_parser = mock.patch.object(release, "github_repository",
            side_effect=lambda url: "example/p4ui" if url == str(self.fixture.remote) else canonical(url))
        self.remote_parser.start()
        self.addCleanup(self.remote_parser.stop)
        self.tools = mock.patch.object(release.shutil, "which", return_value="/test/tool")
        self.tools.start()
        self.addCleanup(self.tools.stop)
        self.check_versions = []

    def checks(self, root, *, baseline=False):
        current, _ = release.read_metadata(root)
        self.check_versions.append((baseline, current))

    def command(self, checks=None, version=NEXT):
        return release.Release(self.fixture.root, version, github=self.github,
                               checks=checks or self.checks)

    def execute(self, command, **kwargs):
        with contextlib.redirect_stdout(io.StringIO()):
            command.execute(**kwargs)

    def assert_unreleased(self):
        self.assertEqual(self.github.publications, 0)
        self.assertTrue(self.github.entry is None or self.github.entry['isDraft'])

    def assert_original(self):
        self.assertEqual(self.fixture.git("rev-parse", "HEAD"), self.fixture.base)
        self.assertEqual(self.fixture.remote_head(), self.fixture.base)
        self.assertEqual(self.fixture.git("status", "--porcelain"), "")
        self.assertEqual(self.fixture.git("tag", "--list", f"v{NEXT}"), "")
        self.assertEqual(len(self.fixture.git("worktree", "list", "--porcelain").split("worktree ")) - 1, 1)
        self.assert_unreleased()

    def test_complete_release_pushes_one_commit_and_managed_tag_then_publishes(self):
        self.execute(self.command())
        self.assertEqual(self.check_versions, [(True, CURRENT), (False, NEXT)])
        self.assertEqual(self.fixture.git("rev-parse", "HEAD^"), self.fixture.base)
        self.assertEqual(self.fixture.git("rev-parse", "HEAD"), self.fixture.remote_head())
        self.assertEqual(self.fixture.git("rev-parse", f"v{NEXT}^{{commit}}"), self.fixture.remote_head())
        self.assertEqual(release.read_metadata(self.fixture.root)[0], NEXT)
        self.assertEqual(release.helper("validate-release").release_mode(self.fixture.root, f"v{NEXT}"), "managed")
        self.assertEqual(self.github.publications, 1)
        self.assertEqual(self.fixture.git("status", "--porcelain"), "")
        release_wait = self.github.calls[-1]
        self.assertEqual(release_wait[1:4], (self.fixture.remote_head(), f"v{NEXT}", True))

    def test_check_validates_candidate_without_commit_tag_or_remote_update(self):
        self.execute(self.command(), check=True)
        self.assertEqual(self.check_versions, [(True, CURRENT), (False, NEXT)])
        self.assert_original()

    def test_push_does_not_publish_unrelated_local_tags(self):
        self.fixture.git('tag', '-a', 'local-only', '-m', 'Keep this local')
        self.fixture.git('config', 'push.followTags', 'true')
        self.execute(self.command())
        self.assertEqual(self.fixture.git('ls-remote', '--tags', 'origin', 'refs/tags/local-only'), '')
        self.assertEqual(self.github.publications, 1)

    def test_failed_baseline_or_candidate_checks_leave_checkout_and_remote_untouched(self):
        for fail_baseline in [True, False]:
            with self.subTest(baseline=fail_baseline):
                def checks(root, *, baseline=False):
                    if baseline == fail_baseline:
                        raise release.ReleaseError("Deliberate check failure")
                with self.assertRaisesRegex(release.ReleaseError, "Deliberate"):
                    self.execute(self.command(checks))
                self.assert_original()

    def test_dirty_index_or_untracked_files_abort(self):
        path = self.fixture.root / "work.txt"
        path.write_text("User work")
        with self.assertRaisesRegex(release.ReleaseError, "dirty"):
            self.execute(self.command())
        self.fixture.git("add", "work.txt")
        with self.assertRaisesRegex(release.ReleaseError, "dirty"):
            self.execute(self.command())
        self.assertEqual(path.read_text(), "User work")
        self.assertEqual(self.fixture.remote_head(), self.fixture.base)
        self.assert_unreleased()

    def test_concurrent_user_changes_are_preserved_and_abort_before_push(self):
        def checks(root, *, baseline=False):
            if not baseline:
                (self.fixture.root / "user-work").write_text("Keep me")
        with self.assertRaisesRegex(release.ReleaseError, "dirty"):
            self.execute(self.command(checks))
        self.assertEqual((self.fixture.root / "user-work").read_text(), "Keep me")
        self.assertEqual(self.fixture.remote_head(), self.fixture.base)
        self.assert_unreleased()

    def test_user_changes_during_tag_signing_abort_and_remove_only_the_owned_tag(self):
        command = self.command()
        git = command.git
        def concurrent(*args, **kwargs):
            result = git(*args, **kwargs)
            if args[:2] == ('tag', '-a'):
                (self.fixture.root / 'user-work').write_text('Keep signed-tag-time edits')
            return result
        with mock.patch.object(command, 'git', side_effect=concurrent):
            with self.assertRaisesRegex(release.ReleaseError, 'dirty'):
                self.execute(command)
        self.assertEqual((self.fixture.root / 'user-work').read_text(), 'Keep signed-tag-time edits')
        self.assertEqual(self.fixture.remote_head(), self.fixture.base)
        self.assertEqual(self.fixture.git('tag', '--list', f'v{NEXT}'), '')
        self.assert_unreleased()

    def test_concurrent_remote_update_aborts_before_a_release_tag_is_created(self):
        def checks(root, *, baseline=False):
            if not baseline:
                self.fixture.advance_remote()
        with self.assertRaisesRegex(release.ReleaseError, "out of date"):
            self.execute(self.command(checks))
        self.assertEqual(self.fixture.git("rev-parse", "HEAD"), self.fixture.base)
        self.assertEqual(self.fixture.git("tag", "--list", f"v{NEXT}"), "")
        self.assert_unreleased()

    def test_ahead_or_behind_checkout_aborts(self):
        self.fixture.advance_remote()
        with self.assertRaisesRegex(release.ReleaseError, "ahead of or behind"):
            self.execute(self.command())
        self.assertEqual(self.fixture.git("rev-parse", "HEAD"), self.fixture.base)
        self.assert_unreleased()

    def test_unpushed_source_commits_must_be_pushed_first(self):
        self.fixture.git("commit", "--allow-empty", "-m", "Unpushed work")
        head = self.fixture.git("rev-parse", "HEAD")
        with self.assertRaisesRegex(release.ReleaseError, "ahead of or behind"):
            self.execute(self.command())
        self.assertEqual(self.fixture.git("rev-parse", "HEAD"), head)
        self.assertEqual(self.fixture.remote_head(), self.fixture.base)
        self.assert_unreleased()

    def test_wrong_branch_or_detached_head_is_preserved(self):
        self.fixture.git("checkout", "-b", "work")
        with self.assertRaisesRegex(release.ReleaseError, "default branch"):
            self.execute(self.command())
        self.assertEqual(self.fixture.git("branch", "--show-current"), "work")
        self.fixture.git("checkout", "--detach", "HEAD")
        with self.assertRaisesRegex(release.ReleaseError, "default branch"):
            self.execute(self.command())
        self.assertEqual(self.fixture.git("branch", "--show-current"), "")
        self.assert_unreleased()

    def test_remote_repository_mismatch_aborts_before_network_mutation(self):
        self.fixture.git("remote", "set-url", "origin", "https://github.com/wrong/repository.git")
        with self.assertRaisesRegex(release.ReleaseError, "does not match"):
            self.execute(self.command())
        self.assert_unreleased()

    def test_missing_prerequisites_and_git_environment_overrides_abort(self):
        with mock.patch.object(release.shutil, 'which', side_effect=lambda name: None if name == 'gh' else '/tool'):
            with self.assertRaisesRegex(release.ReleaseError, "Missing release prerequisite: gh"):
                self.execute(self.command())
        with mock.patch.dict(os.environ, {'GIT_INDEX_FILE': str(self.fixture.root / 'unexpected-index')}):
            with self.assertRaisesRegex(release.ReleaseError, "override environment"):
                self.execute(self.command())
        self.assert_original()

    def test_newer_tag_appearing_during_checks_aborts(self):
        def checks(root, *, baseline=False):
            if not baseline:
                self.fixture.git("tag", "v999.0.0")
                self.fixture.git("push", "origin", "refs/tags/v999.0.0")
        with self.assertRaisesRegex(release.ReleaseError, "newer stable tag"):
            self.execute(self.command(checks))
        self.assertEqual(self.fixture.remote_head(), self.fixture.base)
        self.assertEqual(self.fixture.git("tag", "--list", f"v{NEXT}"), "")
        self.assert_unreleased()

    def test_failed_starting_commit_ci_aborts_before_preparation(self):
        with mock.patch.object(self.github, 'wait', side_effect=release.ReleaseError('Starting CI failed')):
            with self.assertRaisesRegex(release.ReleaseError, 'Starting CI failed'):
                self.execute(self.command())
        self.assertEqual(self.check_versions, [])
        self.assert_original()

    def test_rejected_atomic_push_leaves_both_remote_refs_unchanged(self):
        hook = self.fixture.remote / "hooks/pre-receive"
        hook.write_text("#!/bin/sh\nexit 1\n")
        hook.chmod(0o755)
        with self.assertRaises(release.ReleaseError):
            self.execute(self.command())
        self.assert_original()

    def test_server_without_atomic_push_support_aborts(self):
        self.fixture.git("--git-dir", self.fixture.remote, "config", "receive.advertiseAtomic", "false")
        with self.assertRaisesRegex(release.ReleaseError, "atomic"):
            self.execute(self.command())
        self.assert_original()

    def test_existing_local_tag_or_remote_draft_is_never_overwritten(self):
        self.fixture.git("tag", f"v{NEXT}")
        with self.assertRaisesRegex(release.ReleaseError, "already exists"):
            self.execute(self.command())
        self.fixture.git("tag", "-d", f"v{NEXT}")
        self.github.entry = {"id": "existing", "tagName": f"v{NEXT}", "isDraft": True}
        with self.assertRaisesRegex(release.ReleaseError, "already exists"):
            self.execute(self.command())
        self.assertEqual(self.github.entry['id'], 'existing')
        self.assert_unreleased()

    def test_failed_ci_or_bad_assets_never_publish_and_resume_verifies_again(self):
        self.github.corrupt_assets = True
        with self.assertRaisesRegex(release.ReleaseError, "Checksum"):
            self.execute(self.command())
        self.assert_unreleased()
        self.github.failed_workflow = True
        self.github.corrupt_assets = False
        with self.assertRaisesRegex(release.ReleaseError, "CI failed"):
            self.execute(self.command(version=NEXT), resume=True)
        self.assert_unreleased()
        self.github.failed_workflow = False
        self.execute(self.command(version=NEXT), resume=True)
        self.assertEqual(self.github.publications, 1)
        self.execute(self.command(version=NEXT), resume=True)
        self.assertEqual(self.github.publications, 1, "Already published releases must not be republished")

    def test_remote_advancing_during_asset_verification_leaves_draft_unpublished(self):
        self.github.advance_during_verification = True
        with self.assertRaisesRegex(release.ReleaseError, "out of date"):
            self.execute(self.command())
        self.assert_unreleased()

    def test_assets_replaced_during_verification_are_never_published(self):
        self.github.change_assets = True
        with self.assertRaisesRegex(release.ReleaseError, "assets changed"):
            self.execute(self.command())
        self.assert_unreleased()

    def test_workflow_rerun_during_verification_blocks_publication(self):
        self.github.rerun_workflow = True
        with self.assertRaisesRegex(release.ReleaseError, "workflow reran"):
            self.execute(self.command())
        self.assert_unreleased()

    def test_commit_hook_cannot_change_the_validated_release(self):
        hook = self.fixture.root / ".git/hooks/pre-commit"
        hook.write_text("#!/bin/sh\nprintf 'unexpected' >> README.md\ngit add README.md\n")
        hook.chmod(0o755)
        with self.assertRaisesRegex(release.ReleaseError, "Commit hooks"):
            self.execute(self.command())
        self.assert_original()

    def test_existing_release_lock_blocks_parallel_releases(self):
        with self.command().lock():
            with self.assertRaisesRegex(release.ReleaseError, "Release lock exists"):
                self.execute(self.command())
        self.assert_original()

    def test_only_valid_annotated_markers_select_managed_drafts(self):
        validator = release.helper('validate-release')
        self.fixture.git('tag', '-a', 'ordinary', '-m', 'Ordinary release')
        self.assertEqual(validator.release_mode(self.fixture.root, 'ordinary'), 'auto')
        self.fixture.git('tag', '-a', 'unknown', '-m', 'p4ui-release-mode: invalid')
        with self.assertRaisesRegex(ValueError, 'Invalid'):
            validator.release_mode(self.fixture.root, 'unknown')
        self.fixture.git('tag', '-a', 'duplicate', '-m', release.MANAGED_MARKER + '\n' + release.MANAGED_MARKER)
        with self.assertRaisesRegex(ValueError, 'duplicate'):
            validator.release_mode(self.fixture.root, 'duplicate')

    def test_annotated_tag_mode_reaches_workflow_outputs(self):
        tag = f'v{CURRENT}'
        self.fixture.git('tag', '-f', '-a', tag, '-m', release.MANAGED_MARKER)
        # Run the real validator against this private repository; the production
        # script resolves its own root from __file__, so install a copy there.
        scripts = self.fixture.root / 'scripts'
        scripts.mkdir()
        (scripts / 'validate-release.py').write_text((ROOT / 'scripts/validate-release.py').read_text())
        self.fixture.git('add', 'scripts/validate-release.py')
        self.fixture.git('commit', '-m', 'Validator')
        output = pathlib.Path(self.temporary.name) / 'outputs'
        environment = dict(os.environ, GITHUB_OUTPUT=str(output))
        release.run([__import__('sys').executable, scripts / 'validate-release.py', '--tag', tag],
                    self.fixture.root, env=environment)
        self.assertIn('release_mode=managed\n', output.read_text())


class MetadataTests(unittest.TestCase):
    def test_version_updates_preserve_dependencies_and_move_all_release_notes(self):
        with tempfile.TemporaryDirectory() as directory:
            root = pathlib.Path(directory)
            metadata_fixture(root)
            old_lock = tomllib_load(root / "Cargo.lock")
            updates = release.prepare_metadata(root, NEXT, datetime.datetime(2026, 11, 1, tzinfo=datetime.timezone.utc))
            self.assertEqual(release.read_metadata(root)[0], CURRENT, "Preparation must not write source files")
            for name, text in updates.items():
                (root / name).write_text(text)
            self.assertEqual(release.read_metadata(root)[0], NEXT)
            new_lock = tomllib_load(root / "Cargo.lock")
            old_dependencies = [p for p in old_lock['package'] if p['name'] != 'p4ui']
            self.assertEqual(old_dependencies, [p for p in new_lock['package'] if p['name'] != 'p4ui'])
            changelog = (root / 'CHANGELOG.md').read_text()
            self.assertIn(f'## Unreleased\n\n## {NEXT} — 2026-11-01', changelog)
            self.assertIn('- A tested change.', changelog)

    def test_stale_versions_docs_and_missing_notes_abort_without_writes(self):
        corruptions = {
            'Cargo.lock': lambda text: text.replace(f'name = "p4ui"\nversion = "{CURRENT}"', 'name = "p4ui"\nversion = "0.0.0"'),
            'debian/changelog': lambda text: text.replace(f'({CURRENT}-1)', '(0.0.0-1)', 1),
            'packaging/rpm/p4ui.spec': lambda text: text.replace(f'Version:        {CURRENT}', 'Version:        0.0.0'),
            'docs/BUILDING.md': lambda text: text.replace(f'p4ui-{CURRENT}', 'p4ui-0.0.0'),
            'README.md': lambda text: text.replace(f'./scripts/release.py {NEXT}', './scripts/release.py 0.0.0'),
            'CHANGELOG.md': lambda text: text.replace('- A tested change.', '- TODO'),
        }
        for name, corrupt in corruptions.items():
            with self.subTest(file=name), tempfile.TemporaryDirectory() as directory:
                root = pathlib.Path(directory)
                metadata_fixture(root)
                path = root / name
                path.write_text(corrupt(path.read_text()))
                before = {p: p.read_bytes() for p in root.rglob('*') if p.is_file()}
                with self.assertRaises((release.ReleaseError, ValueError)):
                    release.prepare_metadata(root, NEXT)
                self.assertEqual(before, {p: p.read_bytes() for p in before})

    def test_invalid_or_reused_versions_are_rejected(self):
        for version in ('v1.0.0', '1.0', '01.0.0', '1.0.0-beta.1', '1.0.0;bad', '-1.0.0'):
            with self.subTest(version=version), self.assertRaises(release.ReleaseError):
                release.version_tuple(version)
        with tempfile.TemporaryDirectory() as directory:
            root = pathlib.Path(directory)
            metadata_fixture(root)
            with self.assertRaises(release.ReleaseError):
                release.prepare_metadata(root, CURRENT)

    def test_repository_urls_must_identify_the_expected_github_repository(self):
        for url in ('https://github.com/example/p4ui.git', 'git@github.com:example/p4ui.git',
                    'ssh://git@github.com/example/p4ui.git'):
            self.assertEqual(release.github_repository(url), 'example/p4ui')
        for url in ('https://other.invalid/example/p4ui', 'https://token@github.com/example/p4ui', '/tmp/repo'):
            with self.assertRaises(release.ReleaseError):
                release.github_repository(url)


class WorkflowTests(unittest.TestCase):
    def test_publication_workflow_keeps_managed_tags_draft_only(self):
        text = (ROOT / '.github/workflows/packages.yml').read_text()
        # Exercise the actual publication shell block with a recorder in place
        # of python3. No GitHub operations or package uploads can occur.
        match = re.search(r'        run: \|\n(          args=\(\).*?)(?=^\S|\Z)', text,
                          re.MULTILINE | re.DOTALL)
        self.assertIsNotNone(match)
        script = '\n'.join(line[10:] for line in match[1].splitlines())
        with tempfile.TemporaryDirectory() as directory:
            root = pathlib.Path(directory)
            recorder = root / 'python3'
            recorder.write_text('#!/bin/sh\nprintf "%s\\n" "$@" > "$RECORD"\n')
            recorder.chmod(0o755)
            for mode in ('managed', 'auto'):
                environment = dict(os.environ, PATH=str(root) + os.pathsep + os.environ['PATH'],
                                   RELEASE_MODE=mode, RELEASE_TAG='v1.0.0', SOURCE_COMMIT='a' * 40,
                                   GITHUB_REPOSITORY='example/p4ui', RECORD=str(root / 'arguments'))
                release.run(['bash', '-c', script], root, env=environment)
                arguments = (root / 'arguments').read_text().splitlines()
                self.assertEqual('--draft-only' in arguments, mode == 'managed')
                self.assertIn('a' * 40, arguments)
    def test_wait_timeout_and_newest_failed_run_cannot_authorize_publication(self):
        client = release.GitHub(ROOT, 'example/p4ui', timeout=1)
        row = {'databaseId': 1, 'headSha': 'a' * 40, 'headBranch': 'main', 'event': 'push',
               'status': 'in_progress', 'conclusion': '', 'createdAt': '2026-11-01', 'url': 'run'}
        clock = [0]
        with mock.patch.object(client, 'command', return_value=subprocess.CompletedProcess([], 0, json.dumps([row]), '')), \
             mock.patch.object(release.time, 'monotonic', side_effect=lambda: clock[0]), \
             mock.patch.object(release.time, 'sleep', side_effect=lambda seconds: clock.__setitem__(0, clock[0] + seconds)):
            with self.assertRaisesRegex(release.ReleaseError, 'Timed out'):
                client.wait('ci.yml', 'a' * 40, 'main', ['msrv'])
        rows = [{**row, 'status': 'completed', 'conclusion': 'success'},
                {**row, 'databaseId': 2, 'createdAt': '2026-11-02', 'status': 'completed', 'conclusion': 'failure'}]
        with mock.patch.object(client, 'command', return_value=subprocess.CompletedProcess([], 0, json.dumps(rows), '')):
            with self.assertRaisesRegex(release.ReleaseError, 'failed'):
                client.wait('ci.yml', 'a' * 40, 'main', ['msrv'])

    def test_github_errors_are_not_treated_as_a_missing_release(self):
        client = release.GitHub(ROOT, 'example/p4ui')
        with mock.patch.object(client, 'command', return_value=subprocess.CompletedProcess([], 0, '{"errors":[{"message":"Denied"}]}', '')):
            with self.assertRaisesRegex(release.ReleaseError, 'could not determine'):
                client.context('v1.0.0')

    def test_missing_skipped_duplicate_or_failed_required_jobs_are_not_success(self):
        good = {'name': 'Release source checks / msrv', 'status': 'completed', 'conclusion': 'success'}
        release.verify_jobs([good], ['msrv'])
        for jobs in ([], [good, good], [{**good, 'conclusion': 'skipped'}],
                     [{**good, 'conclusion': 'failure'}], [{**good, 'status': 'in_progress'}]):
            with self.assertRaises(release.ReleaseError):
                release.verify_jobs(jobs, ['msrv'])

    def test_tag_workflow_selection_cannot_accept_the_simultaneous_branch_run(self):
        client = release.GitHub(ROOT, 'example/p4ui')
        rows = [
            {'databaseId': 2, 'attempt': 1, 'headSha': 'a' * 40, 'headBranch': 'main', 'event': 'push',
             'status': 'completed', 'conclusion': 'success', 'createdAt': '2026-11-02', 'url': 'branch'},
            {'databaseId': 1, 'attempt': 1, 'headSha': 'a' * 40, 'headBranch': 'v1.0.0', 'event': 'push',
             'status': 'completed', 'conclusion': 'success', 'createdAt': '2026-11-01', 'url': 'tag'},
        ]
        details = {'headSha': 'a' * 40, 'jobs': [{'name': 'msrv', 'status': 'completed', 'conclusion': 'success'}]}
        with mock.patch.object(client, 'command', side_effect=[
            subprocess.CompletedProcess([], 0, json.dumps(rows), ''),
            subprocess.CompletedProcess([], 0, json.dumps(details), ''),
        ]) as calls:
            self.assertEqual(client.wait('packages.yml', 'a' * 40, 'v1.0.0', ['msrv'], tag_run=True)['url'], 'tag')
            self.assertIn('1', calls.call_args_list[-1].args)


def tomllib_load(path):
    import tomllib
    return tomllib.loads(path.read_text())


if __name__ == '__main__':
    unittest.main()
