//! Safe durable fault identity. Provider error text/frames are never persisted.
use anyhow::{ensure, Result};
use serde_json::Value;

pub fn path(provider: &str, agent: &str) -> String {
    format!("providers/{provider}/{agent}/fault.json")
}
pub fn validate(row: &Value, provider: &Value, agent: &str, project: &Value) -> Result<()> {
    ensure!(
        row["schema_version"] == 1
            && row["record_type"] == "native_provider_fault"
            && row["agent_name"] == agent
            && row["provider"] == *provider
            && row["project_id"] == *project
            && ["blocked", "recovered"].contains(&row["status"].as_str().unwrap_or(""))
            && ["startup", "poll", "start"].contains(&row["phase"].as_str().unwrap_or(""))
            && ["provider_start_failed", "provider_protocol_error"]
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
    Ok(())
}
