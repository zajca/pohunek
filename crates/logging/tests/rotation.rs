use std::fs;
use std::io::Write;
use std::os::unix::fs::{symlink, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use pohunek_logging::{remove_family, Error, Files, Legacy, Policy, Writer};

static NEXT_DIR: AtomicU64 = AtomicU64::new(0);

#[derive(Debug)]
struct TempDir(PathBuf);

impl TempDir {
    fn new(tag: &str) -> Self {
        let sequence = NEXT_DIR.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "pohunek-logging-{tag}-{}-{sequence}",
            std::process::id()
        ));
        fs::create_dir(&path).expect("create isolated test directory");
        Self(path)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.0).expect("remove isolated test directory");
    }
}

fn files(active: &str) -> Files {
    Files::new(active, Legacy::None).expect("valid test filename")
}

fn family_files(dir: &Path, active: &str) -> Vec<(String, u64)> {
    let mut entries = fs::read_dir(dir)
        .expect("read test directory")
        .map(|entry| entry.expect("read test entry"))
        .filter_map(|entry| {
            let kind = entry.file_type().expect("read test file type");
            let name = entry.file_name().to_string_lossy().into_owned();
            (kind.is_file()
                && (name == active || name.starts_with(&format!("{active}.")))
                && name != format!("{active}.lock"))
            .then(|| {
                let size = entry.metadata().expect("read test metadata").len();
                (name, size)
            })
        })
        .collect::<Vec<_>>();
    entries.sort();
    entries
}

#[test]
fn writes_to_owner_private_files() {
    let temp = TempDir::new("happy");
    let mut writer = Writer::open(
        temp.path(),
        files("service.jsonl"),
        Policy::new(64, 2).unwrap(),
    )
    .unwrap();

    writer.write_all(b"{\"ok\":true}\n").unwrap();

    assert_eq!(
        fs::read(temp.path().join("service.jsonl")).unwrap(),
        b"{\"ok\":true}\n"
    );
    assert_eq!(
        fs::metadata(temp.path()).unwrap().permissions().mode() & 0o777,
        0o700
    );
    assert_eq!(
        fs::metadata(temp.path().join("service.jsonl"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
}

#[test]
fn rotates_before_the_limit_and_retains_only_the_configured_count() {
    let temp = TempDir::new("rotate");
    let policy = Policy::new(10, 3).unwrap();
    let mut writer = Writer::open(temp.path(), files("service.jsonl"), policy).unwrap();

    for _ in 0..5 {
        writer.write_all(b"12345\n").unwrap();
    }

    let entries = family_files(temp.path(), "service.jsonl");
    assert_eq!(
        entries,
        vec![
            ("service.jsonl".to_owned(), 6),
            ("service.jsonl.1".to_owned(), 6),
            ("service.jsonl.2".to_owned(), 6),
        ]
    );
    assert!(entries.iter().map(|(_, size)| size).sum::<u64>() <= 30);
}

#[test]
fn startup_prunes_owned_legacy_oversized_and_excess_files_only() {
    let temp = TempDir::new("restart");
    fs::write(temp.path().join("service.jsonl"), vec![b'a'; 12]).unwrap();
    fs::write(temp.path().join("service.jsonl.1"), vec![b'b'; 11]).unwrap();
    fs::write(temp.path().join("service.jsonl.2"), b"kept").unwrap();
    fs::write(temp.path().join("service.jsonl.3"), b"excess").unwrap();
    fs::write(temp.path().join("service.daily.2026-07-27"), b"legacy").unwrap();
    fs::write(temp.path().join("unrelated.txt"), b"preserve").unwrap();
    fs::write(temp.path().join("outside.txt"), b"outside").unwrap();
    symlink(
        temp.path().join("outside.txt"),
        temp.path().join("service.daily.symlink"),
    )
    .unwrap();

    let owned = Files::new("service.jsonl", Legacy::prefix("service.daily.")).unwrap();
    let _writer = Writer::open(temp.path(), owned, Policy::new(10, 3).unwrap()).unwrap();

    assert_eq!(
        family_files(temp.path(), "service.jsonl"),
        vec![("service.jsonl.2".to_owned(), 4)]
    );
    assert_eq!(
        fs::read(temp.path().join("unrelated.txt")).unwrap(),
        b"preserve"
    );
    assert_eq!(
        fs::read(temp.path().join("outside.txt")).unwrap(),
        b"outside"
    );
    assert!(
        fs::symlink_metadata(temp.path().join("service.daily.symlink"))
            .unwrap()
            .file_type()
            .is_symlink()
    );
}

#[test]
fn active_symlink_is_rejected_without_touching_its_target() {
    let temp = TempDir::new("active-symlink");
    let target = temp.path().join("target.txt");
    fs::write(&target, b"target").unwrap();
    symlink(&target, temp.path().join("service.jsonl")).unwrap();

    let error = Writer::open(
        temp.path(),
        files("service.jsonl"),
        Policy::new(32, 2).unwrap(),
    )
    .unwrap_err();

    assert!(matches!(error, Error::UnsafePath { .. }));
    assert_eq!(fs::read(target).unwrap(), b"target");
}

#[test]
fn zero_policy_limits_are_rejected() {
    assert!(matches!(
        Policy::new(0, 1),
        Err(Error::InvalidPolicy { .. })
    ));
    assert!(matches!(
        Policy::new(1, 0),
        Err(Error::InvalidPolicy { .. })
    ));
}

#[test]
fn one_oversized_event_is_replaced_atomically() {
    let temp = TempDir::new("oversize");
    let mut writer = Writer::open(
        temp.path(),
        files("service.jsonl"),
        Policy::new(128, 2).unwrap(),
    )
    .unwrap();
    let oversized = vec![b'x'; 129];

    assert_eq!(writer.write(&oversized).unwrap(), oversized.len());

    let content = fs::read(temp.path().join("service.jsonl")).unwrap();
    assert!(content.len() <= 128);
    assert!(content.ends_with(b"\n"));
    assert_ne!(content, oversized);
    let content = String::from_utf8(content).unwrap();
    assert_eq!(content.lines().count(), 1);
    assert!(content.contains("log event dropped"));
}

#[test]
fn multiple_writers_share_one_family_bound_and_preserve_event_boundaries() {
    let temp = TempDir::new("multi-writer");
    let policy = Policy::new(24, 3).unwrap();
    let owned = files("session.jsonl");
    let mut first = Writer::open(temp.path(), owned.clone(), policy).unwrap();
    let mut second = Writer::open(temp.path(), owned, policy).unwrap();

    for sequence in 0..12 {
        let event = format!("{{\"event\":{sequence:02}}}\n");
        if sequence % 2 == 0 {
            first.write_all(event.as_bytes()).unwrap();
        } else {
            second.write_all(event.as_bytes()).unwrap();
        }
    }

    let entries = family_files(temp.path(), "session.jsonl");
    assert!(entries.len() <= policy.max_files());
    assert!(
        entries.iter().map(|(_, size)| size).sum::<u64>()
            <= policy.max_file_bytes() * u64::try_from(policy.max_files()).unwrap()
    );
    for (name, size) in entries {
        assert!(size <= policy.max_file_bytes(), "{name} exceeded its cap");
        let content = fs::read(temp.path().join(name)).unwrap();
        assert!(content.ends_with(b"\n"));
        for line in content
            .split(|byte| *byte == b'\n')
            .filter(|line| !line.is_empty())
        {
            let text = std::str::from_utf8(line).unwrap();
            assert!(text.starts_with("{\"event\":"));
            assert!(text.ends_with('}'));
        }
    }
}

#[test]
fn an_oversized_event_is_dropped_when_even_the_notice_cannot_fit() {
    let temp = TempDir::new("tiny");
    let mut writer = Writer::open(
        temp.path(),
        files("service.jsonl"),
        Policy::new(4, 1).unwrap(),
    )
    .unwrap();
    let oversized = b"{\"oversized\":true}\n";

    assert_eq!(writer.write(oversized).unwrap(), oversized.len());
    assert!(!temp.path().join("service.jsonl").exists());
}

#[test]
fn remove_family_preserves_unrelated_files_and_symlinks() {
    let temp = TempDir::new("remove");
    let owned = files("service.jsonl");
    fs::write(temp.path().join("service.jsonl"), b"active").unwrap();
    fs::write(temp.path().join("service.jsonl.1"), b"rotated").unwrap();
    fs::write(temp.path().join("notes.txt"), b"notes").unwrap();
    symlink(
        temp.path().join("notes.txt"),
        temp.path().join("service.jsonl.2"),
    )
    .unwrap();

    remove_family(temp.path(), &owned).unwrap();

    assert!(!temp.path().join("service.jsonl").exists());
    assert!(!temp.path().join("service.jsonl.1").exists());
    assert_eq!(fs::read(temp.path().join("notes.txt")).unwrap(), b"notes");
    assert!(fs::symlink_metadata(temp.path().join("service.jsonl.2"))
        .unwrap()
        .file_type()
        .is_symlink());
}
