/// Prints structured core diagnostics and returns a failure exit code when a command fails.
fn main() {
    if let Err(error) = quarry_cli::run(std::env::args().skip(1)) {
        if let Some(core) = error.downcast_ref::<quarry_core::QuarryError>() {
            eprintln!(
                "error: {}",
                quarry_core::Diagnostic::from_error(core).details()
            );
        } else {
            eprintln!("error: {error}");
        }
        std::process::exit(1);
    }
}
