//! Durable native observation, distinct from assistant text or process death.
//! Only the authority writes this receipt, before its completion transaction.
use crate::{
    records,
    terminal_history::{self, Binding},
};
use anyhow::{ensure, Context, Result};
use serde_json::{json, Value};
use std::fs::File;

pub(crate) fn attach(decision: &mut Value, live: &Value, binding: &Binding) {
    decision["diagnostics"]["herdr_idle_observation"] = json!({"schema_version":1,
        "identity":live["identity"],"interactive_ready":live["interactive_ready"],
        "agent_session":live["agent_session"],"history_binding":binding,"observed_at":crate::daemon::now()});
    decision["diagnostics"]["completion_authority"] = json!("herdr_idle_and_anchored_transcript");
}

pub(crate) fn prepare(job: &Value, spec: &Value, decision: &Value) -> Result<Value> {
    let execution = records::string(&job["provider_options"], "native_execution_instance")?;
    uuid::Uuid::parse_str(execution)?;
    let anchor = format!("{}:{execution}", records::string(job, "job_id")?);
    let permit = &job["provider_options"]["herdr_dispatch"];
    let observed = &decision["diagnostics"]["herdr_idle_observation"];
    ensure!(
        job["provider"] == "claude"
            && spec["provider"] == "claude"
            && spec["driver_kind"] == "herdr_tui"
            && job["status"] == "running"
            && permit["schema_version"] == 1
            && permit["job_id"] == job["job_id"]
            && permit["execution_instance_id"] == execution
            && permit["request_anchor"] == anchor,
        "Claude terminal observation execution changed"
    );
    ensure!(
        decision["schema_version"] == 2
            && decision["record_type"] == "completion_decision"
            && decision["terminal"] == true
            && ["completed", "failed", "incomplete", "cancelled"]
                .contains(&decision["status"].as_str().unwrap_or(""))
            && decision["diagnostics"]["native_execution_instance"] == execution
            && decision["diagnostics"]["request_anchor"] == anchor
            && decision["diagnostics"]["completion_authority"]
                == "herdr_idle_and_anchored_transcript"
            && observed["schema_version"] == 1
            && observed["interactive_ready"] == true
            && observed["identity"] == permit["identity"]
            && observed["identity"]["process"].is_object(),
        "Claude terminal observation lacks exact idle authority"
    );
    let binding: Binding = serde_json::from_value(observed["history_binding"].clone())?;
    binding.validate()?;
    ensure!(
        decision["diagnostics"]["session_id"] == binding.session_id
            && json!(binding.cwd) == spec["workspace_path"]
            && (observed["agent_session"].is_null()
                || observed["agent_session"]["agent"] == "claude"
                    && observed["agent_session"]["kind"] == "id"
                    && observed["agent_session"]["value"] == binding.session_id),
        "Claude terminal observation session changed"
    );
    if spec["herdr_discovery"].is_object() {
        crate::herdr_discovery::validate(&spec["herdr_discovery"], &spec["workspace_path"])?;
        ensure!(
            permit["discovery"] == spec["herdr_discovery"]
                && json!(binding.root) == spec["herdr_discovery"]["root"]
                && (spec["herdr_discovery"]["session_id"].is_null()
                    || spec["herdr_discovery"]["session_id"] == binding.session_id),
            "Claude terminal observation discovery scope changed"
        );
    } else {
        ensure!(
            observed["history_binding"] == spec["herdr_history"]
                && permit["history_binding"] == spec["herdr_history"],
            "Claude terminal observation fixed history changed"
        );
    }
    let cursor = &decision["source_cursor"];
    let offset = cursor["offset"]
        .as_u64()
        .context("Claude terminal observation cursor missing")?;
    ensure!(
        cursor["schema_version"] == 2
            && cursor["record_type"] == "completion_cursor"
            && cursor["source_kind"] == "session_event_log"
            && cursor["session_path"] == json!(binding.path)
            && !cursor["identity"].is_null()
            && cursor["digest"]
                .as_str()
                .is_some_and(|d| d.len() == 64 && d.bytes().all(|b| b.is_ascii_hexdigit())),
        "Claude terminal observation cursor invalid"
    );
    let mut file = File::open(&binding.path)?;
    ensure!(
        terminal_history::identity(&file)? == cursor["identity"]
            && terminal_history::boundary_digest(&mut file, offset)? == cursor["digest"],
        "Claude observed terminal history replaced or rewritten"
    );
    if !permit["baseline"].is_null() {
        ensure!(
            cursor["identity"] == permit["baseline"]["identity"]
                && offset
                    >= permit["baseline"]["offset"]
                        .as_u64()
                        .context("Claude dispatch baseline invalid")?,
            "Claude terminal observation precedes its dispatch baseline"
        );
    }
    Ok(
        json!({"schema_version":1,"record_type":"herdr_terminal_observation",
        "job_id":job["job_id"],"agent_name":job["agent_name"],"project_id":job["request"]["project_id"],
        "execution_instance_id":execution,"decision":decision}),
    )
}

pub(crate) fn recover(
    job: &Value,
    spec: &Value,
    identity: &Value,
    binding: &Binding,
    candidate: Option<Value>,
) -> Result<Option<Value>> {
    let receipt = &job["provider_options"]["herdr_terminal_observation"];
    if receipt.is_null() {
        return Ok(None);
    }
    ensure!(
        prepare(job, spec, &receipt["decision"])? == *receipt
            && receipt["decision"]["diagnostics"]["herdr_idle_observation"]["identity"]
                == *identity
            && receipt["decision"]["diagnostics"]["herdr_idle_observation"]["history_binding"]
                == json!(binding),
        "Claude durable terminal observation binding changed"
    );
    let Some(candidate) = candidate else {
        return Ok(None);
    };
    let saved = &receipt["decision"];
    // Later hook feedback, tools, inputs or a different assistant result invalidate
    // the prior observation. Re-reading history is mandatory, even with a receipt.
    let same = ["status", "reason", "reply", "provider_turn_ref"]
        .iter()
        .all(|k| candidate[*k] == saved[*k])
        && ["session_id", "request_anchor", "assistant_uuid"]
            .iter()
            .all(|k| candidate["diagnostics"][*k] == saved["diagnostics"][*k]);
    Ok(same.then(|| saved.clone()))
}
