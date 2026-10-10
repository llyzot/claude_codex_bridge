//! Retry admission preserves logical-message ownership in one transaction.
use crate::{admission, bureau, records, Kernel};
use anyhow::{ensure, Result};
use serde_json::{json, Value};
fn lookup(kernel: &mut Kernel, stream: &str, fields: Value) -> Result<Value> {
    records::dispatch(
        &mut kernel.store,
        "find_last",
        &json!({"stream":stream,"fields":fields}),
    )
}
pub fn source(kernel: &mut Kernel, target: &str) -> Result<Value> {
    let mut attempt = lookup(kernel, "attempts", json!({"attempt_id":target}))?;
    if attempt.is_null() {
        attempt = lookup(kernel, "attempts", json!({"job_id":target}))?;
    }
    ensure!(!attempt.is_null(), "retry target not found: {target}");
    Ok(attempt)
}
pub fn admit(kernel: &mut Kernel, args: &Value) -> Result<Value> {
    let target = records::string(args, "target")?;
    let original = source(kernel, target)?;
    let id = records::string(&original, "attempt_id")?;
    let state = records::string(&original, "attempt_state")?;
    ensure!(
        [
            "completed",
            "incomplete",
            "failed",
            "cancelled",
            "superseded",
            "dead_letter"
        ]
        .contains(&state),
        "attempt is still active: {id}"
    );
    ensure!(
        state != "completed",
        "retry is not allowed for completed attempts: {id}"
    );
    let agent = records::string(&original, "agent_name")?;
    let attempts = records::dispatch(
        &mut kernel.store,
        "latest_filter",
        &json!({"stream":"attempts","identity":"attempt_id","fields":{"message_id":original["message_id"],"agent_name":agent}}),
    )?;
    let latest = attempts
        .as_array()
        .unwrap()
        .iter()
        .max_by(|a, b| {
            (
                a["retry_index"].as_i64().unwrap_or(0),
                a["started_at"].as_str().unwrap_or(""),
                a["attempt_id"].as_str().unwrap_or(""),
            )
                .cmp(&(
                    b["retry_index"].as_i64().unwrap_or(0),
                    b["started_at"].as_str().unwrap_or(""),
                    b["attempt_id"].as_str().unwrap_or(""),
                ))
        })
        .ok_or_else(|| anyhow::anyhow!("message is missing attempt lineage for agent: {agent}"))?;
    ensure!(
        latest["attempt_id"] == original["attempt_id"],
        "retry requires latest attempt for agent: {agent}"
    );
    ensure!(
        args["dispatch_disabled"] != true,
        "agent {agent} is dispatch-disabled"
    );
    let message = lookup(
        kernel,
        "messages",
        json!({"message_id":original["message_id"]}),
    )?;
    ensure!(!message.is_null(), "message not found for attempt: {id}");
    let current = records::dispatch(
        &mut kernel.store,
        "find_last",
        &json!({"stream":"jobs","agent_name":agent,"fields":{"job_id":original["job_id"]}}),
    )?;
    ensure!(!current.is_null(), "job not found for attempt: {id}");
    let timestamp = records::string(args, "accepted_at")?;
    let pending = kernel.dispatch(
        "scheduler.has_outstanding_for",
        &json!({"state_id":args["state_id"],"target_kind":"agent","target_name":agent}),
    )?;
    let status = if pending == true {
        "queued"
    } else {
        "accepted"
    };
    let mut request = current["request"].clone();
    let mut options = current["provider_options"]
        .as_object()
        .cloned()
        .unwrap_or_default();
    let continue_mode = request["message_type"]
        .as_str()
        .is_some_and(|s| s.trim().eq_ignore_ascii_case("ask"))
        && (options
            .get("retry_delivery_mode")
            .and_then(Value::as_str)
            .is_some_and(|s| s.trim().eq_ignore_ascii_case("continue"))
            || records::truthy(&current["terminal_decision"]["anchor_seen"])
            || records::truthy(&current["terminal_decision"]["reply_started"]));
    if continue_mode {
        request["body"] = json!("continue");
        options.insert("retry_delivery_mode".into(), json!("continue"));
    } else {
        options.remove("retry_delivery_mode");
    }
    options.insert("retry_source_job_id".into(), current["job_id"].clone());
    let job = json!({"schema_version":2,"record_type":"job_record","job_id":admission::new_id("job"),"submission_id":message["submission_id"],
        "agent_name":agent,"provider":current["provider"],"provider_instance":null,"provider_options":options,"workspace_path":current["workspace_path"],
        "target_kind":"agent","target_name":agent,"request":request,"status":status,"terminal_decision":null,"cancel_requested_at":null,
        "created_at":timestamp,"updated_at":timestamp});
    admission::append(kernel, "jobs", &job)?;
    admission::event(
        kernel,
        &job,
        if status == "accepted" {
            "job_accepted"
        } else {
            "job_queued"
        },
        json!({"status":status}),
        timestamp,
    )?;
    let attempt = bureau::retry(
        kernel,
        &json!({"message_id":original["message_id"],"job":job,"accepted_at":timestamp}),
    )?;
    if records::truthy(&message["submission_id"]) {
        let mut submission = lookup(
            kernel,
            "submissions",
            json!({"submission_id":message["submission_id"]}),
        )?;
        if !submission.is_null() {
            let jobs = submission["job_ids"]
                .as_array_mut()
                .ok_or_else(|| anyhow::anyhow!("invalid submission jobs"))?;
            if !jobs.contains(&job["job_id"]) {
                jobs.push(job["job_id"].clone());
            }
            submission["updated_at"] = json!(timestamp);
            admission::append(kernel, "submissions", &submission)?;
        }
    }
    kernel.dispatch(
        "scheduler.admit",
        &json!({"state_id":args["state_id"],"jobs":[job]}),
    )?;
    Ok(
        json!({"accepted_at":timestamp,"target":target,"message_id":original["message_id"],"original_attempt_id":original["attempt_id"],
        "attempt_id":attempt,"job_id":job["job_id"],"agent_name":agent,"status":status}),
    )
}
