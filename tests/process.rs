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
