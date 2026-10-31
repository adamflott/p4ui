# Building and packaging p4ui

p4ui requires **Rust 1.88 or newer** (edition 2024) and a C linker. Check `rustc --version` and `cargo --version` before building. Cargo.lock is committed; use `--locked` to keep dependency versions fixed. No Perforce SDK, libgit2, OpenSSL headers, Node, or system service is needed to build p4ui. The separately installed `p4` command-line client is needed at runtime. `p4d` is only required for the optional integration suite.

The recipes below build from the project's repository:

```sh
git clone https://github.com/adamflott/p4ui.git
cd p4ui
cargo build --release --locked
./target/release/p4ui --version
cargo install --path . --locked
```

Cargo installs to `~/.cargo/bin` by default; include that directory on PATH. For a particular release, check out its tag or commit before building. GitHub source ZIP/tar downloads do not include vendored crates; use the source-archive helper below for offline native-package builds.

## macOS (Apple Silicon and Intel)

1. Install Apple's command-line developer tools and finish the installer:

   ```sh
   xcode-select --install
   xcrun --find clang
   ```

2. Install a Rust toolchain. With an existing Homebrew installation:

   ```sh
   brew install rust
   rustc --version
   cargo --version
   ```

   Alternatively use the [official rustup installer](https://www.rust-lang.org/tools/install/). For an existing rustup installation, run `rustup update stable`. Choose one toolchain source and ensure PATH selects it; do not mix Homebrew and rustup toolchains accidentally.

3. Clone and build using the commands at the top of this guide. Build natively on Apple Silicon for `aarch64-apple-darwin`, or on an Intel Mac for `x86_64-apple-darwin`. The output is a terminal executable; no app bundle or background daemon is installed.

4. Install the Perforce client if it is not already available. Homebrew's [Perforce cask](https://formulae.brew.sh/cask/perforce) provides `p4` and `p4d`:

   ```sh
   brew install --cask perforce
   p4 -V
   cd /path/to/your/perforce/workspace
   p4 login                    # if your server requires it
   p4ui --check
   p4ui
   ```

The macOS ARM64 release build and terminal workflow are exercised during development. CI also builds on macOS. Cross-compiling or making a universal binary is optional; native builds are sufficient for ordinary use.

## Linux build prerequisites

Install the compiler/linker and Git using the package manager for your distribution:

| Distribution family | Build prerequisites |
| --- | --- |
| Debian, Ubuntu, Mint, Pop!_OS | `sudo apt update && sudo apt install build-essential git curl ca-certificates` |
| Fedora, RHEL, Rocky, AlmaLinux | `sudo dnf install gcc git curl ca-certificates` |
| openSUSE Tumbleweed / Leap | `sudo zypper install gcc git curl ca-certificates` |
| Arch, Manjaro | `sudo pacman -S --needed base-devel git rust` |
| Alpine | `sudo apk add build-base git rust cargo ca-certificates` |
| Gentoo | `sudo emerge --ask dev-vcs/git dev-lang/rust` (using the system compiler/linker) |

Use your distribution's Rust/Cargo packages if they provide Rust 1.88 or later. On Debian/Ubuntu, Fedora/RHEL, and openSUSE, the [official rustup installer](https://www.rust-lang.org/tools/install/) is an alternative when the packaged toolchain is older. Then build with the common commands above. On Alpine, prefer the distro toolchain so it targets musl correctly. Do not run Cargo or makepkg as root.

Nix users can enter a development shell with:

```sh
nix shell nixpkgs#rustc nixpkgs#cargo nixpkgs#gcc nixpkgs#git
cargo build --release --locked
```

Install the proprietary Perforce `p4` client separately from [Perforce's downloads](https://www.perforce.com/downloads/helix-command-line-client-p4) or its supported package repository for your OS/architecture. The client is not bundled with p4ui. Its package name varies across repositories. Verify `p4 -V`; merely building/installing p4ui does not create a workspace or log in.

## Native Linux packages

The repository supplies these **upstream local-build recipes**:

| Format | Recipe | Distribution families |
| --- | --- | --- |
| `.deb` | [`debian/`](../debian/control) | Debian / Ubuntu and derivatives |
| `.rpm` | [`packaging/rpm/p4ui.spec`](../packaging/rpm/p4ui.spec) | Fedora / RHEL derivatives / openSUSE; build on your target distribution |
| `.pkg.tar.zst` | [`packaging/arch/PKGBUILD.in`](../packaging/arch/PKGBUILD.in) | Arch / Manjaro; generator supplies a real source checksum |

They install `/usr/bin/p4ui`, documentation and license notices. The p4 client remains an external runtime prerequisite because it may be installed from a vendor repo, a local binary, or a differently named package. Installation does not execute p4 commands, alter credentials, or start services.

The [Packages workflow](https://github.com/adamflott/p4ui/actions/workflows/packages.yml) builds Debian/Ubuntu, Fedora RPM and Arch packages on Linux and uploads them as workflow artifacts. Download artifacts from a successful run; unpack the artifact ZIP before installing. They are unsigned build artifacts, not an APT/DNF/Pacman repository or an assertion of official distro acceptance. Build an RPM on RHEL/openSUSE for those systems instead of assuming a Fedora-built binary has a compatible glibc baseline. Native recipes discover ELF dependencies where supported; they cannot make a binary compatible with an older libc.

Official archive submissions may require distro maintainers, signing, unbundled crates, complete per-crate license metadata, clean-chroot builds and policy-specific Rust macros. These files are a starting point for that process; no official package-repository submission has been made. Alpine, Gentoo and Nix currently use the source-build instructions above rather than native package recipes.

### Make an offline source archive

Archive assembly requires Git, Cargo, Python **3.11+**, tar and network access to fetch the locked crates (or an already populated Cargo cache). Commit your changes first: the helper packages **HEAD**, including the recipes from that exact revision, rather than arbitrary working-tree files.

```sh
./scripts/vendor-source.sh
# dist/p4ui-0.1.0.tar.gz
```

For a machine with a separate Python 3.11 executable:

```sh
PYTHON=python3.11 ./scripts/vendor-source.sh
```

The archive contains Cargo.lock, vendored crates, Cargo's source-replacement configuration and generated `THIRD_PARTY_NOTICES`. The helper validates Cargo metadata against an empty Cargo home with `--frozen`. Native package builds then use `--frozen` / offline mode: no crate downloads are permitted during their build/test stages. Python 3 is needed by normal process tests; the already assembled archive does not need Python 3.11 just to compile.

Use `CARGO_NET_OFFLINE=true ./scripts/vendor-source.sh` when assembling from a fully cached lockfile without network. Archives contain optional dependencies for all supported platforms. Preserve the vendor licenses/notices when redistributing.

### Debian / Ubuntu

Install the native build tooling:

```sh
sudo apt install build-essential debhelper dpkg-dev fakeroot python3
```

With an archive assembled as above:

```sh
mkdir -p dist/deb-build
tar -xzf dist/p4ui-0.1.0.tar.gz -C dist/deb-build
cd dist/deb-build/p4ui-0.1.0
dpkg-buildpackage -us -uc -b
# Install the .deb from the parent directory:
sudo apt install ../p4ui_0.1.0-1_*.deb
```

`debian/control` declares Cargo/Rust >= 1.88. If those are installed through **rustup** rather than APT, dpkg cannot see them. After installing all the other Build-Depends above and checking the toolchain version, use `dpkg-buildpackage -d -us -uc -b`; `-d` disables package-database build-dependency checks, not tests or Cargo's offline lockfile checks. Do not use `-d` as a substitute for installing missing tools. The build's runtime library dependencies are populated by debhelper.

These commands make a binary package; making a Debian source upload additionally requires a correctly named orig tarball and distro-policy review. They do not upload to Debian, Ubuntu or a PPA.

### Fedora / RHEL derivatives / openSUSE

Install the target distribution's native build dependencies, including Rust >= 1.88:

```sh
# Fedora / RHEL derivatives (repository availability of the required Rust varies):
sudo dnf install gcc git rust cargo rpm-build python3

# openSUSE alternative:
sudo zypper install gcc git rust cargo rpm-build python3
```

If the distro Rust is too old, select a newer supported toolchain before building. As with dpkg, an externally installed toolchain is not visible in RPM's package database. `rpmbuild --nodeps` is only appropriate after verifying every BuildRequires yourself.

```sh
mkdir -p dist/rpmbuild/{BUILD,BUILDROOT,RPMS,SOURCES,SPECS,SRPMS}
cp dist/p4ui-0.1.0.tar.gz dist/rpmbuild/SOURCES/
cp packaging/rpm/p4ui.spec dist/rpmbuild/SPECS/
rpmbuild -ba --define "_topdir $PWD/dist/rpmbuild" dist/rpmbuild/SPECS/p4ui.spec

# Fedora / RHEL install:
sudo dnf install dist/rpmbuild/RPMS/*/p4ui-0.1.0-1*.rpm
# openSUSE alternative:
# sudo zypper install dist/rpmbuild/RPMS/*/p4ui-0.1.0-1*.rpm
```

`-ba` creates both a binary RPM and a source RPM. Neither is uploaded or signed automatically. Rust dependencies remain vendored for this upstream recipe; downstream Fedora/openSUSE maintainers may need to adapt it to their crate and license policies.

### Arch / Manjaro

```sh
sudo pacman -S --needed base-devel rust git python
python3 scripts/prepare-arch.py dist/p4ui-0.1.0.tar.gz packaging/arch/build
cd packaging/arch/build
makepkg -si
```

Run makepkg as a regular user. The generator copies the archive, fills in its version and SHA-256, and refuses to overwrite an existing build directory. There is no `SKIP` checksum in the generated PKGBUILD. Rebuilding the archive requires preparing a new directory. This is a local package; no AUR or official-repository listing is implied.

## Tests and troubleshooting

```sh
cargo fmt --all -- --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked
# Optional: requires p4 and p4d; uses isolated local servers:
cargo test --locked --test perforce -- --ignored
# Optional: Python 3 and a Unix pseudo-terminal:
cargo build --locked
python3 tests/ui_smoke.py
```

Native package builds run the normal tests; server tests stay ignored unless explicitly requested. Packaging CI checks installed binaries with `p4ui --version` and inspects package metadata. See [TESTING.md](TESTING.md) for application test coverage.

If Rust reports unsupported edition/features, update to >= 1.88. If the linker is missing, install the platform compiler tools above. If an offline package build requests a crate, rebuild the vendored archive from the matching committed lockfile rather than removing `--frozen`. If p4ui reports an unknown client or authentication failure, verify `p4 info`, `p4 login`, `P4CLIENT`, and the current workspace directory.

References: [Cargo install](https://doc.rust-lang.org/cargo/commands/cargo-install.html), [Debian maintainer guide](https://www.debian.org/doc/manuals/debmake-doc/), [Arch PKGBUILD reference](https://man.archlinux.org/man/PKGBUILD.5.en), [Homebrew Rust](https://formulae.brew.sh/formula/rust).
