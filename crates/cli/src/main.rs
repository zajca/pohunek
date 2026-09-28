use std::process::ExitCode;

fn main() -> ExitCode {
    // SAFETY: this is the first statement of `main`: no thread has started
    // and this process has opened no descriptor yet.
    unsafe { pohunek_cli::service::inherited::capture() };
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("the async runtime starts");
    // Box the large entrypoint future so the top-level task stays small.
    runtime.block_on(Box::pin(pohunek_cli::run_cli()))
}
