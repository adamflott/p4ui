use anyhow::{Context, Result, ensure};
use clap::Parser;
use crossterm::{
    event::{
        self, DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
        Event, KeyEventKind, MouseEventKind,
    },
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use p4ui::{app::App, config::Config, model::Tab, p4::Connection, ui};
use ratatui::{Terminal, backend::CrosstermBackend};
use std::{
    io::{self, IsTerminal},
    num::NonZeroUsize,
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

#[derive(Parser, Debug)]
#[command(
    version,
    about = "A keyboard-driven terminal interface for Perforce",
    long_about = "A gitui-inspired Perforce terminal interface. Uses existing p4 environment, P4CONFIG and login tickets. Run inside a configured Perforce workspace."
)]
struct Args {
    /// Initial interactive tab; --tab log avoids the workspace status scan
    #[arg(long, value_enum, default_value = "status")]
    tab: Tab,
    /// Directory to use (status/sync/reconcile are scoped to its descendants)
    #[arg(short = 'd', long, default_value = ".")]
    directory: PathBuf,
    /// Perforce server; overrides P4PORT (otherwise use normal p4 configuration)
    #[arg(short = 'p', long, env = "P4PORT", hide_env_values = true)]
    port: Option<String>,
    /// Perforce user (otherwise P4USER/P4CONFIG)
    #[arg(short = 'u', long)]
    user: Option<String>,
    /// Perforce workspace (otherwise P4CLIENT/P4CONFIG)
    #[arg(short = 'c', long)]
    client: Option<String>,
    /// Perforce character set (otherwise P4CHARSET)
    #[arg(long)]
    charset: Option<String>,
    /// Path to the p4 executable
    #[arg(long, default_value = "p4")]
    p4: PathBuf,
    /// Maximum concurrent p4 child processes (1 runs requests serially)
    #[arg(long, default_value = "4")]
    max_p4_processes: NonZeroUsize,
    /// Alternate configuration file
    #[arg(long)]
    config: Option<PathBuf>,
    /// Read workspace status as JSON and exit without a terminal
    #[arg(long)]
    status: bool,
    /// Read one result-count-limited Log page as JSON without loading workspace status
    #[arg(long, conflicts_with_all = ["status", "check"])]
    log: bool,
    /// Inclusive upper changelist bound for --log (otherwise the latest scoped changes)
    #[arg(long, requires = "log", value_parser = clap::value_parser!(u64).range(1..))]
    before_change: Option<u64>,
    /// Check connection/workspace and print diagnostics without a terminal
    #[arg(long)]
    check: bool,
}
struct TerminalGuard;
impl TerminalGuard {
    fn enter() -> Result<Self> {
        enable_raw_mode()?;
        let guard = Self;
        execute!(
            io::stdout(),
            EnterAlternateScreen,
            EnableBracketedPaste,
            EnableMouseCapture
        )?;
        Ok(guard)
    }
    fn restore() {
        let _ = disable_raw_mode();
        let _ = execute!(
            io::stdout(),
            DisableBracketedPaste,
            DisableMouseCapture,
            LeaveAlternateScreen,
            crossterm::cursor::Show
        );
    }
}
impl Drop for TerminalGuard {
    fn drop(&mut self) {
        Self::restore();
    }
}
fn main() -> std::process::ExitCode {
    match run() {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("p4ui: {e:#}");
            std::process::ExitCode::FAILURE
        }
    }
}
fn run() -> Result<()> {
    let args = Args::parse();
    let config = Config::load(args.config)?;
    // P4 mappings are lexical: resolving /var to /private/var (or a workspace
    // symlink) can put an otherwise valid directory outside its client Root.
    let actual = std::env::current_dir()?;
    let base = std::env::var_os("PWD")
        .map(PathBuf::from)
        .filter(|p| {
            p.is_absolute() && std::fs::canonicalize(p).ok() == std::fs::canonicalize(&actual).ok()
        })
        .unwrap_or(actual);
    let directory = if args.directory.is_absolute() {
        args.directory
    } else {
        base.join(args.directory)
    };
    ensure!(
        directory.is_dir(),
        "Directory does not exist: {}",
        directory.display()
    );
    let mut connection = Connection::new(directory, &config);
    connection.set_max_processes(args.max_p4_processes);
    connection.executable = args.p4;
    // An empty P4PORT leaves resolution to the normal p4 configuration.
    connection.port = args.port.filter(|port| !port.is_empty());
    connection.user = args.user;
    connection.client = args.client;
    connection.charset = args.charset;
    if args.log {
        let cursor = args.before_change.map(p4ui::model::HistoryCursor::new);
        println!(
            "{}",
            serde_json::to_string_pretty(&connection.load_history(Tab::Log, cursor)?)?
        );
        return Ok(());
    }
    if args.status {
        println!(
            "{}",
            serde_json::to_string_pretty(&connection.load(Tab::Status, config.page_size)?)?
        );
        return Ok(());
    }
    if args.check {
        let info = connection.info()?;
        println!(
            "Connected as {}@{}\nServer: {}\nWorkspace root: {}\nStream: {}",
            info.user, info.client, info.server, info.root, info.stream
        );
        return Ok(());
    }
    ensure!(
        io::stdin().is_terminal() && io::stdout().is_terminal(),
        "Interactive mode requires a terminal. Use --status or --check for scripts."
    );
    let old_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        TerminalGuard::restore();
        old_hook(info);
    }));
    let shutdown = Arc::new(AtomicBool::new(false));
    #[cfg(unix)]
    for signal in [
        signal_hook::consts::SIGTERM,
        signal_hook::consts::SIGHUP,
        signal_hook::consts::SIGINT,
    ] {
        signal_hook::flag::register(signal, shutdown.clone())?;
    }
    let mut guard = TerminalGuard::enter()?;
    let mut terminal = Terminal::new(CrosstermBackend::new(io::stdout()))?;
    let mut app = App::new(config, connection);
    if args.tab != Tab::Status {
        app.switch_tab(args.tab);
    }
    while !app.quit {
        let size = terminal.size()?;
        app.set_history_rows(ui::list_rows(size.height));
        app.tick()?;
        if shutdown.load(Ordering::Relaxed) && !app.mutating {
            break;
        }
        if let Some(path) = app.external_edit.take() {
            drop(guard);
            let result = run_editor(&path);
            guard = TerminalGuard::enter()?;
            terminal.clear()?;
            if let Err(error) = result {
                app.editor_error(format!("{error:#}"));
            }
            app.refresh();
        }
        terminal.draw(|f| ui::draw(f, &mut app))?;
        if event::poll(Duration::from_millis(50))? {
            match event::read()? {
                Event::Key(key)
                    if key.kind == KeyEventKind::Press || key.kind == KeyEventKind::Repeat =>
                {
                    app.key(key)?
                }
                Event::Paste(text) => app.paste(&text),
                Event::Mouse(mouse)
                    if mouse.row >= 3 && mouse.row < size.height.saturating_sub(3) =>
                {
                    match mouse.kind {
                        MouseEventKind::ScrollDown => app.scroll(
                            3,
                            u32::from(mouse.column) < u32::from(size.width) * 42 / 100,
                        ),
                        MouseEventKind::ScrollUp => app.scroll(
                            -3,
                            u32::from(mouse.column) < u32::from(size.width) * 42 / 100,
                        ),
                        _ => {}
                    }
                }
                _ => {}
            }
        }
    }
    terminal.show_cursor()?;
    Ok(())
}

fn run_editor(path: &std::path::Path) -> Result<()> {
    let editor = ["P4EDITOR", "VISUAL", "EDITOR"]
        .iter()
        .find_map(|name| std::env::var(name).ok().filter(|s| !s.trim().is_empty()))
        .unwrap_or_else(|| {
            if cfg!(windows) {
                "notepad".into()
            } else {
                "vi".into()
            }
        });
    let parts = shell_words::split(&editor).context("Invalid quoting in editor command")?;
    let executable = parts.first().context("Editor command is empty")?;
    let status = std::process::Command::new(executable)
        .args(&parts[1..])
        .arg(path)
        .status()
        .with_context(|| format!("Starting editor {executable}"))?;
    ensure!(status.success(), "Editor exited with {status}");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn process_limit_defaults_to_four_and_rejects_zero() -> Result<()> {
        assert_eq!(Args::try_parse_from(["p4ui"])?.max_p4_processes.get(), 4);
        assert_eq!(
            Args::try_parse_from(["p4ui", "--max-p4-processes", "1"])?
                .max_p4_processes
                .get(),
            1
        );
        assert!(Args::try_parse_from(["p4ui", "--max-p4-processes", "0"]).is_err());
        assert!(Args::try_parse_from(["p4ui", "--max-p4-processes", "-1"]).is_err());
        Ok(())
    }
}
