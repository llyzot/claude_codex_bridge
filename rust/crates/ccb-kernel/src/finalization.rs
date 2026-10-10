//! Completion reconciliation and authoritative terminal persistence.
use crate::{admission, artifacts, bureau, completion, records, Kernel};
use anyhow::{ensure, Result};
use serde_json::{json, Value};

fn first(values: &[&Value]) -> Value {
    values
        .iter()
        .find(|v| records::truthy(v))
        .map(|v| (*v).clone())
        .unwrap_or(Value::Null)
}
fn is_terminal(job: &Value) -> bool {
    ["completed", "cancelled", "failed", "incomplete"]
        .contains(&job["status"].as_str().unwrap_or(""))
}
fn same_request(a: &Value, b: &Value) -> bool {
    [
        "project_id",
        "to_agent",
        "from_actor",
        "body",
        "task_id",
        "reply_to",
        "message_type",
        "delivery_scope",
    ]
    .iter()
    .all(|key| a[*key] == b[*key])
        && records::truthy(&a["silence_on_success"]) == records::truthy(&b["silence_on_success"])
        && (a["route_options"] == b["route_options"]
            || !records::truthy(&a["route_options"]) && !records::truthy(&b["route_options"]))
        && (a["body_artifact"] == b["body_artifact"]
            || !records::truthy(&a["body_artifact"]) && !records::truthy(&b["body_artifact"]))
}
pub fn merge(decision: &Value, tracked: &Value, prior: &Value) -> Result<Value> {
    ensure!(
        decision["schema_version"] == 2 && decision["record_type"] == "completion_decision",
        "invalid completion decision schema"
    );
    let mut result = decision.clone();
    let diagnostics = result["diagnostics"]
        .as_object_mut()
        .ok_or_else(|| anyhow::anyhow!("diagnostics must be an object"))?;
    let suppress = diagnostics
        .remove("suppress_completion_state_merge")
        .is_some_and(|v| records::truthy(&v));
    let empty = Value::Null;
    let tracked = if suppress { &empty } else { tracked };
    let prior = if suppress { &empty } else { prior };
    result["reply"] = first(&[
        &decision["reply"],
        &tracked["decision"]["reply"],
        &prior["latest_decision"]["reply"],
    ]);
    if result["reply"].is_null() {
        result["reply"] = json!("");
    }
    for flag in ["anchor_seen", "reply_started", "reply_stable"] {
        result[flag] = json!(
            records::truthy(&decision[flag])
                || records::truthy(&tracked["state"][flag])
                || records::truthy(&prior["state"][flag])
        );
    }
    result["provider_turn_ref"] = first(&[
        &decision["provider_turn_ref"],
        &tracked["state"]["provider_turn_ref"],
        &prior["state"]["provider_turn_ref"],
    ]);
    result["source_cursor"] = first(&[
        &decision["source_cursor"],
        &tracked["state"]["latest_cursor"],
        &prior["state"]["latest_cursor"],
    ]);
    Ok(result)
}
fn terminal_state(decision: &Value, prior: &Value) -> Value {
    let mut state = json!({"schema_version":2,"record_type":"completion_state","tool_active":false,"terminal":true,
        "subagent_activity_seen":records::truthy(&prior["subagent_activity_seen"]),"last_reply_hash":prior["last_reply_hash"],
        "last_reply_at":first(&[&decision["finished_at"],&prior["last_reply_at"]]),"stable_since":prior["stable_since"],
        "provider_turn_ref":first(&[&decision["provider_turn_ref"],&prior["provider_turn_ref"]]),
        "latest_cursor":first(&[&decision["source_cursor"],&prior["latest_cursor"]])});
    for flag in ["anchor_seen", "reply_started", "reply_stable"] {
        state[flag] = json!(records::truthy(&decision[flag]) || records::truthy(&prior[flag]));
    }
    state
}
fn snapshot(
    kernel: &mut Kernel,
    job: &Value,
    decision: &Value,
    prior: &Value,
    args: &Value,
) -> Result<Value> {
    let reply = decision["reply"].as_str().unwrap_or("").trim();
    let count = reply.chars().count();
    let limit = args["preview_limit"].as_i64().unwrap_or(240);
    let preview = if (count as i64) <= limit {
        reply.to_owned()
    } else {
        let bound = if limit < 0 {
            count.saturating_sub(limit.unsigned_abs() as usize)
        } else {
            limit as usize
        };
        format!(
            "{}...",
            reply.chars().take(bound).collect::<String>().trim_end()
        )
    };
    let record = json!({"schema_version":2,"record_type":"completion_snapshot","job_id":job["job_id"],"agent_name":job["agent_name"],
        "profile_family":args["profile_family"],"state":terminal_state(decision,&prior["state"]),"latest_decision":decision,
        "latest_reply_preview":preview,"updated_at":args["finished_at"]});
    completion::snapshot(
        kernel,
        "save",
        &json!({"job_id":job["job_id"],"record":record}),
    )?;
    Ok(record)
}
pub fn persist(kernel: &mut Kernel, args: &Value, revise: bool) -> Result<Value> {
    let expected = &args["current"];
    let id = records::string(expected, "job_id")?;
    let agent = records::string(expected, "agent_name")?;
    let current = records::dispatch(
        &mut kernel.store,
        "find_last",
        &json!({"stream":"jobs","agent_name":agent,"fields":{"job_id":id}}),
    )?;
    ensure!(!current.is_null(), "unknown job: {id}");
    ensure!(
        current["provider"] == expected["provider"]
            && same_request(&current["request"], &expected["request"]),
        "terminal job identity changed"
    );
    let prior = completion::snapshot(kernel, "load", &json!({"job_id":id}))?;
    if !revise && is_terminal(&current) {
        return Ok(
            json!({"job":current,"decision":current["terminal_decision"],"prior":prior,"created":false}),
        );
    }
    ensure!(
        revise
            || current["updated_at"] == expected["updated_at"]
                && current["status"] == expected["status"],
        "terminal job changed during finalization"
    );
    if revise {
        ensure!(
            is_terminal(&current),
            "reply revision requires a terminal job"
        );
    }
    ensure!(
        args["decision"]["terminal"] == true,
        "complete requires a terminal completion decision"
    );
    let timestamp = records::string(args, "finished_at")?;
    let tracked = if !args["tracker_group"].is_null() {
        kernel.completion.dispatch(
            "current",
            &json!({"group_id":args["tracker_group"],"job_id":id}),
        )?
    } else {
        args["tracker_view"].clone()
    };
    ensure!(
        tracked.is_null() || tracked["job_id"] == id && tracked["agent_name"] == agent,
        "terminal tracker identity mismatch"
    );
    let decision = if revise {
        args["decision"].clone()
    } else {
        merge(&args["decision"], &tracked, &prior)?
    };
    let decision = artifacts::spill_completion(kernel, &current, &decision, timestamp)?;
    let state_prior = if revise {
        &args["prior_snapshot"]
    } else {
        &prior
    };
    snapshot(kernel, &current, &decision, state_prior, args)?;
    let mut terminal = current.clone();
    terminal["terminal_decision"] = decision.clone();
    if !revise {
        terminal["status"] = decision["status"].clone();
        terminal["updated_at"] = json!(timestamp);
        admission::event(
            kernel,
            &current,
            "completion_terminal",
            decision.clone(),
            timestamp,
        )?;
    }
    admission::append(kernel, "jobs", &terminal)?;
    if !revise {
        let status = records::string(&terminal, "status")?;
        let kind = match status {
            "completed" => "job_completed",
            "cancelled" => "job_cancelled",
            "failed" => "job_failed",
            _ => "job_incomplete",
        };
        admission::event(kernel, &terminal, kind, json!({"status":status}), timestamp)?;
        if args["bureau_enabled"] == true {
            bureau::attempt_transition(
                kernel,
                "terminal",
                &json!({"job":terminal,"timestamp":timestamp}),
            )?;
            let edge = records::dispatch(
                &mut kernel.store,
                "latest_find",
                &json!({"stream":"callbacks","identity":"edge_id","fields":{"parent_job_id":id}}),
            )?;
            let delivery = current["request"]["message_type"] == "reply_delivery"
                || records::truthy(&current["provider_options"]["reply_delivery"]);
            if !delivery
                && !edge.is_null()
                && !["failed", "timed_out", "done"].contains(&edge["state"].as_str().unwrap_or(""))
            {
                terminal["terminal_decision"]["delegated"] = json!(true);
                terminal["terminal_decision"]["suppress_reply"] = json!(true);
                terminal["terminal_decision"]["chain_edge_id"] = edge["edge_id"].clone();
                terminal["terminal_decision"]["chain_child_job_id"] = edge["child_job_id"].clone();
                admission::append(kernel, "jobs", &terminal)?;
                admission::event(
                    kernel,
                    &terminal,
                    "job_delegated_chain",
                    json!({"edge_id":edge["edge_id"],"chain_child_job_id":edge["child_job_id"],"chain_target_agent":edge["callback_target_agent"]}),
                    timestamp,
                )?;
                records::message_transition(
                    &mut kernel.store,
                    "set",
                    &json!({"message_id":edge["parent_message_id"],"state":"running","updated_at":timestamp}),
                )?;
            }
        }
        // Publication/retirement follows all fallible persistent writes.
        kernel.dispatch("scheduler.clear_active_for",&json!({"state_id":args["state_id"],"target_kind":"agent","target_name":agent,"job_id":id}))?;
        if !args["tracker_group"].is_null() {
            kernel.completion.dispatch(
                "finish",
                &json!({"group_id":args["tracker_group"],"job_id":id}),
            )?;
        }
    }
    Ok(json!({"job":terminal,"decision":decision,"prior":prior,"created":true}))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn terminal_snapshot_artifact_and_attempt_recover_at_every_write_boundary() {
        let mut boundaries = 0;
        for boundary in 0..=32 {
            let directory = tempfile::tempdir().unwrap();
            let root = directory.path().join("ccbd");
            let mut kernel = Kernel::open(&root).unwrap();
            let timestamp = "2026-10-11T10:00:00Z";
            kernel
                .dispatch(
                    "scheduler.create",
                    &json!({"state_id":"state","agent_names":["worker"]}),
                )
                .unwrap();
            let job = json!({"schema_version":2,"record_type":"job_record","job_id":"job-terminal","submission_id":null,
                "agent_name":"worker","provider":"pi","target_kind":"agent","target_name":"worker","provider_instance":null,
                "provider_options":{},"workspace_path":null,"status":"running","terminal_decision":null,"cancel_requested_at":null,
                "created_at":timestamp,"updated_at":timestamp,"request":{"project_id":"test","to_agent":"worker","from_actor":"user",
                    "body":"task","task_id":null,"reply_to":null,"message_type":"ask","delivery_scope":"single","silence_on_success":false,
                    "route_options":{},"body_artifact":null}});
            admission::append(&mut kernel, "jobs", &job).unwrap();
            bureau::submission(
                &mut kernel,
                &json!({"request":job["request"],"jobs":[job],"accepted_at":timestamp}),
            )
            .unwrap();
            bureau::attempt_transition(
                &mut kernel,
                "started",
                &json!({"job":job,"timestamp":timestamp}),
            )
            .unwrap();
            kernel.store.commit().unwrap();
            kernel
                .dispatch(
                    "scheduler.record",
                    &json!({"state_id":"state","record":job}),
                )
                .unwrap();
            kernel.dispatch("scheduler.mark_active_for",&json!({"state_id":"state","target_kind":"agent","target_name":"worker","job_id":"job-terminal"})).unwrap();
            let text = "汉".repeat(1800);
            let decision = json!({"schema_version":2,"record_type":"completion_decision","terminal":true,"status":"completed","reason":"task_complete",
                "confidence":"observed","reply":text,"anchor_seen":true,"reply_started":true,"reply_stable":true,"provider_turn_ref":null,
                "source_cursor":null,"finished_at":timestamp,"diagnostics":{}});
            let result=persist(&mut kernel,&json!({"current":job,"decision":decision,"finished_at":timestamp,"state_id":"state", "bureau_enabled":true,
                "profile_family":"protocol_turn","tracker_group":null,"tracker_view":null}),false).unwrap();
            let count = kernel.store.interrupt_for_test(boundary).unwrap();
            assert!(count < 32);
            drop(kernel);
            let mut restored = Kernel::open(&root).unwrap();
            let latest = records::dispatch(
                &mut restored.store,
                "find_last",
                &json!({"stream":"jobs","agent_name":"worker","fields":{"job_id":"job-terminal"}}),
            )
            .unwrap();
            assert_eq!(latest, result["job"]);
            let snapshot =
                completion::snapshot(&mut restored, "load", &json!({"job_id":"job-terminal"}))
                    .unwrap();
            assert_eq!(snapshot["latest_decision"], result["decision"]);
            let artifact = &snapshot["latest_decision"]["diagnostics"]["reply_artifact"];
            assert_eq!(
                std::fs::read_to_string(artifact["path"].as_str().unwrap()).unwrap(),
                text
            );
            let attempts =
                records::dispatch(&mut restored.store, "list", &json!({"stream":"attempts"}))
                    .unwrap();
            assert_eq!(attempts.as_array().unwrap().len(), 3);
            assert_eq!(attempts[2]["attempt_state"], "completed");
            let inbox=restored.dispatch("inbox.latest",&json!({"agent_name":"worker","field":"attempt_id","value":attempts[2]["attempt_id"]})).unwrap();
            assert_eq!(inbox["status"], "consumed");
            drop(restored);
            let mut twice = Kernel::open(&root).unwrap();
            assert_eq!(
                records::dispatch(&mut twice.store, "list", &json!({"stream":"attempts"})).unwrap(),
                attempts
            );
            boundaries += 1;
            if boundary == count {
                break;
            }
        }
        assert!(boundaries > 10);
    }
}
