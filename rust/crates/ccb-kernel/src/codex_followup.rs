//! Same-turn steering; a wire acknowledgement never completes its owning job.
use super::Session;
use anyhow::{ensure, Context, Result};
use serde_json::{json, Value};
impl Session {
    pub fn followup_binding(&self, job_id: &str) -> Result<Value> {
        ensure!(
            !self.closed
                && !self.quarantined
                && !self.forced
                && !self.stdout_closed
                && !self.input_closed,
            "provider unavailable"
        );
        let active = self.active.as_ref().context("no active provider turn")?;
        ensure!(active.job["job_id"] == job_id, "active job changed");
        ensure!(!active.turn.is_empty(), "active provider turn unbound");
        ensure!(
            active.context.is_none()
                && active.skills.is_none()
                && active.cancel.is_none()
                && active.decision.is_none()
                && active.completed.is_null()
                && active.check.is_empty(),
            "provider terminal or control pending"
        );
        Ok(json!({"job_id":job_id,"runtime_instance_id":self.runtime,
            "execution_instance_id":active.job["provider_options"]["native_execution_instance"],
            "thread_id":self.state["thread_id"],"turn_id":active.turn}))
    }
    pub fn send_followup(&mut self, id: &str, binding: &Value, message: &str) -> Result<()> {
        ensure!(
            self.followup_binding(binding["job_id"].as_str().context("missing job")?)? == *binding,
            "followup binding changed"
        );
        ensure!(!self.followups.contains_key(id), "followup already sent");
        let wire = json!({"id":format!("followup-{id}"),"method":"turn/steer","params":{
            "threadId":binding["thread_id"],"expectedTurnId":binding["turn_id"],
            "input":[{"type":"text","text":message}],"clientUserMessageId":id}});
        self.commands
            .try_send(wire)
            .context("followup input unavailable")?;
        self.followups.insert(
            id.to_owned(),
            json!({"turn_id":binding["turn_id"],"status":"sending"}),
        );
        Ok(())
    }
    pub fn followup_result(&mut self, id: &str) -> Option<Value> {
        if self
            .followups
            .get(id)
            .is_some_and(|r| r["status"] != "sending")
        {
            self.followups.remove(id)
        } else {
            None
        }
    }
    pub fn forget_followup(&mut self, id: &str) {
        self.followups.remove(id);
    }
    pub(super) fn ingest_followup(&mut self, record: &Value) -> bool {
        // Server-initiated requests do not acknowledge our client request IDs.
        if record["method"].is_string() {
            return false;
        }
        let Some(id) = record["id"]
            .as_str()
            .and_then(|s| s.strip_prefix("followup-"))
        else {
            return false;
        };
        if let Some(row) = self.followups.get_mut(id) {
            if row["status"] == "sending" {
                let (status, reason) = if !record["error"].is_null() {
                    ("rejected", "provider_steer_rejected")
                } else if record["result"]["turnId"] == row["turn_id"] {
                    ("injected", "provider_turn_steered")
                } else {
                    ("uncertain", "provider_steer_turn_mismatch")
                };
                row["status"] = json!(status);
                row["reason"] = json!(reason);
            }
        }
        true
    }
}
