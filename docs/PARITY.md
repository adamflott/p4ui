# Gitui workflow mapping

The goal is a familiar daily SCM workflow with honest Perforce semantics. This table records implemented behavior and gaps; it is not a blanket parity claim.

| gitui workflow | p4ui implementation | Difference / remaining work |
| --- | --- | --- |
| Status, staged/unstaged panes | Opened and offline/untracked panes, active changelist | Perforce open actions are not an index snapshot; subsequent file edits are included in submit |
| Stage/unstage files, stage all | Reconcile selected/all; reopen to changelist; metadata revert keeping content | No Git index |
| Stage/revert selected lines or hunks | Whole-file actions; external editor for selective edits | No built-in hunk/line selection or partial-content submit |
| Commit | Default or numbered changelist submit, multiline description, confirmation | Server submit triggers run normally |
| Amend | Edit pending descriptions | Submitted changes are immutable revisions; history rewriting is not mapped |
| Stash inspect/save/apply/drop | Shelf inspect/create/unshelve/delete | Shelves need numbered changelists; no force-update, automatic revert or pop shortcut |
| Browse/search log and committed diffs | Viewport-sized history pages with scroll loading, loaded-row filtering, bounded change summaries and per-file revision diffs | No server-wide full-text search; scrolling/+ fetches older pages; summaries/file drilldown show at most 200 files |
| Browse files and blame | Depot files, revision previews, annotate, file/integration history, editor | Flat filtered list, no collapsible directory tree or syntax highlighting |
| Branches | Stream list/spec inspection, create/delete/switch, merge/copy | Existing stream depot/workspace needed; stream flow rules apply; no branch-spec UI or stream-path rename |
| Fetch/push | Sync/submit | Centralized depot; no Git remotes or push queue |
| Tags | Read-only label list/spec inspection | Label creation/tagging/sync-to-label remains in p4 CLI |
| Conflicts | Resolve queue and automatic/safe/yours/theirs resolve | Interactive three-way conflict editing remains external |
| Keyboard/help/theme | Arrow and Vim navigation, contextual hints/help, character overrides, three themes | Not compatible with gitui RON configs; no arbitrary theme palette file |
| Async SCM | Worker notifications, deferred/coalesced reads, bounded read caches, viewport rendering, timeouts and cancellation; independent reads share the P4 process cap | Mutations remain serialized; background results wake the UI without constant redraws |
| Workspace selection | Session-only local workspace selection, root change; full current client-spec editing through the configured editor | Workspace creation remains in CLI; editing a spec does not sync files or change the working directory |
| Submodules, signed commits | No direct mapping | Perforce views/import paths and server permissions/triggers supply different capabilities |

## Release scope and validation gaps

The code has exercised the included workflows against local P4/P4D 2026.1 on macOS ARM64. CI is configured for formatting, Clippy, unit/process tests, Linux/macOS/Windows builds, and real-server Linux tests. CI configuration is not proof that those platforms have already passed remotely.

Before advertising a stable production release, exercise older supported servers, Unicode-mode servers, SSL/SSO/MFA, distributed edge/commit topologies, restrictive production protections, huge workspaces, network partitions during submit, Windows path/filetype behavior, and real user terminals. Large-file previews are bounded; large status scans can hit the configured output limit. Non-UTF-8 filenames are not round-tripped by Perforce's JSON protocol and are outside the supported filename set. Graph depots are outside the current scope.

p4ui deliberately uses ordinary Perforce protection and workspace rules. It does not disable authentication, force sync/clobber, auto-accept trust fingerprints, or alter your workspace/server configuration during startup.
