//! Native context controls return durable receipt identities before observation.
use anyhow::{bail, ensure, Context, Result};
use serde_json::json;
use std::{
    ffi::OsString,
    path::Path,
    time::{Duration, Instant},
};

pub(crate) fn run(root: &Path, action: &str, args: &[OsString]) -> Result<()> {
    let args = args
        .iter()
        .map(|s| s.to_str().context("context arguments must be UTF-8"))
        .collect::<Result<Vec<_>>>()?;
    let mut operation = uuid::Uuid::new_v4().to_string();
    let mut names = Vec::new();
    let (mut dry_run, mut wait, mut timeout) = (false, false, 15_000u64);
    let mut i = 0;
    while i < args.len() {
        match args[i] {
            "--dry-run" => dry_run = true,
            "--wait" => wait = true,
            "--operation-id" => {
                i += 1;
                operation = args
                    .get(i)
                    .context("--operation-id requires a UUID")?
                    .to_string();
            }
            "--timeout-ms" => {
                i += 1;
                timeout = args
                    .get(i)
                    .context("--timeout-ms requires a value")?
                    .parse()?;
            }
            other if other.starts_with('-') => bail!("unknown {action} option: {other}"),
            name => names.push(name),
        }
        i += 1;
    }
    ensure!(
        uuid::Uuid::parse_str(&operation).is_ok(),
        "invalid context operation identity"
    );
    ensure!(
        timeout <= 86_400_000,
        "context observation timeout exceeds one day"
    );
    let mut result = crate::cli::request(root, action, json!({"operation_id":operation,"agent_names":names,"dry_run":dry_run}))
        .with_context(|| format!("context request {operation} failed; inspect context-status {operation} before retrying"))?;
    let deadline = Instant::now() + Duration::from_millis(timeout);
    while wait && !dry_run && result["context"]["status"] == "pending" {
        if Instant::now() >= deadline {
            result["observation_timeout"] = json!(true);
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
        result = crate::cli::request(root, "context-status", json!({"operation_id":operation}))
            .with_context(|| {
                format!("context observation failed; resume context-status {operation}")
            })?;
        ensure!(
            !result["context"].is_null(),
            "context receipt unavailable: {operation}"
        );
    }
    println!("{result}");
    ensure!(
        !["blocked", "failed"].contains(&result["context"]["status"].as_str().unwrap_or("")),
        "context operation did not complete successfully; inspect the returned result"
    );
    Ok(())
}
