//! Layered, fail-closed resolution of per-project config (Part A).
//!
//! The daemon resolves a project's prompts (and, from slice A2, templates and
//! actions) from two layers, most-specific first: the in-repo
//! `<repo_root>/.pohunek/` directory that travels with the repo, then the
//! host-default `<config_dir>/` directory on the daemon's host. Resolution is
//! fail-closed: a requested-but-missing prompt is a hard `prompt_not_found`, never
//! a silent fallback to a built-in.
//!
//! The names come from an untrusted source — both the wire/CLI `<name>` and the
//! in-repo `prompt = "<name>"` field — and the daemon joins a name into a host path
//! and returns the file's bytes over the wire. Two guards therefore run **before
//! any filesystem read**:
//!
//! 1. **A.2.1.1 single-segment charset guard** ([`validate_name`]): a name must
//!    match `^[A-Za-z0-9._-]+$`, be non-empty, and not begin with `.` or `-`. This
//!    blocks `prompt = "../../../../etc/passwd"`.
//! 2. **A.2.1.2 canonicalize-and-contain guard** ([`read_contained`]): git checks
//!    out symlinks, so a charset-clean file can still be a symlink to `/etc/shadow`.
//!    Every file the daemon reads is canonicalized and must stay within the
//!    canonicalized layer root, failing closed otherwise.
//!
//! See `docs/design/per-project-actions-and-worktree-hooks.md` (A.2, A.2.1).

use std::path::{Path, PathBuf};

use protocol::{ErrorClass, ProjectPromptResult, PromptLayer, ProtocolError};

/// The per-project config subdirectory under a repo root.
const POHUNEK_DIR: &str = ".pohunek";
/// The prompts subdirectory (under `.pohunek/` in-repo, or directly under the host
/// config dir).
const PROMPTS_DIR: &str = "prompts";
/// File extension for prompt templates.
const PROMPT_EXT: &str = "tmpl";

/// Resolves per-project config from the in-repo and host-default layers, applying
/// the name + containment guards on every read.
///
/// Constructed per request from the resolved [`crate::store::ProjectRecord`]'s
/// `repo_root` plus the daemon's `config_dir` (read via
/// [`crate::session::SessionRegistry::config_dir`]). It is **not** part of
/// `ProjectManager` (which stays store glue): a value, not a dependency.
#[derive(Debug, Clone)]
pub(crate) struct ProjectConfigResolver {
    /// The project's main checkout; the in-repo layer is `<repo_root>/.pohunek/`.
    repo_root: PathBuf,
    /// The host config dir (`~/.config/pohunek`), or `None` when the host-default
    /// layer is disabled (e.g. a daemon configured without a config dir); the host
    /// layer is then skipped and resolution falls through to the typed not-found.
    config_dir: Option<PathBuf>,
}

impl ProjectConfigResolver {
    /// Build a resolver for one project against the daemon's host config dir.
    pub(crate) fn new(repo_root: PathBuf, config_dir: Option<PathBuf>) -> Self {
        Self {
            repo_root,
            config_dir,
        }
    }

    /// The in-repo config base: `<repo_root>/.pohunek/`.
    fn repo_base(&self) -> PathBuf {
        self.repo_root.join(POHUNEK_DIR)
    }

    /// Resolve a prompt by name, **first-existing-file wins, fail-closed**.
    ///
    /// Order: in-repo `<repo_root>/.pohunek/prompts/<name>.tmpl` → host
    /// `<config_dir>/prompts/<name>.tmpl` → [`prompt_not_found`]. Whole files, never
    /// merged. The charset guard runs before any join; the containment guard runs on
    /// every read.
    pub(crate) fn resolve_prompt(&self, name: &str) -> Result<ProjectPromptResult, ProtocolError> {
        validate_name("prompt", name)?;
        let rel = Path::new(PROMPTS_DIR).join(format!("{name}.{PROMPT_EXT}"));

        // In-repo layer wins.
        let repo_base = self.repo_base();
        if let Some(content) = read_contained(&repo_base, &repo_base.join(&rel))? {
            return Ok(ProjectPromptResult {
                name: name.to_owned(),
                content,
                layer: PromptLayer::InRepo,
            });
        }
        // Host-default layer (skipped when disabled).
        if let Some(config_dir) = &self.config_dir {
            if let Some(content) = read_contained(config_dir, &config_dir.join(&rel))? {
                return Ok(ProjectPromptResult {
                    name: name.to_owned(),
                    content,
                    layer: PromptLayer::Host,
                });
            }
        }
        Err(prompt_not_found(name))
    }
}

/// A.2.1.1 single-segment charset guard. A prompt/action/template/agent/manifest
/// name must match `^[A-Za-z0-9._-]+$`, be non-empty, and not begin with `.` or
/// `-`; any `/`, `\`, `..`, or control character is rejected with the neutral
/// [`invalid_name`] code (one code for every name kind; the message says which).
///
/// Stricter than `validate_git_ref_arg` (`worktree/mod.rs`), which permits `/` and
/// uses `invalid_branch` — do not reuse that guard. Run before any path join/read,
/// on both the wire `<name>` and any in-repo `prompt=`/`template=` value.
pub(crate) fn validate_name(kind: &str, name: &str) -> Result<(), ProtocolError> {
    let first = name.chars().next();
    let charset_ok = name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'));
    // Non-empty, no leading `.`/`-`, charset-clean. A leading-`.` reject also covers
    // `.` and `..`; `/` and `\` are not in the charset, so a path separator or
    // traversal segment can never pass.
    if first.is_some() && first != Some('.') && first != Some('-') && charset_ok {
        Ok(())
    } else {
        Err(invalid_name(kind, name))
    }
}

/// A.2.1.2 canonicalize-and-contain guard + read.
///
/// Canonicalizes `path` (resolving symlinks; requires existence) and requires it to
/// stay within the canonicalized `base`. Returns `Ok(None)` when the file does not
/// exist (so the caller falls through to the next layer), `Ok(Some(content))` when
/// it exists and is contained, and `Err` when a symlink escapes `base` (or another
/// IO/UTF-8 error). `base` is explicit so the **same** function guards the in-repo
/// base (`<repo_root>/.pohunek/`) and the host base (`<config_dir>/`).
///
/// Uses real [`std::fs::canonicalize`] + [`Path::starts_with`], never the
/// best-effort `canonical_or_original` (`worktree/mod.rs`) — containment must fail
/// closed.
pub(crate) fn read_contained(base: &Path, path: &Path) -> Result<Option<String>, ProtocolError> {
    let canonical = match std::fs::canonicalize(path) {
        Ok(canonical) => canonical,
        // A non-existent file is a not-found at the resolution layer, not a guard
        // failure — the caller falls through to the next layer / typed not-found.
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(config_read_failed(path, &err)),
    };
    // `path` exists, so `base` (its ancestor) must too; canonicalize it to compare
    // resolved-to-resolved (a symlinked base is itself resolved).
    let canonical_base =
        std::fs::canonicalize(base).map_err(|err| config_read_failed(base, &err))?;
    if !canonical.starts_with(&canonical_base) {
        return Err(path_escape(base));
    }
    let content =
        std::fs::read_to_string(&canonical).map_err(|err| config_read_failed(path, &err))?;
    Ok(Some(content))
}

/// `runtime/prompt_not_found`: no prompt of that name in either layer (fail-closed).
fn prompt_not_found(name: &str) -> ProtocolError {
    ProtocolError::new(
        ErrorClass::Runtime,
        "prompt_not_found",
        format!("no prompt named '{name}' in the project's .pohunek/prompts or the host config"),
        Some("add prompts/<name>.tmpl under the repo's .pohunek/ or ~/.config/pohunek/".to_owned()),
    )
}

/// `runtime/invalid_name`: the shared neutral bad-name code (A.2.1.1). The `kind`
/// names which sort of name failed (prompt/action/template/agent/manifest).
pub(crate) fn invalid_name(kind: &str, name: &str) -> ProtocolError {
    ProtocolError::new(
        ErrorClass::Runtime,
        "invalid_name",
        format!(
            "invalid {kind} name {name:?}: must be a single segment matching [A-Za-z0-9._-] and not start with '.' or '-'"
        ),
        None,
    )
}

/// `runtime/path_escape`: a charset-clean name resolved (via a symlink) outside its
/// layer root. Names the base only — never the escape target — so the error cannot
/// disclose where the link pointed.
fn path_escape(base: &Path) -> ProtocolError {
    ProtocolError::new(
        ErrorClass::Runtime,
        "path_escape",
        format!(
            "a config file resolves outside {} (symlink escape rejected)",
            base.display()
        ),
        None,
    )
}

/// `runtime/config_read_failed`: an IO/UTF-8 error reading a config file that exists
/// and is contained.
fn config_read_failed(path: &Path, err: &std::io::Error) -> ProtocolError {
    ProtocolError::new(
        ErrorClass::Runtime,
        "config_read_failed",
        format!("failed to read {}: {err}", path.display()),
        None,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    /// A unique empty temp dir for a test.
    fn tmp(tag: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time after epoch")
            .as_nanos();
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "pohunek-cfg-{tag}-{}-{nanos}-{n}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        dir
    }

    fn write(path: &Path, content: &str) {
        std::fs::create_dir_all(path.parent().expect("has parent")).expect("mkdir -p");
        std::fs::write(path, content).expect("write file");
    }

    /// Build a resolver with a fresh repo root and host config dir, returning both
    /// roots so a test can seed files under either layer.
    fn resolver(tag: &str) -> (ProjectConfigResolver, PathBuf, PathBuf) {
        let repo_root = tmp(&format!("{tag}-repo"));
        let config_dir = tmp(&format!("{tag}-cfg"));
        let r = ProjectConfigResolver::new(repo_root.clone(), Some(config_dir.clone()));
        (r, repo_root, config_dir)
    }

    #[test]
    fn in_repo_prompt_wins_over_host() {
        let (r, repo, cfg) = resolver("wins");
        write(&repo.join(".pohunek/prompts/issue.tmpl"), "REPO ${title}");
        write(&cfg.join("prompts/issue.tmpl"), "HOST ${title}");
        let got = r.resolve_prompt("issue").expect("resolves");
        assert_eq!(got.content, "REPO ${title}");
        assert_eq!(got.layer, PromptLayer::InRepo);
        assert_eq!(got.name, "issue");
    }

    #[test]
    fn host_prompt_used_when_no_in_repo() {
        let (r, _repo, cfg) = resolver("host");
        write(&cfg.join("prompts/issue.tmpl"), "HOST ${title}");
        let got = r.resolve_prompt("issue").expect("resolves");
        assert_eq!(got.content, "HOST ${title}");
        assert_eq!(got.layer, PromptLayer::Host);
    }

    #[test]
    fn missing_prompt_is_prompt_not_found() {
        let (r, _repo, _cfg) = resolver("missing");
        let err = r.resolve_prompt("nope").expect_err("no such prompt");
        assert_eq!(err.code, "prompt_not_found");
    }

    #[test]
    fn host_layer_disabled_falls_through_to_not_found() {
        let repo = tmp("no-host-repo");
        let r = ProjectConfigResolver::new(repo, None);
        let err = r.resolve_prompt("issue").expect_err("no in-repo, no host");
        assert_eq!(err.code, "prompt_not_found");
    }

    #[test]
    fn bad_names_each_reject_with_invalid_name() {
        let (r, _repo, _cfg) = resolver("charset2");
        for bad in [
            "../../../../etc/passwd",
            "a/b",
            "a\\b",
            "-leading",
            ".hidden",
            "..",
            "",
            "a\u{7}b",
        ] {
            let err = r.resolve_prompt(bad).expect_err("must reject");
            assert_eq!(err.code, "invalid_name", "name {bad:?}");
        }
    }

    #[test]
    fn valid_dotted_name_is_accepted() {
        // A `.`/`-` inside the segment is fine; only a leading one is rejected.
        let (r, repo, _cfg) = resolver("dotted");
        write(&repo.join(".pohunek/prompts/issue.v2-final.tmpl"), "OK");
        let got = r.resolve_prompt("issue.v2-final").expect("resolves");
        assert_eq!(got.content, "OK");
    }

    #[cfg(unix)]
    #[test]
    fn symlink_escape_rejected_repo_layer() {
        use std::os::unix::fs::symlink;
        let (r, repo, _cfg) = resolver("escape-repo");
        let secret = tmp("escape-repo-secret").join("secret.txt");
        write(&secret, "SECRET");
        let link = repo.join(".pohunek/prompts/evil.tmpl");
        std::fs::create_dir_all(link.parent().unwrap()).unwrap();
        symlink(&secret, &link).expect("symlink");
        let err = r.resolve_prompt("evil").expect_err("escape rejected");
        assert_eq!(err.code, "path_escape");
        // The error must not leak the escape target path.
        assert!(!err.msg.contains("SECRET"));
        assert!(!err.msg.contains("secret.txt"));
    }

    #[cfg(unix)]
    #[test]
    fn symlink_escape_rejected_host_layer() {
        use std::os::unix::fs::symlink;
        let (r, _repo, cfg) = resolver("escape-host");
        let secret = tmp("escape-host-secret").join("secret.txt");
        write(&secret, "SECRET");
        let link = cfg.join("prompts/evil.tmpl");
        std::fs::create_dir_all(link.parent().unwrap()).unwrap();
        symlink(&secret, &link).expect("symlink");
        let err = r.resolve_prompt("evil").expect_err("escape rejected");
        assert_eq!(err.code, "path_escape");
    }

    // The containment guard is shared with slice A2's TOML reads; pin it directly on
    // the `templates.toml`/`actions.toml` paths so A2 can rely on it.
    #[test]
    fn read_contained_returns_none_for_missing_toml() {
        let base = tmp("toml-missing").join(".pohunek");
        std::fs::create_dir_all(&base).unwrap();
        assert!(read_contained(&base, &base.join("templates.toml"))
            .expect("ok")
            .is_none());
        assert!(read_contained(&base, &base.join("actions.toml"))
            .expect("ok")
            .is_none());
    }

    #[test]
    fn read_contained_reads_a_contained_toml() {
        let base = tmp("toml-ok").join(".pohunek");
        write(&base.join("templates.toml"), "[template.x]\n");
        let got = read_contained(&base, &base.join("templates.toml")).expect("ok");
        assert_eq!(got.as_deref(), Some("[template.x]\n"));
    }

    #[cfg(unix)]
    #[test]
    fn read_contained_rejects_symlinked_toml_escape() {
        use std::os::unix::fs::symlink;
        let base = tmp("toml-escape").join(".pohunek");
        std::fs::create_dir_all(&base).unwrap();
        let outside = tmp("toml-escape-outside").join("actions.toml");
        write(&outside, "[action.x]\n");
        for name in ["templates.toml", "actions.toml"] {
            let link = base.join(name);
            symlink(&outside, &link).expect("symlink");
            let err = read_contained(&base, &link).expect_err("escape rejected");
            assert_eq!(err.code, "path_escape");
            std::fs::remove_file(&link).unwrap();
        }
    }
}
