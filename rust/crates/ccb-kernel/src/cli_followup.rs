use anyhow::{bail, ensure, Context, Result};
use serde_json::json;
use std::{
    ffi::OsString,
    path::Path,
    time::{Duration, Instant},
};
pub(crate) fn run(root: &Path, args: &[OsString]) -> Result<()> {
    let job = args
        .first()
        .and_then(|s| s.to_str())
        .context("followup requires a job ID")?;
    let mut message = None;
    let mut operation = None;
    let mut timeout = None;
    let mut i = 1;
    while i < args.len() {
        let option = args[i].to_str().context("invalid option")?;
        i += 1;
        let value = args
            .get(i)
            .and_then(|s| s.to_str())
            .context("missing option value")?;
        match option {
            "--message" => {
                ensure!(message.is_none(), "duplicate message");
                message = Some(value.to_owned());
            }
            "--operation-id" => {
                ensure!(operation.is_none(), "duplicate operation ID");
                operation = Some(uuid::Uuid::parse_str(value)?);
            }
            "--timeout-ms" => {
                ensure!(timeout.is_none(), "duplicate timeout");
                let ms: u64 = value.parse()?;
                ensure!(ms <= 60000, "timeout exceeds 60000 ms");
                timeout = Some(Duration::from_millis(ms));
            }
            _ => bail!("unknown followup option: {option}"),
        }
        i += 1;
    }
    let operation = operation.unwrap_or_else(uuid::Uuid::new_v4);
    let id = format!("fup_{}", operation.simple());
    let message = message.context("followup requires --message")?;
    let mut result = crate::cli::request(root,"followup",json!({"job_id":job,"message":message,"operation_id":operation.to_string()}))
        .with_context(||format!("followup response not observed; inspect {id}, or retry same job/message with --operation-id {operation}"))?;
    let project = result["project_id"].clone();
    let timeout = timeout.unwrap_or(Duration::from_secs(5));
    let started = Instant::now();
    while ["accepted", "sending"].contains(&result["status"].as_str().unwrap_or(""))
        && started.elapsed() < timeout
    {
        std::thread::sleep(
            Duration::from_millis(50).min(timeout.saturating_sub(started.elapsed())),
        );
        result = crate::cli::request(root,"followup-status",json!({"followup_id":id}))
            .with_context(||format!("followup observation failed; inspect {id}; do not create a replacement operation"))?;
        ensure!(
            result["project_id"] == project
                && result["job_id"] == job
                && result["followup_id"] == id,
            "followup observation identity changed"
        );
    }
    println!("{}", crate::cli::request_preview(&result));
    ensure!(result["status"] == "injected","followup not confirmed injected; inspect {id}; the task has not been cancelled or restarted");
    Ok(())
}
