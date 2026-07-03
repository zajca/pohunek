//! Per-agent `SessionStart` hook installation.
//!
//! Ported from herdr (`src/integration/mod.rs` `install_claude`/`install_codex`
//! and `assets/{claude,codex}/herdr-agent-state.sh`), rewritten to emit *our*
//! handshake env names and *our* active-agent/native-id callback methods. The
//! hook reports nested active-agent identity for the owning session and captures
//! the launch agent's native session id for direct-session resume; live activity
//! still comes from the detector unless a hook has reliable activity evidence.
//!
//! Install merges into the agent's own config format idempotently and never
//! clobbers unrelated user hooks: only hooks whose command references our
//! installed script path are stripped before the `SessionStart` hook is
//! (re-)added.

use std::fmt::Write as _;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use protocol::{
    AgentKind, ErrorClass, IntegrationInstallReport, IntegrationInstallResult, ProtocolError,
};
use serde::Serialize;
use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};

// The agent-handshake env var names are defined once in `protocol` (the shared
// contract crate) so the daemon (which injects them), the installed hook (which
// reads them), and the CLI (which reads `ENV_SESSION_ID` for the
// self-feeding-attach guard) cannot drift. Re-exported here so existing daemon
// call sites and tests keep referring to `integration::ENV_*` unchanged.
pub use protocol::{
    ENV_DAEMON_ID, ENV_FLAG, ENV_PROTOCOL_VERSION, ENV_SESSION_ID, ENV_SOCKET_PATH,
};

/// Installed hook script file name (shared by both agents).
const HOOK_INSTALL_NAME: &str = "pohunek-agent-state.sh";
/// The Claude hook script, embedded at compile time.
const CLAUDE_HOOK_ASSET: &str = include_str!("assets/claude/pohunek-agent-state.sh");
/// The Codex hook script, embedded at compile time.
const CODEX_HOOK_ASSET: &str = include_str!("assets/codex/pohunek-agent-state.sh");
/// Per-hook timeout (seconds) recorded in the agent's hook config.
const HOOK_TIMEOUT_SECS: u64 = 10;
/// Action argument passed to the hook script for the `SessionStart` event.
const HOOK_ACTION: &str = "session";
/// `SessionStart` event name in the agents' hook config.
const SESSION_START_EVENT: &str = "SessionStart";

/// Env var overriding Claude's config dir (else `~/.claude`).
const CLAUDE_CONFIG_DIR_ENV: &str = "CLAUDE_CONFIG_DIR";
/// Env var overriding Codex's config dir (else `~/.codex`).
const CODEX_HOME_ENV: &str = "CODEX_HOME";

/// Files the installer wrote for one agent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstallPaths {
    /// Absolute path of the installed hook script.
    pub hook_path: PathBuf,
    /// Config files created or merged into, in the order touched.
    pub config_paths: Vec<PathBuf>,
}

/// Install the `SessionStart` hook for the selected agent(s).
///
/// `Some(agent)` installs that agent only and fails fast if its config dir is
/// absent. `None` installs the hook for every supported agent whose config dir
/// exists, and errors only if none are present.
///
/// # Errors
///
/// `agent_not_installable` for `AgentKind::Shell`, `agent_config_dir_missing`
/// when a requested (or, for `None`, every) agent config dir is absent, or any
/// underlying I/O / settings error.
pub fn install(agent: Option<AgentKind>) -> Result<IntegrationInstallResult, ProtocolError> {
    let installed = match agent {
        Some(AgentKind::Claude) => {
            vec![report(
                AgentKind::Claude,
                &install_claude(&claude_config_dir()?)?,
            )]
        }
        Some(AgentKind::Codex) => {
            vec![report(
                AgentKind::Codex,
                &install_codex(&codex_config_dir()?)?,
            )]
        }
        Some(AgentKind::Shell) => {
            return Err(ProtocolError::new(
                ErrorClass::Runtime,
                "agent_not_installable",
                "shell sessions have no hook integration",
                None,
            ));
        }
        None => install_all_present()?,
    };
    Ok(IntegrationInstallResult { installed })
}

/// Install for every supported agent whose config dir exists.
fn install_all_present() -> Result<Vec<IntegrationInstallReport>, ProtocolError> {
    let mut installed = Vec::new();
    let claude_dir = claude_config_dir()?;
    if claude_dir.is_dir() {
        installed.push(report(AgentKind::Claude, &install_claude(&claude_dir)?));
    }
    let codex_dir = codex_config_dir()?;
    if codex_dir.is_dir() {
        installed.push(report(AgentKind::Codex, &install_codex(&codex_dir)?));
    }
    if installed.is_empty() {
        return Err(ProtocolError::new(
            ErrorClass::Runtime,
            "agent_config_dir_missing",
            format!(
                "no agent config dir found (looked for {} and {})",
                claude_dir.display(),
                codex_dir.display()
            ),
            Some("install Claude Code or Codex first".to_owned()),
        ));
    }
    Ok(installed)
}

fn report(agent: AgentKind, paths: &InstallPaths) -> IntegrationInstallReport {
    IntegrationInstallReport {
        agent,
        hook_path: paths.hook_path.display().to_string(),
        config_paths: paths
            .config_paths
            .iter()
            .map(|path| path.display().to_string())
            .collect(),
    }
}

/// Resolve Claude's config dir: `$CLAUDE_CONFIG_DIR` or `~/.claude`.
pub fn claude_config_dir() -> Result<PathBuf, ProtocolError> {
    config_dir(CLAUDE_CONFIG_DIR_ENV, ".claude")
}

/// Resolve Codex's config dir: `$CODEX_HOME` or `~/.codex`.
pub fn codex_config_dir() -> Result<PathBuf, ProtocolError> {
    config_dir(CODEX_HOME_ENV, ".codex")
}

/// Install the Claude `SessionStart` hook into `claude_dir`.
///
/// Writes `hooks/pohunek-agent-state.sh` and merges a `SessionStart` hook
/// (matcher `*`) into `settings.json`, stripping any hooks this installer owns
/// first so reinstall is idempotent.
///
/// # Errors
///
/// Fails fast if `claude_dir` is absent, if `settings.json` is malformed, or on
/// any I/O error.
pub fn install_claude(claude_dir: &Path) -> Result<InstallPaths, ProtocolError> {
    if !claude_dir.is_dir() {
        return Err(config_dir_missing(AgentKind::Claude, claude_dir));
    }

    let hooks_dir = claude_dir.join("hooks");
    create_dir_all(&hooks_dir)?;
    let hook_path = hooks_dir.join(HOOK_INSTALL_NAME);
    write_file(&hook_path, CLAUDE_HOOK_ASSET)?;
    make_executable(&hook_path)?;

    let settings_path = claude_dir.join("settings.json");
    let mut settings = read_json_object_or_empty(&settings_path)?;
    let hooks = ensure_hooks_object(&mut settings, &settings_path)?;
    remove_owned_command_hooks(hooks, &hook_path);
    ensure_command_hook(hooks, &hook_command(&hook_path), Some("*"))?;
    write_json_pretty(&settings_path, &settings)?;

    Ok(InstallPaths {
        hook_path,
        config_paths: vec![settings_path],
    })
}

/// Install the Codex `SessionStart` hook into `codex_dir`.
///
/// Writes `pohunek-agent-state.sh`, merges a `SessionStart` hook into
/// `hooks.json`, and enables `[features] hooks = true` in `config.toml`,
/// idempotently.
///
/// # Errors
///
/// Fails fast if `codex_dir` is absent, if `hooks.json` is malformed, or on any
/// I/O error.
pub fn install_codex(codex_dir: &Path) -> Result<InstallPaths, ProtocolError> {
    if !codex_dir.is_dir() {
        return Err(config_dir_missing(AgentKind::Codex, codex_dir));
    }

    let hook_path = codex_dir.join(HOOK_INSTALL_NAME);
    write_file(&hook_path, CODEX_HOOK_ASSET)?;
    make_executable(&hook_path)?;

    let hooks_path = codex_dir.join("hooks.json");
    let mut hooks_file = read_json_object_or_empty(&hooks_path)?;
    let hooks = ensure_hooks_object(&mut hooks_file, &hooks_path)?;
    remove_owned_command_hooks(hooks, &hook_path);
    let command = hook_command(&hook_path);
    ensure_command_hook(hooks, &command, None)?;
    let (group_index, handler_index) = command_hook_position(hooks, &command).ok_or_else(|| {
        settings_invalid(
            &hooks_path,
            "installed Codex SessionStart hook was not found after merge",
        )
    })?;
    write_json_pretty(&hooks_path, &hooks_file)?;

    let config_path = codex_dir.join("config.toml");
    let existing = match fs::read_to_string(&config_path) {
        Ok(content) => content,
        Err(err) if err.kind() == io::ErrorKind::NotFound => String::new(),
        Err(err) => return Err(io_error("read", &config_path, &err)),
    };
    let trust_key = codex_hook_trust_key(&hooks_path, group_index, handler_index);
    let trusted_hash = codex_command_hook_trusted_hash(&command, HOOK_TIMEOUT_SECS, None)?;
    let updated = ensure_codex_hook_trust_state(
        &enable_codex_hooks_feature(&existing),
        &trust_key,
        &trusted_hash,
    );
    if updated != existing {
        write_file(&config_path, &updated)?;
    }

    Ok(InstallPaths {
        hook_path,
        config_paths: vec![hooks_path, config_path],
    })
}

fn command_hook_position(hooks: &Map<String, Value>, command: &str) -> Option<(usize, usize)> {
    hooks
        .get(SESSION_START_EVENT)?
        .as_array()?
        .iter()
        .enumerate()
        .find_map(|(group_index, group)| {
            group
                .get("hooks")?
                .as_array()?
                .iter()
                .enumerate()
                .find_map(|(handler_index, hook)| {
                    (hook.get("type").and_then(Value::as_str) == Some("command")
                        && hook.get("command").and_then(Value::as_str) == Some(command))
                    .then_some((group_index, handler_index))
                })
        })
}

fn codex_hook_trust_key(hooks_path: &Path, group_index: usize, handler_index: usize) -> String {
    format!(
        "{}:session_start:{group_index}:{handler_index}",
        hooks_path.display()
    )
}

#[derive(Serialize)]
struct CodexNormalizedHookIdentity {
    event_name: &'static str,
    #[serde(flatten)]
    group: CodexMatcherGroup,
}

#[derive(Clone, Serialize)]
struct CodexMatcherGroup {
    #[serde(default)]
    matcher: Option<String>,
    #[serde(default)]
    hooks: Vec<CodexHookHandlerConfig>,
}

#[derive(Clone, Serialize)]
#[serde(tag = "type")]
enum CodexHookHandlerConfig {
    #[serde(rename = "command")]
    Command {
        command: String,
        #[serde(default, rename = "commandWindows", alias = "command_windows")]
        command_windows: Option<String>,
        #[serde(default, rename = "timeout")]
        timeout_sec: Option<u64>,
        #[serde(default)]
        r#async: bool,
        #[serde(default, rename = "statusMessage")]
        status_message: Option<String>,
    },
}

fn codex_command_hook_trusted_hash(
    command: &str,
    timeout_sec: u64,
    matcher: Option<&str>,
) -> Result<String, ProtocolError> {
    let identity = CodexNormalizedHookIdentity {
        event_name: "session_start",
        group: CodexMatcherGroup {
            matcher: matcher.map(ToOwned::to_owned),
            hooks: vec![CodexHookHandlerConfig::Command {
                command: command.to_owned(),
                command_windows: None,
                timeout_sec: Some(timeout_sec),
                r#async: false,
                status_message: None,
            }],
        },
    };
    let value = toml::Value::try_from(identity).map_err(|err| {
        ProtocolError::new(
            ErrorClass::Runtime,
            "integration_settings_invalid",
            format!("failed to serialize Codex hook trust identity: {err}"),
            None,
        )
    })?;
    Ok(version_for_toml(&value))
}

fn version_for_toml(value: &toml::Value) -> String {
    let json = serde_json::to_value(value).unwrap_or(Value::Null);
    let canonical = canonical_json(&json);
    let serialized = serde_json::to_vec(&canonical).unwrap_or_default();
    let mut hasher = Sha256::new();
    hasher.update(serialized);
    let hash = hasher.finalize();
    let mut hex = String::with_capacity(hash.len() * 2);
    for byte in hash {
        write!(hex, "{byte:02x}").expect("writing to a String is infallible");
    }
    format!("sha256:{hex}")
}

fn canonical_json(value: &Value) -> Value {
    match value {
        Value::Object(map) => {
            let mut sorted = Map::new();
            let mut keys = map.keys().cloned().collect::<Vec<_>>();
            keys.sort();
            for key in keys {
                if let Some(value) = map.get(&key) {
                    sorted.insert(key, canonical_json(value));
                }
            }
            Value::Object(sorted)
        }
        Value::Array(items) => Value::Array(items.iter().map(canonical_json).collect()),
        other => other.clone(),
    }
}

/// Build the shell command string that runs our hook for the `SessionStart`
/// event. `sh '<path>' session`.
fn hook_command(hook_path: &Path) -> String {
    format!(
        "sh {} {}",
        shell_single_quote(&hook_path.display().to_string()),
        HOOK_ACTION
    )
}

/// Single-quote a value for a POSIX shell command line.
fn shell_single_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\"'\"'"))
}

/// Get (or create) the `hooks` object inside an agent settings document.
fn ensure_hooks_object<'a>(
    settings: &'a mut Value,
    settings_path: &Path,
) -> Result<&'a mut Map<String, Value>, ProtocolError> {
    let root = settings
        .as_object_mut()
        .ok_or_else(|| settings_invalid(settings_path, "top level must be a JSON object"))?;
    let hooks = root.entry("hooks").or_insert_with(|| json!({}));
    hooks
        .as_object_mut()
        .ok_or_else(|| settings_invalid(settings_path, "`hooks` must be a JSON object"))
}

/// Add a `SessionStart` command hook in the nested agent format, deduped.
///
/// Nested shape (Claude/Codex):
/// `{ "matcher": "...", "hooks": [{ "type": "command", "command": "...", "timeout": N }] }`.
fn ensure_command_hook(
    hooks: &mut Map<String, Value>,
    command: &str,
    matcher: Option<&str>,
) -> Result<(), ProtocolError> {
    let entries = hooks
        .entry(SESSION_START_EVENT.to_string())
        .or_insert_with(|| Value::Array(Vec::new()))
        .as_array_mut()
        .ok_or_else(|| {
            ProtocolError::new(
                ErrorClass::Runtime,
                "integration_settings_invalid",
                format!("hook entries for {SESSION_START_EVENT} must be an array"),
                None,
            )
        })?;

    let already_installed = entries.iter().any(|entry| {
        entry
            .get("hooks")
            .and_then(Value::as_array)
            .is_some_and(|hook_entries| {
                hook_entries.iter().any(|hook| {
                    hook.get("type").and_then(Value::as_str) == Some("command")
                        && hook.get("command").and_then(Value::as_str) == Some(command)
                })
            })
    });
    if already_installed {
        return Ok(());
    }

    let mut entry = Map::new();
    if let Some(matcher) = matcher {
        entry.insert("matcher".to_string(), Value::String(matcher.to_string()));
    }
    entry.insert(
        "hooks".to_string(),
        json!([{ "type": "command", "command": command, "timeout": HOOK_TIMEOUT_SECS }]),
    );
    entries.push(Value::Object(entry));
    Ok(())
}

/// Strip every command hook this installer owns (command references
/// `hook_path`), across all events, removing now-empty entries and events.
///
/// Ownership is keyed on the installed script path, which is unique to us, so
/// unrelated user hooks are never touched. This makes reinstall idempotent and
/// clears any stale lifecycle hook a prior version may have installed.
fn remove_owned_command_hooks(hooks: &mut Map<String, Value>, hook_path: &Path) {
    let needle = hook_path.display().to_string();
    let events: Vec<String> = hooks.keys().cloned().collect();
    for event in events {
        let Some(entries) = hooks.get_mut(&event).and_then(Value::as_array_mut) else {
            continue;
        };
        entries.retain_mut(|entry| {
            let Some(entry_object) = entry.as_object_mut() else {
                return true;
            };
            let Some(hook_entries) = entry_object.get_mut("hooks").and_then(Value::as_array_mut)
            else {
                return true;
            };
            hook_entries.retain(|hook| !command_references(hook, &needle));
            !hook_entries.is_empty()
        });
        if hooks
            .get(&event)
            .and_then(Value::as_array)
            .is_some_and(Vec::is_empty)
        {
            hooks.remove(&event);
        }
    }
}

/// Whether a hook entry is a command hook whose command references `needle`.
fn command_references(hook: &Value, needle: &str) -> bool {
    hook.get("type").and_then(Value::as_str) == Some("command")
        && hook
            .get("command")
            .and_then(Value::as_str)
            .is_some_and(|command| command.contains(needle))
}

/// Ensure `[features] hooks = true` in a Codex `config.toml`, preserving the
/// rest. Ported from herdr `build_codex_config_with_hooks`.
fn enable_codex_hooks_feature(content: &str) -> String {
    let mut lines: Vec<String> = content.lines().map(str::to_string).collect();
    let trailing_newline = content.ends_with('\n');
    let mut in_features = false;
    let mut features_header_index = None;
    let mut hooks_index = None;

    for (index, line) in lines.iter().enumerate() {
        if let Some(header) = toml_table_header(line) {
            in_features = header == "[features]";
            if in_features && features_header_index.is_none() {
                features_header_index = Some(index);
            }
            continue;
        }
        if in_features && is_toml_key(line, "hooks") {
            hooks_index = Some(index);
        }
    }

    if let Some(index) = hooks_index {
        lines[index] = "hooks = true".to_string();
        return join_toml_lines(&lines, trailing_newline);
    }

    if let Some(index) = features_header_index {
        lines.insert(index + 1, "hooks = true".to_string());
        return join_toml_lines(&lines, trailing_newline);
    }

    let mut result = content.trim_end_matches('\n').to_string();
    if !result.is_empty() {
        result.push_str("\n\n");
    }
    result.push_str("[features]\nhooks = true\n");
    result
}

fn ensure_codex_hook_trust_state(content: &str, trust_key: &str, trusted_hash: &str) -> String {
    let state_header = format!("[hooks.state.{}]", toml_basic_string(trust_key));
    let trusted_hash_line = format!("trusted_hash = {}", toml_basic_string(trusted_hash));
    let mut lines: Vec<String> = content.lines().map(str::to_string).collect();
    let trailing_newline = content.ends_with('\n');

    if let Some(header_index) = lines
        .iter()
        .position(|line| toml_table_header(line) == Some(state_header.as_str()))
    {
        let table_end = lines
            .iter()
            .enumerate()
            .skip(header_index + 1)
            .find_map(|(index, line)| toml_table_header(line).map(|_| index))
            .unwrap_or(lines.len());
        if let Some(trusted_hash_index) =
            (header_index + 1..table_end).find(|index| is_toml_key(&lines[*index], "trusted_hash"))
        {
            lines[trusted_hash_index] = trusted_hash_line;
        } else {
            lines.insert(header_index + 1, trusted_hash_line);
        }
        return join_toml_lines(&lines, trailing_newline);
    }

    let mut result = content.trim_end_matches('\n').to_string();
    if !result.is_empty() {
        result.push_str("\n\n");
    }
    if !has_toml_table(content, "[hooks.state]") {
        result.push_str("[hooks.state]\n\n");
    }
    result.push_str(&state_header);
    result.push('\n');
    result.push_str(&trusted_hash_line);
    result.push('\n');
    result
}

fn has_toml_table(content: &str, header: &str) -> bool {
    content
        .lines()
        .any(|line| toml_table_header(line) == Some(header))
}

fn toml_basic_string(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len() + 2);
    escaped.push('"');
    for ch in value.chars() {
        match ch {
            '\\' => escaped.push_str("\\\\"),
            '"' => escaped.push_str("\\\""),
            '\n' => escaped.push_str("\\n"),
            '\r' => escaped.push_str("\\r"),
            '\t' => escaped.push_str("\\t"),
            _ => escaped.push(ch),
        }
    }
    escaped.push('"');
    escaped
}

fn join_toml_lines(lines: &[String], trailing_newline: bool) -> String {
    let mut result = lines.join("\n");
    if trailing_newline || result.is_empty() {
        result.push('\n');
    }
    result
}

fn toml_table_header(line: &str) -> Option<&str> {
    let trimmed = line.trim_start();
    if trimmed.starts_with('#') || !trimmed.starts_with('[') {
        return None;
    }
    let header_end = if trimmed.starts_with("[[") {
        trimmed.find("]]").map(|index| index + 2)?
    } else {
        trimmed.find(']').map(|index| index + 1)?
    };
    let header = &trimmed[..header_end];
    let rest = trimmed[header_end..].trim_start();
    if !rest.is_empty() && !rest.starts_with('#') {
        return None;
    }
    Some(header)
}

fn is_toml_key(line: &str, key: &str) -> bool {
    let trimmed = line.trim();
    if trimmed.starts_with('#') || !trimmed.starts_with(key) {
        return false;
    }
    trimmed[key.len()..].trim_start().starts_with('=')
}

fn config_dir(env_var: &str, home_relative: &str) -> Result<PathBuf, ProtocolError> {
    if let Some(value) = std::env::var_os(env_var).filter(|value| !value.is_empty()) {
        return Ok(expand_tilde(PathBuf::from(value)));
    }
    let home = std::env::var_os("HOME")
        .filter(|value| !value.is_empty())
        .ok_or_else(|| {
            ProtocolError::new(
                ErrorClass::Configuration,
                "missing_env",
                format!("cannot resolve agent config dir: neither {env_var} nor HOME is set"),
                None,
            )
        })?;
    Ok(PathBuf::from(home).join(home_relative))
}

/// Expand a leading `~`/`~/` against `$HOME`.
fn expand_tilde(path: PathBuf) -> PathBuf {
    let Some(raw) = path.to_str() else {
        return path;
    };
    let Some(home) = std::env::var_os("HOME").filter(|value| !value.is_empty()) else {
        return path;
    };
    if raw == "~" {
        return PathBuf::from(home);
    }
    if let Some(rest) = raw.strip_prefix("~/") {
        return PathBuf::from(home).join(rest);
    }
    path
}

fn read_json_object_or_empty(path: &Path) -> Result<Value, ProtocolError> {
    match fs::read_to_string(path) {
        Ok(content) => serde_json::from_str::<Value>(&content)
            .map_err(|err| settings_invalid(path, &format!("invalid JSON: {err}"))),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(json!({})),
        Err(err) => Err(io_error("read", path, &err)),
    }
}

fn write_json_pretty(path: &Path, value: &Value) -> Result<(), ProtocolError> {
    let body = serde_json::to_string_pretty(value)
        .map_err(|err| settings_invalid(path, &format!("could not serialize settings: {err}")))?;
    write_file(path, &body)
}

fn write_file(path: &Path, body: &str) -> Result<(), ProtocolError> {
    fs::write(path, body).map_err(|err| io_error("write", path, &err))
}

fn create_dir_all(path: &Path) -> Result<(), ProtocolError> {
    fs::create_dir_all(path).map_err(|err| io_error("create directory", path, &err))
}

fn make_executable(path: &Path) -> Result<(), ProtocolError> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        let mut perms = fs::metadata(path)
            .map_err(|err| io_error("stat", path, &err))?
            .permissions();
        perms.set_mode(0o755);
        fs::set_permissions(path, perms).map_err(|err| io_error("chmod", path, &err))?;
    };
    let _ = path;
    Ok(())
}

fn config_dir_missing(agent: AgentKind, dir: &Path) -> ProtocolError {
    let (name, hint) = match agent {
        AgentKind::Claude => ("claude", "install Claude Code first"),
        AgentKind::Codex => ("codex", "install Codex first"),
        AgentKind::Shell => ("shell", "shells have no hook integration"),
    };
    ProtocolError::new(
        ErrorClass::Runtime,
        "agent_config_dir_missing",
        format!("{name} config dir not found at {}", dir.display()),
        Some(hint.to_owned()),
    )
}

fn settings_invalid(path: &Path, message: &str) -> ProtocolError {
    ProtocolError::new(
        ErrorClass::Runtime,
        "integration_settings_invalid",
        format!("{}: {message}", path.display()),
        None,
    )
}

fn io_error(action: &str, path: &Path, source: &io::Error) -> ProtocolError {
    ProtocolError::new(
        ErrorClass::Runtime,
        "integration_io_failed",
        format!("failed to {action} {}: {source}", path.display()),
        None,
    )
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::time::{SystemTime, UNIX_EPOCH};

    use protocol::method;
    use serde_json::{json, Value};

    use super::{
        codex_command_hook_trusted_hash, codex_hook_trust_key, install_claude, install_codex,
        toml_basic_string, CLAUDE_HOOK_ASSET, CODEX_HOOK_ASSET, ENV_FLAG, ENV_PROTOCOL_VERSION,
        ENV_SESSION_ID, ENV_SOCKET_PATH, HOOK_TIMEOUT_SECS,
    };

    fn temp_dir(tag: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time after epoch")
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "pohunek-integration-{tag}-{}-{nanos}",
            std::process::id()
        ));
        fs::create_dir_all(&dir).expect("create temp dir");
        dir
    }

    fn read_json(path: &Path) -> Value {
        serde_json::from_str(&fs::read_to_string(path).expect("read json")).expect("parse json")
    }

    fn session_start_command_hooks(settings: &Value) -> Vec<String> {
        settings["hooks"]["SessionStart"]
            .as_array()
            .map(|entries| {
                entries
                    .iter()
                    .filter_map(|entry| entry.get("hooks").and_then(Value::as_array))
                    .flatten()
                    .filter_map(|hook| hook.get("command").and_then(Value::as_str))
                    .map(str::to_owned)
                    .collect()
            })
            .unwrap_or_default()
    }

    #[test]
    fn codex_hook_trust_hash_matches_codex_normalized_identity() {
        let hash =
            codex_command_hook_trusted_hash("sh '/tmp/pohunek-agent-state.sh' session", 10, None)
                .expect("hash Codex hook identity");

        assert_eq!(
            hash,
            "sha256:93067e645008b68a24d9341f188d245c8491bf9667f89b470391737e93dbe0d4"
        );
    }

    #[test]
    fn assets_fire_active_agent_then_native_id_with_our_env_and_exit_zero_on_missing_env() {
        for asset in [CLAUDE_HOOK_ASSET, CODEX_HOOK_ASSET] {
            assert!(
                asset.starts_with("#!/bin/sh"),
                "hook must be a POSIX sh script"
            );
            assert!(
                asset.contains(method::SESSION_REPORT_AGENT),
                "hook must fire our active-agent method"
            );
            assert!(
                asset.contains(method::SESSION_REPORT_NATIVE_ID),
                "hook must fire our native-id method"
            );
            let report_agent_index = asset
                .find(method::SESSION_REPORT_AGENT)
                .expect("asset contains active-agent method");
            let report_native_index = asset
                .find(method::SESSION_REPORT_NATIVE_ID)
                .expect("asset contains native-id method");
            assert!(
                report_agent_index < report_native_index,
                "hook must report active agent before native id"
            );
            assert!(
                asset.contains("native_id_params[\"transcript_path\"] = transcript_path"),
                "hook must forward transcript_path to native-id reports for path-kind resume"
            );
            for env_name in [
                ENV_FLAG,
                ENV_SOCKET_PATH,
                ENV_SESSION_ID,
                ENV_PROTOCOL_VERSION,
            ] {
                assert!(
                    asset.contains(env_name),
                    "hook must reference handshake env {env_name}"
                );
            }
            // Missing handshake env / runtime must be a silent no-op.
            assert!(
                asset.contains("|| exit 0"),
                "hook must no-op (exit 0) when prerequisites are missing"
            );
            assert!(
                asset.contains("command -v python3"),
                "hook must guard on python3 availability"
            );
            // The terminal python invocation itself must be exit-0-guarded so an
            // abnormal interpreter exit (OOM, hook timeout kill) under `set -e`
            // never propagates a non-zero status that could break the agent.
            assert!(
                asset.contains("python3 - <<'PY' || exit 0"),
                "the python heredoc must be guarded with `|| exit 0`"
            );
        }
    }

    #[test]
    fn install_claude_into_fresh_dir_writes_executable_hook_and_session_start() {
        let claude_dir = temp_dir("claude-fresh");
        let paths = install_claude(&claude_dir).expect("install claude");

        assert!(paths.hook_path.is_file(), "hook script must be written");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(&paths.hook_path)
                .expect("hook metadata")
                .permissions()
                .mode();
            assert_eq!(mode & 0o111, 0o111, "hook must be executable");
        };

        let settings = read_json(&claude_dir.join("settings.json"));
        let commands = session_start_command_hooks(&settings);
        assert_eq!(commands.len(), 1, "exactly one SessionStart hook");
        assert!(commands[0].contains(paths.hook_path.to_str().unwrap()));
        // matcher is "*"
        assert_eq!(settings["hooks"]["SessionStart"][0]["matcher"], json!("*"));
    }

    #[test]
    fn install_claude_preserves_unrelated_hooks_and_is_idempotent() {
        let claude_dir = temp_dir("claude-merge");
        // Pre-existing, unrelated user settings and hooks.
        let settings_path = claude_dir.join("settings.json");
        fs::write(
            &settings_path,
            serde_json::to_string_pretty(&json!({
                "model": "claude-opus-4-8",
                "hooks": {
                    "PreToolUse": [
                        { "matcher": "*", "hooks": [
                            { "type": "command", "command": "echo user-pretool" }
                        ]}
                    ],
                    "SessionStart": [
                        { "matcher": "*", "hooks": [
                            { "type": "command", "command": "echo user-sessionstart" }
                        ]}
                    ]
                }
            }))
            .unwrap(),
        )
        .unwrap();

        install_claude(&claude_dir).expect("first install");
        install_claude(&claude_dir).expect("reinstall");

        let settings = read_json(&settings_path);
        // Unrelated top-level key preserved.
        assert_eq!(settings["model"], json!("claude-opus-4-8"));
        // Unrelated PreToolUse hook preserved.
        assert_eq!(
            settings["hooks"]["PreToolUse"][0]["hooks"][0]["command"],
            json!("echo user-pretool")
        );
        // Both the user's SessionStart hook and exactly one of ours survive
        // (no duplicate from the reinstall).
        let commands = session_start_command_hooks(&settings);
        assert!(commands.contains(&"echo user-sessionstart".to_owned()));
        let ours = commands
            .iter()
            .filter(|command| command.contains("pohunek-agent-state.sh"))
            .count();
        assert_eq!(
            ours, 1,
            "reinstall must not duplicate our hook: {commands:?}"
        );
    }

    #[test]
    fn install_codex_writes_hook_hooks_json_and_enables_feature() {
        let codex_dir = temp_dir("codex-fresh");
        let paths = install_codex(&codex_dir).expect("install codex");

        assert!(paths.hook_path.is_file());
        let hooks = read_json(&codex_dir.join("hooks.json"));
        let commands = session_start_command_hooks(&hooks);
        assert_eq!(commands.len(), 1);
        // Codex SessionStart hook carries no matcher key.
        assert!(hooks["hooks"]["SessionStart"][0].get("matcher").is_none());

        let config = fs::read_to_string(codex_dir.join("config.toml")).expect("config.toml");
        assert!(config.contains("[features]"), "config: {config}");
        assert!(config.contains("hooks = true"), "config: {config}");

        let trust_key = codex_hook_trust_key(&codex_dir.join("hooks.json"), 0, 0);
        let trusted_hash = codex_command_hook_trusted_hash(&commands[0], HOOK_TIMEOUT_SECS, None)
            .expect("hash installed Codex hook");
        assert!(
            config.contains(&format!("[hooks.state.{}]", toml_basic_string(&trust_key))),
            "config: {config}"
        );
        assert!(
            config.contains(&format!(
                "trusted_hash = {}",
                toml_basic_string(&trusted_hash)
            )),
            "config: {config}"
        );
    }

    #[test]
    fn install_codex_is_idempotent_in_config_toml() {
        let codex_dir = temp_dir("codex-idem");
        // Pre-existing config with an unrelated key.
        fs::write(
            codex_dir.join("config.toml"),
            "model = \"gpt-5\"\n\n[features]\nother = true\n",
        )
        .unwrap();

        install_codex(&codex_dir).expect("first install");
        let after_first = fs::read_to_string(codex_dir.join("config.toml")).unwrap();
        install_codex(&codex_dir).expect("reinstall");
        let after_second = fs::read_to_string(codex_dir.join("config.toml")).unwrap();

        assert_eq!(after_first, after_second, "config.toml must be idempotent");
        assert!(after_second.contains("model = \"gpt-5\""), "{after_second}");
        assert!(after_second.contains("other = true"), "{after_second}");
        assert_eq!(
            after_second.matches("hooks = true").count(),
            1,
            "exactly one hooks=true: {after_second}"
        );
    }

    #[test]
    fn install_into_missing_dir_fails_fast() {
        let missing = temp_dir("missing-parent").join("does-not-exist");
        let err = install_claude(&missing).expect_err("missing claude dir");
        assert_eq!(err.code, "agent_config_dir_missing");
        let err = install_codex(&missing).expect_err("missing codex dir");
        assert_eq!(err.code, "agent_config_dir_missing");
    }
}
