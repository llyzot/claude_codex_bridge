//! Completion detectors and reply selection, owned by the Rust runtime.
use crate::{
    parse_time,
    records::{string, truthy},
};
use anyhow::{bail, ensure, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::HashMap;

#[derive(Clone, Copy, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
enum Family {
    ProtocolTurn,
    StructuredResult,
    SessionBoundary,
    AnchoredSessionStability,
    TerminalTextQuiet,
}
#[derive(Clone, Copy, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
enum Kind {
    AnchorSeen,
    AssistantChunk,
    AssistantFinal,
    ToolCall,
    ToolResult,
    Result,
    TurnBoundary,
    TurnAborted,
    CancelInfo,
    Error,
    PaneDead,
    SessionSnapshot,
    SessionMutation,
    SessionRotate,
}
#[derive(Clone, Serialize)]
struct State {
    schema_version: u32,
    record_type: &'static str,
    anchor_seen: bool,
    reply_started: bool,
    reply_stable: bool,
    tool_active: bool,
    subagent_activity_seen: bool,
    last_reply_hash: Option<String>,
    last_reply_at: Option<String>,
    stable_since: Option<String>,
    provider_turn_ref: Option<String>,
    latest_cursor: Value,
    terminal: bool,
}
impl State {
    fn new(cursor: Value) -> Self {
        Self {
            schema_version: 2,
            record_type: "completion_state",
            anchor_seen: false,
            reply_started: false,
            reply_stable: false,
            tool_active: false,
            subagent_activity_seen: false,
            last_reply_hash: None,
            last_reply_at: None,
            stable_since: None,
            provider_turn_ref: None,
            latest_cursor: cursor,
            terminal: false,
        }
    }
}
#[derive(Clone)]
struct Tracker {
    agent: String,
    provider: String,
    req_id: String,
    family: Family,
    state: State,
    decision: Value,
    started_at: String,
    timeout: f64,
    settle: f64,
    candidates: Vec<(u8, String)>,
}
#[derive(Default)]
pub struct Completion {
    groups: HashMap<String, Vec<(String, Tracker)>>,
}
impl Completion {
    pub fn dispatch(&mut self, op: &str, args: &Value) -> Result<Value> {
        if op == "stats" {
            return Ok(
                json!({"group_count":self.groups.len(),"tracker_count":self.groups.values().map(Vec::len).sum::<usize>()}),
            );
        }
        let group = string(args, "group_id")?;
        if op == "drop_group" {
            self.groups.remove(group);
            return Ok(Value::Null);
        }
        if op == "create" {
            ensure!(!self.groups.contains_key(group), "completion group exists");
            self.groups.insert(group.into(), Vec::new());
            return Ok(Value::Null);
        }
        let trackers = self
            .groups
            .get_mut(group)
            .ok_or_else(|| anyhow::anyhow!("unknown completion group"))?;
        if op == "tick_all" {
            let timestamp = string(args, "now")?;
            let mut staged = trackers.clone();
            for (_, tracker) in &mut staged {
                tracker.tick(timestamp)?;
            }
            let result = json!(staged
                .iter()
                .map(|(_, tracker)| tracker.view())
                .collect::<Vec<_>>());
            *trackers = staged;
            return Ok(result);
        }
        let id = string(args, "job_id")?;
        let position = trackers.iter().position(|(key, _)| key == id);
        if op == "start" {
            let profile = &args["profile"];
            let family: Family = serde_json::from_value(profile["completion_family"].clone())?;
            ensure!(
                ["final_message", "structured_result", "session_reply"]
                    .contains(&string(profile, "selector_family")?),
                "invalid selector family"
            );
            let cursor = json!({"schema_version":2,"record_type":"completion_cursor","source_kind":profile["completion_source_kind"],
                "opaque_cursor":null,"session_path":null,"offset":null,"line_no":null,"event_seq":0,"updated_at":args["started_at"]});
            validate_cursor(&cursor)?;
            let timeout = args["timeout_s"]
                .as_f64()
                .ok_or_else(|| anyhow::anyhow!("invalid timeout"))?;
            let agent = string(args, "agent_name")?.trim().to_ascii_lowercase();
            crate::models::validate_agent(&agent)?;
            let mut tracker = Tracker {
                agent,
                provider: string(args, "provider")?.trim().to_ascii_lowercase(),
                req_id: id.into(),
                family,
                state: State::new(cursor),
                decision: Value::Null,
                started_at: string(args, "started_at")?.into(),
                timeout,
                settle: args["settle_window_s"].as_f64().unwrap_or(2.0),
                candidates: Vec::new(),
            };
            tracker.pending();
            let result = tracker.view();
            if let Some(index) = position {
                trackers[index].1 = tracker;
            } else {
                trackers.push((id.into(), tracker));
            }
            return Ok(result);
        }
        if op == "finish" {
            if let Some(index) = position {
                trackers.remove(index);
            }
            return Ok(Value::Null);
        }
        if op == "current" {
            return Ok(position
                .map(|index| trackers[index].1.view())
                .unwrap_or(Value::Null));
        }
        let index = position.ok_or_else(|| anyhow::anyhow!("unknown completion tracker: {id}"))?;
        let mut tracker = trackers[index].1.clone();
        match op {
            "ingest" => tracker.ingest(&args["item"])?,
            "tick" => tracker.tick(string(args, "now")?)?,
            _ => bail!("unsupported completion operation: {op}"),
        }
        let result = tracker.view();
        trackers[index].1 = tracker;
        Ok(result)
    }
}
impl Tracker {
    fn pending(&mut self) {
        self.decision = json!({"schema_version":2,"record_type":"completion_decision","terminal":false,"status":"incomplete","reason":null,"confidence":null,
            "reply":"","anchor_seen":self.state.anchor_seen,"reply_started":self.state.reply_started,"reply_stable":self.state.reply_stable,
            "provider_turn_ref":self.state.provider_turn_ref,"source_cursor":self.state.latest_cursor,"finished_at":null,"diagnostics":{}});
    }
    fn terminal(
        &mut self,
        status: &str,
        reason: &str,
        confidence: &str,
        timestamp: &str,
        reply: &str,
        diagnostics: Value,
    ) {
        self.state.terminal = true;
        self.decision = json!({"schema_version":2,"record_type":"completion_decision","terminal":true,"status":status,"reason":reason,"confidence":confidence,
            "reply":reply,"anchor_seen":self.state.anchor_seen,"reply_started":self.state.reply_started,"reply_stable":self.state.reply_stable,
            "provider_turn_ref":self.state.provider_turn_ref,"source_cursor":self.state.latest_cursor,"finished_at":timestamp,"diagnostics":diagnostics});
    }
    fn view(&self) -> Value {
        let mut decision = self.decision.clone();
        if !truthy(&decision["reply"]) {
            // Lowest priority wins; newer candidates win ties.
            if let Some((_, text)) = self
                .candidates
                .iter()
                .enumerate()
                .min_by_key(|(index, (priority, _))| (*priority, std::cmp::Reverse(*index)))
                .map(|(_, candidate)| candidate)
            {
                decision["reply"] = json!(text);
            }
        }
        json!({"job_id":self.req_id,"agent_name":self.agent,"state":self.state,"decision":decision})
    }
    fn record_reply(
        &mut self,
        timestamp: &str,
        text: &str,
        stable: bool,
        fingerprint: Option<String>,
    ) {
        let text = text.trim();
        if text.is_empty() {
            return;
        }
        self.state.reply_started = true;
        self.state.last_reply_hash =
            Some(fingerprint.unwrap_or_else(|| fingerprint_text(&[text.to_owned()])));
        self.state.last_reply_at = Some(timestamp.into());
        self.state.reply_stable = stable;
    }
    fn consume_common(&mut self, kind: Kind, item: &Value) {
        self.state.latest_cursor = item["cursor"].clone();
        match kind {
            Kind::AnchorSeen => self.state.anchor_seen = true,
            Kind::ToolCall => self.state.tool_active = true,
            Kind::ToolResult => self.state.tool_active = false,
            Kind::SessionRotate => {
                self.state.anchor_seen = false;
                self.state.reply_started = false;
                self.state.reply_stable = false;
                self.state.tool_active = false;
                self.state.last_reply_hash = None;
                self.state.last_reply_at = None;
                self.state.stable_since = None;
            }
            _ => {}
        }
        let payload = &item["payload"];
        if first(payload, &["subagent_id", "subagent_name"]).is_some() {
            self.state.subagent_activity_seen = true;
        }
        if let Some(reference) = first(
            payload,
            &[
                "turn_id",
                "provider_turn_ref",
                "message_id",
                "provider_session_id",
                "session_id",
            ],
        ) {
            self.state.provider_turn_ref = Some(reference);
        }
    }
    fn candidates(&mut self, kind: Kind, payload: &Value) {
        let mut add = |priority: u8, keys: &[&str]| {
            if let Some(text) = first(payload, keys) {
                self.candidates.push((priority, text));
            }
        };
        add(2, &["last_agent_message"]);
        match kind {
            Kind::Result => add(3, &["reply", "result_text", "final_answer", "text"]),
            Kind::AssistantFinal => add(4, &["text", "reply"]),
            Kind::AssistantChunk => add(5, &["merged_text", "text", "reply"]),
            Kind::SessionSnapshot | Kind::SessionMutation => add(6, &["reply", "content", "text"]),
            _ => {}
        }
        add(7, &["fallback_text"]);
    }
    fn ingest(&mut self, item: &Value) -> Result<()> {
        ensure!(
            item["schema_version"] == 2 && item["record_type"] == "completion_item",
            "invalid completion item schema"
        );
        let kind: Kind = serde_json::from_value(item["kind"].clone())?;
        // Keep stale or foreign source observations away from this request.
        if item["req_id"] != self.req_id
            || item["agent_name"] != self.agent
            || item["provider"] != self.provider
        {
            return Ok(());
        }
        let timestamp = string(item, "timestamp")?;
        ensure!(!timestamp.is_empty(), "empty completion timestamp");
        validate_cursor(&item["cursor"])?;
        ensure!(item["payload"].is_object(), "invalid completion payload");
        let payload = &item["payload"];
        if kind == Kind::SessionRotate {
            self.candidates.clear();
        }
        self.candidates(kind, payload);
        self.consume_common(kind, item);
        let diagnostics = diagnostics(item);
        match self.family {
            Family::ProtocolTurn | Family::SessionBoundary => {
                let protocol = self.family == Family::ProtocolTurn;
                if matches!(kind, Kind::AssistantChunk | Kind::AssistantFinal)
                    || protocol && kind == Kind::Result
                {
                    let keys = if protocol {
                        vec![
                            "last_agent_message",
                            "final_answer",
                            "reply",
                            "result_text",
                            "text",
                        ]
                    } else {
                        vec!["text", "reply", "last_agent_message"]
                    };
                    if let Some(reply) = first(payload, &keys) {
                        self.record_reply(timestamp, &reply, false, None);
                    }
                } else if kind == Kind::TurnBoundary {
                    let keys = if protocol {
                        vec!["last_agent_message", "final_answer", "reply", "text"]
                    } else {
                        vec!["last_agent_message", "reply", "text"]
                    };
                    let reply = first(payload, &keys).unwrap_or_default();
                    if !reply.is_empty() {
                        self.record_reply(timestamp, &reply, true, None);
                    } else if !self.state.reply_started
                        || protocol && truthy(&payload["empty_final_message"])
                    {
                        let mut diagnostics = diagnostics;
                        let diagnosis = if protocol {
                            "Provider protocol reported task_complete without assistant reply text; inspect the protocol session log, pane state, and authentication/API output."
                        } else {
                            "Provider session boundary reported completion without assistant reply text; inspect the provider session log, pane state, and authentication/API output."
                        };
                        for (key, value) in [
                            (
                                "provider_terminal_reason",
                                json!(first(payload, &["reason", "completion_reason"])
                                    .unwrap_or_else(|| if protocol {
                                        "task_complete".into()
                                    } else {
                                        "turn_duration".into()
                                    })),
                            ),
                            ("empty_reply", json!(true)),
                            ("error_type", json!("empty_provider_reply")),
                            ("message", json!(diagnosis)),
                            ("diagnosis", json!(diagnosis)),
                        ] {
                            diagnostics
                                .as_object_mut()
                                .unwrap()
                                .entry(key)
                                .or_insert(value);
                        }
                        self.terminal(
                            "incomplete",
                            "task_complete_empty_reply",
                            if protocol { "exact" } else { "observed" },
                            timestamp,
                            "",
                            diagnostics,
                        );
                        return Ok(());
                    }
                    self.terminal(
                        "completed",
                        &first(payload, &["reason", "completion_reason"]).unwrap_or_else(|| {
                            if protocol {
                                "task_complete".into()
                            } else {
                                "turn_duration".into()
                            }
                        }),
                        if protocol { "exact" } else { "observed" },
                        timestamp,
                        &reply,
                        json!({}),
                    );
                    return Ok(());
                } else if protocol && kind == Kind::TurnAborted {
                    let reply = first(payload, &["last_agent_message", "reply", "text"])
                        .unwrap_or_default();
                    if !reply.is_empty() {
                        self.record_reply(timestamp, &reply, true, None);
                    }
                    self.terminal(
                        abort_status(payload),
                        &first(payload, &["reason"]).unwrap_or("turn_aborted".into()),
                        "exact",
                        timestamp,
                        &reply,
                        diagnostics,
                    );
                    return Ok(());
                } else if matches!(kind, Kind::Error | Kind::PaneDead) {
                    let reason = first(
                        payload,
                        if kind == Kind::PaneDead {
                            &["reason"]
                        } else {
                            &["reason", "error"]
                        },
                    )
                    .unwrap_or_else(|| {
                        if kind == Kind::PaneDead {
                            "pane_dead".into()
                        } else if protocol {
                            "transport_error".into()
                        } else {
                            "api_error".into()
                        }
                    });
                    self.terminal(
                        "failed",
                        &reason,
                        if protocol || kind == Kind::PaneDead {
                            "degraded"
                        } else {
                            "observed"
                        },
                        timestamp,
                        "",
                        diagnostics,
                    );
                    return Ok(());
                }
            }
            Family::StructuredResult => {
                if matches!(kind, Kind::AssistantChunk | Kind::AssistantFinal) {
                    if let Some(reply) = first(payload, &["reply", "final_answer", "text"]) {
                        self.record_reply(timestamp, &reply, false, None);
                    }
                } else if kind == Kind::Result {
                    let reply = first(payload, &["reply", "result_text", "final_answer", "text"])
                        .unwrap_or_default();
                    if !reply.is_empty() {
                        self.record_reply(timestamp, &reply, true, None);
                    }
                    self.terminal(
                        "completed",
                        &first(payload, &["reason", "completion_reason"])
                            .unwrap_or("stream_result".into()),
                        "exact",
                        timestamp,
                        &reply,
                        json!({}),
                    );
                    return Ok(());
                } else if matches!(
                    kind,
                    Kind::CancelInfo | Kind::TurnAborted | Kind::Error | Kind::PaneDead
                ) {
                    let (status, fallback, confidence) = match kind {
                        Kind::CancelInfo => ("cancelled", "cancel_info", "exact"),
                        Kind::TurnAborted => (abort_status(payload), "turn_aborted", "exact"),
                        Kind::Error => ("failed", "transport_error", "exact"),
                        _ => ("failed", "pane_dead", "degraded"),
                    };
                    self.terminal(
                        status,
                        &first(
                            payload,
                            if kind == Kind::Error {
                                &["reason", "error"]
                            } else {
                                &["reason"]
                            },
                        )
                        .unwrap_or(fallback.into()),
                        confidence,
                        timestamp,
                        "",
                        diagnostics,
                    );
                    return Ok(());
                }
            }
            Family::AnchoredSessionStability => {
                if kind == Kind::CancelInfo {
                    self.terminal(
                        "cancelled",
                        &first(payload, &["reason"]).unwrap_or("cancel_info".into()),
                        "observed",
                        timestamp,
                        "",
                        json!({}),
                    );
                    return Ok(());
                }
                if matches!(kind, Kind::SessionSnapshot | Kind::SessionMutation) {
                    if !payload["tool_call_count"].is_null() {
                        self.state.tool_active = integer_truth(&payload["tool_call_count"]);
                    }
                    if let Some(reply) = first(payload, &["reply", "content", "text"]) {
                        let hash = fingerprint_text(&[
                            first(payload, &["message_id"]).unwrap_or_default(),
                            reply.clone(),
                            python_string(&payload["message_count"]),
                            python_string(&payload["last_updated"]),
                        ]);
                        if self.state.last_reply_hash.as_deref() != Some(&hash) {
                            self.record_reply(timestamp, &reply, false, Some(hash));
                            self.state.stable_since = Some(timestamp.into());
                        }
                    }
                } else if matches!(kind, Kind::Error | Kind::PaneDead) {
                    let reason = first(
                        payload,
                        if kind == Kind::Error {
                            &["reason", "error"]
                        } else {
                            &["reason"]
                        },
                    )
                    .unwrap_or_else(|| {
                        if kind == Kind::Error {
                            "session_corrupt".into()
                        } else {
                            "pane_dead".into()
                        }
                    });
                    self.terminal(
                        "failed",
                        &reason,
                        if kind == Kind::Error {
                            "observed"
                        } else {
                            "degraded"
                        },
                        timestamp,
                        "",
                        json!({}),
                    );
                    return Ok(());
                }
            }
            Family::TerminalTextQuiet => {
                if matches!(
                    kind,
                    Kind::AssistantChunk | Kind::AssistantFinal | Kind::Result
                ) {
                    let reply = first(payload, &["reply", "result_text", "final_answer", "text"])
                        .unwrap_or_default();
                    if !reply.is_empty() {
                        self.record_reply(timestamp, &reply, false, None);
                    }
                    if truthy(&payload["done_marker"]) || truthy(&payload["ccb_done"]) {
                        self.terminal(
                            "completed",
                            "terminal_done_marker",
                            "degraded",
                            timestamp,
                            &reply,
                            json!({}),
                        );
                        return Ok(());
                    }
                } else if matches!(kind, Kind::CancelInfo | Kind::Error | Kind::PaneDead) {
                    let (status, fallback) = if kind == Kind::CancelInfo {
                        ("cancelled", "cancel_info")
                    } else if kind == Kind::Error {
                        ("failed", "transport_error")
                    } else {
                        ("failed", "pane_dead")
                    };
                    self.terminal(
                        status,
                        &first(
                            payload,
                            if kind == Kind::Error {
                                &["reason", "error"]
                            } else {
                                &["reason"]
                            },
                        )
                        .unwrap_or(fallback.into()),
                        "degraded",
                        timestamp,
                        "",
                        json!({}),
                    );
                    return Ok(());
                }
            }
        }
        self.pending();
        Ok(())
    }
    fn tick(&mut self, now: &str) -> Result<()> {
        if !truthy(&self.decision["terminal"])
            && self.family == Family::AnchoredSessionStability
            && self.state.reply_started
            && self.state.stable_since.is_some()
        {
            if self.state.tool_active {
                self.pending();
            } else {
                let elapsed = seconds_between(self.state.stable_since.as_deref().unwrap(), now)
                    .ok_or_else(|| anyhow::anyhow!("invalid stability timestamp"))?;
                if elapsed >= self.settle {
                    self.state.reply_stable = true;
                    self.terminal(
                        "completed",
                        "session_reply_stable",
                        "observed",
                        now,
                        "",
                        json!({}),
                    );
                } else {
                    self.pending();
                }
            }
        }
        if self.timeout > 0.0
            && !truthy(&self.decision["terminal"])
            && seconds_between(&self.started_at, now).is_some_and(|elapsed| elapsed >= self.timeout)
        {
            if self.family == Family::TerminalTextQuiet && self.state.reply_started {
                self.terminal(
                    "completed",
                    "terminal_quiet",
                    "degraded",
                    now,
                    "",
                    json!({}),
                );
            } else {
                self.terminal("incomplete", "timeout", "degraded", now, "", json!({}));
            }
        }
        Ok(())
    }
}
fn validate_cursor(cursor: &Value) -> Result<()> {
    ensure!(
        cursor["schema_version"] == 2 && cursor["record_type"] == "completion_cursor",
        "invalid completion cursor schema"
    );
    ensure!(
        [
            "protocol_event_stream",
            "structured_result_stream",
            "session_event_log",
            "session_snapshot",
            "terminal_text"
        ]
        .contains(&string(cursor, "source_kind")?),
        "invalid completion source"
    );
    for field in ["offset", "line_no", "event_seq"] {
        ensure!(
            cursor[field].is_null() || cursor[field].as_u64().is_some(),
            "invalid cursor {field}"
        );
    }
    Ok(())
}
fn first(payload: &Value, keys: &[&str]) -> Option<String> {
    keys.iter().find_map(|key| {
        payload[key]
            .as_str()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
    })
}
fn diagnostics(item: &Value) -> Value {
    let mut payload = item["payload"].clone();
    for key in [
        "reply",
        "content",
        "final_answer",
        "result_text",
        "last_agent_message",
    ] {
        payload.as_object_mut().unwrap().remove(key);
    }
    payload
}
fn fingerprint_text(parts: &[String]) -> String {
    let mut hash = Sha256::new();
    for part in parts {
        hash.update(part.as_bytes());
        hash.update([0x1f]);
    }
    format!("{:x}", hash.finalize())
}
fn python_string(value: &Value) -> String {
    match value {
        Value::Null => "None".into(),
        Value::Bool(true) => "True".into(),
        Value::Bool(false) => "False".into(),
        Value::String(s) => s.clone(),
        _ => value.to_string(),
    }
}
fn integer_truth(value: &Value) -> bool {
    if let Some(n) = value.as_f64() {
        n.trunc() > 0.0
    } else if let Some(s) = value.as_str() {
        s.trim()
            .parse::<i64>()
            .map(|n| n > 0)
            .unwrap_or_else(|_| truthy(value))
    } else {
        truthy(value)
    }
}
fn abort_status(payload: &Value) -> &'static str {
    let status = payload["status"]
        .as_str()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    match status.as_str() {
        "completed" => return "completed",
        "cancelled" => return "cancelled",
        "failed" => return "failed",
        "incomplete" => return "incomplete",
        _ => {}
    }
    if payload["reason"]
        .as_str()
        .is_some_and(|s| s.to_lowercase().contains("cancel") || s.to_lowercase().contains("abort"))
    {
        "cancelled"
    } else {
        "failed"
    }
}
fn seconds_between(start: &str, end: &str) -> Option<f64> {
    Some((parse_time(end)? - parse_time(start)?).num_microseconds()? as f64 / 1_000_000.0)
}

pub fn snapshot(kernel: &mut crate::Kernel, op: &str, args: &Value) -> Result<Value> {
    let id = string(args, "job_id")?;
    ensure!(
        !id.is_empty()
            && id != "."
            && id != ".."
            && id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-' || b == b'.'),
        "invalid snapshot job id"
    );
    let path = format!("snapshots/{id}.json");
    match op {
        "load" => Ok(kernel.store.json(&path)?.unwrap_or(Value::Null)),
        "save" => {
            let record = &args["record"];
            ensure!(
                record["schema_version"] == 2
                    && record["record_type"] == "completion_snapshot"
                    && record["job_id"] == id,
                "invalid snapshot schema"
            );
            crate::models::validate_agent(string(record, "agent_name")?)?;
            ensure!(
                !string(record, "updated_at")?.is_empty(),
                "empty snapshot timestamp"
            );
            let _: Family = serde_json::from_value(record["profile_family"].clone())?;
            let state = &record["state"];
            ensure!(
                state["schema_version"] == 2 && state["record_type"] == "completion_state",
                "invalid completion state schema"
            );
            let decision = &record["latest_decision"];
            ensure!(
                decision["schema_version"] == 2 && decision["record_type"] == "completion_decision",
                "invalid completion decision schema"
            );
            ensure!(
                ["completed", "cancelled", "failed", "incomplete"]
                    .contains(&string(decision, "status")?),
                "invalid completion status"
            );
            if truthy(&decision["terminal"]) {
                string(decision, "reason")?;
                string(decision, "finished_at")?;
                ensure!(
                    ["exact", "observed", "degraded"].contains(&string(decision, "confidence")?),
                    "invalid completion confidence"
                );
            } else {
                ensure!(
                    decision["status"] == "incomplete"
                        && decision["reason"].is_null()
                        && decision["confidence"].is_null()
                        && decision["finished_at"].is_null(),
                    "invalid pending decision"
                );
            }
            for cursor in [&state["latest_cursor"], &decision["source_cursor"]] {
                if !cursor.is_null() {
                    validate_cursor(cursor)?;
                }
            }
            kernel.store.save(&path, record)?;
            Ok(Value::Null)
        }
        _ => bail!("unsupported completion snapshot operation"),
    }
}
