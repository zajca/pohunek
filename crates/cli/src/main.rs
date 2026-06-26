use std::process::ExitCode;

#[tokio::main]
async fn main() -> ExitCode {
    pohunek_cli::run_cli().await
}
