//! Real CLI + p4d tests. Each test owns a private server, workspace, settings and tickets.
//! Run with: cargo test --test perforce -- --ignored
#![cfg(unix)]
use anyhow::{Context, Result, ensure};
use p4ui::{
    config::Config,
    model::*,
    p4::{Connection, field},
};
use std::{path::PathBuf, process::Command};
use tempfile::TempDir;

struct Fixture {
    _dir: TempDir,
    connection: Connection,
}
impl Fixture {
    fn new() -> Result<Self> {
        let dir = tempfile::Builder::new().prefix("p4ui-test-").tempdir()?;
        let server = dir.path().join("server");
        let ws = dir.path().join("workspace");
        std::fs::create_dir(&server)?;
        std::fs::create_dir(&ws)?;
        let which = Command::new("which")
            .arg("p4d")
            .output()
            .context("p4d must be on PATH")?;
        ensure!(
            which.status.success(),
            "Install p4d to run integration tests"
        );
        let daemon = String::from_utf8(which.stdout)?.trim().to_string();
        ensure!(
            !daemon.contains(char::is_whitespace)
                && !server.to_string_lossy().contains(char::is_whitespace),
            "Test paths for the rsh transport must not contain whitespace"
        );
        let mut connection = Connection::new(ws.clone(), &Config::default());
        connection.port = Some(format!(
            "rsh:{daemon} -r {} -L {}/log -i",
            server.display(),
            server.display()
        ));
        connection.user = Some("tester".into());
        connection.client = Some("test".into());
        connection.environment = vec![
            ("P4CONFIG".into(), ".p4ui-nonexistent-config".into()),
            (
                "P4ENVIRO".into(),
                dir.path().join("env").to_string_lossy().into(),
            ),
            (
                "P4TICKETS".into(),
                dir.path().join("tickets").to_string_lossy().into(),
            ),
            (
                "P4TRUST".into(),
                dir.path().join("trust").to_string_lossy().into(),
            ),
            ("P4PASSWD".into(), String::new()),
            ("P4CHARSET".into(), String::new()),
            ("P4HOST".into(), String::new()),
        ];
        // Initialize database first: P4D 2026.1 upgrades security defaults at first startup.
        connection.records(&["info"])?;
        for setting in [
            "security=0",
            "dm.user.noautocreate=0",
            "dm.user.setinitialpasswd=1",
            "dm.user.hideinvalid=0",
        ] {
            let out = Command::new(&daemon)
                .arg("-r")
                .arg(&server)
                .arg(format!("-cset {setting}"))
                .output()?;
            ensure!(
                out.status.success(),
                "p4d setup: {}",
                String::from_utf8_lossy(&out.stderr)
            );
        }
        let form = format!(
            "Client: test\nOwner: tester\nRoot: {}\nOptions: noallwrite noclobber nocompress unlocked nomodtime normdir\nLineEnd: local\nView:\n\t//depot/... //test/...\n",
            ws.display()
        );
        connection.execute(&["client", "-i"], Some(&form), false)?;
        Ok(Self {
            _dir: dir,
            connection,
        })
    }
    fn path(&self, name: &str) -> PathBuf {
        self.connection.directory.join(name)
    }
    fn write(&self, name: &str, text: &str) -> Result<()> {
        std::fs::write(self.path(name), text)?;
        Ok(())
    }
    fn files(&self) -> Result<Vec<FileEntry>> {
        Ok(self
            .connection
            .load(Tab::Status, 200)?
            .items
            .into_iter()
            .filter_map(|i| if let Item::File(f) = i { Some(f) } else { None })
            .collect())
    }
    fn file(&self, name: &str) -> Result<FileEntry> {
        self.files()?
            .into_iter()
            .find(|f| f.local.ends_with(name))
            .context("Expected workspace file")
    }
    fn seed(&mut self) -> Result<()> {
        self.write("hello.txt", "one\ntwo\nthree\n")?;
        self.connection
            .act(Action::Reconcile(None, "default".into()))?;
        self.connection
            .act(Action::Submit("default".into(), "Initial content".into()))?;
        Ok(())
    }
    fn change(&mut self, desc: &str) -> Result<String> {
        self.connection.act(Action::NewChange(desc.into()))?;
        let r = self
            .connection
            .records(&["changes", "-s", "pending", "-c", "test", "-m", "1"])?;
        r.first()
            .map(|r| field(r, "change"))
            .context("No pending change")
    }
}
#[test]
#[ignore = "requires p4 and p4d"]
fn full_reconcile_diff_submit_history_and_annotate() -> Result<()> {
    let mut f = Fixture::new()?;
    f.seed()?;
    ensure!(f.files()?.is_empty());
    let mut files = f.connection.load(Tab::Files, 200)?.items;
    let Item::File(file) = files.remove(0) else {
        anyhow::bail!("Expected file")
    };
    ensure!(
        f.connection
            .detail(Detail::Item(Item::File(file.clone()), Tab::Files))?
            .contains("two")
    );
    f.connection.act(Action::Edit(file, "default".into()))?;
    f.write("hello.txt", "one\nchanged\nthree\n")?;
    let file = f.file("hello.txt")?;
    let diff = f
        .connection
        .detail(Detail::Item(Item::File(file.clone()), Tab::Status))?;
    ensure!(diff.contains("-two") && diff.contains("+changed"), "{diff}");
    f.connection.act(Action::Submit(
        "default".into(),
        "Second revision\n\nMultiline description".into(),
    ))?;
    let log = f.connection.load(Tab::Log, 200)?;
    ensure!(log.items.len() == 2);
    let revisions = f.connection.change_files(log.items[0].key(), Tab::Log)?;
    ensure!(revisions.items.len() == 1);
    ensure!(
        f.connection
            .detail(Detail::Item(revisions.items[0].clone(), Tab::Log))?
            .contains("+changed")
    );
    let initial = f.connection.change_files(log.items[1].key(), Tab::Log)?;
    ensure!(
        f.connection
            .detail(Detail::Item(initial.items[0].clone(), Tab::Log))?
            .contains("two")
    );
    ensure!(
        f.connection
            .detail(Detail::Item(log.items[0].clone(), Tab::Log))?
            .contains("+changed")
    );
    ensure!(
        f.connection
            .detail(Detail::Annotate(file.clone()))?
            .contains("changed")
    );
    ensure!(
        f.connection
            .detail(Detail::FileLog(file.clone()))?
            .contains("Second revision")
    );
    ensure!(
        f.connection
            .detail(Detail::Revision(file, "1".into()))?
            .contains("two")
    );
    Ok(())
}
#[test]
#[ignore = "requires p4 and p4d"]
fn numbered_changelist_shelve_unshelve_and_delete() -> Result<()> {
    let mut f = Fixture::new()?;
    f.seed()?;
    let id = f.change("Work in progress")?;
    let file = match f.connection.load(Tab::Files, 200)?.items.remove(0) {
        Item::File(f) => f,
        _ => unreachable!(),
    };
    f.connection.act(Action::Edit(file, id.clone()))?;
    f.write("hello.txt", "shelved content\n")?;
    f.connection.act(Action::EditChange(
        id.clone(),
        "Updated description\nFiles:\n//not-a-form-injection".into(),
    ))?;
    f.connection.act(Action::Shelve(id.clone()))?;
    let shelves = f.connection.load(Tab::Shelves, 200)?;
    ensure!(shelves.items.len() == 1);
    let revisions = f.connection.change_files(&id, Tab::Shelves)?;
    ensure!(revisions.items.len() == 1);
    ensure!(
        f.connection
            .detail(Detail::Item(revisions.items[0].clone(), Tab::Shelves))?
            .contains("+shelved content")
    );
    let opened = f.connection.change_files(&id, Tab::Changes)?;
    ensure!(matches!(&opened.items[0],Item::File(file) if file.opened));
    ensure!(
        f.connection
            .detail(Detail::Item(shelves.items[0].clone(), Tab::Shelves))?
            .contains("+shelved content")
    );
    ensure!(
        f.connection.act(Action::Shelve(id.clone())).is_err(),
        "Existing shelf must be protected"
    );
    f.connection
        .act(Action::Revert(f.file("hello.txt")?, false))?;
    ensure!(std::fs::read_to_string(f.path("hello.txt"))?.contains("two"));
    f.connection
        .act(Action::Unshelve(id.clone(), "default".into()))?;
    ensure!(std::fs::read_to_string(f.path("hello.txt"))? == "shelved content\n");
    f.connection
        .act(Action::Reopen(f.file("hello.txt")?, id.clone()))?;
    ensure!(f.file("hello.txt")?.change == id);
    f.connection
        .act(Action::Revert(f.file("hello.txt")?, false))?;
    f.connection.act(Action::DeleteShelf(id.clone()))?;
    f.connection.act(Action::DeleteChange(id))?;
    ensure!(f.connection.load(Tab::Shelves, 200)?.items.is_empty());
    ensure!(f.connection.load(Tab::Changes, 200)?.items.len() == 1);
    Ok(())
}
#[test]
#[ignore = "requires p4 and p4d"]
fn offline_changes_keep_content_revert_and_delete() -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let mut f = Fixture::new()?;
    f.seed()?;
    std::fs::set_permissions(f.path("hello.txt"), std::fs::Permissions::from_mode(0o644))?;
    f.write("hello.txt", "offline edit\n")?;
    let file = f.file("hello.txt")?;
    ensure!(!file.opened && file.action == "edit");
    f.connection
        .act(Action::Reconcile(Some(file), "default".into()))?;
    f.connection
        .act(Action::Revert(f.file("hello.txt")?, true))?;
    ensure!(std::fs::read_to_string(f.path("hello.txt"))? == "offline edit\n");
    f.connection
        .act(Action::Reconcile(None, "default".into()))?;
    f.connection
        .act(Action::Revert(f.file("hello.txt")?, false))?;
    let file = match f.connection.load(Tab::Files, 200)?.items.remove(0) {
        Item::File(f) => f,
        _ => unreachable!(),
    };
    f.connection.act(Action::Delete(file))?;
    ensure!(!f.path("hello.txt").exists());
    let deleted = f.file("hello.txt")?;
    ensure!(
        f.connection
            .detail(Detail::Item(Item::File(deleted.clone()), Tab::Status))?
            .contains("-two")
    );
    f.connection.act(Action::Revert(deleted, false))?;
    std::fs::remove_file(f.path("hello.txt"))?;
    let offline_deleted = f.file("hello.txt")?;
    ensure!(!offline_deleted.opened && offline_deleted.action == "delete");
    f.connection
        .act(Action::Reconcile(Some(offline_deleted), "default".into()))?;
    f.connection
        .act(Action::Submit("default".into(), "Remove file".into()))?;
    ensure!(f.connection.load(Tab::Files, 200)?.items.is_empty());
    Ok(())
}
#[test]
#[ignore = "requires p4 and p4d"]
fn special_names_binary_and_symlinks() -> Result<()> {
    use std::os::unix::fs::symlink;
    let mut f = Fixture::new()?;
    let name = "日本語 space @hash#percent%star*.txt";
    f.write(name, "special content\n")?;
    f.connection
        .act(Action::Reconcile(None, "default".into()))?;
    let file = f.file(name)?;
    ensure!(
        file.depot.contains("%40")
            && file.depot.contains("%23")
            && file.depot.contains("%25")
            && file.depot.contains("%2A"),
        "{}",
        file.depot
    );
    ensure!(
        f.connection
            .detail(Detail::Item(Item::File(file), Tab::Status))?
            .contains("special content")
    );
    f.connection
        .act(Action::Submit("default".into(), "Special filename".into()))?;
    let file = match f.connection.load(Tab::Files, 200)?.items.remove(0) {
        Item::File(f) => f,
        _ => unreachable!(),
    };
    ensure!(
        f.connection
            .detail(Detail::Item(Item::File(file), Tab::Files))?
            .contains("special content")
    );
    std::fs::write(f.path("binary.bin"), [0, 1, 2, 255])?;
    symlink("/dev/zero", f.path("link"))?;
    f.connection
        .act(Action::Reconcile(None, "default".into()))?;
    let binary = f.file("binary.bin")?;
    ensure!(
        f.connection
            .detail(Detail::Item(Item::File(binary), Tab::Status))?
            .contains("Binary")
    );
    let link = f.file("link")?;
    ensure!(
        f.connection
            .detail(Detail::Item(Item::File(link), Tab::Status))?
            .contains("Symlink")
    );
    f.connection.act(Action::Submit(
        "default".into(),
        "Binary and symlink".into(),
    ))?;
    for item in f.connection.load(Tab::Files, 200)?.items {
        if let Item::File(file) = &item {
            if file.local.ends_with("binary.bin") {
                ensure!(
                    f.connection
                        .detail(Detail::Item(item.clone(), Tab::Files))?
                        .contains("Binary")
                );
            }
            if file.local.ends_with("link") {
                ensure!(
                    f.connection
                        .detail(Detail::Item(item.clone(), Tab::Files))?
                        .contains("/dev/zero")
                );
            }
        }
    }
    Ok(())
}
#[test]
#[ignore = "requires p4 and p4d"]
fn move_lock_submit_numbered_and_sync() -> Result<()> {
    let mut f = Fixture::new()?;
    f.seed()?;
    let id = f.change("Rename file")?;
    let file = match f.connection.load(Tab::Files, 200)?.items.remove(0) {
        Item::File(f) => f,
        _ => unreachable!(),
    };
    f.connection.act(Action::Edit(file, id.clone()))?;
    f.connection.act(Action::Lock(f.file("hello.txt")?, true))?;
    f.connection
        .act(Action::Lock(f.file("hello.txt")?, false))?;
    f.connection
        .act(Action::Move(f.file("hello.txt")?, "new name@#.txt".into()))?;
    ensure!(!f.path("hello.txt").exists() && f.path("new name@#.txt").exists());
    f.connection.act(Action::Submit(id, String::new()))?;
    f.connection.act(Action::Sync)?;
    ensure!(f.files()?.is_empty());
    Ok(())
}
#[test]
#[ignore = "requires p4 and p4d"]
fn permissions_invalid_workspace_and_output_limits_are_errors() -> Result<()> {
    let mut f = Fixture::new()?;
    f.seed()?;
    let mut bad = f.connection.clone();
    bad.client = Some("does-not-exist".into());
    ensure!(bad.load(Tab::Status, 200).is_err());
    let mut short = f.connection.clone();
    short.max_output = 5;
    ensure!(short.info().is_err());
    ensure!(
        f.connection
            .act(Action::Reopen(
                f.file("hello.txt").unwrap_or_default(),
                "1 ...".into()
            ))
            .is_err()
    );
    f.connection.execute(
        &["protect", "-i"],
        Some("Protections:\n\tsuper user tester * //...\n\tlist user nobody * -//...\n"),
        false,
    )?;
    bad.user = Some("nobody".into());
    bad.client = Some("test".into());
    ensure!(bad.load(Tab::Files, 200).is_err());
    Ok(())
}
#[test]
#[ignore = "requires p4 and p4d"]
fn status_honors_current_directory_scope_and_ignores() -> Result<()> {
    let mut f = Fixture::new()?;
    std::fs::create_dir(f.path("sub"))?;
    f.write("outside.txt", "outside")?;
    f.write("sub/inside.txt", "inside")?;
    f.write(".p4ignore", "ignored.txt\n")?;
    f.write("ignored.txt", "ignored")?;
    f.connection
        .environment
        .push(("P4IGNORE".into(), ".p4ignore".into()));
    ensure!(f.files()?.iter().all(|x| !x.local.ends_with("ignored.txt")));
    f.connection.directory = f.path("sub");
    let files = f.files()?;
    ensure!(files.len() == 1 && files[0].local.ends_with("inside.txt"));
    f.connection
        .act(Action::Reconcile(None, "default".into()))?;
    ensure!(f.files()?.len() == 1);
    Ok(())
}
#[test]
#[ignore = "requires p4 and p4d"]
fn workspace_selection_labels_and_stream_listing() -> Result<()> {
    let mut f = Fixture::new()?;
    f.seed()?;
    let other = f._dir.path().join("other");
    std::fs::create_dir(&other)?;
    f.connection.execute(
        &["client", "-i"],
        Some(&format!(
            "Client: other\nOwner: tester\nRoot: {}\nView:\n\t//depot/... //other/...\n",
            other.display()
        )),
        false,
    )?;
    let clients = f.connection.load(Tab::Workspaces, 200)?;
    ensure!(clients.items.len() == 2);
    f.connection.execute(&["label","-i"],Some("Label: release-1\nOwner: tester\nDescription:\n\tFirst release\nView:\n\t//depot/...\n"),false)?;
    ensure!(f.connection.load(Tab::Labels, 200)?.items.len() == 1);
    ensure!(f.connection.load(Tab::Streams, 200)?.items.is_empty());
    f.connection.act(Action::SwitchWorkspace("other".into()))?;
    ensure!(f.connection.directory == other && f.connection.info()?.client == "other");
    f.connection.act(Action::Sync)?;
    ensure!(other.join("hello.txt").exists());
    Ok(())
}
#[test]
#[ignore = "requires p4 and p4d"]
fn concurrent_edits_conflict_detection_and_resolve() -> Result<()> {
    let mut f = Fixture::new()?;
    f.seed()?;
    let other = f._dir.path().join("other");
    std::fs::create_dir(&other)?;
    f.connection.execute(
        &["client", "-i"],
        Some(&format!(
            "Client: other\nOwner: tester\nRoot: {}\nView:\n\t//depot/... //other/...\n",
            other.display()
        )),
        false,
    )?;
    let mut second = f.connection.clone();
    second.client = Some("other".into());
    second.directory = other.clone();
    second.act(Action::Sync)?;
    let file = match f.connection.load(Tab::Files, 200)?.items.remove(0) {
        Item::File(f) => f,
        _ => unreachable!(),
    };
    f.connection
        .act(Action::Edit(file.clone(), "default".into()))?;
    f.write("hello.txt", "ours\ntwo\nthree\n")?;
    second.act(Action::Edit(file, "default".into()))?;
    std::fs::write(other.join("hello.txt"), "theirs\ntwo\nthree\n")?;
    second.act(Action::Submit("default".into(), "Incoming change".into()))?;
    ensure!(
        f.connection
            .act(Action::Submit(
                "default".into(),
                "Must fail before sync".into()
            ))
            .is_err()
    );
    f.connection.act(Action::Sync)?;
    let resolve = f.connection.load(Tab::Resolve, 200)?;
    ensure!(
        resolve.items.len() == 1,
        "Expected unresolved file: {resolve:?}"
    );
    let file = match resolve.items[0].clone() {
        Item::File(f) => f,
        _ => unreachable!(),
    };
    // Safe automatic resolve must preserve the conflicting workspace version.
    let _ = f
        .connection
        .act(Action::Resolve(file.clone(), "-as".into()));
    ensure!(std::fs::read_to_string(f.path("hello.txt"))?.starts_with("ours"));
    ensure!(f.connection.load(Tab::Resolve, 200)?.items.len() == 1);
    f.connection.act(Action::Resolve(file, "-ay".into()))?;
    ensure!(
        f.connection.load(Tab::Resolve, 200)?.items.is_empty(),
        "Resolved fstat: {:?}",
        f.connection.records(&["fstat", "-Ro", "-Or", "..."])?
    );
    // A failed default submit may move its files to a numbered pending changelist.
    let id = f.file("hello.txt")?.change;
    if id != "default" {
        f.connection.act(Action::EditChange(
            id.clone(),
            "Resolved keeping ours".into(),
        ))?;
    }
    f.connection
        .act(Action::Submit(id, "Resolved keeping ours".into()))?;
    Ok(())
}
#[test]
#[ignore = "requires p4 and p4d"]
fn stream_switch_requires_clean_workspace() -> Result<()> {
    let mut f = Fixture::new()?;
    f.connection.execute(&["depot","-i"],Some("Depot: streams\nOwner: tester\nType: stream\nMap: streams/...\nStreamDepth: //streams/1\n"),false)?;
    f.connection.execute(&["stream","-i"],Some("Stream: //streams/main\nOwner: tester\nName: main\nParent: none\nType: mainline\nParentView: inherit\nPaths:\n\tshare ...\n"),false)?;
    f.connection.execute(&["stream","-i"],Some("Stream: //streams/dev\nOwner: tester\nName: dev\nParent: //streams/main\nType: development\nParentView: inherit\nPaths:\n\tshare ...\n"),false)?;
    ensure!(f.connection.load(Tab::Streams, 200)?.items.len() == 2);
    f.connection.execute(
        &["client", "-i"],
        Some(&format!(
            "Client: test\nOwner: tester\nRoot: {}\nStream: //streams/main\n",
            f.connection.directory.display()
        )),
        false,
    )?;
    ensure!(f.connection.info()?.stream == "//streams/main");
    f.write("untracked.txt", "do not lose me\n")?;
    ensure!(
        f.connection
            .act(Action::SwitchStream("//streams/dev".into()))
            .is_err()
    );
    ensure!(std::fs::read_to_string(f.path("untracked.txt"))?.contains("do not lose me"));
    f.connection
        .act(Action::Reconcile(None, "default".into()))?;
    ensure!(
        f.connection
            .act(Action::SwitchStream("//streams/dev".into()))
            .is_err()
    );
    f.connection
        .act(Action::Submit("default".into(), "Mainline content".into()))?;
    f.connection
        .act(Action::SwitchStream("//streams/dev".into()))?;
    ensure!(f.connection.info()?.stream == "//streams/dev");
    Ok(())
}
#[test]
#[ignore = "requires p4 and p4d"]
fn stream_creation_population_merge_and_deletion() -> Result<()> {
    let mut f = Fixture::new()?;
    f.connection.execute(&["depot","-i"],Some("Depot: streams\nOwner: tester\nType: stream\nMap: streams/...\nStreamDepth: //streams/1\n"),false)?;
    f.connection
        .act(Action::CreateStream("//streams/main".into(), String::new()))?;
    f.connection.execute(
        &["client", "-i"],
        Some(&format!(
            "Client: test\nOwner: tester\nRoot: {}\nStream: //streams/main\n",
            f.connection.directory.display()
        )),
        false,
    )?;
    f.seed()?;
    f.connection.act(Action::CreateStream(
        "//streams/dev".into(),
        "//streams/main".into(),
    ))?;
    ensure!(
        f.connection
            .act(Action::CreateStream(
                "//streams/dev".into(),
                "//streams/main".into()
            ))
            .is_err()
    );
    f.connection
        .act(Action::SwitchStream("//streams/dev".into()))?;
    f.connection.act(Action::MergeStream(
        "//streams/main".into(),
        "default".into(),
    ))?;
    for item in f.connection.load(Tab::Resolve, 200)?.items {
        if let Item::File(file) = item {
            f.connection.act(Action::Resolve(file, "-am".into()))?;
        }
    }
    f.connection
        .act(Action::Submit(
            "default".into(),
            "Populate development".into(),
        ))
        .context("Populate submit")?;
    ensure!(f.path("hello.txt").exists());
    f.connection
        .act(Action::SwitchStream("//streams/main".into()))?;
    let file = match f.connection.load(Tab::Files, 200)?.items.remove(0) {
        Item::File(f) => f,
        _ => unreachable!(),
    };
    f.connection.act(Action::Edit(file, "default".into()))?;
    f.write("hello.txt", "incoming parent change\n")?;
    f.connection
        .act(Action::Submit("default".into(), "Parent change".into()))
        .context("Parent submit")?;
    f.connection
        .act(Action::SwitchStream("//streams/dev".into()))?;
    f.connection.act(Action::MergeStream(
        "//streams/main".into(),
        "default".into(),
    ))?;
    for item in f.connection.load(Tab::Resolve, 200)?.items {
        if let Item::File(file) = item {
            f.connection.act(Action::Resolve(file, "-am".into()))?;
        }
    }
    f.connection
        .act(Action::Submit(
            "default".into(),
            "Merge parent change".into(),
        ))
        .context("Merged submit")?;
    ensure!(std::fs::read_to_string(f.path("hello.txt"))?.contains("incoming parent change"));
    let file = match f.connection.load(Tab::Files, 200)?.items.remove(0) {
        Item::File(f) => f,
        _ => unreachable!(),
    };
    f.connection.act(Action::Edit(file, "default".into()))?;
    f.write("hello.txt", "promoted development change\n")?;
    f.connection.act(Action::Submit(
        "default".into(),
        "Advance development".into(),
    ))?;
    f.connection
        .act(Action::SwitchStream("//streams/main".into()))?;
    // Copy is allowed toward the parent after development has merged incoming changes.
    f.connection
        .act(Action::CopyStream("//streams/dev".into(), "default".into()))?;
    f.connection
        .act(Action::Submit(
            "default".into(),
            "Copy from development".into(),
        ))
        .context("Copy submit")?;
    f.connection.act(Action::CreateStream(
        "//streams/empty".into(),
        "//streams/main".into(),
    ))?;
    f.connection
        .act(Action::DeleteStream("//streams/empty".into()))?;
    ensure!(f.connection.load(Tab::Streams, 200)?.items.len() == 2);
    Ok(())
}
#[test]
#[ignore = "requires p4 and p4d"]
fn shelved_add_preview_and_large_file_limits() -> Result<()> {
    let mut f = Fixture::new()?;
    f.write("new.txt", &"new shelved content\n".repeat(300))?;
    let id = f.change("Shelve an addition")?;
    f.connection.act(Action::Reconcile(None, id.clone()))?;
    f.connection.act(Action::Shelve(id.clone()))?;
    let revisions = f.connection.change_files(&id, Tab::Shelves)?;
    ensure!(revisions.items.len() == 1);
    let item = revisions.items[0].clone();
    ensure!(
        f.connection
            .detail(Detail::Item(item.clone(), Tab::Shelves))?
            .contains("new shelved content")
    );
    f.connection.max_output = 2048;
    let result = f.connection.detail(Detail::Item(item, Tab::Shelves));
    ensure!(result.is_err(), "Large shelved content must be limited");
    Ok(())
}
#[test]
#[ignore = "requires p4 and p4d"]
fn cli_p4port_environment_and_connection_precedence() -> Result<()> {
    let mut fixture = Fixture::new()?;
    fixture.seed()?;
    let config = fixture._dir.path().join("p4ui.toml");
    std::fs::write(&config, "refresh_seconds = 0\n")?;
    let port = fixture
        .connection
        .port
        .as_deref()
        .context("Fixture server address missing")?;
    let base = || {
        let mut command = Command::new(env!("CARGO_BIN_EXE_p4ui"));
        command
            .envs(fixture.connection.environment.iter().cloned())
            .env("P4USER", "tester")
            .env("P4CLIENT", "test")
            .args([
                "--directory",
                &fixture.connection.directory.to_string_lossy(),
            ])
            .arg("--config")
            .arg(&config);
        command
    };
    let run = |command: &mut Command| -> Result<String> {
        let output = command.output()?;
        ensure!(
            output.status.success(),
            "p4ui failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        Ok(String::from_utf8(output.stdout)?)
    };
    // No --port/-p anywhere: the exported environment is enough to connect.
    ensure!(run(base().env("P4PORT", port).arg("--check"))?.contains("tester@test"));
    let status: serde_json::Value =
        serde_json::from_str(&run(base().env("P4PORT", port).arg("--status"))?)?;
    ensure!(
        status["info"]["client"] == "test" && status["items"].as_array().is_some_and(Vec::is_empty)
    );
    // Both spellings of the explicit flag take precedence over an unusable env address.
    for flag in ["--port", "-p"] {
        ensure!(
            run(base()
                .env("P4PORT", "127.0.0.1:1")
                .args([flag, port, "--check"]))?
            .contains("tester@test")
        );
    }
    let p4config = fixture.path(".p4ui-nonexistent-config");
    // A provided environment address is an explicit p4ui connection choice.
    std::fs::write(&p4config, "P4PORT=127.0.0.1:1\n")?;
    ensure!(run(base().env("P4PORT", port).arg("--check"))?.contains("tester@test"));
    // With no port or an empty env value, retain the existing p4 config fallback.
    std::fs::write(&p4config, format!("P4PORT={port}\n"))?;
    ensure!(run(base().env_remove("P4PORT").arg("--check"))?.contains("tester@test"));
    ensure!(run(base().env("P4PORT", "").arg("--check"))?.contains("tester@test"));
    // Help advertises the variable without leaking its current server address.
    let help = run(base()
        .env("P4PORT", "private-server.example:1666")
        .arg("--help"))?;
    ensure!(help.contains("P4PORT") && !help.contains("private-server.example"));
    Ok(())
}
