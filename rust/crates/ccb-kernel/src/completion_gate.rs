//! Shared provider authority and delivery completion policies.
use crate::records::{string, truthy};
use anyhow::{ensure, Result};
use serde_json::{json, Value};

fn normalized(value: &Value) -> String {
    value.as_str().unwrap_or("").trim().to_ascii_lowercase()
}
fn incomplete(decision: &mut Value, reason: &str, gate: &str, extra: Value) {
    let status = decision["status"].clone();
    let original_reason = decision["reason"].as_str().unwrap_or("").to_owned();
    let diagnostics = decision["diagnostics"].as_object_mut().unwrap();
    for (key, value) in extra.as_object().unwrap() {
        diagnostics.insert(key.clone(), value.clone());
    }
    diagnostics.insert("completion_gate".into(), json!(gate));
    diagnostics.insert("original_status".into(), status);
    diagnostics.insert("original_reason".into(), json!(original_reason));
    diagnostics.insert("suppress_completion_state_merge".into(), json!(true));
    decision["status"] = json!("incomplete");
    decision["reason"] = json!(reason);
    decision["confidence"] = json!("degraded");
    decision["reply"] = json!("");
    decision["anchor_seen"] = json!(false);
    decision["reply_started"] = json!(false);
    decision["reply_stable"] = json!(false);
}
pub fn validate(args: &Value) -> Result<Value> {
    let mut decision = args["decision"].clone();
    if decision.is_null() {
        return Ok(Value::Null);
    }
    ensure!(
        decision["schema_version"] == 2 && decision["record_type"] == "completion_decision",
        "invalid completion decision schema"
    );
    ensure!(
        ["completed", "cancelled", "failed", "incomplete"].contains(&string(&decision, "status")?),
        "invalid completion status"
    );
    if decision["diagnostics"].is_null() {
        decision["diagnostics"] = json!({});
    }
    ensure!(
        decision["diagnostics"].is_object(),
        "invalid completion diagnostics"
    );
    let submission = &args["submission"];
    let runtime = &submission["runtime_state"];
    let source = if !decision["source_cursor"]["source_kind"].is_null() {
        decision["source_cursor"]["source_kind"].clone()
    } else {
        submission["source_kind"].clone()
    };
    let source = source.as_str().unwrap_or("").trim();
    let diagnostics = decision["diagnostics"].as_object_mut().unwrap();
    if [
        "protocol_event_stream",
        "structured_result_stream",
        "session_event_log",
        "session_snapshot",
        "terminal_text",
    ]
    .contains(&source)
    {
        diagnostics
            .entry("completion_source_kind")
            .or_insert(json!(source));
        diagnostics
            .entry("completion_source")
            .or_insert(json!(match source {
                "protocol_event_stream" => "provider_event_stream",
                "terminal_text" => "terminal_capture",
                _ => "provider_native_log",
            }));
    }
    diagnostics
        .entry("completion_authority")
        .or_insert(json!("provider_execution"));
    if diagnostics.contains_key("herdr_agent_state")
        || diagnostics.contains_key("herdr_agent_state_ref")
    {
        diagnostics
            .entry("herdr_agent_state_role")
            .or_insert(json!("diagnostics_only"));
    }
    let declared = ["completion_source", "completion_source_kind"]
        .iter()
        .any(|key| {
            ["herdr_agent_state", "herdr_agent_state_ref"].contains(
                &normalized(&decision["diagnostics"][key])
                    .replace('-', "_")
                    .as_str(),
            )
        });
    if decision["status"] == "completed" && declared {
        incomplete(
            &mut decision,
            "herdr_agent_state_not_completion_authority",
            "provider_completion_authority",
            json!({}),
        );
    }
    if !truthy(&decision["terminal"]) {
        return Ok(decision);
    }
    let job = &args["job"];
    let delivery = if !job.is_null() {
        normalized(&job["request"]["message_type"]) == "reply_delivery"
            || truthy(&job["provider_options"]["reply_delivery"])
    } else {
        truthy(&runtime["reply_delivery_complete_on_dispatch"])
    };
    if delivery && decision["status"] == "incomplete" {
        let reason = normalized(&decision["reason"]);
        let error_type = normalized(&decision["diagnostics"]["error_type"]);
        if ["task_complete_empty_reply", "hook_stop_empty_reply"].contains(&reason.as_str())
            || reason.ends_with("_empty_reply")
            || error_type == "empty_provider_reply"
        {
            let old_reason = decision["reason"].as_str().unwrap_or("").to_owned();
            let diagnostics = decision["diagnostics"].as_object_mut().unwrap();
            diagnostics.insert("reply_delivery_turn_end".into(), json!(true));
            diagnostics.insert("original_status".into(), json!("incomplete"));
            diagnostics.insert("original_reason".into(), json!(old_reason));
            diagnostics.insert("empty_reply".into(), json!(true));
            decision["status"] = json!("completed");
            decision["reason"] = json!("reply_delivery_turn_complete");
            decision["reply"] = json!("");
        }
    }
    if decision["status"] != "completed"
        || normalized(&submission["provider"]) != "codex"
        || normalized(&runtime["mode"]) != "active"
    {
        return Ok(decision);
    }
    if !delivery && decision["reply"].as_str().unwrap_or("").trim().is_empty() {
        incomplete(
            &mut decision,
            "task_complete_empty_reply",
            "non_empty_reply",
            json!({"empty_reply":true,"error_type":"empty_provider_reply"}),
        );
        return Ok(decision);
    }
    if truthy(&runtime["no_wrap"]) {
        return Ok(decision);
    }
    let delivery_state = normalized(&runtime["delivery_state"]);
    let anchor = truthy(&runtime["anchor_seen"]) || truthy(&decision["anchor_seen"]);
    if delivery_state == "accepted" && anchor {
        return Ok(decision);
    }
    incomplete(
        &mut decision,
        if delivery_state == "accepted" {
            "terminal_after_session_rotate_without_anchor"
        } else {
            "terminal_before_provider_acceptance"
        },
        "provider_acceptance",
        json!({"delivery_state":delivery_state,"anchor_seen":anchor}),
    );
    Ok(decision)
}
