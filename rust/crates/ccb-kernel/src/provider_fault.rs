//! Safe durable fault identity. Provider error text/frames are never persisted.
use anyhow::{ensure, Result};
use serde_json::Value;

/// An owned provider lifetime ended before initialization or work admission. Repeating the
/// same launch would otherwise keep accepted tasks queued indefinitely.
#[derive(Debug)]
pub struct StartupExit(pub Option<i32>);
impl std::fmt::Display for StartupExit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "provider exited before initialization or task admission: {:?}",
            self.0
        )
    }
}
impl std::error::Error for StartupExit {}

#[derive(Debug)]
pub struct TransportClosed(pub &'static str);
impl std::fmt::Display for TransportClosed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "provider {} transport closed before owned process-tree exit",
            self.0
        )
    }
}
impl std::error::Error for TransportClosed {}

/// Allow an observed pipe closure to settle into ordinary process exit before
/// classifying a still-live root as a broken transport. Never infer completion.
#[derive(Default)]
pub struct TransportExitGrace(Option<std::time::Instant>);
impl TransportExitGrace {
    pub fn expired(&mut self, closed: bool, root_exited: bool) -> bool {
        self.observe(closed, root_exited, std::time::Instant::now())
    }
    fn observe(&mut self, closed: bool, root_exited: bool, now: std::time::Instant) -> bool {
        if !closed || root_exited {
            self.0 = None;
            return false;
        }
        now.saturating_duration_since(*self.0.get_or_insert(now))
            >= std::time::Duration::from_millis(500)
    }
}

pub fn path(provider: &str, agent: &str) -> String {
    format!("providers/{provider}/{agent}/fault.json")
}
pub fn seal_provider_scope(scope: &Value, row: &Value) -> Value {
    if scope["supported"] != true
        || !crate::execution_recovery::valid_process_scope(&scope["recovery"])
    {
        return Value::Null;
    }
    let mut proof = scope["recovery"].clone();
    proof["schema_version"] = serde_json::json!(1);
    proof["record_type"] = serde_json::json!("native_provider_scope");
    for key in [
        "project_id",
        "agent_name",
        "provider",
        "runtime_instance_id",
        "daemon_instance_id",
    ] {
        proof[key] = row[key].clone();
    }
    proof
}
pub fn scope_observation(row: &Value) -> Value {
    let proof = if !row["recovery_scope"].is_null() {
        &row["recovery_scope"]
    } else {
        &row["provider_scope"]
    };
    if proof.is_null() {
        serde_json::json!({"status":"unavailable","reason":"provider_scope_missing"})
    } else {
        crate::owned_process::probe_recovery(proof)
    }
}
pub fn validate(row: &Value, provider: &Value, agent: &str, project: &Value) -> Result<()> {
    ensure!(
        row["exit_code"].is_null()
            || row["exit_code"]
                .as_i64()
                .is_some_and(|code| i32::try_from(code).is_ok()),
        "invalid provider startup exit code"
    );
    ensure!(
        row["transport_closed"].is_null()
            || ["stdout", "stdin"].contains(&row["transport_closed"].as_str().unwrap_or("")),
        "invalid provider transport fault detail"
    );
    ensure!(
        row["schema_version"] == 1
            && row["record_type"] == "native_provider_fault"
            && row["agent_name"] == agent
            && row["provider"] == *provider
            && row["project_id"] == *project
            && ["blocked", "recovered"].contains(&row["status"].as_str().unwrap_or(""))
            && ["startup", "poll", "start", "terminate", "recovery"]
                .contains(&row["phase"].as_str().unwrap_or(""))
            && [
                "provider_start_failed",
                "provider_protocol_error",
                "provider_terminated",
                "result_delivery_uncertain"
            ]
            .contains(&row["reason"].as_str().unwrap_or(""))
            && row["fault_id"]
                .as_str()
                .is_some_and(|id| uuid::Uuid::parse_str(id).is_ok())
            && (row["pid"].is_null()
                || row["pid"]
                    .as_u64()
                    .is_some_and(|pid| pid > 0 && pid <= u32::MAX as u64)),
        "invalid native provider fault record"
    );
    if row["idle_recovery"] == true {
        ensure!(
            row["phase"] == "recovery"
                && row["reason"] == "result_delivery_uncertain"
                && row["delivery_failure"] == true
                && row["job_id"].as_str().is_some_and(|id| !id.is_empty())
                && row["execution_instance_id"]
                    .as_str()
                    .is_some_and(|id| uuid::Uuid::parse_str(id).is_ok()),
            "invalid idle delivery recovery record"
        );
    }
    ensure!(
        row["pid"].is_null() == row["runtime_instance_id"].is_null()
            && (row["runtime_instance_id"].is_null()
                || row["runtime_instance_id"]
                    .as_str()
                    .is_some_and(|id| uuid::Uuid::parse_str(id).is_ok()))
            && row["daemon_instance_id"]
                .as_str()
                .is_some_and(|id| !id.is_empty())
            && row["observed_at"]
                .as_str()
                .is_some_and(|time| chrono::DateTime::parse_from_rfc3339(time).is_ok()),
        "invalid native provider fault ownership"
    );
    if !row["recovery_scope"].is_null() {
        let proof = &row["recovery_scope"];
        ensure!(
            crate::execution_recovery::valid_shape(proof)
                && proof["agent_name"] == row["agent_name"]
                && proof["provider"] == row["provider"]
                && proof["project_id"] == row["project_id"]
                && proof["job_id"] == row["job_id"]
                && proof["execution_instance_id"] == row["execution_instance_id"]
                && proof["runtime_instance_id"] == row["runtime_instance_id"]
                && proof["provider_pid"] == row["pid"],
            "invalid provider recovery scope"
        );
    }
    if !row["provider_scope"].is_null() {
        let proof = &row["provider_scope"];
        ensure!(
            proof["schema_version"] == 1
                && proof["record_type"] == "native_provider_scope"
                && crate::execution_recovery::valid_process_scope(proof)
                && [
                    "project_id",
                    "agent_name",
                    "provider",
                    "runtime_instance_id",
                    "daemon_instance_id"
                ]
                .iter()
                .all(|key| proof[*key] == row[*key])
                && proof["provider_pid"] == row["pid"]
                && proof["job_id"].is_null()
                && proof["execution_instance_id"].is_null(),
            "invalid provider scope"
        );
        if !row["recovery_scope"].is_null() {
            ensure!(
                [
                    "scope_id",
                    "scope_name",
                    "owner_pid",
                    "owner_created",
                    "provider_pid",
                    "provider_created",
                    "windows_session",
                    "kill_on_owner_exit"
                ]
                .iter()
                .all(|key| proof[*key] == row["recovery_scope"][*key]),
                "conflicting provider scopes"
            );
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};
    #[test]
    fn transport_grace_never_overrides_a_known_root_exit() {
        let mut grace = TransportExitGrace::default();
        let start = Instant::now();
        assert!(!grace.observe(true, false, start));
        assert!(!grace.observe(true, false, start + Duration::from_millis(499)));
        assert!(grace.observe(true, false, start + Duration::from_millis(500)));
        assert!(!grace.observe(true, true, start + Duration::from_secs(3)));
        assert!(!grace.observe(true, false, start + Duration::from_secs(4)));
        assert!(!grace.observe(false, false, start + Duration::from_secs(5)));
        assert!(!grace.observe(true, false, start + Duration::from_secs(6)));
    }
}
