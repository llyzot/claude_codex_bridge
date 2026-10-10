//! Message admission and attempt transitions are one journaled command each.
use crate::{records, Kernel};
use anyhow::{ensure, Result};
use serde_json::{json, Value};

fn new_id(prefix: &str) -> String {
    format!(
        "{prefix}_{}",
        &uuid::Uuid::new_v4().simple().to_string()[..12]
    )
}
fn append(kernel: &mut Kernel, stream: &str, record: &Value) -> Result<()> {
    records::dispatch(
        &mut kernel.store,
        "append",
        &json!({"stream":stream,"record":record}),
    )?;
    Ok(())
}
fn lookup(kernel: &mut Kernel, stream: &str, fields: Value) -> Result<Value> {
    records::dispatch(
        &mut kernel.store,
        "find_last",
        &json!({"stream":stream,"fields":fields}),
    )
}
fn inbound(kernel: &mut Kernel, job: &Value, attempt: &Value, timestamp: &str) -> Result<()> {
    let agent = records::string(job, "agent_name")?;
    kernel.dispatch("inbox.append",&json!({"agent_name":agent,"record":{
        "schema_version":1,"record_type":"inbound_event_record","inbound_event_id":new_id("iev"),"agent_name":agent,
        "event_type":"task_request","message_id":attempt["message_id"],"attempt_id":attempt["attempt_id"],
        "payload_ref":format!("job:{}",records::string(job,"job_id")?),"priority":100,"status":"queued",
        "created_at":timestamp,"started_at":null,"finished_at":null
    }}))?;
    Ok(())
}
fn attempt(job: &Value, message_id: &str, index: u64, timestamp: &str) -> Value {
    json!({"schema_version":1,"record_type":"attempt_record","attempt_id":new_id("att"),"message_id":message_id,
        "agent_name":job["agent_name"],"provider":job["provider"],"job_id":job["job_id"],"retry_index":index,
        "health_snapshot_ref":null,"started_at":timestamp,"updated_at":timestamp,"attempt_state":"pending"})
}
pub fn submission(kernel: &mut Kernel, args: &Value) -> Result<Value> {
    let jobs = args["jobs"]
        .as_array()
        .ok_or_else(|| anyhow::anyhow!("jobs must be an array"))?;
    if jobs.is_empty() {
        return Ok(Value::Null);
    }
    let timestamp = records::string(args, "accepted_at")?;
    let request = &args["request"];
    let id = new_id("msg");
    let mut origin = args["origin_message_id"].clone();
    if !records::truthy(&origin) {
        let key = request["reply_to"].as_str().unwrap_or("").trim();
        if !key.is_empty() {
            let mut related = lookup(kernel, "attempts", json!({"attempt_id":key}))?;
            if related.is_null() {
                related = lookup(kernel, "attempts", json!({"job_id":key}))?;
            }
            if related.is_null() {
                related = lookup(kernel, "messages", json!({"message_id":key}))?;
            }
            origin = related["message_id"].clone();
        }
    }
    let message = json!({"schema_version":1,"record_type":"message_record","message_id":id,"origin_message_id":origin,
        "from_actor":request["from_actor"],"target_scope":request["delivery_scope"],"target_agents":jobs.iter().map(|j|j["agent_name"].clone()).collect::<Vec<_>>(),
        "message_class":request["message_type"],"reply_policy":{"mode":if jobs.len()>1 {"all"} else {"single"},"expected_reply_count":jobs.len(),"silence_on_success":request["silence_on_success"].as_bool().unwrap_or(false)},
        "retry_policy":{"mode":"auto","max_attempts":3,"retryable_reasons":["api_error","transport_error"],"retry_runtime_when_resume_supported":true,"retryable_runtime_reasons":["pane_dead","pane_unavailable"]},
        "priority":100,"payload_ref":null,"submission_id":args["submission_id"],"created_at":timestamp,"updated_at":timestamp,"message_state":"queued"});
    append(kernel, "messages", &message)?;
    for job in jobs {
        let agent = records::string(job, "agent_name")?;
        records::validate("jobs", job, Some(&job["agent_name"]))?;
        let attempt = attempt(job, &id, 0, timestamp);
        append(kernel, "attempts", &attempt)?;
        inbound(kernel, job, &attempt, timestamp)?;
        kernel.dispatch(
            "apply_incremental_summary_update",
            &json!({"agent_name":agent,"queue_delta":1,"timestamp":timestamp}),
        )?;
    }
    Ok(json!(id))
}
pub fn retry(kernel: &mut Kernel, args: &Value) -> Result<Value> {
    let id = records::string(args, "message_id")?;
    let timestamp = records::string(args, "accepted_at")?;
    let job = &args["job"];
    let message = lookup(kernel, "messages", json!({"message_id":id}))?;
    ensure!(!message.is_null(), "message not found: {id}");
    let targets = message["target_agents"]
        .as_array()
        .ok_or_else(|| anyhow::anyhow!("invalid message targets"))?;
    let agent = records::string(job, "agent_name")?;
    ensure!(
        targets.iter().any(|target| target
            .as_str()
            .is_some_and(|target| target.trim().eq_ignore_ascii_case(agent))),
        "retry agent is not authorized by message targets"
    );
    let mut attempt_record = lookup(kernel, "attempts", json!({"job_id":job["job_id"]}))?;
    if !attempt_record.is_null() {
        ensure!(
            attempt_record["message_id"] == id,
            "retry job is bound to another message"
        );
        if attempt_record["attempt_state"] != "pending" {
            return Ok(attempt_record["attempt_id"].clone());
        }
        let event = kernel.dispatch("inbox.latest",&json!({"agent_name":job["agent_name"],"field":"attempt_id","value":attempt_record["attempt_id"]}))?;
        if event.is_null() {
            let accepted = attempt_record["started_at"]
                .as_str()
                .filter(|s| !s.is_empty())
                .unwrap_or(timestamp);
            inbound(kernel, job, &attempt_record, accepted)?;
        }
        kernel.dispatch(
            "rebuild_mailbox_summary",
            &json!({"agent_name":job["agent_name"],"timestamp":timestamp}),
        )?;
    } else {
        let attempts = records::dispatch(&mut kernel.store, "list", &json!({"stream":"attempts"}))?;
        let index = attempts
            .as_array()
            .unwrap()
            .iter()
            .filter(|a| a["message_id"] == id && a["agent_name"] == job["agent_name"])
            .filter_map(|a| a["retry_index"].as_u64())
            .max()
            .map(|n| n + 1)
            .unwrap_or(0);
        attempt_record = attempt(job, id, index, timestamp);
        append(kernel, "attempts", &attempt_record)?;
        inbound(kernel, job, &attempt_record, timestamp)?;
        kernel.dispatch(
            "apply_incremental_summary_update",
            &json!({"agent_name":job["agent_name"],"queue_delta":1,"timestamp":timestamp}),
        )?;
    }
    records::message_transition(
        &mut kernel.store,
        "set",
        &json!({"message_id":id,"state":"queued","updated_at":timestamp}),
    )?;
    Ok(attempt_record["attempt_id"].clone())
}
pub fn attempt_transition(kernel: &mut Kernel, op: &str, args: &Value) -> Result<Value> {
    let job = &args["job"];
    let timestamp = records::string(args, "timestamp")?;
    let agent = records::string(job, "agent_name")?;
    let mut attempt = lookup(kernel, "attempts", json!({"job_id":job["job_id"]}))?;
    if attempt.is_null() {
        return Ok(Value::Null);
    }
    let started = op == "started";
    let job_status = records::string(job, "status")?;
    let status = if started {
        "running"
    } else if ["completed", "failed", "incomplete", "cancelled"].contains(&job_status) {
        job_status
    } else {
        "incomplete"
    };
    attempt["attempt_state"] = json!(status);
    attempt["updated_at"] = json!(timestamp);
    if started && attempt["started_at"].as_str().is_none_or(str::is_empty) {
        attempt["started_at"] = json!(timestamp);
    }
    append(kernel, "attempts", &attempt)?;
    let options = &job["provider_options"];
    let delivery = job["request"]["message_type"]
        .as_str()
        .is_some_and(|s| s.trim().eq_ignore_ascii_case("reply_delivery"))
        || options["reply_delivery"] == true;
    let explicit = options["reply_delivery_inbound_event_id"]
        .as_str()
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let mut event = if started && delivery && explicit.is_some() {
        kernel.dispatch(
            "inbox.latest",
            &json!({"agent_name":agent,"field":"inbound_event_id","value":explicit}),
        )?
    } else {
        Value::Null
    };
    if event.is_null() {
        event = kernel.dispatch(
            "inbox.latest",
            &json!({"agent_name":agent,"field":"attempt_id","value":attempt["attempt_id"]}),
        )?;
    }
    let pending = !event.is_null()
        && !["consumed", "superseded", "abandoned"]
            .contains(&event["status"].as_str().unwrap_or(""));
    if pending {
        let method = if started {
            "claim"
        } else if job_status == "cancelled"
            && ["created", "queued"].contains(&event["status"].as_str().unwrap_or(""))
        {
            "abandon"
        } else {
            "consume"
        };
        if !started || event["status"] != "delivering" {
            kernel.dispatch(method,&json!({"agent_name":agent,"inbound_event_id":event["inbound_event_id"],"timestamp":timestamp,"lease_ttl_seconds":args["lease_ttl_seconds"]}))?;
        }
    } else {
        kernel.dispatch(
            "rebuild_mailbox_summary",
            &json!({"agent_name":agent,"timestamp":timestamp}),
        )?;
    }
    records::message_transition(
        &mut kernel.store,
        if started { "set" } else { "refresh" },
        &json!({"message_id":attempt["message_id"],"state":"running","updated_at":timestamp}),
    )?;
    Ok(Value::Null)
}

pub fn queue_reply(kernel: &mut Kernel, args: &Value) -> Result<Value> {
    let agent = records::string(args, "agent_name")?;
    let timestamp = records::string(args, "timestamp")?;
    let consumed = args["consumed_notice"] == true;
    kernel.dispatch("inbox.append",&json!({"agent_name":agent,"record":{
        "schema_version":1,"record_type":"inbound_event_record","inbound_event_id":new_id("iev"),"agent_name":agent,
        "event_type":if consumed {"completion_notice"} else {"task_reply"},"message_id":args["message_id"],"attempt_id":args["attempt_id"],
        "payload_ref":format!("reply:{}",records::string(args,"reply_id")?),"priority":10,"status":if consumed {"consumed"} else {"queued"},
        "created_at":timestamp,"started_at":if consumed {Some(timestamp)} else {None},"finished_at":if consumed {Some(timestamp)} else {None}
    }}))?;
    if !consumed {
        kernel.dispatch("apply_incremental_summary_update",&json!({"agent_name":agent,"queue_delta":1,"pending_reply_delta":1,"timestamp":timestamp}))?;
    }
    Ok(Value::Null)
}
pub fn reply(kernel: &mut Kernel, args: &Value) -> Result<Value> {
    let job = &args["job"];
    let decision = &args["decision"];
    let timestamp = records::string(args, "finished_at")?;
    let attempt = lookup(kernel, "attempts", json!({"job_id":job["job_id"]}))?;
    if attempt.is_null() {
        return Ok(Value::Null);
    }
    let status = records::string(job, "status")?;
    let terminal_status = if ["completed", "cancelled", "failed", "incomplete"].contains(&status) {
        status
    } else {
        "incomplete"
    };
    let request = &job["request"];
    let mut text = decision["reply"].as_str().unwrap_or("").to_owned();
    if status == "completed" && records::truthy(&request["silence_on_success"]) {
        text = format!(
            "CCB_COMPLETE from={} status={} job={}",
            records::string(job, "agent_name")?,
            status,
            records::string(job, "job_id")?
        );
        if let Some(task) = request["task_id"]
            .as_str()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            text.push_str(&format!(" task={task}"));
        }
        text.push_str(" result=hidden");
    }
    let artifact = &decision["diagnostics"]["reply_artifact"];
    let empty_cancel = status == "cancelled" && text.trim().is_empty() && !artifact.is_object();
    let id = new_id("rep");
    let mut diagnostics = json!({"reason":decision["reason"],"status":status,"provider_turn_ref":decision["provider_turn_ref"],
        "decision_diagnostics":decision["diagnostics"],"silence_on_success":records::truthy(&request["silence_on_success"])});
    let notice = decision["diagnostics"]["notice_kind"]
        .as_str()
        .unwrap_or("")
        .trim();
    if empty_cancel {
        diagnostics["notice"] = json!(true);
        diagnostics["notice_kind"] = json!("cancelled");
        diagnostics["delivery_mode"] = json!("auto_consumed_control_notice");
    } else if !notice.is_empty() {
        diagnostics["notice"] = json!(true);
        diagnostics["notice_kind"] = json!(notice);
        diagnostics["delivery_mode"] = json!("caller_notice");
    }
    let record = json!({"schema_version":1,"record_type":"reply_record","reply_id":id,"message_id":attempt["message_id"],"attempt_id":attempt["attempt_id"],
        "agent_name":job["agent_name"],"terminal_status":terminal_status,"reply":text,"reply_artifact":if artifact.as_object().is_some_and(|a|!a.is_empty()) {artifact.clone()} else {Value::Null},"diagnostics":diagnostics,"finished_at":timestamp});
    append(kernel, "replies", &record)?;
    if args["deliver_to_caller"] != false {
        let caller = request["from_actor"]
            .as_str()
            .unwrap_or("")
            .trim()
            .to_ascii_lowercase();
        if args["known_mailboxes"]
            .as_array()
            .is_some_and(|a| a.contains(&json!(caller)))
            && crate::models::validate_agent(&caller).is_ok()
        {
            queue_reply(
                kernel,
                &json!({"agent_name":caller,"message_id":attempt["message_id"],"attempt_id":attempt["attempt_id"],"reply_id":id,"timestamp":timestamp,"consumed_notice":empty_cancel}),
            )?;
        }
    }
    records::message_transition(
        &mut kernel.store,
        "refresh",
        &json!({"message_id":attempt["message_id"],"updated_at":timestamp}),
    )?;
    Ok(json!(id))
}
pub fn terminal(kernel: &mut Kernel, args: &Value) -> Result<Value> {
    let mut transition = args.clone();
    transition["timestamp"] = args["finished_at"].clone();
    attempt_transition(kernel, "terminal", &transition)?;
    if args["record_reply_enabled"] == false {
        Ok(Value::Null)
    } else {
        reply(kernel, args)
    }
}
