//! Owned record streams and message/callback transitions. Wire schemas retain
//! the existing fields, including opaque provider-specific options.
use crate::{models::validate_agent, store::Store};
use anyhow::{bail, ensure, Result};
use serde_json::{json, Value};
use std::collections::HashSet;

pub fn path(args: &Value) -> Result<String> {
    let stream = string(args, "stream")?;
    Ok(match stream {
        "jobs" | "job_events" => {
            let agent = string(args, "agent_name")?;
            validate_agent(agent)?;
            format!(
                "agents/{agent}/{}.jsonl",
                if stream == "jobs" { "jobs" } else { "events" }
            )
        }
        "messages" => "messages/messages.jsonl".into(),
        "attempts" => "attempts/attempts.jsonl".into(),
        "replies" => "replies/replies.jsonl".into(),
        "callbacks" => "callbacks/edges.jsonl".into(),
        "submissions" => "submissions.jsonl".into(),
        _ => bail!("unknown record stream"),
    })
}
pub fn dispatch(store: &mut Store, op: &str, args: &Value) -> Result<Value> {
    if op == "metrics" {
        return Ok(store.index_metrics());
    }
    let path = path(args)?;
    match op {
        "append" => {
            validate(
                string(args, "stream")?,
                &args["record"],
                args.get("agent_name"),
            )?;
            store.append(&path, &args["record"])?;
            Ok(Value::Null)
        }
        "list" => Ok(json!(store.rows(&path)?)),
        "tail" | "find_last" | "read_since" => {
            let bytes = store.bytes(&path)?;
            let lines: Vec<_> = bytes
                .as_deref()
                .map(std::str::from_utf8)
                .transpose()?
                .unwrap_or("")
                .lines()
                .filter(|s| !s.trim().is_empty())
                .collect();
            if op == "read_since" {
                let start = integer(args, "start_line")? as usize;
                let rows = lines
                    .iter()
                    .skip(start)
                    .map(|s| object_row(s))
                    .collect::<Result<Vec<_>>>()?;
                return Ok(
                    json!({"line_no":if bytes.is_none() { start } else { lines.len() },"rows":rows}),
                );
            }
            if op == "tail" {
                let limit = integer(args, "limit")? as usize;
                let mut rows = Vec::new();
                for line in lines.iter().rev() {
                    if rows.len() >= limit {
                        break;
                    }
                    match object_row(line) {
                        Ok(row) => rows.push(row),
                        Err(error) if args["ignore_invalid"] != true => return Err(error),
                        Err(_) => {}
                    }
                }
                rows.reverse();
                return Ok(json!(rows));
            }
            for line in lines.iter().rev() {
                let row = object_row(line)?;
                if matches_query(&row, args)? {
                    return Ok(row);
                }
            }
            Ok(Value::Null)
        }
        "latest" | "latest_find" | "latest_filter" => {
            let identity = string(args, "identity")?;
            let rows = store.latest_rows(&path, identity)?;
            if op == "latest" {
                return Ok(json!(rows));
            }
            let mut matching = Vec::new();
            for row in rows {
                if matches_query(&row, args)? {
                    matching.push(row);
                    if op == "latest_find" {
                        break;
                    }
                }
            }
            Ok(if op == "latest_find" {
                matching.into_iter().next().unwrap_or(Value::Null)
            } else {
                json!(matching)
            })
        }
        _ => bail!("unsupported record operation: {op}"),
    }
}
fn object_row(line: &str) -> Result<Value> {
    let row: Value = serde_json::from_str(line)?;
    ensure!(row.is_object(), "expected JSON object row");
    Ok(row)
}
fn matches_query(row: &Value, args: &Value) -> Result<bool> {
    if let Some(fields) = args.get("in_fields") {
        for (key, values) in fields
            .as_object()
            .ok_or_else(|| anyhow::anyhow!("invalid in_fields"))?
        {
            if !values
                .as_array()
                .ok_or_else(|| anyhow::anyhow!("invalid field choices"))?
                .contains(&row[key])
            {
                return Ok(false);
            }
        }
    }
    for key in ["fields", "exclude"] {
        if let Some(fields) = args.get(key) {
            for (field, value) in fields
                .as_object()
                .ok_or_else(|| anyhow::anyhow!("invalid query fields"))?
            {
                let equal = field_value(row, field) == value;
                if (key == "fields" && !equal) || (key == "exclude" && equal) {
                    return Ok(false);
                }
            }
        }
    }
    if let Some(fields) = args.get("nonempty") {
        for field in fields
            .as_array()
            .ok_or_else(|| anyhow::anyhow!("invalid nonempty fields"))?
        {
            let field = field
                .as_str()
                .ok_or_else(|| anyhow::anyhow!("invalid query field"))?;
            if row[field].as_str().is_none_or(|s| s.trim().is_empty()) {
                return Ok(false);
            }
        }
    }
    Ok(true)
}
pub fn field_value<'a>(row: &'a Value, field: &str) -> &'a Value {
    if let Some(value) = row.get(field) {
        return value;
    }
    field
        .split('.')
        .try_fold(row, |value, key| value.get(key))
        .unwrap_or(&Value::Null)
}
pub fn validate(stream: &str, row: &Value, owner: Option<&Value>) -> Result<()> {
    let (schema, kind, required) = match stream {
        "jobs" => (2, "job_record", vec!["job_id", "agent_name", "target_name"]),
        "job_events" => (
            2,
            "job_event",
            vec!["event_id", "job_id", "type", "agent_name", "target_name"],
        ),
        "submissions" => (
            2,
            "submission_record",
            vec!["submission_id", "project_id", "from_actor"],
        ),
        "messages" => (
            1,
            "message_record",
            vec!["message_id", "from_actor", "target_scope"],
        ),
        "attempts" => (
            1,
            "attempt_record",
            vec![
                "attempt_id",
                "message_id",
                "agent_name",
                "provider",
                "job_id",
            ],
        ),
        "replies" => (
            1,
            "reply_record",
            vec!["reply_id", "message_id", "attempt_id", "agent_name"],
        ),
        "callbacks" => (
            1,
            "callback_edge",
            vec![
                "edge_id",
                "parent_job_id",
                "parent_message_id",
                "child_job_id",
                "child_message_id",
            ],
        ),
        _ => bail!("invalid record stream"),
    };
    ensure!(
        row["schema_version"] == schema && row["record_type"] == kind,
        "invalid {kind} schema"
    );
    for field in required {
        ensure!(!string(row, field)?.is_empty(), "empty {field}");
    }
    match stream {
        "jobs" | "job_events" => {
            ensure!(row["target_kind"] == "agent", "invalid target kind");
            ensure!(
                owner.is_some_and(
                    |value| row["target_name"] == *value && row["agent_name"] == *value
                ),
                "record target mismatch"
            );
            if stream == "jobs" {
                let status = string(row, "status")?;
                ensure!(
                    [
                        "accepted",
                        "queued",
                        "running",
                        "completed",
                        "cancelled",
                        "failed",
                        "incomplete"
                    ]
                    .contains(&status),
                    "invalid job status"
                );
                if ["completed", "cancelled", "failed", "incomplete"].contains(&status) {
                    ensure!(
                        !row["terminal_decision"].is_null(),
                        "terminal_decision required"
                    );
                }
                ensure!(row["request"].is_object(), "job request required");
            }
        }
        "messages" => {
            enum_field(
                row,
                "message_state",
                &[
                    "created",
                    "queued",
                    "dispatching",
                    "running",
                    "partially_replied",
                    "completed",
                    "incomplete",
                    "failed",
                    "cancelled",
                    "dead_letter",
                ],
            )?;
            ensure!(
                row["target_agents"]
                    .as_array()
                    .is_some_and(|a| !a.is_empty()),
                "target_agents required"
            );
            integer(row, "priority")?;
        }
        "attempts" => {
            validate_agent(string(row, "agent_name")?)?;
            integer(row, "retry_index")?;
            enum_field(
                row,
                "attempt_state",
                &[
                    "pending",
                    "delivering",
                    "running",
                    "waiting_completion",
                    "reply_ready",
                    "stalled",
                    "runtime_dead",
                    "failed",
                    "incomplete",
                    "cancelled",
                    "superseded",
                    "dead_letter",
                    "completed",
                ],
            )?;
        }
        "replies" => {
            validate_agent(string(row, "agent_name")?)?;
            enum_field(
                row,
                "terminal_status",
                &["completed", "incomplete", "failed", "cancelled"],
            )?;
        }
        "callbacks" => enum_field(
            row,
            "state",
            &[
                "pending",
                "child_completed",
                "continuation_submitted",
                "done",
                "failed",
                "timed_out",
            ],
        )?,
        _ => {}
    }
    Ok(())
}
fn enum_field(row: &Value, field: &str, values: &[&str]) -> Result<()> {
    ensure!(values.contains(&string(row, field)?), "invalid {field}");
    Ok(())
}
pub fn string<'a>(value: &'a Value, field: &str) -> Result<&'a str> {
    value[field]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("missing string: {field}"))
}
fn integer(value: &Value, field: &str) -> Result<u64> {
    value[field]
        .as_u64()
        .ok_or_else(|| anyhow::anyhow!("invalid {field}"))
}

pub fn message_transition(store: &mut Store, op: &str, args: &Value) -> Result<Value> {
    let id = string(args, "message_id")?;
    let query = json!({"stream":"messages","fields":{"message_id":id}});
    let mut message = dispatch(store, "find_last", &query)?;
    if message.is_null() {
        return Ok(Value::Null);
    }
    let state = if op == "set" {
        string(args, "state")?.to_owned()
    } else {
        let attempts = dispatch(
            store,
            "latest",
            &json!({"stream":"attempts","identity":"attempt_id"}),
        )?;
        let attempts: Vec<_> = attempts
            .as_array()
            .unwrap()
            .iter()
            .filter(|a| a["message_id"] == id)
            .collect();
        if attempts.is_empty() {
            return Ok(Value::Null);
        }
        let replies = dispatch(store, "list", &json!({"stream":"replies"}))?;
        let replies: Vec<_> = replies
            .as_array()
            .unwrap()
            .iter()
            .filter(|r| r["message_id"] == id)
            .collect();
        let active = attempts.iter().any(|a| {
            ![
                "completed",
                "incomplete",
                "failed",
                "cancelled",
                "superseded",
                "dead_letter",
            ]
            .contains(&a["attempt_state"].as_str().unwrap_or(""))
        });
        if active {
            if replies.is_empty() {
                "running"
            } else {
                "partially_replied"
            }
            .into()
        } else {
            let terminal: Vec<_> = replies
                .iter()
                .filter(|r| !truthy(&r["diagnostics"]["notice"]))
                .collect();
            let statuses: HashSet<_> = if !terminal.is_empty() {
                terminal
                    .iter()
                    .filter_map(|r| r["terminal_status"].as_str())
                    .collect()
            } else if !replies.is_empty() {
                replies
                    .iter()
                    .filter_map(|r| r["terminal_status"].as_str())
                    .collect()
            } else {
                attempts
                    .iter()
                    .filter_map(|a| a["attempt_state"].as_str())
                    .collect()
            };
            if statuses.len() == 1
                && ["completed", "cancelled", "failed", "incomplete"]
                    .contains(statuses.iter().next().unwrap())
            {
                statuses.iter().next().unwrap().to_string()
            } else {
                "incomplete".into()
            }
        }
    };
    if message["message_state"] == state {
        return Ok(Value::Null);
    }
    message["message_state"] = json!(state);
    message["updated_at"] = args["updated_at"].clone();
    validate("messages", &message, None)?;
    store.append("messages/messages.jsonl", &message)?;
    Ok(Value::Null)
}
pub fn callback_update(store: &mut Store, args: &Value) -> Result<Value> {
    let mut record = args["record"].clone();
    let changes = args["changes"]
        .as_object()
        .ok_or_else(|| anyhow::anyhow!("invalid callback changes"))?;
    for (key, value) in changes {
        ensure!(
            record.get(key).is_some() && !["schema_version", "record_type"].contains(&key.as_str()),
            "unknown callback field"
        );
        record[key] = value.clone();
    }
    validate("callbacks", &record, None)?;
    store.append("callbacks/edges.jsonl", &record)?;
    Ok(record)
}

pub fn truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().is_some_and(|n| n != 0.0),
        Value::String(s) => !s.is_empty(),
        Value::Array(a) => !a.is_empty(),
        Value::Object(m) => !m.is_empty(),
    }
}
