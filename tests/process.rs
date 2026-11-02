//! Verify failure boundaries with a deliberately broken/hanging CLI.
#![cfg(unix)]
use anyhow::{Context, Result};
use p4ui::{config::Config, p4::Connection};
use std::{
    os::unix::fs::PermissionsExt,
    time::{Duration, Instant},
};
fn fake(source: &str) -> Result<(tempfile::TempDir, Connection)> {
    let dir = tempfile::tempdir()?;
    let executable = dir.path().join("fake-p4");
    std::fs::write(&executable, format!("#!/usr/bin/env python3\n{source}\n"))?;
    std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o755))?;
    let mut c = Connection::new(dir.path().into(), &Config::default());
    c.executable = executable;
    Ok((dir, c))
}
#[test]
fn timeout_and_cancellation_reap_the_client() -> Result<()> {
    let (_dir, mut c) = fake("import time\ntime.sleep(10)")?;
    c.timeout = Duration::from_millis(100);
    let start = Instant::now();
    let e = c
        .records(&["info"])
        .err()
        .ok_or_else(|| anyhow::anyhow!("must time out"))?;
    assert!(e.to_string().contains("timed out"));
    assert!(start.elapsed() < Duration::from_secs(3));
    c.cancelled
        .store(true, std::sync::atomic::Ordering::Relaxed);
    assert!(c.records(&["info"]).is_err());
    Ok(())
}
#[test]
fn nonzero_exit_is_never_a_success_even_with_valid_json() -> Result<()> {
    let (_dir, c) = fake(
        "import pathlib,sys\nwith pathlib.Path(__file__).with_suffix('.calls').open('a') as f: f.write('started\\n')\nprint('{\"data\":\"partial success\"}')\nsys.exit(2)",
    )?;
    assert!(c.records(&["info"]).is_err());
    assert_eq!(
        std::fs::read_to_string(c.executable.with_extension("calls"))?,
        "started\n"
    );
    Ok(())
}
#[cfg(target_os = "linux")]
#[test]
fn busy_executable_retries_before_start_and_preserves_form_input() -> Result<()> {
    let (_dir, c) = fake(
        "import pathlib,sys\nwith pathlib.Path(__file__).with_suffix('.calls').open('a') as f: f.write('started\\n')\nprint(sys.stdin.read(),end='')",
    )?;
    // Holding a writer makes exec fail with ETXTBSY before the script can run.
    let writer = std::fs::OpenOptions::new()
        .write(true)
        .open(&c.executable)?;
    let error = std::process::Command::new(&c.executable)
        .spawn()
        .err()
        .context("Expected an executable held open for writing to be busy")?;
    assert_eq!(error.kind(), std::io::ErrorKind::ExecutableFileBusy);
    std::thread::scope(|scope| -> Result<()> {
        scope.spawn(move || {
            std::thread::sleep(Duration::from_millis(60));
            drop(writer);
        });
        assert_eq!(
            c.execute(&["client", "-i"], Some("form input\n"), false)?,
            "form input\n"
        );
        Ok(())
    })?;
    assert_eq!(
        std::fs::read_to_string(c.executable.with_extension("calls"))?,
        "started\n"
    );
    Ok(())
}
#[cfg(target_os = "linux")]
#[test]
fn busy_executable_retry_is_bounded_and_cancellable() -> Result<()> {
    for cancel in [false, true] {
        let (_dir, mut c) = fake("raise AssertionError('Must not start')")?;
        c.timeout = Duration::from_millis(100);
        let _writer = std::fs::OpenOptions::new()
            .write(true)
            .open(&c.executable)?;
        let start = Instant::now();
        let error = std::thread::scope(|scope| -> Result<anyhow::Error> {
            if cancel {
                let cancelled = &c.cancelled;
                scope.spawn(move || {
                    std::thread::sleep(Duration::from_millis(30));
                    cancelled.store(true, std::sync::atomic::Ordering::Relaxed);
                });
            }
            c.records(&["info"]).err().context("Must not start")
        })?;
        assert!(start.elapsed() < Duration::from_secs(2));
        if cancel {
            assert!(error.to_string().contains("cancelled before start"));
        } else {
            assert_eq!(
                error
                    .downcast_ref::<std::io::Error>()
                    .map(std::io::Error::kind),
                Some(std::io::ErrorKind::ExecutableFileBusy)
            );
        }
    }
    Ok(())
}
#[test]
fn excessive_output_is_reported_without_loading_it_all() -> Result<()> {
    let (_dir, mut c) = fake("print('x' * 1000000)")?;
    c.max_output = 1024;
    let error = c
        .records(&["info"])
        .err()
        .ok_or_else(|| anyhow::anyhow!("must fail"))?;
    assert!(error.to_string().contains("output limit"));
    Ok(())
}
#[test]
fn failed_workspace_switch_preserves_original_connection() -> Result<()> {
    use p4ui::model::Action;
    let source = r#"import json,os,pathlib,sys
root=pathlib.Path(__file__).parent
args=sys.argv[1:]
if 'set' in args:
 if os.environ['FAILURE']=='settings':
  print('Settings unavailable',file=sys.stderr);sys.exit(1)
 print('P4PORT=ssl:broker.example:1666\nP4CHARSET=none')
elif 'client' in args:
 assert args[args.index('-p')+1]=='ssl:broker.example:1666'
 assert args[args.index('-u')+1]=='tester'
 print(json.dumps({'Root':str(root/'other')}))
elif 'info' in args:
 if pathlib.Path(args[args.index('-d')+1])==root/'other':
  print(json.dumps({'severity':3,'data':'Workspace unavailable'}));sys.exit(1)
 print(json.dumps({'userName':'tester','clientName':'test','clientRoot':str(root),'serverAddress':'internal.example:1666'}))
else: raise AssertionError('Unexpected command: '+repr(args))
"#;
    for failure in ["settings", "validation"] {
        let (dir, mut connection) = fake(source)?;
        std::fs::create_dir(dir.path().join("other"))?;
        connection
            .environment
            .push(("FAILURE".into(), failure.into()));
        let original_directory = connection.directory.clone();
        let error = connection
            .act(Action::SwitchWorkspace("other".into()))
            .err()
            .context("Expected workspace switch failure")?;
        assert!(format!("{error:#}").contains(if failure == "settings" {
            "Settings unavailable"
        } else {
            "Workspace unavailable"
        }));
        assert_eq!(connection.directory, original_directory);
        assert!(connection.port.is_none());
        assert!(connection.user.is_none());
        assert!(connection.client.is_none());
        assert!(connection.charset.is_none());
        assert_eq!(connection.info()?.client, "test");
    }
    Ok(())
}
#[test]
fn workspace_listing_keeps_files_and_warnings_while_mutations_fail() -> Result<()> {
    use p4ui::model::{Action, Tab};
    let source = r#"import json,sys
if 'info' in sys.argv:
    print(json.dumps({'userName':'tester','clientName':'test','clientRoot':'/workspace'}))
elif 'fstat' in sys.argv:
    print(json.dumps({'depotFile':'//depot/opened.txt','clientFile':'/workspace/opened.txt','action':'edit','change':'default'}))
else:
    print(json.dumps({'severity':2,'generic':4,'data':"/workspace/ordinary.xml - can't reconcile filename with illegal characters."}))
    print(json.dumps({'depotFile':'//depot/new.txt','clientFile':'/workspace/new.txt','action':'add'}))"#;
    let (_dir, mut c) = fake(source)?;
    let view = c.load(Tab::Status, 200)?;
    assert_eq!(view.items.len(), 2);
    assert_eq!(view.warnings.len(), 1);
    assert!(view.warnings[0].contains("illegal characters"));
    assert!(c.records(&["reconcile", "-n", "-f", "..."]).is_err());
    assert!(c.act(Action::Reconcile(None, "default".into())).is_err());
    Ok(())
}
fn history_fake(mode: &str) -> Result<(tempfile::TempDir, Connection)> {
    let source = r#"import json,sys,os,pathlib,time
args=sys.argv[1:]
with pathlib.Path(__file__).with_suffix('.calls').open('a') as f: f.write(json.dumps(args)+'\n')
mode=os.environ['MODE']
if 'info' in args:
 print(json.dumps({'userName':'tester','clientName':'test','clientRoot':'/workspace'}))
elif 'groups' in args:
 if mode=='groups_denied':
  print(json.dumps({'severity':3,'generic':36,'data':'Permission denied'}));sys.exit(1)
 print(json.dumps({'group':'bounded','maxScanRows':'80','maxResults':'3'}))
elif 'changes' in args:
 scope=args[-1]
 assert scope=='...' or (scope.startswith('...@') and scope[4:].isdigit()), 'Unexpected history scope'
 if mode=='groups_denied': assert not any(arg.startswith('-zmaxScanRows=') for arg in args)
 for arg in args:
  if arg.startswith('-zmaxScanRows='): assert int(arg.split('=')[1])<=80
 if mode=='limit' or (mode=='fail_older' and scope!='...'):
  print(json.dumps({'change':'9999','desc':'partial result'}))
  print(json.dumps({'severity':3,'generic':35,'data':'Too many rows scanned; see p4 help maxscanrows.'}));sys.exit(1)
 if mode=='permission':
  print(json.dumps({'severity':3,'generic':36,'data':'Permission denied'}));sys.exit(1)
 changes=list(range(100000000,100000000-240*1009,-1009)) if mode in ('many','fail_older','slow_status','slow_status_once') else [9999,9995,9991]
 before=int(scope[4:]) if scope!='...' else 2**63
 maximum=int(args[args.index('-m')+1]);assert 1<=maximum<=200
 for change in [change for change in changes if change<=before][:maximum]:
  print(json.dumps({'change':str(change),'desc':'example','user':'tester','client':'test','status':'submitted'}))
elif 'fstat' in args and mode in ('slow_status','slow_status_once'):
 assert '-Ro' in args
 marker=pathlib.Path(__file__).with_suffix('.started')
 if mode=='slow_status_once' and marker.exists():
  print(json.dumps({'depotFile':'//depot/opened','clientFile':'/workspace/opened','action':'edit','change':'default'}))
 else:
  marker.touch();time.sleep(10)
elif 'reconcile' in args and mode in ('slow_status','slow_status_once'):
 assert '-n' in args
elif 'sync' in args and mode=='slow_mutation':
 pathlib.Path(__file__).with_suffix('.started').touch()
 time.sleep(0.3)
 print(json.dumps({'data':'Mutation completed'}))
elif 'describe' in args:
 print('Change summary')
else:
 raise AssertionError('Unexpected command: '+repr(args))
"#;
    let (dir, mut connection) = fake(source)?;
    connection.environment.push(("MODE".into(), mode.into()));
    Ok((dir, connection))
}
#[test]
fn history_pages_actual_changes_without_head_lookups_or_counter_windows() -> Result<()> {
    use p4ui::model::Tab;
    let (_dir, mut c) = history_fake("many")?;
    c.history_page_size = 32;
    let first = c.load_history(Tab::Log, None)?;
    assert_eq!(first.items.len(), 32);
    let page = first
        .history
        .ok_or_else(|| anyhow::anyhow!("Missing page"))?;
    assert_eq!(page.requested_scan_limit, None);
    let cursor = page.next.ok_or_else(|| anyhow::anyhow!("Missing cursor"))?;
    assert_eq!(
        cursor.before,
        first
            .items
            .last()
            .context("Missing last change")?
            .key()
            .parse::<u64>()?
            - 1
    );
    let older = c.load_history(Tab::Log, Some(cursor))?;
    assert_eq!(older.items.len(), 32);
    for item in &older.items {
        assert!(item.key().parse::<u64>()? <= cursor.before);
    }
    let calls = std::fs::read_to_string(c.executable.with_extension("calls"))?;
    assert!(!calls.contains("fstat") && !calls.contains("counter") && !calls.contains("groups"));
    assert!(calls.contains(&format!("...@{}", cursor.before)));
    Ok(())
}
#[test]
fn optional_history_scan_budget_never_raises_reported_admin_limits() -> Result<()> {
    use p4ui::model::Tab;
    let (_dir, mut c) = history_fake("success")?;
    c.history_max_scan_rows = 50000;
    let view = c.load_history(Tab::Log, None)?;
    let page = view
        .history
        .ok_or_else(|| anyhow::anyhow!("Missing page"))?;
    assert_eq!(page.requested_scan_limit, Some(80));
    assert_eq!(page.group_scan_limit, Some(80));
    assert_eq!(view.items.len(), 3);
    assert_eq!(page.next.map(|cursor| cursor.before), Some(9990));
    let calls = std::fs::read_to_string(c.executable.with_extension("calls"))?;
    assert!(calls.contains("-zmaxScanRows=80"));
    Ok(())
}
#[test]
fn history_errors_do_not_return_partial_pages_or_automatically_retry() -> Result<()> {
    use p4ui::model::Tab;
    for mode in ["limit", "permission"] {
        let (_dir, c) = history_fake(mode)?;
        let error = c
            .load_history(Tab::Log, None)
            .err()
            .context("Expected history error")?;
        let message = format!("{error:#}");
        assert!(
            message.contains(if mode == "limit" {
                "maxscanrows"
            } else {
                "Permission denied"
            }),
            "{message}"
        );
        let calls = std::fs::read_to_string(c.executable.with_extension("calls"))?;
        assert_eq!(
            calls
                .lines()
                .filter(|line| line.contains("\"changes\""))
                .count(),
            1
        );
    }
    let (_dir, mut c) = history_fake("groups_denied")?;
    c.history_max_scan_rows = 50000;
    let view = c.load_history(Tab::Log, None)?;
    assert!(!view.warnings.is_empty());
    assert_eq!(
        view.history.and_then(|page| page.requested_scan_limit),
        None
    );
    Ok(())
}

fn drive_app(app: &mut p4ui::app::App, ready: impl Fn(&p4ui::app::App) -> bool) -> Result<()> {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        app.tick()?;
        if ready(app) {
            return Ok(());
        }
        anyhow::ensure!(
            Instant::now() < deadline,
            "App did not settle: {}",
            app.status
        );
        std::thread::sleep(Duration::from_millis(15));
    }
}
fn cached_reads_fake() -> Result<(tempfile::TempDir, Connection)> {
    fake(
        r#"import json,pathlib,sys,time
args=sys.argv[1:];root=pathlib.Path(__file__).parent
with root.joinpath('calls').open('a') as f: f.write(json.dumps(args)+'\n')
if 'info' in args:
 print(json.dumps({'userName':'tester','clientName':'other' if root.joinpath('other').exists() else 'test','clientRoot':'/workspace'}))
elif 'changes' in args:
 print(json.dumps({'change':'100','desc':'Example','user':'tester','client':'test','status':'submitted'}))
elif 'describe' in args: print('Summary')
elif 'diff2' in args: print('Diff for '+args[-1])
elif 'fstat' in args: root.joinpath('started').touch();time.sleep(10)
elif 'reconcile' in args: assert '-n' in args
elif 'labels' in args: pass
elif 'sync' in args: print(json.dumps({'data':'Sync completed'}))
else: raise AssertionError('Unexpected command: '+repr(args))
"#,
    )
}
fn cached_calls(dir: &std::path::Path, command: &str) -> usize {
    std::fs::read_to_string(dir.join("calls"))
        .unwrap_or_default()
        .lines()
        .filter(|line| line.contains(&format!("\"{command}\"")))
        .count()
}
#[test]
fn recent_tab_reads_and_details_are_reused_but_manual_refresh_is_fresh() -> Result<()> {
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use p4ui::{app::App, model::Tab};
    let (dir, connection) = cached_reads_fake()?;
    let mut app = App::new(
        Config {
            refresh_seconds: 0,
            ..Config::default()
        },
        connection,
    );
    app.switch_tab(Tab::Log);
    drive_app(&mut app, |app| !app.busy && app.detail == "Summary\n")?;
    app.switch_tab(Tab::Labels);
    drive_app(&mut app, |app| {
        !app.busy && app.status.starts_with("0 items")
    })?;
    app.switch_tab(Tab::Log);
    drive_app(&mut app, |app| !app.busy && app.detail == "Summary\n")?;
    for command in ["info", "changes", "describe", "labels"] {
        assert_eq!(
            cached_calls(dir.path(), command),
            1,
            "Unexpected {command} reload"
        );
    }
    app.refresh();
    drive_app(&mut app, |app| !app.busy && app.detail == "Summary\n")?;
    for command in ["info", "changes", "describe"] {
        assert_eq!(
            cached_calls(dir.path(), command),
            2,
            "Manual refresh reused {command}"
        );
    }
    app.key(KeyEvent::new(KeyCode::Char('f'), KeyModifiers::NONE))?;
    app.key(KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE))?;
    drive_app(&mut app, |app| {
        !app.busy && !app.mutating && app.popup.is_some()
    })?;
    assert_eq!(cached_calls(dir.path(), "sync"), 1);
    app.key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE))?;
    app.switch_tab(Tab::Labels);
    drive_app(&mut app, |app| {
        !app.busy && app.status.starts_with("0 items")
    })?;
    assert_eq!(
        cached_calls(dir.path(), "labels"),
        2,
        "Mutation retained an old cached listing"
    );
    Ok(())
}
#[test]
fn cached_views_cannot_authorize_a_mutation_in_a_changed_workspace() -> Result<()> {
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use p4ui::{
        app::{App, Popup},
        model::Tab,
    };
    let (dir, connection) = cached_reads_fake()?;
    let mut app = App::new(
        Config {
            refresh_seconds: 0,
            ..Config::default()
        },
        connection,
    );
    app.switch_tab(Tab::Log);
    drive_app(&mut app, |app| !app.busy && app.detail == "Summary\n")?;
    std::fs::write(dir.path().join("other"), "changed")?;
    app.key(KeyEvent::new(KeyCode::Char('f'), KeyModifiers::NONE))?;
    app.key(KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE))?;
    drive_app(&mut app, |app| app.popup.is_some() && !app.mutating)?;
    assert!(
        matches!(&app.popup, Some(Popup::Message { text, .. }) if text.contains("Workspace identity changed"))
    );
    assert_eq!(cached_calls(dir.path(), "sync"), 0);
    Ok(())
}
#[test]
fn revision_drilldowns_never_reuse_a_different_changes_diff() -> Result<()> {
    use p4ui::{
        app::App,
        model::{Item, Tab},
    };
    let (dir, connection) = cached_reads_fake()?;
    let mut app = App::new(
        Config {
            refresh_seconds: 0,
            ..Config::default()
        },
        connection,
    );
    app.switch_tab(Tab::Log);
    drive_app(&mut app, |app| !app.busy && app.detail == "Summary\n")?;
    for revision in [2, 3] {
        app.view.items = vec![Item::Revision {
            depot: "//depot/file".into(),
            revision: revision.to_string(),
            action: "edit".into(),
            change: (revision * 100).to_string(),
            shelved: false,
        }];
        app.view.history = None;
        app.scroll(0, true);
        drive_app(&mut app, |app| {
            !app.busy && app.detail == format!("Diff for //depot/file#{revision}\n")
        })?;
    }
    assert_eq!(cached_calls(dir.path(), "diff2"), 2);
    Ok(())
}
#[test]
fn cancelled_tab_reads_preserve_other_successful_read_cache_entries() -> Result<()> {
    use p4ui::{app::App, model::Tab};
    let (dir, connection) = cached_reads_fake()?;
    let mut app = App::new(
        Config {
            refresh_seconds: 0,
            ..Config::default()
        },
        connection,
    );
    app.switch_tab(Tab::Log);
    drive_app(&mut app, |app| !app.busy && app.detail == "Summary\n")?;
    app.switch_tab(Tab::Status);
    drive_app(&mut app, |_| dir.path().join("started").exists())?;
    app.switch_tab(Tab::Log);
    drive_app(&mut app, |app| !app.busy && app.detail == "Summary\n")?;
    assert!(app.popup.is_none());
    assert_eq!(cached_calls(dir.path(), "changes"), 1);
    assert_eq!(cached_calls(dir.path(), "describe"), 1);
    Ok(())
}
fn history_queries(path: &std::path::Path) -> Result<Vec<Vec<String>>> {
    let text = std::fs::read_to_string(path).unwrap_or_default();
    let calls: Vec<Vec<String>> = text
        .lines()
        .map(serde_json::from_str)
        .collect::<serde_json::Result<_>>()?;
    Ok(calls
        .into_iter()
        .filter(|args| args.iter().any(|arg| arg == "changes"))
        .collect())
}
#[test]
fn status_tab_roundtrip_restores_both_rendered_panes_after_fresh_loading() -> Result<()> {
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use p4ui::{app::App, model::Tab, ui};
    use ratatui::{Terminal, backend::TestBackend};
    let source = r#"import json,pathlib,sys
args=sys.argv[1:]
def row(name,opened):
 print(json.dumps({'depotFile':'//depot/'+name,'clientFile':'/workspace/'+name,
                   'action':'edit','change':'default' if opened else ''}))
if 'info' in args:
 print(json.dumps({'userName':'tester','clientName':'test','clientRoot':'/workspace'}))
elif 'fstat' in args:
 if pathlib.Path(__file__).with_suffix('.insert').exists(): row('000_new_file',True)
 for index in range(80): row('opened_file%03d'%index,True)
elif 'reconcile' in args:
 assert '-n' in args
 for index in range(80): row('offline_file%03d'%index,False)
elif 'labels' in args: pass
elif 'diff' in args: print('File details')
else: raise AssertionError('Unexpected command: '+repr(args))
"#;
    let (_directory, connection) = fake(source)?;
    let insert = connection.executable.with_extension("insert");
    let mut app = App::new(
        Config {
            refresh_seconds: 0,
            ..Config::default()
        },
        connection,
    );
    drive_app(&mut app, |app| app.view.items.len() == 160 && !app.busy)?;
    app.key(KeyEvent::new(KeyCode::Char('/'), KeyModifiers::NONE))?;
    for c in "file".chars() {
        app.key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE))?;
    }
    app.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))?;
    let render = |app: &mut App| -> Result<(String, String)> {
        let mut terminal = Terminal::new(TestBackend::new(100, 30))?;
        terminal.draw(|frame| ui::draw(frame, app))?;
        let buffer = terminal.backend().buffer();
        let row = |y| {
            (0..42)
                .filter_map(|x| buffer.cell((x, y)))
                .map(|cell| cell.symbol())
                .collect()
        };
        Ok((row(4), row(16)))
    };
    for _ in 0..4 {
        app.key(KeyEvent::new(KeyCode::PageDown, KeyModifiers::NONE))?;
    }
    render(&mut app)?;
    app.scroll(72, true);
    let before = render(&mut app)?;
    assert!(before.0.contains("opened_file051"), "{}", before.0);
    assert!(before.1.contains("offline_file043"), "{}", before.1);
    assert_eq!(
        app.current().context("No selected file")?.key(),
        "//depot/offline_file052"
    );
    app.switch_tab(Tab::Labels);
    drive_app(&mut app, |app| {
        !app.busy && app.status.starts_with("0 items")
    })?;
    std::fs::write(insert, "insert")?;
    app.switch_tab(Tab::Status);
    assert!(
        app.view.items.is_empty(),
        "Refresh must not expose stale file records"
    );
    // Empty loading renders must not erase the saved viewport offsets.
    render(&mut app)?;
    drive_app(&mut app, |app| app.view.items.len() == 161 && !app.busy)?;
    assert_eq!(app.filter, "file");
    assert_eq!(
        app.current().context("No restored file")?.key(),
        "//depot/offline_file052"
    );
    assert_eq!(render(&mut app)?, before);
    app.focus_detail = true;
    app.key(KeyEvent::new(KeyCode::Char('t'), KeyModifiers::NONE))?;
    assert_eq!(
        app.current().context("No opened selection")?.key(),
        "//depot/000_new_file"
    );
    assert!(!app.focus_detail);
    app.key(KeyEvent::new(KeyCode::Char('t'), KeyModifiers::NONE))?;
    assert_eq!(
        app.current().context("No offline selection")?.key(),
        "//depot/offline_file000"
    );
    assert!(render(&mut app)?.1.contains("offline_file000"));
    Ok(())
}
#[test]
fn history_fills_viewport_and_pages_on_keyboard_wheel_and_resize() -> Result<()> {
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use p4ui::{app::App, model::Tab};
    let (_dir, c) = history_fake("many")?;
    let log = c.executable.with_extension("calls");
    let mut app = App::new(Config::default(), c);
    app.set_history_rows(24);
    app.switch_tab(Tab::Log);
    drive_app(&mut app, |app| app.view.items.len() == 24 && !app.busy)?;
    assert!(app.popup.is_none());
    let first: Vec<_> = app
        .view
        .items
        .iter()
        .map(|item| item.key().to_owned())
        .collect();
    let calls = history_queries(&log)?;
    assert_eq!(calls.len(), 1);
    assert!(calls[0].windows(2).any(|args| args == ["-m", "24"]));
    assert_eq!(calls[0].last().map(String::as_str), Some("..."));
    app.last_refresh = Instant::now() - Duration::from_secs(60);
    for _ in 0..5 {
        app.tick()?;
    }
    assert_eq!(
        history_queries(&log)?.len(),
        1,
        "Idle history must not fetch another page"
    );
    app.key(KeyEvent::new(KeyCode::PageDown, KeyModifiers::NONE))?;
    drive_app(&mut app, |app| app.view.items.len() == 48 && !app.busy)?;
    assert_eq!(
        app.selected, 23,
        "Appending must preserve the selected change"
    );
    assert_eq!(
        &first,
        &app.view.items[..24]
            .iter()
            .map(|item| item.key().to_owned())
            .collect::<Vec<_>>()
    );
    let calls = history_queries(&log)?;
    assert_eq!(calls.len(), 2);
    assert_eq!(
        calls[1].last().context("Missing scope")?,
        &format!(
            "...@{}",
            first.last().context("Missing change")?.parse::<u64>()? - 1
        )
    );
    app.scroll(100, true);
    drive_app(&mut app, |app| app.view.items.len() == 72 && !app.busy)?;
    app.set_history_rows(80);
    drive_app(&mut app, |app| app.view.items.len() == 80 && !app.busy)?;
    let calls = history_queries(&log)?;
    assert_eq!(calls.len(), 4);
    assert!(calls[3].windows(2).any(|args| args == ["-m", "8"]));
    let ids: std::collections::HashSet<_> = app.view.items.iter().map(|item| item.key()).collect();
    assert_eq!(ids.len(), 80, "Pagination must not repeat cached changes");
    let commands = std::fs::read_to_string(&log)?;
    assert_eq!(
        commands
            .lines()
            .filter(|line| line.contains("\"info\""))
            .count(),
        1,
        "Older pages must reuse connection metadata"
    );
    assert!(
        !commands.contains("fstat")
            && !commands.contains("counter")
            && !commands.contains("groups")
    );
    app.filter = "no matching changes".into();
    app.set_history_rows(100);
    app.scroll(100, true);
    for _ in 0..5 {
        app.tick()?;
    }
    assert_eq!(
        history_queries(&log)?.len(),
        4,
        "Filtering must not automatically scan more history"
    );
    Ok(())
}
#[test]
fn failed_history_page_preserves_cache_and_pauses_scroll_paging() -> Result<()> {
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use p4ui::{app::App, model::Tab};
    let (_dir, c) = history_fake("fail_older")?;
    let log = c.executable.with_extension("calls");
    let mut app = App::new(Config::default(), c);
    app.set_history_rows(24);
    app.switch_tab(Tab::Log);
    drive_app(&mut app, |app| app.view.items.len() == 24 && !app.busy)?;
    let cursor = app.view.history.as_ref().and_then(|page| page.next);
    app.scroll(100, true);
    drive_app(&mut app, |app| app.popup.is_some() && !app.busy)?;
    assert_eq!(app.view.items.len(), 24);
    assert_eq!(app.view.history.as_ref().and_then(|page| page.next), cursor);
    app.key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE))?;
    app.scroll(100, true);
    app.set_history_rows(80);
    for _ in 0..5 {
        app.tick()?;
    }
    assert_eq!(
        history_queries(&log)?.len(),
        2,
        "A failed page must not be retried automatically"
    );
    app.key(KeyEvent::new(KeyCode::Char('+'), KeyModifiers::NONE))?;
    drive_app(&mut app, |app| app.popup.is_some() && !app.busy)?;
    assert_eq!(
        history_queries(&log)?.len(),
        3,
        "Explicit retry must still work"
    );
    Ok(())
}
#[test]
fn switching_to_log_cancels_a_slow_status_read() -> Result<()> {
    use p4ui::{app::App, model::Tab};
    let (_dir, c) = history_fake("slow_status")?;
    let started = c.executable.with_extension("started");
    let mut app = App::new(Config::default(), c);
    app.set_history_rows(24);
    drive_app(&mut app, |_| started.exists())?;
    let start = Instant::now();
    app.switch_tab(Tab::Log);
    drive_app(&mut app, |app| app.view.items.len() == 24 && !app.busy)?;
    assert!(
        start.elapsed() < Duration::from_secs(5),
        "Log waited for the 10-second Status command"
    );
    assert!(app.popup.is_none());
    Ok(())
}

#[test]
fn quick_tab_roundtrip_discards_cancelled_reads_and_loads_a_fresh_view() -> Result<()> {
    use p4ui::{app::App, model::Tab};
    let (_dir, connection) = history_fake("slow_status_once")?;
    connection.set_max_processes(std::num::NonZeroUsize::MIN);
    let started = connection.executable.with_extension("started");
    let mut app = App::new(Config::default(), connection);
    drive_app(&mut app, |_| started.exists())?;
    app.switch_tab(Tab::Log);
    app.switch_tab(Tab::Status);
    drive_app(&mut app, |app| app.view.items.len() == 1 && !app.busy)?;
    assert!(
        app.popup.is_none(),
        "Cancelled result was shown after returning to the same tab"
    );
    Ok(())
}

#[test]
fn tab_switch_preserves_an_active_mutating_command() -> Result<()> {
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use p4ui::{
        app::{App, Popup},
        model::Tab,
    };
    let (_dir, c) = history_fake("slow_mutation")?;
    let started = c.executable.with_extension("started");
    let cancelled = c.cancelled.clone();
    let mut app = App::new(Config::default(), c);
    app.key(KeyEvent::new(KeyCode::Char('f'), KeyModifiers::NONE))?;
    app.key(KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE))?;
    drive_app(&mut app, |_| started.exists())?;
    assert!(app.mutating);
    app.switch_tab(Tab::Log);
    assert!(!cancelled.load(std::sync::atomic::Ordering::Relaxed));
    drive_app(&mut app, |app| !app.mutating)?;
    assert!(
        matches!(app.popup,Some(Popup::Message{ref title,ref text}) if title=="Command result" && text.contains("Mutation completed"))
    );
    Ok(())
}
