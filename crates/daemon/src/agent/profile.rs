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

use super::InputRules;
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
    /// Resume override (mode/ref_kind/resumable); fully wired in C2. Parsed here so
    /// the load-time `shell` + `resumable=true` rejection works.
    #[serde(default)]
    resume: Option<RawResume>,
    /// Detection-manifest override name; loaded/validated in C2 (parsed now so a
    /// `manifest = "<name>"` key is accepted, not rejected by deny_unknown_fields).
    #[serde(default)]
    #[allow(dead_code)]
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
    // `mode`/`ref_kind` are wired into the resume argv in C2; parsed now so the
    // keys are accepted and `resumable` (read here) can gate the shell rule.
    #[serde(default)]
    #[allow(dead_code)]
    mode: Option<String>,
    #[serde(default)]
    #[allow(dead_code)]
    ref_kind: Option<String>,
    #[serde(default)]
    resumable: Option<bool>,
}

/// Host-profile launch overrides. Present on a [`ResolvedAgent`] only when a
/// profile file backed the name; a bare base kind carries `None` and launches
/// exactly as the compiled base adapter.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ResolvedProfile {
    /// Launch program (resolved on PATH at launch, like a base kind's program).
    pub program: String,
    /// Launch args.
    pub args: Vec<String>,
    /// Non-secret PTY env, with every `POHUNEK_`-prefixed key already stripped.
    pub env: Vec<(String, String)>,
    /// Input-rules override; `None` ⇒ inherit the base kind's rules.
    pub input_rules: Option<InputRules>,
}

/// The resolution of an agent NAME on this host: its base kind plus optional
/// host-profile overrides.
#[derive(Debug, Clone, PartialEq, Eq)]
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
/// C1 reads + parses + resolves on demand. The owner-only permission gate and the
/// canonicalize-and-contain symlink guard on the `agents/` tree are added in C2.
#[derive(Debug, Clone, Default)]
pub(crate) struct ProfileRegistry {
    /// The `agents/` directory, or `None` when the host-config layer is disabled.
    dir: Option<PathBuf>,
}

impl ProfileRegistry {
    pub(crate) fn new(dir: Option<PathBuf>) -> Self {
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
                return load_profile(name, &path);
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
/// `program`).
fn default_program(base: AgentKind) -> String {
    match base {
        AgentKind::Shell => std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_owned()),
        AgentKind::Codex => "codex".to_owned(),
        AgentKind::Claude => "claude".to_owned(),
    }
}

fn load_profile(name: &str, path: &Path) -> Result<ResolvedAgent, ProtocolError> {
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
    Ok(ResolvedAgent {
        name: name.to_owned(),
        base,
        profile: Some(ResolvedProfile {
            program,
            args,
            env,
            input_rules,
        }),
    })
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
}
