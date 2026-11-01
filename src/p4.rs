//! The CLI is our protocol boundary. No command is evaluated by a shell.
use crate::{config::Config, model::*, process_limit::ProcessLimit};
use anyhow::{Context, Result, bail, ensure};
use serde_json::Value;
use std::{
    collections::HashSet,
    io::{Read, Seek, SeekFrom, Write},
    num::NonZeroUsize,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

#[derive(Debug, Clone)]
pub struct Connection {
    pub executable: PathBuf,
    pub directory: PathBuf,
    pub port: Option<String>,
    pub user: Option<String>,
    pub client: Option<String>,
    pub charset: Option<String>,
    pub timeout: Duration,
    pub max_output: u64,
    pub history_page_size: usize,
    pub history_max_scan_rows: u64,
    /// Used by isolated integration fixtures; normal operation inherits P4 settings.
    pub environment: Vec<(String, String)>,
    pub cancelled: Arc<AtomicBool>,
    processes: Arc<ProcessLimit>,
}

/// Reap a child on every exit path, before its concurrency permit is released.
struct RunningP4(Child);
impl Drop for RunningP4 {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn join_read<T>(task: thread::ScopedJoinHandle<'_, Result<T>>) -> Result<T> {
    task.join()
        .map_err(|_| anyhow::anyhow!("Perforce read task panicked"))?
}

pub(crate) struct HistoryContext {
    info: Info,
    scan_limit: Option<u64>,
    group_scan_limit: Option<u64>,
    max_results: Option<u64>,
    warnings: Vec<String>,
}

impl Connection {
    pub fn new(directory: PathBuf, config: &Config) -> Self {
        Self {
            executable: "p4".into(),
            // Path components remove redundant `.` without resolving symlinks.
            // Older p4 clients otherwise include `/./` in reconcile filenames and
            // reject ordinary names as containing illegal characters.
            directory: directory.components().collect(),
            port: None,
            user: None,
            client: None,
            charset: None,
            timeout: Duration::from_secs(config.command_timeout_seconds),
            max_output: config.max_output_mb as u64 * 1024 * 1024,
            history_page_size: config.history_page_size,
            history_max_scan_rows: config.history_max_scan_rows,
            environment: Vec::new(),
            cancelled: Arc::new(AtomicBool::new(false)),
            processes: Arc::new(ProcessLimit::default()),
        }
    }
    /// Configure the process limit shared by this connection and all its clones.
    pub fn set_max_processes(&self, maximum: NonZeroUsize) {
        self.processes.set_maximum(maximum);
    }
    fn command(&self, json: bool) -> Command {
        let mut c = Command::new(&self.executable);
        c.current_dir(&self.directory)
            .envs(self.environment.iter().cloned());
        // A user's external diff program must never be launched by a background request.
        c.env_remove("P4DIFF").env_remove("P4MERGE");
        for (flag, value) in [
            ("-p", &self.port),
            ("-u", &self.user),
            ("-c", &self.client),
            ("-C", &self.charset),
        ] {
            if let Some(value) = value {
                c.arg(flag).arg(value);
            }
        }
        c.arg("-d").arg(&self.directory);
        c.arg(format!("-vnet.maxwait={}", self.timeout.as_secs().max(1)));
        if json {
            c.args(["-ztag", "-Mj"]);
        }
        c
    }
    /// Tempfiles drain output without pipe deadlocks and bound memory consumption.
    pub fn execute(&self, args: &[&str], form: Option<&str>, json: bool) -> Result<String> {
        let _permit = self.processes.acquire(&self.cancelled)?;
        let mut out = tempfile::tempfile()?;
        let mut err = tempfile::tempfile()?;
        let mut input = tempfile::tempfile()?;
        if let Some(form) = form {
            input.write_all(form.as_bytes())?;
            input.seek(SeekFrom::Start(0))?;
        }
        let mut child = RunningP4(
            self.command(json)
                .args(args)
                .stdin(Stdio::from(input))
                .stdout(out.try_clone()?)
                .stderr(err.try_clone()?)
                .spawn()
                .with_context(|| {
                    format!(
                        "Could not start {}. Install the Perforce command-line client.",
                        self.executable.display()
                    )
                })?,
        );
        let start = Instant::now();
        let status = loop {
            if let Some(status) = child.0.try_wait()? {
                break status;
            }
            let too_large = out.metadata()?.len() + err.metadata()?.len() > self.max_output;
            if too_large
                || start.elapsed() >= self.timeout
                || self.cancelled.load(Ordering::Relaxed)
            {
                let _ = child.0.kill();
                let _ = child.0.wait();
                bail!(
                    "p4 {} stopped: {}. A mutating command may have completed on the server; refresh and inspect before retrying.",
                    args.first().unwrap_or(&""),
                    if too_large {
                        "output limit exceeded"
                    } else if self.cancelled.load(Ordering::Relaxed) {
                        "cancelled"
                    } else {
                        "command timed out"
                    }
                );
            }
            thread::sleep(Duration::from_millis(15));
        };
        ensure!(
            out.metadata()?.len() + err.metadata()?.len() <= self.max_output,
            "p4 output limit exceeded; narrow your scope or raise max_output_mb"
        );
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        out.rewind()?;
        err.rewind()?;
        out.read_to_end(&mut stdout)?;
        err.read_to_end(&mut stderr)?;
        let stdout = String::from_utf8_lossy(&stdout).into_owned();
        let stderr = String::from_utf8_lossy(&stderr);
        if !status.success() && !json {
            bail!("p4 {}: {}{}", args.first().unwrap_or(&""), stdout, stderr);
        }
        if !status.success() && (stdout.trim().is_empty() || !stderr.trim().is_empty()) {
            bail!("p4 {}: {}{}", args.first().unwrap_or(&""), stdout, stderr);
        }
        if !status.success() {
            if json {
                parse_records(&stdout)?;
            }
            bail!("p4 exited with {status}: {stdout}{stderr}");
        }
        Ok(stdout)
    }
    pub fn records(&self, args: &[&str]) -> Result<Vec<Value>> {
        let text = self.execute(args, None, true)?;
        parse_records(&text)
    }
    fn listing_records(&self, args: &[&str], warnings: &mut Vec<String>) -> Result<Vec<Value>> {
        let text = self.execute(args, None, true)?;
        parse_listing_records(&text, warnings)
    }
    pub fn text(&self, args: &[&str]) -> Result<String> {
        self.execute(args, None, false)
    }
    fn raw_info(&self) -> Result<Value> {
        self.records(&["info"])?
            .into_iter()
            .next()
            .context("p4 info returned no connection information")
    }
    pub fn info(&self) -> Result<Info> {
        let r = self.raw_info()?;
        let info = Info {
            user: field(&r, "userName"),
            client: field(&r, "clientName"),
            root: field(&r, "clientRoot"),
            server: field(&r, "serverAddress"),
            stream: field(&r, "clientStream"),
        };
        ensure!(
            !info.client.is_empty() && info.client != "*unknown*" && !info.root.is_empty(),
            "No configured workspace. Set P4CLIENT, use --client, or create a workspace with p4 client first. Run p4 login if authentication is required."
        );
        Ok(info)
    }
    fn pin_identity(&mut self) -> Result<()> {
        // Resolve settings in the original directory before another workspace's
        // P4CONFIG can change them. `info.serverAddress` may name a server behind
        // a broker/proxy and does not retain SSL or rsh transport information.
        let settings = self
            .text(&["set", "-q", "P4PORT", "P4CHARSET"])
            .context("Resolving connection settings before switching workspaces")?;
        let setting = |name| {
            settings
                .lines()
                .filter_map(|line| line.split_once('='))
                .find(|(key, value)| *key == name && !value.is_empty())
                .map(|(_, value)| value.to_owned())
        };
        self.port
            .get_or_insert_with(|| setting("P4PORT").unwrap_or_else(|| "perforce:1666".into()));
        let info = self.raw_info()?;
        // Preserve server/locale autodetection for Unicode servers. Explicit
        // `auto` is rejected by non-Unicode servers on commands such as client.
        let default_charset = if field(&info, "unicode") == "enabled" {
            "auto"
        } else {
            "none"
        };
        self.charset
            .get_or_insert_with(|| setting("P4CHARSET").unwrap_or_else(|| default_charset.into()));
        if self.user.is_none() {
            // Unlike `p4 set`, info also resolves the default OS username.
            let user = field(&info, "userName");
            ensure!(!user.is_empty(), "p4 info returned no connection user");
            self.user = Some(user);
        }
        Ok(())
    }
    pub fn load(&self, tab: Tab, limit: usize) -> Result<View> {
        if matches!(tab, Tab::Status | Tab::Resolve) {
            return self.workspace_status(tab);
        }
        let info = self.info()?;
        let mut view = View {
            info,
            ..View::default()
        };
        let scope = "...";
        let n = limit.to_string();
        match tab {
            Tab::Status | Tab::Resolve => unreachable!("workspace status is handled above"),
            Tab::Log | Tab::Shelves => {
                let context = self.history_context_with_info(view.info);
                return self.history_page(tab, None, limit, &context);
            }
            Tab::Changes => {
                let records = self.listing_records(
                    &[
                        "changes",
                        "-l",
                        "-m",
                        &n,
                        "-s",
                        "pending",
                        "-c",
                        &view.info.client,
                        "-u",
                        &view.info.user,
                    ],
                    &mut view.warnings,
                )?;
                view.items.push(Item::Change {
                    id: "default".into(),
                    description: "Default changelist".into(),
                    user: view.info.user.clone(),
                    client: view.info.client.clone(),
                    status: "pending".into(),
                });
                view.items.extend(
                    records
                        .iter()
                        .filter(|r| r.get("change").is_some())
                        .map(change_item),
                );
                view.limit_reached = records.len() >= limit;
            }
            Tab::Files => {
                let records =
                    self.listing_records(&["fstat", "-m", &n, scope], &mut view.warnings)?;
                view.limit_reached = records.len() >= limit;
                for r in records.iter().filter(|r| {
                    r.get("depotFile").is_some() && !field(r, "headAction").contains("delete")
                }) {
                    view.items
                        .push(Item::File(file(r, r.get("action").is_some())));
                }
            }
            Tab::Streams => {
                let records = self.listing_records(&["streams", "-m", &n], &mut view.warnings)?;
                view.limit_reached = records.len() >= limit;
                for r in records.iter().filter(|r| r.get("Stream").is_some()) {
                    view.items.push(Item::Stream {
                        path: field(r, "Stream"),
                        parent: field(r, "Parent"),
                        kind: field(r, "Type"),
                    });
                }
            }
            Tab::Workspaces => {
                let records = self.listing_records(
                    &["clients", "-u", &view.info.user, "-m", &n],
                    &mut view.warnings,
                )?;
                view.limit_reached = records.len() >= limit;
                for r in records.iter().filter(|r| r.get("client").is_some()) {
                    view.items.push(Item::Workspace {
                        name: field(r, "client"),
                        root: field(r, "Root"),
                        stream: field(r, "Stream"),
                    });
                }
            }
            Tab::Labels => {
                let records = self.listing_records(&["labels", "-m", &n], &mut view.warnings)?;
                view.limit_reached = records.len() >= limit;
                for r in records.iter().filter(|r| r.get("label").is_some()) {
                    view.items.push(Item::Label {
                        name: field(r, "label"),
                        description: field(r, "Description"),
                    });
                }
            }
        }
        Ok(view)
    }
    fn workspace_status(&self, tab: Tab) -> Result<View> {
        thread::scope(|scope| {
            let info = scope.spawn(|| self.info());
            let opened = scope.spawn(|| {
                let mut warnings = Vec::new();
                let rows = self.listing_records(&["fstat", "-Ro", "-Or", "..."], &mut warnings)?;
                Ok((rows, warnings))
            });
            let offline = (tab == Tab::Status).then(|| {
                scope.spawn(|| {
                    let mut warnings = Vec::new();
                    let rows =
                        self.listing_records(&["reconcile", "-n", "-f", "..."], &mut warnings)?;
                    Ok((rows, warnings))
                })
            });
            // Join every task before propagating an error, including a panic
            // converted by join_read, so no scoped task is left unobserved.
            let info = join_read(info);
            let opened = join_read(opened);
            let offline = offline.map(join_read).transpose();
            let info = info?;
            let (opened, warnings) = opened?;
            let mut view = View {
                info,
                warnings,
                ..Default::default()
            };
            for record in opened
                .iter()
                .filter(|record| record.get("depotFile").is_some())
            {
                let file = file(record, true);
                if tab == Tab::Status || file.unresolved {
                    view.items.push(Item::File(file));
                }
            }
            if let Some((records, warnings)) = offline? {
                view.warnings.extend(warnings);
                let known: HashSet<String> = view
                    .items
                    .iter()
                    .map(|item| item.key().to_owned())
                    .collect();
                for record in records {
                    if record.get("depotFile").is_some()
                        && !known.contains(&field(&record, "depotFile"))
                    {
                        view.items.push(Item::File(file(&record, false)));
                    }
                }
                view.items.sort_by_key(|item| match item {
                    Item::File(file) => (!file.opened, file.change.clone(), file.depot.clone()),
                    _ => (false, String::new(), String::new()),
                });
            }
            Ok(view)
        })
    }
    pub fn load_history(&self, tab: Tab, cursor: Option<HistoryCursor>) -> Result<View> {
        Ok(self.start_history(tab, cursor, self.history_page_size)?.1)
    }
    pub(crate) fn start_history(
        &self,
        tab: Tab,
        cursor: Option<HistoryCursor>,
        size: usize,
    ) -> Result<(HistoryContext, View)> {
        if self.history_max_scan_rows != 0 {
            // Group limits depend on the authenticated user returned by info.
            let context = self.history_context_with_info(self.info()?);
            let view = self.history_page(tab, cursor, size, &context)?;
            return Ok((context, view));
        }
        thread::scope(|scope| {
            let info = scope.spawn(|| self.info());
            let context = self.history_context_with_info(Info::default());
            let view = self.history_page(tab, cursor, size, &context);
            let info = join_read(info);
            let mut view = view?;
            let context = self.history_context_with_info(info?);
            view.info = context.info.clone();
            Ok((context, view))
        })
    }
    fn history_context_with_info(&self, info: Info) -> HistoryContext {
        let mut context = HistoryContext {
            info,
            scan_limit: None,
            group_scan_limit: None,
            max_results: None,
            warnings: Vec::new(),
        };
        // By default, inherit the server's limits exactly as the p4 CLI does.
        // Reading group metadata is only necessary for an optional stricter cap:
        // per-command overrides can otherwise increase an administrator's limit.
        if self.history_max_scan_rows == 0 {
            return context;
        }
        let groups = self.records(&["groups", "-i", "-v", "-m", "1000", &context.info.user]);
        match groups {
            Ok(groups)
                if groups.len() < 1000
                    && groups.iter().all(|r| {
                        ["maxScanRows", "maxResults"].iter().all(|key| {
                            let value = field(r, key);
                            value.parse::<u64>().is_ok()
                                || matches!(
                                    value.to_ascii_lowercase().as_str(),
                                    "unset" | "unlimited" | "-1"
                                )
                        })
                    }) =>
            {
                let scan = group_limit(&groups, "maxScanRows");
                context.scan_limit = Some(
                    scan.map(|n| n.min(self.history_max_scan_rows))
                        .unwrap_or(self.history_max_scan_rows),
                );
                context.group_scan_limit = scan;
                context.max_results = group_limit(&groups, "maxResults");
            }
            _ => context.warnings.push(
                "Could not determine complete group limits; leaving server limits unchanged."
                    .into(),
            ),
        }
        context
    }
    pub(crate) fn history_page(
        &self,
        tab: Tab,
        cursor: Option<HistoryCursor>,
        requested: usize,
        context: &HistoryContext,
    ) -> Result<View> {
        ensure!(
            tab.is_history(),
            "History paging is only available in Log and Shelves"
        );
        let mut view = View {
            info: context.info.clone(),
            warnings: context.warnings.clone(),
            ..Default::default()
        };
        let page_size = requested
            .clamp(1, self.history_page_size.clamp(1, 200))
            .min(context.max_results.unwrap_or(200).clamp(1, 200) as usize);
        let scope = cursor
            .filter(|_| tab == Tab::Log)
            .map(|cursor| format!("...@{}", cursor.before))
            .unwrap_or_else(|| "...".into());
        let prefix = context
            .scan_limit
            .map(|limit| format!("-zmaxScanRows={limit}"));
        // -m counts actual changes, regardless of gaps in the server counter.
        // Submitted history continues immediately before the previous page.
        // P4 ignores revision bounds on file-scoped shelf listings. Shelves
        // therefore refetch a bounded prefix and retain an ID frontier as well
        // as an offset, so new/deleted shelves cannot duplicate or skip rows.
        let mut maximum = page_size
            .saturating_add(if tab == Tab::Shelves {
                cursor.and_then(|cursor| cursor.shelf_offset).unwrap_or(0)
            } else {
                0
            })
            .min(10000);
        let records = loop {
            let count = maximum.to_string();
            let mut args = Vec::new();
            if let Some(prefix) = &prefix {
                args.push(prefix.as_str());
            }
            args.extend([
                "changes",
                "-L",
                "-m",
                &count,
                "-s",
                if tab == Tab::Log {
                    "submitted"
                } else {
                    "shelved"
                },
                &scope,
            ]);
            let records = self.records(&args)?;
            let mut matching = 0;
            for record in records
                .iter()
                .filter(|record| record.get("change").is_some())
            {
                let id = field(record, "change")
                    .parse::<u64>()
                    .context("Invalid history changelist")?;
                ensure!(id > 0, "Invalid history changelist");
                matching += usize::from(cursor.is_none_or(|cursor| id <= cursor.before));
            }
            if tab == Tab::Log
                || matching >= page_size
                || records.len() < maximum
                || maximum == 10000
            {
                break records;
            }
            // Shelf insertions can shift the saved offset. Grow only enough to
            // complete this requested page, with the same 10,000-row cache cap.
            maximum = maximum
                .saturating_add(page_size.max(maximum / 2))
                .min(10000);
        };
        let mut changes: Vec<_> = records
            .iter()
            .filter(|r| r.get("change").is_some())
            .collect();
        changes.sort_by_key(|r| std::cmp::Reverse(field(r, "change").parse::<u64>().unwrap_or(0)));
        let mut shelf_offset = None;
        if tab == Tab::Shelves {
            changes = changes
                .into_iter()
                .enumerate()
                .filter(|(_, record)| {
                    cursor.is_none_or(|cursor| {
                        field(record, "change")
                            .parse::<u64>()
                            .is_ok_and(|id| id <= cursor.before)
                    })
                })
                .take(page_size)
                .map(|(index, record)| {
                    shelf_offset = Some(index + 1);
                    record
                })
                .collect();
        }
        let mut oldest = 0;
        let mut newest = 0;
        for change in &changes {
            let id = field(change, "change")
                .parse::<u64>()
                .context("Invalid history changelist")?;
            ensure!(
                id > 0 && cursor.is_none_or(|cursor| id <= cursor.before),
                "Perforce returned a change outside the requested history page"
            );
            newest = newest.max(id);
            oldest = if oldest == 0 { id } else { oldest.min(id) };
        }
        let shelf_capped = tab == Tab::Shelves
            && records.len() >= 10000
            && (changes.len() < page_size || shelf_offset == Some(10000));
        if shelf_capped {
            view.warnings.push("Shelf browsing reached its 10,000-row prefix limit; refresh or narrow the launch directory.".into());
        }
        let next =
            (changes.len() >= page_size && oldest > 1 && !shelf_capped).then(|| HistoryCursor {
                before: oldest - 1,
                shelf_offset,
            });
        view.items.extend(changes.into_iter().map(change_item));
        view.history = Some(HistoryPage {
            first_change: oldest,
            last_change: newest,
            next,
            requested_scan_limit: context.scan_limit,
            group_scan_limit: context.group_scan_limit,
        });
        view.limit_reached = next.is_some();
        Ok(view)
    }
    pub fn change_files(&self, id: &str, tab: Tab) -> Result<View> {
        validate_change(id, true)?;
        let info = self.info()?;
        let mut items = Vec::new();
        if tab == Tab::Changes {
            let scope = format!("//{}/...", info.client);
            for r in self.records(&["fstat", "-Ro", "-Or", &scope])? {
                if r.get("depotFile").is_some() && field(&r, "change") == id {
                    items.push(Item::File(file(&r, true)));
                }
            }
        } else {
            let mut args = vec!["describe", "-s", "-m", "200"];
            if tab == Tab::Shelves {
                args.push("-S");
            }
            args.push(id);
            for r in self.records(&args)? {
                let mut index = 0;
                while r.get(format!("depotFile{index}")).is_some() {
                    items.push(Item::Revision {
                        depot: field(&r, &format!("depotFile{index}")),
                        revision: field(&r, &format!("rev{index}")),
                        action: field(&r, &format!("action{index}")),
                        change: id.into(),
                        shelved: tab == Tab::Shelves,
                    });
                    index += 1;
                }
            }
        }
        let limited = tab != Tab::Changes && items.len() >= 200;
        Ok(View {
            info,
            items,
            warnings: if limited {
                vec!["Showing the first 200 affected files; use p4 describe for the complete change.".into()]
            } else {
                Vec::new()
            },
            limit_reached: limited,
            ..Default::default()
        })
    }
    pub fn detail(&self, detail: Detail) -> Result<String> {
        match detail {
            Detail::Item(
                Item::Revision {
                    depot,
                    revision,
                    action,
                    change,
                    shelved,
                },
                _,
            ) => {
                let rev = revision.parse::<u32>().context("Invalid depot revision")?;
                let right = if shelved {
                    format!("{depot}@={change}")
                } else {
                    format!("{depot}#{rev}")
                };
                if action == "add" || action == "branch" || action == "move/add" {
                    return self.preview_depot(&right);
                }
                let base = if shelved { rev } else { rev.saturating_sub(1) };
                let left = format!("{depot}#{base}");
                if action.contains("delete") {
                    return Ok(self
                        .preview_depot(&left)?
                        .lines()
                        .map(|l| format!("-{l}"))
                        .collect::<Vec<_>>()
                        .join("\n"));
                }
                self.text(&["diff2", "-du", &left, &right])
            }
            Detail::Item(Item::File(f), tab) => {
                if tab == Tab::Files && !f.opened {
                    return self.preview_depot(&f.depot);
                }
                if f.action.contains("add") || f.action == "branch" {
                    return self.preview_local(&f.local);
                }
                if f.action.contains("delete") {
                    return Ok(format!(
                        "--- {} (deleted locally)\n{}",
                        f.depot,
                        self.preview_depot(&format!("{}#have", f.depot))?
                            .lines()
                            .map(|l| format!("-{l}"))
                            .collect::<Vec<_>>()
                            .join("\n")
                    ));
                }
                let text = self.text(&["diff", "-du", "-f", &f.depot])?;
                if text.trim().is_empty() {
                    Ok("No content differences. The file may only have an open action or a filetype change.".into())
                } else {
                    Ok(text)
                }
            }
            Detail::Item(Item::Change { id, .. }, tab) => {
                if id == "default" {
                    return self.text(&["opened", "-c", "default"]);
                }
                if tab == Tab::Changes {
                    self.text(&["describe", "-s", "-m", "200", &id])
                } else if tab == Tab::Shelves {
                    self.text(&["describe", "-S", "-s", "-m", "200", &id])
                } else {
                    self.text(&["describe", "-s", "-m", "200", &id])
                }
            }
            Detail::Item(Item::Stream { path, .. }, _) => self.text(&["stream", "-o", &path]),
            Detail::Item(Item::Workspace { name, .. }, _) => self.text(&["client", "-o", &name]),
            Detail::Item(Item::Label { name, .. }, _) => self.text(&["label", "-o", &name]),
            Detail::Annotate(f) => self.text(&["annotate", "-u", "-c", &f.depot]),
            Detail::FileLog(f) => self.text(&["filelog", "-l", "-i", "-m", "100", &f.depot]),
            Detail::Revision(f, rev) => {
                ensure!(
                    rev.parse::<u32>().is_ok_and(|n| n > 0),
                    "Revision must be a positive integer"
                );
                self.preview_depot(&format!("{}#{rev}", f.depot))
            }
        }
    }
    fn preview_depot(&self, path: &str) -> Result<String> {
        // -o keeps binary data out of JSON and avoids printing arbitrary bytes to the terminal.
        // Match print's single selected revision, rather than enumerating every
        // ancestor and rejecting a small file because an older revision is large.
        let sizes = self.records(&["sizes", path])?;
        for r in &sizes {
            let size = field(r, "fileSize").parse::<u64>().unwrap_or(0);
            ensure!(
                size <= self.max_output,
                "File exceeds preview limit ({} bytes). Use an external viewer.",
                size
            );
        }
        let temp = tempfile::NamedTempFile::new()?;
        self.records(&["print", "-q", "-o", &temp.path().to_string_lossy(), path])?;
        self.preview_local(&temp.path().to_string_lossy())
    }
    fn preview_local(&self, path: &str) -> Result<String> {
        // Reading a symlink's target could escape the workspace or block on a device.
        let meta = std::fs::symlink_metadata(path).with_context(|| format!("Reading {path}"))?;
        if meta.file_type().is_symlink() {
            return Ok(format!("Symlink → {}", std::fs::read_link(path)?.display()));
        }
        ensure!(
            meta.is_file(),
            "Preview is only available for regular files"
        );
        preview(Path::new(path), self.max_output)
    }
    /// Mutations use exact selected paths. Bulk operations always require UI confirmation.
    pub fn act(&mut self, action: Action) -> Result<String> {
        match action {
            Action::Reconcile(f, change) => {
                validate_change(&change, true)?;
                let path = f.as_ref().map(|f| f.depot.as_str()).unwrap_or("...");
                self.mutation(&["reconcile", "-f", "-c", &change, path])
            }
            Action::Edit(f, change) => {
                validate_change(&change, true)?;
                self.mutation(&["edit", "-c", &change, &f.depot])
            }
            Action::Delete(f, change) => {
                validate_change(&change, true)?;
                self.mutation(&["delete", "-c", &change, &f.depot])
            }
            Action::Revert(f, keep) => {
                ensure!(f.opened, "Only opened files can be reverted");
                if keep {
                    self.mutation(&["revert", "-k", &f.depot])
                } else {
                    self.mutation(&["revert", &f.depot])
                }
            }
            Action::Reopen(f, change) => {
                validate_change(&change, true)?;
                self.mutation(&["reopen", "-c", &change, &f.depot])
            }
            Action::NewChange(description) => {
                let info = self.info()?;
                let form = change_form("new", &info, &description)?;
                self.execute(&["change", "-i"], Some(&form), false)
            }
            Action::EditChange(id, description) => {
                validate_change(&id, false)?;
                // Fetching the complete form preserves Jobs, Files, Type and Stream metadata.
                let form = self.text(&["change", "-o", &id])?;
                let form = replace_description(&form, &description)?;
                self.execute(&["change", "-i"], Some(&form), false)
            }
            Action::DeleteChange(id) => {
                validate_change(&id, false)?;
                self.mutation(&["change", "-d", &id])
            }
            Action::Submit(id, description) => {
                validate_change(&id, true)?;
                if id == "default" {
                    ensure!(
                        !description.trim().is_empty(),
                        "A submit description is required"
                    );
                    self.mutation(&["submit", "-d", &description])
                } else {
                    self.mutation(&["submit", "-c", &id])
                }
            }
            Action::Shelve(id) => {
                validate_change(&id, false)?;
                self.mutation(&["shelve", "-c", &id])
            }
            Action::Unshelve(id, target) => {
                validate_change(&id, false)?;
                validate_change(&target, true)?;
                self.mutation(&["unshelve", "-s", &id, "-c", &target])
            }
            Action::DeleteShelf(id) => {
                validate_change(&id, false)?;
                self.mutation(&["shelve", "-d", "-c", &id])
            }
            Action::Sync => self.mutation(&["sync", "..."]),
            Action::SwitchStream(path) => {
                validate_stream(&path)?;
                // Never use p4 switch's implicit shelve/revert. Refuse dirty workspaces.
                self.require_clean()?;
                self.mutation(&["switch", "-Rn", &path])
            }
            Action::SwitchWorkspace(name) => {
                ensure!(
                    !name.is_empty() && !name.starts_with('-') && !name.contains(['\n', '\r']),
                    "Invalid workspace name"
                );
                let mut next = self.clone();
                next.pin_identity()?;
                let spec = next.records(&["client", "-o", &name])?;
                let root = spec
                    .first()
                    .map(|r| field(r, "Root"))
                    .filter(|s| !s.is_empty())
                    .context("Workspace has no local root")?;
                ensure!(
                    Path::new(&root).is_dir(),
                    "Workspace root does not exist on this machine: {root}"
                );
                next.client = Some(name.clone());
                next.directory = PathBuf::from(&root);
                next.info()?;
                *self = next;
                Ok(format!(
                    "Using workspace {name} at {root} (this session only)"
                ))
            }
            Action::Resolve(f, mode) => {
                ensure!(
                    ["-am", "-as", "-ay", "-at"].contains(&mode.as_str()),
                    "Unsupported resolve mode"
                );
                self.mutation(&["resolve", &mode, &f.depot])
            }
            Action::Move(f, destination) => {
                ensure!(
                    f.opened && f.action == "edit",
                    "Open the source file for edit before moving it"
                );
                ensure!(!destination.trim().is_empty(), "A destination is required");
                let dest = PathBuf::from(destination);
                let dest = if dest.is_absolute() {
                    dest
                } else {
                    self.directory.join(dest)
                };
                let encoded = escape_local(&dest.to_string_lossy())?;
                self.mutation(&["move", &f.depot, &encoded])
            }
            Action::CreateStream(path, parent) => {
                validate_stream(&path)?;
                ensure!(
                    self.records(&["streams", &path])?.is_empty(),
                    "Stream already exists"
                );
                let form = if parent.is_empty() {
                    self.text(&["stream", "-o", "-t", "mainline", &path])?
                } else {
                    validate_stream(&parent)?;
                    self.text(&["stream", "-o", "-t", "development", "-P", &parent, &path])?
                };
                self.execute(&["stream", "-i"], Some(&form), false)
            }
            Action::DeleteStream(path) => {
                validate_stream(&path)?;
                self.mutation(&["stream", "-d", &path])
            }
            Action::MergeStream(source, change) => {
                validate_stream(&source)?;
                validate_change(&change, true)?;
                self.require_clean()?;
                ensure!(
                    !self.info()?.stream.is_empty(),
                    "Merging streams requires a stream workspace"
                );
                self.mutation(&["merge", "-Af", "-c", &change, "--from", &source, "..."])
            }
            Action::CopyStream(source, change) => {
                validate_stream(&source)?;
                validate_change(&change, true)?;
                self.require_clean()?;
                let info = self.info()?;
                ensure!(
                    !info.stream.is_empty() && info.stream != source,
                    "Choose a different source from the current stream"
                );
                self.mutation(&[
                    "copy",
                    "-Af",
                    "-c",
                    &change,
                    "-r",
                    "-S",
                    &info.stream,
                    "-P",
                    &source,
                    "...",
                ])
            }
            Action::Lock(f, lock) => {
                self.mutation(&[if lock { "lock" } else { "unlock" }, &f.depot])
            }
        }
    }
    fn require_clean(&self) -> Result<()> {
        let info = self.info()?;
        let scope = format!("//{}/...", info.client);
        ensure!(
            self.records(&["opened", &scope])?
                .iter()
                .all(|r| r.get("depotFile").is_none()),
            "Stream switch requires a workspace with no opened files. Shelve or submit first."
        );
        ensure!(
            self.records(&["reconcile", "-n", "-f", &scope])?
                .iter()
                .all(|r| r.get("depotFile").is_none()),
            "Stream switch requires a workspace with no offline modifications or untracked files."
        );
        Ok(())
    }
    fn mutation(&self, args: &[&str]) -> Result<String> {
        let records = self.records(args)?;
        let text = records
            .iter()
            .filter_map(|r| {
                let data = field(r, "data");
                if !data.is_empty() {
                    return Some(data.trim_end().to_string());
                }
                let submitted = field(r, "submittedChange");
                if !submitted.is_empty() {
                    return Some(format!("Submitted change {submitted}"));
                }
                if r.get("specdef").is_some() {
                    return None;
                }
                let path = field(r, "depotFile");
                if !path.is_empty() {
                    return Some(format!(
                        "{} {}{}",
                        field(r, "action"),
                        path,
                        if r.get("rev").is_some() {
                            format!("#{}", field(r, "rev"))
                        } else {
                            String::new()
                        }
                    ));
                }
                if r.get("change").is_some() {
                    return Some(format!("Changelist {}", field(r, "change")));
                }
                None
            })
            .collect::<Vec<_>>()
            .join("\n");
        Ok(if text.is_empty() {
            "Command completed.".into()
        } else {
            text
        })
    }
}

fn change_item(r: &Value) -> Item {
    Item::Change {
        id: field(r, "change"),
        description: field(r, "desc"),
        user: field(r, "user"),
        client: field(r, "client"),
        status: field(r, "status"),
    }
}
/// P4 uses the highest finite limit across groups; any unlimited group removes
/// the group cap. Zero/unset does not override a finite limit from another group.
fn group_limit(groups: &[Value], key: &str) -> Option<u64> {
    if groups.iter().any(|r| {
        matches!(
            field(r, key).to_ascii_lowercase().as_str(),
            "unlimited" | "-1"
        )
    }) {
        return None;
    }
    groups
        .iter()
        .filter_map(|r| field(r, key).parse::<u64>().ok())
        .filter(|n| *n > 0)
        .max()
}

pub fn field(r: &Value, key: &str) -> String {
    match r.get(key) {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Number(n)) => n.to_string(),
        _ => String::new(),
    }
}
fn file(r: &Value, opened: bool) -> FileEntry {
    FileEntry {
        depot: field(r, "depotFile"),
        local: field(r, "clientFile"),
        action: if r.get("action").is_some() {
            field(r, "action")
        } else {
            field(r, "headAction")
        },
        change: field(r, "change"),
        file_type: if r.get("type").is_some() {
            field(r, "type")
        } else {
            field(r, "headType")
        },
        opened,
        unresolved: r.get("unresolved").is_some(),
    }
}
/// EV_EMPTY (17) means no matching records; authentication/permission failures never do.
pub fn parse_records(text: &str) -> Result<Vec<Value>> {
    let mut warnings = Vec::new();
    let records = parse_listing_records(text, &mut warnings)?;
    // Mutations and clean-workspace preflights must not treat partial results as
    // complete success. Only tab listings can return data alongside warnings.
    ensure!(warnings.is_empty(), "p4 warning: {}", warnings.join("\n"));
    Ok(records)
}
fn parse_listing_records(text: &str, warnings: &mut Vec<String>) -> Result<Vec<Value>> {
    let mut records = Vec::new();
    let mut errors = Vec::new();
    for line in text.lines().filter(|l| !l.trim().is_empty()) {
        let r: Value = serde_json::from_str(line)
            .context("Invalid JSON from p4 (requires a client supporting -Mj)")?;
        let severity = field(&r, "severity").parse::<u32>().unwrap_or(0);
        let generic = field(&r, "generic").parse::<u32>().unwrap_or(0);
        if severity >= 3 {
            errors.push(field(&r, "data"));
        } else if severity > 0 && generic == 17 {
            continue;
        } else if severity == 2 {
            warnings.push(field(&r, "data").trim_end().to_string());
        } else {
            records.push(r);
        }
    }
    ensure!(errors.is_empty(), "{}", errors.join("\n"));
    Ok(records)
}
fn validate_stream(path: &str) -> Result<()> {
    ensure!(
        path.starts_with("//")
            && !path.contains(['*', '@', '#', '%', '\n', '\r', '\0'])
            && !path.contains("..."),
        "Invalid stream path"
    );
    Ok(())
}
fn validate_change(id: &str, default: bool) -> Result<()> {
    ensure!(
        (default && id == "default") || id.parse::<u32>().is_ok_and(|n| n > 0),
        "Expected a positive changelist number{}",
        if default { " or default" } else { "" }
    );
    Ok(())
}
fn change_form(id: &str, info: &Info, description: &str) -> Result<String> {
    ensure!(
        !description.trim().is_empty(),
        "Description cannot be empty"
    );
    ensure!(
        !description.contains('\0') && description.len() <= 65536,
        "Description contains a NUL or exceeds 64 KiB"
    );
    ensure!(
        !info.client.contains(['\n', '\r']) && !info.user.contains(['\n', '\r']),
        "Invalid connection identity"
    );
    Ok(format!(
        "Change: {id}\nClient: {}\nUser: {}\nDescription:\n{}\n",
        info.client,
        info.user,
        description
            .replace('\r', "\n")
            .lines()
            .map(|l| format!("\t{l}"))
            .collect::<Vec<_>>()
            .join("\n")
    ))
}
fn replace_description(form: &str, description: &str) -> Result<String> {
    ensure!(
        !description.trim().is_empty(),
        "Description cannot be empty"
    );
    ensure!(
        !description.contains('\0') && description.len() <= 65536,
        "Description contains a NUL or exceeds 64 KiB"
    );
    let mut out = String::new();
    let mut skipping = false;
    let mut found = false;
    for line in form.lines() {
        if line.starts_with("Description:") {
            out.push_str("Description:\n");
            for l in description.replace('\r', "\n").lines() {
                out.push('\t');
                out.push_str(l);
                out.push('\n');
            }
            found = true;
            skipping = true;
        } else if skipping && (line.starts_with(char::is_whitespace) || line.is_empty()) {
            continue;
        } else {
            skipping = false;
            out.push_str(line);
            out.push('\n');
        }
    }
    ensure!(found, "Missing Description in changelist form");
    Ok(out)
}
/// Local paths need escaping; depot paths from P4 are already escaped.
pub fn escape_local(path: &str) -> Result<String> {
    ensure!(
        !path.contains(['\n', '\r', '\0']) && !path.contains("..."),
        "Path contains unsupported control characters or a Perforce recursive wildcard"
    );
    Ok(path
        .replace('%', "%25")
        .replace('@', "%40")
        .replace('#', "%23")
        .replace('*', "%2A"))
}
fn preview(path: &Path, limit: u64) -> Result<String> {
    let mut data = Vec::new();
    let f = std::fs::File::open(path)?;
    let truncated = f.metadata()?.len() > limit;
    f.take(limit).read_to_end(&mut data)?;
    if data.contains(&0) || std::str::from_utf8(&data).is_err() {
        return Ok(format!(
            "Binary or non-UTF-8 file ({} bytes shown). Use an external viewer.",
            data.len()
        ));
    }
    let mut text = String::from_utf8_lossy(&data).into_owned();
    if truncated {
        text.push_str("\n[Preview truncated at configured output limit]");
    }
    Ok(text)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    #[test]
    fn json_errors_cannot_be_mistaken_for_empty_results() {
        assert!(
            parse_records("{\"severity\":3,\"generic\":36,\"data\":\"login required\"}").is_err()
        );
        assert!(
            parse_records("{\"severity\":2,\"generic\":17,\"data\":\"no such files\"}")
                .is_ok_and(|r| r.is_empty())
        );
        assert!(parse_records("not json").is_err());
        assert!(
            parse_records("{\"severity\":2,\"generic\":6,\"data\":\"maxresults exceeded\"}")
                .is_err()
        );
    }
    #[test]
    fn listings_preserve_partial_results_and_warnings_but_errors_remain_fatal() {
        let text = r#"{"depotFile":"//depot/opened.txt","action":"edit"}
{"severity":2,"generic":4,"data":"/workspace/./ordinary.xml - can't reconcile filename with illegal characters.\n"}
{"depotFile":"//depot/new.txt","action":"add"}"#;
        let mut warnings = Vec::new();
        let records = parse_listing_records(text, &mut warnings).unwrap();
        assert_eq!(records.len(), 2);
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("illegal characters"));
        assert!(
            parse_records(text).is_err(),
            "Mutations/preflight must still be strict"
        );
        let fatal = format!(
            "{text}\n{}",
            r#"{"severity":3,"generic":36,"data":"Permission denied"}"#
        );
        assert!(parse_listing_records(&fatal, &mut Vec::new()).is_err());
        let mut warnings = Vec::new();
        let limited = parse_listing_records(
            r#"{"severity":2,"generic":6,"data":"maxresults exceeded"}"#,
            &mut warnings,
        )
        .unwrap();
        assert!(limited.is_empty());
        assert_eq!(warnings, ["maxresults exceeded"]);
    }
    #[test]
    fn connection_removes_dot_components_without_resolving_symlinks_or_parents() {
        let connection = Connection::new(
            PathBuf::from("/logical/link/./workspace/."),
            &Config::default(),
        );
        assert_eq!(
            connection.directory.to_string_lossy().replace('\\', "/"),
            "/logical/link/workspace"
        );
        let connection = Connection::new(
            PathBuf::from("/logical/link/../workspace/."),
            &Config::default(),
        );
        assert_eq!(
            connection.directory.to_string_lossy().replace('\\', "/"),
            "/logical/link/../workspace"
        );
    }
    #[test]
    fn effective_group_limits_ignore_unset_and_honor_unlimited() {
        let groups = vec![
            serde_json::json!({"maxScanRows":"0"}),
            serde_json::json!({"maxScanRows":"80"}),
            serde_json::json!({"maxScanRows":"120"}),
        ];
        assert_eq!(group_limit(&groups, "maxScanRows"), Some(120));
        let mut groups = groups;
        groups.push(serde_json::json!({"maxScanRows":"-1"}));
        assert_eq!(group_limit(&groups, "maxScanRows"), None);
        assert_eq!(group_limit(&[], "maxScanRows"), None);
    }
    #[test]
    fn description_preserves_jobs_and_files_and_prevents_form_injection() {
        let old = "Change: 12\nDescription:\n\told\n\nJobs:\n\tjob42\nFiles:\n\t//depot/a\n";
        let result = replace_description(old, "new\nFiles:\n//evil/a").unwrap();
        assert!(result.contains("\tFiles:\n\t//evil/a\nJobs:\n\tjob42\nFiles:\n\t//depot/a\n"));
    }
    #[test]
    fn path_escaping_and_changelist_validation() {
        assert_eq!(escape_local("/a/@#%*.txt").unwrap(), "/a/%40%23%25%2A.txt");
        assert!(escape_local("/a/.../b").is_err());
        for id in ["0", "-1", "1 ...", "1\nFiles:"] {
            assert!(validate_change(id, true).is_err());
        }
        assert!(validate_change("default", false).is_err());
    }
}
