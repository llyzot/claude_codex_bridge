//! Native Claude built-ins are verified by UUID lifecycle and post-command state.
use anyhow::{ensure, Context, Result};
use serde_json::{json, Value};
use std::collections::BTreeSet;

pub(crate) enum Step {
    Send(Value),
    Finished(Value, Value),
}
pub(crate) struct Control {
    action: String,
    user: String,
    before: Value,
    preflight: String,
    verify: String,
    phase: &'static str,
    started: bool,
    terminal: Option<String>,
    terminal_session: Option<String>,
    init_sessions: BTreeSet<String>,
    replay_sessions: BTreeSet<String>,
    reset: Option<String>,
    boundary: Option<String>,
    result: Option<Value>,
}
impl Control {
    pub fn new(action: &str, user: &str, before: &Value) -> Result<Self> {
        ensure!(
            ["clear", "compact"].contains(&action),
            "invalid Claude context action"
        );
        Ok(Self {
            action: action.into(),
            user: user.into(),
            before: before.clone(),
            preflight: format!("context-before-{user}"),
            verify: format!("context-after-{user}"),
            phase: "preflight",
            started: false,
            terminal: None,
            terminal_session: None,
            init_sessions: BTreeSet::new(),
            replay_sessions: BTreeSet::new(),
            reset: None,
            boundary: None,
            result: None,
        })
    }
    fn initialize(id: &str) -> Value {
        json!({"type":"control_request","request_id":id,"request":{"subtype":"initialize"}})
    }
    pub fn request(&self) -> Value {
        Self::initialize(&self.preflight)
    }
    pub fn sent(&self) -> bool {
        self.phase != "preflight" && self.phase != "done"
    }
    pub fn has_effect_evidence(&self) -> bool {
        self.reset.is_some() || self.boundary.is_some() || self.result.is_some()
    }
    fn finish(&mut self, state: Value, status: &str, reason: &str, sent: bool) -> Step {
        self.phase = "done";
        let decision = json!({"schema_version":2,"record_type":"completion_decision","terminal":true,"status":status,
            "reason":reason,"confidence":"exact","reply":format!("Context {}: {}.",self.action,status),
            "anchor_seen":sent,"reply_started":true,"reply_stable":true,"provider_turn_ref":state["session_id"],
            "source_cursor":null,"finished_at":crate::daemon::now(),"diagnostics":{
                "completion_authority":"claude_builtin_lifecycle_and_idle","context_action":self.action,"provider_control_sent":sent,
                "previous_session_id":self.before["session_id"],"session_id":state["session_id"],"user_message_uuid":self.user,
                "reset_event_id":self.reset,"compaction_boundary_id":self.boundary,
                "context_changed":state["session_id"]!=self.before["session_id"],"compacted":self.action=="compact" && self.boundary.is_some()}});
        Step::Finished(state, decision)
    }
    pub fn ingest(&mut self, row: &Value, cancelled: bool) -> Result<Option<Step>> {
        let scoped = row["session_id"] == self.before["session_id"]
            || row["session_id"]
                .as_str()
                .is_some_and(|id| self.init_sessions.contains(id));
        if row["parent_tool_use_id"].is_null() && scoped {
            ensure!(
                row["type"] != "conversation_reset"
                    || self.action == "clear"
                        && row["user_message_uuid"] == self.user
                        && self.started
                        && self.phase != "preflight"
                        && self.reset.as_ref().is_none_or(|id| row["uuid"] == *id),
                "unowned Claude context reset during control"
            );
            ensure!(
                row["type"] != "command_lifecycle"
                    || row["state"] != "started"
                    || row["command_uuid"] == self.user,
                "concurrent Claude command during context control"
            );
        }
        if self.phase == "done" {
            return Ok(None);
        }
        if row["type"] == "control_response" {
            let response = &row["response"];
            let preflight = self.phase == "preflight" && response["request_id"] == self.preflight;
            let verify = self.phase == "verify" && response["request_id"] == self.verify;
            if !preflight && !verify {
                return Ok(None);
            }
            if preflight && response["subtype"] != "success" {
                return Ok(Some(self.finish(
                    self.before.clone(),
                    "failed",
                    "claude_context_preflight_rejected",
                    false,
                )));
            }
            ensure!(
                response["subtype"] == "success",
                "Claude context state verification rejected"
            );
            let data = &response["response"];
            let supported = version_supported(&data["claude_code_version"])
                && response["pending_permission_requests"].is_array()
                && response["pending_user_dialog_requests"].is_array()
                && data["session_state"].is_string();
            if preflight && !supported {
                return Ok(Some(self.finish(
                    self.before.clone(),
                    "failed",
                    "claude_context_protocol_unsupported",
                    false,
                )));
            }
            ensure!(
                supported
                    && data["session_state"] == "idle"
                    && response["pending_permission_requests"]
                        .as_array()
                        .is_some_and(Vec::is_empty)
                    && response["pending_user_dialog_requests"]
                        .as_array()
                        .is_some_and(Vec::is_empty),
                "Claude context state is not verified idle without pending requests"
            );
            if preflight {
                let commands = data["commands"]
                    .as_array()
                    .context("Claude command catalogue missing")?;
                let matches: Vec<_> = commands
                    .iter()
                    .filter(|c| c["name"] == self.action)
                    .collect();
                if matches.len() != 1 || matches[0]["builtin"] != true {
                    return Ok(Some(self.finish(
                        self.before.clone(),
                        "failed",
                        "claude_context_builtin_unavailable",
                        false,
                    )));
                }
                if cancelled {
                    return Ok(Some(self.finish(
                        self.before.clone(),
                        "cancelled",
                        "claude_context_cancelled_before_input",
                        false,
                    )));
                }
                self.phase = "command";
                return Ok(Some(Step::Send(
                    json!({"type":"user","uuid":self.user,"session_id":self.before["session_id"],
                    "parent_tool_use_id":null,"message":{"role":"user","content":format!("/{}",self.action)}}),
                )));
            }
            let result = self
                .result
                .as_ref()
                .context("Claude context terminal result missing")?;
            let success = result["subtype"] == "success" && result["is_error"] == false;
            let interrupted = self.terminal.as_deref() == Some("cancelled")
                || ["aborted_streaming", "aborted_tools"]
                    .contains(&result["terminal_reason"].as_str().unwrap_or(""));
            ensure!(
                !success || interrupted || self.terminal.as_deref() == Some("completed"),
                "Claude context success contradicts terminal lifecycle"
            );
            let mut state = self.before.clone();
            state["session_id"] = result["session_id"].clone();
            state["initialized"] = json!(true);
            state["control_initialized"] = json!(true);
            state["require_result_identity"] = json!(true);
            state["last_result_id"] = result["uuid"].clone();
            if let Some(boundary) = &self.boundary {
                state["last_compaction_boundary_id"] = json!(boundary);
            }
            if state["session_id"] != self.before["session_id"] {
                state.as_object_mut().unwrap().remove("role_skills");
                state
                    .as_object_mut()
                    .unwrap()
                    .remove("last_compaction_boundary_id");
            }
            let (status, reason) = if interrupted {
                ("cancelled", "claude_context_interrupted")
            } else if !success {
                ("failed", "claude_context_provider_failed")
            } else if self.action == "compact" && self.boundary.is_none() {
                ("incomplete", "claude_context_compaction_not_performed")
            } else {
                ("completed", "claude_context_verified")
            };
            return Ok(Some(self.finish(state, status, reason, true)));
        }
        if self.phase == "preflight" || !row["parent_tool_use_id"].is_null() {
            return Ok(None);
        }
        let previous = self.before["session_id"]
            .as_str()
            .context("Claude context session missing")?;
        if row["type"] == "command_lifecycle" {
            if row["command_uuid"] != self.user {
                return Ok(None);
            }
            match row["state"].as_str().unwrap_or("") {
                "started" => {
                    ensure!(
                        row["session_id"] == previous,
                        "Claude context started in wrong session"
                    );
                    self.started = true;
                }
                "completed" | "failed" | "cancelled" => {
                    self.terminal = Some(row["state"].as_str().unwrap().into());
                    self.terminal_session = Some(
                        identity(&row["session_id"], "Claude terminal session missing")?.into(),
                    );
                }
                _ => {}
            }
        }
        if row["type"] == "conversation_reset" && row["user_message_uuid"] == self.user {
            ensure!(
                self.action == "clear"
                    && row["trigger"] == "clear"
                    && row["session_id"] == previous
                    && self.started,
                "unexpected Claude context reset"
            );
            let id = identity(&row["uuid"], "Claude reset identity missing")?;
            ensure!(
                self.reset.as_ref().is_none_or(|old| old == id),
                "multiple Claude resets for one command"
            );
            self.reset = Some(id.into());
        }
        if row["type"] == "system" && row["subtype"] == "init" {
            let id = identity(&row["session_id"], "Claude context init session missing")?;
            ensure!(
                self.init_sessions.len() < 4 || self.init_sessions.contains(id),
                "too many Claude context init identities"
            );
            self.init_sessions.insert(id.into());
        }
        if row["type"] == "system"
            && row["subtype"] == "compact_boundary"
            && row["session_id"] == previous
            && self.action == "compact"
            && self.started
            && row["compact_metadata"]["trigger"] == "manual"
        {
            let id = identity(&row["uuid"], "Claude compaction boundary identity missing")?;
            if self.before["last_compaction_boundary_id"] == id {
                return Ok(None);
            }
            ensure!(
                row["compact_metadata"]["pre_tokens"].as_u64().is_some(),
                "Claude compaction metadata missing"
            );
            ensure!(
                self.boundary.as_ref().is_none_or(|old| old == id),
                "multiple Claude manual compaction boundaries"
            );
            self.boundary = Some(id.into());
        }
        if row["type"] == "user" && row["uuid"] == self.user {
            let body = text(&row["message"]["content"]);
            if body.trim() == format!("/{}", self.action)
                || body.lines().next().is_some_and(|line| {
                    line.trim() == format!("<command-name>/{}</command-name>", self.action)
                })
            {
                ensure!(
                    self.replay_sessions.len() < 4
                        || row["session_id"]
                            .as_str()
                            .is_some_and(|id| self.replay_sessions.contains(id)),
                    "too many Claude context replay identities"
                );
                self.replay_sessions
                    .insert(identity(&row["session_id"], "Claude replay session missing")?.into());
            }
        }
        if row["type"] == "result"
            && (row["user_message_uuid"] == self.user
                || row["user_message_uuids"]
                    .as_array()
                    .is_some_and(|ids| ids.iter().any(|id| id == &self.user)))
        {
            ensure!(
                row["local_command"] == self.action && row["queued_turn_count"].as_u64() == Some(0),
                "Claude context result is not an isolated native command"
            );
            let id = identity(&row["uuid"], "Claude context result identity missing")?;
            ensure!(
                self.before["last_result_id"] != id,
                "Claude context result identity was reused"
            );
            ensure!(
                row["is_error"].is_boolean(),
                "Claude context result outcome missing"
            );
            self.result = Some(row.clone());
        }
        if let Some(result) = self
            .result
            .as_ref()
            .filter(|_| self.phase == "command" && self.started && self.terminal.is_some())
        {
            let session = identity(
                &result["session_id"],
                "Claude context result session missing",
            )?;
            ensure!(
                self.terminal_session.as_deref() == Some(session),
                "Claude context lifecycle/result session mismatch"
            );
            if !self.replay_sessions.contains(session) || !self.init_sessions.contains(session) {
                return Ok(None);
            }
            if self.action == "clear"
                && result["subtype"] == "success"
                && result["is_error"] == false
            {
                ensure!(
                    self.reset.is_some() && session != previous,
                    "Claude clear lacked a confirmed session change"
                );
            } else if self.reset.is_none() {
                ensure!(
                    session == previous,
                    "Claude context result changed an unverified session"
                );
            }
            self.phase = "verify";
            return Ok(Some(Step::Send(Self::initialize(&self.verify))));
        }
        Ok(None)
    }
}
fn identity<'a>(value: &'a Value, message: &str) -> Result<&'a str> {
    let id = value.as_str().context(message.to_owned())?;
    ensure!(uuid::Uuid::parse_str(id).is_ok(), "{message}");
    Ok(id)
}
fn version_supported(value: &Value) -> bool {
    let Some(v) = value.as_str() else {
        return false;
    };
    let Ok(parts) = v
        .split('.')
        .map(str::parse::<u32>)
        .collect::<std::result::Result<Vec<_>, _>>()
    else {
        return false;
    };
    parts.len() == 3 && parts.as_slice() >= [2, 1, 296].as_slice()
}
fn text(value: &Value) -> String {
    value.as_str().map(str::to_owned).unwrap_or_else(|| {
        value
            .as_array()
            .map(|parts| {
                parts
                    .iter()
                    .filter(|part| part["type"] == "text")
                    .filter_map(|part| part["text"].as_str())
                    .collect::<Vec<_>>()
                    .join("\n")
            })
            .unwrap_or_default()
    })
}
