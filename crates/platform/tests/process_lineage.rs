//! Exercises fork-lineage attribution of runtime sweeps against real processes.
//!
//! A restricted platform binary (`/bin/sh`, `/bin/sleep`, ...) can run with an
//! environment the kernel withholds from its same-user observers (macOS 27), so
//! a sweep cannot read the runtime marker off it. The scenarios here start a
//! stand-in worker whose descendants run through such binaries, kill the worker,
//! and sweep its runtime through the public `DarwinInspector` and
//! `sweep_runtime` boundary. Which side of the kernel's behavior the host is on
//! is decided by an independent read of the target's environment (`ps -E`, which
//! prints whatever the kernel exposes), never by the code under test, and every
//! assertion holds on both sides: a process whose environment the kernel exposes
//! is attributed by its marker, one whose environment is withheld by its lineage.
//!
//! Every fixture that must carry a readable marker ends in this test binary,
//! re-run as [`hold`], which is not a platform binary on any macOS release.
#![cfg(target_os = "macos")]

// Rust guideline compliant 2026-10-10

use std::collections::BTreeSet;
use std::path::Path;
use std::process::{Child, Stdio};
use std::time::Duration;

use pohunek_platform::process::{
    sweep_runtime, DarwinInspector, Pid, ProcessIdentity, ProcessInspector as _, SkipReason,
    SpawnId, SweepReport, SweepRequest,
};
use pohunek_test_support::env::TestEnv;
use pohunek_test_support::wait::poll_until;

/// Upper bound of each sweep wait for signalled processes to exit.
///
/// A bound only: the sweep returns as soon as every signalled process has
/// exited, so a generous value costs nothing and absorbs a loaded runner.
const SWEEP_GRACE: Duration = Duration::from_secs(10);

/// Liveness polling interval of the sweep.
const SWEEP_POLL: Duration = Duration::from_millis(20);

/// How long a fixture `sleep` or holding image lives unless it is killed.
///
/// Far beyond any scenario so the process is still resident while the scenario
/// inspects it; the scenario kills it, so the length only bounds the leak if
/// the test process itself is killed.
const FIXTURE_LIFETIME_SECONDS: &str = "600";

/// Shell helper every fixture script starts with.
///
/// `publish NAME VALUE` writes `VALUE` to `$DIR/NAME` atomically, so a reader
/// either sees the whole value or no file. `$1` is the holding image and `$2`
/// the scenario directory.
const SCRIPT_PRELUDE: &str = r#"IMAGE=$1
DIR=$2
publish() { printf '%s\n' "$2" > "$DIR/$1.tmp" && /bin/mv "$DIR/$1.tmp" "$DIR/$1"; }
"#;

/// The stand-in worker: starts the creator `a`, publishes its id, then waits.
const WORKER_WITH_CHAIN: &str = r#"/bin/sh "$DIR/a.sh" "$IMAGE" "$DIR" &
publish worker.pids "$!"
wait
"#;

/// The creator `a`: starts a `sleep` and the creator `b`.
const CREATOR_A: &str = r#"/bin/sleep 600 &
sleeper=$!
/bin/sh "$DIR/b.sh" "$IMAGE" "$DIR" &
creator=$!
publish a.pids "$sleeper $creator"
wait
"#;

/// The creator `b`: starts a `sleep` and the holding image, which carries a
/// readable marker on every host.
const CREATOR_B: &str = r#"/bin/sleep 600 &
sleeper=$!
"$IMAGE" --exact hold --ignored &
holder=$!
publish b.pids "$sleeper $holder"
wait
"#;

/// The stand-in worker of the dead-creator shape: starts the creator `m`.
const WORKER_WITH_MORTAL_CREATOR: &str = r#"/bin/sh "$DIR/m.sh" "$IMAGE" "$DIR" &
publish worker.pids "$!"
wait
"#;

/// The creator `m`: starts a `sleep` that will outlive it.
const MORTAL_CREATOR: &str = r#"/bin/sleep 600 &
publish m.pids "$!"
wait
"#;

/// The creator `m` of the orphaning shape: starts the orphaning fixture, which
/// runs this test binary until its creator is gone.
const ORPHANING_CREATOR: &str = r#""$IMAGE" --exact orphan --ignored &
publish orphan.pids "$!"
wait
"#;

/// A shell that starts a `sleep` outside the stand-in worker's tree.
const SIBLING_CREATOR: &str = r#"/bin/sleep 600 &
publish sibling.pids "$!"
wait
"#;

/// Body of the holding image: this test binary, re-run to wait until killed.
///
/// Only the fixtures start it, as a child of a shell script, so a normal run
/// skips it.
#[test]
#[ignore = "image of the fixture processes spawned by the lineage scenarios"]
fn hold() {
    // timing-allowed: #795 the fixture's own lifetime; no scenario waits on this sleep, it ends when the scenario kills the process
    std::thread::sleep(Duration::from_secs(300));
}

/// Body of the orphaning fixture: waits until its creator is gone and the
/// kernel has reparented it, then execs `sleep` in its place.
///
/// The `exec` after the reparenting is the step under test: it keeps the process
/// id, the environment, and the creation number, and the kernel rewrites the
/// parent creation number it reports. Only the orphaning shape starts it, as a
/// child of a shell script, so a normal run skips it.
#[test]
#[ignore = "image of the orphaning fixture spawned by the lineage scenarios"]
fn orphan() {
    use std::os::unix::process::CommandExt as _;

    /// Process id the kernel gives an orphan as its new parent (`launchd`).
    const REPARENTED_TO: i32 = 1;
    /// Bounds the wait when the scenario died before it killed the creator, so
    /// an abandoned fixture does not spin for long.
    const ORPHAN_PATIENCE: Duration = Duration::from_secs(300);

    let deadline = std::time::Instant::now() + ORPHAN_PATIENCE;
    while rustix::process::getppid().map(|parent| parent.as_raw_nonzero().get())
        != Some(REPARENTED_TO)
    {
        assert!(
            std::time::Instant::now() < deadline,
            "the creator never exited"
        );
        std::thread::yield_now();
    }
    let error = std::process::Command::new("/bin/sleep")
        .arg(FIXTURE_LIFETIME_SECONDS)
        .exec();
    panic!("replace the orphan image with sleep: {error}");
}

/// Path of this test binary, which is also the holding image.
fn image() -> String {
    std::env::current_exe()
        .expect("path of the running test binary")
        .into_os_string()
        .into_string()
        .expect("the test binary path is UTF-8")
}

/// What the kernel shows an unprivileged observer of one process environment,
/// judged from `ps -E` output rather than from the code under test.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Exposure {
    /// The environment, including the runtime marker, is visible.
    Exposed,
    /// The arguments are visible and the environment is not.
    Withheld,
}

/// One process of a scenario, named for the failure message.
#[derive(Debug, Clone)]
struct Member {
    name: &'static str,
    identity: ProcessIdentity,
    /// First argument of the process once it runs its final image.
    image: String,
}

/// A killed-and-reaped scenario: the stand-in worker, its tree, and bystanders.
///
/// Dropping the value kills every process that still holds the identity it was
/// recorded under and reaps the children of this test process, on success and
/// while a failed assertion unwinds.
struct Scenario {
    env: TestEnv,
    runtime: String,
    inspector: DarwinInspector,
    children: Vec<Child>,
    tracked: Vec<ProcessIdentity>,
}

impl Drop for Scenario {
    fn drop(&mut self) {
        for identity in &self.tracked {
            // A process that exited, or whose id was reused, is left alone.
            if self.inspector.identity(identity.pid).ok().flatten() == Some(*identity) {
                kill(identity.pid);
            }
        }
        for child in &mut self.children {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

/// Sends `SIGKILL` to a process that was just verified to be the intended one.
fn kill(pid: Pid) {
    let _ = nix::sys::signal::kill(
        nix::unistd::Pid::from_raw(i32::try_from(pid).expect("a process id fits a pid")),
        nix::sys::signal::Signal::SIGKILL,
    );
}

impl Scenario {
    fn new(tag: &str) -> Self {
        Self {
            env: TestEnv::new().expect("test environment"),
            runtime: format!("lineage-test-{}-{tag}", std::process::id()),
            inspector: DarwinInspector::new(),
            children: Vec::new(),
            tracked: Vec::new(),
        }
    }

    fn dir(&self) -> &Path {
        self.env.cwd()
    }

    fn write_script(&self, name: &str, body: &str) {
        pohunek_test_support::fs::write_file(
            self.dir().join(name),
            format!("{SCRIPT_PRELUDE}{body}"),
        )
        .expect("write the fixture script");
    }

    /// Starts `script` as a direct child of this test, carrying the runtime
    /// marker when `marked`, and returns its identity.
    fn spawn_script(&mut self, script: &str, marked: bool) -> ProcessIdentity {
        let mut command = self.env.command("/bin/sh");
        command
            .arg(self.dir().join(script))
            .arg(image())
            .arg(self.dir())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        if marked {
            command.env("POHUNEK_WORKER_INSTANCE_ID", &self.runtime);
        }
        let child = command.spawn().expect("spawn the fixture script");
        let pid = child.id();
        self.children.push(child);
        let identity = self.live_identity(pid);
        self.tracked.push(identity);
        identity
    }

    /// Starts a bystander `sleep` directly under this test, without the marker.
    fn spawn_sleeper(&mut self) -> Member {
        let child = self
            .env
            .command("/bin/sleep")
            .arg(FIXTURE_LIFETIME_SECONDS)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn the bystander sleep");
        let pid = child.id();
        self.children.push(child);
        let identity = self.live_identity(pid);
        self.tracked.push(identity);
        Member {
            name: "bystander sleep",
            identity,
            image: "/bin/sleep".to_owned(),
        }
    }

    fn live_identity(&self, pid: Pid) -> ProcessIdentity {
        poll_until("the fixture to become observable", || {
            self.inspector
                .identity(pid)
                .expect("inspect the fixture identity")
        })
    }

    /// Waits for the fixture script to publish `name` and returns its pids.
    fn published(&self, name: &str) -> Vec<Pid> {
        let path = self.dir().join(name);
        poll_until(&format!("the fixture to publish {name}"), || {
            let text = std::fs::read_to_string(&path).ok()?;
            let pids = text
                .split_whitespace()
                .map(str::parse::<Pid>)
                .collect::<Result<Vec<_>, _>>()
                .ok()?;
            (!pids.is_empty()).then_some(pids)
        })
    }

    /// Resolves a published pid to a tracked member once it runs `image`.
    ///
    /// A forked child shows its creator's arguments until its `exec` finishes,
    /// so the first argument settles which image the process runs.
    fn member(&mut self, name: &'static str, pid: Pid, image: &str) -> Member {
        let identity = poll_until(&format!("{name} to run {image}"), || {
            let fact = self
                .inspector
                .process(pid)
                .expect("inspect the fixture process")?;
            (fact.cmdline.first().map(String::as_str) == Some(image)).then(|| fact.identity())
        });
        self.tracked.push(identity);
        Member {
            name,
            identity,
            image: image.to_owned(),
        }
    }

    /// What the kernel exposes of `member`'s environment, read by `ps -E`.
    ///
    /// The runtime marker is the probe: it is in the environment of every
    /// process of a marked tree.
    fn exposure(&self, member: &Member) -> Exposure {
        let probe = format!("POHUNEK_WORKER_INSTANCE_ID={}", self.runtime);
        poll_until(
            &format!("a verdict on the environment of {}", member.name),
            || {
                let output = self
                    .env
                    .command("/bin/ps")
                    .args(["-E", "-ww", "-o", "command=", "-p"])
                    .arg(member.identity.pid.to_string())
                    .stdin(Stdio::null())
                    .stderr(Stdio::null())
                    .output()
                    .expect("run ps");
                let line = String::from_utf8_lossy(&output.stdout).into_owned();
                // An empty answer or a creator's arguments mean the process is
                // not (yet) the image whose environment is asked about.
                if !line.starts_with(&member.image) {
                    return None;
                }
                Some(if line.contains(&probe) {
                    Exposure::Exposed
                } else {
                    Exposure::Withheld
                })
            },
        )
    }

    /// Kills `identity` and waits until the process no longer runs.
    fn kill_and_wait(&self, identity: ProcessIdentity) {
        assert_eq!(
            self.inspector
                .identity(identity.pid)
                .expect("inspect the victim"),
            Some(identity),
            "the victim must still be the process that was recorded"
        );
        kill(identity.pid);
        poll_until("the victim to stop running", || {
            (!self
                .inspector
                .is_running(identity)
                .expect("inspect the victim liveness"))
            .then_some(())
        });
    }

    fn request(&self) -> SweepRequest {
        SweepRequest::new(
            self.runtime.as_str(),
            rustix::process::geteuid().as_raw(),
            SWEEP_GRACE,
            SWEEP_POLL,
        )
        .expect("a valid sweep request")
    }

    fn spawn_id(&self, identity: ProcessIdentity) -> SpawnId {
        self.inspector
            .lineage(identity.pid)
            .expect("inspect the lineage")
            .expect("a live same-user process reports its lineage")
            .id
    }

    fn is_running(&self, member: &Member) -> bool {
        self.inspector
            .is_running(member.identity)
            .expect("inspect liveness")
    }
}

/// The processes of one stand-in worker's tree.
struct Tree {
    worker: ProcessIdentity,
    /// Members below the worker, hidden or readable.
    members: Vec<Member>,
}

impl Scenario {
    /// Starts a worker whose tree is the creator chain `a -> b` with a `sleep`
    /// at each level and the holding image at the bottom.
    fn launch_chain(&mut self) -> Tree {
        self.write_script("w.sh", WORKER_WITH_CHAIN);
        self.write_script("a.sh", CREATOR_A);
        self.write_script("b.sh", CREATOR_B);
        let worker = self.spawn_script("w.sh", true);

        let creator_a = self.published("worker.pids")[0];
        let [sleeper_a, creator_b] = self.published("a.pids")[..] else {
            panic!("creator a publishes a sleeper and a creator");
        };
        let [sleeper_b, holder] = self.published("b.pids")[..] else {
            panic!("creator b publishes a sleeper and the holder");
        };
        let image = image();
        let members = vec![
            self.member("creator a", creator_a, "/bin/sh"),
            self.member("sleeper under a", sleeper_a, "/bin/sleep"),
            self.member("creator b", creator_b, "/bin/sh"),
            self.member("sleeper under b", sleeper_b, "/bin/sleep"),
            self.member("holder under b", holder, &image),
        ];
        Tree { worker, members }
    }
}

/// Identities the report says were signalled and observed to exit.
fn reaped(report: &SweepReport) -> BTreeSet<(Pid, u64)> {
    report
        .terminated
        .iter()
        .chain(&report.killed)
        .map(|identity| (identity.pid, identity.start_identity.get()))
        .collect()
}

fn key(identity: ProcessIdentity) -> (Pid, u64) {
    (identity.pid, identity.start_identity.get())
}

/// The reasons the report skipped `identity` for.
fn skip_reasons(report: &SweepReport, identity: ProcessIdentity) -> Vec<SkipReason> {
    report
        .skipped
        .iter()
        .filter(|skipped| skipped.identity == identity)
        .map(|skipped| skipped.reason)
        .collect()
}

/// Whether the report mentions `identity` anywhere.
fn is_listed(report: &SweepReport, identity: ProcessIdentity) -> bool {
    report
        .terminated
        .iter()
        .chain(&report.killed)
        .chain(&report.unconfirmed)
        .any(|listed| *listed == identity)
        || !skip_reasons(report, identity).is_empty()
}

/// Asserts the outcome that holds on every host for a hidden descendant whose
/// creator chain the lineage cannot follow: reaped through a readable marker,
/// or neither signalled nor dismissed but left visible as unproven.
fn assert_orphan_outcome(
    scenario: &Scenario,
    report: &SweepReport,
    orphan: &Member,
    exposure: Exposure,
) {
    match exposure {
        Exposure::Exposed => {
            assert_eq!(
                reaped(report),
                BTreeSet::from([key(orphan.identity)]),
                "a readable marker attributes the orphan"
            );
            assert!(!scenario.is_running(orphan));
        }
        Exposure::Withheld => {
            assert!(
                reaped(report).is_empty() && report.unconfirmed.is_empty(),
                "nothing proves the orphan belongs to the runtime, so nothing is signalled: {report:?}"
            );
            assert!(scenario.is_running(orphan));
            assert_eq!(
                skip_reasons(report, orphan.identity),
                vec![SkipReason::MarkersUnreadable],
                "the orphan stays visible as unproven, never dismissed"
            );
        }
    }
}

#[test]
fn a_withheld_process_still_reports_the_lineage_of_its_creator() {
    let mut scenario = Scenario::new("reports");
    let own = scenario
        .inspector
        .lineage(std::process::id())
        .expect("inspect the own lineage")
        .expect("the test process reports its lineage");
    let sleeper = scenario.spawn_sleeper();

    let lineage = scenario
        .inspector
        .lineage(sleeper.identity.pid)
        .expect("inspect the sleeper lineage")
        .expect("a live same-user process reports its lineage");

    assert_eq!(lineage.parent, own.id, "this test created the sleeper");
    assert!(
        lineage.id > own.id,
        "a process is created after its creator"
    );

    // A process owned by another user is outside the same-user contract.
    assert_eq!(
        scenario.inspector.lineage(1).expect("inspect launchd"),
        None
    );
    // A process that exited and was reaped reports nothing.
    scenario.kill_and_wait(sleeper.identity);
    scenario
        .children
        .pop()
        .expect("the sleeper child")
        .wait()
        .expect("reap the sleeper");
    assert_eq!(
        scenario
            .inspector
            .lineage(sleeper.identity.pid)
            .expect("inspect a reaped process"),
        None
    );
}

#[test]
fn every_same_user_process_reports_its_lineage_or_has_exited() {
    let scenario = Scenario::new("table");

    for fact in scenario
        .inspector
        .same_user_processes()
        .expect("inspect the process table")
    {
        // Whatever the kernel withholds from a process's argument region, the
        // lineage of a same-user process is served.
        let lineage = scenario
            .inspector
            .lineage(fact.pid)
            .unwrap_or_else(|error| panic!("lineage of {} ({}): {error:?}", fact.pid, fact.comm));
        if let Some(lineage) = lineage {
            assert!(
                lineage.parent < lineage.id,
                "{} ({}) was created before its creator",
                fact.pid,
                fact.comm
            );
        }
    }
}

#[tokio::test]
async fn a_lost_workers_hidden_descendants_are_reaped_through_its_spawn_id() {
    let mut scenario = Scenario::new("reap");
    let tree = scenario.launch_chain();
    let worker_id = scenario.spawn_id(tree.worker);
    let exposures = tree
        .members
        .iter()
        .map(|member| (member.name, scenario.exposure(member)))
        .collect::<Vec<_>>();
    // The fixture that must read back on every host is the control of the
    // independent read: it exposes its environment through `ps -E`.
    assert_eq!(
        exposures.last().map(|(_, exposure)| *exposure),
        Some(Exposure::Exposed),
        "the holding image is not a platform binary, so `ps -E` must show its marker"
    );
    // The scripts built this creator tree: `a` by the worker, the sleeper
    // under `a` and `b` by `a`, and the sleeper and holder under `b` by `b`.
    for (member, creator) in tree
        .members
        .iter()
        .zip([None, Some(0), Some(0), Some(2), Some(2)])
    {
        let expected = creator.map_or(worker_id, |index: usize| {
            scenario.spawn_id(tree.members[index].identity)
        });
        let lineage = scenario
            .inspector
            .lineage(member.identity.pid)
            .expect("inspect the member lineage")
            .expect("a live member reports its lineage");
        assert_eq!(
            lineage.parent, expected,
            "{} names its creator",
            member.name
        );
    }
    scenario.kill_and_wait(tree.worker);

    let report = sweep_runtime(
        &scenario.inspector,
        &scenario
            .request()
            .with_worker_start_identity(Some(tree.worker.start_identity))
            .with_worker_spawn_id(Some(worker_id)),
    )
    .await
    .expect("sweep the lost worker's runtime");

    let expected = tree
        .members
        .iter()
        .map(|member| key(member.identity))
        .collect::<BTreeSet<_>>();
    assert_eq!(
        reaped(&report),
        expected,
        "every process the worker created is signalled and observed to exit, \
         whether its marker is readable (exposures: {exposures:?}) or its lineage proves it"
    );
    assert!(report.unconfirmed.is_empty());
    for member in &tree.members {
        assert!(!scenario.is_running(member), "{} must be dead", member.name);
        assert_eq!(
            skip_reasons(&report, member.identity),
            Vec::new(),
            "{} is attributed, never skipped",
            member.name
        );
    }
}

#[tokio::test]
async fn hidden_processes_outside_the_workers_tree_are_dismissed_untouched() {
    let mut scenario = Scenario::new("dismiss");
    // Created before the worker: its own number is below the worker's.
    let before = scenario.spawn_sleeper();
    let tree = scenario.launch_chain();
    // Created after the worker, under a creator that is itself newer than the
    // worker: the lineage walk climbs past the creator to the test, which is
    // older than the worker.
    scenario.write_script("sibling.sh", SIBLING_CREATOR);
    let sibling_creator = scenario.spawn_script("sibling.sh", false);
    let sibling_creator = Member {
        name: "sibling creator",
        identity: sibling_creator,
        image: "/bin/sh".to_owned(),
    };
    let sibling_sleeper = scenario.published("sibling.pids")[0];
    let sibling_sleeper = scenario.member("sibling sleeper", sibling_sleeper, "/bin/sleep");
    let worker_id = scenario.spawn_id(tree.worker);
    assert!(
        scenario.spawn_id(sibling_creator.identity) > worker_id
            && scenario.spawn_id(before.identity) < worker_id,
        "the bystanders bracket the worker's creation"
    );
    scenario.kill_and_wait(tree.worker);

    let report = sweep_runtime(
        &scenario.inspector,
        &scenario.request().with_worker_spawn_id(Some(worker_id)),
    )
    .await
    .expect("sweep the lost worker's runtime");

    for bystander in [&before, &sibling_creator, &sibling_sleeper] {
        assert!(
            scenario.is_running(bystander),
            "{} is not part of the runtime and must survive",
            bystander.name
        );
        assert!(
            !is_listed(&report, bystander.identity),
            "{} is dismissed, not reported",
            bystander.name
        );
    }

    assert_eq!(
        reaped(&report),
        tree.members
            .iter()
            .map(|member| key(member.identity))
            .collect::<BTreeSet<_>>()
    );
}

#[tokio::test]
async fn a_hidden_descendant_of_a_dead_creator_is_never_signalled_unless_proven() {
    let mut scenario = Scenario::new("dead-creator");
    scenario.write_script("w.sh", WORKER_WITH_MORTAL_CREATOR);
    scenario.write_script("m.sh", MORTAL_CREATOR);
    let worker = scenario.spawn_script("w.sh", true);
    let creator = scenario.published("worker.pids")[0];
    let orphan = scenario.published("m.pids")[0];
    let creator = scenario.member("mortal creator", creator, "/bin/sh");
    let orphan = scenario.member("orphan sleeper", orphan, "/bin/sleep");
    let worker_id = scenario.spawn_id(worker);
    let exposure = scenario.exposure(&orphan);
    // The creator dies after the worker started and before the sweep, so the
    // orphan names a creator that is newer than the worker and no longer alive.
    scenario.kill_and_wait(creator.identity);
    poll_until("the dead creator to leave the process table", || {
        scenario
            .inspector
            .identity(creator.identity.pid)
            .expect("inspect the dead creator")
            .is_none()
            .then_some(())
    });
    scenario.kill_and_wait(worker);

    let report = sweep_runtime(
        &scenario.inspector,
        &scenario.request().with_worker_spawn_id(Some(worker_id)),
    )
    .await
    .expect("sweep the lost worker's runtime");

    assert_orphan_outcome(&scenario, &report, &orphan, exposure);
}

#[tokio::test]
async fn without_a_spawn_id_hidden_descendants_stay_skipped_as_unreadable() {
    let mut scenario = Scenario::new("no-spawn-id");
    let tree = scenario.launch_chain();
    let exposures = tree
        .members
        .iter()
        .map(|member| scenario.exposure(member))
        .collect::<Vec<_>>();
    scenario.kill_and_wait(tree.worker);

    // What a caller that records only the worker's start identity sends.
    let report = sweep_runtime(
        &scenario.inspector,
        &scenario
            .request()
            .with_worker_start_identity(Some(tree.worker.start_identity)),
    )
    .await
    .expect("sweep the lost worker's runtime");

    let mut expected_reaped = BTreeSet::new();
    for (member, exposure) in tree.members.iter().zip(&exposures) {
        match exposure {
            Exposure::Exposed => {
                expected_reaped.insert(key(member.identity));
                assert!(!scenario.is_running(member), "{} is marked", member.name);
            }
            Exposure::Withheld => {
                assert!(
                    scenario.is_running(member),
                    "{} cannot be attributed without a lineage and must survive",
                    member.name
                );
                assert_eq!(
                    skip_reasons(&report, member.identity),
                    vec![SkipReason::MarkersUnreadable],
                    "{} stays visible as unreadable",
                    member.name
                );
            }
        }
    }
    assert_eq!(reaped(&report), expected_reaped);
    assert!(report.unconfirmed.is_empty());
}

#[tokio::test]
async fn an_orphan_that_re_executes_after_losing_its_creator_is_never_dismissed() {
    let mut scenario = Scenario::new("orphan-exec");
    scenario.write_script("w.sh", WORKER_WITH_MORTAL_CREATOR);
    scenario.write_script("m.sh", ORPHANING_CREATOR);
    let worker = scenario.spawn_script("w.sh", true);
    let creator = scenario.published("worker.pids")[0];
    let orphan = scenario.published("orphan.pids")[0];
    let creator = scenario.member("mortal creator", creator, "/bin/sh");
    let image = image();
    // The fixture runs this test binary and waits for the creator to die, so
    // the image replacement it performs after that is the only `exec` that
    // follows the reparenting.
    scenario.member("waiting orphan", orphan, &image);
    let worker_id = scenario.spawn_id(worker);
    scenario.kill_and_wait(creator.identity);
    let orphan = scenario.member("orphan sleeper", orphan, "/bin/sleep");
    assert_eq!(
        scenario
            .inspector
            .parent_pid(orphan.identity.pid)
            .expect("inspect the orphan parent"),
        Some(1),
        "the creator is gone and the kernel handed the orphan to launchd"
    );
    let exposure = scenario.exposure(&orphan);
    scenario.kill_and_wait(worker);

    let report = sweep_runtime(
        &scenario.inspector,
        &scenario.request().with_worker_spawn_id(Some(worker_id)),
    )
    .await
    .expect("sweep the lost worker's runtime");

    assert_orphan_outcome(&scenario, &report, &orphan, exposure);
}
