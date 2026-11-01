# Automated releases from Git tags

Pushing a **stable `vX.Y.Z` tag** runs `.github/workflows/packages.yml`. After all three Linux package builds, installation checks, and the full CI suite succeed for the validated source commit, the workflow publishes a GitHub Release for that exact tag. Pushing a branch builds artifacts only.

The release includes:

- Debian/Ubuntu `.deb` (x86_64).
- Fedora `.rpm` (x86_64) and source RPM.
- Arch `.pkg.tar.zst` (x86_64).
- `p4ui-X.Y.Z.tar.gz`, with the locked vendored Rust dependencies.
- `SHA256SUMS` for all those files.

An additional Arch debug package is attached when an older tagged recipe produces one; current recipes disable separate debug packages because the release binary is stripped.

macOS/Windows release binaries and prerelease tags such as `v0.2.0-beta.1` are outside this workflow's current scope. Package distribution through official APT/DNF/Pacman repositories remains a separate process.

## Publish a new version

For example, to release `0.1.1`:

1. Set the package version to `0.1.1` in `Cargo.toml`, run `cargo check` to update the local package entry in `Cargo.lock`, add a `0.1.1-1` entry to `debian/changelog`, and set `Version: 0.1.1` in `packaging/rpm/p4ui.spec`. Keep the Rust MSRV unchanged unless the code needs a newer toolchain.
2. Commit the release changes and push them. Review the CI results.
3. Create an annotated tag on the commit you want to ship:

   ```sh
   git tag -a v0.1.1 -m "p4ui 0.1.1"
   git push origin v0.1.1
   ```

4. Follow the **Packages** run under [GitHub Actions](https://github.com/adamflott/p4ui/actions/workflows/packages.yml). The release appears automatically under [Releases](https://github.com/adamflott/p4ui/releases) after every package and CI job passes.

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

A failed build or CI check prevents publication. Release checks cover Linux/macOS/Windows, MSRV, both supported P4 client/server versions and terminal workflows. The reusable CI workflow checks out the validated source SHA, including for manual releases of older tags; optional history terminal tests run when present in that source. Branch CI runs separately; tag CI runs within Packages so publication depends on its result. A new release is first created as a draft; package uploads and checksums finish before it becomes public. If uploading fails, rerun the failed job/workflow to finish the draft. Already published releases are left unchanged on reruns, including their asset bytes. Publish a new version to replace released packages.

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
