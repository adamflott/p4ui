# Upstream local-build recipe. Official Fedora/openSUSE submissions may require
# unbundled Rust crates and distro-specific macros/license declarations.
%global debug_package %{nil}

Name:           p4ui
Version:        0.2.0
Release:        1%{?dist}
Summary:        Keyboard-driven terminal interface for Perforce
License:        MIT
URL:            https://github.com/adamflott/p4ui
Source0:        p4ui-%{version}.tar.gz
BuildRequires:  cargo >= 1.88
BuildRequires:  rust >= 1.88
BuildRequires:  gcc
BuildRequires:  python3

%description
A gitui-inspired terminal interface for Perforce workspace changes, diffs,
changelists, shelves, history, streams and resolve operations.
Install the separately distributed Perforce p4 command-line client and make
it available on PATH before running p4ui.

%prep
%autosetup

%build
export CARGO_NET_OFFLINE=true
cargo build --release --frozen

%check
export CARGO_NET_OFFLINE=true
cargo test --frozen

%install
install -Dm755 target/release/p4ui %{buildroot}%{_bindir}/p4ui

%files
%{_bindir}/p4ui
%license LICENSE
%license THIRD_PARTY_NOTICES
%doc README.md CONTRIBUTING.md docs/BUILDING.md docs/RELEASING.md docs/PARITY.md docs/TESTING.md

%changelog
* Sat Oct 31 2026 Adam Flott <adam@adamflott.com> - 0.2.0-1
- Viewport-sized history pages with scroll loading and responsive tab switches
- P4PORT connections and nonfatal workspace warning handling
- Tagged release packages, vendored source, and checksums

* Sat Oct 31 2026 Adam Flott <adam@adamflott.com> - 0.1.0-1
- Initial upstream package
