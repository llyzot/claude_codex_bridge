//! Cold recovery requires sealed execution ownership and fresh OS observations.
use serde_json::{json, Value};

pub fn seal(job: &Value, scope: &Value, runtime: &Value) -> Value {
    if scope["supported"] != true || scope["recovery"].is_null() {
        return Value::Null;
    }
    let mut proof = scope["recovery"].clone();
    proof["schema_version"] = json!(1);
    for key in ["job_id", "agent_name", "provider"] {
        proof[key] = job[key].clone();
    }
    proof["project_id"] = job["request"]["project_id"].clone();
    proof["execution_instance_id"] = job["provider_options"]["native_execution_instance"].clone();
    proof["daemon_instance_id"] = job["provider_options"]["native_daemon_instance"].clone();
    proof["runtime_instance_id"] = runtime["runtime_instance_id"].clone();
    proof
}
pub fn inspect(job: &Value, current: &str) -> Value {
    let unavailable = |reason| json!({"status":"unavailable","reason":reason});
    if job["status"] != "running" {
        return json!({"status":if ["completed","failed","incomplete","cancelled"].contains(&job["status"].as_str().unwrap_or("")) {"terminal"} else {"pending"},"reason":"not_running"});
    }
    if job["provider_options"]["native_daemon_instance"] == current {
        return json!({"status":"pending","reason":"current_owner"});
    }
    let p = &job["provider_options"]["native_scope_proof"];
    if p.is_null() {
        return unavailable("ownership_proof_missing");
    }
    if !valid_shape(p)
        || ["job_id", "agent_name", "provider"]
            .iter()
            .any(|key| p[*key] != job[*key])
        || p["project_id"] != job["request"]["project_id"]
        || p["execution_instance_id"] != job["provider_options"]["native_execution_instance"]
        || p["daemon_instance_id"] != job["provider_options"]["native_daemon_instance"]
    {
        return unavailable("ownership_proof_invalid");
    }
    let intent = &job["provider_options"]["native_cancel_intent"];
    if !intent.is_null() && !intent["record_reply"].is_boolean() {
        return unavailable("cancel_intent_invalid");
    }
    crate::owned_process::probe_recovery(p)
}
pub fn valid_shape(p: &Value) -> bool {
    let valid_uuid = |v: &Value| {
        v.as_str()
            .is_some_and(|s| uuid::Uuid::parse_str(s).is_ok_and(|id| id.to_string() == s))
    };
    let valid_pid = |v: &Value| {
        v.as_u64()
            .is_some_and(|pid| pid > 0 && pid <= u32::MAX as u64)
    };
    let valid_time = |v: &Value| {
        v.as_str()
            .is_some_and(|s| s.parse::<u64>().is_ok_and(|n| n > 0 && n.to_string() == s))
    };
    !(p["schema_version"] != 1
        || p["kind"] != "windows_job"
        || p["kill_on_owner_exit"] != true
        || !valid_uuid(&p["scope_id"])
        || !valid_uuid(&p["runtime_instance_id"])
        || !valid_uuid(&p["execution_instance_id"])
        || p["daemon_instance_id"]
            .as_str()
            .is_none_or(|id| id.is_empty())
        || !valid_pid(&p["owner_pid"])
        || !valid_pid(&p["provider_pid"])
        || !valid_time(&p["owner_created"])
        || !valid_time(&p["provider_created"])
        || p["windows_session"]
            .as_u64()
            .is_none_or(|id| id > u32::MAX as u64)
        || p["scope_name"]
            != format!(
                "Local\\CCB.Native.Provider.{}",
                p["scope_id"].as_str().unwrap_or("")
            ))
}
pub fn interrupted(job: &Value, observed: &Value, timestamp: &str) -> Value {
    let intent = &job["provider_options"]["native_cancel_intent"];
    let cancel = intent["record_reply"].is_boolean();
    json!({"schema_version":2,"record_type":"completion_decision","terminal":true,
        "status":if cancel {"cancelled"} else {"incomplete"},"reason":"native_owner_stopped","confidence":"observed",
        "reply":"The previous Rust runtime and its owned process scope have stopped without a durable terminal result. Work may already have taken effect. Inspect the saved provider session before explicitly retrying.",
        "anchor_seen":false,"reply_started":false,"reply_stable":false,"provider_turn_ref":null,"source_cursor":null,"finished_at":timestamp,
        "diagnostics":{"completion_authority":"windows_owned_scope_recovery","owner_observation":observed,
            "scope_id":job["provider_options"]["native_scope_proof"]["scope_id"],"automatic_replay":false,
            "recovery_no_replay":true,"delivery_retryable":false,"suppress_completion_state_merge":true,
            "cancel_requested":cancel,"result_reply_enabled":intent["record_reply"]!=false,"deadline_triggered":intent["deadline_triggered"]==true}})
}
