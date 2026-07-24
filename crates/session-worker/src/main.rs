//! Runs one durable pohunek session worker.

// Rust guideline compliant 2026-07-23

use std::fmt::Write as _;
use std::fs;
use std::io::Read;
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::net::UnixDatagram;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use pohunek_paths::BasePaths;
use pohunek_session_worker::{Server, ServerArgs, WorkerConfig, WorkerError};
use tracing::{event, Level};
use tracing_subscriber::prelude::*;

/// Random bytes in a worker process identifier.
const WORKER_ID_RANDOM_BYTES: usize = 16;

#[tokio::main]
async fn main() -> ExitCode {
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

async fn run() -> Result<(), WorkerError> {
    let cli = Cli::parse(std::env::args().skip(1))?;
    let paths = BasePaths::resolve()?;
    let worker_id = match cli.worker_id {
        Some(worker_id) => worker_id,
        None => generate_worker_id()?,
    };
    let socket_path = paths
        .worker_socket(&cli.session_id)
        .ok_or_else(|| WorkerError::InvalidSessionId(cli.session_id.clone()))?;
    let journal_path = paths
        .worker_journal(&cli.session_id, &worker_id)
        .ok_or_else(|| WorkerError::InvalidWorkerId(worker_id.clone()))?;
    let _log_guard = init_logging(&paths.log_dir, &cli.session_id)?;

    let server = Server::bind(ServerArgs {
        session_id: cli.session_id.clone(),
        worker_id: worker_id.clone(),
        socket_path,
        journal_path,
        daemon_socket_path: cli.daemon_socket_path.unwrap_or(paths.socket),
        config: WorkerConfig::new(),
    })
    .await?;
    notify_systemd("READY=1\nSTATUS=Waiting for daemon initialization")?;
    event!(
        name: "worker.bootstrap.ready",
        Level::INFO,
        session.id = %cli.session_id,
        worker.id = %worker_id,
        "worker bootstrap ready for {{session.id}} as {{worker.id}}",
    );
    server.serve().await
}

#[derive(Debug)]
struct Cli {
    session_id: String,
    worker_id: Option<String>,
    daemon_socket_path: Option<PathBuf>,
}

impl Cli {
    fn parse(arguments: impl IntoIterator<Item = String>) -> Result<Self, WorkerError> {
        let mut arguments = arguments.into_iter();
        let mut session_id = None;
        let mut worker_id = None;
        let mut daemon_socket_path = None;
        while let Some(argument) = arguments.next() {
            match argument.as_str() {
                "--session-id" => {
                    session_id = arguments.next();
                }
                "--worker-id" => {
                    worker_id = arguments.next();
                }
                "--daemon-socket-path" => {
                    daemon_socket_path =
                        Some(PathBuf::from(arguments.next().ok_or_else(|| {
                            WorkerError::Protocol(
                                "argument --daemon-socket-path requires a value".to_owned(),
                            )
                        })?));
                }
                _ => {
                    return Err(WorkerError::Protocol(format!(
                        "unknown worker argument `{argument}`"
                    )));
                }
            }
        }
        let session_id = session_id.ok_or_else(|| {
            WorkerError::Protocol("required argument --session-id is missing".to_owned())
        })?;
        if pohunek_paths::valid_worker_session_id(&session_id).is_none() {
            return Err(WorkerError::InvalidSessionId(session_id));
        }
        if worker_id
            .as_deref()
            .is_some_and(|id| pohunek_paths::valid_worker_id(id).is_none())
        {
            return Err(WorkerError::InvalidWorkerId(worker_id.unwrap_or_default()));
        }
        Ok(Self {
            session_id,
            worker_id,
            daemon_socket_path,
        })
    }
}

fn init_logging(
    log_dir: &Path,
    session_id: &str,
) -> Result<tracing_appender::non_blocking::WorkerGuard, WorkerError> {
    fs::create_dir_all(log_dir).map_err(|source| WorkerError::Filesystem {
        path: log_dir.to_path_buf(),
        source,
    })?;
    fs::set_permissions(
        log_dir,
        <fs::Permissions as std::os::unix::fs::PermissionsExt>::from_mode(0o700),
    )
    .map_err(|source| WorkerError::Filesystem {
        path: log_dir.to_path_buf(),
        source,
    })?;
    let path = log_dir.join(format!("pohunek-session-{session_id}.jsonl"));
    let file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .open(&path)
        .map_err(|source| WorkerError::Filesystem {
            path: path.clone(),
            source,
        })?;
    let (writer, guard) = tracing_appender::non_blocking(file);
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
    use super::{generate_worker_id, Cli};

    #[test]
    fn cli_requires_safe_managed_session_id() {
        Cli::parse(["--session-id".to_owned(), "s-42".to_owned()]).expect("valid session");
        Cli::parse(["--session-id".to_owned(), "../bad".to_owned()])
            .expect_err("path-like session must fail");
    }

    #[test]
    fn generated_worker_id_is_safe() {
        let id = generate_worker_id().expect("operating-system entropy");
        assert!(pohunek_paths::valid_worker_id(&id).is_some());
    }
}
