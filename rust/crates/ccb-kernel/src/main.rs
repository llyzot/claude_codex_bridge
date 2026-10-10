use anyhow::{ensure, Result};
use ccb_kernel::{control_plane, rpc, Kernel};
use std::io::{self, BufRead, Write};
use std::path::PathBuf;
fn main() {
    if let Err(error) = run() {
        eprintln!("ccb-kernel: {error:#}");
        std::process::exit(1);
    }
}
fn run() -> Result<()> {
    let args: Vec<_> = std::env::args_os().skip(1).collect();
    ensure!(
        (args.len() == 2 || args.len() == 4) && args[0] == "--state-root",
        "usage: ccb-kernel --state-root <directory> [--token-file <path>]"
    );
    if args.len() == 4 {
        ensure!(
            args[2] == "--token-file",
            "invalid network serving arguments"
        );
        return control_plane::serve(&PathBuf::from(&args[1]), &PathBuf::from(&args[3]));
    }
    let mut kernel = Kernel::open(&PathBuf::from(&args[1]))?;
    let stdin = io::stdin();
    let mut stdout = io::stdout().lock();
    for line in stdin.lock().lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let response = rpc::execute(&mut kernel, &line)?;
        writeln!(stdout, "{response}")?;
        stdout.flush()?;
    }
    Ok(())
}
