//! Durable same-execution outbox. Ambiguous sends are observed, never replayed.
use super::{now, Daemon};
use anyhow::{ensure, Context, Result};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;
const LOG: &str = "native-followups.jsonl";
fn public(mut row: Value) -> Value {
    if let Some(o) = row.as_object_mut() {
        o.remove("message");
        o.remove("message_sha256");
    }
    row
}
impl Daemon {
    pub(super) fn followups_for_jobs(&self, jobs: &[Value]) -> Result<Vec<Value>> {
        let ids = jobs
            .iter()
            .filter_map(|j| j["job_id"].as_str())
            .collect::<BTreeSet<_>>();
        let mut rows = self.kernel.store.latest_rows(LOG, "followup_id")?;
        rows.retain(|r| {
            r["project_id"] == self.config["project_id"]
                && r["job_id"].as_str().is_some_and(|id| ids.contains(id))
        });
        Ok(rows.into_iter().map(public).collect())
    }
    pub(super) fn followup_status(&self, p: &Value) -> Result<Value> {
        let id = crate::records::string(p, "followup_id")?;
        let row = self.kernel.store.latest_by_id(LOG, "followup_id", id)?;
        ensure!(
            !row.is_null() && row["project_id"] == self.config["project_id"],
            "followup not found in this project"
        );
        Ok(public(row))
    }
    pub(super) fn admit_followup(&mut self, p: &Value) -> Result<Value> {
        let job_id = crate::records::string(p, "job_id")?.trim();
        let message = crate::records::string(p, "message")?.trim();
        ensure!(
            !message.is_empty() && message.len() <= 65536,
            "followup message must contain 1..65536 UTF-8 bytes"
        );
        let operation = if p.get("operation_id").is_some() {
            uuid::Uuid::parse_str(crate::records::string(p, "operation_id")?)?
        } else {
            uuid::Uuid::new_v4()
        };
        let id = format!("fup_{}", operation.simple());
        let digest = format!("{:x}", Sha256::digest(message.as_bytes()));
        let prior = self.kernel.store.latest_by_id(LOG, "followup_id", &id)?;
        if !prior.is_null() {
            ensure!(
                prior["project_id"] == self.config["project_id"]
                    && prior["job_id"] == job_id
                    && prior["message_sha256"] == digest,
                "followup operation belongs to another request"
            );
            return Ok(public(prior));
        }
        let job = self.trace_job_record(job_id)?;
        ensure!(!job.is_null(), "unknown followup job");
        let agent = crate::records::string(&job, "agent_name")?;
        self.reload_guard(agent)?;
        let mut row = json!({"schema_version":1,"record_type":"native_active_followup","followup_id":id,
            "operation_id":operation.to_string(),"project_id":self.config["project_id"],"job_id":job_id,
            "agent_name":agent,"provider":job["provider"],"status":"rejected","reason":"job_not_running",
            "mechanism":"codex_turn_steer","created_at":now(),"updated_at":now(),"message":"","message_sha256":digest});
        if job["status"] == "running" {
            let active = self.active_provider_view(agent)?;
            if active["job_id"] != job_id {
                row["reason"] = json!("job_not_active_for_target");
            } else if let Some(session) = self.codex.get(agent) {
                match session.followup_binding(job_id) {
                    Ok(binding) => {
                        ensure!(
                            binding["execution_instance_id"] == active["execution_instance_id"],
                            "followup execution identity mismatch"
                        );
                        row["binding"] = binding;
                        row["expected_provider_turn_ref"] = row["binding"]["turn_id"].clone();
                        row["provider_turn_ref"] = row["expected_provider_turn_ref"].clone();
                        row["status"] = json!("accepted");
                        row["reason"] = json!("durable_outbox_accepted");
                        row["message"] = json!(message);
                    }
                    Err(e) => row["reason"] = json!(e.to_string()),
                }
            } else {
                row["reason"] = json!("provider_active_followup_unsupported");
            }
        } else if ["completed", "failed", "incomplete", "cancelled"]
            .contains(&job["status"].as_str().unwrap_or(""))
        {
            row["status"] = json!("too_late");
            row["reason"] = json!("job_already_terminal");
        }
        let rows = self.kernel.store.latest_rows(LOG, "followup_id")?;
        row["sequence"] = json!(
            rows.iter()
                .filter(|r| r["job_id"] == job_id)
                .map(|r| r["sequence"].as_u64().unwrap_or(0))
                .max()
                .unwrap_or(0)
                + 1
        );
        self.kernel.store.append(LOG, &row)?;
        Ok(public(row))
    }
    pub(super) fn tick_followups(&mut self) -> Result<()> {
        let mut rows = self.kernel.store.latest_rows(LOG, "followup_id")?;
        rows.retain(|r| ["accepted", "sending"].contains(&r["status"].as_str().unwrap_or("")));
        // latest_rows preserves first admission order even when timestamps move
        // backwards or different jobs have equal per-job sequence numbers.
        let mut busy = BTreeSet::new();
        for mut row in rows {
            ensure!(
                row["project_id"] == self.config["project_id"],
                "foreign followup outbox"
            );
            let agent = crate::records::string(&row, "agent_name")?.to_owned();
            if !busy.insert(agent.clone()) {
                continue;
            }
            let id = crate::records::string(&row, "followup_id")?.to_owned();
            let job_id = crate::records::string(&row, "job_id")?.to_owned();
            let sending = row["status"] == "sending";
            let result = self
                .codex
                .get_mut(&agent)
                .and_then(|s| s.followup_result(&id));
            if let Some(result) = result {
                row["status"] = result["status"].clone();
                row["reason"] = result["reason"].clone();
            } else {
                let job = self.trace_job_record(&job_id)?;
                let active = self.active_provider_view(&agent)?;
                let valid = job["status"] == "running"
                    && active["job_id"] == job_id
                    && self
                        .codex
                        .get(&agent)
                        .and_then(|s| s.followup_binding(&job_id).ok())
                        .is_some_and(|b| b == row["binding"]);
                if !valid {
                    row["status"] = json!(if sending { "uncertain" } else { "terminal" });
                    row["reason"] = json!(if sending {
                        "provider_binding_lost_after_send"
                    } else {
                        "provider_binding_changed_before_send"
                    });
                } else if sending {
                    let started = chrono::DateTime::parse_from_rfc3339(
                        row["sent_at"]
                            .as_str()
                            .context("missing followup send time")?,
                    )?;
                    if chrono::DateTime::parse_from_rfc3339(&now())?
                        .signed_duration_since(started)
                        .num_seconds()
                        < 8
                    {
                        continue;
                    }
                    row["status"] = json!("uncertain");
                    row["reason"] = json!("provider_steer_response_not_observed");
                } else {
                    let message = row["message"]
                        .as_str()
                        .context("missing outbox message")?
                        .to_owned();
                    row["status"] = json!("sending");
                    row["reason"] = json!("provider_send_intent");
                    row["sent_at"] = json!(now());
                    row["updated_at"] = row["sent_at"].clone();
                    row["message"] = json!("");
                    self.kernel.store.append(LOG, &row)?;
                    // Commit intent before any external side effect; a crash in
                    // the gap is conservatively uncertain, never a resend.
                    self.kernel.store.commit()?;
                    if self
                        .codex
                        .get_mut(&agent)
                        .unwrap()
                        .send_followup(&id, &row["binding"], &message)
                        .is_ok()
                    {
                        continue;
                    }
                    row["status"] = json!("rejected");
                    row["reason"] = json!("provider_input_not_enqueued");
                }
            }
            if let Some(session) = self.codex.get_mut(&agent) {
                session.forget_followup(&id);
            }
            row["message"] = json!("");
            row["updated_at"] = json!(now());
            self.kernel.store.append(LOG, &row)?;
            self.kernel.store.commit()?;
        }
        Ok(())
    }
}
