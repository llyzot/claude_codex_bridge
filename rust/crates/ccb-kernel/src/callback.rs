//! Durable callback planning and atomic continuation admission.
use crate::{admission, artifacts, bureau, records, Kernel};
use anyhow::{ensure, Result};
use serde_json::{json, Value};

pub(crate) fn lookup(kernel: &mut Kernel, fields: Value) -> Result<Value> {
    records::dispatch(
        &mut kernel.store,
        "latest_find",
        &json!({"stream":"callbacks","identity":"edge_id","fields":fields}),
    )
}
pub(crate) fn update(kernel: &mut Kernel, record: &Value, changes: Value) -> Result<Value> {
    records::callback_update(
        &mut kernel.store,
        &json!({"record":record,"changes":changes}),
    )
}
fn terminal(edge: &Value) -> bool {
    ["continuation_submitted", "done", "failed", "timed_out"]
        .contains(&edge["state"].as_str().unwrap_or(""))
}
pub(crate) fn latest_reply(kernel: &mut Kernel, edge: &Value) -> Result<Value> {
    if records::truthy(&edge["child_reply_id"]) {
        let reply = records::dispatch(
            &mut kernel.store,
            "find_last",
            &json!({"stream":"replies","fields":{"reply_id":edge["child_reply_id"]}}),
        )?;
        if !reply.is_null() {
            return Ok(reply);
        }
    }
    let replies = records::dispatch(&mut kernel.store, "list", &json!({"stream":"replies"}))?;
    Ok(replies
        .as_array()
        .unwrap()
        .iter()
        .rev()
        .find(|r| {
            r["message_id"] == edge["child_message_id"]
                && !records::truthy(&r["diagnostics"]["notice"])
        })
        .cloned()
        .unwrap_or(Value::Null))
}
pub(crate) fn repair_attempt(kernel: &mut Kernel, edge: &Value, job: &Value) -> Result<bool> {
    if !["accepted", "queued"].contains(&job["status"].as_str().unwrap_or("")) {
        return Ok(false);
    }
    let attempt = records::dispatch(
        &mut kernel.store,
        "find_last",
        &json!({"stream":"attempts","fields":{"job_id":job["job_id"]}}),
    )?;
    let inbound = if attempt.is_null() {
        Value::Null
    } else {
        kernel.dispatch("inbox.latest",&json!({"agent_name":job["agent_name"],"field":"attempt_id","value":attempt["attempt_id"]}))?
    };
    if attempt.is_null() || inbound.is_null() {
        let timestamp = job["created_at"]
            .as_str()
            .filter(|s| !s.is_empty())
            .unwrap_or(edge["updated_at"].as_str().unwrap_or(""));
        bureau::retry(
            kernel,
            &json!({"message_id":edge["parent_message_id"],"job":job,"accepted_at":timestamp}),
        )?;
        return Ok(true);
    }
    Ok(false)
}
pub fn prepare(kernel: &mut Kernel, args: &Value) -> Result<Value> {
    let mut edge = lookup(kernel, json!({"child_job_id":args["child_job"]["job_id"]}))?;
    if edge.is_null() {
        edge = args["edge"].clone();
    }
    records::validate("callbacks", &edge, None)?;
    let attempt = records::dispatch(
        &mut kernel.store,
        "find_last",
        &json!({"stream":"attempts","fields":{"job_id":args["child_job"]["job_id"]}}),
    )?;
    // A retry has a new job id but retains its logical child message. Accept
    // its result only through the persisted attempt lineage, never by actor.
    let retry_child = !attempt.is_null()
        && attempt["message_id"] == edge["child_message_id"]
        && attempt["agent_name"] == args["child_job"]["agent_name"]
        && attempt["provider"] == args["child_job"]["provider"];
    ensure!(
        edge["child_job_id"] == args["child_job"]["job_id"] || retry_child,
        "callback child identity mismatch"
    );
    ensure!(
        ["completed", "cancelled", "failed", "incomplete"]
            .contains(&args["child_job"]["status"].as_str().unwrap_or("")),
        "callback child is not terminal"
    );
    if terminal(&edge) {
        return Ok(json!({"action":"terminal","edge":edge}));
    }
    let existing = records::dispatch(
        &mut kernel.store,
        "find_last",
        &json!({"stream":"jobs","agent_name":edge["callback_target_agent"],"fields":{"request.route_options.chain_edge_id":edge["edge_id"]}}),
    )?;
    if !existing.is_null() {
        repair_attempt(kernel, &edge, &existing)?;
        let reply = latest_reply(kernel, &edge)?;
        let child_reply = if records::truthy(&args["child_reply_id"]) {
            &args["child_reply_id"]
        } else if records::truthy(&edge["child_reply_id"]) {
            &edge["child_reply_id"]
        } else {
            &reply["reply_id"]
        };
        let state = if ["completed", "cancelled", "failed", "incomplete"]
            .contains(&existing["status"].as_str().unwrap_or(""))
        {
            "done"
        } else {
            "continuation_submitted"
        };
        let updated = update(
            kernel,
            &edge,
            json!({"state":state,"child_reply_id":child_reply,"child_status":args["child_job"]["status"],"continuation_job_id":existing["job_id"],"continuation_message_id":edge["parent_message_id"],"updated_at":args["finished_at"]}),
        )?;
        return Ok(json!({"action":"existing","edge":updated}));
    }
    if edge["state"] == "child_completed"
        && edge["child_reply_id"] == args["child_reply_id"]
        && edge["child_status"] == args["child_job"]["status"]
        && edge["updated_at"] == args["finished_at"]
    {
        return Ok(json!({"action":"submit","edge":edge}));
    }
    let updated = update(
        kernel,
        &edge,
        json!({"state":"child_completed","child_reply_id":args["child_reply_id"],"child_status":args["child_job"]["status"],"updated_at":args["finished_at"]}),
    )?;
    Ok(json!({"action":"submit","edge":updated}))
}
pub fn enqueue(kernel: &mut Kernel, args: &Value) -> Result<Value> {
    let edge = lookup(kernel, json!({"edge_id":args["edge_id"]}))?;
    ensure!(!edge.is_null(), "callback edge missing");
    if terminal(&edge) {
        return Ok(json!({"created":false,"edge":edge}));
    }
    ensure!(
        edge["state"] == "child_completed",
        "callback child not completed"
    );
    let built = if args["request"].is_null() {
        build_request(
            kernel,
            &edge,
            &args["child_job"],
            &args["decision"],
            records::string(args, "finished_at")?,
        )?
    } else {
        args["request"].clone()
    };
    let request = &built;
    let agent = records::string(request, "to_agent")?;
    ensure!(
        edge["callback_target_agent"] == agent
            && request["route_options"]["chain_edge_id"] == edge["edge_id"],
        "callback target mismatch"
    );
    ensure!(
        request["from_actor"] == edge["original_caller"]
            && request["reply_to"] == edge["parent_message_id"]
            && request["message_type"] == "chain_continuation"
            && request["route_options"]["chain_parent_job_id"] == edge["parent_job_id"]
            && request["route_options"]["chain_child_job_id"] == edge["child_job_id"],
        "callback continuation identity mismatch"
    );
    let outstanding = kernel.dispatch(
        "scheduler.has_outstanding_for",
        &json!({"state_id":args["state_id"],"target_kind":"agent","target_name":agent}),
    )?;
    let status = if outstanding == true {
        "queued"
    } else {
        "accepted"
    };
    let timestamp = records::string(args, "finished_at")?;
    let job_id = format!("job_{}", &uuid::Uuid::new_v4().simple().to_string()[..12]);
    let job = json!({"schema_version":2,"record_type":"job_record","job_id":job_id,"submission_id":null,"agent_name":agent,"provider":args["provider"],
        "request":request,"status":status,"terminal_decision":null,"cancel_requested_at":null,"created_at":timestamp,"updated_at":timestamp,"workspace_path":args["workspace_path"],
        "target_kind":"agent","target_name":agent,"provider_instance":null,"provider_options":{}});
    records::dispatch(
        &mut kernel.store,
        "append",
        &json!({"stream":"jobs","agent_name":agent,"record":job}),
    )?;
    records::dispatch(
        &mut kernel.store,
        "append",
        &json!({"stream":"job_events","agent_name":agent,"record":{
        "schema_version":2,"record_type":"job_event","event_id":format!("evt_{}",&uuid::Uuid::new_v4().simple().to_string()[..12]),"job_id":job_id,"agent_name":agent,"target_kind":"agent","target_name":agent,
        "type":if status=="accepted" {"job_accepted"} else {"job_queued"},"payload":{"status":status,"chain_continuation":true},"timestamp":timestamp}}),
    )?;
    bureau::retry(
        kernel,
        &json!({"message_id":edge["parent_message_id"],"job":job,"accepted_at":timestamp}),
    )?;
    let parent = records::dispatch(
        &mut kernel.store,
        "find_last",
        &json!({"stream":"messages","fields":{"message_id":edge["parent_message_id"]}}),
    )?;
    if records::truthy(&parent["submission_id"]) {
        let mut submission = records::dispatch(
            &mut kernel.store,
            "find_last",
            &json!({"stream":"submissions","fields":{"submission_id":parent["submission_id"]}}),
        )?;
        if !submission.is_null() {
            let ids = submission["job_ids"]
                .as_array_mut()
                .ok_or_else(|| anyhow::anyhow!("invalid submission jobs"))?;
            if !ids.contains(&json!(job_id)) {
                ids.push(json!(job_id));
            }
            submission["updated_at"] = json!(timestamp);
            records::dispatch(
                &mut kernel.store,
                "append",
                &json!({"stream":"submissions","record":submission}),
            )?;
        }
    }
    let updated = update(
        kernel,
        &edge,
        json!({"state":"continuation_submitted","continuation_job_id":job_id,"continuation_message_id":edge["parent_message_id"],"updated_at":timestamp}),
    )?;
    if !args["child_job"].is_null() {
        admission::event(
            kernel,
            &args["child_job"],
            "chain_continuation_submitted",
            json!({"edge_id":updated["edge_id"],
            "continuation_job_id":updated["continuation_job_id"],"continuation_message_id":updated["continuation_message_id"],
            "callback_target_agent":updated["callback_target_agent"]}),
            timestamp,
        )?;
    }
    // No fallible persistence operations follow in-memory index publication.
    // Commit failures terminate the process; restart rebuilds from its journal.
    kernel.dispatch(
        "scheduler.admit",
        &json!({"state_id":args["state_id"],"jobs":[job]}),
    )?;
    Ok(json!({"created":true,"edge":updated,"job":job}))
}

fn build_request(
    kernel: &mut Kernel,
    edge: &Value,
    child: &Value,
    decision: &Value,
    timestamp: &str,
) -> Result<Value> {
    records::validate("jobs", child, Some(&child["agent_name"]))?;
    ensure!(
        decision["terminal"] == true && decision["status"] == child["status"],
        "invalid callback result decision"
    );
    let mut reply = decision["reply"].as_str().unwrap_or("").to_owned();
    let artifact = &decision["diagnostics"]["reply_artifact"];
    if artifact.is_object() {
        if reply.is_empty() {
            reply = "(reply body stored as artifact)".into();
        }
        reply.push_str(&format!(
            "\n\nFull child reply artifact:\npath: {}",
            artifact["path"].as_str().unwrap_or("").trim()
        ));
        if records::truthy(&artifact["bytes"]) {
            reply.push_str(&format!(
                "\nbytes: {}",
                artifact["bytes"]
                    .as_str()
                    .map(str::to_owned)
                    .unwrap_or_else(|| artifact["bytes"].to_string())
                    .trim()
            ));
        }
        if let Some(hash) = artifact["sha256"].as_str().filter(|s| !s.trim().is_empty()) {
            reply.push_str(&format!("\nsha256: {}", hash.trim()));
        }
        reply = reply.trim_end().to_owned();
    }
    let mut lines = vec![
        "CCB result-chain continuation.".into(),
        String::new(),
        format!(
            "Original caller: {}",
            edge["original_caller"].as_str().unwrap_or("")
        ),
        format!(
            "Parent job: {}",
            edge["parent_job_id"].as_str().unwrap_or("")
        ),
        format!("Child job: {}", child["job_id"].as_str().unwrap_or("")),
        format!(
            "Child agent: {}",
            child["agent_name"].as_str().unwrap_or("")
        ),
        format!("Child status: {}", child["status"].as_str().unwrap_or("")),
    ];
    for (key, label) in [
        ("parent_body", "Original task context:"),
        ("child_body", "Delegated child task:"),
    ] {
        let body = edge["diagnostics"][key].as_str().unwrap_or("").trim_end();
        if !body.is_empty() {
            lines.extend([String::new(), label.into(), body.into()]);
        }
    }
    lines.extend([String::new(),"Child result:".into(),if reply.is_empty() {"(no reply body)".into()} else {reply},String::new(),
        "Continue the original task using the child result.".into(),"Finish this current response with the final result.".into(),
        "Do not call ask, --chain, or --silence to the original caller; CCB will deliver this chain result upstream.".into()]);
    let mut body = lines.join("\n");
    let artifact = if body.len() > 4096 {
        let artifact = artifacts::write(
            kernel,
            &body,
            "result-chain-continuation",
            records::string(edge, "edge_id")?,
            timestamp,
        )?;
        body=artifacts::stub(&format!("CCB result-chain continuation {} is larger than 4 KiB and was stored as an artifact.",records::string(edge,"edge_id")?),&artifact,true,None);
        artifact
    } else {
        Value::Null
    };
    Ok(
        json!({"project_id":child["request"]["project_id"],"to_agent":edge["callback_target_agent"],"from_actor":edge["original_caller"],
        "body":body,"task_id":edge["original_task_id"],"reply_to":edge["parent_message_id"],"message_type":"chain_continuation",
        "delivery_scope":"single","silence_on_success":false,"body_artifact":artifact,"route_options":{
            "mode":"chain_continuation","chain_edge_id":edge["edge_id"],"chain_parent_job_id":edge["parent_job_id"],
            "chain_child_job_id":edge["child_job_id"],"chain_child_message_id":edge["child_message_id"],
            "allowed_chain_targets":edge["diagnostics"]["allowed_chain_targets"].as_array().cloned().unwrap_or_default(),
            "bind_chain_workspace_tree":records::truthy(&edge["diagnostics"]["bind_chain_workspace_tree"])}}),
    )
}
pub fn done(kernel: &mut Kernel, args: &Value) -> Result<Value> {
    let edge = lookup(kernel, json!({"edge_id":args["edge_id"]}))?;
    if edge.is_null()
        || ["done", "failed", "timed_out"].contains(&edge["state"].as_str().unwrap_or(""))
    {
        return Ok(Value::Null);
    }
    update(
        kernel,
        &edge,
        json!({"state":"done","updated_at":args["finished_at"]}),
    )?;
    let others = records::dispatch(
        &mut kernel.store,
        "latest_filter",
        &json!({"stream":"callbacks","identity":"edge_id","fields":{"parent_message_id":edge["parent_message_id"],"state":"continuation_submitted"}}),
    )?;
    for prior in others.as_array().unwrap() {
        if prior["edge_id"] != edge["edge_id"] {
            update(
                kernel,
                prior,
                json!({"state":"done","updated_at":args["finished_at"]}),
            )?;
        }
    }
    Ok(Value::Null)
}
