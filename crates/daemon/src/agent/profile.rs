//! Per-host agent profiles (Part C).
//!
//! A **profile** is a host-authored `~/.config/pohunek/agents/<name>.toml` that
//! *extends* a compiled **base kind** (`shell`/`codex`/`claude`) with overrides for
//! the launch program/args, the PTY env, and the input rules (resume + manifest
//! overrides land in C2). The wire/in-repo `agent` is a **name**: it resolves —
//! charset-guarded, fail-closed — to a host profile or a bare base kind, never to a
//! program. `program`/`args`/`env` come ONLY from a host profile or a base kind,
//! never from the wire or a repo (the A.5 boundary).
//!
//! Resolution ([`ProfileRegistry::resolve_agent`]) is the 4-step chain: A.2.1
//! charset guard → profile file → bare base kind → `agent_profile_not_found`.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use protocol::{AgentKind, ErrorClass, ProtocolError};
use serde::Deserialize;
use tracing::warn;

use super::{base_resume_template, InputRules, ResumeMode, ResumeTemplate, SessionRefKind};
use crate::detect::Manifest;
use crate::project::config::validate_name;

/// A parsed `agents/<name>.toml`. `deny_unknown_fields` keeps the surface tight so
/// a typo is a loud error rather than a silently-ignored key.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawProfile {
    /// Base kind to extend: `shell` | `codex` | `claude`.
    base: String,
    /// Launch program (PATH name or absolute path); defaults to the base program.
    #[serde(default)]
    program: Option<String>,
    /// Launch args appended to the program.
    #[serde(default)]
    args: Option<Vec<String>>,
    /// Extra PTY env (every `POHUNEK_`-prefixed key is stripped on load — reserved).
    #[serde(default)]
    env: HashMap<String, String>,
    /// Input-framing override; absent ⇒ the base kind's defaults.
    #[serde(default)]
    input_rules: Option<RawInputRules>,
    /// Resume override (mode/ref_kind/resumable); absent ⇒ inherit the base kind's
    /// resume template (or non-resumable for a shell).
    #[serde(default)]
    resume: Option<RawResume>,
    /// Detection-manifest override name, resolved from `agents/manifests/<name>.toml`
    /// under the same charset + containment guard as a profile file.
    #[serde(default)]
    manifest: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawInputRules {
    #[serde(default)]
    bracketed_paste: Option<bool>,
    #[serde(default)]
    submit_delay_ms: Option<u64>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawResume {
    /// `flag` (`--resume <ref>`) | `subcommand` (`resume <ref>`); absent ⇒ the base
    /// kind's mode.
    #[serde(default)]
    mode: Option<String>,
    /// `id` | `path`; absent ⇒ the base kind's ref kind.
    #[serde(default)]
    ref_kind: Option<String>,
    /// Whether this profile resumes at all; absent ⇒ inherit the base kind (a shell
    /// never resumes, claude/codex do).
    #[serde(default)]
    resumable: Option<bool>,
}

/// Host-profile launch overrides. Present on a [`ResolvedAgent`] only when a
/// profile file backed the name; a bare base kind carries `None` and launches
/// exactly as the compiled base adapter.
///
/// Not `PartialEq`/`Eq`: `manifest` holds a compiled `regex::Regex`, which has no
/// meaningful structural equality. Tests assert on individual fields instead.
#[derive(Debug, Clone)]
pub(crate) struct ResolvedProfile {
    /// Launch program (resolved on PATH at launch, like a base kind's program).
    pub program: String,
    /// Launch args.
    pub args: Vec<String>,
    /// Non-secret PTY env, with every `POHUNEK_`-prefixed key already stripped.
    pub env: Vec<(String, String)>,
    /// Input-rules override; `None` ⇒ inherit the base kind's rules.
    pub input_rules: Option<InputRules>,
    /// Resolved resume template; `Some` ⇒ resumable with this argv mode + ref kind,
    /// `None` ⇒ not resumable. Authoritative for a profile (does NOT fall back to
    /// the base kind when `None`).
    pub resume: Option<ResumeTemplate>,
    /// Parsed detection-manifest override; `None` ⇒ inherit the base kind's manifest.
    pub manifest: Option<Manifest>,
}

/// The resolution of an agent NAME on this host: its base kind plus optional
/// host-profile overrides. Not `PartialEq`/`Eq` (see [`ResolvedProfile`]).
#[derive(Debug, Clone)]
pub(crate) struct ResolvedAgent {
    /// The resolved agent name (a profile name, or a bare base-kind name).
    pub name: String,
    /// The base kind this resolves to (drives detection/resume/handshake env).
    pub base: AgentKind,
    /// Host-profile overrides; `None` for a bare base kind.
    pub profile: Option<ResolvedProfile>,
}

/// Loads + resolves host agent profiles from `<config_dir>/agents`.
///
/// Resolution is on demand (per `session.new`), but the **owner-only security gate**
/// (C.5) runs once at construction: the whole `agents/` tree (the dir itself **and**
/// `agents/manifests/`) must be owned by the daemon user and not group/world-writable,
/// or the entire host-profile layer is disabled fail-closed (a stored `None` dir).
/// Each resolved profile/manifest file is additionally canonicalize-and-contain
/// checked at load to defeat a symlink that escapes the tree.
#[derive(Debug, Clone, Default)]
pub(crate) struct ProfileRegistry {
    /// The `agents/` directory, or `None` when the host-config layer is disabled
    /// (unconfigured, absent, or failing the owner-only gate).
    dir: Option<PathBuf>,
}

impl ProfileRegistry {
    pub(crate) fn new(dir: Option<PathBuf>) -> Self {
        // C.5: gate the whole tree once at boot. An insecure `agents/` OR
        // `agents/manifests/` disables every host profile (fail-closed) — a
        // world-writable manifests dir means no manifest can be trusted, so no
        // profile may load. An absent dir is not "insecure" (there are simply no
        // profiles); the gate only rejects a present-but-unsafe dir.
        let dir = dir.filter(|dir| {
            let manifests = dir.join("manifests");
            let secure = dir_is_owner_secure(dir) && dir_is_owner_secure(&manifests);
            if !secure {
                warn!(
                    dir = %dir.display(),
                    "agent profiles directory is not owner-secure (wrong owner or group/world-writable); ignoring all host agent profiles"
                );
            }
            secure
        });
        Self { dir }
    }

    /// Resolve an agent name (4-step chain, fail-closed):
    /// 1. A.2.1 single-segment charset guard (`invalid_name`).
    /// 2. `<dir>/<name>.toml` exists → that profile.
    /// 3. `name ∈ {shell,codex,claude}` → the bare base kind.
    /// 4. else → `agent_profile_not_found` (no silent fallback).
    pub(crate) fn resolve_agent(&self, name: &str) -> Result<ResolvedAgent, ProtocolError> {
        validate_name("agent", name)?;
        if let Some(dir) = &self.dir {
            let path = dir.join(format!("{name}.toml"));
            if path.is_file() {
                return load_profile(name, &path, dir);
            }
        }
        if let Some(base) = base_kind_from_name(name) {
            return Ok(ResolvedAgent {
                name: name.to_owned(),
                base,
                profile: None,
            });
        }
        Err(agent_profile_not_found(name))
    }

    /// Enumerate the resolvable host profiles, for `host.inspect`. Lists every
    /// `<dir>/<name>.toml` that resolves cleanly; a malformed, non-contained, or
    /// badly-named file is skipped with a warning so one bad profile never hides
    /// the rest. Sorted by name for a deterministic listing.
    pub(crate) fn enumerate(&self) -> Vec<ResolvedAgent> {
        let Some(dir) = &self.dir else {
            return Vec::new();
        };
        let entries = match std::fs::read_dir(dir) {
            Ok(entries) => entries,
            Err(_) => return Vec::new(),
        };
        let mut resolved = Vec::new();
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|ext| ext.to_str()) != Some("toml") || !path.is_file() {
                continue;
            }
            let Some(stem) = path.file_stem().and_then(|stem| stem.to_str()) else {
                continue;
            };
            match self.resolve_agent(stem) {
                Ok(agent) if agent.profile.is_some() => resolved.push(agent),
                Ok(_) => {}
                Err(err) => {
                    warn!(profile = %stem, error = %err, "skipping unresolvable agent profile during enumeration");
                }
            }
        }
        resolved.sort_by(|a, b| a.name.cmp(&b.name));
        resolved
    }
}

/// Whether `dir` is safe to load host config from: owned by the daemon's effective
/// user and not group/world-writable. An absent/unreadable dir is treated as secure
/// (there is simply nothing to load); only a present-but-unsafe dir fails the gate.
#[cfg(unix)]
fn dir_is_owner_secure(dir: &Path) -> bool {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    let Ok(meta) = std::fs::metadata(dir) else {
        return true;
    };
    // SAFETY: `geteuid` is always safe — it reads the calling process's effective
    // uid and cannot fail.
    #[allow(unsafe_code)]
    let euid = unsafe { libc::geteuid() };
    if meta.uid() != euid {
        return false;
    }
    meta.permissions().mode() & 0o022 == 0
}

#[cfg(not(unix))]
fn dir_is_owner_secure(_dir: &Path) -> bool {
    true
}

/// Canonicalize `candidate` and assert it stays within the canonicalized `base_dir`
/// tree — owner-checking a file alone is insufficient because a symlink would exec
/// its (out-of-tree) target. Returns the canonical path on success.
fn assert_contained(base_dir: &Path, candidate: &Path, name: &str) -> Result<(), ProtocolError> {
    let canon_base = std::fs::canonicalize(base_dir)
        .map_err(|err| invalid_profile(name, &format!("agents directory: {err}")))?;
    let canon = std::fs::canonicalize(candidate)
        .map_err(|err| invalid_profile(name, &format!("{}: {err}", candidate.display())))?;
    if !canon.starts_with(&canon_base) {
        return Err(invalid_profile(
            name,
            "resolves outside the agents directory (symlink escape)",
        ));
    }
    Ok(())
}

/// Map a base-kind name to its [`AgentKind`]; `None` for anything else.
pub(crate) fn base_kind_from_name(name: &str) -> Option<AgentKind> {
    match name {
        "shell" => Some(AgentKind::Shell),
        "codex" => Some(AgentKind::Codex),
        "claude" => Some(AgentKind::Claude),
        _ => None,
    }
}

/// The default launch program for a bare base kind (used when a profile omits
/// `program`, and as the frozen snapshot program for a profile-less session).
pub(crate) fn default_program(base: AgentKind) -> String {
    match base {
        AgentKind::Shell => std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_owned()),
        AgentKind::Codex => "codex".to_owned(),
        AgentKind::Claude => "claude".to_owned(),
    }
}

fn load_profile(name: &str, path: &Path, dir: &Path) -> Result<ResolvedAgent, ProtocolError> {
    // Containment first: a symlinked `<name>.toml` that escapes the tree must be
    // rejected before its contents are read or exec'd (C.5).
    assert_contained(dir, path, name)?;
    let content =
        std::fs::read_to_string(path).map_err(|err| invalid_profile(name, &err.to_string()))?;
    let raw: RawProfile =
        toml::from_str(&content).map_err(|err| invalid_profile(name, &err.to_string()))?;
    let base = base_kind_from_name(&raw.base)
        .ok_or_else(|| invalid_profile(name, &format!("unknown base kind '{}'", raw.base)))?;
    // A shell has no native resume; a `base = "shell"` profile may never claim one.
    if base == AgentKind::Shell && raw.resume.as_ref().and_then(|r| r.resumable) == Some(true) {
        return Err(invalid_profile(
            name,
            "base = \"shell\" cannot set resume.resumable = true (a shell has no native resume)",
        ));
    }
    let program = raw.program.unwrap_or_else(|| default_program(base));
    let args = raw.args.unwrap_or_default();
    // Every `POHUNEK_`-prefixed key is reserved for the daemon handshake; strip the
    // whole prefix so a profile can never shadow `POHUNEK_ENV`/`_PROTOCOL_VERSION`/…
    // (the launch path also re-asserts this by appending the handshake env last).
    let env: Vec<(String, String)> = raw
        .env
        .into_iter()
        .filter(|(key, _)| !key.starts_with("POHUNEK_"))
        .collect();
    let input_rules = raw.input_rules.map(|rules| InputRules {
        bracketed_paste: rules.bracketed_paste.unwrap_or(false),
        submit_delay: Duration::from_millis(rules.submit_delay_ms.unwrap_or(0)),
    });
    let resume = resolve_resume(name, base, raw.resume.as_ref())?;
    let manifest = resolve_manifest(name, dir, raw.manifest.as_deref())?;
    Ok(ResolvedAgent {
        name: name.to_owned(),
        base,
        profile: Some(ResolvedProfile {
            program,
            args,
            env,
            input_rules,
            resume,
            manifest,
        }),
    })
}

/// Resolve a profile's effective resume template (C.3). Absent `[resume]` inherits
/// the base kind's template; an explicit block overrides mode/ref_kind/resumable,
/// defaulting any omitted field from the base kind. `None` ⇒ not resumable
/// (authoritative — never falls back to the base kind).
fn resolve_resume(
    name: &str,
    base: AgentKind,
    raw: Option<&RawResume>,
) -> Result<Option<ResumeTemplate>, ProtocolError> {
    let base_template = base_resume_template(base);
    let Some(raw) = raw else {
        return Ok(base_template);
    };
    // Default resumability to whether the base kind resumes at all.
    if !raw.resumable.unwrap_or(base_template.is_some()) {
        return Ok(None);
    }
    let mode = match raw.mode.as_deref() {
        Some(value) => parse_resume_mode(name, value)?,
        None => base_template.map(|template| template.mode).ok_or_else(|| {
            invalid_profile(
                name,
                "resume.mode is required for a profile with no resumable base",
            )
        })?,
    };
    let ref_kind = match raw.ref_kind.as_deref() {
        Some(value) => parse_ref_kind(name, value)?,
        None => base_template
            .map(|template| template.ref_kind)
            .unwrap_or(SessionRefKind::Id),
    };
    Ok(Some(ResumeTemplate { mode, ref_kind }))
}

fn parse_resume_mode(name: &str, value: &str) -> Result<ResumeMode, ProtocolError> {
    match value {
        "flag" => Ok(ResumeMode::Flag),
        "subcommand" => Ok(ResumeMode::Subcommand),
        other => Err(invalid_profile(
            name,
            &format!("unknown resume.mode '{other}' (expected 'flag' or 'subcommand')"),
        )),
    }
}

fn parse_ref_kind(name: &str, value: &str) -> Result<SessionRefKind, ProtocolError> {
    match value {
        "id" => Ok(SessionRefKind::Id),
        "path" => Ok(SessionRefKind::Path),
        other => Err(invalid_profile(
            name,
            &format!("unknown resume.ref_kind '{other}' (expected 'id' or 'path')"),
        )),
    }
}

/// Resolve a profile's optional detection-manifest override (C.3) from
/// `<dir>/manifests/<manifest>.toml`, under the same charset + containment guard as
/// a profile file. A malformed manifest fails the profile closed (`invalid_profile`)
/// rather than panicking the daemon; an empty-rule manifest is accepted (it parses
/// fine and simply disables detection).
fn resolve_manifest(
    name: &str,
    dir: &Path,
    manifest_name: Option<&str>,
) -> Result<Option<Manifest>, ProtocolError> {
    let Some(manifest_name) = manifest_name else {
        return Ok(None);
    };
    validate_name("manifest", manifest_name)?;
    let path = dir.join("manifests").join(format!("{manifest_name}.toml"));
    if !path.is_file() {
        return Err(invalid_profile(
            name,
            &format!("manifest '{manifest_name}' not found in agents/manifests"),
        ));
    }
    assert_contained(dir, &path, name)?;
    let content = std::fs::read_to_string(&path)
        .map_err(|err| invalid_profile(name, &format!("manifest '{manifest_name}': {err}")))?;
    let manifest = Manifest::parse_str(&content)
        .map_err(|err| invalid_profile(name, &format!("manifest '{manifest_name}': {err}")))?;
    Ok(Some(manifest))
}

/// `runtime/agent_profile_not_found`: a name resolved to neither a host profile nor
/// a base kind (fail-closed; no silent default).
pub(crate) fn agent_profile_not_found(name: &str) -> ProtocolError {
    ProtocolError::new(
        ErrorClass::Runtime,
        "agent_profile_not_found",
        format!("no agent profile or base kind named '{name}' on this host"),
        Some(
            "use shell|codex|claude, or add ~/.config/pohunek/agents/<name>.toml on the target host"
                .to_owned(),
        ),
    )
}

/// `runtime/invalid_profile`: a profile file failed to parse, named an unknown base
/// kind, or violated a load-time rule (e.g. shell + resumable).
fn invalid_profile(name: &str, reason: &str) -> ProtocolError {
    ProtocolError::new(
        ErrorClass::Runtime,
        "invalid_profile",
        format!("invalid agent profile '{name}': {reason}"),
        None,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    fn tmp_agents_dir(tag: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("after epoch")
            .as_nanos();
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "pohunek-agents-{tag}-{}-{nanos}-{n}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).expect("create agents dir");
        dir
    }

    #[test]
    fn bare_base_kinds_resolve_without_a_profile() {
        let reg = ProfileRegistry::new(None);
        for (name, base) in [
            ("shell", AgentKind::Shell),
            ("codex", AgentKind::Codex),
            ("claude", AgentKind::Claude),
        ] {
            let resolved = reg.resolve_agent(name).expect("base kind resolves");
            assert_eq!(resolved.base, base);
            assert_eq!(resolved.name, name);
            assert!(
                resolved.profile.is_none(),
                "a bare base kind has no profile override"
            );
        }
    }

    #[test]
    fn unknown_name_is_agent_profile_not_found() {
        let reg = ProfileRegistry::new(Some(tmp_agents_dir("missing")));
        let err = reg.resolve_agent("nope").expect_err("no such agent");
        assert_eq!(err.code, "agent_profile_not_found");
    }

    #[test]
    fn bad_names_are_invalid_name() {
        let reg = ProfileRegistry::new(Some(tmp_agents_dir("bad")));
        for bad in ["../etc", "a/b", "a\\b", "-x", ".hidden", "", "a\u{7}b"] {
            let err = reg.resolve_agent(bad).expect_err("must reject");
            assert_eq!(err.code, "invalid_name", "name {bad:?}");
        }
    }

    #[test]
    fn profile_overrides_program_args_env_input_rules() {
        let dir = tmp_agents_dir("override");
        std::fs::write(
            dir.join("claude-sonnet.toml"),
            "base = \"claude\"\n\
             program = \"claude\"\n\
             args = [\"--model\", \"claude-sonnet-4\"]\n\
             [env]\n\
             ANTHROPIC_MODEL = \"claude-sonnet-4\"\n\
             POHUNEK_ENV = \"0\"\n\
             [input_rules]\n\
             bracketed_paste = false\n\
             submit_delay_ms = 150\n",
        )
        .expect("write profile");
        let reg = ProfileRegistry::new(Some(dir));
        let resolved = reg.resolve_agent("claude-sonnet").expect("resolves");
        assert_eq!(resolved.base, AgentKind::Claude);
        assert_eq!(resolved.name, "claude-sonnet");
        let profile = resolved.profile.expect("has overrides");
        assert_eq!(profile.program, "claude");
        assert_eq!(profile.args, vec!["--model", "claude-sonnet-4"]);
        // ANTHROPIC_MODEL is kept; the reserved POHUNEK_ key is stripped.
        assert!(profile
            .env
            .iter()
            .any(|(k, v)| k == "ANTHROPIC_MODEL" && v == "claude-sonnet-4"));
        assert!(
            !profile.env.iter().any(|(k, _)| k.starts_with("POHUNEK_")),
            "POHUNEK_-prefixed profile env must be stripped: {:?}",
            profile.env
        );
        let rules = profile.input_rules.expect("input rules override");
        assert!(!rules.bracketed_paste);
        assert_eq!(rules.submit_delay, Duration::from_millis(150));
    }

    #[test]
    fn shell_base_with_resumable_is_rejected_at_load() {
        let dir = tmp_agents_dir("shell-resume");
        std::fs::write(
            dir.join("myshell.toml"),
            "base = \"shell\"\n[resume]\nresumable = true\n",
        )
        .expect("write profile");
        let reg = ProfileRegistry::new(Some(dir));
        let err = reg
            .resolve_agent("myshell")
            .expect_err("shell+resumable rejected");
        assert_eq!(err.code, "invalid_profile");
    }

    #[test]
    fn unknown_base_kind_is_invalid_profile() {
        let dir = tmp_agents_dir("bad-base");
        std::fs::write(dir.join("weird.toml"), "base = \"emacs\"\n").expect("write profile");
        let reg = ProfileRegistry::new(Some(dir));
        let err = reg
            .resolve_agent("weird")
            .expect_err("unknown base rejected");
        assert_eq!(err.code, "invalid_profile");
    }

    #[test]
    fn unknown_profile_key_is_invalid_profile() {
        let dir = tmp_agents_dir("bad-key");
        std::fs::write(
            dir.join("p.toml"),
            "base = \"claude\"\nflags = [\"--danger\"]\n",
        )
        .expect("write profile");
        let reg = ProfileRegistry::new(Some(dir));
        let err = reg.resolve_agent("p").expect_err("unknown key rejected");
        assert_eq!(err.code, "invalid_profile");
    }

    /// A non-empty, valid override manifest (mirrors the detect fixture).
    const VALID_MANIFEST: &str = "[[rules]]\n\
         id = \"custom-blocked\"\n\
         state = \"blocked\"\n\
         priority = 1\n\
         region = \"whole_recent\"\n\
         contains = \"custom blocker\"\n";

    fn write_profile(dir: &Path, name: &str, body: &str) {
        std::fs::write(dir.join(format!("{name}.toml")), body).expect("write profile");
    }

    fn write_manifest(dir: &Path, name: &str, body: &str) {
        let manifests = dir.join("manifests");
        std::fs::create_dir_all(&manifests).expect("create manifests dir");
        std::fs::write(manifests.join(format!("{name}.toml")), body).expect("write manifest");
    }

    #[test]
    fn resume_template_inherits_base_kind_without_a_resume_block() {
        let dir = tmp_agents_dir("resume-inherit");
        write_profile(&dir, "c", "base = \"claude\"\n");
        write_profile(&dir, "x", "base = \"codex\"\n");
        let reg = ProfileRegistry::new(Some(dir));

        let claude = reg.resolve_agent("c").expect("resolves").profile.unwrap();
        assert_eq!(
            claude.resume,
            Some(ResumeTemplate {
                mode: ResumeMode::Flag,
                ref_kind: SessionRefKind::Id,
            })
        );
        let codex = reg.resolve_agent("x").expect("resolves").profile.unwrap();
        assert_eq!(
            codex.resume,
            Some(ResumeTemplate {
                mode: ResumeMode::Subcommand,
                ref_kind: SessionRefKind::Id,
            })
        );
    }

    #[test]
    fn resume_block_overrides_mode_and_ref_kind() {
        let dir = tmp_agents_dir("resume-override");
        // A claude-based profile that drives a `resume` subcommand against a path.
        write_profile(
            &dir,
            "weird",
            "base = \"claude\"\n[resume]\nmode = \"subcommand\"\nref_kind = \"path\"\n",
        );
        let reg = ProfileRegistry::new(Some(dir));
        let resume = reg
            .resolve_agent("weird")
            .expect("resolves")
            .profile
            .unwrap()
            .resume;
        assert_eq!(
            resume,
            Some(ResumeTemplate {
                mode: ResumeMode::Subcommand,
                ref_kind: SessionRefKind::Path,
            })
        );
    }

    #[test]
    fn resumable_false_yields_no_resume_template() {
        let dir = tmp_agents_dir("resume-off");
        write_profile(
            &dir,
            "noresume",
            "base = \"codex\"\n[resume]\nresumable = false\n",
        );
        let reg = ProfileRegistry::new(Some(dir));
        let resume = reg
            .resolve_agent("noresume")
            .expect("resolves")
            .profile
            .unwrap()
            .resume;
        assert_eq!(
            resume, None,
            "resumable=false is authoritative, not base-fallback"
        );
    }

    #[test]
    fn unknown_resume_mode_or_ref_kind_is_invalid_profile() {
        let dir = tmp_agents_dir("resume-bad");
        write_profile(
            &dir,
            "badmode",
            "base = \"claude\"\n[resume]\nmode = \"telepathy\"\n",
        );
        write_profile(
            &dir,
            "badkind",
            "base = \"claude\"\n[resume]\nref_kind = \"socket\"\n",
        );
        let reg = ProfileRegistry::new(Some(dir));
        assert_eq!(
            reg.resolve_agent("badmode").expect_err("bad mode").code,
            "invalid_profile"
        );
        assert_eq!(
            reg.resolve_agent("badkind").expect_err("bad ref_kind").code,
            "invalid_profile"
        );
    }

    #[test]
    fn manifest_override_resolves_and_parses() {
        let dir = tmp_agents_dir("manifest-ok");
        write_profile(&dir, "p", "base = \"codex\"\nmanifest = \"mine\"\n");
        write_manifest(&dir, "mine", VALID_MANIFEST);
        let reg = ProfileRegistry::new(Some(dir));
        let manifest = reg
            .resolve_agent("p")
            .expect("resolves")
            .profile
            .unwrap()
            .manifest;
        assert!(
            manifest.is_some(),
            "the override manifest must be parsed and carried"
        );
    }

    #[test]
    fn empty_rule_manifest_is_accepted_with_detection_disabled() {
        // The documented C.3 decision: an empty manifest parses fine (no rules) and
        // is accepted, NOT treated as a load error.
        let dir = tmp_agents_dir("manifest-empty");
        write_profile(&dir, "p", "base = \"codex\"\nmanifest = \"empty\"\n");
        write_manifest(&dir, "empty", "");
        let reg = ProfileRegistry::new(Some(dir));
        let manifest = reg
            .resolve_agent("p")
            .expect("resolves")
            .profile
            .unwrap()
            .manifest;
        assert!(
            manifest.is_some(),
            "an empty-rule manifest loads (detection disabled)"
        );
    }

    #[test]
    fn malformed_manifest_disables_only_that_profile() {
        let dir = tmp_agents_dir("manifest-bad");
        write_profile(&dir, "broken", "base = \"codex\"\nmanifest = \"bad\"\n");
        // A typed ManifestError (invalid state), not just bad TOML.
        write_manifest(
            &dir,
            "bad",
            "[[rules]]\nid = \"x\"\nstate = \"telepathic\"\npriority = 1\nregion = \"whole_recent\"\ncontains = \"x\"\n",
        );
        // A second, healthy profile must still resolve — one bad manifest does not
        // poison the registry, and the daemon does not panic.
        write_profile(&dir, "good", "base = \"claude\"\n");
        let reg = ProfileRegistry::new(Some(dir));
        assert_eq!(
            reg.resolve_agent("broken")
                .expect_err("malformed manifest")
                .code,
            "invalid_profile"
        );
        assert!(
            reg.resolve_agent("good").is_ok(),
            "other profiles still load"
        );
    }

    #[test]
    fn missing_or_badly_named_manifest_is_rejected() {
        let dir = tmp_agents_dir("manifest-missing");
        write_profile(&dir, "p", "base = \"codex\"\nmanifest = \"ghost\"\n");
        write_profile(&dir, "q", "base = \"codex\"\nmanifest = \"../escape\"\n");
        let reg = ProfileRegistry::new(Some(dir));
        assert_eq!(
            reg.resolve_agent("p").expect_err("missing manifest").code,
            "invalid_profile"
        );
        // A traversal-shaped manifest name is rejected by the A.2.1 charset guard.
        assert_eq!(
            reg.resolve_agent("q").expect_err("bad manifest name").code,
            "invalid_name"
        );
    }

    #[test]
    fn enumerate_lists_resolvable_profiles_and_skips_bad_ones() {
        let dir = tmp_agents_dir("enumerate");
        write_profile(&dir, "alpha", "base = \"claude\"\n");
        write_profile(&dir, "beta", "base = \"codex\"\n");
        write_profile(&dir, "broken", "base = \"emacs\"\n"); // unknown base → skipped
        let reg = ProfileRegistry::new(Some(dir));
        let names: Vec<String> = reg.enumerate().into_iter().map(|a| a.name).collect();
        assert_eq!(names, vec!["alpha".to_owned(), "beta".to_owned()]);
    }

    #[cfg(unix)]
    #[test]
    fn group_or_world_writable_agents_dir_loads_no_profiles() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tmp_agents_dir("insecure-agents");
        write_profile(&dir, "p", "base = \"claude\"\n");
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o777))
            .expect("chmod world-writable");
        let reg = ProfileRegistry::new(Some(dir));
        // The whole host layer is disabled: the profile name is now unresolvable.
        assert_eq!(
            reg.resolve_agent("p")
                .expect_err("insecure dir disables profiles")
                .code,
            "agent_profile_not_found"
        );
        assert!(reg.enumerate().is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn group_or_world_writable_manifests_dir_loads_no_profiles() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tmp_agents_dir("insecure-manifests");
        write_profile(&dir, "p", "base = \"claude\"\n");
        write_manifest(&dir, "m", VALID_MANIFEST);
        std::fs::set_permissions(
            dir.join("manifests"),
            std::fs::Permissions::from_mode(0o777),
        )
        .expect("chmod world-writable manifests");
        let reg = ProfileRegistry::new(Some(dir));
        // An untrusted manifests/ dir fails the whole tree closed.
        assert_eq!(
            reg.resolve_agent("p")
                .expect_err("insecure manifests disables profiles")
                .code,
            "agent_profile_not_found"
        );
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_profile_escaping_the_tree_is_rejected() {
        let dir = tmp_agents_dir("symlink-profile");
        let outside = tmp_agents_dir("symlink-outside");
        std::fs::write(outside.join("evil.toml"), "base = \"claude\"\n").expect("write outside");
        std::os::unix::fs::symlink(outside.join("evil.toml"), dir.join("evil.toml"))
            .expect("symlink into tree");
        let reg = ProfileRegistry::new(Some(dir));
        assert_eq!(
            reg.resolve_agent("evil").expect_err("symlink escape").code,
            "invalid_profile"
        );
    }

    #[cfg(unix)]
    #[test]
    fn profile_manifest_resolving_outside_the_tree_is_rejected() {
        let dir = tmp_agents_dir("symlink-manifest");
        let outside = tmp_agents_dir("symlink-manifest-outside");
        std::fs::write(outside.join("evil.toml"), VALID_MANIFEST).expect("write outside manifest");
        let manifests = dir.join("manifests");
        std::fs::create_dir_all(&manifests).expect("create manifests dir");
        std::os::unix::fs::symlink(outside.join("evil.toml"), manifests.join("m.toml"))
            .expect("symlink manifest into tree");
        write_profile(&dir, "p", "base = \"codex\"\nmanifest = \"m\"\n");
        let reg = ProfileRegistry::new(Some(dir));
        assert_eq!(
            reg.resolve_agent("p")
                .expect_err("manifest symlink escape")
                .code,
            "invalid_profile"
        );
    }
}
