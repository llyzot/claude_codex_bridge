//! Correlated Pi context commands; neither an ACK nor agent_end completes one.
use anyhow::{ensure, Context, Result};
use serde_json::{json, Value};

pub(crate) struct Control {
    action: String,
    id: String,
    check: String,
    previous: String,
    outcome: Option<&'static str>,
}
pub(crate) enum Step {
    Send(Value),
    Finished(Value, Value),
}
impl Control {
    pub fn new(action: &str, id: &str, previous: &str) -> Result<Self> {
        ensure!(
            ["clear", "compact"].contains(&action),
            "invalid Pi context action"
        );
        Ok(Self {
            action: action.into(),
            id: format!("context-{id}"),
            check: format!("context-state-{id}"),
            previous: previous.into(),
            outcome: None,
        })
    }
    fn command(&self) -> &str {
        if self.action == "clear" {
            "new_session"
        } else {
            "compact"
        }
    }
    pub fn request(&self) -> Value {
        json!({"id":self.id,"type":self.command()})
    }
    pub fn ingest(&mut self, record: &Value, cancelled: bool) -> Result<Option<Step>> {
        if record["type"] != "response" {
            return Ok(None);
        }
        if record["id"] == self.id && self.outcome.is_none() {
            ensure!(
                record["command"] == self.command(),
                "Pi context command mismatch"
            );
            let success = record["success"]
                .as_bool()
                .context("Pi context result missing")?;
            self.outcome = Some(if !success {
                if cancelled {
                    "cancelled"
                } else {
                    "failed"
                }
            } else if self.action == "clear" {
                if record["data"]["cancelled"]
                    .as_bool()
                    .context("Pi new_session result missing")?
                {
                    "vetoed"
                } else {
                    "completed"
                }
            } else {
                ensure!(
                    record["data"]["summary"].is_string()
                        && record["data"]["firstKeptEntryId"].is_string()
                        && record["data"]["tokensBefore"].is_number(),
                    "Pi compaction result missing"
                );
                "completed"
            });
            return Ok(Some(Step::Send(
                json!({"id":self.check,"type":"get_state"}),
            )));
        }
        if record["id"] != self.check || self.outcome.is_none() {
            return Ok(None);
        }
        ensure!(
            record["command"] == "get_state" && record["success"] == true,
            "Pi context state verification failed"
        );
        let state = &record["data"];
        let session = state["sessionId"]
            .as_str()
            .filter(|s| !s.is_empty())
            .context("Pi context session missing")?;
        ensure!(
            crate::pi_rpc::idle(state),
            "Pi context operation did not return idle"
        );
        let outcome = self.outcome.unwrap();
        let changed = self.action == "clear" && outcome == "completed";
        ensure!(
            (session != self.previous) == changed,
            "Pi context session identity mismatch"
        );
        let status = if outcome == "vetoed" {
            "cancelled"
        } else {
            outcome
        };
        let decision = json!({"schema_version":2,"record_type":"completion_decision","terminal":true,
            "status":status,"reason":format!("pi_context_{}_{}",self.action,outcome),"confidence":"exact",
            "reply":format!("Context {}: {}.",self.action,outcome),"anchor_seen":true,"reply_started":true,"reply_stable":true,
            "provider_turn_ref":null,"source_cursor":null,"finished_at":crate::daemon::now(),
            "diagnostics":{"completion_authority":"pi_context_response_and_idle_state","context_action":self.action,
                "previous_session_id":self.previous,"session_id":session,"context_changed":changed}});
        Ok(Some(Step::Finished(state.clone(), decision)))
    }
}
