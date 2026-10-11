//! Recreate only a confirmed cleared, never subsequently dispatched empty context.
use anyhow::Result;
use serde_json::{json, Value};

pub(crate) fn marker(job: &Value, identity: &Value) -> Value {
    json!({"kind":"confirmed_clear","session_id":identity,"job_id":job["job_id"],
        "operation_id":job["provider_options"]["native_context_control"]["operation_id"]})
}
pub(crate) fn may_recreate(state: &Value, provider: &str) -> Result<bool> {
    let marker = &state["native_empty_context"];
    let (identity, path) = if provider == "pi" {
        ("sessionId", "sessionFile")
    } else {
        ("thread_id", "path")
    };
    if marker["kind"] != "confirmed_clear"
        || marker["session_id"] != state[identity]
        || !marker["session_id"].is_string()
        || !marker["job_id"].is_string()
        || provider == "pi" && state["messageCount"].as_u64() != Some(0)
    {
        return Ok(false);
    }
    let Some(path) = state[path].as_str().filter(|p| !p.is_empty()) else {
        return Ok(provider == "codex");
    };
    match std::fs::metadata(path) {
        Ok(_) => Ok(false),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(true),
        Err(error) => Err(error.into()),
    }
}
pub(crate) fn restore_marker(state: &mut Value, saved: &Value, identity: &str, recreated: bool) {
    let marker = &saved["native_empty_context"];
    if marker["kind"] == "confirmed_clear" && (recreated || marker["session_id"] == state[identity])
    {
        let mut marker = marker.clone();
        if recreated {
            marker["recreated_from"] = saved[identity].clone();
        }
        marker["session_id"] = state[identity].clone();
        state["native_empty_context"] = marker;
    }
}
