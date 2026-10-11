//! One-time Claude tool permissions bound to observed, stamped tool calls.
use anyhow::{ensure, Context, Result};
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};

struct Pending {
    handle: String,
    wire: String,
    params: Value,
    answer: Option<Value>,
}
#[derive(Default)]
pub struct Broker {
    pending: Vec<Pending>,
    tools: HashMap<String, String>,
    retired: HashSet<String>,
    assistant_bound: bool,
    stream_bound: bool,
}
impl Broker {
    pub fn consistent(&self, wire: &str, params: &Value) -> bool {
        self.pending
            .iter()
            .find(|p| p.wire == wire)
            .is_none_or(|p| p.params == *params)
    }
    pub fn observe(&mut self, row: &Value, user: &str) -> Result<()> {
        let kind = row["type"].as_str().unwrap_or("");
        if !["assistant", "stream_event"].contains(&kind) {
            return Ok(());
        }
        ensure!(
            row["parent_tool_use_id"].is_null()
                || row["parent_tool_use_id"]
                    .as_str()
                    .is_some_and(|id| !id.is_empty() && id.len() <= 256),
            "invalid Claude tool parent identity"
        );
        let parent = row["parent_tool_use_id"].as_str();
        let owned = if let Some(parent) = parent {
            self.tools.contains_key(parent)
        } else {
            if !row["user_message_uuid"].is_null() || !row["user_message_uuids"].is_null() {
                let matches = row["user_message_uuid"] == user
                    || row["user_message_uuids"]
                        .as_array()
                        .is_some_and(|ids| ids.len() <= 64 && ids.iter().any(|id| id == user));
                if !matches {
                    self.assistant_bound = false;
                    self.stream_bound = false;
                } else if kind == "assistant" {
                    self.assistant_bound = true;
                } else {
                    self.stream_bound = true;
                }
            }
            if kind == "assistant" {
                self.assistant_bound
            } else {
                self.stream_bound
            }
        };
        if !owned {
            return Ok(());
        }
        if kind == "assistant" {
            if let Some(blocks) = row["message"]["content"].as_array() {
                for block in blocks {
                    self.tool(block)?;
                }
            }
        } else if row["event"]["type"] == "content_block_start" {
            self.tool(&row["event"]["content_block"])?;
        }
        Ok(())
    }
    fn tool(&mut self, block: &Value) -> Result<()> {
        if block["type"] != "tool_use" {
            return Ok(());
        }
        let id = text(&block["id"])?;
        let name = text(&block["name"])?;
        if let Some(previous) = self.tools.get(id) {
            ensure!(previous == name, "Claude tool identity was reused");
        } else {
            ensure!(
                self.tools.len() < 4096,
                "Claude tool identity capacity exceeded"
            );
            self.tools.insert(id.into(), name.into());
        }
        Ok(())
    }
    pub fn add(&mut self, wire: &str, params: &Value) -> Result<()> {
        text(&json!(wire))?;
        ensure!(
            params["subtype"] == "can_use_tool",
            "unsupported Claude control request"
        );
        text(&params["tool_name"])?;
        text(&params["tool_use_id"])?;
        ensure!(
            params["input"].is_object(),
            "invalid Claude permission input"
        );
        for key in [
            "requires_user_interaction",
            "suppress_always_allow_rule",
            "default_to_no",
            "classifier_approvable",
        ] {
            ensure!(
                params[key].is_null() || params[key].is_boolean(),
                "invalid Claude consent metadata"
            );
        }
        ensure!(!self.retired.contains(wire), "Claude request was withdrawn");
        if let Some(prior) = self.pending.iter().find(|p| p.wire == wire) {
            ensure!(
                prior.params == *params,
                "Claude permission request identity was reused"
            );
            return Ok(());
        }
        ensure!(
            self.pending.len() < 32
                && self
                    .pending
                    .iter()
                    .map(|p| p.params.to_string().len())
                    .sum::<usize>()
                    + params.to_string().len()
                    <= 512 * 1024,
            "Claude pending permission capacity exceeded"
        );
        self.pending.push(Pending {
            handle: format!("crq-{}", uuid::Uuid::new_v4().simple()),
            wire: wire.into(),
            params: params.clone(),
            answer: None,
        });
        Ok(())
    }
    fn anchored(&self, p: &Pending) -> bool {
        p.params["tool_use_id"]
            .as_str()
            .and_then(|id| self.tools.get(id))
            .is_some_and(|name| p.params["tool_name"] == *name)
    }
    fn needs_ui(p: &Pending) -> bool {
        p.params["requires_user_interaction"] == true || p.params["tool_name"] == "AskUserQuestion"
    }
    pub fn rows(&self, job: &Value, runtime: &str) -> Value {
        json!(self.pending.iter().map(|p| json!({"request_id":p.handle,"method":"can_use_tool","params":p.params,
            "job_id":job["job_id"],"execution_instance_id":job["provider_options"]["native_execution_instance"],
            "runtime_instance_id":runtime,"status":if p.answer.is_some(){"response_queued"}else if !self.anchored(p){"awaiting_tool_anchor"}
                else if Self::needs_ui(p){"native_ui_required"}else{"awaiting_user"}})).collect::<Vec<_>>())
    }
    pub fn summaries(&self) -> Value {
        json!(self.pending.iter().map(|p| json!({"request_id":p.handle,"method":"can_use_tool",
            "status":if p.answer.is_some(){"response_queued"}else if !self.anchored(p){"awaiting_tool_anchor"}
                else if Self::needs_ui(p){"native_ui_required"}else{"awaiting_user"}})).collect::<Vec<_>>())
    }
    pub fn validate(&self, handle: &str, result: &Value) -> Result<bool> {
        let p = self
            .pending
            .iter()
            .find(|p| p.handle == handle)
            .context("unknown or resolved Claude request")?;
        ensure!(
            self.anchored(p),
            "Claude request has no matching current-task tool anchor"
        );
        ensure!(
            !Self::needs_ui(p),
            "Claude request requires a native interaction surface"
        );
        let answer = answer(&p.params, result)?;
        if let Some(prior) = &p.answer {
            ensure!(
                *prior == answer,
                "Claude request already has a different answer"
            );
            return Ok(false);
        }
        Ok(true)
    }
    pub fn response(&self, handle: &str, result: &Value) -> Result<Value> {
        let p = self
            .pending
            .iter()
            .find(|p| p.handle == handle)
            .context("unknown Claude request")?;
        Ok(
            json!({"type":"control_response","response":{"subtype":"success","request_id":p.wire,"response":answer(&p.params,result)?}}),
        )
    }
    pub fn answered(&mut self, handle: &str, result: &Value) -> Result<()> {
        let p = self
            .pending
            .iter_mut()
            .find(|p| p.handle == handle)
            .context("unknown Claude request")?;
        p.answer = Some(answer(&p.params, result)?);
        Ok(())
    }
    pub fn withdraw(&mut self, wire: &str) -> Result<()> {
        text(&json!(wire))?;
        ensure!(
            self.retired.len() < 4096 || self.retired.contains(wire),
            "Claude withdrawn request capacity exceeded"
        );
        self.retired.insert(wire.into());
        self.pending.retain(|p| p.wire != wire);
        Ok(())
    }
    pub fn clear(&mut self) {
        self.pending.clear();
    }
}
fn text(value: &Value) -> Result<&str> {
    value
        .as_str()
        .filter(|s| !s.is_empty() && s.len() <= 256)
        .context("invalid Claude request identity")
}
fn answer(params: &Value, result: &Value) -> Result<Value> {
    let map = result
        .as_object()
        .context("Claude answer must be an object")?;
    let allow = result["behavior"] == "allow";
    ensure!(
        allow || result["behavior"] == "deny",
        "Claude answer requires allow or deny"
    );
    let keys: &[&str] = if allow {
        &["behavior", "updatedInput", "toolUseID"]
    } else {
        &["behavior", "message", "interrupt", "toolUseID"]
    };
    ensure!(
        map.keys().all(|key| keys.contains(&key.as_str())),
        "Claude answer contains unsupported fields; only one-time decisions are supported"
    );
    if let Some(id) = map.get("toolUseID") {
        ensure!(
            *id == params["tool_use_id"],
            "Claude answer tool identity mismatch"
        );
    }
    if allow {
        if let Some(input) = map.get("updatedInput") {
            ensure!(
                *input == params["input"],
                "Claude approval cannot change the requested input"
            );
        }
        Ok(
            json!({"behavior":"allow","updatedInput":params["input"],"toolUseID":params["tool_use_id"]}),
        )
    } else {
        ensure!(
            result["message"]
                .as_str()
                .is_some_and(|s| !s.is_empty() && s.len() <= 8192),
            "Claude denial requires a bounded message"
        );
        ensure!(
            result["interrupt"].is_null() || result["interrupt"].is_boolean(),
            "invalid Claude denial interrupt flag"
        );
        let mut value = json!({"behavior":"deny","message":result["message"],"toolUseID":params["tool_use_id"]});
        if !result["interrupt"].is_null() {
            value["interrupt"] = result["interrupt"].clone();
        }
        Ok(value)
    }
}
pub fn reject(wire: &Value, message: &str) -> Value {
    json!({"type":"control_response","response":{"subtype":"error","request_id":wire,"error":message}})
}
