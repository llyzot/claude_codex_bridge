//! Callback admission policy is checked against the native active/durable state.
use crate::{admission, records, Kernel};
use anyhow::{ensure, Result};
use serde_json::{json, Value};
use std::collections::HashSet;

fn normalized(value: &Value) -> String {
    value.as_str().unwrap_or("").trim().to_lowercase()
}
pub fn is_chain(request: &Value) -> bool {
    normalized(&request["route_options"]["mode"]) == "chain"
}
fn lookup(kernel: &mut Kernel, stream: &str, identity: &str, fields: Value) -> Result<Value> {
    records::dispatch(
        &mut kernel.store,
        "latest_find",
        &json!({"stream":stream,"identity":identity,"fields":fields}),
    )
}
fn job(kernel: &mut Kernel, agent: &str, id: &Value) -> Result<Value> {
    records::dispatch(
        &mut kernel.store,
        "find_last",
        &json!({"stream":"jobs","agent_name":agent,"fields":{"job_id":id}}),
    )
}
fn parent_message(kernel: &mut Kernel, parent: &Value) -> Result<Value> {
    let attempt = records::dispatch(
        &mut kernel.store,
        "find_last",
        &json!({"stream":"attempts","fields":{"job_id":parent["job_id"]}}),
    )?;
    lookup(
        kernel,
        "messages",
        "message_id",
        json!({"message_id":attempt["message_id"]}),
    )
}
fn allowed(parent: &Value) -> Vec<String> {
    let mut seen = HashSet::new();
    parent["request"]["route_options"]["allowed_chain_targets"]
        .as_array()
        .into_iter()
        .flatten()
        .map(normalized)
        .filter(|s| !s.is_empty() && seen.insert(s.clone()))
        .collect()
}
pub fn validate(kernel: &mut Kernel, args: &Value) -> Result<Value> {
    let request = &args["request"];
    admission::validate_request(request)?;
    let actor = normalized(&request["from_actor"]);
    let names = args["agent_names"]
        .as_array()
        .ok_or_else(|| anyhow::anyhow!("agent_names must be an array"))?;
    let active = if names.contains(&json!(actor)) {
        kernel.dispatch(
            "scheduler.active_job_for",
            &json!({"state_id":args["state_id"],"target_kind":"agent","target_name":actor}),
        )?
    } else {
        Value::Null
    };
    let mut parent = if active.is_null() {
        Value::Null
    } else {
        job(kernel, &actor, &active)?
    };
    if ["completed", "cancelled", "failed", "incomplete"]
        .contains(&parent["status"].as_str().unwrap_or(""))
        || normalized(&parent["request"]["message_type"]) == "reply_delivery"
        || records::truthy(&parent["provider_options"]["reply_delivery"])
    {
        parent = Value::Null;
    }
    let allowed = allowed(&parent);
    if !is_chain(request) {
        ensure!(
            allowed.is_empty(),
            "restricted workflow job may only use ask --chain to its assigned target"
        );
        ensure!(normalized(&request["message_type"]) != "ask" || request["silence_on_success"] == true || parent.is_null(),
            "plain ask from an active CCB task requires --chain when the child result is needed, or --silence for independent fire-and-forget work");
        return Ok(Value::Null);
    }
    ensure!(
        args["bureau_enabled"] == true,
        "ask --chain requires message bureau support"
    );
    ensure!(
        request["delivery_scope"] == "single",
        "ask --chain supports exactly one target agent"
    );
    ensure!(
        !parent.is_null(),
        "ask --chain requires an active parent job for the sender"
    );
    let message = parent_message(kernel, &parent)?;
    ensure!(
        !message.is_null(),
        "ask --chain could not resolve parent message"
    );
    let target = normalized(&request["to_agent"]);
    let options = &parent["request"]["route_options"];
    if normalized(&parent["request"]["message_type"]) == "chain_continuation"
        || normalized(&options["mode"]) == "chain_continuation"
    {
        let edge_id = normalized(&options["chain_edge_id"]);
        ensure!(!edge_id.is_empty(), "ask --chain from result-chain continuation job {} cannot resolve chain edge; finish the current continuation directly or report the metadata error",parent["job_id"].as_str().unwrap_or(""));
        let edge = lookup(
            kernel,
            "callbacks",
            "edge_id",
            json!({"edge_id":options["chain_edge_id"]}),
        )?;
        ensure!(!edge.is_null(), "ask --chain from result-chain continuation job {} could not resolve chain edge {}; finish the current continuation directly or report the metadata error",parent["job_id"].as_str().unwrap_or(""),edge_id);
        ensure!(target != normalized(&edge["original_caller"]), "ask --chain from result-chain continuation job {} to original caller {} is not allowed for chain edge {}; finish the current response and CCB will deliver it upstream",parent["job_id"].as_str().unwrap_or(""),edge["original_caller"].as_str().unwrap_or(""),edge_id);
    }
    ensure!(
        allowed.is_empty() || allowed.contains(&target),
        "ask --chain target is not the assigned reviewer for this workflow job"
    );
    ensure!(
        lookup(
            kernel,
            "callbacks",
            "edge_id",
            json!({"parent_job_id":parent["job_id"]})
        )?
        .is_null(),
        "ask --chain allows one outstanding chain edge per parent job"
    );
    let mut id = message["message_id"].clone();
    let mut seen = HashSet::new();
    let mut actors = HashSet::from([normalized(&parent["agent_name"])]);
    let mut depth = 1;
    while let Some(value) = id.as_str().filter(|s| !s.is_empty()) {
        if !seen.insert(value.to_owned()) {
            break;
        }
        let edge = lookup(
            kernel,
            "callbacks",
            "edge_id",
            json!({"child_message_id":id}),
        )?;
        if edge.is_null() {
            break;
        }
        actors.insert(normalized(&edge["parent_agent"]));
        actors.insert(normalized(&edge["diagnostics"]["child_agent"]));
        depth += 1;
        id = edge["parent_message_id"].clone();
    }
    let max = args["max_callback_depth"].as_u64().unwrap_or(5).max(1);
    ensure!(depth <= max, "ask --chain exceeds max chain depth {max}");
    ensure!(!actors.contains(&target), "ask --chain cycle detected");
    Ok(parent)
}
fn body_summary(request: &Value) -> String {
    let body = request["body"].as_str().unwrap_or("");
    let cut = ["\n\nCCB_REPLY_MODE:", "\n\nCCB reply guidance:"]
        .into_iter()
        .filter_map(|m| body.find(m))
        .min();
    let body = cut.map_or(body, |i| body[..i].trim_end());
    let artifact = &request["body_artifact"];
    if !records::truthy(artifact) {
        return body.to_owned();
    }
    let body = body.trim();
    let preview = if body.chars().count() > 800 {
        format!(
            "{}\n...[truncated]",
            body.chars().take(800).collect::<String>().trim_end()
        )
    } else {
        body.to_owned()
    };
    let mut lines = vec![
        preview,
        String::new(),
        "Full original request artifact:".into(),
        format!("path: {}", artifact["path"].as_str().unwrap_or("").trim()),
    ];
    if records::truthy(&artifact["bytes"]) {
        let bytes = artifact["bytes"]
            .as_str()
            .map(str::to_owned)
            .unwrap_or_else(|| artifact["bytes"].to_string());
        lines.push(format!("bytes: {}", bytes.trim()));
    }
    if let Some(digest) = artifact["sha256"].as_str().filter(|s| !s.trim().is_empty()) {
        lines.push(format!("sha256: {}", digest.trim()));
    }
    lines.join("\n").trim_end().to_owned()
}
pub fn register(
    kernel: &mut Kernel,
    parent: &Value,
    request: &Value,
    child: &Value,
    child_message: &Value,
    timestamp: &str,
) -> Result<Value> {
    let message = parent_message(kernel, parent)?;
    ensure!(
        !message.is_null(),
        "ask --chain could not resolve parent message"
    );
    if records::truthy(&parent["request"]["route_options"]["bind_chain_workspace_tree"]) {
        ensure!(
            records::truthy(&request["route_options"]["bind_chain_workspace_tree"])
                && request["route_options"]["review_workspace_path"]
                    .as_str()
                    .is_some_and(|s| !s.trim().is_empty())
                && request["route_options"]["review_tree_digest"]
                    .as_str()
                    .is_some_and(|s| !s.trim().is_empty()),
            "restricted review chain requires a bound parent workspace tree"
        );
    }
    let edge = json!({"schema_version":1,"record_type":"callback_edge","edge_id":admission::new_id("cb"),
        "parent_job_id":parent["job_id"],"parent_message_id":message["message_id"],"parent_agent":parent["agent_name"],
        "child_job_id":child["job_id"],"child_message_id":child_message,"callback_target_agent":parent["agent_name"],
        "original_caller":parent["request"]["from_actor"],"original_task_id":if records::truthy(&parent["request"]["task_id"]) {&parent["request"]["task_id"]} else {&message["message_id"]},
        "state":"pending","child_reply_id":null,"child_status":null,"continuation_job_id":null,"continuation_message_id":null,
        "timeout_at":null,"created_at":timestamp,"updated_at":timestamp,"diagnostics":{
            "route_mode":"chain","child_agent":child["agent_name"],"parent_body":body_summary(&parent["request"]),
            "child_body":body_summary(&child["request"]),"parent_body_artifact":parent["request"]["body_artifact"],
            "child_body_artifact":child["request"]["body_artifact"],"allowed_chain_targets":allowed(parent),
            "bind_chain_workspace_tree":records::truthy(&request["route_options"]["bind_chain_workspace_tree"]),
            "review_workspace_path":request["route_options"]["review_workspace_path"],"review_tree_digest":request["route_options"]["review_tree_digest"]}});
    admission::append(kernel, "callbacks", &edge)?;
    admission::event(
        kernel,
        child,
        "callback_edge_created",
        json!({"edge_id":edge["edge_id"],"parent_job_id":edge["parent_job_id"],
        "parent_message_id":edge["parent_message_id"],"callback_target_agent":edge["callback_target_agent"],"original_caller":edge["original_caller"]}),
        timestamp,
    )?;
    Ok(edge)
}
