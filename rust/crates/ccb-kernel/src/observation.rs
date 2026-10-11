//! Mailbox-aware observations preserve scheduler and incoming-reply identities.
use super::Daemon;
use anyhow::{ensure, Context, Result};
use serde_json::{json, Value};

impl Daemon {
    fn cached_queue_summary(&mut self, agent: &str) -> Value {
        let read = self.read_mailbox_summary(agent);
        let (row, status, error) = match read {
            Ok(v) if v.is_null() => (v, "missing", Value::Null),
            Ok(v) => (v, "ok", Value::Null),
            Err(e) => (Value::Null, "error", json!(e.to_string())),
        };
        json!({"agent_name":agent,"mailbox_id":row["mailbox_id"].as_str().map(str::to_owned).unwrap_or_else(||format!("mbx_{agent}")),
            "mailbox_state":row["mailbox_state"],"lease_version":row["lease_version"].as_u64().unwrap_or(0),
            "queue_depth":row["queue_depth"].as_u64().unwrap_or(0),"pending_reply_count":row["pending_reply_count"].as_u64().unwrap_or(0),
            "active_inbound_event_id":row["active_inbound_event_id"],"last_inbound_started_at":row["last_inbound_started_at"],
            "last_inbound_finished_at":row["last_inbound_finished_at"],"summary_status":status,"summary_error":error})
    }
    pub(super) fn queue_view(&mut self, p: &Value) -> Result<Value> {
        let target = p["target"].as_str().unwrap_or("all").trim().to_lowercase();
        ensure!(
            p["detail"].is_null() || p["detail"].is_boolean(),
            "detail must be boolean"
        );
        let names = self
            .names()
            .into_iter()
            .filter(|a| target == "all" || a == &target)
            .collect::<Vec<_>>();
        ensure!(!names.is_empty(), "unknown queue target");
        let mut queues = Vec::new();
        let mut agents = Vec::new();
        for agent in names {
            let slot = json!({"state_id":self.state,"target_kind":"agent","target_name":agent});
            queues.push(
                json!({"agent_name":agent,"target_kind":"agent","target_name":agent,
                "active_job_id":self.kernel.dispatch("scheduler.active_job_for",&slot)?,
                "queued_job_ids":self.kernel.dispatch("scheduler.queued_items_for",&slot)?}),
            );
            let detail = target != "all" && p["detail"] == true;
            let mut summary = self.cached_queue_summary(&agent);
            if detail {
                let view = self.mailbox_observation(&agent, true)?;
                summary = view["agent"].clone();
                summary["active"] = if summary["active_inbound_event_id"].is_null() {
                    view["head"].clone()
                } else {
                    view["items"]
                        .as_array()
                        .and_then(|rows| {
                            rows.iter().find(|r| {
                                r["inbound_event_id"] == summary["active_inbound_event_id"]
                            })
                        })
                        .cloned()
                        .unwrap_or(Value::Null)
                };
                summary["queued_events"] = view["items"].clone();
            }
            agents.push(summary);
        }
        let mut result = json!({"target":target,"queues":queues});
        if target == "all" {
            result["agent_count"] = json!(agents.len());
            result["queued_agent_count"] = json!(agents
                .iter()
                .filter(|a| a["queue_depth"].as_u64().unwrap_or(0) > 0)
                .count());
            result["active_agent_count"] = json!(agents
                .iter()
                .filter(|a| !a["active_inbound_event_id"].is_null())
                .count());
            result["total_queue_depth"] = json!(agents
                .iter()
                .map(|a| a["queue_depth"].as_u64().unwrap_or(0))
                .sum::<u64>());
            result["total_pending_reply_count"] = json!(agents
                .iter()
                .map(|a| a["pending_reply_count"].as_u64().unwrap_or(0))
                .sum::<u64>());
            result["agents"] = json!(agents);
        } else {
            result["agent"] = agents.remove(0);
        }
        Ok(result)
    }
    pub(super) fn pend_view(&mut self, p: &Value) -> Result<Value> {
        let target = crate::records::string(p, "target")?.trim();
        if target.starts_with("job_") {
            ensure!(
                !self.trace_job_record(target)?.is_null(),
                "unknown pend job"
            );
            return self.watch(&json!({"target":target}));
        }
        self.spec(target)?;
        let view = self.mailbox_observation(target, false)?;
        let head = &view["head"];
        let rows = self
            .kernel
            .store
            .rows(&format!("agents/{target}/jobs.jsonl"))?;
        let has_reply = head["event_type"] == "task_reply"
            && head["reply_id"].as_str().is_some_and(|s| !s.is_empty())
            && head["lineage_missing"] != true;
        let mut result = if let Some(last) = rows.last() {
            let id = last["job_id"]
                .as_str()
                .context("invalid latest job identity")?;
            ensure!(!self.trace_job_record(id)?.is_null(), "missing latest job");
            self.watch(&json!({"target":id}))?
        } else {
            json!({"job_id":if has_reply {head["job_id"].clone()}else{Value::Null},"agent_name":target,"target_kind":"agent","target_name":target,
                "provider_instance":null,"provider":null,"status":if has_reply {"mailbox_reply"}else{"unknown"},
                "job":null,"snapshot":null,"reply":"","completion_reason":null,"completion_confidence":null,
                "updated_at":if has_reply{head["reply_finished_at"].clone()}else{Value::Null},"terminal":false})
        };
        result["mailbox_summary_status"] = view["summary_status"].clone();
        result["mailbox_summary_error"] = view["summary_error"].clone();
        result["mailbox_reply_ready"] = json!(has_reply);
        result["mailbox_lineage_missing"] = head["lineage_missing"].clone();
        if has_reply {
            result["mailbox_reply_id"] = head["reply_id"].clone();
            result["mailbox_reply_from_agent"] = head["source_actor"].clone();
            result["mailbox_reply_job_id"] = head["job_id"].clone();
            result["mailbox_reply"] = head["reply"].clone();
            result["mailbox_reply_artifact"] = head["reply_artifact"].clone();
            result["mailbox_reply_truncated"] = head["reply_truncated"].clone();
            result["mailbox_inbound_event_id"] = head["inbound_event_id"].clone();
            result["mailbox_inbound_event_status"] = head["status"].clone();
            for key in [
                "terminal_status",
                "finished_at",
                "notice",
                "notice_kind",
                "last_progress_at",
                "heartbeat_silence_seconds",
            ] {
                result[format!("mailbox_reply_{key}")] = head[format!("reply_{key}")].clone();
            }
        }
        Ok(result)
    }
}
