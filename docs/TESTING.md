# Validation

## Automated coverage

`cargo test --locked` runs parser, configuration, confirmation/input, terminal rendering, and process-boundary tests. UI rendering is exercised at 40×10 (resize notice), 60×14, 100×30, and 160×50 with Unicode and every popup/tab. Process tests check hanging commands, excess output and nonzero exits with otherwise valid JSON.

`cargo test --locked --test perforce -- --ignored` runs the real client/server suite. Each test owns a temporary database, workspace, configuration, trust and ticket paths. The suite covers:

- Add/reconcile, offline edit/delete discovery, file diffs, default and numbered submits.
- Read committed/shelved content, per-change file drilldown, annotation and revision/integration logs.
- Changelist description updates with form-injection text, reopen, empty-change deletion.
- Shelve, protected existing shelves, revert, unshelve and shelf deletion.
- Metadata-only revert preserving content and destructive revert restoring content.
- Spaces, Unicode, reserved Perforce filename characters, binary files and symlinks.
- Move/rename, lock/unlock and sync.
- Client selection, label/stream listing and current-directory/ignore scoping.
- Default / dot-suffixed launch directories without spurious illegal-filename warnings; lexical cleanup preserves symlink spelling.
- Invalid workspaces, output limits and permission failures.
- Cursor paging through sparse scoped history without duplicate/missing changes, seeking across large server-counter gaps and gaps between scoped changes.
- Deleted-file history and result-count paging independent of changelist-number gaps.
- Scoped shelf pagination, including new/deleted shelves while browsing.
- Optional scan-budget clamping, server-limit inheritance, upper-bound cursors, non-retrying resource failures and retained fatal permission errors.
- Viewport-sized initial loads, keyboard/wheel paging, resize fills, selection/drilldown preservation, metadata reuse, idle stability and paused paging after failures.
- Cancelling a slow Status read on a tab switch without cancelling an active mutation.
- CLI connections using only P4PORT for the server address, explicit-port precedence, empty-env/config fallback and environment-driven JSON status.
- Concurrent edits from two workspaces, failed submits, safe resolve preserving conflicts, explicit resolution and successful resubmit.
- Dirty-workspace switch rejection, stream creation/deletion, branch population, parent merge and copy promotion under normal stream flow rules.

`cargo build --locked && python3 tests/ui_smoke.py` drives the real executable through a Unix pseudo-terminal. It reconciles, invokes an external editor, accepts a multiline submit description, submits, checks help/history, exits and checks that termios settings were restored. It also checks the noninteractive JSON status output. This complements backend tests by exercising event decoding and terminal lifecycle.

To regenerate the sample screenshot: `cargo run --example snapshot > docs/status.svg`. This is deterministic sample data, not a live server capture.

## Failure semantics

A nonzero `p4` exit or severity-3 structured error is never presented as success. No-result warnings are distinguished from permissions/authentication errors. Workspace tab listings retain usable records alongside nonfatal warnings; the status bar flags incomplete results and `?` shows the full messages. Mutating commands and clean-workspace preflights remain strict about warnings. A command can have partially succeeded before failure. Refresh and inspect pending changes before retrying. Default submit failures may create numbered pending changelists.

The CLI runs without a shell. Only the explicit editor command is parsed into an executable/arguments; even there, shell expressions are not evaluated. The terminal is restored on regular errors, panics and handled termination signals. Active mutations are allowed to finish before a normal exit/handled termination. A timeout kills the local client; it cannot guarantee server rollback. Read-only child requests are cancelled and reaped when the worker shuts down.

## Manual release checks

Use a disposable workspace to exercise server authentication, network timeout/reconnect, large/binary file handling, resize, terminal suspend behavior, configured editor success/failure, each theme and remapped key. Verify client root spelling, ignore files, line endings, permissions and stream flow rules on the target operating system. Test backups/recovery independently of this UI.

`cargo build --locked && python3 tests/history_ui.py` drives the real executable through a PTY with a read-only fake P4 CLI. It verifies the actual terminal row count, resize fills, PageDown and SGR mouse-wheel decoding, exact continuation cursors, idle stability and terminal restoration. No real P4 server is contacted.
