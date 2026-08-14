fn main() {
    if let Err(error) = quarry_cli::run(std::env::args().skip(1)) {
        eprintln!("error: {error}");
        std::process::exit(1);
    }
}
