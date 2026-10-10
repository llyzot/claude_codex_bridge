//! A submitted plan is one durable transaction, including its callback edge.
use crate::{bureau, callback_policy, records, Kernel};
use anyhow::{ensure, Result};
use serde_json::{json, Value};
use std::collections::HashSet;

pub(crate) fn new_id(prefix: &str) -> String {
    format!(
        "{prefix}_{}",
        &uuid::Uuid::new_v4().simple().to_string()[..12]
    )
}
pub(crate) fn append(kernel: &mut Kernel, stream: &str, record: &Value) -> Result<()> {
    let mut args = json!({"stream":stream,"record":record});
    if stream == "jobs" || stream == "job_events" {
        args["agent_name"] = record["agent_name"].clone();
    }
    records::dispatch(&mut kernel.store, "append", &args)?;
    Ok(())
}
pub(crate) fn event(
    kernel: &mut Kernel,
    job: &Value,
    kind: &str,
    payload: Value,
    timestamp: &str,
) -> Result<()> {
    append(
        kernel,
        "job_events",
        &json!({"schema_version":2,"record_type":"job_event",
        "event_id":new_id("evt"),"job_id":job["job_id"],"agent_name":job["agent_name"],
        "target_kind":job["target_kind"],"target_name":job["target_name"],
        "type":kind,"payload":payload,"timestamp":timestamp}),
    )
}
pub(crate) fn validate_request(request: &Value) -> Result<()> {
    for key in [
        "project_id",
        "to_agent",
        "from_actor",
        "body",
        "message_type",
    ] {
        ensure!(
            !records::string(request, key)?.trim().is_empty(),
            "empty {key}"
        );
    }
    let to = records::string(request, "to_agent")?;
    ensure!(
        (to == "all" && request["delivery_scope"] == "broadcast")
            || (to != "all" && request["delivery_scope"] == "single"),
        "invalid request delivery scope"
    );
    if to != "all" {
        crate::models::validate_agent(to)?;
    }
    ensure!(
        request["route_options"].is_object(),
        "route_options must be an object"
    );
    ensure!(
        request["silence_on_success"].is_boolean(),
        "silence_on_success must be boolean"
    );
    Ok(())
}
pub fn submit(kernel: &mut Kernel, args: &Value) -> Result<Value> {
    let state = records::string(args, "state_id")?;
    // Validate the index before any persistence. Publication uses a candidate
    // state, so malformed plans cannot leak a partial in-memory admission.
    kernel.dispatch("scheduler.slots", &json!({"state_id":state}))?;
    let timestamp = records::string(args, "accepted_at")?;
    ensure!(!timestamp.is_empty(), "accepted_at cannot be empty");
    let plan = &args["plan"];
    let request = &plan["request"];
    validate_request(request)?;
    let actor = records::string(request, "from_actor")?
        .trim()
        .to_lowercase();
    ensure!(
        ["user", "system", "manual", "email", "maintenance-heartbeat"].contains(&actor.as_str())
            || args["agent_names"]
                .as_array()
                .is_some_and(|names| names.contains(&json!(actor))),
        "unknown sender agent: {actor}"
    );
    ensure!(
        plan["project_id"] == request["project_id"]
            && plan["from_actor"] == request["from_actor"]
            && plan["task_id"] == request["task_id"],
        "submission plan identity mismatch"
    );
    let drafts = plan["drafts"]
        .as_array()
        .ok_or_else(|| anyhow::anyhow!("drafts must be an array"))?;
    ensure!(!drafts.is_empty(), "jobs cannot be empty");
    let broadcast = request["delivery_scope"] == "broadcast";
    ensure!(
        broadcast
            == plan["submission_id"]
                .as_str()
                .is_some_and(|s| !s.is_empty()),
        "invalid submission id"
    );
    ensure!(
        broadcast || drafts.len() == 1,
        "single submit receipt requires exactly one job"
    );
    let context = json!({"request":request,"state_id":state,"agent_names":args["agent_names"],
        "bureau_enabled":args["bureau_enabled"],"max_callback_depth":args["max_callback_depth"]});
    let parent = callback_policy::validate(kernel, &context)?;
    let mut targets = HashSet::new();
    let mut jobs = Vec::new();
    let mut receipts = Vec::new();
    for draft in drafts {
        let agent = records::string(draft, "agent_name")?;
        crate::models::validate_agent(agent)?;
        ensure!(targets.insert(agent), "duplicate submission target");
        ensure!(
            draft["target_kind"] == "agent" && draft["target_name"] == agent,
            "record target mismatch"
        );
        ensure!(
            args["agent_names"]
                .as_array()
                .is_some_and(|names| names.contains(&json!(agent))),
            "unknown target agent"
        );
        ensure!(
            !records::string(draft, "provider")?.trim().is_empty(),
            "empty provider"
        );
        let mut expected_request = request.clone();
        expected_request["to_agent"] = json!(agent);
        expected_request["delivery_scope"] = json!("single");
        ensure!(
            draft["request"] == expected_request,
            "draft request identity mismatch"
        );
        let outstanding = kernel.dispatch(
            "scheduler.has_outstanding_for",
            &json!({"state_id":state,"target_kind":"agent","target_name":agent}),
        )?;
        let status = if outstanding == true {
            "queued"
        } else {
            "accepted"
        };
        let options = if draft["provider_options"].is_null() {
            json!({})
        } else {
            draft["provider_options"].clone()
        };
        ensure!(options.is_object(), "provider_options must be an object");
        let job = json!({"schema_version":2,"record_type":"job_record","job_id":new_id("job"),
            "submission_id":plan["submission_id"],"agent_name":agent,"provider":draft["provider"],
            "provider_instance":null,"provider_options":options,"workspace_path":draft["workspace_path"],
            "target_kind":"agent","target_name":agent,"request":draft["request"],"status":status,
            "terminal_decision":null,"cancel_requested_at":null,"created_at":timestamp,"updated_at":timestamp});
        append(kernel, "jobs", &job)?;
        event(
            kernel,
            &job,
            if status == "accepted" {
                "job_accepted"
            } else {
                "job_queued"
            },
            json!({"status":status}),
            timestamp,
        )?;
        receipts.push(json!({"job_id":job["job_id"],"agent_name":agent,"target_kind":"agent","target_name":agent,
            "provider_instance":null,"status":status,"accepted_at":timestamp}));
        jobs.push(job);
    }
    if broadcast {
        append(
            kernel,
            "submissions",
            &json!({"schema_version":2,"record_type":"submission_record",
            "submission_id":plan["submission_id"],"project_id":plan["project_id"],"from_actor":plan["from_actor"],
            "target_scope":plan["target_scope"].as_str().unwrap_or("all"),"task_id":plan["task_id"],
            "job_ids":jobs.iter().map(|j|j["job_id"].clone()).collect::<Vec<_>>(),"created_at":timestamp,"updated_at":timestamp}),
        )?;
    }
    let message = if args["bureau_enabled"] == true {
        bureau::submission(
            kernel,
            &json!({"request":request,"jobs":jobs,"submission_id":plan["submission_id"],
            "accepted_at":timestamp,"origin_message_id":plan["origin_message_id"]}),
        )?
    } else {
        Value::Null
    };
    let edge = if !parent.is_null() && callback_policy::is_chain(request) {
        callback_policy::register(kernel, &parent, request, &jobs[0], &message, timestamp)?
    } else {
        Value::Null
    };
    // Final publication: every fallible durable mutation has already staged.
    kernel.dispatch("scheduler.admit", &json!({"state_id":state,"jobs":jobs}))?;
    let receipt = if broadcast {
        json!({"accepted_at":timestamp,"submission_id":plan["submission_id"],"jobs":receipts})
    } else {
        receipts[0].clone()
    };
    Ok(json!({"receipt":receipt,"jobs":jobs,"message_id":message,"edge":edge}))
}

#[cfg(test)]
mod tests {
    use super::*;
    fn args(agent: &str, actor: &str, mode: &str) -> Value {
        let request = json!({"project_id":"project","to_agent":agent,"from_actor":actor,"body":"task",
            "task_id":"task","reply_to":null,"message_type":"ask","delivery_scope":"single",
            "silence_on_success":false,"route_options":{"mode":mode},"body_artifact":null});
        json!({"state_id":"state","agent_names":["pi_lead","codex_worker"],"bureau_enabled":true,
            "max_callback_depth":5,"accepted_at":"2026-10-11T10:00:00Z","plan":{
                "project_id":"project","from_actor":actor,"task_id":"task","request":request,"submission_id":null,
                "origin_message_id":null,"target_scope":null,"drafts":[{"agent_name":agent,"provider":"fake",
                    "target_kind":"agent","target_name":agent,"request":request,"provider_options":{},"workspace_path":null}]}})
    }
    #[test]
    fn chain_admission_recovers_at_every_actual_write_boundary() {
        let mut boundaries = 0;
        for boundary in 0..=32 {
            let directory = tempfile::tempdir().unwrap();
            let root = directory.path().join("ccbd");
            let mut kernel = Kernel::open(&root).unwrap();
            kernel
                .dispatch(
                    "scheduler.create",
                    &json!({"state_id":"state","agent_names":["pi_lead","codex_worker"]}),
                )
                .unwrap();
            let parent = submit(&mut kernel, &args("pi_lead", "user", "")).unwrap();
            kernel.store.commit().unwrap();
            let mut job = parent["jobs"][0].clone();
            job["status"] = json!("running");
            append(&mut kernel, "jobs", &job).unwrap();
            kernel.store.commit().unwrap();
            kernel
                .dispatch(
                    "scheduler.pop_next_for",
                    &json!({"state_id":"state","target_kind":"agent","target_name":"pi_lead"}),
                )
                .unwrap();
            kernel.dispatch("scheduler.mark_active_for",&json!({"state_id":"state","target_kind":"agent","target_name":"pi_lead","job_id":job["job_id"]})).unwrap();
            let child = submit(&mut kernel, &args("codex_worker", "pi_lead", "chain")).unwrap();
            let count = kernel.store.interrupt_for_test(boundary).unwrap();
            assert!(count < 32);
            drop(kernel); // No normal commit/cleanup: a journaled interrupted command.
            let mut recovered = Kernel::open(&root).unwrap();
            let jobs = records::dispatch(
                &mut recovered.store,
                "list",
                &json!({"stream":"jobs","agent_name":"codex_worker"}),
            )
            .unwrap();
            assert_eq!(jobs, json!(child["jobs"]));
            let edges =
                records::dispatch(&mut recovered.store, "list", &json!({"stream":"callbacks"}))
                    .unwrap();
            assert_eq!(edges, json!([child["edge"]]));
            let attempts =
                records::dispatch(&mut recovered.store, "list", &json!({"stream":"attempts"}))
                    .unwrap();
            assert_eq!(attempts.as_array().unwrap().len(), 2);
            assert_eq!(attempts[1]["job_id"], child["jobs"][0]["job_id"]);
            assert_eq!(attempts[1]["message_id"], child["message_id"]);
            let inbox = recovered
                .dispatch("inbox.list", &json!({"agent_name":"codex_worker"}))
                .unwrap();
            assert_eq!(inbox.as_array().unwrap().len(), 1);
            assert_eq!(inbox[0]["attempt_id"], attempts[1]["attempt_id"]);
            drop(recovered);
            let mut twice = Kernel::open(&root).unwrap();
            assert_eq!(
                records::dispatch(&mut twice.store, "list", &json!({"stream":"callbacks"}))
                    .unwrap(),
                edges
            );
            boundaries += 1;
            if boundary == count {
                break;
            }
        }
        assert!(boundaries > 7);
    }
}
