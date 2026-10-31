//! Count actual child processes, rather than only testing the in-memory gate.
#![cfg(unix)]
use anyhow::{Context, Result, ensure};
use p4ui::{config::Config, p4::Connection};
use std::{
    num::NonZeroUsize,
    os::unix::fs::PermissionsExt,
    time::{Duration, Instant},
};

fn fixture() -> Result<(tempfile::TempDir, Connection)> {
    let dir = tempfile::tempdir()?;
    let executable = dir.path().join("p4-counted");
    std::fs::write(
        &executable,
        r#"#!/usr/bin/env python3
import fcntl,json,os,pathlib,sys,time
path=pathlib.Path(__file__).with_suffix('.stats')
def change(delta):
 with path.with_suffix('.lock').open('a') as lock:
  fcntl.flock(lock,fcntl.LOCK_EX)
  state=json.loads(path.read_text()) if path.exists() else {'active':0,'peak':0,'starts':0}
  state['active']+=delta;state['peak']=max(state['peak'],state['active'])
  if delta>0: state['starts']+=1;state['pid']=os.getpid()
  path.write_text(json.dumps(state))
change(1)
path.with_suffix('.started').touch()
try:
 time.sleep(2 if 'slow' in sys.argv else float(os.environ.get('HOLD_SECONDS','0.2')))
 if 'fail' in sys.argv:
  print(json.dumps({'severity':3,'generic':35,'data':'Deliberate command failure'}));sys.exit(1)
 elif 'info' in sys.argv: print(json.dumps({'userName':'tester','clientName':'test','clientRoot':'/workspace'}))
 elif 'fstat' in sys.argv: print(json.dumps({'depotFile':'//depot/opened','clientFile':'/workspace/opened','action':'edit','change':'default'}))
 elif 'reconcile' in sys.argv:
  assert '-n' in sys.argv
  print(json.dumps({'depotFile':'//depot/new','clientFile':'/workspace/new','action':'add'}))
finally: change(-1)
"#,
    )?;
    std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o755))?;
    let mut connection = Connection::new(dir.path().into(), &Config::default());
    connection.executable = executable;
    Ok((dir, connection))
}
fn stats(connection: &Connection) -> Result<serde_json::Value> {
    Ok(serde_json::from_str(&std::fs::read_to_string(
        connection.executable.with_extension("stats"),
    )?)?)
}

#[test]
fn process_limit_is_shared_across_connection_clones() -> Result<()> {
    for (configured, maximum) in [(None, 4), (Some(1), 1), (Some(2), 2)] {
        let (_dir, connection) = fixture()?;
        let clones: Vec<_> = (0..8).map(|_| connection.clone()).collect();
        if let Some(configured) = configured {
            clones[0].set_max_processes(NonZeroUsize::new(configured).context("Invalid limit")?);
        }
        std::thread::scope(|scope| -> Result<()> {
            let tasks: Vec<_> = clones
                .iter()
                .map(|connection| scope.spawn(move || connection.info()))
                .collect();
            for task in tasks {
                task.join()
                    .map_err(|_| anyhow::anyhow!("Read task panicked"))??;
            }
            Ok(())
        })?;
        let state = stats(&connection)?;
        let peak = state["peak"].as_u64().context("Missing peak")?;
        assert!(
            peak <= maximum,
            "Observed {peak} children with limit {maximum}"
        );
        if maximum > 1 {
            assert!(peak > 1, "Independent reads were serialized");
        }
        assert_eq!(state["active"], 0);
        assert_eq!(state["starts"], 8);
    }
    Ok(())
}

#[test]
fn cli_limit_controls_parallel_status_queries() -> Result<()> {
    for (limit, expected_peak) in [(None, 3), (Some(1), 1), (Some(2), 2)] {
        let (directory, connection) = fixture()?;
        let mut command = std::process::Command::new(env!("CARGO_BIN_EXE_p4ui"));
        command.arg("--status");
        if let Some(limit) = limit {
            command.args(["--max-p4-processes", &limit.to_string()]);
        }
        let output = command
            .arg("--p4")
            .arg(&connection.executable)
            .arg("--directory")
            .arg(directory.path())
            .output()?;
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let view: serde_json::Value = serde_json::from_slice(&output.stdout)?;
        assert_eq!(view["items"].as_array().context("Missing items")?.len(), 2);
        let state = stats(&connection)?;
        assert_eq!(state["peak"], expected_peak);
        assert_eq!(state["starts"], 3);
        assert_eq!(state["active"], 0);
    }
    Ok(())
}

#[test]
fn queued_request_can_be_cancelled_without_waiting_for_the_active_process() -> Result<()> {
    use std::sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    };
    let (_dir, mut running) = fixture()?;
    running.set_max_processes(NonZeroUsize::MIN);
    running
        .environment
        .push(("HOLD_SECONDS".into(), "1".into()));
    let mut queued = running.clone();
    queued.cancelled = Arc::new(AtomicBool::new(false));
    let marker = running.executable.with_extension("started");
    std::thread::scope(|scope| -> Result<()> {
        let first = scope.spawn(|| running.info());
        let deadline = Instant::now() + Duration::from_secs(3);
        while !marker.exists() {
            ensure!(Instant::now() < deadline, "First command never started");
            std::thread::sleep(Duration::from_millis(5));
        }
        let start = Instant::now();
        let second = scope.spawn(|| queued.info());
        std::thread::sleep(Duration::from_millis(40));
        queued.cancelled.store(true, Ordering::Relaxed);
        let error = second
            .join()
            .map_err(|_| anyhow::anyhow!("Queued task panicked"))?
            .err()
            .context("Expected cancellation")?;
        assert!(format!("{error:#}").contains("cancelled"));
        assert!(
            start.elapsed() < Duration::from_millis(800),
            "Cancellation waited for the active process"
        );
        first
            .join()
            .map_err(|_| anyhow::anyhow!("Active task panicked"))??;
        Ok(())
    })?;
    let state = stats(&running)?;
    assert_eq!(
        state["starts"], 1,
        "Cancelled queued command should never spawn"
    );
    assert_eq!(state["active"], 0);
    Ok(())
}

#[test]
fn failed_and_timed_out_children_release_slots_and_are_reaped() -> Result<()> {
    let (_dir, mut connection) = fixture()?;
    connection.set_max_processes(NonZeroUsize::MIN);
    let mut missing = connection.clone();
    missing.executable = missing.directory.join("missing-p4");
    assert!(missing.info().is_err());
    assert!(connection.records(&["fail"]).is_err());
    connection.timeout = Duration::from_millis(100);
    let error = connection
        .records(&["slow"])
        .err()
        .context("Expected timeout")?;
    assert!(format!("{error:#}").contains("timed out"));
    let pid = stats(&connection)?["pid"]
        .as_u64()
        .context("Missing timed-out PID")?;
    let reaped = std::process::Command::new("python3").args(["-c",
        "import os,sys\ntry: os.kill(int(sys.argv[1]),0)\nexcept ProcessLookupError: sys.exit(0)\nsys.exit(1)",
        &pid.to_string()]).status()?;
    assert!(
        reaped.success(),
        "Timed-out child is still alive or unreaped"
    );
    connection.timeout = Duration::from_secs(2);
    connection.info()?;
    Ok(())
}
