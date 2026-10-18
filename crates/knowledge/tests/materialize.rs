//! Materializing the embedded knowledge bundle into a fresh cache directory.

#![cfg(unix)]

use std::os::unix::fs::{DirBuilderExt as _, PermissionsExt as _};

/// Set in the child process that runs under the permissive umask.
const UMASK_CHILD_VAR: &str = "KNOWLEDGE_MATERIALIZE_UMASK_CHILD";

/// Umask of a default login session, under which a plain `mkdir` is 0755.
const PERMISSIVE_UMASK: &str = "022";

#[test]
fn materializing_into_a_fresh_cache_creates_its_directories_owner_private() {
    // The umask is process-wide, so a child owns it and this binary keeps its own.
    let output = std::process::Command::new("/bin/sh")
        .args([
            "-c",
            &format!("umask {PERMISSIVE_UMASK}; exec \"$0\" \"$@\""),
        ])
        .arg(std::env::current_exe().expect("test executable"))
        .args(pohunek_test_support::exact_test_filter(
            module_path!(),
            "materialize_under_a_permissive_umask_child",
        ))
        .args(["--nocapture", "--test-threads=1"])
        .env(UMASK_CHILD_VAR, "1")
        .output()
        .expect("run the child under the permissive umask");
    assert!(
        output.status.success() && String::from_utf8_lossy(&output.stdout).contains("1 passed"),
        "child failed: {}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn materialize_under_a_permissive_umask_child() {
    if std::env::var_os(UMASK_CHILD_VAR).is_none() {
        return;
    }
    let root = pohunek_test_support::tempdir().expect("private fixture root");
    // `pohunek` stands for `XDG_CACHE_HOME/pohunek`, which does not exist yet.
    let cache = root.path().join("pohunek");

    let bundle =
        knowledge::materialize(&cache, "0123456789abcdef").expect("materialize the bundle");

    assert!(bundle.starts_with(&cache), "{}", bundle.display());
    for directory in [cache.clone(), cache.join("knowledge")] {
        let mode = std::fs::metadata(&directory)
            .expect("the cache directory exists")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o700, "{}", directory.display());
    }
}

#[test]
fn materializing_repairs_a_nonempty_incomplete_version_and_reuses_the_repair() {
    let root = pohunek_test_support::tempdir().expect("private fixture root");
    let cache = root.path().join("pohunek");
    let version = "interrupted-version";
    let target = knowledge::materialize(&cache, version).expect("initial materialization");
    std::fs::remove_file(target.join(".complete")).expect("simulate an interrupted publication");
    std::fs::write(target.join("log.md"), b"partial log").expect("leave partial bundle content");
    std::fs::write(target.join("interrupted-write"), b"orphan")
        .expect("leave an extra partial file");

    let repaired = knowledge::materialize(&cache, version).expect("repair the incomplete version");

    assert_eq!(repaired, target);
    assert_eq!(
        std::fs::read_to_string(repaired.join("log.md")).expect("repaired log"),
        knowledge::embedded_bundle()
            .get_text("log.md")
            .expect("embedded log")
    );
    assert!(!repaired.join("interrupted-write").exists());
    assert!(repaired.join(".complete").is_file());
    assert_eq!(
        knowledge::materialize(&cache, version).expect("reuse the repaired version"),
        repaired
    );
    knowledge::gc(&cache, version).expect("collect the retired incomplete version");
    let remaining: Vec<_> = std::fs::read_dir(cache.join("knowledge"))
        .expect("read the private knowledge root")
        .map(|entry| entry.expect("directory entry"))
        .filter(|entry| entry.file_type().expect("entry type").is_dir())
        .map(|entry| entry.file_name())
        .collect();
    assert_eq!(remaining, [std::ffi::OsString::from(version)]);
}

#[test]
fn incomplete_version_repair_never_follows_a_version_or_marker_symlink() {
    use std::os::unix::fs::symlink;

    let root = pohunek_test_support::tempdir().expect("private fixture root");
    let cache = root.path().join("pohunek");
    let external = root.path().join("external");
    std::fs::create_dir(&external).expect("external directory");
    std::fs::write(external.join("sentinel"), b"preserve external content")
        .expect("external sentinel");
    let target = knowledge::materialize(&cache, "version").expect("initial materialization");
    std::fs::remove_dir_all(&target).expect("replace the version with a symlink");
    symlink(&external, &target).expect("version symlink");

    let error = knowledge::materialize(&cache, "version").expect_err("refuse version symlink");
    assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
    assert!(std::fs::symlink_metadata(&target)
        .expect("version link remains")
        .is_symlink());
    assert_eq!(
        std::fs::read(external.join("sentinel")).expect("external sentinel remains"),
        b"preserve external content"
    );
    assert!(!external.join(".complete").exists());

    std::fs::remove_file(&target).expect("remove only the version symlink");
    let target = knowledge::materialize(&cache, "version").expect("restore the real version");
    std::fs::remove_file(target.join(".complete")).expect("replace the marker with a symlink");
    symlink(external.join("sentinel"), target.join(".complete")).expect("marker symlink");
    let error = knowledge::materialize(&cache, "version").expect_err("refuse marker symlink");
    assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
    assert_eq!(
        std::fs::read(external.join("sentinel")).expect("external sentinel remains"),
        b"preserve external content"
    );
}

#[test]
fn gc_collects_old_legacy_temps_but_keeps_recent_temps_and_external_targets() {
    let root = pohunek_test_support::tempdir().expect("private fixture root");
    let cache = root.path().join("pohunek");
    knowledge::materialize(&cache, "kept-version").expect("initial materialization");
    let knowledge_root = cache.join("knowledge");
    let environment = pohunek_test_support::env::TestEnv::new().expect("isolated process fixture");
    let mut child = environment
        .command(std::env::current_exe().expect("test executable"))
        .arg("--list")
        .stdout(std::process::Stdio::null())
        .spawn()
        .expect("short-lived owner");
    let owner_pid = child.id();
    assert!(child.wait().expect("reap the old owner").success());
    let old = knowledge_root.join(format!(".tmp-interrupted-{owner_pid}-0-0"));
    let recent = knowledge_root.join(format!(".tmp-interrupted-{owner_pid}-0-1"));
    let live = knowledge_root.join(format!(".tmp-live-{}-0-0", std::process::id()));
    for directory in [&old, &recent, &live] {
        std::fs::create_dir(directory).expect("legacy temporary directory");
        std::fs::write(directory.join("partial"), b"interrupted extraction").expect("partial file");
    }
    std::fs::File::open(&old)
        .expect("open the old temporary directory")
        .set_times(std::fs::FileTimes::new().set_modified(std::time::UNIX_EPOCH))
        .expect("make the legacy temp older than any retention grace period");
    std::fs::File::open(&live)
        .expect("open the live legacy temporary directory")
        .set_times(std::fs::FileTimes::new().set_modified(std::time::UNIX_EPOCH))
        .expect("age alone cannot override a live legacy owner");
    let external = root.path().join("external");
    std::fs::create_dir(&external).expect("external directory");
    std::fs::write(external.join("sentinel"), b"preserve external content")
        .expect("external sentinel");
    let link = knowledge_root.join(format!(".tmp-external-{owner_pid}-0-2"));
    std::os::unix::fs::symlink(&external, &link).expect("temporary directory symlink");

    knowledge::gc(&cache, "kept-version").expect("collect stale temporary directories");

    assert!(
        !old.exists(),
        "the interrupted legacy extraction is collected"
    );
    assert!(
        recent.join("partial").is_file(),
        "a recent legacy extraction is retained"
    );
    assert!(
        live.join("partial").is_file(),
        "a live legacy owner is retained even past the age bound"
    );
    assert_eq!(
        std::fs::read(external.join("sentinel")).expect("external sentinel remains"),
        b"preserve external content"
    );
    assert!(knowledge_root.join("kept-version/.complete").is_file());
}

#[test]
fn gc_collects_orphaned_owned_temps_but_never_a_locked_temp() {
    let root = pohunek_test_support::tempdir().expect("private fixture root");
    let cache = root.path().join("pohunek");
    knowledge::materialize(&cache, "kept-version").expect("initial materialization");
    let knowledge_root = cache.join("knowledge");
    let orphan = knowledge_root.join(".tmp-v2-orphan");
    let held = knowledge_root.join(".tmp-v2-held");
    for directory in [&orphan, &held] {
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(directory)
            .expect("owned temporary directory");
        std::fs::write(directory.join(".owner"), b"").expect("owner lock marker");
        std::fs::set_permissions(
            directory.join(".owner"),
            std::fs::Permissions::from_mode(0o600),
        )
        .expect("private owner marker");
        std::fs::write(directory.join("partial"), b"interrupted extraction").expect("partial file");
    }
    let owner = std::fs::File::open(&held).expect("open the active temporary directory");
    owner
        .lock()
        .expect("hold the real directory ownership lock");

    knowledge::gc(&cache, "kept-version").expect("collect abandoned owned temporary directories");

    assert!(
        !orphan.exists(),
        "an ownerless v2 extraction is collected without an age delay"
    );
    assert!(
        held.join("partial").is_file(),
        "the live extraction owner keeps its directory"
    );
    drop(owner);
    knowledge::gc(&cache, "kept-version").expect("collect after the owner releases its lock");
    assert!(
        !held.exists(),
        "the extraction is collectable after its owner releases the lock"
    );
}

#[test]
fn concurrent_materializers_publish_the_same_complete_version() {
    // Exercise simultaneous callers through the public extraction boundary.
    const CALLERS: usize = 8;
    let root = pohunek_test_support::tempdir().expect("private fixture root");
    let cache = root.path().join("pohunek");
    let barrier = std::sync::Barrier::new(CALLERS);
    std::thread::scope(|scope| {
        let callers: Vec<_> = (0..CALLERS)
            .map(|_| {
                scope.spawn(|| {
                    barrier.wait();
                    knowledge::materialize(&cache, "same-version")
                        .expect("each simultaneous materializer succeeds")
                })
            })
            .collect();
        for caller in callers {
            let target = caller.join().expect("materializer thread");
            assert_eq!(target, cache.join("knowledge/same-version"));
            assert!(target.join(".complete").is_file());
            assert_eq!(
                std::fs::read_to_string(target.join("log.md")).expect("complete log"),
                knowledge::embedded_bundle()
                    .get_text("log.md")
                    .expect("embedded log")
            );
        }
    });
}

#[cfg(feature = "test-util")]
#[test]
fn publication_synchronizes_every_file_and_directory_before_rename_and_parent_after() {
    use knowledge::Event;

    let root = pohunek_test_support::tempdir().expect("private fixture root");
    let cache = root.path().join("pohunek");
    let mut events = Vec::new();
    let target = knowledge::materialize_with_observer(&cache, "durable-version", |event, path| {
        events.push((event, path.to_owned()));
        Ok(())
    })
    .expect("durable publication through real filesystem operations");
    let position = |event| {
        events
            .iter()
            .position(|(seen, _)| *seen == event)
            .expect("event observed")
    };
    let temp_sync = position(Event::TempSync);
    let publish = position(Event::Publish);
    let parent_sync = position(Event::ParentSync);
    assert!(temp_sync < publish && publish < parent_sync);
    let temporary = &events[temp_sync].1;
    let marker_sync = events
        .iter()
        .position(|(event, path)| *event == Event::FileSync && path == &temporary.join(".complete"))
        .expect("completion marker synchronization");
    assert!(marker_sync < temp_sync);
    assert_eq!(events[publish].1, target);
    assert_eq!(events[parent_sync].1, cache.join("knowledge"));

    let mut directories = vec![target.clone()];
    while let Some(directory) = directories.pop() {
        let relative = directory
            .strip_prefix(&target)
            .expect("bundle-relative directory");
        let staged = temporary.join(relative);
        let directory_sync = events
            .iter()
            .position(|(event, path)| *event == Event::DirectorySync && path == &staged)
            .expect("every extracted directory is synchronized");
        assert!(directory_sync < temp_sync);
        for entry in std::fs::read_dir(&directory).expect("read published directory") {
            let entry = entry.expect("bundle entry");
            let kind = entry.file_type().expect("bundle entry type");
            let staged_entry = staged.join(entry.file_name());
            if kind.is_dir() {
                let child_sync = events
                    .iter()
                    .position(|(event, path)| {
                        *event == Event::DirectorySync && path == &staged_entry
                    })
                    .expect("child directory synchronization");
                assert!(
                    child_sync < directory_sync,
                    "directories synchronize bottom-up"
                );
                directories.push(entry.path());
            } else if entry.file_name() != ".owner" && entry.file_name() != ".complete" {
                let file_sync = events
                    .iter()
                    .position(|(event, path)| *event == Event::FileSync && path == &staged_entry)
                    .expect("every embedded file is synchronized");
                assert!(file_sync < directory_sync);
                assert!(file_sync < publish);
            }
        }
    }
}

#[cfg(feature = "test-util")]
#[test]
fn durability_failures_are_reported_and_a_normal_retry_finishes_the_bundle() {
    use knowledge::Event;

    for (failure, marker_only) in [
        (Event::FileSync, false),
        (Event::FileSync, true),
        (Event::DirectorySync, false),
        (Event::TempSync, false),
        (Event::ParentSync, false),
    ] {
        let root = pohunek_test_support::tempdir().expect("private fixture root");
        let cache = root.path().join("pohunek");
        let mut injected = false;
        let error = knowledge::materialize_with_observer(&cache, "retry-version", |event, path| {
            if !injected && event == failure && (!marker_only || path.ends_with(".complete")) {
                injected = true;
                return Err(std::io::Error::other("injected durability failure"));
            }
            Ok(())
        })
        .expect_err("a failed durability operation must fail materialization");
        assert!(
            injected,
            "the selected durability boundary was exercised: {failure:?}"
        );
        assert!(error.to_string().contains("injected durability failure"));
        let target =
            knowledge::materialize(&cache, "retry-version").expect("retry real publication");
        assert!(target.join(".complete").is_file());
        assert_eq!(
            std::fs::read_to_string(target.join("log.md")).expect("complete log after retry"),
            knowledge::embedded_bundle()
                .get_text("log.md")
                .expect("embedded log")
        );
        for entry in std::fs::read_dir(cache.join("knowledge")).expect("knowledge root") {
            let entry = entry.expect("knowledge entry");
            assert!(
                !entry.file_name().to_string_lossy().starts_with(".tmp-"),
                "failed attempts leave no active temporary directory"
            );
        }
    }
}

#[test]
fn reserved_materializer_names_are_rejected_without_creating_a_cache() {
    let root = pohunek_test_support::tempdir().expect("private fixture root");
    let cache = root.path().join("pohunek");
    for version in [
        ".tmp-reserved",
        ".tmp-v2-reserved",
        ".pohunek-reserved",
        ".materialize.lock",
    ] {
        assert_eq!(
            knowledge::materialize(&cache, version)
                .expect_err("reserved publication name")
                .kind(),
            std::io::ErrorKind::InvalidInput
        );
        assert_eq!(
            knowledge::gc(&cache, version)
                .expect_err("reserved keep name")
                .kind(),
            std::io::ErrorKind::InvalidInput
        );
    }
    assert!(!cache.exists());
}

#[test]
fn materialization_and_gc_refuse_symlinked_cache_roots_without_touching_external_data() {
    for knowledge_link in [false, true] {
        let root = pohunek_test_support::tempdir().expect("private fixture root");
        let cache = root.path().join("pohunek");
        let external = root.path().join("external");
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(&external)
            .expect("private external directory");
        std::fs::write(external.join("sentinel"), b"preserve external content")
            .expect("external sentinel");
        let link = if knowledge_link {
            std::fs::DirBuilder::new()
                .mode(0o700)
                .create(&cache)
                .expect("private cache root");
            cache.join("knowledge")
        } else {
            cache.clone()
        };
        std::os::unix::fs::symlink(&external, &link).expect("cache root symlink");
        knowledge::materialize(&cache, "version").expect_err("refuse a linked cache root");
        knowledge::gc(&cache, "version").expect_err("refuse linked GC authority");
        assert!(std::fs::symlink_metadata(&link)
            .expect("root link remains")
            .is_symlink());
        let entries: Vec<_> = std::fs::read_dir(&external)
            .expect("external root")
            .map(|entry| entry.expect("external entry").file_name())
            .collect();
        assert_eq!(entries, [std::ffi::OsString::from("sentinel")]);
        assert_eq!(
            std::fs::read(external.join("sentinel")).expect("external sentinel remains"),
            b"preserve external content"
        );
    }
}

#[test]
fn materialization_refuses_nonprivate_roots_and_writable_version_directories() {
    for unsafe_component in ["cache", "knowledge", "version"] {
        let root = pohunek_test_support::tempdir().expect("private fixture root");
        let cache = root.path().join("pohunek");
        let target =
            knowledge::materialize(&cache, "version").expect("initial private publication");
        let unsafe_path = match unsafe_component {
            "cache" => cache.clone(),
            "knowledge" => cache.join("knowledge"),
            "version" => target.clone(),
            _ => unreachable!("fixture component is known"),
        };
        // Legacy versions can be readable, but no version may be writable by
        // another account. Cache authorities themselves must remain 0700.
        let unsafe_mode = if unsafe_component == "version" {
            0o770
        } else {
            0o755
        };
        std::fs::set_permissions(&unsafe_path, std::fs::Permissions::from_mode(unsafe_mode))
            .expect("make the selected authority unsafe");
        knowledge::materialize(&cache, "version").expect_err("refuse unsafe authority permissions");
        if unsafe_component != "version" {
            knowledge::gc(&cache, "version").expect_err("refuse unsafe GC authority permissions");
        }
        assert_eq!(
            std::fs::metadata(&unsafe_path)
                .expect("unsafe directory remains")
                .permissions()
                .mode()
                & 0o777,
            unsafe_mode
        );
        assert!(target.join(".complete").is_file());
    }
}
