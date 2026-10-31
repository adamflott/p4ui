//! The CLI is our protocol boundary. No command is evaluated by a shell.
use crate::{config::Config, model::*};
use anyhow::{Context, Result, bail, ensure};
use serde_json::Value;
use std::{
    collections::HashSet,
    io::{Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    process::{Command, Stdio},
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
    pub history_change_window: u64,
    pub history_max_scan_rows: u64,
    /// Used by isolated integration fixtures; normal operation inherits P4 settings.
    pub environment: Vec<(String, String)>,
    pub cancelled: Arc<AtomicBool>,
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
            history_change_window: config.history_change_window,
            history_max_scan_rows: config.history_max_scan_rows,
            environment: Vec::new(),
            cancelled: Arc::new(AtomicBool::new(false)),
        }
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
        ensure!(
            !self.cancelled.load(Ordering::Relaxed),
            "Perforce request cancelled"
        );
        let mut out = tempfile::tempfile()?;
        let mut err = tempfile::tempfile()?;
        let mut input = tempfile::tempfile()?;
        if let Some(form) = form {
            input.write_all(form.as_bytes())?;
            input.seek(SeekFrom::Start(0))?;
        }
        let mut child = self
            .command(json)
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
            })?;
        let start = Instant::now();
        let status = loop {
            if let Some(status) = child.try_wait()? {
                break status;
            }
            let too_large = out.metadata()?.len() + err.metadata()?.len() > self.max_output;
            if too_large
                || start.elapsed() >= self.timeout
                || self.cancelled.load(Ordering::Relaxed)
            {
                let _ = child.kill();
                let _ = child.wait();
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
    pub fn info(&self) -> Result<Info> {
        let records = self.records(&["info"])?;
        let r = records
            .first()
            .context("p4 info returned no connection information")?;
        let info = Info {
            user: field(r, "userName"),
            client: field(r, "clientName"),
            root: field(r, "clientRoot"),
            server: field(r, "serverAddress"),
            stream: field(r, "clientStream"),
        };
        ensure!(
            !info.client.is_empty() && info.client != "*unknown*" && !info.root.is_empty(),
            "No configured workspace. Set P4CLIENT, use --client, or create a workspace with p4 client first. Run p4 login if authentication is required."
        );
        Ok(info)
    }
    pub fn load(&self, tab: Tab, limit: usize) -> Result<View> {
        let info = self.info()?;
        let mut view = View {
            info,
            ..View::default()
        };
        let scope = "...";
        let n = limit.to_string();
        match tab {
            Tab::Status | Tab::Resolve => {
                let opened =
                    self.listing_records(&["fstat", "-Ro", "-Or", scope], &mut view.warnings)?;
                for r in opened.iter().filter(|r| r.get("depotFile").is_some()) {
                    let f = file(r, true);
                    if tab == Tab::Status || f.unresolved {
                        view.items.push(Item::File(f));
                    }
                }
                if tab == Tab::Status {
                    let known: HashSet<String> =
                        view.items.iter().map(|i| i.key().to_string()).collect();
                    for r in
                        self.listing_records(&["reconcile", "-n", "-f", scope], &mut view.warnings)?
                    {
                        if r.get("depotFile").is_some() && !known.contains(&field(&r, "depotFile"))
                        {
                            view.items.push(Item::File(file(&r, false)));
                        }
                    }
                    view.items.sort_by_key(|i| match i {
                        Item::File(f) => (!f.opened, f.change.clone(), f.depot.clone()),
                        _ => (false, String::new(), String::new()),
                    });
                }
            }
            Tab::Log | Tab::Shelves => return self.history_with_info(view.info, tab, None),
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
    pub fn load_history(&self, tab: Tab, cursor: Option<HistoryCursor>) -> Result<View> {
        ensure!(
            tab.is_history(),
            "History paging is only available in Log and Shelves"
        );
        self.history_with_info(self.info()?, tab, cursor)
    }
    fn history_with_info(
        &self,
        info: Info,
        tab: Tab,
        cursor: Option<HistoryCursor>,
    ) -> Result<View> {
        let mut view = View {
            info,
            ..Default::default()
        };
        // Group metadata is cheap compared with revision history. Never increase
        // a reported admin limit, including on servers permitting overrides.
        let groups = self.records(&["groups", "-i", "-v", "-m", "1000", &view.info.user]);
        let (scan_limit, group_limit, page_size, head_limit) = match groups {
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
                let results = group_limit(&groups, "maxResults");
                let page = results
                    .map(|r| self.history_page_size.min(r as usize))
                    .unwrap_or(self.history_page_size)
                    .max(1);
                (
                    Some(
                        scan.map(|r| r.min(self.history_max_scan_rows))
                            .unwrap_or(self.history_max_scan_rows),
                    ),
                    scan,
                    page,
                    results.unwrap_or(50001).clamp(1, 50001) as usize,
                )
            }
            _ => {
                view.warnings.push("Could not determine complete group limits; leaving server limits unchanged and using a bounded changelist window.".into());
                (None, None, self.history_page_size, 50001)
            }
        };
        let prefix = scan_limit.map(|limit| format!("-zmaxScanRows={limit}"));
        let read = |args: &[&str]| -> Result<Vec<Value>> {
            let mut bounded = Vec::new();
            if let Some(prefix) = &prefix {
                bounded.push(prefix.as_str());
            }
            bounded.extend_from_slice(args);
            self.records(&bounded)
        };
        let mut upper = cursor.map(|cursor| cursor.before);
        if tab == Tab::Log {
            // A global change counter can be millions of IDs ahead of a quiet
            // branch. Seek using one revision per scoped file, rather than
            // walking empty windows or scanning every revision of every file.
            // At an older cursor, fstat's headChange describes that snapshot.
            let snapshot = upper
                .map(|before| format!("...@{before}"))
                .unwrap_or_else(|| "...".into());
            let maximum = head_limit.to_string();
            match read(&["fstat", "-Os", "-T", "headChange", "-m", &maximum, &snapshot]) {
                Ok(records) if records.len() < head_limit => {
                    let mut latest = 0;
                    for record in records {
                        if record.get("headChange").is_some() {
                            let change = field(&record, "headChange")
                                .parse::<u64>()
                                .context("Invalid head changelist in history lookup")?;
                            ensure!(
                                upper.is_none_or(|before| change <= before),
                                "History lookup returned a change newer than the requested snapshot"
                            );
                            latest = latest.max(change);
                        }
                    }
                    upper = Some(latest);
                }
                Ok(_) => view.warnings.push("History position lookup reached its file limit; falling back to bounded changelist windows. Empty windows may require + to continue.".into()),
                Err(error) if resource_limited(&error) => view.warnings.push(format!("History position lookup reached a server resource limit; falling back to a bounded changelist window. {error:#}")),
                Err(error) => return Err(error),
            }
        }
        let upper = if let Some(upper) = upper {
            upper
        } else {
            // The global counter is an O(1) high-water mark and also includes
            // pending shelves newer than the latest submitted changelist.
            read(&["counter", "change"])?
                .iter()
                .filter_map(|r| field(r, "value").parse::<u64>().ok())
                .max()
                .unwrap_or(0)
        };
        if upper == 0 {
            return Ok(view);
        }
        let window = cursor
            .map(|c| c.window)
            .unwrap_or(self.history_change_window)
            .clamp(1, self.history_change_window);
        let lower = upper.saturating_sub(window - 1).max(1);
        let scope = format!("...@{lower},@{upper}");
        let page = page_size.clamp(1, 200).to_string();
        let result = read(&[
            "changes",
            "-L",
            "-m",
            &page,
            "-s",
            if tab == Tab::Log {
                "submitted"
            } else {
                "shelved"
            },
            &scope,
        ]);
        let next = match result {
            Ok(records) => {
                let mut changes: Vec<_> = records
                    .iter()
                    .filter(|r| r.get("change").is_some())
                    .collect();
                changes.sort_by_key(|r| {
                    std::cmp::Reverse(field(r, "change").parse::<u64>().unwrap_or(0))
                });
                let before = if changes.len() >= page_size {
                    changes
                        .last()
                        .and_then(|r| field(r, "change").parse::<u64>().ok())
                        .unwrap_or(lower)
                        .saturating_sub(1)
                } else {
                    lower.saturating_sub(1)
                };
                view.items.extend(changes.into_iter().map(change_item));
                (before > 0).then_some(HistoryCursor { before, window })
            }
            Err(error) if resource_limited(&error) => {
                if window > 1 {
                    view.warnings.push(format!("History scan stopped at a server resource limit. No automatic retry; + retries this range with a smaller window. {error:#}"));
                    Some(HistoryCursor {
                        before: upper,
                        window: (window / 2).max(1),
                    })
                } else {
                    view.warnings.push(format!("Change {upper} exceeds the history query budget and was skipped; + continues before it. Inspect this change with the p4 CLI if needed. {error:#}"));
                    (upper > 1).then_some(HistoryCursor {
                        before: upper - 1,
                        window: self.history_change_window,
                    })
                }
            }
            Err(error) => return Err(error),
        };
        view.history = Some(HistoryPage {
            first_change: lower,
            last_change: upper,
            next,
            requested_scan_limit: scan_limit,
            group_scan_limit: group_limit,
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
        let sizes = self.records(&["sizes", "-a", path])?;
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
            Action::Delete(f) => self.mutation(&["delete", &f.depot]),
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
                let spec = self.records(&["client", "-o", &name])?;
                let root = spec
                    .first()
                    .map(|r| field(r, "Root"))
                    .filter(|s| !s.is_empty())
                    .context("Workspace has no local root")?;
                ensure!(
                    Path::new(&root).is_dir(),
                    "Workspace root does not exist on this machine: {root}"
                );
                let mut next = self.clone();
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

fn resource_limited(error: &anyhow::Error) -> bool {
    let message = error.to_string().to_ascii_lowercase();
    message.contains("maxscanrows") || message.contains("maxresults")
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
