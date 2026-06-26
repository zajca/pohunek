use std::process::ExitCode;

#[tokio::main]
async fn main() -> ExitCode {
    // Box the large entrypoint future so the top-level task stays small.
    Box::pin(pohunek_cli::run_cli()).await
}
