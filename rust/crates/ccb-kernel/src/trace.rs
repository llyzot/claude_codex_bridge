//! Read-only, project-scoped legacy trace projection plus callback graph links.
use super::Daemon;
use anyhow::{ensure, Result};
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet};

fn pick(row: &Value, keys: &[&str]) -> Value {
    if row.is_null() {
        return Value::Null;
    }
    let mut out = json!({});
    for key in keys {
        out[*key] = row[*key].clone();
    }
    out
}
fn message(row: &Value) -> Value {
    let mut out = pick(
        row,
        &[
            "message_id",
            "origin_message_id",
            "submission_id",
            "from_actor",
            "target_scope",
            "target_agents",
            "message_class",
            "message_state",
            "priority",
            "created_at",
            "updated_at",
        ],
    );
    if out.is_null() {
        return out;
    }
    for (name, key) in [
        ("reply_mode", "mode"),
        ("expected_reply_count", "expected_reply_count"),
        ("silence_on_success", "silence_on_success"),
    ] {
        out[name] = row["reply_policy"][key].clone();
    }
    out["silence_on_success"] = json!(row["reply_policy"]["silence_on_success"] == true);
    out["retry_mode"] = row["retry_policy"]["mode"].clone();
    out
}
fn attempt(row: &Value) -> Value {
    pick(
        row,
        &[
            "attempt_id",
            "message_id",
            "agent_name",
            "provider",
            "job_id",
            "retry_index",
            "attempt_state",
            "health_snapshot_ref",
            "started_at",
            "updated_at",
        ],
    )
}
fn submission(row: &Value) -> Value {
    pick(
        row,
        &[
            "submission_id",
            "from_actor",
            "target_scope",
            "task_id",
            "job_ids",
            "created_at",
            "updated_at",
        ],
    )
}
pub(super) fn reply(row: &Value) -> Value {
    if row.is_null() {
        return Value::Null;
    }
    let mut out = pick(
        row,
        &[
            "reply_id",
            "message_id",
            "attempt_id",
            "agent_name",
            "terminal_status",
            "finished_at",
            "reply_artifact",
        ],
    );
    let text = row["reply"].as_str().unwrap_or("");
    let count = text.chars().count();
    out["reply"] = json!(text.chars().take(32768).collect::<String>());
    out["reply_size"] = json!(count);
    out["reply_truncated"] = json!(count > 32768);
    let preview = text
        .replace('\r', "")
        .replace('\n', "\\n")
        .trim()
        .to_owned();
    out["reply_preview"] = json!(if preview.chars().count() > 120 {
        format!(
            "{}...",
            preview.chars().take(120).collect::<String>().trim_end()
        )
    } else {
        preview
    });
    let d = &row["diagnostics"];
    let decision = &d["decision_diagnostics"];
    let artifact = &row["reply_artifact"];
    for key in ["path", "bytes", "sha256"] {
        out[format!("reply_artifact_{key}")] = artifact[key].clone();
    }
    for key in [
        "notice_kind",
        "last_progress_at",
        "heartbeat_silence_seconds",
        "reason",
        "status",
        "provider_turn_ref",
    ] {
        out[key] = d[key].clone();
    }
    let kind = if crate::records::truthy(&d["notice_kind"]) {
        let text = d["notice_kind"]
            .as_str()
            .map(str::to_owned)
            .unwrap_or_else(|| d["notice_kind"].to_string())
            .trim()
            .to_lowercase();
        if text.is_empty() {
            None
        } else {
            Some(text)
        }
    } else {
        None
    };
    out["notice_kind"] = json!(kind);
    out["last_progress_at"] = json!(d["last_progress_at"]
        .as_str()
        .map(str::trim)
        .filter(|s| !s.is_empty()));
    out["heartbeat_silence_seconds"] = json!(d["heartbeat_silence_seconds"]
        .as_f64()
        .or_else(|| d["heartbeat_silence_seconds"]
            .as_str()
            .and_then(|s| s.parse::<f64>().ok()))
        .filter(|n| n.is_finite()));
    out["notice"] = json!(crate::records::truthy(&d["notice"]) || kind.is_some());
    out["silence_on_success"] = json!(d["silence_on_success"] == true);
    for (key, source) in [
        ("completion_mode", "mode"),
        ("total_secs", "total_secs"),
        ("anchor_seen", "anchor_seen"),
        ("captured_reply_chars", "reply_chars"),
    ] {
        out[key] = decision[source].clone();
    }
    let forced = decision["artifact_reply_forced"] == true;
    let empty = artifact.is_object() && artifact["bytes"].as_u64() == Some(0);
    out["artifact_reply_forced"] = json!(forced);
    out["empty_reply_artifact"] = json!(empty);
    out["no_captured_reply"] = json!(forced && empty && row["terminal_status"] != "completed");
    out
}
fn member(set: &BTreeSet<String>, v: &Value) -> bool {
    v.as_str().is_some_and(|s| set.contains(s))
}
fn id(row: &Value, key: &str) -> String {
    row[key].as_str().unwrap_or("").to_owned()
}
fn add(set: &mut BTreeSet<String>, v: &Value) -> bool {
    v.as_str()
        .filter(|s| !s.is_empty())
        .is_some_and(|s| set.insert(s.into()))
}
fn find(rows: &[Value], key: &str, target: &str) -> Value {
    rows.iter()
        .find(|r| r[key] == target)
        .cloned()
        .unwrap_or(Value::Null)
}
fn page(out: &mut Value, key: &str, rows: Vec<Value>, offset: usize, limit: usize) -> bool {
    let total = rows.len();
    out[format!("{key}_total")] = json!(total);
    out[key] = json!(rows
        .into_iter()
        .skip(offset)
        .take(limit)
        .collect::<Vec<_>>());
    offset.saturating_add(limit) < total
}
impl Daemon {
    pub(super) fn trace_job_record(&self, job_id: &str) -> Result<Value> {
        for agent in &self.known_names {
            let row = self.kernel.store.latest_by_id(
                &format!("agents/{agent}/jobs.jsonl"),
                "job_id",
                job_id,
            )?;
            if !row.is_null() {
                ensure!(
                    row["request"]["project_id"] == self.config["project_id"],
                    "trace job belongs to another project"
                );
                return Ok(row);
            }
        }
        Ok(Value::Null)
    }
    fn trace_job_summary(&self, job: &Value, attempts: &[Value], replies: &[Value]) -> Value {
        let mut out = pick(
            job,
            &[
                "job_id",
                "agent_name",
                "provider",
                "status",
                "submission_id",
                "created_at",
                "updated_at",
            ],
        );
        if out.is_null() {
            return out;
        }
        out["business_status"] = job["status"].clone();
        out["message_type"] = job["request"]["message_type"].clone();
        out["terminal_reason"] = job["terminal_decision"]["reason"].clone();
        out["visible_reply_id"] = Value::Null;
        if job["terminal_decision"]["delegated"] == true {
            let a = attempts.iter().find(|a| a["job_id"] == job["job_id"]);
            let final_reply = a.and_then(|a| {
                replies.iter().rev().find(|r| {
                    r["message_id"] == a["message_id"]
                        && r["diagnostics"]["notice"] != true
                        && r["diagnostics"]["notice_kind"]
                            .as_str()
                            .is_none_or(str::is_empty)
                })
            });
            out["business_status"] = final_reply
                .map(|r| r["terminal_status"].clone())
                .unwrap_or(json!("waiting_callback"));
            out["visible_reply_id"] = final_reply
                .map(|r| r["reply_id"].clone())
                .unwrap_or(Value::Null);
        }
        out
    }
    pub(super) fn trace(&mut self, p: &Value) -> Result<Value> {
        let target = crate::records::string(p, "target")?.trim();
        ensure!(
            target.len() <= 256
                && target
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"_-".contains(&b)),
            "invalid trace target"
        );
        let kind = match target.split('_').next().unwrap_or("") {
            "sub" => "submission",
            "msg" => "message",
            "att" => "attempt",
            "rep" => "reply",
            "job" => "job",
            _ => anyhow::bail!("trace requires a submission, message, attempt, reply or job ID"),
        };
        let limit = p["limit"].as_u64().unwrap_or(100);
        let offset = p["offset"].as_u64().unwrap_or(0);
        ensure!(
            (1..=1000).contains(&limit)
                && (p["limit"].is_null() || p["limit"].is_u64())
                && (p["offset"].is_null() || p["offset"].is_u64())
                && offset <= 1000000,
            "invalid trace page"
        );
        let messages = self
            .kernel
            .store
            .latest_rows("messages/messages.jsonl", "message_id")?;
        let mut attempts = self
            .kernel
            .store
            .latest_rows("attempts/attempts.jsonl", "attempt_id")?;
        attempts.sort_by_key(|a| {
            (
                a["retry_index"].as_u64().unwrap_or(0),
                id(a, "started_at"),
                id(a, "attempt_id"),
            )
        });
        let mut replies = self
            .kernel
            .store
            .latest_rows("replies/replies.jsonl", "reply_id")?;
        replies.sort_by_key(|r| (id(r, "finished_at"), id(r, "reply_id")));
        let mut selected_job = Value::Null;
        let mut selected_attempt = Value::Null;
        let mut selected_reply = Value::Null;
        let mut selected_message = Value::Null;
        let mut selected_submission = Value::Null;
        match kind {
            "submission" => {
                selected_submission =
                    self.kernel
                        .store
                        .latest_by_id("submissions.jsonl", "submission_id", target)?
            }
            "message" => selected_message = find(&messages, "message_id", target),
            "attempt" => selected_attempt = find(&attempts, "attempt_id", target),
            "reply" => {
                selected_reply = find(&replies, "reply_id", target);
                selected_attempt =
                    find(&attempts, "attempt_id", &id(&selected_reply, "attempt_id"));
            }
            _ => {
                selected_job = self.trace_job_record(target)?;
                selected_attempt = find(&attempts, "job_id", target);
            }
        }
        let found = match kind {
            "submission" => &selected_submission,
            "message" => &selected_message,
            "attempt" => &selected_attempt,
            "reply" => &selected_reply,
            _ => &selected_job,
        };
        ensure!(!found.is_null(), "trace target not found: {target}");
        if ["job", "attempt", "reply"].contains(&kind) {
            ensure!(!selected_attempt.is_null(), "trace attempt link missing");
            selected_message = find(
                &messages,
                "message_id",
                &id(&selected_attempt, "message_id"),
            );
            ensure!(!selected_message.is_null(), "trace message link missing");
        }
        if !selected_message["submission_id"].is_null() {
            selected_submission = self.kernel.store.latest_by_id(
                "submissions.jsonl",
                "submission_id",
                &id(&selected_message, "submission_id"),
            )?;
        }
        ensure!(
            selected_submission.is_null()
                || selected_submission["project_id"] == self.config["project_id"],
            "trace submission belongs to another project"
        );
        let selected_messages: Vec<_> = if kind == "submission" {
            messages
                .iter()
                .filter(|m| m["submission_id"] == selected_submission["submission_id"])
                .cloned()
                .collect()
        } else {
            vec![selected_message.clone()]
        };
        let mids: BTreeSet<_> = selected_messages
            .iter()
            .map(|m| id(m, "message_id"))
            .collect();
        let selected_attempts: Vec<_> = if selected_attempt.is_null() {
            attempts
                .iter()
                .filter(|a| member(&mids, &a["message_id"]))
                .cloned()
                .collect()
        } else {
            vec![selected_attempt.clone()]
        };
        let aids: BTreeSet<_> = selected_attempts
            .iter()
            .map(|a| id(a, "attempt_id"))
            .collect();
        let selected_replies: Vec<_> = if !selected_reply.is_null() {
            vec![selected_reply.clone()]
        } else {
            replies
                .iter()
                .filter(|r| {
                    member(&mids, &r["message_id"])
                        && (selected_attempt.is_null() || member(&aids, &r["attempt_id"]))
                })
                .cloned()
                .collect()
        };
        let mut jobs = BTreeMap::new();
        let mut missing = BTreeSet::new();
        let ordered_ids: Vec<String> = if kind == "submission" {
            selected_submission["job_ids"]
                .as_array()
                .map(|ids| {
                    ids.iter()
                        .filter_map(Value::as_str)
                        .map(str::to_owned)
                        .collect()
                })
                .unwrap_or_default()
        } else {
            selected_attempts.iter().map(|a| id(a, "job_id")).collect()
        };
        for jid in &ordered_ids {
            let job = self.trace_job_record(jid)?;
            if job.is_null() {
                missing.insert(jid.clone());
            } else {
                jobs.insert(jid.clone(), job);
            }
        }
        let job_summaries: Vec<_> = ordered_ids
            .iter()
            .filter_map(|j| jobs.get(j))
            .map(|j| self.trace_job_summary(j, &attempts, &replies))
            .collect();
        let delivery = selected_job["provider_options"]["reply_delivery"] == true;
        let source_reply = if delivery {
            find(
                &replies,
                "reply_id",
                &id(&selected_job["provider_options"], "reply_delivery_reply_id"),
            )
        } else {
            Value::Null
        };
        ensure!(
            !delivery || !source_reply.is_null(),
            "trace delivery source reply missing"
        );
        if delivery {
            let source_attempt = find(&attempts, "attempt_id", &id(&source_reply, "attempt_id"));
            ensure!(
                !source_attempt.is_null(),
                "trace delivery source attempt missing"
            );
            self.trace_job_record(&id(&source_attempt, "job_id"))?;
        }
        let mut delivery_event = Value::Null;
        let mut events = Vec::new();
        for agent in &self.known_names {
            let mailbox = self
                .kernel
                .store
                .json(&format!("mailboxes/{agent}/mailbox.json"))?
                .unwrap_or(Value::Null);
            for event in self.kernel.store.latest_rows(
                &format!("mailboxes/{agent}/inbox.jsonl"),
                "inbound_event_id",
            )? {
                let delivery_match = delivery
                    && event["inbound_event_id"]
                        == selected_job["provider_options"]["reply_delivery_inbound_event_id"];
                if delivery_match
                    || member(&mids, &event["message_id"])
                        && (selected_attempt.is_null() || member(&aids, &event["attempt_id"]))
                {
                    let mut e = pick(
                        &event,
                        &[
                            "inbound_event_id",
                            "agent_name",
                            "event_type",
                            "message_id",
                            "attempt_id",
                            "payload_ref",
                            "priority",
                            "status",
                            "created_at",
                            "started_at",
                            "finished_at",
                        ],
                    );
                    e["mailbox_state"] = mailbox["mailbox_state"].clone();
                    e["mailbox_active"] = json!(
                        !mailbox.is_null()
                            && mailbox["active_inbound_event_id"] == event["inbound_event_id"]
                    );
                    if delivery_match {
                        delivery_event = e.clone();
                    }
                    events.push(e);
                }
            }
        }
        events.sort_by_key(|e| (id(e, "created_at"), id(e, "inbound_event_id")));
        let edges = self
            .kernel
            .store
            .latest_rows("callbacks/edges.jsonl", "edge_id")?;
        let mut links = BTreeMap::new();
        let mut job_ids: BTreeSet<_> = jobs.keys().cloned().collect();
        loop {
            let before = job_ids.len();
            for e in &edges {
                if ["parent_job_id", "child_job_id", "continuation_job_id"]
                    .iter()
                    .any(|k| member(&job_ids, &e[*k]))
                {
                    for key in ["parent_job_id", "child_job_id", "continuation_job_id"] {
                        add(&mut job_ids, &e[key]);
                    }
                    links.insert(
                        id(e, "edge_id"),
                        pick(
                            e,
                            &[
                                "edge_id",
                                "state",
                                "parent_job_id",
                                "child_job_id",
                                "continuation_job_id",
                                "parent_message_id",
                                "child_message_id",
                                "child_reply_id",
                                "parent_agent",
                                "child_agent",
                                "callback_target_agent",
                                "original_caller",
                                "child_status",
                                "created_at",
                                "updated_at",
                            ],
                        ),
                    );
                }
            }
            ensure!(
                job_ids.len() <= 4096 && links.len() <= 4096,
                "trace callback graph exceeds limit"
            );
            if before == job_ids.len() {
                break;
            }
        }
        let mut callback_jobs = Vec::new();
        if !links.is_empty() {
            for jid in job_ids {
                let j = self.trace_job_record(&jid)?;
                if j.is_null() {
                    missing.insert(jid);
                } else {
                    callback_jobs.push(self.trace_job_summary(&j, &attempts, &replies));
                }
            }
        }
        let mid = if !selected_message.is_null() {
            selected_message["message_id"].clone()
        } else if selected_messages.len() == 1 {
            selected_messages[0]["message_id"].clone()
        } else {
            Value::Null
        };
        let mut out = json!({"target":target,"resolved_kind":kind,"submission_id":selected_submission["submission_id"],"message_id":mid,"attempt_id":selected_attempt["attempt_id"],"reply_id":selected_reply["reply_id"],"job_id":selected_job["job_id"],
            "submission":submission(&selected_submission),"message":message(&selected_message),"attempt":attempt(&selected_attempt),"reply":reply(&selected_reply),"job":self.trace_job_summary(&selected_job,&attempts,&replies),
            "message_count":selected_messages.len(),"attempt_count":selected_attempts.len(),"reply_count":selected_replies.len(),"event_count":events.len(),"job_count":job_summaries.len(),"missing_job_ids":missing,"offset":offset,"limit":limit,
            "job_role":selected_job["request"]["message_type"],"source_reply":reply(&source_reply),"delivery_event":delivery_event});
        let groups = [
            ("messages", selected_messages.iter().map(message).collect()),
            ("attempts", selected_attempts.iter().map(attempt).collect()),
            ("replies", selected_replies.iter().map(reply).collect()),
            ("events", events),
            ("jobs", job_summaries),
            ("callbacks", links.into_values().collect()),
            ("callback_jobs", callback_jobs),
        ];
        let mut truncated = false;
        for (key, rows) in groups {
            truncated |= page(&mut out, key, rows, offset as usize, limit as usize);
        }
        out["truncated"] = json!(truncated);
        out["next_offset"] = if truncated {
            json!(offset + limit)
        } else {
            Value::Null
        };
        ensure!(
            out.to_string().len() <= 900 * 1024,
            "trace response exceeds transport budget; reduce --limit"
        );
        Ok(out)
    }
}
