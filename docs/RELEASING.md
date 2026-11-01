# Automated releases from Git tags

Pushing a **stable `vX.Y.Z` tag** runs `.github/workflows/packages.yml`. After all three Linux package builds, installation checks, and the full CI suite succeed for the validated source commit, ordinary tags publish a GitHub Release. Tags created by the release command prepare a draft for the command to verify and publish. Pushing a branch builds artifacts only.

The release includes:

- Debian/Ubuntu `.deb` (x86_64).
- Fedora `.rpm` (x86_64) and source RPM.
- Arch `.pkg.tar.zst` (x86_64).
- `p4ui-X.Y.Z.tar.gz`, with the locked vendored Rust dependencies.
- `SHA256SUMS` for all those files.

An additional Arch debug package is attached when an older tagged recipe produces one; current recipes disable separate debug packages because the release binary is stripped.

macOS/Windows release binaries and prerelease tags such as `v0.2.0-beta.1` are outside this workflow's current scope. Package distribution through official APT/DNF/Pacman repositories remains a separate process.

## One-command release

From a clean default branch synchronized with `origin`, run:

```sh
./scripts/release.py 0.2.1
```

Choose a stable version higher than the current package version and every existing stable tag/release. Push ordinary source changes first and add real notes under `CHANGELOG.md`'s Unreleased section. Python **3.11+**, Git, an authenticated GitHub CLI (`gh auth login`), Rust/Cargo with rustfmt and Clippy, Bash, `p4`, and `p4d` must be on PATH. Run on macOS or Linux; GitHub CI validates Windows. Git's commit identity and any configured signing credentials must already work.

The command:

1. Requires a clean index/worktree, complete Git history, the GitHub default branch with the correct upstream, matching origin/manifest repositories, and write permission. It fetches branch metadata and aborts if the checkout is ahead of or behind origin, a Git operation/release is unfinished, the version/tag/release is reused, or existing metadata/build examples disagree.
2. Requires the exact starting commit's CI and native package checks to pass. It prepares all changes in a temporary detached worktree, preserving the original checkout during validation.
3. Updates Cargo.toml, only p4ui's entry in Cargo.lock, Debian and RPM versions/changelogs, CHANGELOG.md (moving Unreleased notes into a dated release), build archive examples, and the next release-command examples. It verifies the locked dependency graph and runs formatting, Clippy, all Rust tests including isolated P4 tests, packaging/release helper tests, and both terminal tests. P4 tests use private temporary servers.
4. Rechecks local and remote state, commits the validated metadata, and creates an annotated managed tag. An atomic push updates the default branch and tag together with an explicit lease on the original branch SHA. Unsupported atomic pushes or concurrent updates abort. The original checkout is then advanced with a fast-forward only.
5. Waits for the tag's exact commit to pass all cross-platform, MSRV, P4, terminal and native-package jobs. Managed tags create a **draft**; they do not automatically publish it. The command downloads all assets, checks required package types and SHA256SUMS, and rechecks the branch, tag, release identity, workflow attempt and asset IDs/digests immediately before publishing. A rerun or asset replacement during verification aborts publication.

Check everything locally and remotely without making a release:

```sh
./scripts/release.py 0.2.1 --check
```

`--check` creates no commit, tag, GitHub release, or remote update. It uses and removes a temporary worktree and may populate ordinary Cargo caches. There are no flags to skip tests or override failed checks.

If the command stops before the atomic push, the original checkout is unchanged. After a successful push, the version commit/tag and any draft remain for inspection; failures do not delete or rewrite remote history. Fix transient CI failures by rerunning the failed tag workflow in GitHub, then finish the same candidate with:

```sh
./scripts/release.py VERSION --resume
```

Resume requires the same clean, synchronized release commit and an unchanged annotated managed tag. It rechecks GitHub jobs, downloaded assets and remote state before publishing. If newer work has reached the default branch, the candidate is out of date and cannot be published with this command. Inspect the failure and prepare a new version. A leftover `.git/p4ui-release.lock` blocks another release; inspect the recorded process/version before manually removing an interrupted run's lock.

A failed check, missing result, network error, or timeout exits nonzero. Managed drafts remain unpublished until final promotion. If the connection is interrupted during that final GitHub publication request, use `--resume` to determine whether publication completed; the command never assumes an unconfirmed request failed. `--timeout-minutes` controls the maximum wait per GitHub workflow (default 60, maximum 240).

## Manual tags

Ordinary annotated or lightweight `vX.Y.Z` tags without the managed marker retain automatic publication after the package and full CI gates pass. Their Cargo manifest/lockfile, Debian changelog and RPM version must already agree. The one-command path is recommended for creating new versions because it also performs preflight checks and verifies draft assets before publication.

Do not reuse or move a published version tag. The workflow checks that the tag, Cargo manifest/lockfile, Debian changelog and RPM version agree before starting builds. Every builder uses the same resolved source commit; the publishing job rechecks the remote tag before creating a release.

Only the publishing job has `contents: write`. It uses GitHub's built-in `GITHUB_TOKEN`; no personal access token or additional secret is needed for ordinary GitHub-hosted repository releases.

## Publish an existing tag

The existing `v0.1.0` tag predates the release automation. There is no need to move it. Run the current workflow from **main**, supplying the existing tag:

```sh
gh workflow run packages.yml --repo adamflott/p4ui --ref main -f release_tag=v0.1.0
```

Alternatively use **Actions → Packages → Run workflow**, select **main**, and enter `v0.1.0` in `release_tag`. This uses the current automation to rebuild the tagged source commit and publish its packages. Running the workflow on a branch with an empty tag input produces build artifacts only. Selecting a tag ref also requests a release of that tag.

Re-running the old tag's original workflow definition will not pick up newly added automation. Use the `main` + `release_tag` method for tags that predate it.

## Retries and validation

A failed build or CI check prevents publication. Release checks cover Linux/macOS/Windows, MSRV, both supported P4 client/server versions and terminal workflows. The reusable CI workflow checks out the validated source SHA, including for manual releases of older tags; optional history terminal tests run when present in that source. Branch CI runs separately; tag CI runs within Packages so publication depends on its result. A new release is first created as a draft; package uploads and checksums finish before it becomes public. If uploading fails, rerun the failed job/workflow to finish the draft. For an already public release, reruns download its original assets and verify completeness and every digest against its published `SHA256SUMS`. A complete, valid release succeeds without changing any remote files. Missing assets, missing checksums or corrupt packages fail the workflow; they are never reported as a successful release. Verification uses the original published checksums because rebuilt packages may have different timestamps. Recover missing original files from the original workflow artifacts, or publish a new version; do not replace released package bytes with a rebuild.

Local helper checks:

```sh
python3 tests/packaging.py
python3 scripts/validate-release.py --tag v0.1.0
```

A publishing-helper dry run validates downloaded package artifacts and computes checksums without making any GitHub requests:

```sh
python3 scripts/publish-release.py v0.1.0 FULL_TAG_COMMIT_SHA ./downloaded-artifacts \
  --repo adamflott/p4ui --dry-run
```

The directory must contain all five artifact types listed above, possibly in nested artifact folders. The helper rejects missing types, mismatched versions and duplicate asset names. CI automatically downloads these from the same workflow run.

To verify a downloaded release, download its package assets and `SHA256SUMS` into one directory, then run `sha256sum -c SHA256SUMS` on Linux or `shasum -a 256 -c SHA256SUMS` on macOS.
