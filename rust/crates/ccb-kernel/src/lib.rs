mod admission;
mod artifacts;
mod bureau;
mod callback;
mod callback_policy;
mod callback_recovery;
mod claude_stream;
pub mod cli;
mod codex_requests;
mod codex_rpc;
mod completion;
mod completion_gate;
pub mod config;
pub mod control_plane;
pub mod daemon;
mod executables;
mod execution_recovery;
mod finalization;
mod lifecycle;
pub mod models;
mod owned_process;
mod pi_rpc;
mod provider_fault;
mod records;
mod result_policy;
mod retry;
pub mod rpc;
mod scheduler;
pub mod security;
mod startup;
pub mod store;

use anyhow::{bail, ensure, Result};
use chrono::{DateTime, Duration, NaiveDateTime, SecondsFormat, Utc};
use models::{validate_agent, Event, EventStatus, EventType, Lease, LeaseState};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::Path;
use store::Store;

pub const API_VERSION: u32 = 1;
pub struct Kernel {
    pub store: Store,
    scheduler: scheduler::Scheduler,
    completion: completion::Completion,
}
impl Kernel {
    pub(crate) fn memory_checkpoint(&self) -> (scheduler::Scheduler, completion::Completion) {
        (self.scheduler.clone(), self.completion.clone())
    }
    pub(crate) fn restore_memory(
        &mut self,
        checkpoint: (scheduler::Scheduler, completion::Completion),
    ) {
        self.scheduler = checkpoint.0;
        self.completion = checkpoint.1;
    }
    pub fn open(root: &Path) -> Result<Self> {
        Ok(Self {
            store: Store::open(root)?,
            scheduler: scheduler::Scheduler::default(),
            completion: completion::Completion::default(),
        })
    }
    pub fn dispatch(&mut self, op: &str, args: &Value) -> Result<Value> {
        if let Some(method) = op.strip_prefix("daemon_lease.") {
            return lifecycle::dispatch(self, method, args);
        }
        if op == "daemon_ownership.inspect" {
            return lifecycle::ownership(args);
        }
        if op == "daemon_ownership.verify_claim" || op == "daemon_ownership.expected_claim" {
            let current = self.store.json("lease.json")?.unwrap_or(Value::Null);
            let inspected = &args["lease"];
            let same = current.is_null() && inspected.is_null()
                || !current.is_null()
                    && !inspected.is_null()
                    && [
                        "project_id",
                        "ccbd_pid",
                        "socket_path",
                        "mount_state",
                        "last_heartbeat_at",
                        "daemon_instance_id",
                    ]
                    .iter()
                    .all(|key| current[*key] == inspected[*key])
                    && current["generation"].as_u64().unwrap_or(1)
                        == inspected["generation"].as_u64().unwrap_or(1);
            ensure!(same, "ccbd lease changed during ownership inspection");
            return lifecycle::claim(args, op == "daemon_ownership.expected_claim");
        }
        if op == "completion.gate" {
            return completion_gate::validate(args);
        }
        if let Some(method) = op.strip_prefix("completion_snapshot.") {
            return completion::snapshot(self, method, args);
        }
        if let Some(method) = op.strip_prefix("completion.") {
            return self.completion.dispatch(method, args);
        }
        match op {
            "dispatcher.submit" => return admission::submit(self, args),
            "dispatcher.retry" => return retry::admit(self, args),
            "retry.source" => return retry::source(self, records::string(args, "target")?),
            "result.plan" => return result_policy::plan(self, args),
            "result.record" => return result_policy::record(self, args),
            "dispatcher.persist_terminal" => return finalization::persist(self, args, false),
            "dispatcher.revise_reply" => return finalization::persist(self, args, true),
            "artifact.spill_completion" => {
                return artifacts::spill_completion(
                    self,
                    &args["job"],
                    &args["decision"],
                    records::string(args, "timestamp")?,
                )
            }
            "callback.validate" => return callback_policy::validate(self, args),
            "bureau.submit" => return bureau::submission(self, args),
            "bureau.retry" => return bureau::retry(self, args),
            "bureau.reply" => return bureau::reply(self, args),
            "bureau.terminal" => return bureau::terminal(self, args),
            "bureau.queue_reply" => return bureau::queue_reply(self, args),
            "callback.prepare" => return callback::prepare(self, args),
            "callback.enqueue" => return callback::enqueue(self, args),
            "callback.done" => return callback::done(self, args),
            "callback.repair" => return callback_recovery::repair(self, args),
            "bureau.attempt_started" => return bureau::attempt_transition(self, "started", args),
            "bureau.attempt_terminal" => return bureau::attempt_transition(self, "terminal", args),
            _ => {}
        }
        if let Some(method) = op.strip_prefix("records.") {
            return records::dispatch(&mut self.store, method, args);
        }
        if let Some(method) = op.strip_prefix("message.") {
            ensure!(
                ["set", "refresh"].contains(&method),
                "unsupported message operation"
            );
            return records::message_transition(&mut self.store, method, args);
        }
        if op == "callback.update" {
            return records::callback_update(&mut self.store, args);
        }
        if let Some(method) = op.strip_prefix("scheduler.") {
            return self.scheduler.dispatch(method, args);
        }
        if op == "ping" {
            return Ok(
                json!({"service":"ccb-kernel", "api_version":API_VERSION, "pid":std::process::id(),
                    "capabilities":["mailbox","scheduler","records","bureau","callback","completion","control_plane","lifecycle","admission","finalization","callback_recovery","result_policy","retry"]}),
            );
        }
        if op == "lease.list" || op == "mailbox.list" {
            let category = if op == "lease.list" {
                "leases"
            } else {
                "mailboxes"
            };
            let mut records = Vec::new();
            for path in self.store.list(category)? {
                // Match legacy list_all: bad individual records do not prevent inspection.
                if let Ok(Some(record)) = self.store.json(&path) {
                    if op == "lease.list" {
                        if let Ok(lease) = serde_json::from_value::<Lease>(record.clone()) {
                            if lease.validate().is_ok() {
                                records.push(record);
                            }
                        }
                    } else if validate_mailbox(&record).is_ok() {
                        records.push(record);
                    }
                }
            }
            return Ok(json!(records));
        }
        if op == "expired_delivery_leases" {
            let now = parse_time(text(args, "now")?);
            let mut expired = Vec::new();
            if let Some(now) = now {
                for value in self.dispatch("lease.list", &json!({}))?.as_array().unwrap() {
                    let lease: Lease = serde_json::from_value(value.clone())?;
                    let deadline = lease
                        .expires_at
                        .as_deref()
                        .and_then(parse_time)
                        .or_else(|| {
                            ttl(args).and_then(|seconds| {
                                lease
                                    .last_progress_at
                                    .as_deref()
                                    .and_then(parse_time)
                                    .or_else(|| parse_time(&lease.acquired_at))
                                    .map(|base| base + duration(seconds))
                            })
                        });
                    if lease.lease_state == LeaseState::Acquired
                        && deadline.is_some_and(|d| now >= d)
                    {
                        expired.push(value.clone());
                    }
                }
            }
            return Ok(json!(expired));
        }
        let agent = text(args, "agent_name")?.trim().to_ascii_lowercase();
        validate_agent(&agent)?;
        match op {
            "inbox.append" => {
                let record = args["record"].clone();
                let event: Event = serde_json::from_value(record.clone())?;
                event.validate()?;
                ensure!(event.agent_name == agent, "event owner mismatch");
                self.store.append(&inbox(&agent), &record)?;
                Ok(Value::Null)
            }
            "inbox.list" => Ok(json!(self.store.rows(&inbox(&agent))?)),
            "inbox.read_since" => {
                let start = args["start_line"]
                    .as_u64()
                    .ok_or_else(|| anyhow::anyhow!("invalid start_line"))?
                    as usize;
                let bytes = self.store.bytes(&inbox(&agent))?;
                let Some(bytes) = bytes else {
                    return Ok(json!({"line_no":start,"rows":[]}));
                };
                let lines: Vec<_> = std::str::from_utf8(&bytes)?
                    .lines()
                    .filter(|line| !line.trim().is_empty())
                    .collect();
                let rows: Vec<Value> = lines
                    .iter()
                    .skip(start)
                    .map(|line| {
                        let row: Value = serde_json::from_str(line)?;
                        ensure!(row.is_object(), "expected JSON object row");
                        Ok(row)
                    })
                    .collect::<Result<_>>()?;
                Ok(json!({"line_no":lines.len(),"rows":rows}))
            }
            "inbox.latest" => {
                let field = text(args, "field")?;
                ensure!(
                    ["inbound_event_id", "attempt_id"].contains(&field),
                    "invalid event lookup field"
                );
                let expected = text(args, "value")?;
                let bytes = self.store.bytes(&inbox(&agent))?.unwrap_or_default();
                for line in std::str::from_utf8(&bytes)?
                    .lines()
                    .rev()
                    .filter(|line| !line.trim().is_empty())
                {
                    let row: Value = serde_json::from_str(line)?;
                    ensure!(row.is_object(), "expected JSON object row");
                    if row[field].as_str().unwrap_or("") == expected {
                        return Ok(row);
                    }
                }
                Ok(Value::Null)
            }
            "mailbox.load" => Ok(self.store.json(&mailbox(&agent))?.unwrap_or(Value::Null)),
            "mailbox.save" | "mailbox.compare_and_save" => {
                validate_mailbox(&args["record"])?;
                ensure!(
                    args["record"]["agent_name"] == agent,
                    "mailbox owner mismatch"
                );
                if op == "mailbox.compare_and_save" {
                    let current = self.store.json(&mailbox(&agent))?;
                    let version = current
                        .as_ref()
                        .map(|v| v["summary_version"].clone())
                        .unwrap_or(Value::Null);
                    if version != args["expected_summary_version"] {
                        return Ok(json!(false));
                    }
                }
                self.store.save(&mailbox(&agent), &args["record"])?;
                Ok(if op == "mailbox.compare_and_save" {
                    json!(true)
                } else {
                    Value::Null
                })
            }
            "lease.load" => Ok(self.store.json(&lease_path(&agent))?.unwrap_or(Value::Null)),
            "lease.save" => {
                let lease: Lease = serde_json::from_value(args["record"].clone())?;
                lease.validate()?;
                ensure!(lease.agent_name == agent, "lease owner mismatch");
                self.store.save(&lease_path(&agent), &args["record"])?;
                Ok(Value::Null)
            }
            "lease.remove" => {
                self.store.remove(&lease_path(&agent))?;
                Ok(Value::Null)
            }
            "latest_events" | "pending_events" | "head_pending_event" | "peek_next" => {
                let mut events = self.events(&agent)?;
                if op != "latest_events" {
                    events.retain(|e| !e.status.terminal());
                }
                if op == "head_pending_event" || op == "peek_next" {
                    let head = events.first();
                    let event_type = args.get("event_type").filter(|v| !v.is_null());
                    let selected = head.filter(|e| {
                        op == "head_pending_event"
                            || (e.status.claimable()
                                && event_type.is_none_or(|t| {
                                    serde_json::to_value(e.event_type).unwrap() == *t
                                }))
                    });
                    Ok(serde_json::to_value(selected)?)
                } else {
                    if op == "pending_events" {
                        if let Some(t) = args.get("event_type").filter(|v| !v.is_null()) {
                            let kind: EventType = serde_json::from_value(t.clone())?;
                            events.retain(|e| e.event_type == kind);
                        }
                    }
                    Ok(serde_json::to_value(events)?)
                }
            }
            "claim" | "claim_next" => {
                let timestamp = text(args, "timestamp")?;
                let id = if op == "claim_next" {
                    let head = self.dispatch("peek_next", args)?;
                    if head.is_null() {
                        self.rebuild(&agent, timestamp)?;
                        return Ok(Value::Null);
                    }
                    head["inbound_event_id"].as_str().unwrap().to_owned()
                } else {
                    text(args, "inbound_event_id")?.to_owned()
                };
                self.claim(&agent, &id, timestamp, args)
            }
            "consume" | "abandon" | "supersede" => {
                let status = match op {
                    "consume" => EventStatus::Consumed,
                    "abandon" => EventStatus::Abandoned,
                    _ => EventStatus::Superseded,
                };
                self.terminal(
                    &agent,
                    text(args, "inbound_event_id")?,
                    status,
                    text(args, "timestamp")?,
                )
            }
            "ack_reply" => self.ack_reply(&agent, args),
            "rewrite_head" => self.rewrite(&agent, args),
            "expire_lease" => self.expire(&agent, text(args, "timestamp")?),
            "project_mailbox_summary" => {
                let prior = if let Some(prior) = args.get("prior") {
                    prior.clone()
                } else {
                    self.prior(&agent)?
                };
                self.history_summary(&agent, &prior, text(args, "timestamp")?)
            }
            "rebuild_mailbox_summary" | "refresh_mailbox" => {
                self.rebuild(&agent, text(args, "timestamp")?)
            }
            "apply_incremental_summary_update" | "upsert_mailbox_summary" => {
                self.incremental(&agent, args)
            }
            "apply_transition_summary_update" => self.transition(&agent, args),
            _ => bail!("unsupported kernel operation: {op}"),
        }
    }
    fn events(&self, agent: &str) -> Result<Vec<Event>> {
        let mut indices = HashMap::new();
        let mut events = Vec::new();
        for row in self.store.rows(&inbox(agent))? {
            let event: Event = serde_json::from_value(row)?;
            event.validate()?;
            ensure!(event.agent_name == agent, "stored event owner mismatch");
            if let Some(index) = indices.get(&event.inbound_event_id) {
                events[*index] = event;
            } else {
                indices.insert(event.inbound_event_id.clone(), events.len());
                events.push(event);
            }
        }
        Ok(events)
    }
    fn prior(&self, agent: &str) -> Result<Value> {
        Ok(self.store.json(&mailbox(agent))?.unwrap_or(Value::Null))
    }
    fn lease(&self, agent: &str) -> Result<Option<Lease>> {
        self.store
            .json(&lease_path(agent))?
            .map(|v| {
                let lease: Lease = serde_json::from_value(v)?;
                lease.validate()?;
                Ok(lease)
            })
            .transpose()
    }
    fn append_event(&mut self, agent: &str, event: &Event) -> Result<()> {
        self.store
            .append(&inbox(agent), &serde_json::to_value(event)?)
    }
    fn active(&self, agent: &str) -> Result<Value> {
        Ok(self
            .lease(agent)?
            .filter(|l| l.lease_state == LeaseState::Acquired)
            .map(|l| json!(l.inbound_event_id))
            .unwrap_or(Value::Null))
    }
    fn history_summary(&self, agent: &str, prior: &Value, timestamp: &str) -> Result<Value> {
        let events = self.events(agent)?;
        let pending: Vec<&Event> = events.iter().filter(|e| !e.status.terminal()).collect();
        let mut started = prior["last_inbound_started_at"].clone();
        let mut finished = prior["last_inbound_finished_at"].clone();
        for event in &events {
            started = latest(&started, &json!(event.started_at));
            finished = latest(&finished, &json!(event.finished_at));
        }
        self.summary(
            agent,
            prior,
            pending.len() as u64,
            pending
                .iter()
                .filter(|e| e.event_type == EventType::TaskReply)
                .count() as u64,
            self.active(agent)?,
            started,
            finished,
            pending.first().map(|e| head(e)).unwrap_or_else(empty_head),
            timestamp,
            "history-refresh",
        )
    }
    fn rebuild(&mut self, agent: &str, timestamp: &str) -> Result<Value> {
        let result = self.history_summary(agent, &self.prior(agent)?, timestamp)?;
        self.store.save(&mailbox(agent), &result)?;
        Ok(result)
    }
    #[allow(clippy::too_many_arguments)]
    fn summary(
        &self,
        agent: &str,
        prior: &Value,
        depth: u64,
        replies: u64,
        active: Value,
        started: Value,
        finished: Value,
        head: Value,
        timestamp: &str,
        source: &str,
    ) -> Result<Value> {
        let lease = self.lease(agent)?;
        let acquired = lease
            .as_ref()
            .is_some_and(|l| l.lease_state == LeaseState::Acquired);
        let state = if acquired || !active.is_null() {
            "delivering"
        } else if depth > 0 {
            "blocked"
        } else {
            "idle"
        };
        let lease_version = lease
            .as_ref()
            .map(|l| l.lease_version)
            .unwrap_or_else(|| number(prior, "lease_version"));
        let mut record = json!({"schema_version":1, "record_type":"mailbox_record", "mailbox_id":prior.get("mailbox_id").cloned().unwrap_or_else(|| json!(format!("mbx_{agent}"))),
            "agent_name":agent, "summary_version":number(prior,"summary_version"), "summary_source":source, "summary_refreshed_at":timestamp,
            "active_inbound_event_id":active, "queue_depth":depth, "pending_reply_count":replies,
            "last_inbound_started_at":started, "last_inbound_finished_at":finished, "mailbox_state":state, "lease_version":lease_version, "updated_at":timestamp});
        for (key, value) in head.as_object().unwrap() {
            record[key] = value.clone();
        }
        let unchanged = !prior.is_null()
            && [
                "active_inbound_event_id",
                "queue_depth",
                "pending_reply_count",
                "head_inbound_event_id",
                "head_event_type",
                "head_status",
                "head_message_id",
                "head_attempt_id",
                "head_payload_ref",
                "last_inbound_started_at",
                "last_inbound_finished_at",
                "mailbox_state",
                "lease_version",
            ]
            .iter()
            .all(|key| record[*key] == prior[*key]);
        if !unchanged {
            record["summary_version"] = json!(number(prior, "summary_version") + 1);
        }
        validate_mailbox(&record)?;
        Ok(record)
    }
    fn resolved_head(&self, agent: &str) -> Result<Value> {
        Ok(self
            .events(agent)?
            .iter()
            .find(|e| !e.status.terminal())
            .map(head)
            .unwrap_or_else(empty_head))
    }
    fn claim(&mut self, agent: &str, id: &str, timestamp: &str, args: &Value) -> Result<Value> {
        let events = self.events(agent)?;
        let current = events
            .iter()
            .find(|e| e.inbound_event_id == id && !e.status.terminal());
        let conflicting = self
            .lease(agent)?
            .is_some_and(|l| l.lease_state == LeaseState::Acquired && l.inbound_event_id != id);
        if current.is_none() || conflicting {
            self.rebuild(agent, timestamp)?;
            return Ok(Value::Null);
        }
        let current = current.unwrap();
        if current.status == EventStatus::Delivering {
            self.rebuild(agent, timestamp)?;
            return Ok(serde_json::to_value(current)?);
        }
        let is_head = events
            .iter()
            .find(|e| !e.status.terminal())
            .is_some_and(|e| e.inbound_event_id == id);
        if !current.status.claimable() || !is_head {
            self.rebuild(agent, timestamp)?;
            return Ok(Value::Null);
        }
        let mut updated = current.clone();
        updated.status = EventStatus::Delivering;
        updated.started_at = updated.started_at.or_else(|| Some(timestamp.into()));
        updated.finished_at = None;
        let prior = self.prior(agent)?;
        let version = self
            .lease(agent)?
            .map(|l| l.lease_version)
            .unwrap_or_else(|| number(&prior, "lease_version"))
            + 1;
        self.append_event(agent, &updated)?;
        self.store.save(&lease_path(agent), &json!({"schema_version":1,"record_type":"delivery_lease", "agent_name":agent,"inbound_event_id":id,"lease_version":version,
            "acquired_at":timestamp,"last_progress_at":timestamp,"expires_at":ttl(args).and_then(|t| parse_time(timestamp).map(|base| format_time(base + duration(t)))),"lease_state":"acquired"}))?;
        if prior.is_null()
            || number(&prior, "queue_depth") == 0
            || prior["head_inbound_event_id"] != id
        {
            self.rebuild(agent, timestamp)?;
        } else {
            let result = self.summary(
                agent,
                &prior,
                number(&prior, "queue_depth"),
                number(&prior, "pending_reply_count"),
                json!(id),
                latest(
                    &prior["last_inbound_started_at"],
                    &json!(updated.started_at),
                ),
                prior["last_inbound_finished_at"].clone(),
                head(&updated),
                timestamp,
                "transition-claim",
            )?;
            self.store.save(&mailbox(agent), &result)?;
        }
        Ok(serde_json::to_value(updated)?)
    }
    fn release(&mut self, agent: &str, id: &str) -> Result<()> {
        if self
            .lease(agent)?
            .is_some_and(|l| l.lease_state == LeaseState::Acquired && l.inbound_event_id == id)
        {
            self.store.remove(&lease_path(agent))?;
        }
        Ok(())
    }
    fn terminal(
        &mut self,
        agent: &str,
        id: &str,
        status: EventStatus,
        timestamp: &str,
    ) -> Result<Value> {
        let current = self
            .events(agent)?
            .into_iter()
            .find(|e| e.inbound_event_id == id);
        let Some(mut updated) = current else {
            self.rebuild(agent, timestamp)?;
            return Ok(Value::Null);
        };
        if updated.status.terminal() {
            self.rebuild(agent, timestamp)?;
            return Ok(serde_json::to_value(updated)?);
        }
        let is_reply = updated.event_type == EventType::TaskReply;
        updated.status = status;
        updated.finished_at = Some(timestamp.into());
        self.append_event(agent, &updated)?;
        self.release(agent, id)?;
        let prior = self.prior(agent)?;
        if prior.is_null() || number(&prior, "queue_depth") == 0 {
            self.rebuild(agent, timestamp)?;
        } else {
            let depth = number(&prior, "queue_depth").saturating_sub(1);
            let next_head = if prior["head_inbound_event_id"] == id {
                if depth == 0 {
                    empty_head()
                } else {
                    self.resolved_head(agent)?
                }
            } else {
                prior_head(&prior)
            };
            if depth > 1 && next_head["head_inbound_event_id"].is_null() {
                self.rebuild(agent, timestamp)?;
            } else {
                let active = if prior["active_inbound_event_id"] == id {
                    Value::Null
                } else {
                    prior["active_inbound_event_id"].clone()
                };
                let result = self.summary(
                    agent,
                    &prior,
                    depth,
                    number(&prior, "pending_reply_count").saturating_sub(u64::from(is_reply)),
                    active,
                    prior["last_inbound_started_at"].clone(),
                    latest(&prior["last_inbound_finished_at"], &json!(timestamp)),
                    next_head,
                    timestamp,
                    "transition-terminal",
                )?;
                self.store.save(&mailbox(agent), &result)?;
            }
        }
        Ok(serde_json::to_value(updated)?)
    }
    fn ack_reply(&mut self, agent: &str, args: &Value) -> Result<Value> {
        let id = text(args, "inbound_event_id")?;
        let timestamp = text(args, "timestamp")?;
        let events = self.events(agent)?;
        let head = events.iter().find(|e| !e.status.terminal());
        if !head.is_some_and(|e| e.inbound_event_id == id && e.event_type == EventType::TaskReply) {
            self.rebuild(agent, timestamp)?;
            return Ok(Value::Null);
        }
        if head.unwrap().status != EventStatus::Delivering {
            let start = args["started_at"].as_str().unwrap_or(timestamp);
            if self.claim(agent, id, start, args)?.is_null() {
                self.rebuild(agent, timestamp)?;
                return Ok(Value::Null);
            }
        }
        self.terminal(
            agent,
            id,
            EventStatus::Consumed,
            args["finished_at"].as_str().unwrap_or(timestamp),
        )
    }
    fn rewrite(&mut self, agent: &str, args: &Value) -> Result<Value> {
        let id = text(args, "inbound_event_id")?;
        let timestamp = text(args, "timestamp")?;
        let current = self
            .events(agent)?
            .into_iter()
            .find(|e| e.inbound_event_id == id && !e.status.terminal());
        let Some(mut updated) = current else {
            self.rebuild(agent, timestamp)?;
            return Ok(Value::Null);
        };
        updated.payload_ref = args["payload_ref"].as_str().map(str::to_owned);
        updated.status = serde_json::from_value(args["status"].clone())?;
        if args["clear_progress"] == true {
            updated.started_at = None;
            updated.finished_at = None;
        }
        self.append_event(agent, &updated)?;
        self.release(agent, id)?;
        let prior = self.prior(agent)?;
        if prior.is_null()
            || prior["head_inbound_event_id"] != id
            || number(&prior, "queue_depth") == 0
        {
            self.rebuild(agent, timestamp)?;
        } else {
            let active = if prior["active_inbound_event_id"] == id {
                Value::Null
            } else {
                prior["active_inbound_event_id"].clone()
            };
            let result = self.summary(
                agent,
                &prior,
                number(&prior, "queue_depth"),
                number(&prior, "pending_reply_count"),
                active,
                prior["last_inbound_started_at"].clone(),
                prior["last_inbound_finished_at"].clone(),
                head(&updated),
                timestamp,
                "transition-rewrite-head",
            )?;
            self.store.save(&mailbox(agent), &result)?;
        }
        Ok(serde_json::to_value(updated)?)
    }
    fn expire(&mut self, agent: &str, timestamp: &str) -> Result<Value> {
        let lease = self
            .lease(agent)?
            .filter(|l| l.lease_state == LeaseState::Acquired);
        let Some(mut lease) = lease else {
            return Ok(Value::Null);
        };
        let current = self
            .events(agent)?
            .into_iter()
            .find(|e| e.inbound_event_id == lease.inbound_event_id);
        if current.is_none_or(|e| e.status.terminal()) {
            self.store.remove(&lease_path(agent))?;
            self.rebuild(agent, timestamp)?;
            return Ok(Value::Null);
        }
        lease.lease_state = LeaseState::Expired;
        self.store
            .save(&lease_path(agent), &serde_json::to_value(&lease)?)?;
        self.terminal(
            agent,
            &lease.inbound_event_id,
            EventStatus::Abandoned,
            timestamp,
        )
    }
    fn incremental(&mut self, agent: &str, args: &Value) -> Result<Value> {
        let prior = self.prior(agent)?;
        let depth = (number(&prior, "queue_depth") as i64
            + args["queue_delta"].as_i64().unwrap_or(0))
        .max(0) as u64;
        let replies = (number(&prior, "pending_reply_count") as i64
            + args["pending_reply_delta"].as_i64().unwrap_or(0))
        .max(0) as u64;
        let result = self.summary(
            agent,
            &prior,
            depth,
            replies,
            args.get("active_inbound_event_id")
                .cloned()
                .unwrap_or_else(|| prior["active_inbound_event_id"].clone()),
            latest(&prior["last_inbound_started_at"], &args["last_started_at"]),
            latest(
                &prior["last_inbound_finished_at"],
                &args["last_finished_at"],
            ),
            self.resolved_head(agent)?,
            text(args, "timestamp")?,
            "incremental-upsert",
        )?;
        self.store.save(&mailbox(agent), &result)?;
        Ok(result)
    }
    fn transition(&mut self, agent: &str, args: &Value) -> Result<Value> {
        let prior = self.prior(agent)?;
        let summary_head = args
            .get("summary_head")
            .cloned()
            .map(Ok)
            .unwrap_or_else(|| self.resolved_head(agent))?;
        ensure!(summary_head.is_object(), "summary_head must be an object");
        let mut normalized_head = empty_head();
        for (key, value) in normalized_head.as_object_mut().unwrap() {
            *value = summary_head[key].clone();
        }
        let result = self.summary(
            agent,
            &prior,
            args["queue_depth"].as_i64().unwrap_or(0).max(0) as u64,
            args["pending_reply_count"].as_i64().unwrap_or(0).max(0) as u64,
            args["active_inbound_event_id"].clone(),
            args["last_started_at"].clone(),
            args["last_finished_at"].clone(),
            normalized_head,
            text(args, "timestamp")?,
            text(args, "summary_source")?,
        )?;
        self.store.save(&mailbox(agent), &result)?;
        Ok(result)
    }
}
fn inbox(agent: &str) -> String {
    format!("mailboxes/{agent}/inbox.jsonl")
}
fn mailbox(agent: &str) -> String {
    format!("mailboxes/{agent}/mailbox.json")
}
fn lease_path(agent: &str) -> String {
    format!("leases/{agent}.json")
}
fn text<'a>(args: &'a Value, key: &str) -> Result<&'a str> {
    args[key]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("missing string: {key}"))
}
fn number(args: &Value, key: &str) -> u64 {
    args[key].as_u64().unwrap_or(0)
}
fn latest(current: &Value, candidate: &Value) -> Value {
    if candidate
        .as_str()
        .is_some_and(|s| !s.is_empty() && current.as_str().is_none_or(|c| s > c))
    {
        candidate.clone()
    } else {
        current.clone()
    }
}
fn head(event: &Event) -> Value {
    json!({"head_inbound_event_id":event.inbound_event_id,"head_event_type":event.event_type,"head_status":event.status,"head_message_id":event.message_id,"head_attempt_id":event.attempt_id,"head_payload_ref":event.payload_ref})
}
fn empty_head() -> Value {
    json!({"head_inbound_event_id":null,"head_event_type":null,"head_status":null,"head_message_id":null,"head_attempt_id":null,"head_payload_ref":null})
}
fn prior_head(prior: &Value) -> Value {
    let mut head = empty_head();
    for (key, value) in head.as_object_mut().unwrap() {
        *value = prior[key].clone();
    }
    head
}
fn ttl(args: &Value) -> Option<f64> {
    args["lease_ttl_seconds"]
        .as_f64()
        .filter(|n| n.is_finite() && *n > 0.0)
}
fn duration(seconds: f64) -> Duration {
    Duration::microseconds((seconds * 1_000_000.0).round() as i64)
}
fn parse_time(value: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(value)
        .ok()
        .map(|d| d.with_timezone(&Utc))
        .or_else(|| {
            ["%Y-%m-%dT%H:%M:%S%.f", "%Y-%m-%d %H:%M:%S%.f"]
                .iter()
                .find_map(|format| {
                    NaiveDateTime::parse_from_str(value, format)
                        .ok()
                        .map(|d| d.and_utc())
                })
        })
}
fn format_time(time: DateTime<Utc>) -> String {
    time.to_rfc3339_opts(
        if time.timestamp_subsec_micros() == 0 {
            SecondsFormat::Secs
        } else {
            SecondsFormat::Micros
        },
        true,
    )
}
fn validate_mailbox(value: &Value) -> Result<()> {
    ensure!(
        value["schema_version"] == 1 && value["record_type"] == "mailbox_record",
        "invalid mailbox schema"
    );
    validate_agent(text(value, "agent_name")?)?;
    for key in ["mailbox_id", "summary_source", "summary_refreshed_at"] {
        ensure!(!text(value, key)?.trim().is_empty(), "empty {key}");
    }
    for key in [
        "summary_version",
        "queue_depth",
        "pending_reply_count",
        "lease_version",
    ] {
        ensure!(value[key].as_u64().is_some(), "invalid {key}");
    }
    ensure!(
        ["idle", "delivering", "blocked", "recovering", "degraded"]
            .contains(&text(value, "mailbox_state")?),
        "invalid mailbox state"
    );
    Ok(())
}
