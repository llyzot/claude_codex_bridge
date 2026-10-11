//! Read-only inbox projection and FIFO acknowledgements with durable receipts.
use super::{now, payload, trace, Daemon};
use anyhow::{ensure, Context, Result};
use serde_json::{json, Value};

const ACKS: &str = "mailbox-acks.jsonl";
fn text<'a>(row: &'a Value, key: &str) -> &'a str {
    row[key].as_str().unwrap_or("")
}
fn terminal(state: &Value) -> bool {
    [
        "completed",
        "incomplete",
        "failed",
        "cancelled",
        "superseded",
        "dead_letter",
    ]
    .contains(&state.as_str().unwrap_or(""))
}
fn pending(event: &Value) -> bool {
    !["consumed", "superseded", "abandoned"].contains(&text(event, "status"))
}
impl Daemon {
    fn inbox_item(&self, event: &Value, position: usize) -> Result<Value> {
        let attempt = self.kernel.store.latest_by_id(
            "attempts/attempts.jsonl",
            "attempt_id",
            text(event, "attempt_id"),
        )?;
        let message = self.kernel.store.latest_by_id(
            "messages/messages.jsonl",
            "message_id",
            text(event, "message_id"),
        )?;
        let job = self.trace_job_record(text(&attempt, "job_id"))?;
        let mut item = event.clone();
        item["position"] = json!(position);
        item["attempt_state"] = attempt["attempt_state"].clone();
        item["message_state"] = message["message_state"].clone();
        item["job_id"] = attempt["job_id"].clone();
        item["source_actor"] = message["from_actor"].clone();
        item["lineage_missing"] = json!(attempt.is_null() || message.is_null() || job.is_null());
        if event["event_type"] == "task_reply" {
            let id = payload(&event["payload_ref"], "reply:").unwrap_or_default();
            let reply = trace::reply(&self.kernel.store.latest_by_id(
                "replies/replies.jsonl",
                "reply_id",
                &id,
            )?);
            item["reply_id"] = json!(id);
            if !reply.is_null() {
                item["source_actor"] = reply["agent_name"].clone();
            }
            item["lineage_missing"] = json!(item["lineage_missing"] == true || reply.is_null());
            for key in [
                "terminal_status",
                "finished_at",
                "notice",
                "notice_kind",
                "last_progress_at",
                "heartbeat_silence_seconds",
            ] {
                item[format!("reply_{key}")] = reply[key].clone();
            }
            item["reply_preview"] = reply["reply_preview"].clone();
            if position == 1 {
                for key in ["reply", "reply_artifact", "reply_truncated", "reply_size"] {
                    item[key] = reply[key].clone();
                }
            }
        }
        Ok(item)
    }
    pub(super) fn inbox_view(&mut self, p: &Value) -> Result<Value> {
        let agent = crate::records::string(p, "agent_name")?;
        ensure!(
            p["detail"].is_null() || p["detail"].is_boolean(),
            "detail must be boolean"
        );
        self.mailbox_observation(agent, p["detail"] == true)
    }
    pub(super) fn read_mailbox_summary(&mut self, agent: &str) -> Result<Value> {
        self.kernel
            .dispatch("mailbox.load", &json!({"agent_name":agent}))
            .and_then(|v| {
                if !v.is_null() {
                    crate::validate_mailbox(&v)?;
                    ensure!(v["agent_name"] == agent, "mailbox owner mismatch");
                }
                Ok(v)
            })
    }
    pub(super) fn mailbox_observation(&mut self, agent: &str, detail: bool) -> Result<Value> {
        self.spec(agent)?;
        let args = json!({"agent_name":agent});
        let events = self.kernel.dispatch("latest_events", &args)?;
        let rows = events.as_array().context("invalid inbox events")?;
        let pending_rows = rows.iter().filter(|e| pending(e)).collect::<Vec<_>>();
        let cached = self.read_mailbox_summary(agent);
        let (summary, error) = match cached {
            Ok(v) => (v, Value::Null),
            Err(e) => (Value::Null, json!(e.to_string())),
        };
        let active = pending_rows
            .iter()
            .find(|e| e["status"] == "delivering")
            .map(|e| e["inbound_event_id"].clone())
            .unwrap_or(Value::Null);
        let replies = pending_rows
            .iter()
            .filter(|e| e["event_type"] == "task_reply")
            .count();
        let lease = self.kernel.dispatch("lease.load", &args)?;
        let state = if !active.is_null() || lease["lease_state"] == "acquired" {
            "delivering"
        } else if pending_rows.is_empty() {
            "idle"
        } else {
            "blocked"
        };
        let status = if !error.is_null() {
            "error"
        } else if summary.is_null() {
            "missing"
        } else if summary["queue_depth"] != pending_rows.len()
            || summary["pending_reply_count"] != replies
            || summary["active_inbound_event_id"] != active
            || summary["mailbox_state"] != state
        {
            "stale"
        } else {
            "ok"
        };
        let mailbox = json!({"agent_name":agent,"mailbox_id":format!("mbx_{agent}"),
            "mailbox_state":state,
            "lease_version":summary["lease_version"].as_u64().unwrap_or(0),"queue_depth":pending_rows.len(),"pending_reply_count":replies,
            "active_inbound_event_id":active,"last_inbound_started_at":summary["last_inbound_started_at"],
            "last_inbound_finished_at":summary["last_inbound_finished_at"],"summary_status":status,"summary_error":error});
        let head = pending_rows
            .first()
            .map(|e| self.inbox_item(e, 1))
            .transpose()?
            .unwrap_or(Value::Null);
        let items = if detail {
            pending_rows
                .iter()
                .enumerate()
                .map(|(i, e)| self.inbox_item(e, i + 1))
                .collect::<Result<Vec<_>>>()?
        } else {
            vec![]
        };
        Ok(
            json!({"target":agent,"agent_name":agent,"summary_status":status,"summary_error":error,
            "agent":mailbox,"item_count":pending_rows.len(),"head":head,"items":items,"events":events}),
        )
    }
    fn ack_response(&mut self, receipt: &Value, repeated: bool) -> Result<Value> {
        let view = self.inbox_view(&json!({"agent_name":receipt["agent_name"]}))?;
        let mut out = receipt.clone();
        out["already_acknowledged"] = json!(repeated);
        out["mailbox"] = view["agent"].clone();
        out["next_inbound_event_id"] = view["head"]["inbound_event_id"].clone();
        out["next_event_type"] = view["head"]["event_type"].clone();
        if let Some(id) = receipt["reply_id"].as_str() {
            let reply = trace::reply(&self.kernel.store.latest_by_id(
                "replies/replies.jsonl",
                "reply_id",
                id,
            )?);
            for key in ["reply", "reply_artifact", "reply_truncated", "reply_size"] {
                out[key] = reply[key].clone();
            }
            out["reply_from_agent"] = reply["agent_name"].clone();
            for key in [
                "terminal_status",
                "finished_at",
                "notice",
                "notice_kind",
                "last_progress_at",
                "heartbeat_silence_seconds",
            ] {
                out[format!("reply_{key}")] = reply[key].clone();
            }
        }
        Ok(out)
    }
    pub(super) fn ack_inbox(&mut self, p: &Value) -> Result<Value> {
        let agent = crate::records::string(p, "agent_name")?;
        self.spec(agent)?;
        self.reload_guard(agent)?;
        ensure!(
            p.get("inbound_event_id").is_none_or(Value::is_string),
            "inbound_event_id must be a string"
        );
        let args = json!({"agent_name":agent});
        let head = self.kernel.dispatch("head_pending_event", &args)?;
        let id = p["inbound_event_id"]
            .as_str()
            .unwrap_or(text(&head, "inbound_event_id"));
        ensure!(!id.is_empty(), "inbox is empty or event ID is empty");
        let ack_id = format!("{agent}:{id}");
        let prior = self.kernel.store.latest_by_id(ACKS, "ack_id", &ack_id)?;
        if !prior.is_null() {
            ensure!(
                prior["schema_version"] == 1
                    && prior["record_type"] == "native_mailbox_ack"
                    && prior["project_id"] == self.config["project_id"]
                    && prior["agent_name"] == agent
                    && prior["acknowledged_inbound_event_id"] == id,
                "ack receipt identity mismatch"
            );
            return self.ack_response(&prior, true);
        }
        ensure!(
            head["inbound_event_id"] == id,
            "ack requires the FIFO head event"
        );
        ensure!(
            payload(&head["payload_ref"], "delivery:").is_none(),
            "automatic reply delivery has already been scheduled"
        );
        ensure!(
            self.active_provider_view(agent)?.is_null(),
            "ack cannot release an active task execution slot"
        );
        let lease = self.kernel.dispatch("lease.load", &args)?;
        ensure!(
            lease["lease_state"] != "acquired" || lease["inbound_event_id"] == id,
            "another inbound event holds the mailbox lease"
        );
        let attempt = self.kernel.store.latest_by_id(
            "attempts/attempts.jsonl",
            "attempt_id",
            text(&head, "attempt_id"),
        )?;
        let message = self.kernel.store.latest_by_id(
            "messages/messages.jsonl",
            "message_id",
            text(&head, "message_id"),
        )?;
        let job = self.trace_job_record(text(&attempt, "job_id"))?;
        ensure!(
            !attempt.is_null()
                && !job.is_null()
                && !message.is_null()
                && attempt["message_id"] == head["message_id"]
                && job["job_id"] == attempt["job_id"]
                && job["agent_name"] == attempt["agent_name"],
            "ack requires valid source lineage"
        );
        let mut receipt = json!({"schema_version":1,"record_type":"native_mailbox_ack","ack_id":ack_id,
            "project_id":self.config["project_id"],"target":agent,"agent_name":agent,"acknowledged_inbound_event_id":id,
            "acknowledged_event_type":head["event_type"],"message_id":head["message_id"],"attempt_id":head["attempt_id"],
            "job_id":attempt["job_id"],"attempt_state":attempt["attempt_state"],"acknowledged_at":now()});
        let op = if head["event_type"] == "task_reply" {
            let reply_id =
                payload(&head["payload_ref"], "reply:").context("reply identity is missing")?;
            let reply =
                self.kernel
                    .store
                    .latest_by_id("replies/replies.jsonl", "reply_id", &reply_id)?;
            ensure!(
                !reply.is_null()
                    && reply["attempt_id"] == head["attempt_id"]
                    && reply["message_id"] == head["message_id"]
                    && reply["agent_name"] == attempt["agent_name"],
                "reply source lineage mismatch"
            );
            receipt["reply_id"] = json!(reply_id);
            "ack_reply"
        } else {
            ensure!(
                head["event_type"] == "task_request",
                "ack only supports task_reply or terminal task_request"
            );
            ensure!(
                attempt["agent_name"] == agent
                    && terminal(&attempt["attempt_state"])
                    && terminal(&job["status"]),
                "ack requires a proven terminal task_request"
            );
            if attempt["attempt_state"] == "cancelled"
                && ["created", "queued"].contains(&text(&head, "status"))
            {
                "abandon"
            } else {
                "consume"
            }
        };
        let consumed = self.kernel.dispatch(op,&json!({"agent_name":agent,"inbound_event_id":id,"timestamp":receipt["acknowledged_at"]}))?;
        ensure!(!consumed.is_null(), "failed to acknowledge FIFO event");
        receipt["event_status"] = consumed["status"].clone();
        self.kernel.store.append(ACKS, &receipt)?;
        self.ack_response(&receipt, false)
    }
}
