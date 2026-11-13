fn main() {
    match xtask::run(std::env::args_os().skip(1)) {
        Ok(()) => {}
        Err(xtask::XtaskError::ExitCode(code)) => std::process::exit(code),
        Err(error) => {
            eprintln!("error: {error}");
            std::process::exit(error.exit_code());
        }
    }
}
