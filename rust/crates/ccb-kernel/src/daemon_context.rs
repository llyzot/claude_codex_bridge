//! Context controls are journaled Jobs, sharing FIFO, crash fences and observation.
use super::{now, Daemon};
use crate::{admission, records};
use anyhow::{ensure, Context, Result};
use serde_json::{json, Value};
use std::collections::BTreeSet;

impl Daemon {
    pub(super) fn context_status(&mut self, request: &Value) -> Result<Value> {
        let id = records::string(request, "operation_id")?;
        ensure!(
            uuid::Uuid::parse_str(id).is_ok(),
            "invalid context operation identity"
        );
        let mut operation =
            self.kernel
                .store
                .latest_by_id("context-operations.jsonl", "operation_id", id)?;
        if operation.is_null() {
            return Ok(json!({"context":null}));
        }
        let mut results = Vec::new();
        for receipt in operation["receipts"]
            .as_array()
            .context("invalid context receipt")?
        {
            results.push(self.watch(&json!({"target":receipt["job_id"]}))?);
        }
        operation["status"] = json!(if results.iter().any(|r| r["terminal"] != true) {
            "pending"
        } else if results.iter().all(|r| r["status"] == "completed") {
            "completed"
        } else {
            "failed"
        });
        operation["results"] = json!(results);
        Ok(json!({"context":operation}))
    }

    pub(super) fn context_control(&mut self, request: &Value, action: &str) -> Result<Value> {
        ensure!(
            ["clear", "compact"].contains(&action),
            "unknown context action"
        );
        ensure!(
            request["dry_run"].is_null() || request["dry_run"].is_boolean(),
            "dry_run must be a boolean"
        );
        let id = request["operation_id"]
            .as_str()
            .map(str::to_owned)
            .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
        ensure!(
            (request["operation_id"].is_null() || request["operation_id"].is_string())
                && uuid::Uuid::parse_str(&id).is_ok(),
            "invalid context operation identity"
        );
        let mut selectors: Vec<String> = if request["agent_names"].is_null() {
            vec!["all".into()]
        } else {
            serde_json::from_value(request["agent_names"].clone())
                .context("agent_names must be strings")?
        };
        if selectors.is_empty() {
            selectors.push("all".into());
        }
        for name in &mut selectors {
            *name = name.trim().to_lowercase();
        }
        selectors.sort();
        selectors.dedup();
        ensure!(
            selectors.len() <= 32 && !selectors.iter().any(String::is_empty),
            "invalid context targets"
        );
        ensure!(
            selectors.len() == 1 || !selectors.iter().any(|s| s == "all"),
            "all cannot be combined with agent names"
        );
        let previous =
            self.kernel
                .store
                .latest_by_id("context-operations.jsonl", "operation_id", &id)?;
        if !previous.is_null() {
            ensure!(
                previous["action"] == action && previous["requested_agents"] == json!(selectors),
                "context operation identity already bound to another request"
            );
            return self.context_status(&json!({"operation_id":id}));
        }
        let names: BTreeSet<String> = if selectors == ["all"] {
            self.names().into_iter().collect()
        } else {
            selectors
                .iter()
                .map(|s| crate::roles::resolve(s, &self.config["agents"]))
                .collect::<Result<_>>()?
        };
        let names: Vec<_> = names.into_iter().collect();
        let mut blockers = super::reload::blockers(&mut self.kernel, &names)?;
        let mut runtimes = json!({});
        for name in &names {
            let spec = &self.config["agents"][name];
            let owned = self.owned_view(name);
            if !["pi_rpc", "codex_app_server", "claude_stream"]
                .contains(&spec["driver_kind"].as_str().unwrap_or(""))
            {
                blockers.push(
                    json!({"agent_name":name,"reason":"native_context_protocol_unsupported"}),
                );
            }
            if self.reload_frozen(name)
                || spec["dispatch_disabled"] == true
                || owned["ready"] != true
                || owned["closed"] == true
            {
                blockers.push(json!({"agent_name":name,"reason":"provider_not_available"}));
            }
            runtimes[name] = owned["runtime_instance_id"].clone();
        }
        let plan = json!({"action":action,"agent_names":names,"can_apply":blockers.is_empty(),"blockers":blockers});
        if request["dry_run"] == true {
            return Ok(json!({"plan":plan}));
        }
        if plan["can_apply"] != true {
            return Ok(json!({"context":{"status":"blocked","plan":plan}}));
        }
        let mut receipts = Vec::new();
        for name in &names {
            let spec = &self.config["agents"][name];
            let request = json!({"project_id":self.config["project_id"],"to_agent":name,"from_actor":"user",
                "body":format!("Native context {action}"),"message_type":"context_control","delivery_scope":"single",
                "silence_on_success":false,"route_options":{},"body_artifact":null,"task_id":null,"reply_to":null});
            let control =
                json!({"action":action,"operation_id":id,"runtime_instance_id":runtimes[name]});
            let draft = json!({"agent_name":name,"provider":spec["provider"],"target_kind":"agent","target_name":name,
                "request":request,"workspace_path":spec["workspace_path"],"provider_options":{"native_context_control":control}});
            let names = self.names();
            let admitted = admission::submit(
                &mut self.kernel,
                &json!({"state_id":self.state,"agent_names":names,
                "bureau_enabled":true,"max_callback_depth":5,"accepted_at":now(),"plan":{"project_id":request["project_id"],
                "from_actor":"user","task_id":null,"request":request,"drafts":[draft],"submission_id":null,
                "target_scope":"single","origin_message_id":null}}),
            )?;
            receipts.push(admitted["receipt"].clone());
        }
        // The actor commits this receipt and every Job in the same transaction,
        // before its next tick may send any provider command.
        let mut operation = json!({"schema_version":1,"record_type":"native_context_operation",
            "operation_id":id,"action":action,"requested_agents":selectors,"agent_names":names,"receipts":receipts,"accepted_at":now()});
        self.kernel
            .store
            .append("context-operations.jsonl", &operation)?;
        operation["status"] = json!("pending");
        Ok(json!({"context":operation}))
    }
}
