use crate::{config::Config, model::*, p4::Connection};
use anyhow::{Context, Result};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use std::{
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
}
enum Response {
    History(Tab, Option<HistoryCursor>, u64, Result<View>),
    Load(Tab, Result<View>),
    ChangeFiles(String, Tab, Result<View>),
    Detail(String, Result<String>),
    Act(Action, Result<String>),
    Edit(Result<std::path::PathBuf>),
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
    fn new(mut connection: Connection) -> Self {
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
                };
                if results.send(result).is_err() {
                    break;
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
    pub external_edit: Option<std::path::PathBuf>,
    pub target: String,
    pub limit: usize,
    pub drill: Option<String>,
    parent: Option<(View, usize)>,
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
}
impl App {
    pub fn new(config: Config, connection: Connection) -> Self {
        let mut app = Self::for_view(config, View::default());
        app.worker = Some(Worker::new(connection));
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
            target: "default".into(),
            limit,
            drill: None,
            parent: None,
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
        }
    }
    pub fn visible(&self) -> Vec<&Item> {
        let needle = self.filter.to_lowercase();
        self.view
            .items
            .iter()
            .filter(|i| needle.is_empty() || i.search_text().to_lowercase().contains(&needle))
            .collect()
    }
    pub fn current(&self) -> Option<Item> {
        self.visible().get(self.selected).map(|i| (*i).clone())
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
        self.detail_title = "Diff / details".into();
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
    pub fn tick(&mut self) -> Result<()> {
        let response = self.worker.as_ref().and_then(|w| w.receive.try_recv().ok());
        if let Some(response) = response {
            self.busy = false;
            if !self.mutating
                && self
                    .worker
                    .as_ref()
                    .is_some_and(|worker| worker.cancel.load(Ordering::Relaxed))
            {
                // Discard cancelled reads even after a quick tab round trip.
                return Ok(());
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
                                let key = self.current().map(|i| i.key().to_string());
                                if !self.view.info.client.is_empty()
                                    && self.view.info.client != view.info.client
                                {
                                    self.target = "default".into();
                                    self.filter.clear();
                                    self.selection_updated();
                                }
                                self.view = view;
                                self.selected = key
                                    .and_then(|k| self.visible().iter().position(|i| i.key() == k))
                                    .unwrap_or(self.selected)
                                    .min(self.visible().len().saturating_sub(1));
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
                    }
                }
                Response::Edit(result) => {
                    match result {
                        Ok(path) => self.external_edit = Some(path),
                        Err(e) => self.message("Cannot edit file", format!("{e:#}")),
                    }
                    self.refresh_needed = true;
                }
                Response::Act(action, result) => {
                    match result {
                        Ok(text) => {
                            if matches!(&action, Action::Submit(id, _) | Action::DeleteChange(id)
                                if id == &self.target)
                            {
                                self.target = "default".into();
                            }
                            self.message("Command result", text);
                            self.status = "Completed; refreshing…".into();
                        }
                        Err(e) => self.message(
                            "Command failed — inspect state before retrying",
                            format!("{e:#}"),
                        ),
                    }
                    self.refresh_needed = true;
                }
            }
        }
        if self.config.refresh_seconds > 0
            && !self.tab.is_history()
            && self.drill.is_none()
            && self.popup.is_none()
            && !self.searching
            && self.last_refresh.elapsed() >= Duration::from_secs(self.config.refresh_seconds)
        {
            self.refresh_needed = true;
        }
        if !self.busy && self.refresh_needed {
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
            self.detail_needed = false;
            if let Some(item) = self.current() {
                let key = self.current_key();
                self.detail_title = item.key().to_string();
                self.send(Request::Detail(key, Detail::Item(item, self.tab)))?;
            } else {
                self.detail.clear();
            }
        }
        Ok(())
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
        self.selected = 0;
        self.filter.clear();
        self.focus_detail = false;
        self.refresh_needed = true;
        self.selection_updated();
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
                    if let Some((view, selected)) = self.parent.take() {
                        self.view = view;
                        self.selected = selected;
                    }
                    self.refresh_needed = !self.tab.is_history();
                }
                self.focus_detail = false;
                self.filter.clear();
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
                    self.parent = Some((self.view.clone(), self.selected));
                    self.view.items.clear();
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
                KeyCode::Esc | KeyCode::Char('n') | KeyCode::Char('N') => {}
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

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use crossterm::event::KeyModifiers;
    fn key(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE)
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
