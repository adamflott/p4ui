use crate::{config::Config, events::Wake, model::*, p4::Connection};
use anyhow::{Context, Result};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use std::{
    cell::{Ref, RefCell},
    io::{Read, Write},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver, Sender},
    },
    thread,
    time::{Duration, Instant},
};

#[derive(Debug, Clone)]
pub enum InputKind {
    NewChange,
    NewStream(String),
    EditChange(String),
    SubmitDefault,
    Reopen(FileEntry),
    Unshelve(String),
    Move(FileEntry),
    Revision(FileEntry),
    Resolve(FileEntry),
}
impl InputKind {
    pub fn title(&self) -> &'static str {
        match self {
            Self::NewChange => "New changelist description",
            Self::NewStream(_) => "Create stream — full //depot/stream path",
            Self::EditChange(_) => "Edit changelist description",
            Self::SubmitDefault => "Submit DEFAULT changelist — description",
            Self::Reopen(_) => "Move file to changelist (number or default)",
            Self::Unshelve(_) => "Unshelve into changelist (number or default)",
            Self::Move(_) => "Move opened file — destination local path",
            Self::Revision(_) => "View depot revision — positive revision number",
            Self::Resolve(_) => "Resolve mode: -am merge, -as safe, -ay yours, -at theirs",
        }
    }
    pub fn multiline(&self) -> bool {
        matches!(
            self,
            Self::NewChange | Self::EditChange(_) | Self::SubmitDefault
        )
    }
}
#[derive(Debug, Clone)]
pub enum Popup {
    Help,
    Message { title: String, text: String },
    Input { kind: InputKind, value: String },
    Confirm { action: Action, text: String },
}

enum Request {
    History(Tab, Option<HistoryCursor>, usize, u64),
    Load(Tab, usize),
    ChangeFiles(String, Tab),
    Detail(String, Detail),
    Act(Action),
    Edit(FileEntry, String),
    ClientSpec,
}
enum Response {
    History(Tab, Option<HistoryCursor>, u64, Result<View>),
    Load(Tab, Result<View>),
    ChangeFiles(String, Tab, Result<View>),
    Detail(String, Result<String>),
    Act(Action, Result<String>),
    Edit(Result<std::path::PathBuf>),
    ClientSpec(Result<ExternalEdit>),
}

pub enum ExternalEdit {
    File(std::path::PathBuf),
    ClientSpec {
        name: String,
        original: String,
        path: tempfile::TempPath,
    },
}
impl ExternalEdit {
    pub fn path(&self) -> &std::path::Path {
        match self {
            Self::File(path) => path,
            Self::ClientSpec { path, .. } => path,
        }
    }
    fn client_spec(name: String, original: String) -> Result<Self> {
        let mut file = tempfile::Builder::new()
            .prefix("p4ui-client-")
            .suffix(".p4s")
            .tempfile()?;
        file.write_all(original.as_bytes())?;
        Ok(Self::ClientSpec {
            name,
            original,
            // Close the file before launching the editor (also on Windows).
            path: file.into_temp_path(),
        })
    }
}
struct Worker {
    send: Sender<Request>,
    receive: Receiver<Response>,
    cancel: Arc<AtomicBool>,
    handle: Option<thread::JoinHandle<()>>,
}
impl Drop for Worker {
    fn drop(&mut self) {
        self.cancel.store(true, Ordering::Relaxed);
        let (dummy, _) = mpsc::channel();
        drop(std::mem::replace(&mut self.send, dummy));
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}
impl Worker {
    fn new(mut connection: Connection, wake: Option<Sender<Wake>>) -> Self {
        let (send, jobs) = mpsc::channel();
        let (results, receive) = mpsc::channel();
        let cancel = connection.cancelled.clone();
        let handle = thread::spawn(move || {
            let mut history_context: Option<(u64, crate::p4::HistoryContext)> = None;
            while let Ok(job) = jobs.recv() {
                let result = match job {
                    Request::History(tab, cursor, size, epoch) => {
                        let result = (|| {
                            // Reuse connection metadata within a browsing session.
                            // Explicit overrides must recheck current admin limits.
                            if connection.history_max_scan_rows != 0
                                || history_context
                                    .as_ref()
                                    .is_none_or(|(generation, _)| *generation != epoch)
                            {
                                let (context, view) =
                                    connection.start_history(tab, cursor, size)?;
                                history_context = Some((epoch, context));
                                return Ok(view);
                            }
                            let (_, context) = history_context
                                .as_ref()
                                .context("Missing history connection")?;
                            connection.history_page(tab, cursor, size, context)
                        })();
                        Response::History(tab, cursor, epoch, result)
                    }
                    Request::Load(tab, limit) => Response::Load(tab, connection.load(tab, limit)),
                    Request::ChangeFiles(id, tab) => {
                        Response::ChangeFiles(id.clone(), tab, connection.change_files(&id, tab))
                    }
                    Request::Detail(key, detail) => {
                        Response::Detail(key, connection.detail(detail))
                    }
                    Request::Act(action) => {
                        let result = connection.act(action.clone());
                        Response::Act(action, result)
                    }
                    Request::Edit(file, target) => Response::Edit((|| {
                        let meta = std::fs::symlink_metadata(&file.local)?;
                        anyhow::ensure!(
                            meta.is_file(),
                            "Only regular local files can be opened in an editor"
                        );
                        if !file.opened {
                            connection.act(if file.action == "add" {
                                Action::Reconcile(Some(file.clone()), target)
                            } else {
                                Action::Edit(file.clone(), target)
                            })?;
                        }
                        Ok(std::path::PathBuf::from(file.local))
                    })()),
                    Request::ClientSpec => Response::ClientSpec((|| {
                        let (name, form) = connection.client_spec()?;
                        ExternalEdit::client_spec(name, form)
                    })()),
                };
                if results.send(result).is_err() {
                    break;
                }
                if let Some(wake) = &wake {
                    let _ = wake.send(Wake::Worker);
                }
            }
        });
        Self {
            send,
            receive,
            cancel,
            handle: Some(handle),
        }
    }
}

struct ParentView {
    view: View,
    selected: usize,
    filter: String,
    list_states: [ratatui::widgets::ListState; 3],
}

struct StatusState {
    info: Info,
    selected_key: Option<String>,
    selected: usize,
    opened: Option<bool>,
    pane_index: usize,
    filter: String,
    list_states: [ratatui::widgets::ListState; 3],
    top_keys: [Option<String>; 2],
}

pub struct App {
    pub config: Config,
    pub tab: Tab,
    pub view: View,
    pub selected: usize,
    pub(crate) list_states: [ratatui::widgets::ListState; 3],
    pub filter: String,
    pub searching: bool,
    pub detail: String,
    pub detail_title: String,
    pub detail_scroll: (u16, u16),
    pub popup_scroll: u16,
    pub focus_detail: bool,
    pub popup: Option<Popup>,
    pub status: String,
    pub busy: bool,
    pub quit: bool,
    pub mutating: bool,
    pub external_edit: Option<ExternalEdit>,
    client_draft: Option<tempfile::TempPath>,
    pub target: String,
    pub limit: usize,
    pub drill: Option<String>,
    parent: Option<ParentView>,
    status_state: Option<StatusState>,
    status_restore_pending: bool,
    history_more_requested: bool,
    history_scroll_requested: bool,
    history_paging_blocked: bool,
    history_rows: usize,
    history_epoch: u64,
    pub last_refresh: Instant,
    worker: Option<Worker>,
    refresh_needed: bool,
    detail_needed: bool,
    selection_changed: Instant,
    row_cache: RefCell<crate::view_cache::Rows>,
    detail_index: RefCell<crate::view_cache::Lines>,
}
impl App {
    pub fn new(config: Config, connection: Connection) -> Self {
        Self::with_wake(config, connection, None)
    }
    pub fn with_wake(config: Config, connection: Connection, wake: Option<Sender<Wake>>) -> Self {
        let mut app = Self::for_view(config, View::default());
        app.worker = Some(Worker::new(connection, wake));
        app.refresh_needed = true;
        app
    }
    /// Also useful for rendering deterministic offline snapshots.
    pub fn for_view(config: Config, view: View) -> Self {
        let limit = config.page_size;
        Self {
            config,
            tab: Tab::Status,
            view,
            selected: 0,
            list_states: Default::default(),
            filter: String::new(),
            searching: false,
            detail: String::new(),
            detail_title: "Diff".into(),
            detail_scroll: (0, 0),
            popup_scroll: 0,
            focus_detail: false,
            popup: None,
            status: "Connecting…".into(),
            busy: false,
            quit: false,
            mutating: false,
            external_edit: None,
            client_draft: None,
            target: "default".into(),
            limit,
            drill: None,
            parent: None,
            status_state: None,
            status_restore_pending: false,
            history_more_requested: false,
            history_scroll_requested: false,
            history_paging_blocked: false,
            history_rows: 20,
            history_epoch: 0,
            last_refresh: Instant::now(),
            worker: None,
            refresh_needed: false,
            detail_needed: true,
            selection_changed: Instant::now(),
            row_cache: RefCell::default(),
            detail_index: RefCell::default(),
        }
    }
    pub fn visible(&self) -> Vec<&Item> {
        self.rows()
            .indices
            .iter()
            .map(|index| &self.view.items[*index])
            .collect()
    }
    pub(crate) fn rows(&self) -> Ref<'_, crate::view_cache::Rows> {
        self.row_cache
            .borrow_mut()
            .update(&self.view.items, &self.filter);
        self.row_cache.borrow()
    }
    fn invalidate_rows(&self) {
        self.row_cache.borrow_mut().invalidate();
    }
    pub(crate) fn detail_window(&self, first: usize, count: usize) -> Vec<&str> {
        self.detail_index
            .borrow_mut()
            .window(&self.detail, first, count)
    }
    pub fn current(&self) -> Option<Item> {
        self.rows()
            .indices
            .get(self.selected)
            .map(|index| self.view.items[*index].clone())
    }
    fn current_key(&self) -> String {
        self.current()
            .map(|i| format!("{:?}:{}", self.tab, i.key()))
            .unwrap_or_default()
    }
    fn selection_updated(&mut self) {
        self.detail_needed = true;
        self.selection_changed = Instant::now();
        self.detail_scroll = (0, 0);
        self.detail.clear();
        self.detail_index.borrow_mut().invalidate();
        self.detail_title = "Diff / details".into();
    }
    fn jump_status_pane(&mut self) {
        let visible = self.visible();
        let target_opened = visible
            .get(self.selected)
            .is_some_and(|item| matches!(item, Item::File(file) if !file.opened));
        let destination = visible
            .iter()
            .position(|item| matches!(item, Item::File(file) if file.opened == target_opened));
        self.focus_detail = false;
        let pane = if target_opened {
            "Opened"
        } else {
            "Offline / untracked"
        };
        if let Some(index) = destination {
            self.selected = index;
            self.selection_updated();
            self.status = format!("{pane} · first matching file");
        } else {
            self.status = format!(
                "No {pane} files{}",
                if self.filter.is_empty() {
                    " in the loaded Status"
                } else {
                    " match the current filter"
                }
            );
        }
    }
    fn message(&mut self, title: &str, text: impl Into<String>) {
        self.popup_scroll = 0;
        self.popup = Some(Popup::Message {
            title: title.into(),
            text: text.into(),
        });
    }
    fn input(&mut self, kind: InputKind, value: String) {
        self.popup = Some(Popup::Input { kind, value });
    }
    fn confirm(&mut self, action: Action, text: impl Into<String>) {
        self.popup = Some(Popup::Confirm {
            action,
            text: text.into(),
        });
    }
    fn send(&mut self, req: Request) -> Result<()> {
        if let Some(worker) = &self.worker {
            worker.cancel.store(false, Ordering::Relaxed);
            worker.send.send(req)?;
            self.busy = true;
        }
        Ok(())
    }
    /// Process ready results/timers and report whether the visible state changed.
    pub fn tick(&mut self) -> Result<bool> {
        let mut changed = false;
        let response = self.worker.as_ref().and_then(|w| w.receive.try_recv().ok());
        if let Some(response) = response {
            changed = true;
            self.busy = false;
            if !self.mutating
                && self
                    .worker
                    .as_ref()
                    .is_some_and(|worker| worker.cancel.load(Ordering::Relaxed))
            {
                // Discard cancelled reads even after a quick tab round trip.
                return Ok(changed);
            }
            self.mutating = false;
            match response {
                Response::History(tab, cursor, epoch, result) => {
                    if tab == self.tab && self.drill.is_none() && epoch == self.history_epoch {
                        match result {
                            Ok(view) => self.apply_history_page(view, cursor.is_some()),
                            Err(error) => {
                                self.history_paging_blocked = true;
                                self.history_more_requested = false;
                                self.history_scroll_requested = false;
                                self.status = format!("{error:#}");
                                self.message("History query failed",format!("{error:#}\n\nAutomatic paging is paused. Narrow the launch directory if the server limit was reached. Press r to refresh or + to retry an older page."));
                            }
                        }
                        self.last_refresh = Instant::now();
                    }
                }
                Response::ChangeFiles(id, tab, result) => {
                    if tab == self.tab && self.drill.as_ref() == Some(&id) {
                        match result {
                            Ok(view) => {
                                self.view = view;
                                self.invalidate_rows();
                                self.selected =
                                    self.selected.min(self.visible().len().saturating_sub(1));
                                self.detail_needed = true;
                                self.status = format!("Changelist {id} · Esc returns to list");
                            }
                            Err(e) => {
                                self.message("Cannot load changelist files", format!("{e:#}"))
                            }
                        }
                        self.last_refresh = Instant::now();
                    }
                }
                Response::Load(tab, result) => {
                    if tab == self.tab && self.drill.is_none() {
                        match result {
                            Ok(view) => {
                                let mut key = self.current().map(|i| i.key().to_string());
                                let restore = if tab == Tab::Status && self.status_restore_pending {
                                    self.status_restore_pending = false;
                                    self.status_state.take()
                                } else {
                                    None
                                };
                                let workspace_changed = !self.view.info.client.is_empty()
                                    && self.view.info.client != view.info.client
                                    || restore.as_ref().is_some_and(|state| {
                                        !state.info.client.is_empty() && state.info != view.info
                                    });
                                if workspace_changed {
                                    self.target = "default".into();
                                    self.filter.clear();
                                    self.selected = 0;
                                    self.list_states = Default::default();
                                    self.status_state = None;
                                    key = None;
                                    self.selection_updated();
                                }
                                self.view = view;
                                self.invalidate_rows();
                                self.selected = key
                                    .and_then(|k| self.visible().iter().position(|i| i.key() == k))
                                    .unwrap_or(self.selected)
                                    .min(self.visible().len().saturating_sub(1));
                                if !workspace_changed && let Some(state) = restore {
                                    self.restore_status_state(state);
                                }
                                let warnings = if self.view.warnings.is_empty() {
                                    String::new()
                                } else {
                                    format!(
                                        " · {} warnings (? for details; results may be incomplete)",
                                        self.view.warnings.len()
                                    )
                                };
                                self.status = format!(
                                    "{} items{warnings} · scope: current directory and descendants",
                                    self.view.items.len()
                                );
                                self.detail_needed = true;
                            }
                            Err(e) => {
                                self.status = format!("{e:#}");
                                self.message("Perforce error",format!("{e:#}\n\nCheck p4 info and p4 login in this directory, then press r to retry."));
                            }
                        }
                        self.last_refresh = Instant::now();
                    }
                }
                Response::Detail(key, result) => {
                    if key == self.current_key() {
                        self.detail = result.unwrap_or_else(|e| format!("{e:#}"));
                        self.detail_index.borrow_mut().invalidate();
                    }
                }
                Response::Edit(result) => {
                    match result {
                        Ok(path) => self.external_edit = Some(ExternalEdit::File(path)),
                        Err(e) => self.message("Cannot edit file", format!("{e:#}")),
                    }
                    self.refresh_needed = true;
                }
                Response::ClientSpec(result) => match result {
                    Ok(edit) => self.external_edit = Some(edit),
                    Err(e) => self.message("Cannot edit client specification", format!("{e:#}")),
                },
                Response::Act(action, result) => {
                    let draft = if matches!(&action, Action::SaveClientSpec { .. }) {
                        self.client_draft.take()
                    } else {
                        None
                    };
                    match result {
                        Ok(text) => {
                            if matches!(
                                &action,
                                Action::SwitchWorkspace(_) | Action::SwitchStream(_)
                            ) {
                                self.status_state = None;
                                self.status_restore_pending = false;
                                self.target = "default".into();
                            }
                            if matches!(&action, Action::Submit(id, _) | Action::DeleteChange(id)
                                if id == &self.target)
                            {
                                self.target = "default".into();
                            }
                            self.message("Command result", text);
                            self.status = "Completed; refreshing…".into();
                        }
                        Err(e) => {
                            let recovery = draft.map(preserve_draft).unwrap_or_default();
                            self.message(
                                "Command failed — inspect state before retrying",
                                format!("{e:#}{recovery}"),
                            );
                        }
                    }
                    self.refresh_needed = true;
                }
            }
        }
        // Keep the worker idle while the client form is in an editor or awaiting
        // confirmation. A background refresh must not consume its save request.
        if self.external_edit.is_some() || self.client_draft.is_some() {
            return Ok(changed);
        }
        if self.config.refresh_seconds > 0
            && !self.busy
            && !self.tab.is_history()
            && self.drill.is_none()
            && self.popup.is_none()
            && !self.searching
            && self.last_refresh.elapsed() >= Duration::from_secs(self.config.refresh_seconds)
        {
            self.refresh_needed = true;
        }
        if !self.busy && self.refresh_needed {
            changed = true;
            self.refresh_needed = false;
            if let Some(id) = &self.drill {
                self.send(Request::ChangeFiles(id.clone(), self.tab))?;
            } else if self.tab.is_history() {
                self.history_epoch = self.history_epoch.wrapping_add(1);
                self.history_more_requested = false;
                self.history_scroll_requested = false;
                self.history_paging_blocked = false;
                self.send(Request::History(
                    self.tab,
                    None,
                    self.history_batch_size(),
                    self.history_epoch,
                ))?;
            } else {
                self.send(Request::Load(self.tab, self.limit))?;
            }
        } else if !self.busy && (self.history_more_requested || self.history_page_needed()) {
            changed = true;
            self.history_more_requested = false;
            self.history_scroll_requested = false;
            if let Some(cursor) = self.view.history.as_ref().and_then(|p| p.next) {
                let missing = self.history_rows.saturating_sub(self.view.items.len());
                let size = if missing > 0 {
                    missing.min(self.history_batch_size())
                } else {
                    self.history_batch_size()
                };
                self.send(Request::History(
                    self.tab,
                    Some(cursor),
                    size,
                    self.history_epoch,
                ))?;
            }
        } else if !self.busy
            && self.detail_needed
            && self.selection_changed.elapsed() >= Duration::from_millis(100)
        {
            changed = true;
            self.detail_needed = false;
            if let Some(item) = self.current() {
                let key = self.current_key();
                self.detail_title = item.key().to_string();
                self.send(Request::Detail(key, Detail::Item(item, self.tab)))?;
            } else {
                self.detail.clear();
                self.detail_index.borrow_mut().invalidate();
            }
        }
        Ok(changed)
    }
    /// Deadline for deferred work; worker completion has its own wake message.
    pub fn next_wake(&self) -> Duration {
        let mut wait = Duration::from_millis(250);
        if self.busy || self.external_edit.is_some() || self.client_draft.is_some() {
            return wait;
        }
        if self.refresh_needed {
            return Duration::ZERO;
        }
        if self.detail_needed {
            wait = wait
                .min(Duration::from_millis(100).saturating_sub(self.selection_changed.elapsed()));
        }
        if self.config.refresh_seconds > 0
            && !self.tab.is_history()
            && self.drill.is_none()
            && self.popup.is_none()
            && !self.searching
        {
            wait = wait.min(
                Duration::from_secs(self.config.refresh_seconds)
                    .saturating_sub(self.last_refresh.elapsed()),
            );
        }
        wait
    }
    fn apply_history_page(&mut self, mut page: View, append: bool) {
        self.history_paging_blocked = false;
        self.history_scroll_requested = false;
        let was_empty = self.view.items.is_empty();
        let key = self.current().map(|item| item.key().to_string());
        if append {
            let mut known: std::collections::HashSet<_> = self
                .view
                .items
                .iter()
                .map(|item| item.key().to_string())
                .collect();
            self.view.items.extend(
                page.items
                    .drain(..)
                    .filter(|item| known.insert(item.key().to_string())),
            );
            for warning in page.warnings {
                if !self.view.warnings.contains(&warning) {
                    self.view.warnings.push(warning);
                }
            }
            self.view.history = page.history;
            self.view.limit_reached = page.limit_reached;
        } else {
            self.view = page;
        }
        if self.view.items.len() >= 10000 {
            self.view.items.truncate(10000);
            if let Some(page) = &mut self.view.history {
                page.next = None;
            }
            self.view.limit_reached = false;
            self.view.warnings.push(
                "10,000 changes cached; refresh or use a narrower scope to bound local memory."
                    .into(),
            );
        }
        self.invalidate_rows();
        self.selected = key
            .and_then(|key| self.visible().iter().position(|item| item.key() == key))
            .unwrap_or(self.selected)
            .min(self.visible().len().saturating_sub(1));
        let warnings = if self.view.warnings.is_empty() {
            String::new()
        } else {
            format!(" · {} warnings (?)", self.view.warnings.len())
        };
        self.status = if let Some(page) = &self.view.history
            && page.last_change > 0
        {
            format!(
                "{} changes{warnings} · loaded {}..{} · scan limit {} · {}",
                self.view.items.len(),
                page.first_change,
                page.last_change,
                page.requested_scan_limit
                    .map(|n| n.to_string())
                    .unwrap_or_else(|| "server policy".into()),
                if page.next.is_some() {
                    "scroll for older"
                } else {
                    "end of history"
                }
            )
        } else {
            format!(
                "{} changes{warnings} · end of history",
                self.view.items.len()
            )
        };
        if !append || was_empty {
            self.detail_needed = true;
        }
    }
    pub fn switch_tab(&mut self, tab: Tab) {
        if tab == self.tab {
            return;
        }
        // A quick round trip during a read retains the original bookmark.
        // A newly typed filter starts fresh navigation instead of reviving old anchors.
        if self.tab == Tab::Status
            && (!self.status_restore_pending
                || self
                    .status_state
                    .as_ref()
                    .is_none_or(|state| state.filter != self.filter))
        {
            self.status_state = Some(self.capture_status_state());
        }
        if self.busy
            && !self.mutating
            && let Some(worker) = &self.worker
        {
            worker.cancel.store(true, Ordering::Relaxed);
        }
        self.drill = None;
        self.parent = None;
        self.history_more_requested = false;
        self.history_scroll_requested = false;
        self.history_paging_blocked = false;
        self.history_epoch = self.history_epoch.wrapping_add(1);
        self.tab = tab;
        self.list_states = Default::default();
        self.view.items.clear();
        self.invalidate_rows();
        self.selected = 0;
        self.filter.clear();
        self.status_restore_pending = false;
        if tab == Tab::Status
            && let Some(state) = &self.status_state
        {
            self.filter = state.filter.clone();
            self.status_restore_pending = true;
        }
        self.focus_detail = false;
        self.refresh_needed = true;
        self.selection_updated();
    }
    fn capture_status_state(&self) -> StatusState {
        let visible = self.visible();
        let current = visible.get(self.selected);
        let opened = current.and_then(|item| match item {
            Item::File(file) => Some(file.opened),
            _ => None,
        });
        let pane_index = visible
            .iter()
            .take(self.selected)
            .filter(|item| matches!(item, Item::File(file) if Some(file.opened) == opened))
            .count();
        let top_keys = std::array::from_fn(|pane| {
            visible
                .iter()
                .filter(|item| matches!(item, Item::File(file) if file.opened == (pane == 0)))
                .nth(self.list_states[pane].offset())
                .map(|item| item.key().to_string())
        });
        StatusState {
            info: self.view.info.clone(),
            selected_key: current.map(|item| item.key().to_string()),
            selected: self.selected,
            opened,
            pane_index,
            filter: self.filter.clone(),
            list_states: self.list_states,
            top_keys,
        }
    }
    fn restore_status_state(&mut self, state: StatusState) {
        // Search input made while the refresh was running takes precedence.
        if self.filter != state.filter {
            return;
        }
        let visible = self.visible();
        let same_pane: Vec<usize> = visible
            .iter()
            .enumerate()
            .filter_map(|(index, item)| {
                matches!(item, Item::File(file) if Some(file.opened) == state.opened)
                    .then_some(index)
            })
            .collect();
        let selected = state
            .selected_key
            .as_deref()
            .and_then(|key| visible.iter().position(|item| item.key() == key))
            .or_else(|| {
                same_pane
                    .get(state.pane_index.min(same_pane.len().saturating_sub(1)))
                    .copied()
            })
            .unwrap_or(state.selected)
            .min(visible.len().saturating_sub(1));
        let mut list_states = state.list_states;
        for (pane, key) in state.top_keys.iter().enumerate() {
            if let Some(key) = key
                && let Some(offset) = visible
                    .iter()
                    .filter(|item| matches!(item, Item::File(file) if file.opened == (pane == 0)))
                    .position(|item| item.key() == key)
            {
                *list_states[pane].offset_mut() = offset;
            }
        }
        self.selected = selected;
        self.list_states = list_states;
    }
    fn act(&mut self, action: Action) -> Result<()> {
        if self.busy {
            self.message("Busy", "Wait for the current Perforce request to finish.");
            return Ok(());
        }
        self.status = "Running Perforce command…".into();
        self.mutating = true;
        self.send(Request::Act(action))
    }
    pub fn key(&mut self, key: KeyEvent) -> Result<()> {
        if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
            self.request_quit();
            return Ok(());
        }
        if self.popup.is_some() {
            return self.popup_key(key);
        }
        if self.searching {
            match key.code {
                KeyCode::Esc => {
                    self.searching = false;
                    self.filter.clear();
                    self.selected = 0;
                    self.selection_updated();
                }
                KeyCode::Enter => self.searching = false,
                KeyCode::Backspace => {
                    self.filter.pop();
                    self.selected = 0;
                    self.selection_updated();
                }
                KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                    self.filter.push(c);
                    self.selected = 0;
                    self.selection_updated();
                }
                _ => {}
            }
            return Ok(());
        }
        match key.code {
            KeyCode::Esc => {
                if self.drill.take().is_some() {
                    if let Some(parent) = self.parent.take() {
                        self.view = parent.view;
                        self.invalidate_rows();
                        self.selected = parent.selected;
                        self.filter = parent.filter;
                        self.list_states = parent.list_states;
                    }
                    self.refresh_needed = !self.tab.is_history();
                } else {
                    self.filter.clear();
                }
                self.focus_detail = false;
                self.selection_updated();
            }
            KeyCode::Tab => {
                self.focus_detail = !self.focus_detail;
            }
            KeyCode::BackTab => {
                self.switch_tab(Tab::ALL[(self.tab.index() + 8) % 9]);
            }
            KeyCode::Left if !self.focus_detail => {
                self.switch_tab(Tab::ALL[(self.tab.index() + 8) % 9])
            }
            KeyCode::Right if !self.focus_detail => {
                self.switch_tab(Tab::ALL[(self.tab.index() + 1) % 9])
            }
            KeyCode::Left => self.detail_scroll.1 = self.detail_scroll.1.saturating_sub(4),
            KeyCode::Right => self.detail_scroll.1 = self.detail_scroll.1.saturating_add(4),
            KeyCode::Down => self.navigate(1),
            KeyCode::Up => self.navigate(-1),
            KeyCode::PageDown => self.navigate(self.navigation_page_size()),
            KeyCode::PageUp => self.navigate(-self.navigation_page_size()),
            KeyCode::Home => {
                if self.focus_detail {
                    self.detail_scroll = (0, 0);
                } else {
                    self.selected = 0;
                    self.selection_updated();
                }
            }
            KeyCode::End => {
                if self.focus_detail {
                    self.detail_scroll.0 = self
                        .detail
                        .lines()
                        .count()
                        .saturating_sub(5)
                        .min(u16::MAX as usize) as u16;
                } else {
                    self.selected = self.visible().len().saturating_sub(1);
                    self.selection_updated();
                    self.history_scroll_requested = self.near_history_end();
                }
            }
            KeyCode::Enter => {
                if let Some(Item::Change { id, .. }) = self.current() {
                    self.parent = Some(ParentView {
                        view: self.view.clone(),
                        selected: self.selected,
                        filter: std::mem::take(&mut self.filter),
                        list_states: std::mem::take(&mut self.list_states),
                    });
                    self.view.items.clear();
                    self.invalidate_rows();
                    self.view.history = None;
                    self.drill = Some(id);
                    self.selected = 0;
                    self.filter.clear();
                    self.selection_updated();
                    self.refresh_needed = true;
                } else {
                    self.focus_detail = true;
                }
            }
            KeyCode::F(1) => self.popup = Some(Popup::Help),
            KeyCode::F(5) => self.refresh_needed = true,
            KeyCode::Char(c) if c.is_ascii_digit() && c != '0' => {
                self.switch_tab(Tab::ALL[c as usize - '1' as usize])
            }
            KeyCode::Char('+') if self.tab.is_history() => {
                if self.drill.is_some() {
                    self.status =
                        "Affected files are limited to 200; use p4 describe for the complete list."
                            .into();
                } else if self.busy {
                    self.status =
                        "Wait for the current request before fetching another history page.".into();
                } else if self.view.history.as_ref().and_then(|p| p.next).is_some() {
                    self.history_paging_blocked = false;
                    self.history_more_requested = true;
                } else {
                    self.status = "End of history; r checks for new changes.".into();
                }
            }
            KeyCode::Char('+') => {
                self.limit = (self.limit + self.config.page_size).min(10000);
                self.refresh_needed = true;
            }
            KeyCode::Char(c) => self.char_key(c)?,
            _ => {}
        }
        Ok(())
    }
    fn navigate(&mut self, amount: i32) {
        if self.focus_detail {
            let max = self
                .detail
                .lines()
                .count()
                .saturating_sub(1)
                .min(u16::MAX as usize) as u16;
            self.detail_scroll.0 =
                (self.detail_scroll.0 as i32 + amount).clamp(0, max as i32) as u16;
        } else {
            let max = self.visible().len().saturating_sub(1);
            self.selected = self
                .selected
                .saturating_add_signed(amount as isize)
                .min(max);
            self.selection_updated();
            if amount > 0 {
                self.history_scroll_requested = self.near_history_end();
            }
        }
    }

    pub fn set_history_rows(&mut self, rows: usize) {
        self.history_rows = rows.max(1);
    }
    fn history_batch_size(&self) -> usize {
        self.history_rows
            .min(self.config.history_page_size)
            .clamp(1, 200)
    }
    fn navigation_page_size(&self) -> i32 {
        if self.tab.is_history() && self.drill.is_none() && !self.focus_detail {
            self.history_rows.min(i32::MAX as usize) as i32
        } else {
            15
        }
    }
    fn near_history_end(&self) -> bool {
        self.tab.is_history()
            && self.drill.is_none()
            && !self.focus_detail
            && !self.searching
            && self.filter.is_empty()
            && !self.view.items.is_empty()
            && self
                .selected
                .saturating_add((self.history_rows / 4).clamp(1, 5))
                >= self.view.items.len().saturating_sub(1)
    }
    fn history_page_needed(&self) -> bool {
        self.tab.is_history()
            && self.drill.is_none()
            && !self.history_paging_blocked
            && self.popup.is_none()
            && !self.searching
            && self.filter.is_empty()
            && self
                .view
                .history
                .as_ref()
                .is_some_and(|page| page.next.is_some())
            && (self.view.items.len() < self.history_rows
                || (self.history_scroll_requested && self.near_history_end()))
    }
    pub fn scroll(&mut self, amount: i32, list: bool) {
        if self.popup.is_some() {
            self.popup_scroll =
                (self.popup_scroll as i32 + amount).clamp(0, u16::MAX as i32) as u16;
        } else if !self.searching {
            self.focus_detail = !list;
            self.navigate(amount);
        }
    }
    fn char_key(&mut self, c: char) -> Result<()> {
        let bound = |action| c == self.config.key(action);
        if bound("quit") {
            self.request_quit();
        } else if bound("help") {
            self.popup = Some(Popup::Help);
        } else if bound("refresh") {
            self.refresh_needed = true;
        } else if bound("search") {
            self.searching = true;
        } else if bound("up") {
            self.navigate(-1);
        } else if bound("down") {
            self.navigate(1);
        } else if bound("status_pane") && self.tab == Tab::Status {
            self.jump_status_pane();
        } else if bound("edit_client") {
            if self.busy {
                self.message("Busy", "Wait for the current Perforce request to finish.");
            } else {
                self.status = "Reading current client specification…".into();
                self.send(Request::ClientSpec)?;
            }
        } else if bound("sync") {
            self.confirm(Action::Sync,"Sync the current directory and descendants to depot HEAD? Perforce may update local files. Opened files are preserved by normal p4 sync semantics.");
        } else if bound("new_change") {
            if self.tab == Tab::Streams {
                self.input(
                    InputKind::NewStream(self.view.info.stream.clone()),
                    String::new(),
                );
            } else {
                self.input(InputKind::NewChange, String::new());
            }
        } else if c == 'A' {
            self.confirm(
                Action::Reconcile(None, self.target.clone()),
                format!(
                    "Reconcile ALL offline changes in this directory into changelist {}?",
                    self.target
                ),
            );
        } else if bound("submit") {
            let id = if self.tab == Tab::Changes && self.drill.is_some() {
                self.drill.clone().unwrap_or_default()
            } else {
                match self.current() {
                    Some(Item::Change { id, .. }) if self.tab == Tab::Changes => id,
                    _ => self.target.clone(),
                }
            };
            if id == "default" {
                self.input(InputKind::SubmitDefault, String::new());
            } else {
                self.confirm(Action::Submit(id.clone(),String::new()),format!("Submit ALL files in changelist {id} to the depot using its saved description? This publishes the change for other users."));
            }
        } else if let Some(item) = self.current() {
            match item {
                Item::File(f) => {
                    if bound("open") {
                        if !self.busy {
                            self.mutating = true;
                            self.send(Request::Edit(f, self.target.clone()))?;
                        }
                    } else if bound("reconcile") || c == ' ' {
                        self.act(if f.opened {
                            Action::Reopen(f, self.target.clone())
                        } else {
                            Action::Reconcile(Some(f), self.target.clone())
                        })?;
                    } else if bound("edit") {
                        self.act(Action::Edit(f, self.target.clone()))?;
                    } else if bound("revert") {
                        self.confirm(Action::Revert(f.clone(),false),format!("REVERT {}?\n\nLocal edits will be lost. Added files remain on disk. Shelve work first if it may be needed.",f.depot));
                    } else if bound("unshelve") {
                        self.confirm(Action::Revert(f.clone(),true),format!("Remove {} from its changelist while KEEPING workspace content?\n\nPerforce revert -k changes metadata only. Deleted files stay deleted.",f.depot));
                    } else if bound("move_change") {
                        self.input(InputKind::Reopen(f), self.target.clone());
                    } else if bound("delete") {
                        self.confirm(Action::Delete(f.clone(), self.target.clone()),format!("Open {} for DELETE in changelist {}? This removes the local file; the depot deletion happens on submit.",f.depot, self.target));
                    } else if bound("rename") {
                        self.input(InputKind::Move(f), String::new());
                    } else if bound("resolve") {
                        self.input(InputKind::Resolve(f), "-am".into());
                    } else if bound("annotate") {
                        self.request_detail(Detail::Annotate(f), "Annotate — submitted revisions")?;
                    } else if bound("filelog") {
                        self.request_detail(Detail::FileLog(f), "File history / integrations")?;
                    } else if bound("revision") {
                        self.input(InputKind::Revision(f), String::new());
                    } else if bound("lock") {
                        self.act(Action::Lock(f, true))?;
                    } else if c == 'l' {
                        self.act(Action::Lock(f, false))?;
                    }
                }
                Item::Revision {
                    depot, revision, ..
                } => {
                    let file = FileEntry {
                        depot: depot.clone(),
                        ..Default::default()
                    };
                    if bound("filelog") {
                        self.request_detail(Detail::FileLog(file), "File history")?;
                    } else if bound("annotate") {
                        self.request_detail(
                            Detail::Annotate(FileEntry {
                                depot: format!("{depot}#{revision}"),
                                ..Default::default()
                            }),
                            "Annotate revision",
                        )?;
                    }
                }
                Item::Change {
                    id, description, ..
                } => {
                    if self.tab == Tab::Changes {
                        if c == ' ' || bound("open") {
                            self.target = id.clone();
                            self.status = format!("Active changelist: {id}");
                        } else if bound("shelve") {
                            self.confirm(Action::Shelve(id.clone()),format!("Shelve files in changelist {id}?\nExisting shelved revisions are never overwritten automatically. Default must first be moved to a numbered changelist."));
                        } else if bound("edit_change") {
                            self.input(InputKind::EditChange(id), description);
                        } else if bound("delete") {
                            self.confirm(Action::DeleteChange(id.clone()),format!("Delete empty pending changelist {id}? Perforce refuses nonempty changelists."));
                        }
                    } else if self.tab == Tab::Shelves {
                        if bound("unshelve") || c == ' ' {
                            self.input(InputKind::Unshelve(id), self.target.clone());
                        } else if bound("delete") {
                            self.confirm(
                                Action::DeleteShelf(id.clone()),
                                format!(
                                    "Permanently DELETE all shelved revisions in changelist {id}?"
                                ),
                            );
                        }
                    }
                }
                Item::Stream { path, .. } => {
                    if bound("open") || c == ' ' {
                        self.confirm(Action::SwitchStream(path.clone()),format!("Switch workspace to stream {path} and sync its files? This requires a clean stream workspace."));
                    } else if bound("delete") {
                        self.confirm(Action::DeleteStream(path.clone()),format!("Delete stream specification {path}? Submitted file history is retained. Perforce checks for active clients and children."));
                    } else if c == 'g' {
                        self.confirm(Action::MergeStream(path.clone(),self.target.clone()),format!("Merge files from {path} into the CURRENT stream {} in changelist {}?
Review and resolve the result before submitting.",self.view.info.stream,self.target));
                    } else if c == 'p' {
                        self.confirm(Action::CopyStream(path.clone(),self.target.clone()),format!("Copy files from {path} into the CURRENT stream {} in changelist {}?
This may replace target content. Inspect the pending change before submitting.",self.view.info.stream,self.target));
                    }
                }
                Item::Workspace { name, .. } if bound("open") || c == ' ' => {
                    self.confirm(Action::SwitchWorkspace(name.clone()),format!("Use workspace {name} for this session? The working directory will change to its root."));
                }
                _ => {}
            }
        }
        Ok(())
    }
    fn request_detail(&mut self, detail: Detail, title: &str) -> Result<()> {
        if self.busy {
            self.status = "Wait for the current request, then retry.".into();
            return Ok(());
        }
        self.detail_needed = false;
        self.focus_detail = true;
        self.detail_scroll = (0, 0);
        self.detail_title = title.into();
        self.send(Request::Detail(self.current_key(), detail))
    }
    fn popup_key(&mut self, key: KeyEvent) -> Result<()> {
        let Some(popup) = self.popup.take() else {
            return Ok(());
        };
        match popup {
            Popup::Confirm { action, text } => match key.code {
                KeyCode::Char('y') | KeyCode::Char('Y') => self.act(action)?,
                KeyCode::Esc | KeyCode::Char('n') | KeyCode::Char('N') => {
                    if matches!(action, Action::SaveClientSpec { .. }) {
                        self.client_draft = None;
                    }
                }
                _ => self.popup = Some(Popup::Confirm { action, text }),
            },
            Popup::Input { kind, mut value } => {
                let submit = key.code == KeyCode::Enter
                    && (!kind.multiline() || key.modifiers.contains(KeyModifiers::CONTROL))
                    || key.code == KeyCode::Char('s')
                        && key.modifiers.contains(KeyModifiers::CONTROL);
                if key.code == KeyCode::Esc {
                    return Ok(());
                }
                if submit {
                    if value.trim().is_empty() {
                        self.popup = Some(Popup::Input { kind, value });
                        return Ok(());
                    }
                    match kind {
                        InputKind::NewChange => self.act(Action::NewChange(value))?,
                        InputKind::NewStream(parent) => self.confirm(Action::CreateStream(value.trim().into(),parent.clone()),format!("Create stream specification {} with parent {}? File content is populated separately using copy or merge.",value.trim(),if parent.is_empty() {"none (mainline)"} else {&parent})),
                        InputKind::EditChange(id) => self.act(Action::EditChange(id,value))?,
                        InputKind::SubmitDefault => self.confirm(Action::Submit("default".into(),value.clone()),format!("Submit ALL files in the DEFAULT changelist?\n\n{value}\n\nThis publishes the change for other users.")),
                        InputKind::Reopen(f) => self.act(Action::Reopen(f,value.trim().into()))?,
                        InputKind::Unshelve(id) => self.confirm(Action::Unshelve(id.clone(),value.trim().into()),format!("Unshelve {id} into {}? This may modify workspace files and require resolve. No force overwrite is used.",value.trim())),
                        InputKind::Move(f) => self.confirm(Action::Move(f.clone(),value.clone()),format!("Move {} to {value}?",f.depot)),
                        InputKind::Revision(f) => self.request_detail(Detail::Revision(f,value.trim().into()),"Depot revision")?,
                        InputKind::Resolve(f) => self.confirm(Action::Resolve(f.clone(),value.trim().into()),format!("Resolve {} with {}?\n\n-am merges nonconflicting changes; -as is safe-only; -ay discards incoming changes; -at discards your changes. Conflicts left by automatic merge require an external merge tool.",f.depot,value.trim())),
                    }
                } else {
                    match key.code {
                        KeyCode::Backspace => {
                            value.pop();
                        }
                        KeyCode::Enter if kind.multiline() => value.push('\n'),
                        KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                            value.clear()
                        }
                        KeyCode::Char(c)
                            if !key.modifiers.contains(KeyModifiers::CONTROL)
                                && value.len() < 65536 =>
                        {
                            value.push(c)
                        }
                        _ => {}
                    }
                    self.popup = Some(Popup::Input { kind, value });
                }
            }
            popup => match key.code {
                KeyCode::Esc | KeyCode::Enter | KeyCode::Char('q') | KeyCode::Char('?') => {}
                KeyCode::Down | KeyCode::Char('j') => {
                    self.popup_scroll = self.popup_scroll.saturating_add(1);
                    self.popup = Some(popup);
                }
                KeyCode::Up | KeyCode::Char('k') => {
                    self.popup_scroll = self.popup_scroll.saturating_sub(1);
                    self.popup = Some(popup);
                }
                KeyCode::PageDown => {
                    self.popup_scroll = self.popup_scroll.saturating_add(15);
                    self.popup = Some(popup);
                }
                KeyCode::PageUp => {
                    self.popup_scroll = self.popup_scroll.saturating_sub(15);
                    self.popup = Some(popup);
                }
                _ => self.popup = Some(popup),
            },
        }
        Ok(())
    }
    fn request_quit(&mut self) {
        if self.mutating {
            self.status =
                "A mutating command is running. Wait for completion before quitting.".into();
        } else {
            self.quit = true;
        }
    }
    pub fn refresh(&mut self) {
        self.refresh_needed = true;
    }
    pub fn editor_error(&mut self, error: String) {
        self.message("Editor failed", error);
    }
    pub fn editor_finished(&mut self, edit: ExternalEdit, result: Result<()>) {
        match edit {
            ExternalEdit::File(_) => {
                if let Err(error) = result {
                    self.editor_error(format!("{error:#}"));
                }
            }
            ExternalEdit::ClientSpec {
                name,
                original,
                path,
            } => {
                let edited = result.and_then(|()| {
                    let maximum = (self.config.max_output_mb as u64) * 1024 * 1024;
                    let mut form = String::new();
                    std::fs::File::open(&path)?
                        .take(maximum + 1)
                        .read_to_string(&mut form)?;
                    anyhow::ensure!(
                        form.len() as u64 <= maximum,
                        "Client specification exceeds the output limit"
                    );
                    Ok(form)
                });
                match edited {
                    Ok(form) if form == original => {
                        self.status = "Client specification unchanged; nothing saved.".into();
                    }
                    Ok(form) => {
                        self.client_draft = Some(path);
                        self.confirm(
                            Action::SaveClientSpec { name: name.clone(), form },
                            format!("Save the edited client specification for {name}?\n\nThis updates the workspace specification on the server. Files are not synced. Press n or Esc to discard the draft."),
                        );
                    }
                    Err(error) => self.editor_error(format!("{error:#}{}", preserve_draft(path))),
                }
            }
        }
        self.refresh_needed = true;
    }
    pub fn paste(&mut self, text: &str) {
        if let Some(Popup::Input { kind, value }) = &mut self.popup {
            for c in text
                .chars()
                .filter(|c| !c.is_control() || *c == '\n' || *c == '\t')
            {
                if value.len() >= 65536 {
                    break;
                }
                if c == '\n' && !kind.multiline() {
                    continue;
                }
                value.push(c);
            }
        }
    }
}

fn preserve_draft(path: tempfile::TempPath) -> String {
    match path.keep() {
        Ok(path) => format!(
            "\n\nYour edited client specification is preserved at {}",
            path.display()
        ),
        Err(mut error) => {
            let path = error.path.to_path_buf();
            error.path.disable_cleanup(true);
            format!(
                "\n\nCould not persist the draft: {}. Recover it from {}",
                error.error,
                path.display()
            )
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use crossterm::event::KeyModifiers;
    fn key(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE)
    }
    fn status_view(opened: usize, offline: usize) -> View {
        View {
            info: Info {
                user: "tester".into(),
                client: "test".into(),
                root: "/workspace".into(),
                ..Default::default()
            },
            items: [(true, opened), (false, offline)]
                .into_iter()
                .flat_map(|(open, count)| {
                    (0..count).map(move |n| {
                        Item::File(FileEntry {
                            depot: format!(
                                "//depot/{}_file{n:03}",
                                if open { "opened" } else { "offline" }
                            ),
                            opened: open,
                            ..Default::default()
                        })
                    })
                })
                .collect(),
            ..Default::default()
        }
    }
    fn reply(app: &mut App, response: Response) {
        let (send, _requests) = mpsc::channel();
        let (responses, receive) = mpsc::channel();
        app.worker = Some(Worker {
            send,
            receive,
            cancel: Arc::new(AtomicBool::new(false)),
            handle: None,
        });
        app.refresh_needed = false;
        app.selection_changed = Instant::now();
        responses.send(response).unwrap();
        app.tick().unwrap();
        app.worker = None;
        app.busy = false;
    }
    #[test]
    fn status_navigation_crosses_the_opened_offline_boundary() {
        let mut app = App::for_view(Config::default(), status_view(2, 2));
        app.key(key('j')).unwrap();
        assert_eq!(app.current().unwrap().key(), "//depot/opened_file001");
        app.key(key('j')).unwrap();
        assert_eq!(app.current().unwrap().key(), "//depot/offline_file000");
        app.key(key('k')).unwrap();
        assert_eq!(app.current().unwrap().key(), "//depot/opened_file001");
    }
    #[test]
    fn fresh_view_invalidates_filter_indices_even_when_reusing_the_same_allocation() {
        let mut app = App::for_view(Config::default(), status_view(4, 0));
        app.filter = "opened".into();
        assert_eq!(app.visible().len(), 4);
        let mut fresh = std::mem::take(&mut app.view);
        if let Item::File(file) = &mut fresh.items[0] {
            file.depot = "//depot/renamed".into();
        }
        reply(&mut app, Response::Load(Tab::Status, Ok(fresh)));
        assert_eq!(app.visible().len(), 3);
    }
    #[test]
    fn idle_ticks_do_not_request_rendering_and_deferred_work_has_a_deadline() {
        let mut app = App::for_view(
            Config {
                refresh_seconds: 0,
                ..Config::default()
            },
            View::default(),
        );
        app.detail_needed = false;
        for _ in 0..10 {
            assert!(!app.tick().unwrap());
        }
        assert_eq!(app.next_wake(), Duration::from_millis(250));
        app.detail_needed = true;
        app.selection_changed = Instant::now();
        assert!(app.next_wake() <= Duration::from_millis(100));
        app.selection_changed -= Duration::from_secs(1);
        assert_eq!(app.next_wake(), Duration::ZERO);
        assert!(app.tick().unwrap());
        assert!(!app.tick().unwrap());
    }
    #[test]
    fn automatic_refresh_does_not_queue_a_second_scan_while_loading() {
        let mut app = App::for_view(Config::default(), status_view(2, 2));
        let (send, requests) = mpsc::channel();
        let (responses, receive) = mpsc::channel();
        app.worker = Some(Worker {
            send,
            receive,
            cancel: Arc::new(AtomicBool::new(false)),
            handle: None,
        });
        app.detail_needed = false;
        app.last_refresh = Instant::now() - Duration::from_secs(60);
        app.tick().unwrap();
        assert!(matches!(
            requests.try_recv().unwrap(),
            Request::Load(Tab::Status, _)
        ));
        for _ in 0..10 {
            app.tick().unwrap();
        }
        assert!(!app.refresh_needed);
        responses
            .send(Response::Load(Tab::Status, Ok(status_view(2, 2))))
            .unwrap();
        app.tick().unwrap();
        assert!(
            requests.try_recv().is_err(),
            "Automatic scan was duplicated"
        );
        // A manual refresh requested while a scan is busy still runs once.
        app.refresh();
        app.tick().unwrap();
        assert!(matches!(
            requests.try_recv().unwrap(),
            Request::Load(Tab::Status, _)
        ));
        app.refresh();
        responses
            .send(Response::Load(Tab::Status, Ok(status_view(2, 2))))
            .unwrap();
        app.tick().unwrap();
        assert!(matches!(
            requests.try_recv().unwrap(),
            Request::Load(Tab::Status, _)
        ));
        assert!(requests.try_recv().is_err());
    }
    #[test]
    fn status_pane_shortcut_jumps_past_large_opened_lists_and_leaves_diff_focus() {
        let mut app = App::for_view(Config::default(), status_view(10_000, 3));
        app.selected = 5_000;
        app.focus_detail = true;
        app.detail_scroll = (100, 20);
        app.key(key('t')).unwrap();
        assert_eq!(app.current().unwrap().key(), "//depot/offline_file000");
        assert!(!app.focus_detail);
        assert_eq!(app.detail_scroll, (0, 0));
        app.key(key('t')).unwrap();
        assert_eq!(app.current().unwrap().key(), "//depot/opened_file000");
    }
    #[test]
    fn status_pane_shortcut_uses_visible_rows_and_reports_empty_destinations() {
        let mut app = App::for_view(Config::default(), status_view(80, 80));
        app.filter = "file07".into();
        app.key(key('t')).unwrap();
        assert_eq!(app.current().unwrap().key(), "//depot/offline_file070");
        assert_eq!(app.filter, "file07");
        app.key(key('t')).unwrap();
        assert_eq!(app.current().unwrap().key(), "//depot/opened_file070");
        app.filter = "opened".into();
        app.selected = 5;
        app.key(key('t')).unwrap();
        assert_eq!(app.selected, 5);
        assert!(
            app.status
                .contains("Offline / untracked files match the current filter")
        );
        for (opened, offline) in [(0, 0), (4, 0), (0, 4)] {
            let mut app = App::for_view(Config::default(), status_view(opened, offline));
            app.key(key('t')).unwrap();
            assert_eq!(app.selected, 0);
            assert!(app.status.starts_with("No "));
        }
    }
    #[test]
    fn status_pane_shortcut_is_remappable_and_scoped_to_status() {
        let mut config = Config::default();
        config.keys.insert("status_pane".into(), "w".into());
        let mut app = App::for_view(config, status_view(4, 4));
        app.key(key('t')).unwrap();
        assert_eq!(app.selected, 0);
        app.key(key('w')).unwrap();
        assert_eq!(app.selected, 4);
        app.tab = Tab::Files;
        app.key(key('w')).unwrap();
        assert_eq!(app.selected, 4);
    }
    #[test]
    fn client_form_requires_confirmation_and_discard_removes_the_draft() {
        for cancel in [KeyCode::Esc, KeyCode::Char('n')] {
            let mut app = App::for_view(Config::default(), View::default());
            let edit = ExternalEdit::client_spec("test".into(), "Client: test\n".into()).unwrap();
            let path = edit.path().to_path_buf();
            std::fs::write(&path, "Client: test\nDescription:\n\tNew description\n").unwrap();
            app.editor_finished(edit, Ok(()));
            assert!(
                matches!(&app.popup, Some(Popup::Confirm { action: Action::SaveClientSpec { name, form }, .. })
                if name == "test" && form.contains("New description"))
            );
            app.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
                .unwrap();
            assert!(matches!(app.popup, Some(Popup::Confirm { .. })));
            assert!(path.exists());
            app.key(KeyEvent::new(cancel, KeyModifiers::NONE)).unwrap();
            assert!(app.popup.is_none());
            assert!(!path.exists());
        }
    }
    #[test]
    fn unchanged_client_form_does_not_request_a_save() {
        let mut app = App::for_view(Config::default(), View::default());
        let edit = ExternalEdit::client_spec("test".into(), "Client: test\n".into()).unwrap();
        let path = edit.path().to_path_buf();
        app.editor_finished(edit, Ok(()));
        assert!(app.popup.is_none());
        assert!(app.status.contains("unchanged"));
        assert!(!path.exists());
    }
    #[test]
    fn client_editor_and_save_failures_preserve_recoverable_drafts() {
        for editor_failed in [true, false] {
            let mut app = App::for_view(Config::default(), View::default());
            let edit = ExternalEdit::client_spec("test".into(), "Client: test\n".into()).unwrap();
            let path = edit.path().to_path_buf();
            let edited = "Client: test\nDescription:\n\tImportant edits\n";
            std::fs::write(&path, edited).unwrap();
            app.editor_finished(
                edit,
                if editor_failed {
                    Err(anyhow::anyhow!("Editor failed"))
                } else {
                    Ok(())
                },
            );
            if !editor_failed {
                let Some(Popup::Confirm { action, .. }) = app.popup.take() else {
                    panic!("Expected confirmation");
                };
                reply(
                    &mut app,
                    Response::Act(action, Err(anyhow::anyhow!("Permission denied"))),
                );
            }
            assert!(
                matches!(&app.popup, Some(Popup::Message { text, .. }) if text.contains(path.to_string_lossy().as_ref()))
            );
            drop(app);
            assert_eq!(std::fs::read_to_string(&path).unwrap(), edited);
            std::fs::remove_file(path).unwrap();
        }
    }
    #[test]
    fn successful_client_save_removes_the_draft_and_refreshes() {
        let mut app = App::for_view(Config::default(), View::default());
        let edit = ExternalEdit::client_spec("test".into(), "Client: test\n".into()).unwrap();
        let path = edit.path().to_path_buf();
        std::fs::write(&path, "Client: test\nDescription:\n\tEdited\n").unwrap();
        app.editor_finished(edit, Ok(()));
        let Some(Popup::Confirm { action, .. }) = app.popup.take() else {
            panic!("Expected confirmation");
        };
        app.mutating = true;
        reply(
            &mut app,
            Response::Act(action, Ok("Client test saved.".into())),
        );
        assert!(!path.exists());
        assert!(app.client_draft.is_none());
        assert!(matches!(app.popup, Some(Popup::Message { .. })));
    }
    #[test]
    fn status_roundtrip_preserves_selection_filter_and_each_panes_top_file() {
        let mut app = App::for_view(Config::default(), status_view(80, 80));
        app.filter = "file".into();
        app.selected = 132;
        *app.list_states[0].offset_mut() = 51;
        *app.list_states[1].offset_mut() = 43;
        app.switch_tab(Tab::Labels);
        app.switch_tab(Tab::Status);
        // Another round trip while the reload is pending cannot overwrite the bookmark.
        app.switch_tab(Tab::Files);
        app.switch_tab(Tab::Status);
        let mut fresh = status_view(80, 80);
        fresh.items.insert(
            0,
            Item::File(FileEntry {
                depot: "//depot/000_new_file".into(),
                opened: true,
                ..Default::default()
            }),
        );
        reply(&mut app, Response::Load(Tab::Status, Ok(fresh)));
        assert_eq!(app.current().unwrap().key(), "//depot/offline_file052");
        assert_eq!(app.selected, 133);
        assert_eq!(app.filter, "file");
        assert_eq!(app.list_states[0].offset(), 52);
        assert_eq!(app.list_states[1].offset(), 43);
        let selected = app.selected;
        app.switch_tab(Tab::Status);
        assert_eq!(app.selected, selected);
        assert!(!app.view.items.is_empty());
    }
    #[test]
    fn missing_status_selection_falls_back_inside_the_same_pane() {
        let mut app = App::for_view(Config::default(), status_view(20, 20));
        app.selected = 30;
        app.switch_tab(Tab::Labels);
        app.switch_tab(Tab::Status);
        let mut fresh = status_view(120, 20);
        fresh
            .items
            .retain(|item| item.key() != "//depot/offline_file010");
        reply(&mut app, Response::Load(Tab::Status, Ok(fresh)));
        assert_eq!(app.current().unwrap().key(), "//depot/offline_file011");
    }
    #[test]
    fn failed_status_reload_retains_the_bookmark_for_retry() {
        let mut app = App::for_view(Config::default(), status_view(20, 20));
        app.selected = 30;
        *app.list_states[1].offset_mut() = 6;
        app.switch_tab(Tab::Labels);
        app.switch_tab(Tab::Status);
        reply(
            &mut app,
            Response::Load(Tab::Status, Err(anyhow::anyhow!("Temporary failure"))),
        );
        assert!(app.status_restore_pending);
        app.popup = None;
        app.switch_tab(Tab::Labels);
        app.switch_tab(Tab::Status);
        reply(
            &mut app,
            Response::Load(Tab::Status, Ok(status_view(20, 20))),
        );
        assert_eq!(app.current().unwrap().key(), "//depot/offline_file010");
        assert_eq!(app.list_states[1].offset(), 6);
    }
    #[test]
    fn changed_filter_during_reload_takes_precedence_over_status_bookmark() {
        let mut app = App::for_view(Config::default(), status_view(20, 20));
        app.filter = "file".into();
        app.selected = 30;
        app.switch_tab(Tab::Labels);
        app.switch_tab(Tab::Status);
        app.filter = "opened".into();
        // Preserve this newly typed filter over another pending tab round trip.
        app.switch_tab(Tab::Labels);
        app.switch_tab(Tab::Status);
        reply(
            &mut app,
            Response::Load(Tab::Status, Ok(status_view(20, 20))),
        );
        assert_eq!(app.filter, "opened");
        assert_eq!(app.current().unwrap().key(), "//depot/opened_file000");
        assert_eq!(app.list_states[0].offset(), 0);
    }
    #[test]
    fn status_bookmark_is_invalidated_by_workspace_changes() {
        let mut app = App::for_view(Config::default(), status_view(20, 20));
        app.selected = 30;
        app.filter = "file".into();
        app.target = "123".into();
        app.switch_tab(Tab::Labels);
        let mut fresh = status_view(20, 20);
        fresh.info.client = "other".into();
        // Other tabs may already have observed the new workspace.
        app.view.info = fresh.info.clone();
        app.switch_tab(Tab::Status);
        reply(&mut app, Response::Load(Tab::Status, Ok(fresh)));
        assert_eq!(app.selected, 0);
        assert!(app.filter.is_empty());
        assert_eq!(app.target, "default");
        assert_eq!(app.list_states[1].offset(), 0);
    }
    #[test]
    fn first_status_load_keeps_a_target_selected_in_another_tab() {
        let mut app = App::for_view(Config::default(), View::default());
        app.switch_tab(Tab::Changes);
        app.view.info = status_view(0, 0).info;
        app.target = "123".into();
        app.switch_tab(Tab::Status);
        reply(&mut app, Response::Load(Tab::Status, Ok(status_view(2, 2))));
        assert_eq!(app.target, "123");
    }
    #[test]
    fn only_successful_workspace_or_stream_switches_invalidate_status_navigation() {
        for action in [
            Action::SwitchWorkspace("other".into()),
            Action::SwitchStream("//streams/dev".into()),
        ] {
            for success in [true, false] {
                let mut app = App::for_view(Config::default(), status_view(20, 20));
                app.selected = 30;
                app.target = "123".into();
                app.switch_tab(Tab::Workspaces);
                reply(
                    &mut app,
                    Response::Act(
                        action.clone(),
                        if success {
                            Ok("Switched".into())
                        } else {
                            Err(anyhow::anyhow!("Switch failed"))
                        },
                    ),
                );
                assert_eq!(app.status_state.is_none(), success);
                assert_eq!(app.target, if success { "default" } else { "123" });
            }
        }
    }
    #[test]
    fn destructive_actions_require_explicit_yes() {
        let mut app = App::for_view(
            Config::default(),
            View {
                items: vec![Item::File(FileEntry {
                    depot: "//depot/a".into(),
                    opened: true,
                    ..Default::default()
                })],
                ..Default::default()
            },
        );
        app.key(key('U')).unwrap();
        assert!(matches!(app.popup, Some(Popup::Confirm { .. })));
        app.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
            .unwrap();
        assert!(matches!(app.popup, Some(Popup::Confirm { .. })));
        app.key(key('n')).unwrap();
        assert!(app.popup.is_none());
        app.target = "123".into();
        app.key(key('D')).unwrap();
        assert!(
            matches!(app.popup, Some(Popup::Confirm { ref action, ref text })
            if matches!(action, Action::Delete(_, change) if change == "123")
                && text.contains("changelist 123"))
        );
    }
    #[test]
    fn successful_submit_or_delete_resets_only_the_removed_target() {
        for action in [
            Action::Submit("123".into(), String::new()),
            Action::DeleteChange("123".into()),
        ] {
            for target in ["123", "999"] {
                for success in [false, true] {
                    let mut app = App::for_view(Config::default(), View::default());
                    app.target = target.into();
                    let (send, _requests) = mpsc::channel();
                    let (responses, receive) = mpsc::channel();
                    app.worker = Some(Worker {
                        send,
                        receive,
                        cancel: Arc::new(AtomicBool::new(false)),
                        handle: None,
                    });
                    responses
                        .send(Response::Act(
                            action.clone(),
                            if success {
                                Ok("Completed".into())
                            } else {
                                Err(anyhow::anyhow!("Command failed"))
                            },
                        ))
                        .unwrap();
                    app.tick().unwrap();
                    assert_eq!(
                        app.target,
                        if success && target == "123" {
                            "default"
                        } else {
                            target
                        },
                        "action={action:?}, success={success}"
                    );
                }
            }
        }
    }
    fn history_view(id: &str, next: Option<HistoryCursor>) -> View {
        View {
            items: vec![Item::Change {
                id: id.into(),
                description: "test".into(),
                user: "user".into(),
                client: "client".into(),
                status: "submitted".into(),
            }],
            history: Some(HistoryPage {
                first_change: 1,
                last_change: 100,
                next,
                requested_scan_limit: Some(50000),
                group_scan_limit: None,
            }),
            ..Default::default()
        }
    }
    #[test]
    fn history_load_more_appends_and_drilldown_retains_the_cursor() {
        let mut app = App::for_view(
            Config::default(),
            history_view("100", Some(HistoryCursor::new(99))),
        );
        app.tab = Tab::Log;
        app.key(key('+')).unwrap();
        assert!(app.history_more_requested);
        assert_eq!(app.limit, 200);
        app.history_more_requested = false;
        let mut page = history_view("90", Some(HistoryCursor::new(89)));
        page.items.push(app.view.items[0].clone());
        app.apply_history_page(page, true);
        assert_eq!(app.view.items.len(), 2);
        assert_eq!(app.view.items[0].key(), "100");
        app.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
            .unwrap();
        assert!(app.drill.is_some());
        app.view = View::default();
        app.key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE))
            .unwrap();
        assert_eq!(app.view.items.len(), 2);
        assert_eq!(
            app.view
                .history
                .as_ref()
                .and_then(|page| page.next)
                .map(|c| c.before),
            Some(89)
        );
        assert!(!app.refresh_needed);
    }
    #[test]
    fn history_does_not_poll_automatically_or_queue_more_while_busy() {
        let mut app = App::for_view(
            Config::default(),
            history_view("100", Some(HistoryCursor::new(99))),
        );
        app.tab = Tab::Log;
        app.detail_needed = false;
        app.last_refresh = Instant::now() - Duration::from_secs(60);
        app.tick().unwrap();
        assert!(!app.refresh_needed);
        app.busy = true;
        app.key(key('+')).unwrap();
        assert!(!app.history_more_requested);
    }
    #[test]
    fn drilldown_restores_the_filtered_selection_and_viewport() {
        for tab in [Tab::Log, Tab::Shelves, Tab::Changes] {
            let mut view = history_view("100", Some(HistoryCursor::new(59)));
            view.items = (60..=100)
                .rev()
                .map(|id| Item::Change {
                    id: id.to_string(),
                    description: if id % 2 == 0 { "matching" } else { "other" }.into(),
                    user: "user".into(),
                    client: "client".into(),
                    status: "submitted".into(),
                })
                .collect();
            let mut app = App::for_view(Config::default(), view);
            app.tab = tab;
            app.filter = "matching".into();
            app.selected = 10;
            app.list_states[2] = ratatui::widgets::ListState::default()
                .with_offset(5)
                .with_selected(Some(10));
            assert_eq!(app.current().unwrap().key(), "80");
            app.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
                .unwrap();
            assert!(app.filter.is_empty());
            assert_eq!(app.list_states[2].offset(), 0);
            app.view = View::default();
            app.key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE))
                .unwrap();
            assert_eq!(app.filter, "matching");
            assert_eq!(app.current().unwrap().key(), "80");
            assert_eq!(app.list_states[2].offset(), 5);
            assert_eq!(app.view.history.unwrap().next, Some(HistoryCursor::new(59)));
        }
    }
    #[test]
    fn unicode_input_and_search_are_safe() {
        let mut app = App::for_view(Config::default(), View::default());
        app.key(key('n')).unwrap();
        app.paste("💥\nFiles:\n//depot/evil");
        app.key(KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE))
            .unwrap();
        assert!(matches!(app.popup,Some(Popup::Input {ref value,..}) if value.ends_with("evi")));
        app.key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE))
            .unwrap();
        app.key(key('/')).unwrap();
        app.key(key('日')).unwrap();
        app.key(KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE))
            .unwrap();
        assert!(app.filter.is_empty());
    }
}
