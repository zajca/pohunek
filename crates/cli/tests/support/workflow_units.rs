//! A hermetic `pohunek service` installation with workflow plugins whose jobs
//! and services run through the `__exec` trampoline.
//!
//! Shared by `workflow_exec.rs`, which runs the trampoline directly, and by
//! `workflow_systemd.rs` and `workflow_launchd.rs`, whose service manager
//! starts it from the plugin units. Every installation lives in its own
//! [`TestEnv`]: `service.toml` is written by the installer's own
//! configuration code, and `<prefix>/bin/pohunek` is a copy of this build's
//! CLI, which is what every unit runs. Plugin programs are shell scripts that
//! use absolute paths only, because a plugin gets no `PATH`, and record what
//! they do in the plugin state directory.

#![allow(
    dead_code,
    reason = "each workflow unit test binary uses a different subset of these helpers"
)]

// Rust guideline compliant 2026-10-13

use std::fmt::Write as _;
use std::fs;
use std::os::unix::fs::{DirBuilderExt as _, PermissionsExt as _};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use nix::sys::signal::kill;
use nix::unistd::Pid;
use package::{build_archive, read_archive, ArchiveEntry, Limits, PackageDigest};
use pohunek_cli::service::definition::identity_config;
use pohunek_cli::service::{Context, VERSION};
use pohunek_cli::workflow_units::{disable_plugin, enable_plugin};
use pohunek_paths::{BasePaths, PathEnv, Platform};
use pohunek_platform::supervisor::{Namespace, PluginUnitKey, PluginUnitKind, PluginUnits};
use pohunek_test_support::env::TestEnv;
use pohunek_test_support::wait::poll_until;
use serde_json::Value;

/// Owner knobs every scenario starts from: short intervals and delays so a
/// scenario observes several runs quickly, and a short kill grace.
pub(crate) const KNOBS: &str = "[limits]\nkill_grace_ms = 300\n\n\
    [jobs]\nmin_interval_s = 1\ncheck_period_s = 1\ntimeout_default_ms = 20000\n\n\
    [services]\nrestart_initial_ms = 500\nrestart_max_ms = 2000\nstable_after_ms = 60000\n";

/// Records a `start` and an `end` line per run, without waiting, and writes
/// [`TICK_OUTPUT`] to stdout and to stderr.
const TICK: &str = r#"#!/bin/sh
printf 'start %s\n' "$$" >> "$POHUNEK_PLUGIN_STATE_DIR/tick.marks"
printf 'tick\n'
printf 'tick\n' >&2
printf 'end %s\n' "$$" >> "$POHUNEK_PLUGIN_STATE_DIR/tick.marks"
"#;

/// Bytes the `tick` program writes to each output stream: `tick\n`.
pub(crate) const TICK_OUTPUT: u64 = 5;

/// Records `start`, waits until the test creates `release`, removes it and
/// records `end`, so a run stays active exactly as long as a scenario needs.
// timing-allowed: #326 the release poll is the fixture's own readiness loop; no scenario waits on it
const GATED: &str = r#"#!/bin/sh
printf 'start %s\n' "$$" >> "$POHUNEK_PLUGIN_STATE_DIR/gated.marks"
while [ ! -e "$POHUNEK_PLUGIN_STATE_DIR/release" ]; do /bin/sleep 0.05; done
/bin/rm "$POHUNEK_PLUGIN_STATE_DIR/release"
printf 'end %s\n' "$$" >> "$POHUNEK_PLUGIN_STATE_DIR/gated.marks"
"#;

/// Leaves a long-sleeping grandchild in its group, records both PIDs and
/// waits, so only a group kill ends the run.
// timing-allowed: #326 a long-running grandchild is the subject of the stop scenarios
const HANG: &str = r#"#!/bin/sh
/bin/sleep 600 &
printf '%s\n' "$!" > "$POHUNEK_PLUGIN_STATE_DIR/$1.grandchild.tmp"
/bin/mv "$POHUNEK_PLUGIN_STATE_DIR/$1.grandchild.tmp" "$POHUNEK_PLUGIN_STATE_DIR/$1.grandchild"
printf '%s\n' "$$" > "$POHUNEK_PLUGIN_STATE_DIR/$1.child.tmp"
/bin/mv "$POHUNEK_PLUGIN_STATE_DIR/$1.child.tmp" "$POHUNEK_PLUGIN_STATE_DIR/$1.child"
wait
"#;

/// Exits with status 3 at once.
const CRASH: &str = "#!/bin/sh\nexit 3\n";

/// Writes its whole environment to `env-<pid>.txt`, then renames it to
/// `env.txt` once complete.
const ENV: &str = r#"#!/bin/sh
/usr/bin/env > "$POHUNEK_PLUGIN_STATE_DIR/env-$$.txt"
/bin/mv "$POHUNEK_PLUGIN_STATE_DIR/env-$$.txt" "$POHUNEK_PLUGIN_STATE_DIR/env.txt"
"#;

/// One plugin's manifest under construction; every plugin carries every
/// fixture program.
#[derive(Debug, Clone)]
pub(crate) struct Plugin {
    pub(crate) id: String,
    tables: String,
}

impl Plugin {
    /// A plugin `id` with one action, so it is valid without units too.
    pub(crate) fn new(id: &str) -> Self {
        Self {
            id: id.to_owned(),
            tables: "[actions.tick]\nargv = [\"bin/tick\"]\n".to_owned(),
        }
    }

    /// Declares job `name` running `program` every `every_s` seconds.
    pub(crate) fn job(mut self, name: &str, program: &str, every_s: u64) -> Self {
        let _ = write!(
            self.tables,
            "\n[jobs.{name}]\nargv = [\"bin/{program}\", \"{name}\"]\nevery_s = {every_s}\n"
        );
        self
    }

    /// Declares job `name` like [`Plugin::job`] with a deadline.
    pub(crate) fn job_with_timeout(
        mut self,
        name: &str,
        program: &str,
        every_s: u64,
        timeout_ms: u64,
    ) -> Self {
        let _ = write!(
            self.tables,
            "\n[jobs.{name}]\nargv = [\"bin/{program}\", \"{name}\"]\nevery_s = {every_s}\n\
             timeout_ms = {timeout_ms}\n"
        );
        self
    }

    /// Declares service `name` running `program`.
    pub(crate) fn service(mut self, name: &str, program: &str) -> Self {
        let _ = write!(
            self.tables,
            "\n[services.{name}]\nargv = [\"bin/{program}\", \"{name}\"]\n"
        );
        self
    }

    fn manifest(&self) -> String {
        format!(
            "schema = 1\n\n[plugin]\nid = \"{}\"\nversion = \"1.0.0\"\n\
             description = \"Workflow unit scenarios\"\n\n{}",
            self.id, self.tables
        )
    }

    fn files(&self) -> Vec<ArchiveEntry> {
        let entry = |path: &str, contents: &str, executable: bool| ArchiveEntry {
            path: path.to_owned(),
            contents: contents.as_bytes().to_vec(),
            executable,
        };
        vec![
            entry("workflow.toml", &self.manifest(), false),
            entry("bin/tick", TICK, true),
            entry("bin/gated", GATED, true),
            entry("bin/hang", HANG, true),
            entry("bin/crash", CRASH, true),
            entry("bin/env", ENV, true),
        ]
    }
}

/// One hermetic installation: its environment, the service context of that
/// environment, and the installed CLI copy.
pub(crate) struct UnitEnv {
    pub(crate) env: TestEnv,
    pub(crate) context: Context,
}

impl UnitEnv {
    /// Creates the environment, records `service.toml` for it and installs
    /// this build's CLI as `<prefix>/bin/pohunek`. `unit_dir` chooses where
    /// the plugin units go.
    pub(crate) fn new(unit_dir: impl FnOnce(&TestEnv) -> PathBuf) -> Self {
        let env = TestEnv::new().expect("create the hermetic test environment");
        let os = |path: &Path| Some(path.as_os_str().to_owned());
        let paths = BasePaths::resolve_for(
            Platform::current().expect("supported platform"),
            nix::unistd::Uid::effective().as_raw(),
            &PathEnv {
                xdg_runtime_dir: os(env.runtime_dir()),
                xdg_data_home: os(env.data_home()),
                xdg_state_home: os(env.state_home()),
                xdg_cache_home: os(env.cache_home()),
                xdg_config_home: os(env.config_home()),
                home: os(env.home()),
            },
        )
        .expect("resolve the hermetic paths");
        let supervisor_dir = unit_dir(&env);
        let context = Context::new(
            paths,
            nix::unistd::Uid::effective().as_raw(),
            Some(env.home().to_path_buf()),
            Some(env.runtime_dir().to_path_buf()),
            supervisor_dir,
            pohunek_test_support::bin_exe("pohunek"),
        );
        let prefix = env.root().join("prefix");
        identity_config(&context, &prefix, VERSION)
            .expect("the installation configuration")
            .write(&context.config_path())
            .expect("write service.toml");
        let bin = prefix.join("bin");
        fs::create_dir_all(&bin).expect("create the prefix bin directory");
        fs::copy(
            pohunek_test_support::bin_exe("pohunek"),
            bin.join("pohunek"),
        )
        .expect("install the CLI copy");
        let unit_env = Self { env, context };
        unit_env.knobs(KNOBS);
        unit_env
    }

    /// `pohunek <arguments>` in the hermetic environment.
    pub(crate) fn command(&self, arguments: &[&str]) -> Command {
        let mut command = self.env.command(pohunek_test_support::bin_exe("pohunek"));
        command.args(arguments);
        command
    }

    /// Writes `text` as the owner knobs file with mode 0600.
    pub(crate) fn knobs(&self, text: &str) {
        let dir = self.env.config_home().join("pohunek");
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&dir)
            .expect("create the pohunek config directory");
        let path = dir.join("workflow.toml");
        fs::write(&path, text).expect("write workflow.toml");
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).expect("chmod knobs");
    }

    /// Installs `plugin` disabled and returns its digest.
    pub(crate) fn install(&self, plugin: &Plugin) -> PackageDigest {
        let digest = self.archive(plugin);
        let output = self.install_with(plugin, &digest, &["--no-enable"]);
        assert_eq!(output.status.code(), Some(0), "{}", describe(&output));
        digest
    }

    /// Writes `plugin`'s archive as `<root>/<id>.tar.zst` and returns its
    /// digest.
    pub(crate) fn archive(&self, plugin: &Plugin) -> PackageDigest {
        let bytes = build_archive(&plugin.files(), &Limits::DEFAULT).expect("the archive builds");
        let digest = read_archive(&bytes, &Limits::DEFAULT)
            .expect("the archive reads")
            .digest()
            .clone();
        fs::write(self.archive_path(plugin), bytes).expect("write the archive");
        digest
    }

    fn archive_path(&self, plugin: &Plugin) -> PathBuf {
        self.env.root().join(format!("{}.tar.zst", plugin.id))
    }

    /// `pohunek workflow install <archive> --sha256 <digest> --yes --json
    /// <extra>` of `plugin`'s archive written by [`UnitEnv::archive`].
    pub(crate) fn install_with(
        &self,
        plugin: &Plugin,
        digest: &PackageDigest,
        extra: &[&str],
    ) -> Output {
        let archive = self.archive_path(plugin);
        let mut arguments = vec![
            "workflow",
            "install",
            archive.to_str().expect("UTF-8 path"),
            "--sha256",
            digest.as_str(),
            "--yes",
            "--json",
        ];
        arguments.extend_from_slice(extra);
        self.command(&arguments).output().expect("run the install")
    }

    /// `pohunek workflow __exec <id> <digest> <kind> <name>`.
    pub(crate) fn exec(&self, id: &str, digest: &str, kind: &str, name: &str) -> Command {
        self.command(&["workflow", "__exec", id, digest, kind, name])
    }

    /// `pohunek workflow <verb> <id> --json`, which must succeed.
    pub(crate) fn toggle(&self, verb: &str, id: &str) -> Value {
        let output = self
            .command(&["workflow", verb, id, "--json"])
            .output()
            .expect("run pohunek workflow");
        assert_eq!(output.status.code(), Some(0), "{}", describe(&output));
        serde_json::from_slice(&output.stdout).expect("a JSON envelope")
    }

    /// The plugins `pohunek workflow list --json` reports.
    pub(crate) fn listed(&self) -> Vec<Value> {
        let output = self
            .command(&["workflow", "list", "--json"])
            .output()
            .expect("run pohunek workflow list");
        assert_eq!(output.status.code(), Some(0), "{}", describe(&output));
        let document: Value = serde_json::from_slice(&output.stdout).expect("a JSON envelope");
        document["ok"]["plugins"]
            .as_array()
            .expect("a plugins array")
            .clone()
    }

    /// The plugin's state directory.
    pub(crate) fn state_dir(&self, id: &str) -> PathBuf {
        self.env
            .state_home()
            .join("pohunek/workflow-plugin-state")
            .join(id)
    }

    /// The plugin's run records, oldest first.
    pub(crate) fn records(&self, id: &str) -> Vec<Value> {
        let text =
            fs::read_to_string(self.state_dir(id).join("logs/runs.jsonl")).unwrap_or_default();
        text.lines()
            .map(|line| serde_json::from_str(line).expect("a run record is one JSON line"))
            .collect()
    }

    /// The plugin's run records of unit `name`.
    pub(crate) fn unit_records(&self, id: &str, kind: &str, name: &str) -> Vec<Value> {
        self.records(id)
            .into_iter()
            .filter(|record| record["kind"] == kind && record["name"] == name)
            .collect()
    }

    /// The lines a fixture program appended to `<name>.marks`.
    pub(crate) fn marks(&self, id: &str, name: &str) -> Vec<String> {
        fs::read_to_string(self.state_dir(id).join(format!("{name}.marks")))
            .unwrap_or_default()
            .lines()
            .map(str::to_owned)
            .collect()
    }

    /// How many runs of fixture `name` started and how many ended.
    pub(crate) fn run_counts(&self, id: &str, name: &str) -> (usize, usize) {
        let marks = self.marks(id, name);
        let started = marks
            .iter()
            .filter(|mark| mark.starts_with("start "))
            .count();
        (started, marks.len() - started)
    }

    /// Lets the waiting `gated` run of plugin `id` finish.
    pub(crate) fn release(&self, id: &str) {
        fs::write(self.state_dir(id).join("release"), b"").expect("release the gated run");
    }

    /// The PID a `hang` run of unit `name` recorded under `role`
    /// (`child` or `grandchild`), waiting until it is written.
    pub(crate) fn pid(&self, id: &str, name: &str, role: &str) -> Pid {
        let path = self.state_dir(id).join(format!("{name}.{role}"));
        poll_until(&format!("{} to be written", path.display()), || {
            let text = fs::read_to_string(&path).ok()?;
            text.trim_end().parse().ok().map(Pid::from_raw)
        })
    }

    /// Makes job `name` due again by removing the trampoline's documented
    /// `schedule/job-<name>.json`.
    pub(crate) fn reset_schedule(&self, id: &str, name: &str) {
        fs::remove_file(self.state_dir(id).join(format!("schedule/job-{name}.json")))
            .expect("remove the schedule file");
    }

    /// Makes job `name` due again and removes the PID files of its earlier
    /// `hang` runs, so the next [`UnitEnv::pid`] waits for a new run. The
    /// schedule file is the trampoline's documented
    /// `schedule/job-<name>.json`.
    pub(crate) fn reset_job(&self, id: &str, name: &str) {
        let state = self.state_dir(id);
        for path in [
            state.join(format!("{name}.child")),
            state.join(format!("{name}.grandchild")),
            state.join(format!("schedule/job-{name}.json")),
        ] {
            fs::remove_file(&path).expect("remove a run file");
        }
    }

    /// The `KEY=VALUE` lines the `env` program wrote, waiting until they are
    /// complete.
    pub(crate) fn environment(&self, id: &str) -> Vec<(String, String)> {
        let path = self.state_dir(id).join("env.txt");
        let text = poll_until("the plugin environment", || fs::read_to_string(&path).ok());
        text.lines()
            .map(|line| {
                let (key, value) = line.split_once('=').expect("KEY=VALUE");
                (key.to_owned(), value.to_owned())
            })
            .collect()
    }
}

/// Waits until process `pid` is gone.
pub(crate) fn wait_gone(pid: Pid) {
    poll_until(&format!("process {pid} to end"), || {
        kill(pid, None).is_err().then_some(())
    });
}

/// The ordered `start`/`end` lines of runs that never overlapped: each run
/// ends before the next one starts.
pub(crate) fn assert_alternating(marks: &[String]) {
    for (index, mark) in marks.iter().enumerate() {
        let expected = if index % 2 == 0 { "start " } else { "end " };
        assert!(
            mark.starts_with(expected),
            "runs overlapped: line {index} is {mark:?} in {marks:?}"
        );
    }
}

pub(crate) fn describe(output: &Output) -> String {
    format!(
        "status {:?}\nstdout: {}\nstderr: {}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

/// [`KNOBS`] with a one-hour check period: the units are installed and
/// armed, but their timers never fire during a scenario, so the scenario
/// alone decides when the trampoline runs.
pub(crate) const DIRECT_KNOBS: &str = "[limits]\nkill_grace_ms = 300\n\n\
    [jobs]\nmin_interval_s = 1\ncheck_period_s = 3600\ntimeout_default_ms = 20000\n\n\
    [services]\nrestart_initial_ms = 500\nrestart_max_ms = 2000\nstable_after_ms = 60000\n";

/// Variables a POSIX shell adds to its own environment before the fixture
/// program's `/usr/bin/env` reads it; the runner never sets them.
const SHELL_VARIABLES: [&str; 3] = ["PWD", "SHLVL", "_"];

/// Every variable a supervised run sets: the bootstrap roots and the plugin
/// variables (`docs/knowledge/guides/workflow-plugins.md`).
pub(crate) const SUPERVISED_ENVIRONMENT: [&str; 13] = [
    "HOME",
    "POHUNEK_CLI",
    "POHUNEK_PLUGIN_CONFIG_DIR",
    "POHUNEK_PLUGIN_CONTEXT",
    "POHUNEK_PLUGIN_DIGEST",
    "POHUNEK_PLUGIN_ID",
    "POHUNEK_PLUGIN_STATE_DIR",
    "POHUNEK_PLUGIN_VERSION",
    "XDG_CACHE_HOME",
    "XDG_CONFIG_HOME",
    "XDG_DATA_HOME",
    "XDG_RUNTIME_DIR",
    "XDG_STATE_HOME",
];

/// The variable names of `environment` without the shell's own.
pub(crate) fn variable_names(environment: &[(String, String)]) -> Vec<String> {
    let mut names: Vec<String> = environment
        .iter()
        .map(|(key, _value)| key.clone())
        .filter(|key| !SHELL_VARIABLES.contains(&key.as_str()))
        .collect();
    names.sort();
    names
}

/// A [`UnitEnv`] whose plugins have units in the real service manager, and
/// whose drop removes them even when the scenario failed.
pub(crate) struct Managed {
    pub(crate) units: UnitEnv,
    namespace: Namespace,
    plugins: Vec<(String, Vec<PluginUnitKey>)>,
}

impl Managed {
    /// A fresh installation with `knobs`.
    pub(crate) fn new(knobs: &str) -> Self {
        native::require_manager();
        let units = UnitEnv::new(native::unit_dir);
        units.knobs(knobs);
        let namespace = units
            .context
            .namespace()
            .expect("the installation namespace");
        Self {
            units,
            namespace,
            plugins: Vec::new(),
        }
    }

    /// Installs and enables `plugin` and returns its digest.
    pub(crate) async fn enable(&mut self, plugin: &Plugin) -> PackageDigest {
        let digest = self.units.install(plugin);
        self.plugins.push((plugin.id.clone(), plugin.keys()));
        enable_plugin(&self.units.context, &plugin.id)
            .await
            .expect("enable the plugin");
        digest
    }

    /// Disables plugin `id`.
    pub(crate) async fn disable(&self, id: &str) {
        disable_plugin(&self.units.context, id)
            .await
            .expect("disable the plugin");
    }

    /// The native names of plugin `id`'s units: systemd unit files or launchd
    /// labels.
    pub(crate) fn names(&self, id: &str) -> Vec<String> {
        let keys = self
            .plugins
            .iter()
            .find(|(plugin, _keys)| plugin == id)
            .map(|(_plugin, keys)| keys.clone())
            .unwrap_or_default();
        let mut names = native::names(&self.namespace, &keys);
        names.sort();
        names
    }

    /// This installation's unit definition files in the unit directory.
    pub(crate) fn unit_files(&self) -> Vec<String> {
        let mut files: Vec<String> = fs::read_dir(self.units.context.supervisor_dir())
            .map(|entries| {
                entries
                    .map(|entry| {
                        entry
                            .expect("a unit directory entry")
                            .file_name()
                            .to_string_lossy()
                            .into_owned()
                    })
                    .filter(|name| name.starts_with(&native::plugin_prefix(&self.namespace)))
                    .collect()
            })
            .unwrap_or_default();
        files.sort();
        files
    }

    /// The platform backend of this installation's plugin units.
    pub(crate) async fn backend(&self) -> Box<dyn PluginUnits> {
        native::backend(&self.namespace, self.units.context.supervisor_dir()).await
    }

    /// The native names of unit `key`: systemd units or a launchd label.
    pub(crate) fn native_names(&self, key: &PluginUnitKey) -> Vec<String> {
        native::names(&self.namespace, std::slice::from_ref(key))
    }

    /// This installation's plugin units the manager still has loaded.
    pub(crate) fn loaded(&self) -> Vec<String> {
        let keys: Vec<PluginUnitKey> = self
            .plugins
            .iter()
            .flat_map(|(_id, keys)| keys.iter().cloned())
            .collect();
        native::loaded(&self.namespace, &keys)
    }
}

impl Drop for Managed {
    /// Disables every plugin the scenario enabled, then removes whatever of
    /// this namespace's plugin units remains.
    fn drop(&mut self) {
        let context = self.units.context.clone();
        let ids: Vec<String> = self.plugins.iter().map(|(id, _keys)| id.clone()).collect();
        // The scenario's runtime cannot be re-entered from a synchronous drop,
        // so the teardown owns a runtime on its own thread.
        let teardown = std::thread::spawn(move || {
            let runtime = tokio::runtime::Runtime::new().expect("a teardown runtime");
            runtime.block_on(async {
                for id in ids {
                    if let Err(error) = disable_plugin(&context, &id).await {
                        eprintln!("teardown could not disable {id}: {error}");
                    }
                }
            });
        });
        if teardown.join().is_err() {
            eprintln!("the workflow unit teardown panicked");
        }
        let keys: Vec<PluginUnitKey> = self
            .plugins
            .iter()
            .flat_map(|(_id, keys)| keys.iter().cloned())
            .collect();
        native::remove(&self.namespace, &keys, self.units.context.supervisor_dir());
    }
}

impl Plugin {
    /// The unit keys of every job and service the plugin declares.
    fn keys(&self) -> Vec<PluginUnitKey> {
        let document: toml::Value = toml::from_str(&self.manifest()).expect("a TOML manifest");
        let mut keys = Vec::new();
        for (table, kind) in [
            ("jobs", PluginUnitKind::Job),
            ("services", PluginUnitKind::Service),
        ] {
            let names = document
                .get(table)
                .and_then(toml::Value::as_table)
                .map(|units| units.keys().cloned().collect::<Vec<_>>())
                .unwrap_or_default();
            for name in names {
                keys.push(PluginUnitKey::new(&self.id, kind, name).expect("a valid unit key"));
            }
        }
        keys
    }
}

/// The systemd user manager, opted in with `POHUNEK_SYSTEMD_E2E=1`.
#[cfg(target_os = "linux")]
pub(crate) mod native {
    use std::path::{Path, PathBuf};
    use std::process::Command;

    use pohunek_platform::supervisor::systemd::SystemdPluginUnits;
    use pohunek_platform::supervisor::{Namespace, PluginUnitKey, PluginUnitKind, PluginUnits};
    use pohunek_test_support::env::TestEnv;

    pub(crate) fn require_manager() {
        assert_eq!(
            std::env::var("POHUNEK_SYSTEMD_E2E").as_deref(),
            Ok("1"),
            "set POHUNEK_SYSTEMD_E2E=1 explicitly"
        );
    }

    /// The manager's runtime unit directory: a user manager loads persistent
    /// units only from its search path, never from a temporary config home.
    pub(crate) fn unit_dir(_env: &TestEnv) -> PathBuf {
        PathBuf::from(
            std::env::var_os("XDG_RUNTIME_DIR").expect("XDG_RUNTIME_DIR of the user session"),
        )
        .join("systemd/user")
    }

    pub(crate) fn plugin_prefix(namespace: &Namespace) -> String {
        format!("pohunek-{namespace}-wf-")
    }

    pub(crate) fn names(namespace: &Namespace, keys: &[PluginUnitKey]) -> Vec<String> {
        keys.iter()
            .flat_map(|key| {
                let mut units = vec![namespace.plugin_service_unit(key)];
                if key.kind() == PluginUnitKind::Job {
                    units.push(namespace.plugin_timer_unit(key));
                }
                units
            })
            .collect()
    }

    pub(crate) fn loaded(namespace: &Namespace, _keys: &[PluginUnitKey]) -> Vec<String> {
        let output = systemctl()
            .args(["list-units", "--all", "--plain", "--no-legend"])
            .arg(format!("{}*", plugin_prefix(namespace)))
            .output()
            .expect("run systemctl list-units");
        assert!(output.status.success(), "systemctl list-units failed");
        String::from_utf8(output.stdout)
            .expect("UTF-8 unit list")
            .lines()
            .filter_map(|line| line.split_whitespace().next().map(str::to_owned))
            .collect()
    }

    pub(crate) fn remove(namespace: &Namespace, _keys: &[PluginUnitKey], unit_dir: &Path) {
        let pattern = format!("{}*", plugin_prefix(namespace));
        quiet(&["stop", &pattern]);
        if let Ok(entries) = std::fs::read_dir(unit_dir) {
            for entry in entries.flatten() {
                let name = entry.file_name().to_string_lossy().into_owned();
                if name.starts_with(&plugin_prefix(namespace)) {
                    quiet(&["disable", &name]);
                    if let Err(error) = std::fs::remove_file(entry.path()) {
                        eprintln!("teardown could not remove {name}: {error}");
                    }
                }
            }
        }
        quiet(&["daemon-reload"]);
        quiet(&["reset-failed", &pattern]);
    }

    /// Bound on every D-Bus call of the test's own backend.
    const CALL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

    /// The systemd backend of `namespace`'s plugin units in `unit_dir`.
    pub(crate) async fn backend(namespace: &Namespace, unit_dir: &Path) -> Box<dyn PluginUnits> {
        Box::new(
            SystemdPluginUnits::connect(namespace.clone(), unit_dir.to_path_buf(), CALL_TIMEOUT)
                .await
                .expect("connect to the systemd user manager"),
        )
    }

    /// Starts the service unit among `units` now, without waiting for it.
    pub(crate) fn start(units: &[String]) {
        let service = units
            .iter()
            .find(|unit| unit.ends_with(".service"))
            .expect("a service unit");
        let status = systemctl()
            .args(["start", "--no-block", service])
            .status()
            .expect("run systemctl start");
        assert!(status.success(), "systemctl start {service} failed");
    }

    /// Whether systemd records any of `units` as failed.
    pub(crate) fn failed(units: &[String]) -> bool {
        units.iter().any(|unit| {
            systemctl()
                .args(["is-failed", "--quiet", unit])
                .status()
                .expect("run systemctl is-failed")
                .success()
        })
    }

    /// Plants `name=value` in the manager's own environment.
    pub(crate) fn set_manager_variable(name: &str, value: &str) {
        let status = systemctl()
            .args(["set-environment", &format!("{name}={value}")])
            .status()
            .expect("run systemctl set-environment");
        assert!(status.success(), "systemctl set-environment failed");
    }

    /// Removes `name` from the manager's own environment.
    pub(crate) fn unset_manager_variable(name: &str) {
        quiet(&["unset-environment", name]);
    }

    /// The value of `name` in the manager's own environment.
    pub(crate) fn manager_variable(name: &str) -> Option<String> {
        let output = systemctl()
            .arg("show-environment")
            .output()
            .expect("run systemctl show-environment");
        assert!(output.status.success(), "systemctl show-environment failed");
        String::from_utf8_lossy(&output.stdout)
            .lines()
            .find_map(|line| line.strip_prefix(&format!("{name}=")).map(str::to_owned))
    }

    // The inherited environment selects the real user manager (session bus
    // address and XDG_RUNTIME_DIR); these suites run only when opted in.
    fn systemctl() -> Command {
        let mut command = Command::new("systemctl");
        command.arg("--user");
        command
    }

    fn quiet(arguments: &[&str]) {
        match systemctl().arg("--quiet").args(arguments).output() {
            Ok(output) if output.status.success() => {}
            Ok(output) => eprintln!(
                "teardown `systemctl --user {}` exited with {}",
                arguments.join(" "),
                output.status
            ),
            Err(error) => eprintln!("teardown could not run systemctl: {error}"),
        }
    }
}

/// launchd in `gui/<uid>`; a missing domain fails the scenario.
#[cfg(target_os = "macos")]
pub(crate) mod native {
    use std::path::{Path, PathBuf};
    use std::process::{Command, Stdio};

    use pohunek_platform::supervisor::launchd::LaunchdPluginUnits;
    use pohunek_platform::supervisor::{Namespace, PluginUnitKey, PluginUnits};
    use pohunek_test_support::env::TestEnv;

    /// Exit status of `launchctl print` for a label absent from its domain.
    const LAUNCHCTL_ABSENT: i32 = 113;

    /// Deadline of every `launchctl` command of the test's own backend.
    const COMMAND_DEADLINE: std::time::Duration = std::time::Duration::from_secs(30);

    /// The launchd backend of `namespace`'s plugin units in `agents`.
    #[expect(
        clippy::unused_async,
        reason = "one call shape with the systemd backend, which connects asynchronously"
    )]
    pub(crate) async fn backend(namespace: &Namespace, agents: &Path) -> Box<dyn PluginUnits> {
        Box::new(
            LaunchdPluginUnits::new(
                namespace.clone(),
                uid(),
                agents.to_path_buf(),
                COMMAND_DEADLINE,
            )
            .expect("the launchd plugin unit backend"),
        )
    }

    /// Starts the job of the definition among `names` now.
    pub(crate) fn start(names: &[String]) {
        let label = names[0].trim_end_matches(".plist");
        assert!(
            launchctl(&["kickstart", &format!("gui/{}/{label}", uid())]).success(),
            "launchctl kickstart {label} failed"
        );
    }

    /// launchd keeps no failed state for a job: a stopped job is booted out
    /// or waits for its next start, so there is no failure to read.
    pub(crate) fn failed(_labels: &[String]) -> bool {
        false
    }

    fn uid() -> u32 {
        nix::unistd::Uid::effective().as_raw()
    }

    fn launchctl(arguments: &[&str]) -> std::process::ExitStatus {
        // launchctl addresses launchd through the mach bootstrap port, not
        // through any variable.
        Command::new("/bin/launchctl")
            .env_clear()
            .current_dir("/")
            .args(arguments)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .expect("launchctl runs")
    }

    pub(crate) fn require_manager() {
        let domain = format!("gui/{}", uid());
        assert!(
            launchctl(&["print", &domain]).success(),
            "launchd domain {domain} is unavailable"
        );
    }

    /// A private `LaunchAgents` directory; `bootstrap` loads from any path.
    pub(crate) fn unit_dir(env: &TestEnv) -> PathBuf {
        env.root().join("LaunchAgents")
    }

    pub(crate) fn plugin_prefix(namespace: &Namespace) -> String {
        format!("io.github.zajca.pohunek.{namespace}.wf.")
    }

    pub(crate) fn names(namespace: &Namespace, keys: &[PluginUnitKey]) -> Vec<String> {
        keys.iter()
            .map(|key| format!("{}.plist", namespace.plugin_label(key)))
            .collect()
    }

    pub(crate) fn loaded(namespace: &Namespace, keys: &[PluginUnitKey]) -> Vec<String> {
        keys.iter()
            .map(|key| namespace.plugin_label(key))
            .filter(|label| {
                let target = format!("gui/{}/{label}", uid());
                launchctl(&["print", &target]).code() != Some(LAUNCHCTL_ABSENT)
            })
            .collect()
    }

    pub(crate) fn remove(namespace: &Namespace, keys: &[PluginUnitKey], unit_dir: &Path) {
        for key in keys {
            let label = namespace.plugin_label(key);
            let _status = launchctl(&["bootout", &format!("gui/{}/{label}", uid())]);
            let _missing = std::fs::remove_file(unit_dir.join(format!("{label}.plist")));
        }
    }

    /// Plants `name=value` in the launchd user domain's environment.
    pub(crate) fn set_manager_variable(name: &str, value: &str) {
        assert!(
            launchctl(&["setenv", name, value]).success(),
            "launchctl setenv failed"
        );
    }

    /// Removes `name` from the launchd user domain's environment.
    pub(crate) fn unset_manager_variable(name: &str) {
        let _status = launchctl(&["unsetenv", name]);
    }

    /// The value of `name` in the launchd user domain's environment.
    pub(crate) fn manager_variable(name: &str) -> Option<String> {
        let output = Command::new("/bin/launchctl")
            .env_clear()
            .current_dir("/")
            .args(["getenv", name])
            .stdin(Stdio::null())
            .output()
            .expect("launchctl runs");
        let value = String::from_utf8_lossy(&output.stdout)
            .trim_end()
            .to_owned();
        (!value.is_empty()).then_some(value)
    }
}
