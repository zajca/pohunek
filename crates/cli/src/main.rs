use std::process::ExitCode;
use std::time::Duration;

/// Longest wait for blocking tasks when the runtime shuts down: a write to a
/// terminal or pipe that takes no output must not keep the process alive after
/// the command finished.
const RUNTIME_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(1);

fn main() -> ExitCode {
    // Runs before the async runtime starts its threads, so removing the
    // variable from the environment races no reader.
    pohunek_cli::service::inherited::capture();
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("the async runtime starts");
    // Box the large entrypoint future so the top-level task stays small.
    let code = runtime.block_on(Box::pin(pohunek_cli::run_cli()));
    runtime.shutdown_timeout(RUNTIME_SHUTDOWN_TIMEOUT);
    code
}
