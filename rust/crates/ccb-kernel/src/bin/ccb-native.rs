fn main() {
    if let Err(error) = ccb_kernel::cli::run(std::env::args_os().skip(1).collect()) {
        eprintln!("ccb-native: {error:#}");
        std::process::exit(1);
    }
}
