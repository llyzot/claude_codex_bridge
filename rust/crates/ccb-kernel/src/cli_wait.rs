//! Legacy wait syntax over nonblocking reply snapshots; no restart or mutation.
use anyhow::{bail, ensure, Context, Result};
use serde_json::{json, Value};
use std::{
    ffi::OsString,
    path::Path,
    time::{Duration, Instant},
};
pub(crate) fn run(root: &Path, op: &str, args: &[OsString]) -> Result<()> {
    let mode = op.strip_prefix("wait-").context("invalid wait command")?;
    let mut positional = Vec::new();
    let mut timeout = None;
    let mut index = 0;
    while index < args.len() {
        let arg = args[index].to_str().context("invalid wait argument")?;
        if ["--timeout", "--timeout-ms"].contains(&arg) {
            ensure!(timeout.is_none(), "duplicate wait timeout");
            index += 1;
            let value = args
                .get(index)
                .and_then(|s| s.to_str())
                .context("missing wait timeout")?;
            timeout = Some(if arg == "--timeout-ms" {
                Duration::from_millis(value.parse()?)
            } else {
                let seconds: f64 = value.parse()?;
                ensure!(
                    seconds.is_finite() && seconds > 0.0 && seconds <= 86400.0,
                    "wait timeout must be finite, positive and at most one day"
                );
                Duration::from_secs_f64(seconds.max(0.1))
            });
        } else if arg.starts_with('-') {
            bail!("unknown wait option: {arg}");
        } else {
            positional.push(arg.to_owned());
        }
        index += 1;
    }
    ensure!(
        positional.len() == if mode == "quorum" { 2 } else { 1 },
        "wait requires an ID, preceded by a positive count for wait-quorum"
    );
    let mut payload = json!({"mode":mode,"target":positional.last().unwrap().trim()});
    if mode == "quorum" {
        let n: u64 = positional[0].parse()?;
        ensure!(n > 0, "wait quorum must be positive");
        payload["quorum"] = json!(n);
    }
    let env_seconds = |name: &str, default: f64, min: f64| {
        std::env::var(name)
            .ok()
            .and_then(|s| s.parse::<f64>().ok())
            .filter(|v| v.is_finite())
            .unwrap_or(default)
            .max(min)
    };
    let seconds = env_seconds("CCB_WAIT_TIMEOUT_S", 30.0, 0.1);
    ensure!(
        timeout.is_some() || seconds <= 86400.0,
        "wait timeout exceeds one day"
    );
    let timeout = timeout.unwrap_or_else(|| Duration::from_secs_f64(seconds));
    ensure!(
        timeout <= Duration::from_secs(86400),
        "wait timeout exceeds one day"
    );
    let poll =
        Duration::from_secs_f64(env_seconds("CCB_WAIT_POLL_INTERVAL_S", 0.1, 0.01).min(86400.0));
    let start = Instant::now();
    let mut project = Value::Null;
    loop {
        let mut row = crate::cli::request(root, "wait-summary", payload.clone())
            .context("wait observation failed; existing tasks were not cancelled or restarted")?;
        ensure!(
            row["target"] == payload["target"] && row["mode"] == payload["mode"],
            "wait response target changed"
        );
        if project.is_null() {
            project = row["project_id"].clone();
        } else {
            ensure!(
                project == row["project_id"],
                "wait response project changed"
            );
        }
        row["waited_s"] = json!(start.elapsed().as_secs_f64());
        if row["satisfied"] == true {
            println!("{}", crate::cli::request_preview(&row));
            return Ok(());
        }
        if start.elapsed() >= timeout {
            row["observation_timed_out"] = json!(true);
            println!("{}", crate::cli::request_preview(&row));
            bail!("wait observation timed out; tasks remain available under their original IDs");
        }
        std::thread::sleep(poll.min(timeout.saturating_sub(start.elapsed())));
    }
}
