//! Recovery walks durable lineage, never terminal activity or elapsed time.
use crate::{callback, records, Kernel};
use anyhow::{ensure, Result};
use serde_json::{json, Value};

fn query(kernel: &mut Kernel, stream: &str, fields: Value) -> Result<Value> {
    records::dispatch(
        &mut kernel.store,
        "find_last",
        &json!({"stream":stream,"fields":fields}),
    )
}
fn job(kernel: &mut Kernel, args: &Value, id: &Value) -> Result<Value> {
    if id.is_null() {
        return Ok(Value::Null);
    }
    let target = kernel.dispatch(
        "scheduler.target_for_job",
        &json!({"state_id":args["state_id"],"job_id":id}),
    )?;
    if let Some(agent) = target[1].as_str() {
        return records::dispatch(
            &mut kernel.store,
            "find_last",
            &json!({"stream":"jobs","agent_name":agent,"fields":{"job_id":id}}),
        );
    }
    for agent in args["agent_names"]
        .as_array()
        .ok_or_else(|| anyhow::anyhow!("agent_names must be an array"))?
    {
        let found = records::dispatch(
            &mut kernel.store,
            "find_last",
            &json!({"stream":"jobs","agent_name":agent,"fields":{"job_id":id}}),
        )?;
        if !found.is_null() {
            return Ok(found);
        }
    }
    Ok(Value::Null)
}
fn outstanding(kernel: &mut Kernel, args: &Value, id: &Value, queued_only: bool) -> Result<bool> {
    let target = kernel.dispatch(
        "scheduler.target_for_job",
        &json!({"state_id":args["state_id"],"job_id":id}),
    )?;
    if target.is_null() {
        return Ok(false);
    }
    let slot = json!({"state_id":args["state_id"],"target_kind":target[0],"target_name":target[1]});
    if !queued_only && kernel.dispatch("scheduler.active_job_for", &slot)? == *id {
        return Ok(true);
    }
    Ok(kernel
        .dispatch("scheduler.queued_items_for", &slot)?
        .as_array()
        .is_some_and(|q| q.contains(id)))
}
fn decision(reply: &Value, child: &Value, fallback: &Value) -> Value {
    let terminal = &child["terminal_decision"];
    let mut diagnostics = terminal["diagnostics"]
        .as_object()
        .cloned()
        .unwrap_or_default();
    if !diagnostics.contains_key("reply_artifact") && records::truthy(&reply["reply_artifact"]) {
        diagnostics.insert("reply_artifact".into(), reply["reply_artifact"].clone());
    }
    json!({"schema_version":2,"record_type":"completion_decision","terminal":true,
        "status":terminal["status"].as_str().unwrap_or(child["status"].as_str().unwrap_or("incomplete")),
        "reason":terminal["reason"].as_str().filter(|s|!s.is_empty()).unwrap_or(child["status"].as_str().unwrap_or("incomplete")),
        "confidence":terminal["confidence"].as_str().unwrap_or("degraded"),"reply":reply["reply"].as_str().unwrap_or(""),
        "anchor_seen":records::truthy(&terminal["anchor_seen"]),"reply_started":records::truthy(&terminal["reply_started"]),
        "reply_stable":records::truthy(&terminal["reply_stable"]),"provider_turn_ref":terminal["provider_turn_ref"],"source_cursor":null,
        "finished_at":if records::truthy(&terminal["finished_at"]) {&terminal["finished_at"]} else if records::truthy(&reply["finished_at"]) {&reply["finished_at"]} else {fallback},
        "diagnostics":diagnostics})
}
fn reply_child(kernel: &mut Kernel, args: &Value, edge: &Value, reply: &Value) -> Result<Value> {
    if reply.is_null() {
        return Ok(Value::Null);
    }
    let attempt = query(
        kernel,
        "attempts",
        json!({"attempt_id":reply["attempt_id"]}),
    )?;
    ensure!(
        !attempt.is_null()
            && attempt["message_id"] == edge["child_message_id"]
            && reply["message_id"] == edge["child_message_id"],
        "callback reply lineage mismatch"
    );
    let child = job(kernel, args, &attempt["job_id"])?;
    ensure!(
        child.is_null()
            || child["agent_name"] == attempt["agent_name"]
                && child["provider"] == attempt["provider"]
                && reply["agent_name"] == child["agent_name"],
        "callback reply provider identity mismatch"
    );
    Ok(child)
}
// The terminal transaction may commit immediately before its caller dies.
// Repair that gap only after every logical-child attempt is terminal; a queued
// or running retry must retain ownership of the eventual result.
fn recover_missing_result(kernel: &mut Kernel, args: &Value, edge: &Value) -> Result<Value> {
    let attempts = records::dispatch(
        &mut kernel.store,
        "latest_filter",
        &json!({"stream":"attempts","identity":"attempt_id","fields":{"message_id":edge["child_message_id"]}}),
    )?;
    let attempts = attempts.as_array().unwrap();
    if attempts.is_empty()
        || attempts.iter().any(|a| {
            ![
                "completed",
                "cancelled",
                "failed",
                "incomplete",
                "superseded",
                "dead_letter",
            ]
            .contains(&a["attempt_state"].as_str().unwrap_or(""))
        })
    {
        return Ok(Value::Null);
    }
    let attempt = attempts
        .iter()
        .max_by_key(|a| a["retry_index"].as_u64().unwrap_or(0))
        .unwrap();
    let child = job(kernel, args, &attempt["job_id"])?;
    let terminal = &child["terminal_decision"];
    if child.is_null()
        || !["completed", "cancelled", "failed", "incomplete"]
            .contains(&child["status"].as_str().unwrap_or(""))
        || !terminal.is_object()
        || records::truthy(&terminal["delegated"])
        || records::truthy(&terminal["suppress_reply"])
        || terminal["diagnostics"]["result_reply_enabled"] == false
    {
        return Ok(Value::Null);
    }
    ensure!(
        child["agent_name"] == attempt["agent_name"] && child["provider"] == attempt["provider"],
        "callback child provider identity mismatch"
    );
    let timestamp = terminal["finished_at"]
        .as_str()
        .filter(|s| !s.is_empty())
        .unwrap_or(edge["updated_at"].as_str().unwrap_or(""));
    // The transitional result-policy adapter still decides retry/notice
    // behavior. Resuming it is essential: fabricating a failed reply here
    // would bypass automatic retry after a crash before retry scheduling.
    let reply = json!({"reply":terminal["reply"],"reply_artifact":terminal["diagnostics"]["reply_artifact"],"finished_at":timestamp});
    Ok(
        json!({"kind":"finalize_result","edge":edge,"child_job":child,"decision":decision(&reply,&child,&json!(timestamp)),"finished_at":timestamp}),
    )
}
fn authoritative_reply(kernel: &mut Kernel, edge: &Value) -> Result<(bool, Value)> {
    let attempts = records::dispatch(
        &mut kernel.store,
        "latest_filter",
        &json!({"stream":"attempts","identity":"attempt_id","fields":{"message_id":edge["child_message_id"]}}),
    )?;
    let attempts = attempts.as_array().unwrap();
    if attempts.iter().any(|a| {
        ![
            "completed",
            "cancelled",
            "failed",
            "incomplete",
            "superseded",
            "dead_letter",
        ]
        .contains(&a["attempt_state"].as_str().unwrap_or(""))
    }) {
        return Ok((true, Value::Null));
    }
    let Some(attempt) = attempts
        .iter()
        .max_by_key(|a| a["retry_index"].as_u64().unwrap_or(0))
    else {
        return Ok((false, callback::latest_reply(kernel, edge)?));
    };
    let explicit = callback::latest_reply(kernel, edge)?;
    if !explicit.is_null() && explicit["attempt_id"] == attempt["attempt_id"] {
        return Ok((false, explicit));
    }
    let rows = records::dispatch(&mut kernel.store, "list", &json!({"stream":"replies"}))?;
    let reply = rows
        .as_array()
        .unwrap()
        .iter()
        .rev()
        .find(|reply| {
            reply["attempt_id"] == attempt["attempt_id"]
                && reply["message_id"] == edge["child_message_id"]
                && (!records::truthy(&reply["diagnostics"]["notice"])
                    || reply["diagnostics"]["notice_kind"] == "cancelled"
                        && reply["terminal_status"] == "cancelled")
        })
        .cloned()
        .unwrap_or(Value::Null);
    Ok((false, reply))
}
pub fn repair(kernel: &mut Kernel, args: &Value) -> Result<Value> {
    kernel.dispatch("scheduler.slots", &json!({"state_id":args["state_id"]}))?;
    let edges = records::dispatch(
        &mut kernel.store,
        "latest",
        &json!({"stream":"callbacks","identity":"edge_id"}),
    )?;
    let mut repaired = Vec::new();
    let mut actions = Vec::new();
    for edge in edges.as_array().unwrap() {
        let state = edge["state"].as_str().unwrap_or("");
        if ["done", "failed", "timed_out"].contains(&state) {
            continue;
        }
        if state == "pending" && outstanding(kernel, args, &edge["child_job_id"], false)? {
            continue;
        }
        let existing = if records::truthy(&edge["continuation_job_id"]) {
            if state != "continuation_submitted"
                || !outstanding(kernel, args, &edge["continuation_job_id"], true)?
            {
                continue;
            }
            job(kernel, args, &edge["continuation_job_id"])?
        } else {
            records::dispatch(
                &mut kernel.store,
                "find_last",
                &json!({"stream":"jobs","agent_name":edge["callback_target_agent"],"fields":{"request.route_options.chain_edge_id":edge["edge_id"]}}),
            )?
        };
        if state == "continuation_submitted" {
            if !existing.is_null() && callback::repair_attempt(kernel, edge, &existing)? {
                repaired.push(edge.clone());
            }
            continue;
        }
        if !["pending", "child_completed"].contains(&state) {
            continue;
        }
        if !existing.is_null() {
            let reply = callback::latest_reply(kernel, edge)?;
            callback::repair_attempt(kernel, edge, &existing)?;
            let child = reply_child(kernel, args, edge, &reply)?;
            let updated = callback::update(
                kernel,
                edge,
                json!({"state":"continuation_submitted",
                "child_reply_id":if records::truthy(&edge["child_reply_id"]) {&edge["child_reply_id"]} else {&reply["reply_id"]},
                "child_status":if records::truthy(&edge["child_status"]) {&edge["child_status"]} else {&child["status"]},
                "continuation_job_id":existing["job_id"],"continuation_message_id":edge["parent_message_id"],"updated_at":edge["updated_at"]}),
            )?;
            repaired.push(updated);
            continue;
        }
        let (busy, reply) = authoritative_reply(kernel, edge)?;
        if busy {
            continue;
        }
        if reply.is_null() {
            let action = recover_missing_result(kernel, args, edge)?;
            if !action.is_null() {
                actions.push(action);
            }
            continue;
        }
        let child = reply_child(kernel, args, edge, &reply)?;
        if reply.is_null() || child.is_null() || !child["terminal_decision"].is_object() {
            continue;
        }
        let decision = decision(&reply, &child, &edge["updated_at"]);
        let finished = if records::truthy(&edge["updated_at"]) {
            &edge["updated_at"]
        } else {
            &reply["finished_at"]
        };
        let prepared = callback::prepare(
            kernel,
            &json!({"edge":edge,"child_job":child,"child_reply_id":reply["reply_id"],"finished_at":finished}),
        )?;
        if prepared["action"] == "submit" {
            actions.push(json!({"kind":"continue","edge":prepared["edge"],"child_job":child,"child_reply_id":reply["reply_id"],"decision":decision,"finished_at":finished}));
        } else {
            repaired.push(prepared["edge"].clone());
        }
    }
    Ok(json!({"repaired":repaired,"actions":actions}))
}
