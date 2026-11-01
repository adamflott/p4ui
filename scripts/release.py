#!/usr/bin/env python3
"""Prepare, validate, tag, and publish a stable p4ui release, failing closed."""
import argparse
import contextlib
import datetime
import email.utils
import importlib.util
import json
import os
import pathlib
import re
import shutil
import signal
import subprocess
import sys
import tempfile
import time

if sys.version_info < (3, 11):
    sys.exit("Release tooling requires Python 3.11 or newer")
import tomllib

ROOT = pathlib.Path(__file__).resolve().parents[1]
VERSION_PATTERN = r"(?:0|[1-9][0-9]*)\.(?:0|[1-9][0-9]*)\.(?:0|[1-9][0-9]*)"
MANAGED_MARKER = "p4ui-release-mode: managed"
VERSION_FILES = ("Cargo.toml", "Cargo.lock", "debian/changelog", "packaging/rpm/p4ui.spec")
DOCUMENTS = ("CHANGELOG.md", "docs/BUILDING.md", "README.md", "docs/RELEASING.md")
CI_JOBS = ("rust (ubuntu-latest)", "rust (macos-latest)", "rust (windows-latest)",
           "msrv", "perforce (r24.2)", "perforce (r26.1)")
PACKAGE_JOBS = ("Validate versions and select source commit", "Debian / Ubuntu (x86_64)",
                "Fedora RPM (x86_64)", "Arch Linux (x86_64)")


class ReleaseError(RuntimeError):
    pass


def require(condition, message):
    if not condition:
        raise ReleaseError(message)


def helper(name):
    spec = importlib.util.spec_from_file_location(name, ROOT / f"scripts/{name}.py")
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def version_tuple(version):
    require(re.fullmatch(VERSION_PATTERN, version), "Version must be stable X.Y.Z without a v prefix")
    parts = tuple(map(int, version.split(".")))
    require(all(n <= 2**64 - 1 for n in parts), "Version component exceeds Cargo's SemVer range")
    return parts


def github_repository(url):
    match = re.fullmatch(r"(?:https://github\.com/|git@github\.com:|ssh://git@github\.com/)"
                         r"([A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+?)(?:\.git)?/?", url)
    require(match is not None, "Origin and package repository must use a standard GitHub HTTPS or SSH URL")
    return match[1]


def replace_once(text, pattern, replacement, label):
    result, count = re.subn(pattern, replacement, text, flags=re.MULTILINE)
    require(count == 1, f"Expected one {label}; found {count}. Inspect release metadata")
    return result


def read_metadata(root):
    files = {name: (root / name).read_text() for name in (*VERSION_FILES, *DOCUMENTS)}
    current = helper("validate-release").validate_versions(files)
    sections = re.findall(r"^## (" + VERSION_PATTERN + r") — (\d{4}-\d{2}-\d{2})$",
                          files["CHANGELOG.md"], re.MULTILINE)
    require(sections and sections[0][0] == current, "CHANGELOG.md release version is out of date")
    require(len(re.findall(r"^## Unreleased$", files["CHANGELOG.md"], re.MULTILINE)) == 1,
            "CHANGELOG.md must have exactly one Unreleased section")
    rpm_entries = re.findall(r"^\* .+ - (" + VERSION_PATTERN + r")-\d+$",
                             files["packaging/rpm/p4ui.spec"], re.MULTILINE)
    require(rpm_entries and rpm_entries[0] == current, "RPM changelog version is out of date")
    build_versions = set(re.findall(r"p4ui[-_](" + VERSION_PATTERN + r")", files["docs/BUILDING.md"]))
    require(build_versions == {current}, "Build instructions contain stale or missing package versions")
    major, minor, patch = version_tuple(current)
    next_example = f"{major}.{minor}.{patch + 1}"
    for name in ("README.md", "docs/RELEASING.md"):
        examples = re.findall(r"\./scripts/release\.py (" + VERSION_PATTERN + r")", files[name])
        require(examples and set(examples) == {next_example}, f"{name} release command example is out of date")
    return current, files


def prepare_metadata(root, version, now=None):
    current, files = read_metadata(root)
    require(version_tuple(version) > version_tuple(current), "New version must exceed the current package version")
    now = now or datetime.datetime.now(datetime.timezone.utc)
    require(now.tzinfo is not None, "Release date must include a timezone")
    now = now.astimezone(datetime.timezone.utc)
    changelog = files["CHANGELOG.md"]
    match = re.search(r"^## Unreleased\n(.*?)(?=^## |\Z)", changelog, re.MULTILINE | re.DOTALL)
    require(match is not None, "Missing Unreleased section")
    notes = match[1].strip()
    require(notes and re.search(r"^- \S", notes, re.MULTILINE), "Add real release notes under Unreleased first")
    require(not re.search(r"^\s*(?:[-*] )?(?:TODO|TBD|\.\.\.)\s*$", notes, re.MULTILINE),
            "Replace placeholder release notes before releasing")
    require(not re.search(r"^## " + re.escape(version) + r"\b", changelog, re.MULTILINE),
            "CHANGELOG.md already contains this release")
    files["CHANGELOG.md"] = changelog[:match.start()] + (
        f"## Unreleased\n\n## {version} — {now.date().isoformat()}\n\n{notes}\n\n"
    ) + changelog[match.end():]
    # Match the package version only; dependency versions remain locked unchanged.
    package = re.search(r"(?ms)^\[package\]\n.*?(?=^\[|\Z)", files["Cargo.toml"])
    require(package is not None, "Missing Cargo package table")
    updated = replace_once(package[0], r'^version = "' + re.escape(current) + r'"$',
                           f'version = "{version}"', "Cargo package version")
    files["Cargo.toml"] = files["Cargo.toml"][:package.start()] + updated + files["Cargo.toml"][package.end():]
    files["Cargo.lock"] = replace_once(
        files["Cargo.lock"], r'^name = "p4ui"\nversion = "' + re.escape(current) + r'"$',
        f'name = "p4ui"\nversion = "{version}"', "locked p4ui version")
    maintainer = re.search(r"^Maintainer: (.+)$", (root / "debian/control").read_text(), re.MULTILINE)
    require(maintainer and email.utils.parseaddr(maintainer[1])[1].count("@") == 1,
            "debian/control has no valid maintainer identity")
    files["debian/changelog"] = (
        f"p4ui ({version}-1) unstable; urgency=medium\n\n"
        f"  * Release {version}. See CHANGELOG.md for the complete release notes.\n\n"
        f" -- {maintainer[1]}  {email.utils.format_datetime(now)}\n\n"
        + files["debian/changelog"])
    rpm = replace_once(files["packaging/rpm/p4ui.spec"], r"^Version:\s*\S+\s*$",
                       f"Version:        {version}", "RPM version")
    rpm = replace_once(rpm, r"^Release:\s*\S+\s*$", "Release:        1%{?dist}", "RPM release")
    # English weekday/month names are required regardless of the user's locale.
    date_parts = email.utils.format_datetime(now).split()
    rpm_date = f"{date_parts[0].rstrip(',')} {date_parts[2]} {date_parts[1]} {date_parts[3]}"
    files["packaging/rpm/p4ui.spec"] = replace_once(
        rpm, r"^%changelog$",
        f"%changelog\n* {rpm_date} {maintainer[1]} - {version}-1\n- Release {version}; see CHANGELOG.md\n",
        "RPM changelog")
    files["docs/BUILDING.md"] = files["docs/BUILDING.md"].replace(current, version)
    major, minor, patch = version_tuple(version)
    for name in ("README.md", "docs/RELEASING.md"):
        files[name] = re.sub(r"(\./scripts/release\.py )" + VERSION_PATTERN,
                             lambda m: m[1] + f"{major}.{minor}.{patch + 1}", files[name])
    helper("validate-release").validate_versions(files, f"v{version}")
    return {name: text for name, text in files.items() if text != (root / name).read_text()}


def run(args, cwd, *, capture=True, env=None, timeout=120, check=True):
    args = list(map(str, args))
    with subprocess.Popen(args, cwd=cwd, text=True, env=env, start_new_session=True,
                          stdout=subprocess.PIPE if capture else None,
                          stderr=subprocess.PIPE if capture else None) as process:
        try:
            stdout, stderr = process.communicate(timeout=timeout)
        except (subprocess.TimeoutExpired, KeyboardInterrupt):
            os.killpg(process.pid, signal.SIGTERM)
            try:
                process.communicate(timeout=5)
            except subprocess.TimeoutExpired:
                os.killpg(process.pid, signal.SIGKILL)
                process.communicate()
            raise
        result = subprocess.CompletedProcess(args, process.returncode, stdout, stderr)
    if check and result.returncode:
        detail = (result.stderr or result.stdout or "").strip()
        raise ReleaseError(f"{args[0]} {args[1]} failed ({result.returncode}): {detail}")
    return result


def verify_jobs(jobs, required):
    for name in required:
        matches = [job for job in jobs if job["name"] == name or job["name"].endswith(" / " + name)]
        require(len(matches) == 1 and matches[0]["status"] == "completed"
                and matches[0]["conclusion"] == "success", f"Required check did not pass: {name}")


class GitHub:
    def __init__(self, root, repository, timeout=3600):
        self.root, self.repository, self.timeout = root, repository, timeout

    def command(self, *args, **kwargs):
        return run(["gh", *args], self.root, **kwargs)

    def context(self, tag):
        owner, name = self.repository.split("/")
        query = """query($owner:String!,$name:String!,$tag:String!){
          repository(owner:$owner,name:$name){nameWithOwner viewerPermission
            defaultBranchRef{name target{oid}} latestRelease{tagName isPrerelease}
            release(tagName:$tag){id tagName isDraft isPrerelease url}}}"""
        result = json.loads(self.command("api", "graphql", "-f", f"query={query}",
                           "-f", f"owner={owner}", "-f", f"name={name}", "-f", f"tag={tag}").stdout)
        require(not result.get("errors"), "GitHub could not determine repository/release state")
        repository = result.get("data", {}).get("repository")
        require(repository and repository["nameWithOwner"].lower() == self.repository.lower(),
                "GitHub repository identity does not match origin")
        require(repository["viewerPermission"] in ("ADMIN", "MAINTAIN", "WRITE"),
                "GitHub login needs write permission to this repository")
        require(repository["defaultBranchRef"], "GitHub repository has no default branch")
        return repository

    def wait(self, workflow, sha, branch, required, *, tag_run=False):
        deadline = time.monotonic() + self.timeout
        previous = None
        while time.monotonic() < deadline:
            rows = json.loads(self.command(
                "run", "list", "--repo", self.repository, "--workflow", workflow,
                "--commit", sha, "--limit", "100", "--json",
                "databaseId,attempt,headSha,headBranch,event,status,conclusion,url,createdAt",
                timeout=min(120, max(1, deadline - time.monotonic()))).stdout)
            events = ("push", "workflow_dispatch") if tag_run else ("push",)
            matches = [row for row in rows if row["headSha"] == sha
                       and row["headBranch"] == branch and row["event"] in events]
            if matches:
                current = max(matches, key=lambda row: row["createdAt"])
                state = current["status"]
                if state == "completed":
                    require(current["conclusion"] == "success", f"{workflow} failed: {current['url']}")
                    details = json.loads(self.command("run", "view", str(current["databaseId"]),
                                        "--repo", self.repository, "--json", "jobs,headSha",
                                        timeout=min(120, max(1, deadline - time.monotonic()))).stdout)
                    require(details["headSha"] == sha, "Workflow checked a different source commit")
                    verify_jobs(details["jobs"], required)
                    return current
            else:
                state = "waiting for workflow"
            if state != previous:
                print(f"{workflow} at {sha[:12]} ({branch}): {state}", flush=True)
                previous = state
            time.sleep(min(15, max(0, deadline - time.monotonic())))
        raise ReleaseError(f"Timed out waiting for {workflow}. Inspect GitHub Actions before resuming")

    def verify_assets(self, tag):
        with tempfile.TemporaryDirectory(prefix="p4ui-release-assets-") as temporary:
            directory = pathlib.Path(temporary)
            self.command("release", "download", tag, "--repo", self.repository,
                         "--dir", str(directory), timeout=300)
            helper("publish-release").verify_release_directory(tag, directory)

    def asset_snapshot(self, tag):
        assets = json.loads(self.command("release", "view", tag, "--repo", self.repository,
                            "--json", "assets").stdout)["assets"]
        require(0 < len(assets) < 100, "Release asset metadata is missing or incomplete")
        require(all(asset["state"] == "uploaded" and asset["size"] > 0 for asset in assets),
                "Release contains unfinished or empty uploads")
        return sorted((asset["id"], asset["name"], asset["size"], asset.get("digest"), asset["updatedAt"])
                      for asset in assets)

    def publish(self, tag):
        self.command("release", "edit", tag, "--repo", self.repository, "--verify-tag", "--draft=false")


class Release:
    def __init__(self, root, version, *, github=None, checks=None, timeout=3600):
        self.root = pathlib.Path(root).resolve()
        self.version, self.tag = version, f"v{version}"
        self.github, self.checks, self.timeout = github, checks or local_checks, timeout
        self.phase = "preflight"

    def git(self, *args, **kwargs):
        return run(["git", *args], self.root, **kwargs).stdout.strip()

    def clean(self, head=None):
        require(not self.git("status", "--porcelain=v1", "--untracked-files=all"),
                "Working tree/index is dirty. Commit or remove changes before releasing")
        branch = run(["git", "symbolic-ref", "--quiet", "--short", "HEAD"], self.root, check=False)
        require(branch.returncode == 0 and branch.stdout.strip() == self.branch,
                "Checkout must remain on the GitHub default branch")
        if head:
            require(self.git("rev-parse", "HEAD") == head, "Local HEAD changed during release; aborting")
        for state in ("MERGE_HEAD", "CHERRY_PICK_HEAD", "REVERT_HEAD", "rebase-merge", "rebase-apply", "BISECT_LOG"):
            path = pathlib.Path(self.git("rev-parse", "--git-path", state))
            require(not (self.root / path).exists(), f"Unfinished Git operation: {state}")

    def refs(self):
        rows = self.git("ls-remote", "--heads", "--tags", "origin").splitlines()
        return dict((ref, sha) for sha, ref in (row.split("\t", 1) for row in rows))

    def remote_matches(self, head, tag_object=None):
        require(self.origin_repository().lower() == self.repository.lower(), "Origin changed during release")
        context = self.github.context(self.tag)
        require(context["defaultBranchRef"]["name"] == self.branch
                and context["defaultBranchRef"]["target"]["oid"] == head,
                "GitHub default branch moved; release source is out of date")
        refs = self.refs()
        require(refs.get(f"refs/heads/{self.branch}") == head, "Remote branch is out of date")
        stable_versions = [version_tuple(ref.removeprefix("refs/tags/v"))
                           for ref in refs if re.fullmatch(r"refs/tags/v" + VERSION_PATTERN, ref)]
        require(not stable_versions or max(stable_versions) <= version_tuple(self.version),
                "A newer stable tag appeared; this release is out of date")
        latest = context.get("latestRelease")
        if latest and not latest["isPrerelease"]:
            require(latest["tagName"].startswith("v")
                    and version_tuple(latest["tagName"][1:]) <= version_tuple(self.version),
                    "A newer published release appeared; this release is out of date")
        if tag_object:
            require(refs.get(f"refs/tags/{self.tag}") == tag_object
                    and refs.get(f"refs/tags/{self.tag}^{{}}") == head, "Release tag moved or is not annotated")
        else:
            require(f"refs/tags/{self.tag}" not in refs and context["release"] is None,
                    "Release tag or GitHub release already exists; use --resume only for a managed candidate")
        return context, refs

    def origin_repository(self):
        urls = self.git("remote", "get-url", "--all", "origin").splitlines()
        pushes = self.git("remote", "get-url", "--push", "--all", "origin").splitlines()
        require(len(urls) == len(pushes) == 1, "Origin must have exactly one fetch and one push destination")
        repository = github_repository(urls[0])
        require(github_repository(pushes[0]).lower() == repository.lower(), "Origin push/fetch repositories differ")
        manifest = tomllib.loads((self.root / "Cargo.toml").read_text())["package"]
        require(github_repository(manifest["repository"]).lower() == repository.lower(),
                "Origin does not match Cargo.toml's repository")
        return repository

    @contextlib.contextmanager
    def lock(self):
        common = pathlib.Path(self.git("rev-parse", "--git-common-dir"))
        path = (self.root / common).resolve() / "p4ui-release.lock"
        try:
            descriptor = os.open(path, os.O_CREAT | os.O_EXCL | os.O_WRONLY, 0o600)
        except FileExistsError as error:
            raise ReleaseError(f"Release lock exists: {path}. Inspect the other/interrupted release before removing it") from error
        try:
            with os.fdopen(descriptor, "w") as output:
                output.write(f"pid={os.getpid()}\nversion={self.version}\n")
            yield
        finally:
            path.unlink()

    def preflight(self, resume):
        version_tuple(self.version)
        require(os.name == "posix", "Run the release command on macOS or Linux; CI validates Windows")
        for executable in ("git", "gh", "cargo", "rustc", "bash", "p4", "p4d"):
            require(shutil.which(executable), f"Missing release prerequisite: {executable}")
        require(self.git("rev-parse", "--is-shallow-repository") == "false", "Fetch full Git history before releasing")
        repository = self.repository = self.origin_repository()
        self.github = self.github or GitHub(self.root, repository, self.timeout)
        context = self.github.context(self.tag)
        self.branch = context["defaultBranchRef"]["name"]
        self.clean()
        self.base = self.git("rev-parse", "HEAD")
        self.git("fetch", "--no-tags", "origin", f"refs/heads/{self.branch}:refs/remotes/origin/{self.branch}")
        require(self.git("rev-parse", f"origin/{self.branch}") == self.base,
                "Checkout is ahead of or behind origin. Synchronize and push source changes first")
        require(self.git("rev-parse", "--symbolic-full-name", "@{upstream}") == f"refs/remotes/origin/{self.branch}",
                "Branch must track origin's default branch")
        self.git("var", "GIT_AUTHOR_IDENT")
        self.git("var", "GIT_COMMITTER_IDENT")
        current, _ = read_metadata(self.root)
        refs = self.refs()
        if resume:
            require(current == self.version, "Resume requires the release version to be checked out")
            self.tag_object = self.git("rev-parse", f"refs/tags/{self.tag}")
            require(self.git("cat-file", "-t", self.tag_object) == "tag", "Resume requires an annotated managed tag")
            require(MANAGED_MARKER in self.git("for-each-ref", "--format=%(contents)", f"refs/tags/{self.tag}").splitlines(),
                    "Tag was not created by the managed release command")
            require(self.git("rev-parse", f"{self.tag}^{{commit}}") == self.base, "Checked-out source differs from the release tag")
            self.remote_matches(self.base, self.tag_object)
        else:
            require(version_tuple(self.version) > version_tuple(current), "New version must exceed the current package version")
            require(not self.git("tag", "--list", self.tag), "Local release tag already exists")
            self.remote_matches(self.base)
            stable = [(version_tuple(ref.removeprefix("refs/tags/v")), ref)
                      for ref in refs if re.fullmatch(r"refs/tags/v" + VERSION_PATTERN, ref)]
            if stable:
                latest, ref = max(stable)
                require(version_tuple(current) >= latest, "Package version is behind the latest stable tag")
                require(version_tuple(self.version) > latest, "New version must exceed every existing stable tag")
                commit = refs.get(ref + "^{}", refs[ref])
                self.git("merge-base", "--is-ancestor", commit, self.base)
            latest = context.get("latestRelease")
            if latest and not latest["isPrerelease"]:
                require(latest["tagName"].startswith("v"), "Latest published release has an unsupported tag")
                require(version_tuple(self.version) > version_tuple(latest["tagName"][1:]),
                        "Version is not newer than the latest published release")
        # A missing, skipped, failed, or stale required check cannot authorize a release.
        self.github.wait("ci.yml", self.base, self.branch, CI_JOBS)
        self.github.wait("packages.yml", self.base, self.branch, PACKAGE_JOBS)
        self.clean(self.base)

    def prepare_and_push(self, check):
        print(f"Preparing {self.tag} in an isolated worktree", flush=True)
        with tempfile.TemporaryDirectory(prefix="p4ui-release-") as temporary:
            worktree = pathlib.Path(temporary) / "source"
            self.git("worktree", "add", "--detach", worktree, self.base)
            try:
                # Reject an already-stale lockfile before changing only the root package version.
                self.checks(worktree, baseline=True)
                updates = prepare_metadata(worktree, self.version)
                for name, text in updates.items():
                    (worktree / name).write_text(text)
                diff = ["git", "diff", "--no-ext-diff", "--no-textconv", "--binary", "--full-index"]
                patch = run(diff, worktree).stdout
                self.checks(worktree)
                require(run(diff, worktree).stdout == patch,
                        "Checks changed tracked release files; inspect the failed candidate")
                require(not run(["git", "ls-files", "--others", "--exclude-standard"], worktree).stdout,
                        "Checks left unexpected untracked files in the candidate")
                self.clean(self.base)
                self.remote_matches(self.base)
                if check:
                    print(f"All preflight and local checks passed for {self.tag}; no commit, tag, or release created")
                    return
                run(["git", "add", "--", *updates], worktree)
                run(["git", "commit", "--only", "-m", f"Release p4ui {self.version}", "--", *updates], worktree)
                self.candidate = run(["git", "rev-parse", "HEAD"], worktree).stdout.strip()
                parents = run(["git", "rev-list", "--parents", "-n", "1", "HEAD"], worktree).stdout.split()
                require(parents == [self.candidate, self.base], "Release must be exactly one commit after the validated source")
                require(not run(["git", "status", "--porcelain=v1"], worktree).stdout,
                        "Commit hooks modified the release candidate")
                require(run([*diff, self.base, "HEAD"], worktree).stdout == patch,
                        "Commit hooks changed the validated release patch")
                for name, text in updates.items():
                    require(run(["git", "show", f"HEAD:{name}"], worktree).stdout == text,
                            f"Commit hooks changed {name}")
                self.clean(self.base)
                self.remote_matches(self.base)
                self.git("tag", "-a", self.tag, self.candidate, "-m",
                         f"p4ui {self.version}\n\n{MANAGED_MARKER}")
                self.tag_object = self.git("rev-parse", f"refs/tags/{self.tag}")
                push_started = False
                try:
                    # Signing a tag can take time; user work or remote state may
                    # have changed since the previous check.
                    self.clean(self.base)
                    self.remote_matches(self.base)
                    print(f"Pushing release commit and {self.tag} atomically", flush=True)
                    push_started = True
                    self.git("push", "--atomic", "--no-follow-tags", f"--force-with-lease=refs/heads/{self.branch}:{self.base}",
                             "origin", f"{self.candidate}:refs/heads/{self.branch}", f"refs/tags/{self.tag}:refs/tags/{self.tag}")
                except (ReleaseError, subprocess.TimeoutExpired, KeyboardInterrupt) as error:
                    if not push_started:
                        self.git("update-ref", "-d", f"refs/tags/{self.tag}", self.tag_object)
                        raise
                    # A disconnected push may have been accepted. Confirm both refs before proceeding.
                    refs = self.refs()
                    if not (refs.get(f"refs/heads/{self.branch}") == self.candidate
                            and refs.get(f"refs/tags/{self.tag}") == self.tag_object):
                        self.git("update-ref", "-d", f"refs/tags/{self.tag}", self.tag_object)
                        raise
                    if isinstance(error, KeyboardInterrupt):
                        self.phase = "pushed"
                        raise
                self.phase = "pushed"
                self.clean(self.base)
                self.git("merge", "--ff-only", self.candidate)
                self.clean(self.candidate)
            finally:
                self.git("worktree", "remove", "--force", worktree)

    def finish(self):
        sha = self.git("rev-parse", "HEAD")
        self.remote_matches(sha, self.tag_object)
        required = (*CI_JOBS, *PACKAGE_JOBS, "Publish tagged GitHub release")
        producer = self.github.wait("packages.yml", sha, self.tag, required, tag_run=True)
        context, _ = self.remote_matches(sha, self.tag_object)
        release = context["release"]
        require(release and release["tagName"] == self.tag and not release["isPrerelease"],
                "The workflow did not produce the expected stable release")
        print(f"Verifying all {self.tag} assets and checksums", flush=True)
        assets = self.github.asset_snapshot(self.tag)
        self.github.verify_assets(self.tag)
        self.clean(sha)
        confirmed = self.github.wait("packages.yml", sha, self.tag, required, tag_run=True)
        require((producer["databaseId"], producer["attempt"]) == (confirmed["databaseId"], confirmed["attempt"]),
                "Release workflow reran during verification; resume after it finishes")
        require(self.github.asset_snapshot(self.tag) == assets, "Release assets changed during verification; aborting")
        latest, _ = self.remote_matches(sha, self.tag_object)
        require(latest["release"] and latest["release"]["id"] == release["id"], "Release identity changed during verification")
        if release["isDraft"]:
            require(latest["release"]["isDraft"], "Release was published by another process during verification")
            self.phase = "publishing"
            print(f"All checks passed; publishing {self.tag}", flush=True)
            self.github.publish(self.tag)
        final = self.github.context(self.tag)["release"]
        require(final and not final["isDraft"] and final["id"] == release["id"], "Publication outcome could not be confirmed")
        self.phase = "published"
        print(f"Published and verified {self.tag}: {final['url']}")

    def execute(self, *, check=False, resume=False):
        require(not any(os.environ.get(name) for name in
                        ("GIT_DIR", "GIT_WORK_TREE", "GIT_INDEX_FILE", "GIT_COMMON_DIR")),
                "Unset Git directory/index override environment variables before releasing")
        with self.lock():
            print(f"Checking repository state and existing checks for {self.tag}", flush=True)
            self.preflight(resume)
            if not resume:
                self.prepare_and_push(check)
            if not check:
                self.finish()


def local_checks(root, *, baseline=False):
    environment = dict(os.environ, CARGO_TARGET_DIR=str(root / "target"), PYTHONDONTWRITEBYTECODE="1")
    if baseline:
        print("Checking the original locked dependency graph", flush=True)
        run(["cargo", "metadata", "--locked", "--format-version", "1"], root, env=environment)
        return
    commands = [
        ["cargo", "fmt", "--all", "--", "--check"],
        ["cargo", "clippy", "--locked", "--all-targets", "--", "-D", "warnings"],
        ["cargo", "test", "--locked", "--all-targets", "--", "--include-ignored"],
        [sys.executable, "tests/packaging.py"],
        [sys.executable, "tests/release.py"],
        ["bash", "-n", "scripts/vendor-source.sh", "scripts/install-docs.sh", "packaging/arch/PKGBUILD.in"],
        ["cargo", "build", "--locked"],
        [sys.executable, "tests/ui_smoke.py"],
        [sys.executable, "tests/history_ui.py"],
    ]
    for args in commands:
        print("Checking: " + " ".join(args), flush=True)
        run(args, root, capture=False, env=environment, timeout=1800)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("version", help="New stable version, e.g. 0.2.1")
    modes = parser.add_mutually_exclusive_group()
    modes.add_argument("--check", action="store_true", help="Run all preflight/local checks without committing or pushing")
    modes.add_argument("--resume", action="store_true", help="Verify and finish an existing managed release candidate")
    parser.add_argument("--timeout-minutes", type=int, default=60, help="Maximum wait per GitHub workflow (default 60)")
    args = parser.parse_args()
    if not 1 <= args.timeout_minutes <= 240:
        parser.error("--timeout-minutes must be 1..240")
    release = Release(ROOT, args.version, timeout=args.timeout_minutes * 60)
    try:
        release.execute(check=args.check, resume=args.resume)
    except (ReleaseError, ValueError, OSError, subprocess.SubprocessError, KeyboardInterrupt) as error:
        print(f"Release stopped: {error or 'interrupted'}", file=sys.stderr)
        if release.phase == "publishing":
            print(f"Publication may have completed. Confirm with ./scripts/release.py {args.version} --resume", file=sys.stderr)
        else:
            print("No publication was requested. Any pushed candidate/draft is retained for inspection and --resume.", file=sys.stderr)
        return 130 if isinstance(error, KeyboardInterrupt) else 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
