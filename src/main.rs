fn main() {
    if let Err(error) = smol_os::cli::run() {
        eprintln!("picos: {error}");
        std::process::exit(1);
    }
}
