use std::path::{Path, PathBuf};

use knowledge::{gc, materialize};

fn temp_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "pohunek-knowledge-{name}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock is after unix epoch")
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).expect("create temp dir");
    dir
}

fn touch_dir(path: impl AsRef<Path>) {
    std::fs::create_dir_all(path).expect("create directory");
}

#[test]
fn materialize_writes_embedded_bundle_to_versioned_cache_dir() {
    let cache_dir = temp_dir("materialize-writes");
    let version_hash = "sha256:test-materialize-writes";

    let materialized = materialize(&cache_dir, version_hash).expect("materialize bundle");

    assert_eq!(materialized, cache_dir.join("knowledge").join(version_hash));
    assert!(materialized.join(".complete").is_file());
    assert!(materialized.join("index.md").is_file());
    assert!(materialized
        .join("concepts")
        .join("architecture.md")
        .is_file());
}

#[test]
fn materialize_is_idempotent_when_complete_marker_exists() {
    let cache_dir = temp_dir("materialize-idempotent");
    let version_hash = "sha256:test-materialize-idempotent";
    let materialized = materialize(&cache_dir, version_hash).expect("materialize bundle");
    let sentinel = materialized.join("sentinel.txt");
    std::fs::write(&sentinel, "preserved").expect("write sentinel");

    let second = materialize(&cache_dir, version_hash).expect("materialize bundle again");

    assert_eq!(second, materialized);
    assert_eq!(
        std::fs::read_to_string(sentinel).expect("read sentinel"),
        "preserved"
    );
}

#[test]
fn materialize_concurrent_same_version_preserves_complete_bundle() {
    let cache_dir = temp_dir("materialize-concurrent");
    let version_hash = "sha256:test-materialize-concurrent";

    let handles = (0..8)
        .map(|_| {
            let cache_dir = cache_dir.clone();
            std::thread::spawn(move || materialize(&cache_dir, version_hash))
        })
        .collect::<Vec<_>>();

    let results = handles
        .into_iter()
        .map(|handle| handle.join().expect("thread should not panic"))
        .collect::<Result<Vec<_>, _>>()
        .expect("all materializers should succeed");

    let expected = cache_dir.join("knowledge").join(version_hash);
    assert!(results.iter().all(|path| path == &expected));
    assert!(expected.join(".complete").is_file());
    assert!(expected.join("index.md").is_file());
}

#[test]
fn materialize_refuses_nonempty_incomplete_version_dir_without_deleting_it() {
    let cache_dir = temp_dir("materialize-incomplete");
    let version_hash = "sha256:test-materialize-incomplete";
    let target = cache_dir.join("knowledge").join(version_hash);
    touch_dir(&target);
    std::fs::write(target.join("stale.txt"), "stale").expect("write stale file");

    let error = materialize(&cache_dir, version_hash).expect_err("incomplete target should fail");

    assert!(
        matches!(
            error.kind(),
            std::io::ErrorKind::AlreadyExists
                | std::io::ErrorKind::DirectoryNotEmpty
                | std::io::ErrorKind::Other
        ),
        "unexpected error: {error:?}"
    );
    assert_eq!(
        std::fs::read_to_string(target.join("stale.txt")).expect("stale file remains"),
        "stale"
    );
    assert!(!target.join(".complete").exists());
}

#[cfg(unix)]
#[test]
fn materialize_refuses_symlinked_version_dir_even_when_complete() {
    let cache_dir = temp_dir("materialize-symlink-target");
    let version_hash = "sha256:test-materialize-symlink-target";
    let target = cache_dir.join("knowledge").join(version_hash);
    let outside = cache_dir.join("outside-complete");
    touch_dir(&outside);
    std::fs::write(outside.join(".complete"), "complete").expect("write marker");
    touch_dir(cache_dir.join("knowledge"));
    std::os::unix::fs::symlink(&outside, &target).expect("create target symlink");

    let error = materialize(&cache_dir, version_hash).expect_err("symlink target should fail");

    assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
}

#[cfg(unix)]
#[test]
fn materialize_refuses_symlinked_complete_marker() {
    let cache_dir = temp_dir("materialize-symlink-marker");
    let version_hash = "sha256:test-materialize-symlink-marker";
    let target = cache_dir.join("knowledge").join(version_hash);
    touch_dir(&target);
    std::fs::write(cache_dir.join("outside-marker"), "complete").expect("write outside marker");
    std::os::unix::fs::symlink(cache_dir.join("outside-marker"), target.join(".complete"))
        .expect("create marker symlink");

    let error = materialize(&cache_dir, version_hash).expect_err("symlink marker should fail");

    assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
}

#[test]
fn gc_removes_stale_version_dirs_and_keeps_current_only_under_knowledge_cache() {
    let cache_dir = temp_dir("materialize-gc");
    let keep = "sha256:keep";
    let stale = "sha256:stale";
    let another_stale = "sha256:another-stale";
    let knowledge_dir = cache_dir.join("knowledge");
    touch_dir(knowledge_dir.join(keep));
    touch_dir(knowledge_dir.join(stale));
    touch_dir(knowledge_dir.join(another_stale));
    touch_dir(cache_dir.join("outside-knowledge"));

    gc(&cache_dir, keep).expect("garbage collect stale versions");

    assert!(knowledge_dir.join(keep).is_dir());
    assert!(!knowledge_dir.join(stale).exists());
    assert!(!knowledge_dir.join(another_stale).exists());
    assert!(cache_dir.join("outside-knowledge").is_dir());
}

#[test]
fn materialize_prunes_stale_version_dirs() {
    let cache_dir = temp_dir("materialize-prunes-stale");
    let version_hash = "sha256:test-materialize-prunes-stale";
    let knowledge_dir = cache_dir.join("knowledge");
    let stale = knowledge_dir.join("sha256:old-version");
    touch_dir(&stale);

    let materialized = materialize(&cache_dir, version_hash).expect("materialize bundle");

    assert!(materialized.join(".complete").is_file());
    assert!(
        !stale.exists(),
        "stale version dir should be pruned by materialize"
    );
}

#[test]
fn gc_skips_in_progress_temp_dirs() {
    let cache_dir = temp_dir("materialize-gc-temp");
    let keep = "sha256:keep";
    let stale = "sha256:stale";
    let knowledge_dir = cache_dir.join("knowledge");
    let temp = knowledge_dir.join(".tmp-sha256:next-123-456");
    touch_dir(knowledge_dir.join(keep));
    touch_dir(knowledge_dir.join(stale));
    touch_dir(&temp);

    gc(&cache_dir, keep).expect("garbage collect stale versions");

    assert!(knowledge_dir.join(keep).is_dir());
    assert!(!knowledge_dir.join(stale).exists());
    assert!(temp.is_dir());
}
