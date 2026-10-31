# Contributing

Start with the README and docs/PARITY.md. Keep file operations explicit and respect Perforce's workspace, changelist, permission and flow rules. Do not introduce force flags without a concrete recovery story and dedicated tests.

Run formatting, Clippy and the normal tests. Backend changes should also run the isolated p4d suite; terminal lifecycle changes should run the PTY smoke test. Update the feature mapping when implementing a missing workflow. Keep Cargo.lock committed for reproducible application builds.

The backend (`src/p4.rs`) owns the CLI/protocol boundary. `model.rs` defines data/actions, `app.rs` manages events and serialized background work, `ui.rs` renders, and `main.rs` owns CLI/terminal/editor lifecycle. Report reproducible failures with operating system, `p4 -V`/`p4d -V`, a redacted client spec, exact actions and the error text. Never include passwords, tickets, trust files, proprietary source or unredacted connection details.

Please disclose security-sensitive problems privately to the repository maintainers. Use a private security report if the hosting repository enables that feature.
