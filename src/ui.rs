use crate::{
    app::{App, Popup},
    model::{Item, Tab},
};
use ratatui::{
    Frame,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span, Text},
    widgets::{Block, Borders, Clear, List, ListItem, Paragraph, Tabs, Wrap},
};

#[derive(Clone, Copy)]
struct Palette {
    fg: Color,
    muted: Color,
    accent: Color,
    added: Color,
    removed: Color,
    bg: Color,
    select: Color,
}
impl Palette {
    fn for_app(app: &App) -> Self {
        match app.config.theme.as_str() {
            "light" => Self {
                fg: Color::Black,
                muted: Color::DarkGray,
                accent: Color::Blue,
                added: Color::Green,
                removed: Color::Red,
                bg: Color::White,
                select: Color::Gray,
            },
            "mono" => Self {
                fg: Color::White,
                muted: Color::Gray,
                accent: Color::White,
                added: Color::White,
                removed: Color::White,
                bg: Color::Reset,
                select: Color::DarkGray,
            },
            _ => Self {
                fg: Color::Gray,
                muted: Color::DarkGray,
                accent: Color::Cyan,
                added: Color::Green,
                removed: Color::Red,
                bg: Color::Reset,
                select: Color::DarkGray,
            },
        }
    }
    fn block(self, title: String, active: bool) -> Block<'static> {
        Block::default()
            .borders(Borders::ALL)
            .title(Line::from(title))
            .border_style(Style::default().fg(if active { self.accent } else { self.muted }))
    }
}
/// Prevent control characters from depot files or server messages reaching the terminal.
pub fn clean(text: &str) -> String {
    text.chars()
        .filter_map(|c| match c {
            '\n' => Some('\n'),
            '\t' => Some(' '),
            c if c.is_control() => None,
            c => Some(c),
        })
        .collect()
}
fn one_line(s: &str) -> String {
    clean(s).split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Header, tabs, status, hints and list borders occupy eight terminal rows.
pub fn list_rows(height: u16) -> usize {
    height.saturating_sub(8).max(1) as usize
}

pub fn draw(frame: &mut Frame, app: &mut App) {
    let p = Palette::for_app(app);
    let size = frame.area();
    frame.render_widget(
        Block::default().style(Style::default().fg(p.fg).bg(p.bg)),
        size,
    );
    if size.width < 60 || size.height < 14 {
        frame.render_widget(
            Paragraph::new(
                "p4ui needs a terminal at least 60 × 14.\nResize the window, or press q to quit.",
            )
            .wrap(Wrap { trim: false }),
            size,
        );
        return;
    }
    let layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Length(2),
            Constraint::Min(6),
            Constraint::Length(1),
            Constraint::Length(2),
        ])
        .split(size);
    let info = &app.view.info;
    let top = Line::from(vec![
        Span::styled(
            " p4ui ",
            Style::default().fg(p.accent).add_modifier(Modifier::BOLD),
        ),
        Span::raw(format!(
            " {}@{} ",
            one_line(&info.user),
            one_line(&info.client)
        )),
        Span::styled(
            if info.stream.is_empty() {
                one_line(&info.server)
            } else {
                one_line(&info.stream)
            },
            Style::default().fg(p.muted),
        ),
        Span::styled(
            format!(
                "  target: {} {}",
                one_line(&app.target),
                if app.busy { "⟳" } else { "" }
            ),
            Style::default().fg(p.added),
        ),
    ]);
    frame.render_widget(Paragraph::new(top), layout[0]);
    let count = if size.width >= 125 {
        9
    } else {
        (size.width as usize / 17).max(1)
    };
    let start = (app.tab.index() / count) * count;
    let titles: Vec<Line> = Tab::ALL
        .iter()
        .enumerate()
        .skip(start)
        .take(count)
        .map(|(i, t)| Line::from(format!("{} {}", i + 1, t.name())))
        .collect();
    frame.render_widget(
        Tabs::new(titles)
            .select(app.tab.index() - start)
            .highlight_style(Style::default().fg(p.accent).add_modifier(Modifier::BOLD))
            .divider("│")
            .padding(" ", " "),
        layout[1],
    );
    let horizontal = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(42), Constraint::Percentage(58)])
        .split(layout[2]);
    if app.tab == Tab::Status {
        let vertical = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
            .split(horizontal[0]);
        draw_list(frame, app, vertical[0], p, Some(true));
        draw_list(frame, app, vertical[1], p, Some(false));
    } else {
        draw_list(frame, app, horizontal[0], p, None);
    }
    let detail_text: Vec<Line> = app
        .detail_window(
            app.detail_scroll.0 as usize,
            horizontal[1].height.saturating_sub(2) as usize,
        )
        .into_iter()
        .map(|l| {
            let color = if l.starts_with('+') {
                p.added
            } else if l.starts_with('-') {
                p.removed
            } else if l.starts_with("@@") {
                p.accent
            } else {
                p.fg
            };
            Line::styled(clean(l), Style::default().fg(color))
        })
        .collect();
    let content = if app.detail.is_empty() {
        Text::from(if app.busy {
            "Loading…"
        } else {
            "Select an item to inspect it.\nTab focuses this pane.  ? opens help."
        })
    } else {
        Text::from(detail_text)
    };
    frame.render_widget(
        Paragraph::new(content)
            .block(p.block(
                format!(" {} ", one_line(&app.detail_title)),
                app.focus_detail,
            ))
            .scroll((0, app.detail_scroll.1)),
        horizontal[1],
    );
    let status = if app.searching || !app.filter.is_empty() {
        format!(
            " /{}{}",
            app.filter,
            if app.searching {
                "▏"
            } else {
                "  [Esc clears]"
            }
        )
    } else {
        format!(
            " {}{}",
            one_line(&app.status),
            if app.view.limit_reached {
                if app.tab.is_history() && app.drill.is_none() {
                    ""
                } else {
                    " · limit reached: + loads more"
                }
            } else {
                ""
            }
        )
    };
    frame.render_widget(
        Paragraph::new(status).style(Style::default().fg(if app.searching {
            p.accent
        } else if !app.view.warnings.is_empty() {
            Color::Yellow
        } else {
            p.muted
        })),
        layout[3],
    );
    let hints = contextual_hints(app);
    frame.render_widget(
        Paragraph::new(format!(
            " {hints}\n ↑↓/{}{} select  ←→ tabs  Tab pane  {} refresh  {} sync  {} help  {} quit",
            app.config.key("up"),
            app.config.key("down"),
            app.config.key("refresh"),
            app.config.key("sync"),
            app.config.key("help"),
            app.config.key("quit")
        ))
        .style(Style::default().fg(p.accent)),
        layout[4],
    );
    if let Some(popup) = &app.popup {
        draw_popup(frame, app, popup, p);
    }
}
fn contextual_hints(app: &App) -> String {
    let k = |action| app.config.key(action);
    if app.drill.is_some() {
        return if app.tab == Tab::Changes {
            format!(
                "reopen: {}/Space   move CL: {}   submit this CL: {}   Esc: back",
                k("reconcile"),
                k("move_change"),
                k("submit")
            )
        } else {
            format!(
                "diff: Enter   annotate: {}   history: {}   Esc: back",
                k("annotate"),
                k("filelog")
            )
        };
    }
    match app.tab {
        Tab::Status => format!(
            "jump pane: {}   reconcile: {}/Space   all: A   keep: {}   revert: {}   move CL: {}   submit: {}",
            k("status_pane"),
            k("reconcile"),
            k("unshelve"),
            k("revert"),
            k("move_change"),
            k("submit")
        ),
        Tab::Log => format!(
            "browse files: Enter   search loaded: {}   scroll for older   newest: {}",
            k("search"),
            k("refresh")
        ),
        Tab::Changes => format!(
            "target: {}/Space   new: {}   describe: {}   shelve: {}   submit: {}   delete: {}",
            k("open"),
            k("new_change"),
            k("edit_change"),
            k("shelve"),
            k("submit"),
            k("delete")
        ),
        Tab::Shelves => format!(
            "unshelve: {}/Space   delete shelf: {}   files: Enter   older: +",
            k("unshelve"),
            k("delete")
        ),
        Tab::Files => format!(
            "editor: {}   open for edit: {}   annotate: {}   history: {}   revision: {}   delete: {}   rename: {}",
            k("open"),
            k("edit"),
            k("annotate"),
            k("filelog"),
            k("revision"),
            k("delete"),
            k("rename")
        ),
        Tab::Streams => format!(
            "switch: {}/Space   new: {}   delete: {}   merge from: g   copy from: p",
            k("open"),
            k("new_change"),
            k("delete")
        ),
        Tab::Resolve => format!(
            "resolve: {}   diff: Enter   annotate: {}   history: {}",
            k("resolve"),
            k("annotate"),
            k("filelog")
        ),
        Tab::Workspaces => format!(
            "use workspace: {}/Space   inspect: Enter   edit current client spec: {}",
            k("open"),
            k("edit_client")
        ),
        Tab::Labels => format!("inspect: Enter   search: {}", k("search")),
    }
}
fn draw_list(frame: &mut Frame, app: &mut App, area: Rect, p: Palette, opened: Option<bool>) {
    let rows = app.rows();
    let positions = opened.map(|open| &rows.panes[usize::from(!open)]);
    let count = positions.map_or(rows.indices.len(), |positions| positions.len());
    let selected = if let Some(positions) = positions {
        positions.binary_search(&app.selected).ok()
    } else {
        (app.selected < count).then_some(app.selected)
    };
    let state_index = match opened {
        Some(true) => 0,
        Some(false) => 1,
        None => 2,
    };
    let height = area.height.saturating_sub(2) as usize;
    let mut offset = app.list_states[state_index]
        .offset()
        .min(count.saturating_sub(height));
    if height > 0
        && let Some(selected) = selected
    {
        if selected < offset {
            offset = selected;
        } else if selected >= offset + height {
            offset = selected + 1 - height;
        }
    }
    let title = match opened {
        Some(true) => format!(" Opened ({count}) "),
        Some(false) => format!(" Offline / untracked ({count}) "),
        None => format!(
            " {} ({}) ",
            app.drill.as_deref().unwrap_or(app.tab.name()),
            count
        ),
    };
    let items: Vec<ListItem> = (offset..count.min(offset + height))
        .map(|position| {
            let visible_position = positions.map_or(position, |positions| positions[position]);
            let item = &app.view.items[rows.indices[visible_position]];
            #[cfg(test)]
            FORMATTED_ROWS.with(|count| count.set(count.get() + 1));
            let (text, color) = match item {
                Item::File(f) => {
                    let icon = match f.action.as_str() {
                        "add" | "move/add" | "branch" => "A",
                        "delete" | "move/delete" => "D",
                        _ => "M",
                    };
                    let color = if f.unresolved {
                        p.removed
                    } else if icon == "A" {
                        p.added
                    } else if icon == "D" {
                        p.removed
                    } else {
                        p.fg
                    };
                    let path = if !f.local.is_empty() && f.local.starts_with(&app.view.info.root) {
                        f.local
                            .strip_prefix(&app.view.info.root)
                            .unwrap_or(&f.local)
                            .trim_start_matches('/')
                    } else {
                        &f.depot
                    };
                    (
                        format!(
                            "{icon} {}{}{}",
                            one_line(path),
                            if f.unresolved { " [resolve]" } else { "" },
                            if f.opened {
                                format!(" ({})", f.change)
                            } else {
                                String::new()
                            }
                        ),
                        color,
                    )
                }
                Item::Revision {
                    depot,
                    revision,
                    action,
                    ..
                } => (format!("{action} {depot}#{revision}"), p.fg),
                Item::Change {
                    id,
                    description,
                    user,
                    ..
                } => (
                    format!(
                        "{}{} {}  {}",
                        if *id == app.target { "● " } else { "" },
                        id,
                        one_line(user),
                        one_line(description)
                    ),
                    p.fg,
                ),
                Item::Stream { path, kind, .. } => (
                    format!(
                        "{}{} [{kind}]",
                        if *path == app.view.info.stream {
                            "● "
                        } else {
                            ""
                        },
                        one_line(path)
                    ),
                    p.fg,
                ),
                Item::Workspace { name, root, .. } => (
                    format!(
                        "{}{} {}",
                        if *name == app.view.info.client {
                            "● "
                        } else {
                            ""
                        },
                        one_line(name),
                        one_line(root)
                    ),
                    p.fg,
                ),
                Item::Label { name, description } => (
                    format!("{} {}", one_line(name), one_line(description)),
                    p.fg,
                ),
            };
            ListItem::new(text).style(Style::default().fg(color))
        })
        .collect();
    drop(rows);
    let list = List::new(items)
        .block(p.block(title, !app.focus_detail && selected.is_some()))
        .highlight_style(Style::default().bg(p.select).add_modifier(Modifier::BOLD))
        .highlight_symbol("› ");
    let state = &mut app.list_states[state_index];
    // Updating selection directly preserves an inactive Status pane's offset;
    // ListState::select(None) would reset it when focus moves to the other pane.
    *state.selected_mut() = selected;
    *state.offset_mut() = offset;
    let mut viewport = ratatui::widgets::ListState::default();
    *viewport.selected_mut() = selected
        .and_then(|index| index.checked_sub(offset))
        .filter(|index| *index < height);
    frame.render_stateful_widget(list, area, &mut viewport);
}
#[cfg(test)]
thread_local! { static FORMATTED_ROWS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) }; }
fn draw_popup(frame: &mut Frame, app: &App, popup: &Popup, p: Palette) {
    let size = frame.area();
    let width = size.width.saturating_sub(6).min(100);
    let height = size.height.saturating_sub(4).min(30);
    let area = Rect::new(
        (size.width - width) / 2,
        (size.height - height) / 2,
        width,
        height,
    );
    frame.render_widget(Clear, area);
    let (title, text, footer) = match popup {
        Popup::Help => (
            "Help — scroll with ↑↓ / PgUp PgDn".to_string(),
            help(app),
            "Esc / Enter closes",
        ),
        Popup::Message { title, text } => (
            title.clone(),
            clean(text),
            "Esc / Enter closes · ↑↓ scrolls",
        ),
        Popup::Confirm { text, .. } => (
            "Confirm operation".into(),
            format!(
                "{}\n\nPress y to proceed. Press n or Esc to cancel.",
                clean(text)
            ),
            "y confirm · n / Esc cancel",
        ),
        Popup::Input { kind, value } => (
            kind.title().into(),
            format!("{}▏", clean(value)),
            if kind.multiline() {
                "Enter newline · Ctrl-S / Ctrl-Enter accept · Ctrl-U clear · Esc cancel"
            } else {
                "Enter accept · Ctrl-U clear · Esc cancel"
            },
        ),
    };
    let block = p
        .block(format!(" {} ", one_line(&title)), true)
        .style(Style::default().bg(p.bg).fg(p.fg));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let chunks = Layout::vertical([Constraint::Min(1), Constraint::Length(2)]).split(inner);
    let paragraph = Paragraph::new(text).wrap(Wrap { trim: false });
    let scroll = if matches!(popup, Popup::Input { .. }) {
        paragraph
            .line_count(chunks[0].width)
            .saturating_sub(chunks[0].height as usize)
            .min(u16::MAX as usize) as u16
    } else {
        app.popup_scroll
    };
    frame.render_widget(paragraph.scroll((scroll, 0)), chunks[0]);
    frame.render_widget(
        Paragraph::new(footer)
            .style(Style::default().fg(p.accent))
            .wrap(Wrap { trim: false }),
        chunks[1],
    );
}
fn help(app: &App) -> String {
    let k = |action| app.config.key(action);
    let instructions = format!(
        "p4ui — Perforce in your terminal\n\nNavigation\n  1–9 / ←→: tabs    ↑↓ / {}{}: select    Tab / Enter: diff focus\n  PageUp/PageDown: scroll    Home/End: first/last\n  {}: filter loaded rows    Esc: clear filter / return to list\n  {} / F5: refresh    +: load more    {}: quit    Ctrl-C: exit\n\nStatus panes\n  {status_pane}: jump between Opened and Offline / untracked (first matching file).\n  The shortcut works from diff focus and respects the active filter.\n  Opened and Offline / untracked share one selection list.\n  Down / {down} past the last Opened row enters Offline / untracked.\n  Up / {up} from its first row returns to Opened.\n  Tab toggles file-list / diff focus. End selects the last file.\n  Status remembers selection, filter, and scroll across tab switches.\n\nFile action badges\n  A: add, branch, or destination of a move\n  M: edit/modify; also the fallback badge for other actions\n  D: delete or source of a move\n  Files uses the last submitted action for unopened files.\n  A/M there may already be submitted. Use Status for local changes.\n\nStatus and files\n  {} / Space: reconcile selected offline file into active changelist\n  A: reconcile all files in the current directory and descendants\n  {}: open selected depot file for edit; {open} opens your external editor\n  {}: move opened file to a different changelist\n  {unshelve}: remove open action, KEEP workspace content (revert -k)\n  {}: REVERT file and discard edits (confirmation required)\n  {}: open file for delete    {}: move/rename opened file\n  {}: annotate/blame    {}: revision/integration log    {}: view revision\n  {}: lock opened file    l: unlock\n\nChangelists\n  Space / {}: set active target changelist\n  {}: new numbered changelist    {}: edit saved description\n  {}: submit active or selected pending changelist\n  {}: delete empty pending changelist\n\nShelves\n  {} (Changes): shelve a numbered changelist; existing shelves are protected\n  {} (Shelves): unshelve into a chosen changelist\n  {} (Shelves): permanently delete shelf (confirmation required)\n  Shelving preserves opened files. Revert separately when ready.\n\nSync, streams, workspaces and resolve\n  {}: sync current directory and descendants to depot HEAD\n  {} / Space (Streams): switch clean workspace and sync\n  {} / Space (Workspaces): choose local workspace for this session\n  {}: resolve selected file (-am merge, -as safe, -ay yours, -at theirs)\n  Streams: {new_change} creates a development child of the CURRENT stream (or mainline).\n  {delete} deletes a stream spec. g merges / p copies from the selected stream.\n  Use an external merge tool for conflicts automatic resolve leaves behind.\n\nClient specification\n  {edit_client} (any tab): edit the CURRENT workspace client spec in your editor.\n  Save and exit the editor; y saves to Perforce, n / Esc discards.\n  Keep Client: unchanged. Unchanged forms are not saved.\n  Files are not synced; p4ui keeps its current working directory.\n  Editor/save failures show a recovery draft path.\n\nPerforce semantics\n  Status and bulk reconcile/sync/merge/copy use the current directory and descendants.\n  Pending changelist files span the whole workspace. Selected-file actions apply to that file, even outside this directory.\n  Submit/shelve act on the WHOLE selected changelist, even outside this scope.\n  Files are opened as complete files: Perforce has no index or line staging.\n  Authentication uses existing p4 login tickets, P4CONFIG and P4 environment.\n  Never ignore a command error: a submit or sync can partially complete.\n  History fills the visible list; scrolling near the bottom fetches older pages.\n  Log and Shelves do not automatically poll the server.\n  Recent listings/previews can reuse read results for up to 10 seconds.\n  Refresh bypasses caches; mutations and workspace changes invalidate them.\n  The mouse wheel and PageDown scroll; + also fetches an older page.\n  Change summaries show up to 200 files; Enter opens per-file diffs.\n  Filters search loaded rows only. Other tabs: + raises the result limit.\n  Theme and character bindings: ~/.config/p4ui/config.toml\n\nSee README.md for feature mapping, limitations and server tests.",
        k("up"),
        k("down"),
        k("search"),
        k("refresh"),
        k("quit"),
        k("reconcile"),
        k("edit"),
        k("move_change"),
        k("revert"),
        k("delete"),
        k("rename"),
        k("annotate"),
        k("filelog"),
        k("revision"),
        k("lock"),
        k("open"),
        k("new_change"),
        k("edit_change"),
        k("submit"),
        k("delete"),
        k("shelve"),
        k("unshelve"),
        k("delete"),
        k("sync"),
        k("open"),
        k("open"),
        k("resolve"),
        open = k("open"),
        unshelve = k("unshelve"),
        new_change = k("new_change"),
        delete = k("delete"),
        up = k("up"),
        down = k("down"),
        edit_client = k("edit_client"),
        status_pane = k("status_pane"),
    );
    if app.view.warnings.is_empty() {
        instructions
    } else {
        format!(
            "Workspace warnings — results may be incomplete\n{}\n\n{instructions}",
            clean(&app.view.warnings.join("\n"))
        )
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use crate::{config::Config, model::*};
    use ratatui::{Terminal, backend::TestBackend};
    #[test]
    fn large_status_formats_only_the_visible_rows_and_updates_filtered_counts() {
        let items = (0..100_000)
            .map(|index| {
                Item::File(FileEntry {
                    depot: format!("//depot/file{index:06}"),
                    opened: index % 2 == 0,
                    action: "edit".into(),
                    ..Default::default()
                })
            })
            .collect();
        let mut app = App::for_view(
            Config::default(),
            View {
                items,
                ..View::default()
            },
        );
        app.selected = 99_999;
        app.detail = (0..100_000)
            .map(|index| format!("line {index}\n"))
            .collect();
        app.detail_scroll.0 = 60_000;
        let mut terminal = Terminal::new(TestBackend::new(140, 40)).unwrap();
        FORMATTED_ROWS.with(|count| count.set(0));
        terminal.draw(|frame| draw(frame, &mut app)).unwrap();
        assert!(FORMATTED_ROWS.with(|count| count.get()) <= 32);
        let text = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(text.contains("Opened (50000)"));
        assert!(text.contains("Offline / untracked (50000)"));
        assert!(text.contains("file099999"));
        assert!(text.contains("line 60000"));
        app.filter = "file000123".into();
        app.selected = 0;
        FORMATTED_ROWS.with(|count| count.set(0));
        terminal.draw(|frame| draw(frame, &mut app)).unwrap();
        assert_eq!(FORMATTED_ROWS.with(|count| count.get()), 1);
    }
    #[test]
    fn all_tabs_and_popups_render_at_small_and_large_sizes() {
        for (w, h) in [(40, 10), (60, 14), (100, 30), (160, 50)] {
            let mut terminal = Terminal::new(TestBackend::new(w, h)).unwrap();
            let mut app = App::for_view(
                Config::default(),
                View {
                    info: Info {
                        user: "tester".into(),
                        client: "workspace".into(),
                        ..Default::default()
                    },
                    items: vec![Item::File(FileEntry {
                        depot: "//depot/日本語.txt".into(),
                        action: "edit".into(),
                        opened: true,
                        ..Default::default()
                    })],
                    ..Default::default()
                },
            );
            app.detail = "@@ -1 +1 @@\n-old\n+new\n日本語 💥".into();
            for tab in Tab::ALL {
                app.tab = tab;
                terminal.draw(|f| draw(f, &mut app)).unwrap();
            }
            for popup in [
                Popup::Help,
                Popup::Message {
                    title: "Error".into(),
                    text: "server error\n".repeat(100),
                },
                Popup::Confirm {
                    action: Action::Sync,
                    text: "Sync?".into(),
                },
                Popup::Input {
                    kind: crate::app::InputKind::NewChange,
                    value: "日本語💥".into(),
                },
            ] {
                app.popup = Some(popup);
                terminal.draw(|f| draw(f, &mut app)).unwrap();
            }
        }
    }
    #[test]
    fn scrolling_up_preserves_the_list_viewport() {
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        let mut app = App::for_view(
            Config::default(),
            View {
                items: (0..100)
                    .map(|n| Item::Label {
                        name: format!("row{n:03}"),
                        description: String::new(),
                    })
                    .collect(),
                ..Default::default()
            },
        );
        app.tab = Tab::Labels;
        app.selected = 50;
        let mut terminal = Terminal::new(TestBackend::new(100, 30)).unwrap();
        terminal.draw(|f| draw(f, &mut app)).unwrap();
        let offset = app.list_states[2].offset();
        assert!(offset > 0);
        app.key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE))
            .unwrap();
        terminal.draw(|f| draw(f, &mut app)).unwrap();
        assert_eq!(app.selected, 49);
        assert_eq!(app.list_states[2].offset(), offset);
        assert_eq!(app.list_states[2].selected(), Some(49));
        app.key(KeyEvent::new(KeyCode::Home, KeyModifiers::NONE))
            .unwrap();
        terminal.draw(|f| draw(f, &mut app)).unwrap();
        assert_eq!(app.list_states[2].offset(), 0);
        app.switch_tab(Tab::Status);
        app.view.items = (0..100)
            .map(|n| {
                Item::File(FileEntry {
                    depot: format!("//depot/file{n:03}"),
                    opened: n < 50,
                    ..Default::default()
                })
            })
            .collect();
        app.selected = 49;
        terminal.draw(|f| draw(f, &mut app)).unwrap();
        let opened_offset = app.list_states[0].offset();
        assert!(opened_offset > 0);
        app.selected = 50;
        terminal.draw(|f| draw(f, &mut app)).unwrap();
        assert_eq!(app.list_states[0].offset(), opened_offset);
        assert_eq!(app.list_states[1].selected(), Some(0));
    }
    #[test]
    fn remapped_keys_are_rendered_without_replacement_collisions() {
        let mut app = App::for_view(Config::default(), View::default());
        for (action, key) in [
            ("submit", "e"),
            ("edit", "c"),
            ("open", "u"),
            ("unshelve", "o"),
            ("new_change", "D"),
            ("delete", "n"),
            ("search", "r"),
            ("refresh", "/"),
            ("up", "X"),
            ("down", "Y"),
            ("edit_client", "V"),
            ("status_pane", "w"),
        ] {
            app.config.keys.insert(action.into(), key.into());
        }
        let mut terminal = Terminal::new(TestBackend::new(160, 30)).unwrap();
        for (tab, expected) in [
            (
                Tab::Status,
                vec!["submit: e", "keep: o", "↑↓/XY select", "jump pane: w"],
            ),
            (
                Tab::Files,
                vec!["editor: u", "open for edit: c", "delete: n"],
            ),
            (Tab::Streams, vec!["switch: u/Space", "new: D", "delete: n"]),
            (Tab::Log, vec!["search loaded: r", "newest: /"]),
            (Tab::Workspaces, vec!["edit current client spec: V"]),
        ] {
            app.tab = tab;
            terminal.draw(|f| draw(f, &mut app)).unwrap();
            let rendered: String = terminal
                .backend()
                .buffer()
                .content()
                .iter()
                .map(|cell| cell.symbol())
                .collect();
            for text in expected {
                assert!(rendered.contains(text), "Missing {text:?} on {tab:?}");
            }
        }
        let text = help(&app);
        assert!(text.contains("u opens your external editor"));
        assert!(text.contains("o: remove open action"));
        assert!(text.contains("Streams: D creates"));
        assert!(text.contains("n deletes a stream spec"));
        assert!(text.contains("V (any tab): edit the CURRENT workspace client spec"));
        assert!(text.contains("w: jump between Opened and Offline / untracked"));
    }
    #[test]
    fn workspace_warnings_are_available_in_help() {
        let app = App::for_view(
            Config::default(),
            View {
                warnings: vec!["Skipped an unsupported filename".into()],
                ..Default::default()
            },
        );
        let text = help(&app);
        assert!(text.starts_with("Workspace warnings"));
        assert!(text.contains("Skipped an unsupported filename"));
        assert!(text.contains("results may be incomplete"));
    }
    #[test]
    fn wrapped_input_keeps_the_cursor_visible_after_resize() {
        for value in [
            "X".repeat(400),
            "wrapped words ".repeat(200),
            "日本語 💥 ".repeat(200),
            "line\n".repeat(100),
        ] {
            let mut app = App::for_view(Config::default(), View::default());
            app.popup = Some(Popup::Input {
                kind: crate::app::InputKind::NewChange,
                value,
            });
            for (width, height) in [(60, 14), (100, 30), (60, 14)] {
                let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
                terminal.draw(|f| draw(f, &mut app)).unwrap();
                assert!(
                    terminal
                        .backend()
                        .buffer()
                        .content()
                        .iter()
                        .any(|cell| cell.symbol() == "▏"),
                    "Input cursor hidden at {width}x{height}"
                );
            }
        }
    }
    #[test]
    fn control_sequences_are_not_rendered() {
        assert_eq!(clean("a\x1b[31m\x00b\t\n"), "a[31mb \n");
    }
}
