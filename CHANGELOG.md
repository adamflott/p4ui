# Changelog

## Unreleased

- `--max-p4-processes N` caps concurrent P4 processes per instance (default 4; use 1 for serial requests). Independent Status and initial history reads run in parallel; mutations remain serialized.
- Cancelled background reads are discarded after rapid tab switches, and child processes are reaped before their concurrency slots are released.

## 0.2.0 — 2026-10-31

- Log and Shelves fill the visible terminal rows and load older changes as you scroll with the keyboard or mouse wheel. Enlarging the terminal fills the extra rows.
- History pages count actual changes and preserve selection and continuation cursors. Idle history does not poll the server; failed pages pause automatic loading without discarding cached changes.
- Tab switches cancel obsolete background reads while allowing active mutations to finish.
- `P4PORT` selects the Perforce server; an explicit `--port` takes precedence.
- `--tab log` opens history directly. `--log` emits a history page as JSON, and `--before-change` selects its inclusive upper bound.
- Launch paths containing redundant `.` components no longer cause spurious illegal-filename reconcile warnings. Nonfatal workspace warnings retain usable results and appear in help.
- Submitted-change and shelf summaries show up to 200 files; per-file revision diffs remain available through Enter.
- Tagged releases publish Debian/Ubuntu, Fedora RPM, and Arch Linux packages, vendored source, and SHA-256 checksums.

### Configuration

History now inherits the server's resource limits by default: `history_max_scan_rows = 0`. If an older configuration contains `50000`, change it to `0` to use normal server limits. Positive values remain available as optional stricter scan budgets.

`history_page_size` defaults to 100 and caps each batch; interactive loads use the visible row count. The legacy `history_change_window` key remains accepted but no longer affects paging.

## 0.1.0 — 2026-10-31

- Initial Perforce terminal interface with workspace status, diffs, changelists, shelves, submitted history, files, streams, workspaces, labels, and resolve workflows.
- MIT-licensed source, build instructions, native Linux package recipes, and isolated P4/P4D integration tests.
