//! Verify failure boundaries with a deliberately broken/hanging CLI.
#![cfg(unix)]
use anyhow::Result;
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
    let (_dir, c) = fake("import sys\nprint('{\"data\":\"partial success\"}')\nsys.exit(2)")?;
    assert!(c.records(&["info"]).is_err());
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
    let source = r#"import json,sys,os,pathlib
with pathlib.Path(__file__).with_suffix('.calls').open('a') as f: f.write(json.dumps(sys.argv[1:])+'\n')
if 'info' in sys.argv:
 print(json.dumps({'userName':'tester','clientName':'test','clientRoot':'/workspace'}))
elif 'groups' in sys.argv:
 if os.environ['MODE']=='groups_denied':
  print(json.dumps({'severity':3,'generic':36,'data':'Permission denied'}));sys.exit(1)
 print(json.dumps({'group':'bounded','maxScanRows':'80','maxResults':'3'}))
elif 'counter' in sys.argv: print(json.dumps({'value':'10000'}))
elif 'changes' in sys.argv:
 assert any(arg.startswith('...@') and ',@' in arg for arg in sys.argv), 'Unbounded history query'
 if os.environ['MODE']=='groups_denied': assert not any(arg.startswith('-zmaxScanRows=') for arg in sys.argv)
 else:
  assert '-zmaxScanRows=80' in sys.argv
  assert sys.argv[sys.argv.index('-m')+1]=='3'
 if os.environ['MODE']=='limit':
  print(json.dumps({'severity':3,'generic':35,'data':'Too many rows scanned (over 80); see p4 help maxscanrows.'}));sys.exit(1)
 if os.environ['MODE']=='permission':
  print(json.dumps({'severity':3,'generic':36,'data':'Permission denied'}));sys.exit(1)
 for change in ['9999','9995','9991']:
  print(json.dumps({'change':change,'desc':'example','user':'tester','client':'test','status':'submitted'}))
"#;
    let (dir, mut connection) = fake(source)?;
    connection.environment.push(("MODE".into(), mode.into()));
    Ok((dir, connection))
}
#[test]
fn history_uses_bounded_ranges_and_never_raises_admin_limits() -> Result<()> {
    use p4ui::model::Tab;
    let (_dir, c) = history_fake("success")?;
    let view = c.load_history(Tab::Log, None)?;
    let page = view
        .history
        .ok_or_else(|| anyhow::anyhow!("Missing history metadata"))?;
    assert_eq!(page.requested_scan_limit, Some(80));
    assert_eq!(page.group_scan_limit, Some(80));
    assert_eq!(view.items.len(), 3);
    assert_eq!(page.next.map(|cursor| cursor.before), Some(9990));
    let calls = std::fs::read_to_string(c.executable.with_extension("calls"))?;
    assert!(calls.contains("...@9001,@10000"));
    assert_eq!(
        calls
            .lines()
            .filter(|line| line.contains("\"changes\""))
            .count(),
        1
    );
    Ok(())
}
#[test]
fn resource_failure_returns_smaller_cursor_without_automatic_retries() -> Result<()> {
    use p4ui::model::Tab;
    let (_dir, c) = history_fake("limit")?;
    let view = c.load_history(Tab::Log, None)?;
    assert!(view.items.is_empty());
    assert_eq!(view.warnings.len(), 1);
    let next = view
        .history
        .and_then(|page| page.next)
        .ok_or_else(|| anyhow::anyhow!("Expected retry cursor"))?;
    assert_eq!(next.before, 10000);
    assert_eq!(next.window, 500);
    let calls = std::fs::read_to_string(c.executable.with_extension("calls"))?;
    assert_eq!(
        calls
            .lines()
            .filter(|line| line.contains("\"changes\""))
            .count(),
        1
    );
    Ok(())
}
#[test]
fn history_permission_failures_are_fatal_and_unknown_limits_are_not_overridden() -> Result<()> {
    use p4ui::model::Tab;
    let (_dir, c) = history_fake("permission")?;
    assert!(c.load_history(Tab::Log, None).is_err());
    let (_dir, c) = history_fake("groups_denied")?;
    let view = c.load_history(Tab::Log, None)?;
    assert!(!view.warnings.is_empty());
    assert_eq!(
        view.history.and_then(|page| page.requested_scan_limit),
        None
    );
    Ok(())
}
