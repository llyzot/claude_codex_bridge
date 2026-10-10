use anyhow::{ensure, Result};
use ccb_kernel::{Kernel, API_VERSION};
use serde::Deserialize;
use serde_json::{json, Value};
use std::io::{self, BufRead, Write};
use std::path::PathBuf;

#[derive(Deserialize)]
struct Request {
    api_version: u32,
    id: u64,
    op: String,
    #[serde(default)]
    args: Value,
}
fn main() {
    if let Err(error) = run() {
        eprintln!("ccb-kernel: {error:#}");
        std::process::exit(1);
    }
}
fn run() -> Result<()> {
    let args: Vec<_> = std::env::args_os().skip(1).collect();
    ensure!(
        args.len() == 2 && args[0] == "--state-root",
        "usage: ccb-kernel --state-root <ccbd-directory>"
    );
    let mut kernel = Kernel::open(&PathBuf::from(&args[1]))?;
    let stdin = io::stdin();
    let mut stdout = io::stdout().lock();
    for line in stdin.lock().lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let response = match serde_json::from_str::<Request>(&line) {
            Ok(request) if request.api_version == API_VERSION => {
                match kernel.dispatch(&request.op, &request.args) {
                    Ok(result) => {
                        // A failed commit exits with its journal intact. Do not
                        // serve further requests against a half-applied state.
                        kernel.store.commit()?;
                        json!({"api_version":API_VERSION,"id":request.id,"ok":true,"result":result})
                    }
                    Err(error) => {
                        kernel.store.rollback();
                        json!({"api_version":API_VERSION,"id":request.id,"ok":false,"error":error.to_string()})
                    }
                }
            }
            Ok(request) => {
                json!({"api_version":API_VERSION,"id":request.id,"ok":false,"error":"unsupported api_version"})
            }
            Err(error) => {
                json!({"api_version":API_VERSION,"id":null,"ok":false,"error":error.to_string()})
            }
        };
        writeln!(stdout, "{response}")?;
        stdout.flush()?;
    }
    Ok(())
}
