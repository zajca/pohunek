use std::process::ExitCode;

fn main() -> ExitCode {
    // Runs before the async runtime starts its threads, so removing the
    // variable from the environment races no reader.
    pohunek_cli::service::inherited::capture();
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("the async runtime starts");
    // Box the large entrypoint future so the top-level task stays small.
    runtime.block_on(Box::pin(pohunek_cli::run_cli()))
}
