//! Explicit new-message admission with frozen original recipients and receipts.
use super::Daemon;
use anyhow::{ensure, Context, Result};
use serde_json::{json, Value};
use std::collections::BTreeMap;
const OPERATIONS: &str = "resubmissions.jsonl";
fn text(row: &Value, key: &str) -> String {
    row[key].as_str().unwrap_or("").to_owned()
}
fn order(row: &Value) -> (u64, String, String) {
    (
        row["retry_index"].as_u64().unwrap_or(0),
        text(row, "updated_at"),
        text(row, "attempt_id"),
    )
}
impl Daemon {
    pub(super) fn resubmit(&mut self, p: &Value) -> Result<Value> {
        ensure!(
            p.as_object().is_some_and(|v| v
                .keys()
                .all(|k| ["message_id", "operation_id"].contains(&k.as_str()))),
            "unsupported resubmit fields"
        );
        let message_id = crate::records::string(p, "message_id")?.trim();
        ensure!(
            message_id.starts_with("msg_")
                && message_id.len() <= 256
                && message_id
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'_'),
            "resubmit requires a message ID"
        );
        let operation = if p.get("operation_id").is_some() {
            uuid::Uuid::parse_str(crate::records::string(p, "operation_id")?)?.to_string()
        } else {
            uuid::Uuid::new_v4().to_string()
        };
        let prior = self
            .kernel
            .store
            .latest_by_id(OPERATIONS, "operation_id", &operation)?;
        if !prior.is_null() {
            ensure!(
                prior["schema_version"] == 1
                    && prior["record_type"] == "native_resubmission"
                    && prior["project_id"] == self.config["project_id"]
                    && prior["message_id"] == message_id
                    && prior["receipt"].is_object()
                    && prior["receipt"]["original_message_id"] == message_id
                    && prior["receipt"]["operation_id"] == operation,
                "resubmit operation identity belongs to another request"
            );
            return Ok(prior["receipt"].clone());
        }
        let message =
            self.kernel
                .store
                .latest_by_id("messages/messages.jsonl", "message_id", message_id)?;
        ensure!(!message.is_null(), "resubmit message not found");
        let targets: Vec<String> = serde_json::from_value(message["target_agents"].clone())?;
        ensure!(!targets.is_empty(), "resubmit message has no targets");
        let attempts = self
            .kernel
            .store
            .latest_rows("attempts/attempts.jsonl", "attempt_id")?
            .into_iter()
            .filter(|a| a["message_id"] == message_id)
            .collect::<Vec<_>>();
        let mut latest: BTreeMap<String, &Value> = BTreeMap::new();
        for a in &attempts {
            let name = text(a, "agent_name");
            if latest.get(&name).is_none_or(|old| order(a) > order(old)) {
                latest.insert(name, a);
            }
        }
        ensure!(!latest.is_empty(), "resubmit message has no attempts");
        ensure!(
            latest.values().all(|a| [
                "completed",
                "incomplete",
                "failed",
                "cancelled",
                "superseded",
                "dead_letter"
            ]
            .contains(&a["attempt_state"].as_str().unwrap_or(""))),
            "message still has active attempts"
        );
        for edge in self
            .kernel
            .store
            .latest_rows("callbacks/edges.jsonl", "edge_id")?
        {
            if [
                "parent_message_id",
                "child_message_id",
                "continuation_message_id",
            ]
            .iter()
            .any(|k| edge[*k] == message_id)
            {
                ensure!(
                    ["done", "failed", "timed_out"].contains(&edge["state"].as_str().unwrap_or("")),
                    "message has unresolved callback lineage"
                );
            }
        }
        let mut sources = Vec::new();
        for target in &targets {
            self.reload_guard(target)?;
            self.spec(target)?;
            let a = latest
                .get(target)
                .context("message is missing target attempt lineage")?;
            let mut job = self.trace_job_record(&text(a, "job_id"))?;
            ensure!(!job.is_null(), "resubmit source job is missing");
            ensure!(
                job["agent_name"] == *target,
                "resubmit source job belongs to another agent"
            );
            ensure!(
                !job["provider_options"]["native_context_control"].is_object(),
                "context controls cannot be resubmitted; issue a new control explicitly"
            );
            // A completed chain parent has review continuations under the same
            // message. Re-submit its original user request, not generated review text.
            if job["request"]["message_type"] == "chain_continuation" {
                let mut original = attempts
                    .iter()
                    .filter(|a| a["agent_name"] == *target)
                    .collect::<Vec<_>>();
                original.sort_by_key(|a| order(a));
                for attempt in original {
                    let candidate = self.trace_job_record(&text(attempt, "job_id"))?;
                    if candidate["request"]["message_type"] == "ask"
                        && candidate["request"]["from_actor"] == message["from_actor"]
                    {
                        job = candidate;
                        break;
                    }
                }
            }
            ensure!(
                job["request"]["message_type"] == "ask",
                "internal delivery and control messages cannot be resubmitted"
            );
            ensure!(!crate::callback_policy::is_chain(&job["request"]),"chain child resubmission cannot bind to a different active parent; resubmit the completed root request");
            sources.push(job);
        }
        let mut request = sources[0]["request"].clone();
        request["from_actor"] = message["from_actor"].clone();
        request["delivery_scope"] = json!(if targets.len() > 1 {
            "broadcast"
        } else {
            "single"
        });
        request["to_agent"] = if targets.len() > 1 {
            json!("all")
        } else {
            json!(targets[0])
        };
        let admitted = self.admit_request(&request, Some(targets), json!(message_id))?;
        let receipt = &admitted["receipt"];
        let jobs = receipt["jobs"]
            .as_array()
            .cloned()
            .unwrap_or_else(|| vec![receipt.clone()]);
        let result = json!({"accepted_at":receipt["accepted_at"],"original_message_id":message_id,"message_id":admitted["message_id"],"submission_id":receipt["submission_id"],"jobs":jobs,"operation_id":operation});
        self.kernel.store.append(OPERATIONS,&json!({"schema_version":1,"record_type":"native_resubmission","operation_id":operation,"project_id":self.config["project_id"],"message_id":message_id,"receipt":result}))?;
        Ok(result)
    }
}
