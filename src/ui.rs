use crate::{
    app::{App, Popup},
    model::{Item, Tab},
};
use ratatui::{
    Frame,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span, Text},
    widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph, Tabs, Wrap},
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

pub fn draw(frame: &mut Frame, app: &App) {
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
        .detail
        .lines()
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
            .scroll(app.detail_scroll),
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
                " · limit reached: + loads more"
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
    let hint = if app.drill.is_some() {
        if app.tab == Tab::Changes {
            "reopen: Space   move CL: m   submit this CL: c   Esc: back"
        } else {
            "diff: Enter   annotate: b   history: h   Esc: back"
        }
    } else {
        match app.tab {
            Tab::Status => {
                "reconcile: a/Space   all: A   keep: u   revert: U   move CL: m   submit: c"
            }
            Tab::Log => "browse files: Enter   search: /   more: +   Esc: back",
            Tab::Changes => {
                "target: Space   new: n   describe: E   shelve: s   submit: c   delete: D"
            }
            Tab::Shelves => "unshelve: u/Space   delete shelf: D   inspect: Enter",
            Tab::Files => {
                "editor: o   open for edit: e   annotate: b   history: h   revision: v   delete: D   rename: M"
            }
            Tab::Streams => "switch: o/Space   new: n   delete: D   merge from: g   copy from: p",
            Tab::Resolve => "resolve: R   diff: Enter   annotate: b   history: h",
            Tab::Workspaces => "use workspace: o/Space   inspect: Enter",
            Tab::Labels => "inspect: Enter   search: /",
        }
    };
    // Show remapped keys in contextual hints too.
    let mut hints = hint.to_string();
    for (action, default) in [
        ("reconcile", "a/Space"),
        ("submit", "c"),
        ("revert", "U"),
        ("move_change", "m"),
        ("new_change", "n"),
        ("shelve", "s"),
        ("unshelve", "u/Space"),
        ("edit_change", "E"),
        ("delete", "D"),
        ("edit", "e"),
        ("annotate", "b"),
        ("filelog", "h"),
        ("revision", "v"),
        ("rename", "M"),
        ("resolve", "R"),
        ("open", "o/Space"),
    ] {
        if app.config.keys.contains_key(action) {
            hints = hints.replace(
                &format!(": {default}"),
                &format!(": {}", app.config.key(action)),
            );
        }
    }
    frame.render_widget(
        Paragraph::new(format!(
            " {hints}\n ↑↓/jk select  ←→ tabs  Tab pane  {} refresh  {} sync  {} help  {} quit",
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
fn draw_list(frame: &mut Frame, app: &App, area: Rect, p: Palette, opened: Option<bool>) {
    let visible = app.visible();
    let rows: Vec<(usize, &Item)> = visible
        .iter()
        .enumerate()
        .filter_map(|(i, item)| {
            if let Some(open) = opened
                && !matches!(item,Item::File(f) if f.opened==open)
            {
                return None;
            }
            Some((i, *item))
        })
        .collect();
    let title = match opened {
        Some(true) => format!(" Opened ({}) ", rows.len()),
        Some(false) => format!(" Offline / untracked ({}) ", rows.len()),
        None => format!(
            " {} ({}) ",
            app.drill.as_deref().unwrap_or(app.tab.name()),
            rows.len()
        ),
    };
    let selected = rows.iter().position(|(i, _)| *i == app.selected);
    let items: Vec<ListItem> = rows
        .iter()
        .map(|(_, item)| {
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
    let list = List::new(items)
        .block(p.block(title, !app.focus_detail && selected.is_some()))
        .highlight_style(Style::default().bg(p.select).add_modifier(Modifier::BOLD))
        .highlight_symbol("› ");
    let mut state = ListState::default().with_selected(selected);
    frame.render_stateful_widget(list, area, &mut state);
}
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
    let scroll = if matches!(popup, Popup::Input { .. }) {
        text.lines()
            .count()
            .saturating_sub(chunks[0].height as usize)
            .min(u16::MAX as usize) as u16
    } else {
        app.popup_scroll
    };
    frame.render_widget(
        Paragraph::new(text)
            .wrap(Wrap { trim: false })
            .scroll((scroll, 0)),
        chunks[0],
    );
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
        "p4ui — Perforce in your terminal\n\nNavigation\n  1–9 / ←→: tabs    ↑↓ / {}{}: select    Tab / Enter: diff focus\n  PageUp/PageDown: scroll    Home/End: first/last\n  {}: filter loaded rows    Esc: clear filter / return to list\n  {} / F5: refresh    +: load more    {}: quit    Ctrl-C: exit\n\nStatus and files\n  {} / Space: reconcile selected offline file into active changelist\n  A: reconcile all files in the current directory and descendants\n  {}: open selected depot file for edit; o opens your external editor\n  {}: move opened file to a different changelist\n  u: remove open action, KEEP workspace content (revert -k)\n  {}: REVERT file and discard edits (confirmation required)\n  {}: open file for delete    {}: move/rename opened file\n  {}: annotate/blame    {}: revision/integration log    {}: view revision\n  {}: lock opened file    l: unlock\n\nChangelists\n  Space / {}: set active target changelist\n  {}: new numbered changelist    {}: edit saved description\n  {}: submit active or selected pending changelist\n  {}: delete empty pending changelist\n\nShelves\n  {} (Changes): shelve a numbered changelist; existing shelves are protected\n  {} (Shelves): unshelve into a chosen changelist\n  {} (Shelves): permanently delete shelf (confirmation required)\n  Shelving preserves opened files. Revert separately when ready.\n\nSync, streams, workspaces and resolve\n  {}: sync current directory and descendants to depot HEAD\n  {} / Space (Streams): switch clean workspace and sync\n  {} / Space (Workspaces): choose local workspace for this session\n  {}: resolve selected file (-am merge, -as safe, -ay yours, -at theirs)\n  Streams: n creates a development child of the CURRENT stream (or mainline).\n  D deletes a stream spec. g merges / p copies from the selected stream.\n  Use an external merge tool for conflicts automatic resolve leaves behind.\n\nPerforce semantics\n  All file actions and status are scoped to the launch directory and descendants.\n  Submit/shelve act on the WHOLE selected changelist, even outside this scope.\n  Files are opened as complete files: Perforce has no index or line staging.\n  Authentication uses existing p4 login tickets, P4CONFIG and P4 environment.\n  Never ignore a command error: a submit or sync can partially complete.\n  Filters search loaded rows only; + raises the query limit up to 10,000.\n  Theme and character bindings: ~/.config/p4ui/config.toml\n\nSee README.md for feature mapping, limitations and server tests.",
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
        k("resolve")
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
                terminal.draw(|f| draw(f, &app)).unwrap();
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
                terminal.draw(|f| draw(f, &app)).unwrap();
            }
        }
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
    fn control_sequences_are_not_rendered() {
        assert_eq!(clean("a\x1b[31m\x00b\t\n"), "a[31mb \n");
    }
}
