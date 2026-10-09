//! Runs one durable pohunek session worker.

// Rust guideline compliant 2026-09-27

use std::fmt::Write as _;
use std::fs;
use std::io::Read;
use std::os::unix::net::UnixDatagram;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use pohunek_paths::{validate_socket_path, BasePaths, Platform, SocketKind};
use pohunek_service_config::ServiceConfig;
use pohunek_session_worker::{load_service_config, Server, ServerArgs, WorkerConfig, WorkerError};
use tracing::{event, Level};
use tracing_subscriber::prelude::*;

/// Random bytes in a worker process identifier.
const WORKER_ID_RANDOM_BYTES: usize = 16;

/// Sole argument that prints the binary name and version and exits.
///
/// `pohunek service install` runs it to confirm a staged executable before
/// referencing it, so it must work without any environment or paths.
const VERSION_FLAG: &str = "--version";

#[tokio::main]
async fn main() -> ExitCode {
    let arguments = std::env::args_os().skip(1).collect::<Vec<_>>();
    if arguments == [VERSION_FLAG] {
        return print_version();
    }
    match run().await {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("pohunek-sessiond: fatal: {error}");
            event!(
                name: "worker.process.failed",
                Level::ERROR,
                error.type = "worker",
                error.message = %error,
                "session worker failed: {{error.message}}",
            );
            ExitCode::FAILURE
        }
    }
}

/// Prints `pohunek-sessiond <version>` for the installer.
fn print_version() -> ExitCode {
    use std::io::Write as _;

    match writeln!(
        std::io::stdout(),
        "pohunek-sessiond {}",
        env!("CARGO_PKG_VERSION")
    ) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("pohunek-sessiond: failed to print the version: {error}");
            ExitCode::FAILURE
        }
    }
}

async fn run() -> Result<(), WorkerError> {
    let cli = Cli::parse(std::env::args().skip(1))?;
    let paths = BasePaths::resolve()?;
    let service = match &cli.service_config {
        Some(service_config) => {
            let executable =
                std::env::current_exe().map_err(|source| WorkerError::Executable { source })?;
            Some(load_service_config(
                service_config,
                rustix::process::geteuid().as_raw(),
                &paths,
                &executable,
            )?)
        }
        None => None,
    };
    let worker_id = match cli.worker_id {
        Some(worker_id) => worker_id,
        None => generate_worker_id()?,
    };
    let socket_path = paths
        .worker_socket(&cli.session_id)
        .map_err(WorkerError::Paths)?
        .ok_or_else(|| WorkerError::InvalidSessionId(cli.session_id.clone()))?;
    let journal_path = paths
        .worker_journal(&cli.session_id, &worker_id)
        .ok_or_else(|| WorkerError::InvalidWorkerId(worker_id.clone()))?;
    let daemon_socket_path = cli.daemon_socket_path.unwrap_or(paths.socket);
    validate_socket_path(
        &daemon_socket_path,
        Platform::current()?,
        SocketKind::Daemon,
    )?;
    let _log_guard = init_logging(&paths.log_dir, &cli.session_id)?;

    let server = Server::bind(ServerArgs {
        session_id: cli.session_id.clone(),
        worker_id: worker_id.clone(),
        generation: cli.generation.clone(),
        socket_path,
        journal_path,
        daemon_socket_path,
        config: worker_config(service.as_ref()),
    })
    .await?;
    notify_systemd("READY=1\nSTATUS=Waiting for daemon initialization")?;
    event!(
        name: "worker.bootstrap.ready",
        Level::INFO,
        session.id = %cli.session_id,
        worker.id = %worker_id,
        worker.generation = %cli.generation,
        "worker bootstrap ready for {{session.id}} as {{worker.id}} generation {{worker.generation}}",
    );
    server.serve().await
}

/// Builds the worker policy for a run with or without `--service-config`.
///
/// A supervised worker expires an uninitialized bootstrap after the recorded
/// `deadlines.worker_initialize_ms`, the same window the daemon waits before
/// retiring its job. A dev/test worker started as a direct child keeps the
/// [`WorkerConfig::new`] bootstrap window, which the daemon's subprocess
/// contract (`DEV_WORKER_INITIALIZE`) deliberately exceeds.
fn worker_config(service: Option<&ServiceConfig>) -> WorkerConfig {
    let mut config = WorkerConfig::new();
    if let Some(service) = service {
        config.initialize_deadline = service.deadlines().worker_initialize;
    }
    config
}

#[derive(Debug)]
struct Cli {
    session_id: String,
    generation: String,
    worker_id: Option<String>,
    daemon_socket_path: Option<PathBuf>,
    service_config: Option<PathBuf>,
}

impl Cli {
    fn parse(arguments: impl IntoIterator<Item = String>) -> Result<Self, WorkerError> {
        let mut arguments = arguments.into_iter();
        let mut session_id = None;
        let mut generation = None;
        let mut worker_id = None;
        let mut daemon_socket_path = None;
        let mut service_config = None;
        while let Some(argument) = arguments.next() {
            let slot = match argument.as_str() {
                "--session-id" => &mut session_id,
                "--worker-generation" => &mut generation,
                "--worker-id" => &mut worker_id,
                "--daemon-socket-path" => &mut daemon_socket_path,
                "--service-config" => &mut service_config,
                VERSION_FLAG => {
                    return Err(WorkerError::Protocol(format!(
                        "{VERSION_FLAG} must be the only argument"
                    )));
                }
                _ => {
                    return Err(WorkerError::Protocol(format!(
                        "unknown worker argument `{argument}`"
                    )));
                }
            };
            if slot.is_some() {
                return Err(WorkerError::Protocol(format!(
                    "argument {argument} was given more than once"
                )));
            }
            *slot = Some(arguments.next().ok_or_else(|| {
                WorkerError::Protocol(format!("argument {argument} requires a value"))
            })?);
        }
        let session_id = session_id.ok_or_else(|| {
            WorkerError::Protocol("required argument --session-id is missing".to_owned())
        })?;
        if pohunek_paths::valid_worker_session_id(&session_id).is_none() {
            return Err(WorkerError::InvalidSessionId(session_id));
        }
        let generation = generation.ok_or_else(|| {
            WorkerError::Protocol("required argument --worker-generation is missing".to_owned())
        })?;
        if pohunek_paths::valid_worker_generation(&generation).is_none() {
            return Err(WorkerError::InvalidGeneration(generation));
        }
        if worker_id
            .as_deref()
            .is_some_and(|id| pohunek_paths::valid_worker_id(id).is_none())
        {
            return Err(WorkerError::InvalidWorkerId(worker_id.unwrap_or_default()));
        }
        let service_config = service_config.map(PathBuf::from);
        if let Some(path) = service_config.as_ref().filter(|path| !path.is_absolute()) {
            return Err(WorkerError::Protocol(format!(
                "argument --service-config must be an absolute path, got {}",
                path.display()
            )));
        }
        Ok(Self {
            session_id,
            generation,
            worker_id,
            daemon_socket_path: daemon_socket_path.map(PathBuf::from),
            service_config,
        })
    }
}

fn init_logging(
    log_dir: &Path,
    session_id: &str,
) -> Result<tracing_appender::non_blocking::WorkerGuard, WorkerError> {
    let writer = pohunek_logging::Writer::open(
        log_dir,
        pohunek_logging::config::worker_files(session_id)?,
        pohunek_logging::config::worker_policy()?,
    )?;
    let (writer, guard) = tracing_appender::non_blocking(writer);
    tracing_subscriber::registry()
        .with(
            tracing_subscriber::fmt::layer()
                .json()
                .with_ansi(false)
                .with_writer(writer),
        )
        .try_init()
        .map_err(|error| {
            WorkerError::Protocol(format!("logging initialization failed: {error}"))
        })?;
    Ok(guard)
}

fn notify_systemd(message: &str) -> Result<(), WorkerError> {
    let Some(socket) = std::env::var_os("NOTIFY_SOCKET") else {
        return Ok(());
    };
    let path = PathBuf::from(socket);
    let datagram = UnixDatagram::unbound().map_err(|source| WorkerError::Socket {
        path: path.clone(),
        source,
    })?;

    #[cfg(target_os = "linux")]
    if let Some(name) = path.as_os_str().as_encoded_bytes().strip_prefix(b"@") {
        use std::os::linux::net::SocketAddrExt;

        let address =
            std::os::unix::net::SocketAddr::from_abstract_name(name).map_err(|source| {
                WorkerError::Socket {
                    path: path.clone(),
                    source,
                }
            })?;
        datagram
            .send_to_addr(message.as_bytes(), &address)
            .map(|_| ())
            .map_err(|source| WorkerError::Socket { path, source })?;
        return Ok(());
    }

    datagram
        .connect(&path)
        .and_then(|()| datagram.send(message.as_bytes()).map(|_| ()))
        .map_err(|source| WorkerError::Socket { path, source })
}

fn generate_worker_id() -> Result<String, WorkerError> {
    let mut bytes = [0_u8; WORKER_ID_RANDOM_BYTES];
    fs::File::open("/dev/urandom")
        .and_then(|mut file| file.read_exact(&mut bytes))
        .map_err(|source| WorkerError::Filesystem {
            path: PathBuf::from("/dev/urandom"),
            source,
        })?;
    let mut id = String::from("worker-");
    for byte in bytes {
        write!(&mut id, "{byte:02x}").expect("writing to String is infallible");
    }
    Ok(id)
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::time::Duration;

    use pohunek_service_config::{ConfigSpec, Deadlines, ServiceConfig};
    use pohunek_session_worker::WorkerConfig;

    use super::worker_config;

    /// A service configuration whose deadlines differ from every worker default.
    fn service_config(worker_initialize: Duration) -> ServiceConfig {
        let other = worker_initialize + Duration::from_millis(1);
        ServiceConfig::new(ConfigSpec {
            prefix: PathBuf::from("/home/u/.local"),
            active_version: "1.2.3".to_owned(),
            uid: 1000,
            state_root: PathBuf::from("/home/u/.local/state/pohunek"),
            runtime_root: PathBuf::from("/run/user/1000/pohunek"),
            deadlines: Deadlines {
                worker_connect: other,
                worker_initialize,
                launchctl_command: other,
                worker_exit_timeout: other,
                daemon_exit_timeout: other,
                daemon_restart_throttle: other,
            },
            environment_allowlist: vec!["PATH".to_owned()],
            search_path: pohunek_platform::shell_env::SearchPath::default(),
            sweep_grace: other,
            input_timing: pohunek_service_config::DEFAULT_INPUT_TIMING,
            open_files: 8192,
        })
        .expect("valid service configuration")
    }

    #[test]
    fn a_supervised_worker_uses_the_recorded_initialize_deadline() {
        let defaults = WorkerConfig::new();
        let recorded = defaults.initialize_deadline + Duration::from_secs(15);

        let config = worker_config(Some(&service_config(recorded)));

        assert_eq!(config.initialize_deadline, recorded);
        assert_eq!(
            config,
            WorkerConfig {
                initialize_deadline: recorded,
                ..defaults
            },
            "only the bootstrap window follows service.toml"
        );
        config.validate().expect("valid worker policy");
    }

    #[test]
    fn a_dev_worker_keeps_the_default_initialize_deadline() {
        assert_eq!(worker_config(None), WorkerConfig::new());
    }
}
