//! Fixed, bounded Hermes CLI operations for the Pohunek plugin lifecycle.
//!
//! This module exposes only the pinned version and plugin-list queries. It
//! clears the inherited environment, owns each child process group, and never
//! retains subprocess output in an error.

// Rust guideline compliant 2026-08-12

#![expect(
    clippy::map_err_ignore,
    reason = "runner errors intentionally redact subprocess and filesystem details"
)]
#![expect(
    clippy::struct_excessive_bools,
    reason = "the payload-free probe contract exposes independent security findings"
)]

use std::fmt;
use std::fs;
use std::io::Read;
use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStderr, ChildStdout, Command, ExitStatus, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::thread;
use std::time::{Duration, Instant};

use nix::unistd::{eaccess, AccessFlags, Uid};
use serde::{Deserialize, Serialize};

use super::error::Error;
use super::target::{HermesInvocation, ResolvedTarget};

/// The release prefix required by `compat/hermes/compatibility-lock.json`.
const PINNED_HERMES_VERSION_LINE: &str = "Hermes Agent v0.20.0 (2026.8.3)";
/// Hermes appends source-provenance metadata after this delimiter for Git installs.
const HERMES_VERSION_METADATA_DELIMITER: &str = " · ";
/// A local lifecycle query must finish promptly and never block the CLI indefinitely.
const COMMAND_TIMEOUT: Duration = Duration::from_secs(5);
/// This caps each pipe independently while still draining it to prevent a pipe deadlock.
const MAX_STREAM_BYTES: usize = 64 * 1024;
/// Polling keeps timeout latency bounded without busy-spinning.
const PROCESS_POLL_INTERVAL: Duration = Duration::from_millis(10);
/// File descriptors zero through two are the only handles allowed across exec.
const FIRST_INHERITED_FD: libc::c_int = 3;
/// POSIX guarantees at least this fallback when `_SC_OPEN_MAX` is indeterminate.
const MINIMUM_FD_LIMIT: libc::c_int = 20;
/// A deterministic locale makes the pinned human-readable version line stable.
const RUNNER_LOCALE: &str = "C";
/// Hermes must not emit colour control sequences into its fixed version output.
const NO_COLOR_VALUE: &str = "1";
/// Hermes receives a non-interactive terminal declaration for local lifecycle operations.
const RUNNER_TERM: &str = "dumb";
/// Installed probes forward only non-secret path roots required by the Pohunek CLI.
const PROBE_ENVIRONMENT_KEYS: [&str; 6] = [
    "HOME",
    "XDG_RUNTIME_DIR",
    "XDG_STATE_HOME",
    "XDG_CONFIG_HOME",
    "XDG_DATA_HOME",
    "XDG_CACHE_HOME",
];
/// Any group or other write bit could replace the executable after validation.
const UNSAFE_WRITE_BITS: u32 = 0o022;
/// Sticky shared directories such as `/tmp` safely isolate entry replacement.
const STICKY_BIT: u32 = 0o1000;
/// The only operator plugin record accepted from Hermes's JSON list.
const POHUNEK_PLUGIN_NAME: &str = "pohunek";
/// Lifecycle stages are accepted only beside the fixed final plugin directory.
const STAGE_PREFIX: &str = ".pohunek-stage-";
/// Plugin and staged directories are private and executable by their owner.
const PRIVATE_DIRECTORY_MODE: u32 = 0o700;
/// Managed source, marker, skill, and policy files are owner-readable only.
const PRIVATE_FILE_MODE: u32 = 0o600;
/// The fixed Python validator imports no Hermes profile and calls no plugin registration.
const STAGED_VALIDATOR_SCRIPT: &str = r#"
import ast
import importlib.util
import json
import os
import stat
import sys
from pathlib import Path

import yaml

# `-I` ignores PYTHONDONTWRITEBYTECODE, so set this before importing the staged
# managed package to keep validation side-effect free.
sys.dont_write_bytecode = True

TOOLS = [
    "pohunek_hosts", "pohunek_sessions", "pohunek_session_get",
    "pohunek_session_screen", "pohunek_session_output", "pohunek_session_wait",
    "pohunek_session_diff", "pohunek_session_start", "pohunek_session_send",
    "pohunek_session_resume", "pohunek_session_fork", "pohunek_session_resize",
    "pohunek_session_rename", "pohunek_session_set_metadata",
    "pohunek_session_stop", "pohunek_session_remove",
]
HOOKS = [
    "on_session_start", "pre_llm_call", "pre_approval_request",
    "post_approval_response", "post_llm_call", "on_session_end",
    "on_session_finalize",
]
PYTHON_FILES = ["__init__.py", "cli.py", "hooks.py", "policy.py", "redact.py", "tools.py"]
MANIFEST_KEYS = {"name", "version", "description", "provides_tools", "provides_hooks"}

if len(sys.argv) != 4 or sys.argv[1] != "--":
    raise ValueError("fixed staged validator arguments required")
plugin = Path(sys.argv[2])
staged_policy = Path(sys.argv[3])
plugin_resolved = plugin.resolve(strict=True)

def managed_file(relative):
    relative_path = Path(relative)
    current = plugin
    for component in relative_path.parts[:-1]:
        current = current / component
        info = current.lstat()
        if not stat.S_ISDIR(info.st_mode) or info.st_uid != os.getuid() or stat.S_IMODE(info.st_mode) != 0o700:
            raise ValueError("managed directory is not owner-private")
    path = plugin / relative_path
    info = path.lstat()
    if not stat.S_ISREG(info.st_mode) or info.st_uid != os.getuid() or stat.S_IMODE(info.st_mode) != 0o600:
        raise ValueError("managed file is not owner-private regular data")
    resolved = path.resolve(strict=True)
    if plugin_resolved not in resolved.parents:
        raise ValueError("managed file escapes plugin")
    return path

manifest_text = managed_file("plugin.yaml").read_text(encoding="utf-8")
manifest = yaml.safe_load(manifest_text)
manifest_node = yaml.compose(manifest_text, Loader=yaml.SafeLoader)
if not isinstance(manifest, dict) or not isinstance(manifest_node, yaml.MappingNode):
    raise ValueError("manifest must be a mapping")
manifest_keys = [key.value for key, _ in manifest_node.value]
if len(manifest_keys) != len(set(manifest_keys)) or set(manifest) != MANIFEST_KEYS:
    raise ValueError("manifest keys are invalid")
if manifest.get("name") != "pohunek" or manifest.get("version") != "1":
    raise ValueError("manifest identity is invalid")
if not isinstance(manifest.get("description"), str) or not manifest["description"].strip():
    raise ValueError("manifest description is invalid")
if manifest.get("provides_tools") != TOOLS or len(set(manifest["provides_tools"])) != len(TOOLS):
    raise ValueError("manifest tools are invalid")
if manifest.get("provides_hooks") != HOOKS or len(set(manifest["provides_hooks"])) != len(HOOKS):
    raise ValueError("manifest hooks are invalid")

for relative in PYTHON_FILES:
    source_path = managed_file(relative)
    source = source_path.read_text(encoding="utf-8")
    parsed = ast.parse(source, filename=relative)
    compile(parsed, relative, "exec")

marker = json.loads(managed_file(".pohunek-owned.json").read_text(encoding="utf-8"))
if not isinstance(marker, dict) or set(marker) != {"version", "hermes_home", "policy_path", "assets"}:
    raise ValueError("ownership marker is invalid")
if marker.get("version") != 1 or not isinstance(marker.get("policy_path"), str):
    raise ValueError("ownership marker policy is invalid")
bound_policy = Path(marker["policy_path"])
if not bound_policy.is_absolute():
    raise ValueError("bound policy must be absolute")
bound_resolved = bound_policy.resolve(strict=False)
if bound_resolved == plugin_resolved or plugin_resolved in bound_resolved.parents:
    raise ValueError("bound policy must be outside plugin")

package_name = "_pohunek_staged_validation"
spec = importlib.util.spec_from_file_location(
    package_name,
    plugin / "__init__.py",
    submodule_search_locations=[str(plugin)],
)
if spec is None or spec.loader is None:
    raise ValueError("staged package cannot be imported")
module = importlib.util.module_from_spec(spec)
sys.modules[package_name] = module
spec.loader.exec_module(module)
if getattr(module, "POLICY_PATH", None) != marker["policy_path"]:
    raise ValueError("embedded policy binding does not match marker")
module.load_policy(str(staged_policy))

skill_text = managed_file("skills/pohunek/SKILL.md").read_text(encoding="utf-8")
if not skill_text.startswith("---\n") or "\n---\n" not in skill_text[4:]:
    raise ValueError("skill frontmatter is missing")
frontmatter_text, body = skill_text[4:].split("\n---\n", 1)
frontmatter = yaml.safe_load(frontmatter_text)
if not isinstance(frontmatter, dict) or set(frontmatter) != {"name", "description", "metadata"}:
    raise ValueError("skill frontmatter is invalid")
if frontmatter.get("name") != "pohunek" or not isinstance(frontmatter.get("description"), str):
    raise ValueError("skill identity is invalid")
metadata = frontmatter.get("metadata")
hermes = metadata.get("hermes") if isinstance(metadata, dict) else None
if not isinstance(hermes, dict) or set(hermes) != {"requires_tools"}:
    raise ValueError("skill Hermes metadata is invalid")
required = hermes.get("requires_tools")
if required != TOOLS[:7] or len(set(required)) != 7 or not body.strip():
    raise ValueError("skill requirements are invalid")
"#;

/// The installed probe is intentionally self-contained: it imports only the
/// exact managed plugin and uses an `AF_UNIX` responder created for this process.
const INSTALLED_PROBE_SCRIPT: &str = r#"
import importlib.util
import builtins
import io
import json
import os
import socket
import sqlite3
import stat
import subprocess
import sys
import tempfile
import threading
import time
from pathlib import Path

sys.dont_write_bytecode = True

READ = (
    "pohunek_hosts", "pohunek_sessions", "pohunek_session_get",
    "pohunek_session_screen", "pohunek_session_output", "pohunek_session_wait",
    "pohunek_session_diff",
)
MANAGE = (
    "pohunek_session_start", "pohunek_session_send", "pohunek_session_resume",
    "pohunek_session_fork", "pohunek_session_resize", "pohunek_session_rename",
    "pohunek_session_set_metadata",
)
FULL = ("pohunek_session_stop", "pohunek_session_remove")
HOOKS = (
    "on_session_start", "pre_llm_call", "pre_approval_request",
    "post_approval_response", "post_llm_call", "on_session_end", "on_session_finalize",
)
LATENCY_CEILING_MS = 1_000
PROBE_PHASE = 1

def fail_closed(_kind, value, _traceback):
    failure_errno = value.errno if isinstance(value, OSError) and isinstance(value.errno, int) else 0
    result = {
        "probe_complete": False, "failure_phase": PROBE_PHASE,
        "failure_errno": max(0, min(failure_errno, 255)),
        "tool_count": 0, "tools_ok": False,
        "skill_count": 0, "skill_ok": False,
        "hook_count": 0, "hooks_ok": False,
        "integration_ready": False, "hook_no_subprocess": False,
        "hook_no_network": False, "hook_no_database": False,
        "forced_socket_failure_swallowed": False, "hook_latency_ms": 0,
    }
    print(json.dumps(result, separators=(",", ":")))
    sys.stdout.flush()
    os._exit(0)

sys.excepthook = fail_closed

if len(sys.argv) != 4 or sys.argv[1] != "--":
    raise ValueError("fixed installed probe arguments required")
plugin = Path(sys.argv[2])
policy_path = Path(sys.argv[3])

def private(path, directory):
    data = path.lstat()
    return ((stat.S_ISDIR(data.st_mode) if directory else stat.S_ISREG(data.st_mode))
            and data.st_uid == os.getuid() and stat.S_IMODE(data.st_mode) == (0o700 if directory else 0o600))

if not private(plugin, True) or not private(policy_path, False):
    raise ValueError("managed paths are not private")
for relative in ("__init__.py", "hooks.py", "tools.py", "cli.py", "policy.py", "redact.py", "skills/pohunek/SKILL.md"):
    candidate = plugin / relative
    if candidate.is_symlink() or not private(candidate, False):
        raise ValueError("managed plugin asset is unsafe")
if plugin.resolve(strict=True) not in policy_path.resolve(strict=True).parents:
    pass
elif policy_path.resolve(strict=True).is_relative_to(plugin.resolve(strict=True)):
    raise ValueError("policy must be external")

class Ctx:
    def __init__(self):
        self.tools = []
        self.hooks = []
        self.skills = []
    def register_tool(self, **kwargs):
        self.tools.append(kwargs.get("name"))
    def register_hook(self, name, callback):
        self.hooks.append((name, callback))
    def register_skill(self, name, path):
        self.skills.append((name, path))

def serve(server, stop):
    server.settimeout(0.05)
    while not stop.is_set():
        try:
            client, _ = server.accept()
        except TimeoutError:
            continue
        except OSError:
            break
        with client:
            try:
                client.recv(4096)
                client.sendall(b'{"ok":true}\n')
            except OSError:
                break

class Audit:
    def __init__(self, endpoint):
        self.endpoint = endpoint
        self.subprocess = False
        self.network = False
        self.database = False
        self._restore = []
        self._socket_forwards = 0
        self._connect_forwards = 0

    def replace(self, owner, name, replacement):
        self._restore.append((owner, name, getattr(owner, name)))
        setattr(owner, name, replacement)

    def start(self):
        original_socket = socket.socket
        audit = self
        class AuditedSocket:
            def __init__(self, family=socket.AF_INET, *args, **kwargs):
                self._family = family
                if family != socket.AF_UNIX:
                    audit.network = True
                    raise RuntimeError("hook network denied")
                audit._socket_forwards += 1
                self._inner = original_socket(family, *args, **kwargs)
            def connect(self, address):
                if self._family != socket.AF_UNIX or address != audit.endpoint:
                    audit.network = True
                    raise RuntimeError("hook network denied")
                audit._connect_forwards += 1
                return self._inner.connect(address)
            def __getattr__(self, name):
                return getattr(self._inner, name)
            def __enter__(self):
                self._inner.__enter__()
                return self
            def __exit__(self, *args):
                return self._inner.__exit__(*args)
        def deny_subprocess(*args, **kwargs):
            audit.subprocess = True
            raise RuntimeError("hook subprocess denied")
        def deny_network(*args, **kwargs):
            audit.network = True
            raise RuntimeError("hook network denied")
        def deny_database(*args, **kwargs):
            audit.database = True
            raise RuntimeError("hook database denied")
        original_open = builtins.open
        def audited_open(path, *args, **kwargs):
            if os.fspath(path) != f"/proc/{os.getpid()}/stat":
                audit.database = True
                raise RuntimeError("hook file access denied")
            return original_open(path, *args, **kwargs)
        def audited_path_open(path, *args, **kwargs):
            return audited_open(path, *args, **kwargs)
        def audited_os_open(path, *args, **kwargs):
            audit.database = True
            raise RuntimeError("hook file access denied")
        self.replace(socket, "socket", AuditedSocket)
        for name in ("getaddrinfo", "gethostbyname", "gethostbyname_ex", "gethostbyaddr", "create_connection"):
            self.replace(socket, name, deny_network)
        self.replace(builtins, "open", audited_open)
        self.replace(io, "open", audited_open)
        self.replace(Path, "open", audited_path_open)
        self.replace(os, "open", audited_os_open)
        for name in ("system", "popen", "fork", "forkpty", "posix_spawn", "posix_spawnp", "spawnv", "spawnve", "spawnvp", "spawnvpe", "execl", "execle", "execlp", "execlpe", "execv", "execve", "execvp", "execvpe"):
            if hasattr(os, name):
                self.replace(os, name, deny_subprocess)
        self.replace(subprocess, "Popen", deny_subprocess)
        self.replace(subprocess, "run", deny_subprocess)
        self.replace(subprocess, "call", deny_subprocess)
        self.replace(subprocess, "check_call", deny_subprocess)
        self.replace(subprocess, "check_output", deny_subprocess)
        self.replace(sqlite3, "connect", deny_database)

    def reset_attempts(self):
        self.subprocess = False
        self.network = False
        self.database = False

    def self_test(self, denied_path):
        def denied(callback):
            try:
                callback()
            except RuntimeError:
                return True
            except BaseException:
                return False
            return False
        socket_forwards = self._socket_forwards
        connect_forwards = self._connect_forwards
        checks = [
            denied(lambda: socket.socket(socket.AF_INET, socket.SOCK_STREAM)),
            denied(lambda: socket.socket(socket.AF_INET6, socket.SOCK_STREAM)),
            denied(lambda: socket.getaddrinfo("example.invalid", 443)),
            denied(lambda: socket.socket(socket.AF_UNIX, socket.SOCK_STREAM).connect(str(denied_path))),
            denied(lambda: builtins.open(denied_path, "rb")),
            denied(lambda: io.open(denied_path, "rb")),
            denied(lambda: denied_path.open("rb")),
            denied(lambda: os.open(denied_path, os.O_RDONLY)),
            denied(lambda: sqlite3.connect(str(denied_path))),
            denied(lambda: subprocess.run(("true",), check=True)),
            denied(lambda: os.system("true")),
        ]
        # AF_INET/AF_INET6 are denied before construction and the foreign
        # AF_UNIX endpoint is denied before the wrapped socket connects.
        guard_ok = (
            all(checks)
            and self._socket_forwards == socket_forwards + 1
            and self._connect_forwards == connect_forwards
        )
        self.reset_attempts()
        return guard_ok

    def stop(self):
        for owner, name, original in reversed(self._restore):
            setattr(owner, name, original)
        self._restore.clear()

PROBE_PHASE = 2
with tempfile.TemporaryDirectory(prefix="pohunek-hermes-doctor-") as directory:
    PROBE_PHASE = 21
    endpoint = str(Path(directory) / "worker.sock")
    server = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    PROBE_PHASE = 211
    server.bind(endpoint)
    PROBE_PHASE = 212
    # Listening before hooks are created prevents a connection-refused race.
    server.listen(8)
    PROBE_PHASE = 213
    stop = threading.Event()
    thread = threading.Thread(target=serve, args=(server, stop), daemon=True)
    PROBE_PHASE = 214
    thread.start()
    PROBE_PHASE = 22
    os.environ.update({
        "POHUNEK_ENV": "1", "POHUNEK_SESSION_ID": "doctor-session",
        "POHUNEK_RUNTIME_ID": "doctor-runtime", "POHUNEK_WORKER_SOCKET_PATH": endpoint,
        "POHUNEK_PROTOCOL_VERSION": "1", "POHUNEK_HOOK_TIMEOUT_MS": "50",
    })
    PROBE_PHASE = 23
    PROBE_PHASE = 3
    package = "_pohunek_installed_probe"
    spec = importlib.util.spec_from_file_location(package, plugin / "__init__.py", submodule_search_locations=[str(plugin)])
    if spec is None or spec.loader is None:
        raise ValueError("managed plugin import failed")
    module = importlib.util.module_from_spec(spec)
    sys.modules[package] = module
    spec.loader.exec_module(module)
    if module.POLICY_PATH != str(policy_path):
        raise ValueError("policy binding mismatch")
    PROBE_PHASE = 4
    context = Ctx()
    module.register(context)
    policy = module.load_policy(str(policy_path))
    expected = READ + (MANAGE if policy.access_mode in ("manage", "full") else ()) + (FULL if policy.access_mode == "full" else ())
    names = tuple(context.tools)
    hook_names = tuple(name for name, _ in context.hooks)
    PROBE_PHASE = 5
    audit = Audit(endpoint)
    audit.start()
    try:
        if not audit.self_test(Path(directory) / "audit-denied"):
            raise ValueError("hook audit self-test failed")
        started = time.monotonic()
        for _name, callback in context.hooks:
            callback({"session_id": "native-doctor", "completed": True})
        latency_ms = int((time.monotonic() - started) * 1000)
    finally:
        audit.stop()
    stop.set()
    server.close()
    thread.join(timeout=0.2)
    forced_swallowed = True
    audit.start()
    try:
        for _name, callback in context.hooks:
            callback({"session_id": "native-doctor", "completed": True})
    except BaseException:
        forced_swallowed = False
    finally:
        audit.stop()
    PROBE_PHASE = 6
    result = {
        "probe_complete": True, "failure_phase": 0, "failure_errno": 0,
        "tool_count": len(names), "tools_ok": names == expected and len(set(names)) == len(names),
        "skill_count": len(context.skills),
        "skill_ok": len(context.skills) == 1 and context.skills[0][0] == "pohunek" and Path(context.skills[0][1]).resolve() == (plugin / "skills/pohunek/SKILL.md").resolve(),
        "hook_count": len(hook_names), "hooks_ok": hook_names == HOOKS and len(set(hook_names)) == len(hook_names),
        "integration_ready": module.integration_status() == {"state": "ready", "failure_count": 0},
        "hook_no_subprocess": not audit.subprocess, "hook_no_network": not audit.network,
        "hook_no_database": not audit.database, "forced_socket_failure_swallowed": forced_swallowed,
        "hook_latency_ms": latency_ms,
    }
    if latency_ms > LATENCY_CEILING_MS:
        result["hooks_ok"] = False
    print(json.dumps(result, separators=(",", ":")))
"#;

/// The fixed state reported by Hermes for the managed Pohunek plugin.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PohunekState {
    /// Hermes loaded the operator plugin.
    Enabled,
    /// Hermes knows the plugin but currently keeps it disabled.
    Disabled,
    /// Hermes discovered the plugin but did not enable it for this profile.
    NotEnabled,
}

impl PohunekState {
    /// Returns whether Hermes will load the managed plugin in a turn.
    #[must_use]
    pub(crate) const fn is_enabled(self) -> bool {
        matches!(self, Self::Enabled)
    }
}

/// A canonical local Hermes executable restricted to fixed lifecycle queries.
pub(crate) struct HermesRunner {
    executable: PathBuf,
    timeout: Duration,
    stream_limit: usize,
}

impl fmt::Debug for HermesRunner {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("HermesRunner")
            .field("executable", &"<redacted>")
            .field("timeout", &self.timeout)
            .field("stream_limit", &self.stream_limit)
            .finish()
    }
}

impl HermesRunner {
    /// Creates a fixed runner from one operator-selected Hermes executable.
    ///
    /// # Errors
    ///
    /// Returns a payload-free error when `executable` is not a canonical,
    /// current-user-owned, non-writable executable.
    pub(crate) fn new(executable: &Path) -> Result<Self, Error> {
        Ok(Self {
            executable: validate_executable(executable)?,
            timeout: COMMAND_TIMEOUT,
            stream_limit: MAX_STREAM_BYTES,
        })
    }

    /// Verifies the exact Hermes release pinned by the compatibility contract.
    ///
    /// # Errors
    ///
    /// Returns [`Error::UnsupportedHermes`] when no output line contains the
    /// pinned release, optionally followed by Hermes's provenance metadata.
    pub(crate) fn verify_version(&self, target: &ResolvedTarget) -> Result<(), Error> {
        let output = self.run(target, FixedCommand::Version)?;
        let version = std::str::from_utf8(&output.stdout).map_err(|_| Error::UnsupportedHermes)?;
        if version.lines().any(pinned_version_line_matches) {
            Ok(())
        } else {
            Err(Error::UnsupportedHermes)
        }
    }

    /// Reads exactly one valid Pohunek entry from Hermes's JSON plugin list.
    ///
    /// # Errors
    ///
    /// Rejects malformed, duplicate, missing, wrong-source, or unsupported
    /// state records without returning command output.
    pub(crate) fn pohunek_state(&self, target: &ResolvedTarget) -> Result<PohunekState, Error> {
        let output = self.run(target, FixedCommand::List)?;
        parse_plugin_state(&output.stdout)
    }

    /// Reports whether exactly the managed Pohunek plugin is enabled.
    ///
    /// # Errors
    ///
    /// Forwards the fixed plugin-list validation failure without exposing its
    /// command output.
    pub(crate) fn is_enabled(&self, target: &ResolvedTarget) -> Result<bool, Error> {
        match fs::symlink_metadata(target.plugin_root()) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
            Ok(_) => {}
            Err(_) => return Err(Error::InvalidHermesState),
        }
        Ok(self.pohunek_state(target)?.is_enabled())
    }

    /// Enables only the fixed Pohunek plugin and verifies the pinned state.
    ///
    /// # Errors
    ///
    /// Returns a payload-free command or state error unless the follow-up list
    /// contains exactly one enabled user plugin.
    pub(crate) fn enable(&self, target: &ResolvedTarget) -> Result<(), Error> {
        self.run(target, FixedCommand::Enable)?;
        (self.pohunek_state(target)? == PohunekState::Enabled)
            .then_some(())
            .ok_or(Error::InvalidHermesState)
    }

    /// Disables only the fixed Pohunek plugin and verifies `disabled` exactly.
    ///
    /// # Errors
    ///
    /// Returns a payload-free command or state error when Hermes reports
    /// `enabled`, `not enabled`, or an invalid record after the command.
    pub(crate) fn disable(&self, target: &ResolvedTarget) -> Result<(), Error> {
        self.run(target, FixedCommand::Disable)?;
        (self.pohunek_state(target)? == PohunekState::Disabled)
            .then_some(())
            .ok_or(Error::InvalidHermesState)
    }

    /// Validates one complete staged plugin using the sibling pinned Python.
    ///
    /// # Errors
    ///
    /// Returns a payload-free staged-validation error for unsafe paths,
    /// unsupported assets, imports, policy bytes, or generated skill metadata.
    pub(crate) fn validate_staged(
        &self,
        target: &ResolvedTarget,
        staged_plugin: &Path,
        staged_policy: &Path,
    ) -> Result<(), Error> {
        self.verify_version(target)?;
        let (staged_plugin, staged_policy) = validate_stage_paths(
            target,
            staged_plugin,
            staged_policy,
            Uid::effective().as_raw(),
        )?;
        let runtime = self.staged_runtime()?;
        let mut command = Command::new(runtime);
        command
            .args(["-I", "-c", STAGED_VALIDATOR_SCRIPT, "--"])
            .arg(staged_plugin)
            .arg(staged_policy);
        self.execute(command, ChildEnvironment::Isolated)
            .map(|_| ())
            .map_err(|_| Error::StagedValidation)
    }

    /// Probes the exact installed plugin registration without Hermes profile state.
    ///
    /// # Errors
    ///
    /// Returns a redacted probe failure for unsafe managed paths, malformed
    /// output, registration mismatch, or a bounded subprocess failure.
    pub(crate) fn probe_installed(
        &self,
        target: &ResolvedTarget,
        policy_path: &Path,
    ) -> Result<InstalledProbe, Error> {
        let (plugin, policy) = validate_installed_probe_paths(target, policy_path)?;
        let runtime = self.staged_runtime()?;
        let mut command = Command::new(runtime);
        command
            .args(["-I", "-c", INSTALLED_PROBE_SCRIPT, "--"])
            .arg(plugin)
            .arg(policy);
        let output = self
            .execute(command, ChildEnvironment::InstalledProbe)
            .map_err(|_| Error::InstalledProbe)?;
        parse_installed_probe(&output.stdout)
    }

    fn staged_runtime(&self) -> Result<PathBuf, Error> {
        let executable_parent = self
            .executable
            .parent()
            .ok_or(Error::InvalidHermesRuntime)?;
        let installation_root = executable_parent
            .parent()
            .and_then(Path::parent)
            .ok_or(Error::InvalidHermesRuntime)?;
        let installation_root =
            fs::canonicalize(installation_root).map_err(|_| Error::InvalidHermesRuntime)?;
        let requested = executable_parent.join("python3");
        let runtime = fs::canonicalize(&requested).map_err(|_| Error::InvalidHermesRuntime)?;
        if !runtime.starts_with(&installation_root) {
            return Err(Error::InvalidHermesRuntime);
        }
        validate_runtime(&runtime, Uid::effective().as_raw())?;
        // Spawn through the sibling path so Python observes `pyvenv.cfg` and
        // the pinned venv site-packages. The canonical target above is used
        // only for containment, ownership, mode, and effective-X validation.
        Ok(requested)
    }

    fn run(&self, target: &ResolvedTarget, fixed: FixedCommand) -> Result<ProcessOutput, Error> {
        let mut command = Command::new(&self.executable);
        configure_target(&mut command, target);
        command.args(fixed.args());
        self.execute(command, ChildEnvironment::Hermes(target_home(target)?))
    }

    fn execute(
        &self,
        mut command: Command,
        environment: ChildEnvironment<'_>,
    ) -> Result<ProcessOutput, Error> {
        let deadline = Instant::now() + self.timeout;
        command
            .current_dir(Path::new("/"))
            .env_clear()
            .env("NO_COLOR", NO_COLOR_VALUE)
            .env("TERM", RUNNER_TERM)
            .env("LANG", RUNNER_LOCALE)
            .env("PYTHONDONTWRITEBYTECODE", NO_COLOR_VALUE)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        match environment {
            ChildEnvironment::Isolated => {}
            ChildEnvironment::Hermes(hermes_home) => {
                command.env("HERMES_HOME", hermes_home);
            }
            ChildEnvironment::InstalledProbe => {
                for key in PROBE_ENVIRONMENT_KEYS {
                    if let Some(value) = absolute_environment_value(key) {
                        command.env(key, value);
                    }
                }
            }
        }
        configure_child(&mut command, fallback_fd_limit());

        let mut child = command.spawn().map_err(|_| Error::HermesCommand)?;
        let stdout = child.stdout.take().ok_or(Error::HermesCommand)?;
        let stderr = child.stderr.take().ok_or(Error::HermesCommand)?;
        let stdout_reader = spawn_bounded_reader_stdout(stdout, self.stream_limit);
        let stderr_reader = spawn_bounded_reader_stderr(stderr, self.stream_limit);
        let status = wait_for_child(&mut child, deadline);
        let output = receive_readers(&stdout_reader, &stderr_reader, deadline);

        let status = status?;
        let (stdout, stderr) = output?;
        if stdout.overflow || stderr.overflow {
            return Err(Error::HermesOutputLimit);
        }
        if !status.success() {
            return Err(Error::HermesCommand);
        }
        Ok(ProcessOutput {
            stdout: stdout.bytes,
        })
    }
}

fn pinned_version_line_matches(line: &str) -> bool {
    line.strip_prefix(PINNED_HERMES_VERSION_LINE)
        .is_some_and(|suffix| {
            suffix.is_empty() || suffix.starts_with(HERMES_VERSION_METADATA_DELIMITER)
        })
}

/// The exact environment contract for one fixed child process family.
#[derive(Debug, Clone, Copy)]
enum ChildEnvironment<'a> {
    /// Stage validation imports managed files without user path roots.
    Isolated,
    /// Hermes commands receive only their explicitly resolved profile home.
    Hermes(&'a Path),
    /// Installed registration probes forward only Pohunek's non-secret path roots.
    InstalledProbe,
}

fn absolute_environment_value(key: &str) -> Option<std::ffi::OsString> {
    let value = std::env::var_os(key)?;
    (!value.is_empty() && Path::new(&value).is_absolute()).then_some(value)
}

/// Bounded findings from one installed plugin registration probe.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct InstalledProbe {
    /// Whether the fixed probe reached its complete result boundary.
    pub(crate) probe_complete: bool,
    /// Payload-free fixed phase for a failed probe, or zero after completion.
    pub(crate) failure_phase: u8,
    /// Payload-free operating-system error number, or zero when unavailable.
    pub(crate) failure_errno: u8,
    /// Number of tools reported by the fake supported context.
    pub(crate) tool_count: u8,
    /// Whether the access-mode tool sequence exactly matched the policy.
    pub(crate) tools_ok: bool,
    /// Number of generated skills reported by the fake supported context.
    pub(crate) skill_count: u8,
    /// Whether the generated skill name and exact path matched.
    pub(crate) skill_ok: bool,
    /// Number of managed hooks reported by the fake supported context.
    pub(crate) hook_count: u8,
    /// Whether the managed hook sequence exactly matched the contract.
    pub(crate) hooks_ok: bool,
    /// Whether the plugin completed registration in its ready state.
    pub(crate) integration_ready: bool,
    /// Whether hooks contain no subprocess integration.
    pub(crate) hook_no_subprocess: bool,
    /// Whether hooks contain no `AF_INET` or `AF_INET6` integration.
    pub(crate) hook_no_network: bool,
    /// Whether hooks contain no Hermes database or profile-state integration.
    pub(crate) hook_no_database: bool,
    /// Whether every hook swallows a forced local socket failure.
    pub(crate) forced_socket_failure_swallowed: bool,
    /// Maximum measured callback batch latency in milliseconds.
    pub(crate) hook_latency_ms: u32,
}

impl super::lifecycle::HermesControl for HermesRunner {
    fn validate_staged(
        &mut self,
        target: &ResolvedTarget,
        staged_root: &Path,
        staged_policy: &Path,
    ) -> Result<(), Error> {
        HermesRunner::validate_staged(self, target, staged_root, staged_policy)
    }

    fn is_enabled(&mut self, target: &ResolvedTarget) -> Result<bool, Error> {
        HermesRunner::is_enabled(self, target)
    }

    fn enable(&mut self, target: &ResolvedTarget) -> Result<(), Error> {
        HermesRunner::enable(self, target)
    }

    fn disable(&mut self, target: &ResolvedTarget) -> Result<(), Error> {
        HermesRunner::disable(self, target)
    }
}

/// The exhaustive commands allowed to reach the Hermes subprocess.
#[derive(Debug, Clone, Copy)]
enum FixedCommand {
    Version,
    List,
    Enable,
    Disable,
}

impl FixedCommand {
    fn args(self) -> &'static [&'static str] {
        match self {
            Self::Version => &["--version"],
            Self::List => &["plugins", "list", "--json"],
            Self::Enable => &["plugins", "enable", "pohunek", "--no-allow-tool-override"],
            Self::Disable => &["plugins", "disable", "pohunek"],
        }
    }
}

struct ProcessOutput {
    stdout: Vec<u8>,
}

struct BoundedOutput {
    bytes: Vec<u8>,
    overflow: bool,
}

#[derive(Deserialize)]
struct PluginRecord {
    name: String,
    status: String,
    source: String,
}

fn parse_plugin_state(bytes: &[u8]) -> Result<PohunekState, Error> {
    let mut documents =
        serde_json::Deserializer::from_slice(bytes).into_iter::<Vec<PluginRecord>>();
    let records = documents
        .next()
        .transpose()
        .map_err(|_| Error::InvalidHermesState)?
        .ok_or(Error::InvalidHermesState)?;
    if documents.next().is_some() {
        return Err(Error::InvalidHermesState);
    }

    let mut records = records
        .into_iter()
        .filter(|record| record.name == POHUNEK_PLUGIN_NAME);
    let record = records.next().ok_or(Error::InvalidHermesState)?;
    if records.next().is_some() || record.source != "user" {
        return Err(Error::InvalidHermesState);
    }
    match record.status.as_str() {
        "enabled" => Ok(PohunekState::Enabled),
        "disabled" => Ok(PohunekState::Disabled),
        "not enabled" => Ok(PohunekState::NotEnabled),
        _ => Err(Error::InvalidHermesState),
    }
}

fn parse_installed_probe(bytes: &[u8]) -> Result<InstalledProbe, Error> {
    serde_json::from_slice(bytes).map_err(|_| Error::InstalledProbe)
}

fn validate_installed_probe_paths(
    target: &ResolvedTarget,
    policy_path: &Path,
) -> Result<(PathBuf, PathBuf), Error> {
    if !target.hermes_home().is_absolute()
        || !target.plugin_root().is_absolute()
        || !policy_path.is_absolute()
        || !target.plugin_root().starts_with(target.hermes_home())
        || policy_path.starts_with(target.hermes_home())
        || policy_path.starts_with(target.plugin_root())
    {
        return Err(Error::InstalledProbe);
    }
    reject_probe_symlinks(target.plugin_root())?;
    reject_probe_symlinks(policy_path)?;
    let plugin = fs::canonicalize(target.plugin_root()).map_err(|_| Error::InstalledProbe)?;
    let policy = fs::canonicalize(policy_path).map_err(|_| Error::InstalledProbe)?;
    if plugin != target.plugin_root() || policy != policy_path {
        return Err(Error::InstalledProbe);
    }
    validate_private_probe_path(&plugin, true)?;
    for relative in ["skills", "skills/pohunek"] {
        let path = plugin.join(relative);
        reject_probe_symlinks(&path)?;
        validate_private_probe_path(&path, true)?;
    }
    validate_private_probe_path(&policy, false)?;
    validate_private_probe_ancestors(&policy)?;
    for relative in [
        "__init__.py",
        "hooks.py",
        "tools.py",
        "cli.py",
        "policy.py",
        "redact.py",
        "skills/pohunek/SKILL.md",
    ] {
        let path = plugin.join(relative);
        reject_probe_symlinks(&path)?;
        validate_private_probe_path(&path, false)?;
    }
    Ok((plugin, policy))
}

fn reject_probe_symlinks(path: &Path) -> Result<(), Error> {
    let mut current = PathBuf::new();
    for component in path.components() {
        current.push(component.as_os_str());
        let metadata = fs::symlink_metadata(&current).map_err(|_| Error::InstalledProbe)?;
        if metadata.file_type().is_symlink() {
            return Err(Error::InstalledProbe);
        }
    }
    Ok(())
}

fn validate_private_probe_path(path: &Path, directory: bool) -> Result<(), Error> {
    let metadata = fs::symlink_metadata(path).map_err(|_| Error::InstalledProbe)?;
    if metadata.file_type().is_symlink()
        || metadata.uid() != Uid::effective().as_raw()
        || metadata.permissions().mode() & 0o777
            != if directory {
                PRIVATE_DIRECTORY_MODE
            } else {
                PRIVATE_FILE_MODE
            }
        || (directory && !metadata.is_dir())
        || (!directory && !metadata.is_file())
    {
        return Err(Error::InstalledProbe);
    }
    Ok(())
}

fn validate_private_probe_ancestors(path: &Path) -> Result<(), Error> {
    let uid = Uid::effective().as_raw();
    let root_uid = fs::metadata(Path::new("/"))
        .map(|metadata| metadata.uid())
        .map_err(|_| Error::InstalledProbe)?;
    for ancestor in path.parent().ok_or(Error::InstalledProbe)?.ancestors() {
        let metadata = fs::metadata(ancestor).map_err(|_| Error::InstalledProbe)?;
        let mode = metadata.permissions().mode();
        let shared_sticky_directory = metadata.is_dir()
            && metadata.uid() == root_uid
            && mode & STICKY_BIT != 0
            && mode & UNSAFE_WRITE_BITS != 0;
        if !metadata.is_dir()
            || (metadata.uid() != uid && metadata.uid() != root_uid)
            || (mode & UNSAFE_WRITE_BITS != 0 && !shared_sticky_directory)
        {
            return Err(Error::InstalledProbe);
        }
    }
    Ok(())
}

fn validate_stage_paths(
    target: &ResolvedTarget,
    staged_plugin: &Path,
    staged_policy: &Path,
    uid: u32,
) -> Result<(PathBuf, PathBuf), Error> {
    if !staged_plugin.is_absolute() || !staged_policy.is_absolute() {
        return Err(Error::StagedValidation);
    }
    reject_stage_symlinks(staged_plugin)?;
    reject_stage_symlinks(staged_policy)?;
    let staged_plugin = fs::canonicalize(staged_plugin).map_err(|_| Error::StagedValidation)?;
    let staged_policy = fs::canonicalize(staged_policy).map_err(|_| Error::StagedValidation)?;
    let expected_parent = target
        .plugin_root()
        .parent()
        .ok_or(Error::StagedValidation)?;
    let expected_parent = fs::canonicalize(expected_parent).map_err(|_| Error::StagedValidation)?;
    let stage_name = staged_plugin
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or(Error::StagedValidation)?;
    if staged_plugin.parent() != Some(expected_parent.as_path())
        || !stage_name.starts_with(STAGE_PREFIX)
        || staged_policy.starts_with(&staged_plugin)
    {
        return Err(Error::StagedValidation);
    }
    validate_private_stage(&staged_plugin, true, PRIVATE_DIRECTORY_MODE, uid)?;
    validate_private_stage(&staged_policy, false, PRIVATE_FILE_MODE, uid)?;
    Ok((staged_plugin, staged_policy))
}

fn reject_stage_symlinks(path: &Path) -> Result<(), Error> {
    let mut current = PathBuf::new();
    for component in path.components() {
        current.push(component.as_os_str());
        let metadata = fs::symlink_metadata(&current).map_err(|_| Error::StagedValidation)?;
        if metadata.file_type().is_symlink() {
            return Err(Error::StagedValidation);
        }
    }
    Ok(())
}

fn validate_private_stage(path: &Path, directory: bool, mode: u32, uid: u32) -> Result<(), Error> {
    let metadata = fs::metadata(path).map_err(|_| Error::StagedValidation)?;
    if metadata.uid() != uid
        || metadata.permissions().mode() & 0o777 != mode
        || (directory && !metadata.is_dir())
        || (!directory && !metadata.is_file())
    {
        return Err(Error::StagedValidation);
    }
    validate_safe_ancestors(path, uid, Error::StagedValidation)
}

fn validate_runtime(path: &Path, uid: u32) -> Result<(), Error> {
    let metadata = fs::metadata(path).map_err(|_| Error::InvalidHermesRuntime)?;
    let root_uid = filesystem_root_uid(Error::InvalidHermesRuntime)?;
    if !metadata.is_file()
        || (metadata.uid() != root_uid && metadata.uid() != uid)
        || metadata.permissions().mode() & UNSAFE_WRITE_BITS != 0
        || eaccess(path, AccessFlags::X_OK).is_err()
    {
        return Err(Error::InvalidHermesRuntime);
    }
    validate_safe_ancestors(path, uid, Error::InvalidHermesRuntime)
}

fn validate_safe_ancestors(path: &Path, uid: u32, error: Error) -> Result<(), Error> {
    let root_uid = filesystem_root_uid(error.clone())?;
    for ancestor in path.ancestors() {
        let metadata = fs::metadata(ancestor).map_err(|_| error.clone())?;
        if !safe_ancestor(
            metadata.uid(),
            metadata.permissions().mode(),
            metadata.is_dir(),
            uid,
            root_uid,
        ) {
            return Err(error);
        }
    }
    Ok(())
}

fn safe_ancestor(owner: u32, mode: u32, directory: bool, uid: u32, root_uid: u32) -> bool {
    let allowed_owner = owner == uid || owner == root_uid;
    let root_owned_sticky_shared =
        directory && owner == root_uid && mode & STICKY_BIT != 0 && mode & UNSAFE_WRITE_BITS != 0;
    allowed_owner && (mode & UNSAFE_WRITE_BITS == 0 || root_owned_sticky_shared)
}

fn filesystem_root_uid(error: Error) -> Result<u32, Error> {
    fs::metadata(Path::new("/"))
        .map(|metadata| metadata.uid())
        .map_err(|_| error)
}

fn validate_executable(path: &Path) -> Result<PathBuf, Error> {
    if !path.is_absolute() {
        return Err(Error::InvalidHermesExecutable);
    }
    reject_symlink_components(path)?;
    let canonical = fs::canonicalize(path).map_err(|_| Error::InvalidHermesExecutable)?;
    let metadata = fs::metadata(&canonical).map_err(|_| Error::InvalidHermesExecutable)?;
    if !metadata.is_file()
        || metadata.uid() != Uid::effective().as_raw()
        || metadata.permissions().mode() & UNSAFE_WRITE_BITS != 0
        || eaccess(&canonical, AccessFlags::X_OK).is_err()
    {
        return Err(Error::InvalidHermesExecutable);
    }
    validate_safe_ancestors(
        &canonical,
        Uid::effective().as_raw(),
        Error::InvalidHermesExecutable,
    )?;
    Ok(canonical)
}

fn reject_symlink_components(path: &Path) -> Result<(), Error> {
    let mut current = PathBuf::new();
    for component in path.components() {
        current.push(component.as_os_str());
        let metadata =
            fs::symlink_metadata(&current).map_err(|_| Error::InvalidHermesExecutable)?;
        if metadata.file_type().is_symlink() {
            return Err(Error::InvalidHermesExecutable);
        }
    }
    Ok(())
}

fn configure_target(command: &mut Command, target: &ResolvedTarget) {
    if let HermesInvocation::Profile(profile) = target.invocation() {
        if profile.as_str() != "default" {
            command.arg("--profile").arg(profile.as_str());
        }
    }
}

fn target_home(target: &ResolvedTarget) -> Result<&Path, Error> {
    match target.invocation() {
        HermesInvocation::CustomHome => Ok(target.hermes_home()),
        HermesInvocation::Profile(profile) if profile.as_str() == "default" => {
            Ok(target.hermes_home())
        }
        HermesInvocation::Profile(_) => target
            .hermes_home()
            .parent()
            .and_then(Path::parent)
            .ok_or(Error::UnsafeTarget),
    }
}

/// Captures the descriptor bound before fork, outside the constrained child.
#[expect(
    unsafe_code,
    reason = "sysconf is the libc boundary for the process open-file limit"
)]
fn fallback_fd_limit() -> libc::c_int {
    // SAFETY: `_SC_OPEN_MAX` takes no pointers and only queries process limits.
    let limit = unsafe { libc::sysconf(libc::_SC_OPEN_MAX) };
    libc::c_int::try_from(limit)
        .ok()
        .filter(|limit| *limit > FIRST_INHERITED_FD)
        .unwrap_or(MINIMUM_FD_LIMIT)
}

fn configure_child(command: &mut Command, fallback_fd_limit: libc::c_int) {
    use std::os::unix::process::CommandExt as _;

    // `pgid := child pid`, so timeout cleanup can terminate descendants too.
    command.process_group(0);
    // `pre_exec` runs after stdio has been duplicated to descriptors 0, 1,
    // and 2. Only async-signal-safe raw syscalls execute in the child.
    #[expect(
        unsafe_code,
        reason = "pre_exec is required to close inherited descriptors after stdio setup"
    )]
    // SAFETY: the closure calls only raw async-signal-safe descriptor syscalls.
    unsafe {
        command.pre_exec(move || {
            // This also closes std::process's CLOEXEC exec-error pipe, so exec
            // failures become non-zero child exits; both map to `HermesCommand`.
            close_inherited_fds(fallback_fd_limit);
            Ok(())
        });
    }
}

/// Closes every non-stdio descriptor using only async-signal-safe syscalls.
///
/// Current Linux kernels support one `close_range` call. The bounded `close`
/// loop is the compatibility fallback for older kernels or restricted syscall
/// policies; its upper bound was captured with `sysconf` before fork.
#[expect(
    unsafe_code,
    reason = "raw close_range and close calls are required inside pre_exec"
)]
fn close_inherited_fds(fallback_fd_limit: libc::c_int) {
    let first = FIRST_INHERITED_FD as libc::c_uint;
    // SAFETY: close_range receives scalar descriptor bounds and no pointers.
    let result = unsafe { libc::syscall(libc::SYS_close_range, first, libc::c_uint::MAX, 0_u32) };
    if result == 0 {
        return;
    }
    for descriptor in FIRST_INHERITED_FD..fallback_fd_limit {
        // SAFETY: close accepts any integer descriptor. EBADF and EINTR are
        // harmless here because the child is about to exec or abort.
        unsafe {
            libc::close(descriptor);
        }
    }
}

fn wait_for_child(child: &mut Child, deadline: Instant) -> Result<ExitStatus, Error> {
    loop {
        match child_exited(child) {
            Ok(true) => {
                kill_process_group(child.id());
                return child.wait().map_err(|_| Error::HermesCommand);
            }
            Ok(false) if Instant::now() >= deadline => {
                terminate_process_group(child)?;
                return Err(Error::HermesTimeout);
            }
            Ok(false) => thread::sleep(
                PROCESS_POLL_INTERVAL.min(deadline.saturating_duration_since(Instant::now())),
            ),
            Err(_) => {
                terminate_process_group(child)?;
                return Err(Error::HermesCommand);
            }
        }
    }
}

/// Observes exit without reaping, preserving the process-group ID until cleanup.
#[expect(
    unsafe_code,
    reason = "waitid with WNOWAIT is required to avoid a process-group ID reuse race"
)]
fn child_exited(child: &Child) -> Result<bool, Error> {
    let mut information = std::mem::MaybeUninit::<libc::siginfo_t>::zeroed();
    // SAFETY: the output points to initialized storage, `child.id()` names our
    // direct child, and WNOWAIT deliberately leaves that child waitable.
    let result = unsafe {
        libc::waitid(
            libc::P_PID,
            child.id(),
            information.as_mut_ptr(),
            libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
        )
    };
    if result != 0 {
        return Err(Error::HermesCommand);
    }
    // SAFETY: successful waitid initializes `siginfo_t`, including a zero PID
    // when WNOHANG found no state change.
    let pid = unsafe { information.assume_init().si_pid() };
    Ok(pid != 0)
}

/// Kills the still-live child group and always reaps the direct child.
fn terminate_process_group(child: &mut Child) -> Result<(), Error> {
    kill_process_group(child.id());
    // The group kill includes the direct child. `Child::kill` is a safe
    // fallback if a platform or policy rejected negative-PID signalling.
    let _ = child.kill();
    child.wait().map(|_| ()).map_err(|_| Error::HermesCommand)
}

/// Kills the group while its direct child still reserves the group identifier.
#[expect(
    unsafe_code,
    reason = "libc kill(2) is the existing dependency API for a negative process-group PID"
)]
fn kill_process_group(child_id: u32) {
    #[expect(
        clippy::cast_possible_wrap,
        reason = "Linux PIDs are bounded below i32::MAX, so conversion to pid_t cannot wrap"
    )]
    let process_group = child_id as libc::pid_t;
    // SAFETY: `process_group(0)` made the direct child's PID its process-group
    // identifier. WNOWAIT or a still-running direct child reserves the PID, so
    // it cannot be recycled; negative PID targets precisely that process group.
    unsafe {
        let _ = libc::kill(-process_group, libc::SIGKILL);
    }
}

fn spawn_bounded_reader(
    reader: impl Read + Send + 'static,
    limit: usize,
) -> Receiver<Result<BoundedOutput, Error>> {
    let (sender, receiver) = mpsc::sync_channel(1);
    thread::spawn(move || {
        let _ = sender.send(read_bounded(reader, limit));
    });
    receiver
}

fn spawn_bounded_reader_stdout(
    reader: ChildStdout,
    limit: usize,
) -> Receiver<Result<BoundedOutput, Error>> {
    spawn_bounded_reader(reader, limit)
}

fn spawn_bounded_reader_stderr(
    reader: ChildStderr,
    limit: usize,
) -> Receiver<Result<BoundedOutput, Error>> {
    spawn_bounded_reader(reader, limit)
}

fn read_bounded(mut reader: impl Read, limit: usize) -> Result<BoundedOutput, Error> {
    let mut bytes = Vec::with_capacity(limit);
    let mut buffer = [0_u8; 8 * 1024];
    let mut overflow = false;
    loop {
        let count = reader.read(&mut buffer).map_err(|_| Error::HermesCommand)?;
        if count == 0 {
            return Ok(BoundedOutput { bytes, overflow });
        }
        let available = limit.saturating_sub(bytes.len());
        let retained = count.min(available);
        bytes.extend_from_slice(&buffer[..retained]);
        overflow |= retained != count;
    }
}

fn receive_readers(
    stdout: &Receiver<Result<BoundedOutput, Error>>,
    stderr: &Receiver<Result<BoundedOutput, Error>>,
    deadline: Instant,
) -> Result<(BoundedOutput, BoundedOutput), Error> {
    let stdout = receive_reader(stdout, deadline)?;
    let stderr = receive_reader(stderr, deadline)?;
    Ok((stdout?, stderr?))
}

fn receive_reader(
    receiver: &Receiver<Result<BoundedOutput, Error>>,
    deadline: Instant,
) -> Result<Result<BoundedOutput, Error>, Error> {
    match receiver.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
        Ok(output) => Ok(output),
        Err(RecvTimeoutError::Timeout) => Err(Error::HermesTimeout),
        Err(RecvTimeoutError::Disconnected) => Err(Error::HermesCommand),
    }
}

#[cfg(test)]
mod tests {
    use std::fs::OpenOptions;
    use std::os::fd::AsRawFd as _;
    use std::os::unix::fs::symlink;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Mutex, OnceLock};

    use super::*;
    use crate::hermes_integration::assets;
    use crate::hermes_integration::target::{ProfileName, TargetContext, TargetSelection};

    static NEXT_DIR: AtomicUsize = AtomicUsize::new(0);
    static ENV_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

    struct Fixture(PathBuf);

    impl Drop for Fixture {
        fn drop(&mut self) {
            if let Err(error) = fs::remove_dir_all(&self.0) {
                assert_eq!(
                    error.kind(),
                    std::io::ErrorKind::NotFound,
                    "cleanup fixture"
                );
            }
        }
    }

    struct EnvironmentGuard {
        key: &'static str,
        previous: Option<std::ffi::OsString>,
    }

    struct StageFixture {
        root: Fixture,
        target: ResolvedTarget,
        runner: HermesRunner,
        plugin: PathBuf,
        policy: PathBuf,
        runtime: PathBuf,
    }

    impl EnvironmentGuard {
        fn set(key: &'static str, value: &str) -> Self {
            let previous = std::env::var_os(key);
            std::env::set_var(key, value);
            Self { key, previous }
        }

        fn remove(key: &'static str) -> Self {
            let previous = std::env::var_os(key);
            std::env::remove_var(key);
            Self { key, previous }
        }
    }

    impl Drop for EnvironmentGuard {
        fn drop(&mut self) {
            match self.previous.take() {
                Some(value) => std::env::set_var(self.key, value),
                None => std::env::remove_var(self.key),
            }
        }
    }

    fn fixture(tag: &str) -> Fixture {
        loop {
            let path = crate::hermes_integration::target::isolated_test_temp_root().join(format!(
                "pohunek-hermes-runner-{tag}-{}-{}",
                std::process::id(),
                NEXT_DIR.fetch_add(1, Ordering::Relaxed)
            ));
            match fs::create_dir(&path) {
                Ok(()) => {
                    fs::set_permissions(&path, fs::Permissions::from_mode(0o700))
                        .expect("private fixture");
                    return Fixture(path);
                }
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(error) => panic!("create fixture: {error}"),
            }
        }
    }

    fn target(root: &Path, selection: TargetSelection) -> ResolvedTarget {
        let hermes = root.join("hermes-home");
        let home = root.join("home");
        let workspace = root.join("workspace");
        for path in [&hermes, &home, &workspace] {
            fs::create_dir_all(path).expect("create target directory");
            fs::set_permissions(path, fs::Permissions::from_mode(0o700))
                .expect("private target directory");
        }
        if let TargetSelection::Profile(profile) = &selection {
            if profile.as_str() != "default" {
                let profile_root = hermes.join("profiles").join(profile.as_str());
                fs::create_dir_all(&profile_root).expect("create profile target");
                fs::set_permissions(&profile_root, fs::Permissions::from_mode(0o700))
                    .expect("private profile target");
            }
        }
        TargetContext::new(hermes, home, vec![workspace])
            .expect("target context")
            .resolve(selection)
            .expect("resolved target")
    }

    fn script(root: &Path, body: &str) -> PathBuf {
        let path = root.join("hermes");
        fs::write(&path, format!("#!/bin/sh\nset -eu\n{body}\n")).expect("write script");
        fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).expect("executable script");
        path
    }

    fn runner(root: &Path, body: &str) -> HermesRunner {
        HermesRunner::new(&script(root, body)).expect("validated controlled executable")
    }

    fn mark_plugin_present(target: &ResolvedTarget) {
        fs::create_dir_all(target.plugin_root()).expect("create fixed plugin root");
        fs::set_permissions(
            target.plugin_root(),
            fs::Permissions::from_mode(PRIVATE_DIRECTORY_MODE),
        )
        .expect("private plugin root");
    }

    fn list(status: &str, source: &str) -> String {
        format!(
            "printf '%s\\n' '[{{\"name\":\"pohunek\",\"status\":\"{status}\",\"source\":\"{source}\",\"extra\":true}}]'"
        )
    }

    fn stage_fixture(tag: &str) -> StageFixture {
        let root = fixture(tag);
        let selected = target(&root.0, TargetSelection::Profile(ProfileName::default()));
        let plugin_parent = selected.plugin_root().parent().expect("plugin parent");
        fs::create_dir_all(plugin_parent).expect("create plugin parent");
        let plugin = plugin_parent.join(format!("{STAGE_PREFIX}test"));
        fs::create_dir(&plugin).expect("create plugin stage");
        fs::set_permissions(&plugin, fs::Permissions::from_mode(PRIVATE_DIRECTORY_MODE))
            .expect("private plugin stage");

        let config = root.0.join("config");
        fs::create_dir(&config).expect("create config root");
        fs::set_permissions(&config, fs::Permissions::from_mode(PRIVATE_DIRECTORY_MODE))
            .expect("private config root");
        let final_policy = config.join("policy.json");
        let policy = config.join(".pohunek-policy-stage-test");
        let cli = write_executable(&root.0.join("pohunek"), "exit 0");
        let policy_bytes = serde_json::to_vec(&serde_json::json!({
            "schema_version": 1,
            "pohunek_cli": cli,
            "protocol_min": 2,
            "protocol_max": 2,
            "access_mode": "full",
            "allowed_hosts": ["local"],
            "tool_timeout_ms": 1_000,
            "request_timeout_ms": 500,
            "max_output_bytes": 65_536,
            "max_screen_bytes": 32_768,
            "max_concurrency": 2
        }))
        .expect("policy JSON");
        fs::write(&policy, policy_bytes).expect("write staged policy");
        fs::set_permissions(&policy, fs::Permissions::from_mode(PRIVATE_FILE_MODE))
            .expect("private staged policy");

        let rendered = assets::render(&final_policy).expect("render plugin assets");
        for asset in &rendered {
            let destination = plugin.join(asset.path());
            create_private_parent(&plugin, destination.parent().expect("asset parent"));
            fs::write(&destination, asset.bytes()).expect("write staged asset");
            fs::set_permissions(&destination, fs::Permissions::from_mode(PRIVATE_FILE_MODE))
                .expect("private staged asset");
        }
        let ownership = assets::ownership(selected.hermes_home(), &final_policy, &rendered)
            .expect("stage ownership");
        let marker = plugin.join(assets::MARKER_NAME);
        fs::write(
            &marker,
            assets::marker_bytes(&ownership).expect("marker JSON"),
        )
        .expect("write stage marker");
        fs::set_permissions(&marker, fs::Permissions::from_mode(PRIVATE_FILE_MODE))
            .expect("private marker");

        let installation_bin = root.0.join("installation").join("venv").join("bin");
        create_private_parent(&root.0, &installation_bin);
        let executable = write_executable(
            &installation_bin.join("hermes"),
            "test \"${1-}\" = '--version'\nprintf '%s\\n' 'Hermes Agent v0.20.0 (2026.8.3)'",
        );
        let runtime_target = root
            .0
            .join("installation")
            .join("python")
            .join("bin")
            .join("python3");
        create_private_parent(&root.0, runtime_target.parent().expect("runtime parent"));
        write_executable(&runtime_target, "exec /usr/bin/python3 \"$@\"");
        let runtime = installation_bin.join("python3");
        symlink("../../python/bin/python3", &runtime).expect("internal runtime symlink");
        let runner = HermesRunner::new(&executable).expect("stage runner");
        StageFixture {
            root,
            target: selected,
            runner,
            plugin,
            policy,
            runtime,
        }
    }

    fn create_private_parent(root: &Path, parent: &Path) {
        let relative = parent.strip_prefix(root).expect("contained asset parent");
        let mut current = root.to_owned();
        for component in relative.components() {
            current.push(component.as_os_str());
            if !current.exists() {
                fs::create_dir(&current).expect("create asset directory");
            }
            fs::set_permissions(&current, fs::Permissions::from_mode(PRIVATE_DIRECTORY_MODE))
                .expect("private asset directory");
        }
    }

    fn write_executable(path: &Path, body: &str) -> PathBuf {
        fs::write(path, format!("#!/bin/sh\nset -eu\n{body}\n")).expect("write executable");
        fs::set_permissions(path, fs::Permissions::from_mode(0o700)).expect("executable mode");
        path.to_owned()
    }

    #[test]
    fn target_environment_and_arguments_are_fixed_for_default_named_and_custom_targets() {
        let _environment = ENV_LOCK
            .get_or_init(|| Mutex::new(()))
            .lock()
            .expect("environment lock");
        let _leaked = EnvironmentGuard::set("POHUNEK_HERMES_RUNNER_TEST_LEAK", "secret");
        let root = fixture("targets");
        let log = root.0.join("log");
        let body = format!(
            "printf '%s|%s|%s|%s|%s|%s|%s|%s|%s\\n' \"$HERMES_HOME\" \"${{1-}}\" \"${{2-}}\" \"${{3-}}\" \"${{POHUNEK_HERMES_RUNNER_TEST_LEAK-absent}}\" \"$NO_COLOR\" \"$TERM\" \"$LANG\" \"$PYTHONDONTWRITEBYTECODE\" > '{}'\nprintf '%s\\n' 'Hermes Agent v0.20.0 (2026.8.3)'",
            log.display()
        );
        let cases = [
            (
                TargetSelection::Profile(ProfileName::default()),
                "--version",
                "",
                "",
            ),
            (
                TargetSelection::Profile(ProfileName::new("operator").expect("profile")),
                "--profile",
                "operator",
                "--version",
            ),
            (
                TargetSelection::CustomHome(root.0.join("custom")),
                "--version",
                "",
                "",
            ),
        ];
        for (selection, first, second, third) in cases {
            let selected = target(&root.0, selection);
            runner(&root.0, &body)
                .verify_version(&selected)
                .expect("pinned version");
            let parts: Vec<_> = fs::read_to_string(&log)
                .expect("log")
                .trim()
                .split('|')
                .map(str::to_owned)
                .collect();
            assert_eq!(parts.len(), 9);
            assert_eq!(
                parts[0],
                target_home(&selected)
                    .expect("target home")
                    .display()
                    .to_string()
            );
            assert_eq!(parts[1], first);
            assert_eq!(parts[2], second);
            assert_eq!(parts[3], third);
            assert_eq!(parts[4], "absent");
            assert_eq!(
                parts[5..],
                [NO_COLOR_VALUE, RUNNER_TERM, RUNNER_LOCALE, NO_COLOR_VALUE]
            );
        }
    }

    #[test]
    fn installed_probe_forwards_only_non_secret_path_roots() {
        let _environment = ENV_LOCK
            .get_or_init(|| Mutex::new(()))
            .lock()
            .expect("environment lock");
        let values = [
            ("HOME", "/controlled/home"),
            ("XDG_RUNTIME_DIR", "/controlled/runtime"),
            ("XDG_STATE_HOME", "/controlled/state"),
            ("XDG_CONFIG_HOME", "/controlled/config"),
            ("XDG_DATA_HOME", "/controlled/data"),
            ("XDG_CACHE_HOME", "/controlled/cache"),
            ("HTTPS_PROXY", "https://credential.invalid"),
            ("OPENAI_API_KEY", "secret"),
            ("POHUNEK_SOCKET_PATH", "/controlled/override.sock"),
            ("HERMES_HOME", "/controlled/ambient-hermes"),
        ];
        let _guards: Vec<_> = values
            .into_iter()
            .map(|(key, value)| EnvironmentGuard::set(key, value))
            .collect();
        let root = fixture("probe-environment");
        let fixed_runner = runner(&root.0, "exit 0");
        let probe = write_executable(
            &root.0.join("installed-probe"),
            "printf '%s|%s|%s|%s|%s|%s|%s|%s|%s|%s' \"$HOME\" \"$XDG_RUNTIME_DIR\" \"$XDG_STATE_HOME\" \"$XDG_CONFIG_HOME\" \"$XDG_DATA_HOME\" \"$XDG_CACHE_HOME\" \"${HTTPS_PROXY-absent}\" \"${OPENAI_API_KEY-absent}\" \"${POHUNEK_SOCKET_PATH-absent}\" \"${HERMES_HOME-absent}\"",
        );
        let command = Command::new(probe);
        let output = fixed_runner
            .execute(command, ChildEnvironment::InstalledProbe)
            .expect("installed probe environment");

        assert_eq!(
            String::from_utf8(output.stdout).expect("UTF-8 probe environment"),
            "/controlled/home|/controlled/runtime|/controlled/state|/controlled/config|/controlled/data|/controlled/cache|absent|absent|absent|absent"
        );
    }

    #[test]
    fn installed_probe_omits_missing_empty_and_relative_path_roots() {
        let _environment = ENV_LOCK
            .get_or_init(|| Mutex::new(()))
            .lock()
            .expect("environment lock");
        let _guards = [
            EnvironmentGuard::remove("HOME"),
            EnvironmentGuard::set("XDG_RUNTIME_DIR", ""),
            EnvironmentGuard::set("XDG_STATE_HOME", "relative-state"),
            EnvironmentGuard::remove("XDG_CONFIG_HOME"),
            EnvironmentGuard::set("XDG_DATA_HOME", "./relative-data"),
            EnvironmentGuard::set("XDG_CACHE_HOME", "../relative-cache"),
        ];
        let root = fixture("invalid-probe-environment");
        let fixed_runner = runner(&root.0, "exit 0");
        let probe = write_executable(
            &root.0.join("installed-probe"),
            "printf '%s|%s|%s|%s|%s|%s' \"${HOME:-absent}\" \"${XDG_RUNTIME_DIR:-absent}\" \"${XDG_STATE_HOME:-absent}\" \"${XDG_CONFIG_HOME:-absent}\" \"${XDG_DATA_HOME:-absent}\" \"${XDG_CACHE_HOME:-absent}\"",
        );
        let output = fixed_runner
            .execute(Command::new(probe), ChildEnvironment::InstalledProbe)
            .expect("invalid path roots remain omitted");

        assert_eq!(
            String::from_utf8(output.stdout).expect("UTF-8 probe environment"),
            "absent|absent|absent|absent|absent|absent"
        );
    }

    #[test]
    fn rejects_relative_symlink_non_executable_and_writable_hermes_executables() {
        let root = fixture("unsafe-executable");
        assert!(matches!(
            HermesRunner::new(Path::new("relative")),
            Err(Error::InvalidHermesExecutable)
        ));
        let plain = root.0.join("plain");
        fs::write(&plain, "plain").expect("plain file");
        fs::set_permissions(&plain, fs::Permissions::from_mode(0o600)).expect("plain mode");
        assert!(matches!(
            HermesRunner::new(&plain),
            Err(Error::InvalidHermesExecutable)
        ));
        let target = script(&root.0, "exit 0");
        let link = root.0.join("link");
        symlink(&target, &link).expect("controlled symlink");
        assert!(matches!(
            HermesRunner::new(&link),
            Err(Error::InvalidHermesExecutable)
        ));
        fs::set_permissions(&target, fs::Permissions::from_mode(0o722)).expect("unsafe mode");
        assert!(matches!(
            HermesRunner::new(&target),
            Err(Error::InvalidHermesExecutable)
        ));
    }

    #[test]
    fn verifies_the_pinned_version_with_optional_provenance_metadata() {
        let root = fixture("version");
        let selected = target(&root.0, TargetSelection::Profile(ProfileName::default()));
        runner(
            &root.0,
            "printf '%s\\n%s\\n' 'metadata' 'Hermes Agent v0.20.0 (2026.8.3)'",
        )
        .verify_version(&selected)
        .expect("supported Hermes version");
        runner(
            &root.0,
            "printf '%s\\n' 'Hermes Agent v0.20.0 (2026.8.3) · upstream 7aecab56 · local 3c27eb62'",
        )
        .verify_version(&selected)
        .expect("supported Hermes version with provenance");
        assert_eq!(
            runner(
                &root.0,
                "printf '%s\\n' 'Hermes Agent v0.20.0 (2026.8.3)-unexpected'",
            )
            .verify_version(&selected),
            Err(Error::UnsupportedHermes)
        );
        assert_eq!(
            runner(&root.0, "printf '%s\\n' 'Hermes Agent v0.20.1 (2026.8.3)'")
                .verify_version(&selected),
            Err(Error::UnsupportedHermes)
        );
        assert_eq!(
            runner(&root.0, "printf '\\377'").verify_version(&selected),
            Err(Error::UnsupportedHermes)
        );
    }

    #[test]
    fn parses_each_pinned_plugin_state_and_allows_unrelated_record_fields() {
        let root = fixture("states");
        let selected = target(&root.0, TargetSelection::Profile(ProfileName::default()));
        for (status, expected) in [
            ("enabled", PohunekState::Enabled),
            ("disabled", PohunekState::Disabled),
            ("not enabled", PohunekState::NotEnabled),
        ] {
            let current = runner(&root.0, &list(status, "user"))
                .pohunek_state(&selected)
                .expect("valid plugin state");
            assert_eq!(current, expected);
            assert_eq!(current.is_enabled(), status == "enabled");
        }
    }

    #[test]
    fn plugin_list_rejects_missing_duplicate_wrong_source_unknown_and_trailing_documents() {
        let root = fixture("list-invalid");
        let selected = target(&root.0, TargetSelection::Profile(ProfileName::default()));
        mark_plugin_present(&selected);
        let cases = [
            ("printf '%s' '[]'", Error::InvalidHermesState),
            ("printf '%s' '[{\"name\":\"pohunek\",\"status\":\"enabled\",\"source\":\"user\"},{\"name\":\"pohunek\",\"status\":\"enabled\",\"source\":\"user\"}]'", Error::InvalidHermesState),
            ("printf '%s' '[{\"name\":\"pohunek\",\"status\":\"enabled\",\"source\":\"builtin\"}]'", Error::InvalidHermesState),
            ("printf '%s' '[{\"name\":\"pohunek\",\"status\":\"unknown\",\"source\":\"user\"}]'", Error::InvalidHermesState),
            ("printf '%s' '[{\"name\":\"pohunek\",\"status\":\"enabled\",\"source\":\"user\"}] []'", Error::InvalidHermesState),
        ];
        for (body, expected) in cases {
            assert_eq!(runner(&root.0, body).is_enabled(&selected), Err(expected));
        }
    }

    #[test]
    fn fresh_target_is_disabled_without_list_but_existing_missing_plugin_fails() {
        let root = fixture("fresh-state");
        let selected = target(&root.0, TargetSelection::Profile(ProfileName::default()));
        let fresh_runner = runner(&root.0, "exit 91");
        assert!(!fresh_runner
            .is_enabled(&selected)
            .expect("fresh target disabled"));
        mark_plugin_present(&selected);
        assert_eq!(
            runner(&root.0, "printf '%s' '[]'").is_enabled(&selected),
            Err(Error::InvalidHermesState)
        );
    }

    #[test]
    fn enable_and_disable_use_only_fixed_argv_and_verify_exact_pinned_states() {
        let root = fixture("enable-disable");
        let selected = target(&root.0, TargetSelection::Profile(ProfileName::default()));
        mark_plugin_present(&selected);
        let state = root.0.join("state");
        fs::write(&state, "disabled").expect("initial state");
        let body = format!(
            "case \"$*\" in\n  'plugins enable pohunek --no-allow-tool-override') printf enabled > '{}' ;;&\n  'plugins disable pohunek') printf disabled > '{}' ;;&\n  'plugins list --json') state=$(cat '{}'); printf '[{{\"name\":\"pohunek\",\"status\":\"%s\",\"source\":\"user\"}}]' \"$state\" ;;&\n  *) exit 90 ;;&\nesac",
            state.display(),
            state.display(),
            state.display()
        )
        .replace(";;&", ";;");
        let state_runner = runner(&root.0, &body);
        state_runner.enable(&selected).expect("fixed enable");
        assert_eq!(
            fs::read_to_string(&state).expect("enabled state"),
            "enabled"
        );
        state_runner.disable(&selected).expect("fixed disable");
        assert_eq!(
            fs::read_to_string(&state).expect("disabled state"),
            "disabled"
        );

        assert_eq!(
            runner(&root.0, "exit 7").enable(&selected),
            Err(Error::HermesCommand)
        );
        assert_eq!(
            runner(&root.0, "exit 8").disable(&selected),
            Err(Error::HermesCommand)
        );
        assert_eq!(
            runner(&root.0, &list("disabled", "user")).enable(&selected),
            Err(Error::InvalidHermesState)
        );
        assert_eq!(
            runner(&root.0, &list("not enabled", "user")).disable(&selected),
            Err(Error::InvalidHermesState)
        );
    }

    #[test]
    #[expect(
        clippy::too_many_lines,
        reason = "one table-driven test covers every fixed staged-package contract"
    )]
    fn staged_validator_accepts_complete_stage_and_rejects_each_fixed_contract_violation() {
        let stage = stage_fixture("stage-valid");
        stage
            .runner
            .validate_staged(&stage.target, &stage.plugin, &stage.policy)
            .expect("valid isolated stage");

        let extra = stage_fixture("stage-manifest-extra");
        append_file(&extra.plugin.join("plugin.yaml"), "unexpected: true\n");
        assert_stage_error(&extra, Error::StagedValidation);

        let missing_field = stage_fixture("stage-manifest-missing-field");
        replace_file(
            &missing_field.plugin.join("plugin.yaml"),
            "description: Safe, policy-bounded Pohunek session tools and lifecycle reporting.\n",
            "",
        );
        assert_stage_error(&missing_field, Error::StagedValidation);

        let missing_tool = stage_fixture("stage-tool-missing");
        replace_file(
            &missing_tool.plugin.join("plugin.yaml"),
            "  - pohunek_session_remove\n",
            "",
        );
        assert_stage_error(&missing_tool, Error::StagedValidation);

        let duplicate_tool = stage_fixture("stage-tool-duplicate");
        replace_file(
            &duplicate_tool.plugin.join("plugin.yaml"),
            "  - pohunek_session_remove\n",
            "  - pohunek_hosts\n",
        );
        assert_stage_error(&duplicate_tool, Error::StagedValidation);

        let syntax = stage_fixture("stage-python-syntax");
        append_file(&syntax.plugin.join("redact.py"), "\nif :\n");
        assert_stage_error(&syntax, Error::StagedValidation);

        let import = stage_fixture("stage-python-import");
        append_file(
            &import.plugin.join("redact.py"),
            "\nimport pohunek_missing_validation_module\n",
        );
        assert_stage_error(&import, Error::StagedValidation);

        let escaped_asset = stage_fixture("stage-asset-symlink");
        let outside_source = escaped_asset.root.0.join("outside.py");
        fs::write(&outside_source, "value = 1\n").expect("write outside source");
        fs::set_permissions(
            &outside_source,
            fs::Permissions::from_mode(PRIVATE_FILE_MODE),
        )
        .expect("private outside source");
        let managed_source = escaped_asset.plugin.join("redact.py");
        fs::remove_file(&managed_source).expect("remove managed source");
        symlink(&outside_source, &managed_source).expect("escaping managed symlink");
        assert_stage_error(&escaped_asset, Error::StagedValidation);

        let invalid_policy = stage_fixture("stage-policy-invalid");
        fs::write(&invalid_policy.policy, "{}").expect("replace invalid policy");
        assert_stage_error(&invalid_policy, Error::StagedValidation);

        let policy_mode = stage_fixture("stage-policy-mode");
        fs::set_permissions(&policy_mode.policy, fs::Permissions::from_mode(0o640))
            .expect("unsafe policy mode");
        assert_stage_error(&policy_mode, Error::StagedValidation);

        let binding = stage_fixture("stage-policy-binding");
        let marker_path = binding.plugin.join(assets::MARKER_NAME);
        let mut marker: serde_json::Value = serde_json::from_slice(
            &fs::read(&marker_path).expect("read marker for binding mutation"),
        )
        .expect("marker JSON");
        marker["policy_path"] = serde_json::Value::String(
            binding
                .root
                .0
                .join("different-policy.json")
                .display()
                .to_string(),
        );
        fs::write(
            &marker_path,
            serde_json::to_vec(&marker).expect("mutated marker JSON"),
        )
        .expect("mutate marker binding");
        assert_stage_error(&binding, Error::StagedValidation);

        let missing_skill = stage_fixture("stage-skill-missing");
        fs::remove_file(
            missing_skill
                .plugin
                .join("skills")
                .join("pohunek")
                .join("SKILL.md"),
        )
        .expect("remove generated skill");
        assert_stage_error(&missing_skill, Error::StagedValidation);

        let missing_runtime = stage_fixture("stage-runtime-missing");
        fs::remove_file(&missing_runtime.runtime).expect("remove sibling runtime");
        assert_stage_error(&missing_runtime, Error::InvalidHermesRuntime);

        let unsafe_runtime = stage_fixture("stage-runtime-unsafe");
        fs::set_permissions(&unsafe_runtime.runtime, fs::Permissions::from_mode(0o722))
            .expect("unsafe runtime mode");
        assert_stage_error(&unsafe_runtime, Error::InvalidHermesRuntime);

        let escaped_runtime = stage_fixture("stage-runtime-symlink");
        fs::remove_file(&escaped_runtime.runtime).expect("remove sibling runtime");
        symlink("/usr/bin/python3", &escaped_runtime.runtime).expect("escaping runtime symlink");
        assert_stage_error(&escaped_runtime, Error::InvalidHermesRuntime);

        let relative = stage_fixture("stage-relative");
        assert_eq!(
            relative.runner.validate_staged(
                &relative.target,
                Path::new("relative"),
                &relative.policy
            ),
            Err(Error::StagedValidation)
        );
    }

    #[test]
    fn installed_probe_exercises_real_registration_and_local_hook_fallback() {
        let stage = stage_fixture("installed-probe");
        let final_policy = stage
            .policy
            .parent()
            .expect("policy parent")
            .join("policy.json");
        fs::write(
            &stage.policy,
            serde_json::to_vec(&serde_json::json!({
                "schema_version": 1,
                "pohunek_cli": stage.policy.parent().expect("policy parent").parent().expect("fixture root").join("pohunek"),
                "protocol_min": 2,
                "protocol_max": 2,
                "access_mode": "full",
                "allowed_hosts": ["local"],
                "tool_timeout_ms": 1_000,
                "request_timeout_ms": 500,
                "max_output_bytes": 65_536,
                "max_screen_bytes": 32_768,
                "max_concurrency": 2
            }))
            .expect("policy JSON"),
        )
        .expect("rewrite policy");
        let cli = final_policy
            .parent()
            .expect("config parent")
            .parent()
            .expect("fixture root")
            .join("pohunek");
        write_executable(
            &cli,
            "printf '%s\\n' '{\"protocol\":{\"minimum\":2,\"maximum\":2},\"ok\":{}}'",
        );
        fs::rename(&stage.plugin, stage.target.plugin_root()).expect("activate plugin fixture");
        fs::rename(&stage.policy, &final_policy).expect("activate policy fixture");

        let probe = stage
            .runner
            .probe_installed(&stage.target, &final_policy)
            .expect("installed probe");
        assert!(
            probe.probe_complete,
            "installed probe stopped in phase {} with errno {}",
            probe.failure_phase, probe.failure_errno
        );
        assert!(probe.tools_ok);
        assert_eq!(probe.tool_count, 16);
        assert!(probe.skill_ok);
        assert_eq!(probe.skill_count, 1);
        assert!(probe.hooks_ok);
        assert_eq!(probe.hook_count, 7);
        assert!(probe.integration_ready);
        assert!(probe.hook_no_subprocess);
        assert!(probe.hook_no_network);
        assert!(probe.hook_no_database);
        assert!(probe.forced_socket_failure_swallowed);
        assert!(probe.hook_latency_ms <= 1_000);
    }

    #[test]
    fn installed_probe_parser_rejects_unknown_duplicate_and_payload_documents() {
        for document in [
            b"{}".as_slice(),
            br#"{"tool_count":1,"tool_count":1}"#,
            br#"{"probe_complete":true,"failure_phase":0,"failure_errno":0,"tool_count":1,"tools_ok":true,"skill_count":1,"skill_ok":true,"hook_count":7,"hooks_ok":true,"integration_ready":true,"hook_no_subprocess":true,"hook_no_network":true,"hook_no_database":true,"forced_socket_failure_swallowed":true,"hook_latency_ms":0,"extra":true}"#,
            br#"{"probe_complete":true,"failure_phase":0,"failure_errno":0,"tool_count":1,"tools_ok":true,"skill_count":1,"skill_ok":true,"hook_count":7,"hooks_ok":true,"integration_ready":true,"hook_no_subprocess":true,"hook_no_network":true,"hook_no_database":true,"forced_socket_failure_swallowed":true,"hook_latency_ms":0} {}"#,
        ] {
            assert_eq!(parse_installed_probe(document), Err(Error::InstalledProbe));
        }
    }

    #[test]
    fn installed_probe_rejects_unsafe_managed_intermediate_and_policy_parents() {
        for (relative, mode) in [("skills", 0o770), ("skills/pohunek", 0o777)] {
            let stage = stage_fixture("probe-intermediate-permissions");
            let policy = activate_stage_fixture(&stage);
            fs::set_permissions(
                stage.target.plugin_root().join(relative),
                fs::Permissions::from_mode(mode),
            )
            .expect("unsafe intermediate mode");
            assert_eq!(
                stage.runner.probe_installed(&stage.target, &policy),
                Err(Error::InstalledProbe),
                "{relative}"
            );
        }

        let stage = stage_fixture("probe-policy-parent-permissions");
        let policy = activate_stage_fixture(&stage);
        fs::set_permissions(
            policy.parent().expect("policy parent"),
            fs::Permissions::from_mode(0o777),
        )
        .expect("unsafe policy parent");
        assert_eq!(
            stage.runner.probe_installed(&stage.target, &policy),
            Err(Error::InstalledProbe)
        );
    }

    fn activate_stage_fixture(stage: &StageFixture) -> PathBuf {
        let policy = stage
            .policy
            .parent()
            .expect("policy parent")
            .join("policy.json");
        fs::rename(&stage.plugin, stage.target.plugin_root()).expect("activate plugin");
        fs::rename(&stage.policy, &policy).expect("activate policy");
        policy
    }

    fn assert_stage_error(stage: &StageFixture, expected: Error) {
        assert_eq!(
            stage
                .runner
                .validate_staged(&stage.target, &stage.plugin, &stage.policy),
            Err(expected)
        );
    }

    fn append_file(path: &Path, suffix: &str) {
        let mut contents = fs::read_to_string(path).expect("read staged file");
        contents.push_str(suffix);
        fs::write(path, contents).expect("append staged file");
    }

    fn replace_file(path: &Path, needle: &str, replacement: &str) {
        let contents = fs::read_to_string(path).expect("read staged file");
        assert!(contents.contains(needle), "mutation needle is present");
        fs::write(path, contents.replacen(needle, replacement, 1)).expect("mutate staged file");
    }

    #[test]
    fn bounded_stdout_and_stderr_are_rejected_without_leaking_payloads() {
        let root = fixture("output-limit");
        let selected = target(&root.0, TargetSelection::Profile(ProfileName::default()));
        mark_plugin_present(&selected);
        assert_eq!(
            runner(&root.0, "yes x | head -c 70000").is_enabled(&selected),
            Err(Error::HermesOutputLimit)
        );
        let error = runner(&root.0, "yes super-secret-token | head -c 70000 >&2")
            .is_enabled(&selected)
            .expect_err("oversized stderr must fail");
        assert_eq!(error, Error::HermesOutputLimit);
        let display = error.to_string();
        let debug = format!("{error:?}");
        assert!(!display.contains("super-secret-token"));
        assert!(!debug.contains("super-secret-token"));
    }

    #[test]
    fn nonzero_exit_is_typed_without_stderr_payload() {
        let root = fixture("nonzero");
        let selected = target(&root.0, TargetSelection::Profile(ProfileName::default()));
        mark_plugin_present(&selected);
        let error = runner(&root.0, "printf 'super-secret-token' >&2\nexit 7")
            .is_enabled(&selected)
            .expect_err("nonzero must fail");
        assert_eq!(error, Error::HermesCommand);
        assert!(!error.to_string().contains("super-secret-token"));
        assert!(!format!("{error:?}").contains("super-secret-token"));
    }

    #[test]
    fn direct_exit_cleans_background_pipe_holder_before_reader_deadline() {
        let root = fixture("early-exit-descendant");
        let selected = target(&root.0, TargetSelection::Profile(ProfileName::default()));
        let pid_file = root.0.join("descendant.pid");
        let body = format!(
            "sleep 30 &\nchild=$!\nprintf '%s' \"$child\" > '{}'\nprintf '%s\\n' 'Hermes Agent v0.20.0 (2026.8.3)'\nexit 0",
            pid_file.display()
        );
        let timeout = Duration::from_secs(2);
        let runner = HermesRunner {
            executable: validate_executable(&script(&root.0, &body)).expect("executable"),
            timeout,
            stream_limit: MAX_STREAM_BYTES,
        };
        let started = Instant::now();
        runner
            .verify_version(&selected)
            .expect("direct child output");
        assert!(
            started.elapsed() < timeout,
            "reader exceeded shared deadline"
        );
        let child_pid: libc::pid_t = fs::read_to_string(pid_file)
            .expect("descendant PID")
            .parse()
            .expect("numeric descendant PID");
        assert_process_gone(child_pid);
    }

    #[test]
    fn pre_exec_closes_deliberately_inheritable_non_stdio_descriptor() {
        let root = fixture("inherited-fd");
        let selected = target(&root.0, TargetSelection::Profile(ProfileName::default()));
        let inherited_path = root.0.join("inherited");
        let inherited = OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(inherited_path)
            .expect("open controlled descriptor");
        let descriptor = inherited.as_raw_fd();
        make_inheritable(descriptor);
        let result = root.0.join("fd-result");
        let body = format!(
            "if (: >&{descriptor}) 2>/dev/null; then state=inherited; else state=closed; fi\nprintf '%s' \"$state\" > '{}'\nprintf '%s\\n' 'Hermes Agent v0.20.0 (2026.8.3)'",
            result.display()
        );
        runner(&root.0, &body)
            .verify_version(&selected)
            .expect("fixed version after descriptor check");
        assert_eq!(
            fs::read_to_string(result).expect("descriptor result"),
            "closed"
        );
    }

    #[test]
    fn runner_debug_redacts_executable_path() {
        let root = fixture("debug-path-sentinel");
        let runner = runner(&root.0, "exit 0");
        let rendered = format!("{runner:?}");
        assert!(rendered.contains("HermesRunner"));
        assert!(rendered.contains("<redacted>"));
        assert!(!rendered.contains("debug-path-sentinel"));
        assert!(!rendered.contains(&root.0.display().to_string()));
    }

    #[test]
    fn staged_runtime_spawns_through_symlinked_venv_with_venv_only_module() {
        let root = fixture("venv-runtime");
        let installation = root.0.join("installation");
        let venv = installation.join("venv");
        let version_output = Command::new("/usr/bin/python3")
            .args([
                "-I",
                "-c",
                "import sys; print(f'python{sys.version_info.major}.{sys.version_info.minor}')",
            ])
            .env_clear()
            .env("LANG", RUNNER_LOCALE)
            .output()
            .expect("query local Python version");
        assert!(
            version_output.status.success(),
            "query local Python version"
        );
        let version_directory = std::str::from_utf8(&version_output.stdout)
            .expect("UTF-8 Python version")
            .trim();
        assert!(version_directory.starts_with("python"));

        let venv_bin = venv.join("bin");
        let site_packages = venv
            .join("lib")
            .join(version_directory)
            .join("site-packages");
        create_private_parent(&root.0, &venv_bin);
        create_private_parent(&root.0, &site_packages);
        fs::write(
            venv.join("pyvenv.cfg"),
            "home = /usr/bin\ninclude-system-site-packages = false\n",
        )
        .expect("manual isolated venv configuration");

        let internal_runtime = installation.join("python/bin/python3");
        create_private_parent(
            &root.0,
            internal_runtime.parent().expect("internal runtime parent"),
        );
        fs::copy("/usr/bin/python3", &internal_runtime).expect("copy local base interpreter");
        fs::set_permissions(
            &internal_runtime,
            fs::Permissions::from_mode(PRIVATE_DIRECTORY_MODE),
        )
        .expect("private executable runtime");
        symlink("../../python/bin/python3", venv_bin.join("python3"))
            .expect("internal venv runtime link");
        fs::write(
            site_packages.join("pohunek_venv_only.py"),
            "VALUE = 'venv-only'\n",
        )
        .expect("write venv-only module");

        let hermes = write_executable(&venv.join("bin/hermes"), "exit 0");
        let runner = HermesRunner::new(&hermes).expect("validated Hermes executable");
        let runtime = runner.staged_runtime().expect("validated sibling runtime");
        assert_eq!(runtime, venv.join("bin/python3"));
        let mut command = Command::new(runtime);
        command.args([
            "-I",
            "-c",
            "import pohunek_venv_only; assert pohunek_venv_only.VALUE == 'venv-only'",
        ]);
        runner
            .execute(command, ChildEnvironment::Isolated)
            .expect("venv-only module import through sibling path");
    }

    #[test]
    fn ancestor_predicate_rejects_foreign_read_only_and_foreign_sticky_paths() {
        let uid = 1_000;
        let root_uid = 0;
        assert!(safe_ancestor(uid, 0o755, true, uid, root_uid));
        assert!(safe_ancestor(root_uid, 0o755, true, uid, root_uid));
        assert!(safe_ancestor(root_uid, 0o1777, true, uid, root_uid));
        assert!(!safe_ancestor(2_000, 0o555, true, uid, root_uid));
        assert!(!safe_ancestor(2_000, 0o1777, true, uid, root_uid));
        assert!(!safe_ancestor(uid, 0o1777, true, uid, root_uid));
    }

    #[test]
    fn timeout_kills_and_reaps_the_descendant_process_group() {
        let root = fixture("timeout");
        let selected = target(&root.0, TargetSelection::Profile(ProfileName::default()));
        let pid_file = root.0.join("descendant.pid");
        let body = format!(
            "sleep 30 &\nchild=$!\nprintf '%s' \"$child\" > '{}'\nsleep 30",
            pid_file.display()
        );
        let runner = HermesRunner {
            executable: validate_executable(&script(&root.0, &body)).expect("executable"),
            timeout: Duration::from_millis(80),
            stream_limit: MAX_STREAM_BYTES,
        };
        assert_eq!(runner.pohunek_state(&selected), Err(Error::HermesTimeout));
        let child_pid: libc::pid_t = fs::read_to_string(pid_file)
            .expect("descendant PID")
            .parse()
            .expect("numeric descendant PID");
        assert_process_gone(child_pid);
    }

    fn assert_process_gone(child_pid: libc::pid_t) {
        let deadline = Instant::now() + Duration::from_secs(1);
        while process_exists(child_pid) && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(10));
        }
        assert!(
            !process_exists(child_pid),
            "descendant process survived timeout"
        );
    }

    #[expect(
        unsafe_code,
        reason = "fcntl is required to construct the controlled inheritance regression fixture"
    )]
    fn make_inheritable(descriptor: libc::c_int) {
        // SAFETY: the descriptor is owned by the live `File` in this test, and
        // F_GETFD/F_SETFD only inspect and clear its close-on-exec flag.
        unsafe {
            let flags = libc::fcntl(descriptor, libc::F_GETFD);
            assert!(flags >= 0, "read descriptor flags");
            assert_eq!(
                libc::fcntl(descriptor, libc::F_SETFD, flags & !libc::FD_CLOEXEC),
                0,
                "clear close-on-exec"
            );
        }
    }

    #[expect(
        unsafe_code,
        reason = "kill(pid, 0) is the portable local test probe for a recorded descendant PID"
    )]
    fn process_exists(pid: libc::pid_t) -> bool {
        // SAFETY: signal zero only probes a PID owned by this controlled test;
        // it never changes process state and the PID was parsed from our script.
        unsafe { libc::kill(pid, 0) == 0 }
    }
}
