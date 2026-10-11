//! Reply-route snapshots. Waiting belongs to the CLI, never to the actor loop.
use super::Daemon;
use anyhow::{ensure, Context, Result};
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet};
fn text(row: &Value, key: &str) -> String {
    row[key].as_str().unwrap_or("").to_owned()
}
fn attempt_key(a: &Value) -> (u64, String, String) {
    (
        a["retry_index"].as_u64().unwrap_or(0),
        text(a, "updated_at"),
        text(a, "attempt_id"),
    )
}
fn reply_key(r: &Value) -> (String, String) {
    (text(r, "finished_at"), text(r, "reply_id"))
}
impl Daemon {
    pub(super) fn wait_summary(&self, p: &Value) -> Result<Value> {
        let target = crate::records::string(p, "target")?.trim();
        let mode = crate::records::string(p, "mode")?;
        ensure!(
            ["any", "all", "quorum"].contains(&mode),
            "invalid wait mode"
        );
        ensure!(
            mode == "quorum" || p["quorum"].is_null(),
            "quorum is only valid for wait-quorum"
        );
        ensure!(
            target.len() <= 256
                && target
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"_-".contains(&b)),
            "invalid wait target"
        );
        let kind = match target.split('_').next().unwrap_or("") {
            "sub" => "submission",
            "msg" => "message",
            "att" => "attempt",
            "rep" => "reply",
            "job" => "job",
            _ => anyhow::bail!("wait requires a submission, message, attempt, reply or job ID"),
        };
        let attempts = self
            .kernel
            .store
            .latest_rows("attempts/attempts.jsonl", "attempt_id")?;
        let replies = self
            .kernel
            .store
            .latest_rows("replies/replies.jsonl", "reply_id")?;
        let mut message_ids = BTreeSet::new();
        let mut pinned_attempt = None;
        let mut pinned_reply = None;
        if kind == "submission" {
            let sub =
                self.kernel
                    .store
                    .latest_by_id("submissions.jsonl", "submission_id", target)?;
            ensure!(
                !sub.is_null() && sub["project_id"] == self.config["project_id"],
                "wait submission missing or belongs to another project"
            );
            for m in self
                .kernel
                .store
                .latest_rows("messages/messages.jsonl", "message_id")?
            {
                if m["submission_id"] == target {
                    message_ids.insert(text(&m, "message_id"));
                }
            }
        } else if kind == "message" {
            let m =
                self.kernel
                    .store
                    .latest_by_id("messages/messages.jsonl", "message_id", target)?;
            ensure!(!m.is_null(), "wait message not found");
            message_ids.insert(target.into());
        } else {
            let attempt = if kind == "job" {
                let job = self.trace_job_record(target)?;
                ensure!(!job.is_null(), "wait job not found");
                let a = attempts
                    .iter()
                    .find(|a| a["job_id"] == target)
                    .context("wait job has no attempt route")?;
                if job["terminal_decision"]["delegated"] != true {
                    pinned_attempt = Some(text(a, "attempt_id"));
                }
                a
            } else if kind == "reply" {
                let r = replies
                    .iter()
                    .find(|r| r["reply_id"] == target)
                    .context("wait reply not found")?;
                pinned_reply = Some(target.to_owned());
                pinned_attempt = Some(text(r, "attempt_id"));
                attempts
                    .iter()
                    .find(|a| a["attempt_id"] == r["attempt_id"])
                    .context("wait reply has no attempt route")?
            } else {
                let a = attempts
                    .iter()
                    .find(|a| a["attempt_id"] == target)
                    .context("wait attempt not found")?;
                pinned_attempt = Some(target.to_owned());
                a
            };
            message_ids.insert(text(attempt, "message_id"));
        }
        let mut routes: BTreeMap<(String, String), &Value> = BTreeMap::new();
        for a in &attempts {
            if !message_ids.contains(&text(a, "message_id"))
                || pinned_attempt
                    .as_ref()
                    .is_some_and(|id| a["attempt_id"] != *id)
            {
                continue;
            }
            let job = self.trace_job_record(&text(a, "job_id"))?;
            ensure!(!job.is_null(), "wait route job is missing");
            let key = (text(a, "message_id"), text(a, "agent_name"));
            if routes
                .get(&key)
                .is_none_or(|previous| attempt_key(a) > attempt_key(previous))
            {
                routes.insert(key, a);
            }
        }
        ensure!(!routes.is_empty(), "wait target has no attempt routes");
        let expected = routes.len();
        let quorum = match mode {
            "any" => 1,
            "all" => expected,
            _ => p["quorum"]
                .as_u64()
                .filter(|v| *v > 0 && *v <= expected as u64)
                .context("wait quorum must be positive and cannot exceed available routes")?
                as usize,
        };
        let mut by_attempt: BTreeMap<String, &Value> = BTreeMap::new();
        for r in &replies {
            if pinned_reply.as_ref().is_some_and(|id| r["reply_id"] != *id) {
                continue;
            }
            let aid = text(r, "attempt_id");
            if by_attempt
                .get(&aid)
                .is_none_or(|old| reply_key(r) > reply_key(old))
            {
                by_attempt.insert(aid, r);
            }
        }
        let mut selected = Vec::new();
        let mut notices = 0;
        for a in routes.values() {
            if let Some(r) = by_attempt.get(&text(a, "attempt_id")) {
                ensure!(
                    r["message_id"] == a["message_id"] && r["agent_name"] == a["agent_name"],
                    "wait reply route identity mismatch"
                );
                let projected = super::trace::reply(r);
                let mut item = json!({});
                for key in [
                    "reply_id",
                    "message_id",
                    "attempt_id",
                    "agent_name",
                    "terminal_status",
                    "notice",
                    "notice_kind",
                    "last_progress_at",
                    "heartbeat_silence_seconds",
                    "reason",
                    "finished_at",
                    "reply",
                    "reply_artifact",
                    "reply_size",
                    "reply_truncated",
                ] {
                    item[key] = projected[key].clone();
                }
                item["job_id"] = a["job_id"].clone();
                if item["notice"] == true {
                    notices += 1;
                }
                selected.push(item);
            }
        }
        selected.sort_by_key(reply_key);
        let received = selected.len();
        let terminal = received - notices;
        let satisfied = received >= quorum;
        let result = json!({"wait_status":if !satisfied {"pending"}else if terminal>=quorum {"satisfied"}else{"notice"},"satisfied":satisfied,
            "project_id":self.config["project_id"],"mode":mode,"target":target,"resolved_kind":kind,"expected_count":expected,"received_count":received,
            "terminal_count":terminal,"notice_count":notices,"quorum":quorum,"waited_s":0.0,"replies":selected});
        ensure!(
            result.to_string().len() <= 900 * 1024,
            "wait response exceeds transport budget; inspect individual trace routes"
        );
        Ok(result)
    }
}
