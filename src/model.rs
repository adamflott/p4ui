use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default, clap::ValueEnum)]
pub enum Tab {
    #[default]
    Status,
    Log,
    Changes,
    Shelves,
    Files,
    Streams,
    Resolve,
    Workspaces,
    Labels,
}
impl Tab {
    pub const ALL: [Self; 9] = [
        Self::Status,
        Self::Log,
        Self::Changes,
        Self::Shelves,
        Self::Files,
        Self::Streams,
        Self::Resolve,
        Self::Workspaces,
        Self::Labels,
    ];
    pub fn is_history(self) -> bool {
        matches!(self, Self::Log | Self::Shelves)
    }
    pub fn name(self) -> &'static str {
        match self {
            Self::Status => "Status",
            Self::Log => "Log",
            Self::Changes => "Changes",
            Self::Shelves => "Shelves",
            Self::Files => "Files",
            Self::Streams => "Streams",
            Self::Resolve => "Resolve",
            Self::Workspaces => "Workspaces",
            Self::Labels => "Labels",
        }
    }
    pub fn index(self) -> usize {
        Self::ALL.iter().position(|x| *x == self).unwrap_or(0)
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Info {
    pub user: String,
    pub client: String,
    pub root: String,
    pub server: String,
    pub stream: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct FileEntry {
    /// Server-escaped depot path; safe to pass back to p4 unchanged.
    pub depot: String,
    pub local: String,
    pub action: String,
    pub change: String,
    pub file_type: String,
    pub opened: bool,
    pub unresolved: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Item {
    File(FileEntry),
    Revision {
        depot: String,
        revision: String,
        action: String,
        change: String,
        shelved: bool,
    },
    Change {
        id: String,
        description: String,
        user: String,
        client: String,
        status: String,
    },
    Stream {
        path: String,
        parent: String,
        kind: String,
    },
    Workspace {
        name: String,
        root: String,
        stream: String,
    },
    Label {
        name: String,
        description: String,
    },
}
impl Item {
    pub fn key(&self) -> &str {
        match self {
            Self::File(f) => &f.depot,
            Self::Revision { depot, .. } => depot,
            Self::Change { id, .. } => id,
            Self::Stream { path, .. } => path,
            Self::Workspace { name, .. } | Self::Label { name, .. } => name,
        }
    }
    pub fn search_text(&self) -> String {
        match self {
            Self::Revision {
                depot,
                revision,
                action,
                change,
                ..
            } => format!("{depot}#{revision} {action} {change}"),
            Self::File(f) => format!("{} {} {} {}", f.depot, f.local, f.action, f.change),
            Self::Change {
                id,
                description,
                user,
                client,
                ..
            } => format!("{id} {description} {user} {client}"),
            Self::Stream { path, parent, kind } => format!("{path} {parent} {kind}"),
            Self::Workspace { name, root, stream } => format!("{name} {root} {stream}"),
            Self::Label { name, description } => format!("{name} {description}"),
        }
    }
}

/// The next inclusive upper changelist bound, immediately before the oldest
/// returned change. Changelist-number gaps do not affect the page size.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct HistoryCursor {
    pub before: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shelf_offset: Option<usize>,
}
impl HistoryCursor {
    pub fn new(before: u64) -> Self {
        Self {
            before,
            shelf_offset: None,
        }
    }
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HistoryPage {
    pub first_change: u64,
    pub last_change: u64,
    pub next: Option<HistoryCursor>,
    pub requested_scan_limit: Option<u64>,
    pub group_scan_limit: Option<u64>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct View {
    pub info: Info,
    pub items: Vec<Item>,
    pub warnings: Vec<String>,
    pub limit_reached: bool,
    #[serde(default)]
    pub history: Option<HistoryPage>,
}

#[derive(Debug, Clone)]
pub enum Detail {
    Item(Item, Tab),
    Annotate(FileEntry),
    FileLog(FileEntry),
    Revision(FileEntry, String),
}

#[derive(Debug, Clone)]
pub enum Action {
    Reconcile(Option<FileEntry>, String),
    Edit(FileEntry, String),
    Delete(FileEntry, String),
    Revert(FileEntry, bool),
    Reopen(FileEntry, String),
    NewChange(String),
    EditChange(String, String),
    DeleteChange(String),
    Submit(String, String),
    Shelve(String),
    Unshelve(String, String),
    DeleteShelf(String),
    Sync,
    SwitchStream(String),
    SwitchWorkspace(String),
    SaveClientSpec { name: String, form: String },
    Resolve(FileEntry, String),
    Move(FileEntry, String),
    Lock(FileEntry, bool),
    CreateStream(String, String),
    DeleteStream(String),
    MergeStream(String, String),
    CopyStream(String, String),
}
