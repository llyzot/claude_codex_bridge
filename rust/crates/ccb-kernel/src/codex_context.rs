//! Manual context controls on an exclusively owned app-server thread.
use anyhow::{ensure, Context, Result};
use serde_json::{json, Value};
use std::collections::BTreeSet;

pub(crate) enum Step {
    Send(Value),
    Finished(Value, Value),
}
pub(crate) struct Control {
    action: String,
    before: Value,
    thread: String,
    baseline: String,
    request: String,
    check: String,
    options: Value,
    phase: &'static str,
    prior_turns: BTreeSet<String>,
    pub turn: String,
    started_items: BTreeSet<String>,
    completed_items: BTreeSet<String>,
    terminal: Value,
    accepted: bool,
    rejected: bool,
    host_name: Option<String>,
    publish: String,
}
impl Control {
    pub fn new(
        action: &str,
        id: &str,
        state: &Value,
        spec: &Value,
        host_name: Option<&str>,
    ) -> Result<Self> {
        ensure!(
            ["clear", "compact"].contains(&action),
            "invalid Codex context action"
        );
        let mut options = spec["thread_options"]
            .as_object()
            .cloned()
            .unwrap_or_default();
        if let Some(cwd) = spec["workspace_path"].as_str() {
            options.insert("cwd".into(), json!(cwd));
        }
        options.remove("threadId");
        options.remove("excludeTurns");
        Ok(Self {
            action: action.into(),
            before: state.clone(),
            thread: state["thread_id"]
                .as_str()
                .context("missing Codex context thread")?
                .into(),
            baseline: format!("context-before-{id}"),
            request: format!("context-{id}"),
            check: format!("context-after-{id}"),
            options: json!(options),
            phase: "baseline",
            prior_turns: BTreeSet::new(),
            turn: String::new(),
            started_items: BTreeSet::new(),
            completed_items: BTreeSet::new(),
            terminal: Value::Null,
            accepted: false,
            rejected: false,
            host_name: host_name
                .filter(|_| spec["thread_options"]["ephemeral"] != true)
                .map(str::to_owned),
            publish: format!("context-publish-{id}"),
        })
    }
    fn read(&self, id: &str) -> Value {
        json!({"id":id,"method":"thread/read","params":{"threadId":self.thread,"includeTurns":self.action=="compact"}})
    }
    pub fn request(&self) -> Value {
        self.read(&self.baseline)
    }
    fn finish(&self, state: Value, status: &str, reason: &str, sent: bool) -> Step {
        let decision = json!({"schema_version":2,"record_type":"completion_decision","terminal":true,"status":status,
            "reason":reason,"confidence":"exact","reply":format!("Context {}: {}.",self.action,status),
            "anchor_seen":true,"reply_started":true,"reply_stable":true,"provider_turn_ref":self.turn,
            "source_cursor":null,"finished_at":crate::daemon::now(),"diagnostics":{
                "completion_authority":"codex_context_correlated_state","context_action":self.action,"provider_control_sent":sent,
                "previous_thread_id":self.before["thread_id"],"thread_id":state["thread_id"],"turn_id":self.turn}});
        Step::Finished(state, decision)
    }
    fn observed(&self, thread: &Value) -> Value {
        json!({"thread_id":thread["id"],"path":thread["path"],"status":thread["status"]})
    }
    pub fn ingest(&mut self, record: &Value, cancelled: bool) -> Result<Option<Step>> {
        if record["id"] == self.publish && self.phase == "publish" {
            ensure!(
                record["error"].is_null() && record["result"].is_object(),
                "Codex cleared thread publication failed"
            );
            self.phase = "verify";
            return Ok(Some(Step::Send(self.read(&self.check))));
        }
        if record["id"] == self.baseline && self.phase == "baseline" {
            if !record["error"].is_null() {
                return Ok(Some(self.finish(
                    self.before.clone(),
                    "failed",
                    "codex_context_preflight_rejected",
                    false,
                )));
            }
            let thread = &record["result"]["thread"];
            ensure!(
                thread["id"] == self.thread && thread["status"]["type"] == "idle",
                "Codex context baseline is not the owned idle thread"
            );
            if cancelled {
                return Ok(Some(self.finish(
                    self.observed(thread),
                    "cancelled",
                    "codex_context_cancelled_before_input",
                    false,
                )));
            }
            if self.action == "compact" {
                for turn in thread["turns"]
                    .as_array()
                    .context("Codex context baseline turns missing")?
                {
                    self.prior_turns.insert(
                        turn["id"]
                            .as_str()
                            .context("Codex context baseline turn identity missing")?
                            .into(),
                    );
                }
            }
            self.phase = "action";
            return Ok(Some(Step::Send(if self.action == "clear" {
                json!({"id":self.request,"method":"thread/start","params":self.options})
            } else {
                json!({"id":self.request,"method":"thread/compact/start","params":{"threadId":self.thread}})
            })));
        }
        if self.phase == "baseline" {
            return Ok(None);
        }
        if record["id"] == self.request && self.phase == "action" && !self.accepted {
            self.accepted = true;
            if !record["error"].is_null() {
                ensure!(
                    self.turn.is_empty(),
                    "Codex rejected context input after emitting a new turn"
                );
                self.rejected = true;
                self.phase = "verify";
                return Ok(Some(Step::Send(self.read(&self.check))));
            }
            ensure!(
                record["result"].is_object(),
                "Codex context acknowledgement missing"
            );
            if self.action == "clear" {
                let thread = &record["result"]["thread"];
                let id = thread["id"]
                    .as_str()
                    .filter(|s| !s.is_empty())
                    .context("Codex new context identity missing")?;
                ensure!(
                    id != self.thread && thread["status"]["type"] == "idle",
                    "Codex did not create a fresh idle thread"
                );
                self.thread = id.into();
                if let Some(name) = &self.host_name {
                    self.phase = "publish";
                    return Ok(Some(Step::Send(
                        json!({"id":self.publish,"method":"thread/name/set","params":{"threadId":self.thread,"name":name}}),
                    )));
                }
                self.phase = "verify";
                return Ok(Some(Step::Send(self.read(&self.check))));
            }
        }
        if record["id"] == self.check && self.phase == "verify" {
            ensure!(
                record["error"].is_null(),
                "Codex context verification rejected"
            );
            let thread = &record["result"]["thread"];
            ensure!(
                thread["id"] == self.thread,
                "Codex context verification identity mismatch"
            );
            if thread["status"]["type"] != "idle" {
                return Ok(None);
            }
            if self.rejected {
                return Ok(Some(self.finish(
                    self.observed(thread),
                    "failed",
                    "codex_context_rejected",
                    true,
                )));
            }
            if self.action == "clear" {
                if self.host_name.is_some() {
                    ensure!(
                        thread["path"]
                            .as_str()
                            .is_some_and(|p| std::path::Path::new(p).is_file()),
                        "Codex cleared thread history was not persisted"
                    );
                }
                return Ok(Some(self.finish(
                    self.observed(thread),
                    "completed",
                    "codex_context_cleared",
                    true,
                )));
            }
            let turn = thread["turns"]
                .as_array()
                .and_then(|turns| turns.iter().find(|t| t["id"] == self.turn))
                .context("Codex compaction turn absent from verified history")?;
            let status = turn["status"].as_str().unwrap_or("");
            if !["completed", "failed", "interrupted"].contains(&status) {
                return Ok(None);
            }
            ensure!(
                turn["status"] == self.terminal["status"],
                "Codex compaction terminal status mismatch"
            );
            if status == "completed" {
                ensure!(
                    !self.completed_items.is_empty()
                        && turn["items"]
                            .as_array()
                            .is_some_and(|items| items.iter().any(|item| item["type"]
                                == "contextCompaction"
                                && item["id"]
                                    .as_str()
                                    .is_some_and(|id| self.completed_items.contains(id)))),
                    "Codex compaction completion item missing"
                );
            }
            return Ok(Some(self.finish(
                self.observed(thread),
                if status == "interrupted" {
                    "cancelled"
                } else {
                    status
                },
                "codex_context_compaction_terminal",
                true,
            )));
        }
        if self.phase == "action" && self.accepted && !self.terminal.is_null() {
            self.phase = "verify";
            return Ok(Some(Step::Send(self.read(&self.check))));
        }
        let params = &record["params"];
        if params["threadId"] != self.thread {
            return Ok(None);
        }
        if self.phase == "verify"
            && record["method"] == "thread/status/changed"
            && params["status"]["type"] == "idle"
        {
            return Ok(Some(Step::Send(self.read(&self.check))));
        }
        if self.action != "compact" || self.phase != "action" {
            return Ok(None);
        }
        if record["method"] == "turn/started" {
            let id = params["turn"]["id"]
                .as_str()
                .context("Codex compaction start identity missing")?;
            if self.prior_turns.contains(id) {
                return Ok(None);
            }
            ensure!(
                self.turn.is_empty() || self.turn == id,
                "multiple new turns during Codex compaction"
            );
            self.turn = id.into();
        }
        if !self.turn.is_empty()
            && params["turnId"] == self.turn
            && params["item"]["type"] == "contextCompaction"
        {
            let id = params["item"]["id"]
                .as_str()
                .context("Codex compaction item identity missing")?;
            if record["method"] == "item/started" {
                self.started_items.insert(id.into());
            }
            if record["method"] == "item/completed" && self.started_items.contains(id) {
                self.completed_items.insert(id.into());
            }
        }
        if !self.turn.is_empty()
            && record["method"] == "turn/completed"
            && params["turn"]["id"] == self.turn
        {
            self.terminal = params["turn"].clone();
        }
        if self.accepted && !self.terminal.is_null() {
            self.phase = "verify";
            return Ok(Some(Step::Send(self.read(&self.check))));
        }
        Ok(None)
    }
}
