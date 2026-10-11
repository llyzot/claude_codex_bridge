//! In-place configuration changes. Durable work fences replacements; workspace
//! preparation runs off the actor. Configuration secrets never enter the journal.
use super::{now, Daemon};
use crate::{records, Kernel};
use anyhow::{ensure, Context, Result};
use serde_json::{json, Value};
use std::{collections::BTreeSet, sync::mpsc};

pub(super) struct Pending {
    config: Value,
    affected: Vec<String>,
    record: Value,
    prepared: mpsc::Receiver<bool>,
}

fn hash(value: &Value) -> Result<String> {
    use sha2::{Digest, Sha256};
    Ok(format!("{:x}", Sha256::digest(serde_json::to_vec(value)?)))
}
fn descriptors(config: &Value) -> Result<Value> {
    let mut result = json!({});
    for (name, spec) in config["agents"].as_object().context("missing agents")? {
        let mut homes = json!({});
        for key in [
            "PI_CODING_AGENT_DIR",
            "CODEX_HOME",
            "CLAUDE_CONFIG_DIR",
            "HOME",
            "USERPROFILE",
        ] {
            homes[key] = spec["env"]
                .get(key)
                .cloned()
                .unwrap_or_else(|| json!(std::env::var_os(key)));
        }
        result[name] = json!({"spec_hash":hash(spec)?,
            "session_binding":hash(&json!([spec["provider"],spec["driver_kind"],spec["workspace_path"],spec["env"],homes]))?});
    }
    Ok(result)
}
fn namespace(config: &Value) -> Value {
    let mut value = config.clone();
    for key in ["agents", "source_config", "startup_id"] {
        value.as_object_mut().unwrap().remove(key);
    }
    value
}
fn history(kernel: &Kernel, prior: &Value, config: &Value) -> Result<BTreeSet<String>> {
    let mut names: BTreeSet<String> = config["agents"]
        .as_object()
        .unwrap()
        .keys()
        .cloned()
        .collect();
    if let Some(rows) = prior["history_agents"].as_object() {
        names.extend(rows.keys().cloned());
    }
    for path in kernel.store.list("mailboxes")? {
        names.insert(
            path.split('/')
                .nth(1)
                .context("invalid mailbox path")?
                .into(),
        );
    }
    for name in &names {
        crate::models::validate_agent(name)?;
    }
    Ok(names)
}
fn configuration(kernel: &Kernel, config: &Value) -> Result<Value> {
    let prior = kernel
        .store
        .json("configuration.json")?
        .unwrap_or(Value::Null);
    let agents = descriptors(config)?;
    let mut previous = prior["history_agents"]
        .as_object()
        .cloned()
        .unwrap_or_default();
    for name in history(kernel, &prior, config)? {
        previous.entry(name).or_insert(Value::Null);
    }
    previous.extend(agents.as_object().unwrap().clone());
    Ok(
        json!({"schema_version":1,"record_type":"native_configuration",
        "project_id":config["project_id"],"namespace_hash":hash(&namespace(config))?,
        "config_fingerprint":crate::config::fingerprint(config)?,
        "agents":agents,"history_agents":previous}),
    )
}
fn latest(kernel: &mut Kernel, stream: &str, identity: &str, agent: Option<&str>) -> Result<Value> {
    records::dispatch(
        &mut kernel.store,
        "latest",
        &json!({"stream":stream,"identity":identity,"agent_name":agent}),
    )
}
// The mailbox alone is insufficient: an idle Lead may still expect a result
// from a running Worker, before a reply event has even been materialized.
fn blockers(kernel: &mut Kernel, names: &[String]) -> Result<Vec<Value>> {
    let mut blocked = Vec::new();
    for agent in names {
        for job in latest(kernel, "jobs", "job_id", Some(agent))?
            .as_array()
            .unwrap()
        {
            if !["completed", "failed", "cancelled", "incomplete"]
                .contains(&job["status"].as_str().unwrap_or(""))
            {
                blocked.push(
                    json!({"agent_name":agent,"reason":"unfinished_job","job_id":job["job_id"]}),
                );
            }
        }
        let events = kernel.dispatch("pending_events", &json!({"agent_name":agent}))?;
        for event in events.as_array().unwrap() {
            blocked.push(json!({"agent_name":agent,"reason":"pending_mailbox","inbound_event_id":event["inbound_event_id"]}));
        }
        for provider in ["pi", "codex", "claude"] {
            if let Some(fault) = kernel
                .store
                .json(&crate::provider_fault::path(provider, agent))?
            {
                if fault["status"] == "blocked" {
                    blocked.push(json!({"agent_name":agent,"reason":"provider_fault","fault_id":fault["fault_id"]}));
                }
            }
        }
    }
    if names.is_empty() {
        return Ok(blocked);
    }
    for edge in latest(kernel, "callbacks", "edge_id", None)?
        .as_array()
        .unwrap()
    {
        if ["done", "failed", "timed_out"].contains(&edge["state"].as_str().unwrap_or("")) {
            continue;
        }
        for agent in names {
            if ["parent_agent", "callback_target_agent", "original_caller"]
                .iter()
                .any(|k| edge[k] == *agent)
                || edge["diagnostics"]["child_agent"] == *agent
            {
                blocked.push(
                    json!({"agent_name":agent,"reason":"open_callback","edge_id":edge["edge_id"]}),
                );
            }
        }
    }
    for message in latest(kernel, "messages", "message_id", None)?
        .as_array()
        .unwrap()
    {
        if [
            "completed",
            "incomplete",
            "failed",
            "cancelled",
            "dead_letter",
        ]
        .contains(&message["message_state"].as_str().unwrap_or(""))
        {
            continue;
        }
        for agent in names {
            if message["from_actor"]
                .as_str()
                .is_some_and(|s| s.trim().eq_ignore_ascii_case(agent))
                && message["reply_policy"]["mode"] != "none"
            {
                blocked.push(json!({"agent_name":agent,"reason":"outstanding_reply","message_id":message["message_id"]}));
            }
        }
    }
    Ok(blocked)
}

/// Runs before workspace materialization, including explicit stop/start. A
/// different boot configuration must not hide unresolved jobs or reply routes.
pub(super) fn startup_guard(kernel: &mut Kernel, config: &Value) -> Result<()> {
    let prior = kernel
        .store
        .json("configuration.json")?
        .unwrap_or(Value::Null);
    let desired = descriptors(config)?;
    let names = history(kernel, &prior, config)?;
    for name in &names {
        ensure!(
            latest(kernel, "jobs", "job_id", Some(name))?
                .as_array()
                .unwrap()
                .iter()
                .all(|job| job["request"]["project_id"] == config["project_id"]),
            "agent journal belongs to another project; choose a separate runtime parent directory"
        );
    }
    let prior_lease = kernel.store.json("lease.json")?.unwrap_or(Value::Null);
    let namespace_changed =
        !prior.is_null() && prior["namespace_hash"] != hash(&namespace(config))?;
    let legacy_unverified = prior.is_null()
        && !prior_lease.is_null()
        && prior_lease["config_signature"] != crate::config::fingerprint(config)?;
    if !prior.is_null() {
        ensure!(
            prior["schema_version"] == 1
                && prior["record_type"] == "native_configuration"
                && prior["project_id"] == config["project_id"],
            "native configuration identity mismatch"
        );
    }
    let changed: Vec<_> = names
        .into_iter()
        .filter(|name| {
            if prior.is_null() {
                // Legacy state cannot prove which argv/environment owned queued work.
                // Require it to be drained using its old runtime before changing owners.
                legacy_unverified || desired[name].is_null()
            } else {
                namespace_changed || desired[name] != prior["agents"][name]
            }
        })
        .collect();
    ensure!(blockers(kernel, &changed)?.is_empty(),
        "configuration change would strand unfinished work; restart with the prior agent configuration");
    Ok(())
}

impl Daemon {
    pub(super) fn initialize_configuration(&mut self) -> Result<()> {
        let next = configuration(&self.kernel, &self.config)?;
        self.known_names = next["history_agents"]
            .as_object()
            .unwrap()
            .keys()
            .cloned()
            .collect();
        self.reset_session_bindings(&next)?;
        self.kernel.store.save("configuration.json", &next)?;
        if let Some(mut record) = self.kernel.store.json("reload.json")? {
            if ["preparing", "retiring"].contains(&record["status"].as_str().unwrap_or("")) {
                record["status"] = json!("interrupted");
                record["reason"] = json!("owner_restarted_explicit_configuration_required");
                record["finished_at"] = json!(now());
                self.kernel.store.save("reload.json", &record)?;
            }
        }
        Ok(())
    }
    fn reset_session_bindings(&mut self, next: &Value) -> Result<()> {
        let prior = self
            .kernel
            .store
            .json("configuration.json")?
            .unwrap_or(Value::Null);
        for name in next["agents"].as_object().unwrap().keys() {
            let previous = &prior["history_agents"][name];
            if !previous.is_null()
                && previous["session_binding"] != next["agents"][name]["session_binding"]
            {
                // Only the native resume pointer is removed, never provider history.
                for provider in ["pi", "codex", "claude"] {
                    self.kernel
                        .store
                        .remove(&format!("providers/{provider}/{name}/session.json"))?;
                }
            }
        }
        Ok(())
    }
    pub(super) fn reload_frozen(&self, agent: &str) -> bool {
        self.reload.as_ref().is_some_and(|p| {
            p.affected
                .iter()
                .any(|s| s.eq_ignore_ascii_case(agent.trim()))
        })
    }
    pub(super) fn reload_guard(&self, agent: &str) -> Result<()> {
        ensure!(
            !self.reload_frozen(agent),
            "agent configuration reload in progress: {agent}"
        );
        Ok(())
    }
    pub(super) fn reload_plan(&mut self, desired: &Value) -> Result<Value> {
        Self::validate_config(desired)?;
        ensure!(
            namespace(&self.config) == namespace(desired),
            "reload cannot change the daemon namespace or global runtime options"
        );
        let old = self.config["agents"].as_object().unwrap();
        let new = desired["agents"].as_object().unwrap();
        let added: Vec<_> = new
            .keys()
            .filter(|k| !old.contains_key(*k))
            .cloned()
            .collect();
        let removed: Vec<_> = old
            .keys()
            .filter(|k| !new.contains_key(*k))
            .cloned()
            .collect();
        let changed: Vec<_> = old
            .keys()
            .filter(|k| new.get(*k).is_some_and(|v| v != &old[*k]))
            .cloned()
            .collect();
        let unchanged: Vec<_> = old
            .keys()
            .filter(|k| new.get(*k) == old.get(*k))
            .cloned()
            .collect();
        let affected: Vec<_> = removed
            .iter()
            .chain(&changed)
            .chain(&added)
            .cloned()
            .collect();
        let mut blocked = blockers(&mut self.kernel, &affected)?;
        let mut retiring = json!({});
        for agent in &affected {
            let owned = self.owned_view(agent);
            if !owned.is_null() && owned["closed"] != true {
                let scope = self.owned_scope(agent)?;
                if owned["ready"] != true || scope["supported"] != true {
                    blocked.push(json!({"agent_name":agent,"reason":"provider_not_retirable"}));
                }
                retiring[agent] = json!({"owned":owned,"scope":scope});
            }
        }
        Ok(
            json!({"current_fingerprint":crate::config::fingerprint(&self.config)?,
            "desired_fingerprint":crate::config::fingerprint(desired)?,"added":added,"removed":removed,
            "changed":changed,"unchanged":unchanged,"affected":affected,"retiring":retiring,
            "blockers":blocked,"can_apply":blocked.is_empty()}),
        )
    }
    pub(super) fn request_reload(&mut self, request: &Value) -> Result<Value> {
        let desired = &request["config"];
        let fingerprint = crate::config::fingerprint(desired)?;
        if let Some(pending) = &self.reload {
            ensure!(
                pending.record["desired_fingerprint"] == fingerprint,
                "another configuration reload is in progress"
            );
            return Ok(json!({"reload":pending.record}));
        }
        let plan = self.reload_plan(desired)?;
        if plan["current_fingerprint"] == plan["desired_fingerprint"] {
            return Ok(json!({"reload":{"status":"unchanged","desired_fingerprint":fingerprint}}));
        }
        ensure!(
            request["expected_fingerprint"] == plan["current_fingerprint"],
            "configuration reload fingerprint mismatch"
        );
        if plan["can_apply"] != true {
            return Ok(json!({"reload":{"status":"blocked","plan":plan}}));
        }
        let affected: Vec<String> = serde_json::from_value(plan["affected"].clone())?;
        let record = json!({"schema_version":1,"record_type":"native_reload","operation_id":uuid::Uuid::new_v4().to_string(),
            "daemon_instance_id":self.state,"status":"preparing","started_at":now(),
            "current_fingerprint":plan["current_fingerprint"],"desired_fingerprint":fingerprint,"plan":plan});
        self.kernel.store.save("reload.json", &record)?;
        self.kernel.store.commit()?;
        let mut prepare = desired.clone();
        prepare["agents"]
            .as_object_mut()
            .unwrap()
            .retain(|name, _| affected.contains(name));
        let (send, prepared) = mpsc::channel();
        // Thread creation itself is fallible; preserve a readable failed receipt.
        let spawned = std::thread::Builder::new()
            .name("ccb-workspace-prepare".into())
            .spawn(move || {
                let _ = send.send(crate::workspaces::materialize(&prepare).is_ok());
            });
        self.reload = Some(Pending {
            config: desired.clone(),
            affected,
            record: record.clone(),
            prepared,
        });
        if spawned.is_err() {
            self.fail_reload("workspace_preparation_unavailable")?;
        }
        Ok(json!({"reload":self.kernel.store.json("reload.json")?}))
    }
    fn fail_reload(&mut self, reason: &str) -> Result<()> {
        let pending = self.reload.as_ref().context("missing reload")?;
        let mut record = pending.record.clone();
        record["status"] = json!("failed");
        record["reason"] = json!(reason);
        record["finished_at"] = json!(now());
        self.kernel.store.save("reload.json", &record)?;
        self.kernel.store.commit()?;
        self.reload = None;
        Ok(())
    }
    pub(super) fn tick_reload(&mut self) -> Result<()> {
        let Some(pending) = &self.reload else {
            return Ok(());
        };
        if pending.record["status"] == "preparing" {
            match pending.prepared.try_recv() {
                Err(mpsc::TryRecvError::Empty) => return Ok(()),
                Ok(true) => {}
                _ => return self.fail_reload("workspace_preparation_failed"),
            }
            let affected = pending.affected.clone();
            if !blockers(&mut self.kernel, &affected)?.is_empty() {
                return self.fail_reload("work_appeared_during_preparation");
            }
            let pending = self.reload.as_mut().unwrap();
            pending.record["status"] = json!("retiring");
            self.kernel.store.save("reload.json", &pending.record)?;
            self.kernel.store.commit()?;
            for agent in affected {
                let owned = self.owned_view(&agent);
                if !owned.is_null()
                    && owned["closed"] != true
                    && self.send_tree_termination(&agent).is_err()
                {
                    return self.fail_reload("provider_retirement_failed");
                }
            }
        }
        let affected = self.reload.as_ref().unwrap().affected.clone();
        for agent in &affected {
            let result = if let Some(session) = self.pi.get_mut(agent) {
                let prior = session.state.clone();
                let result = session.poll();
                if result.is_err() {
                    session.quarantine(prior);
                }
                result
            } else if let Some(session) = self.codex.get_mut(agent) {
                let prior = session.state.clone();
                let result = session.poll();
                if result.is_err() {
                    session.quarantine(prior);
                }
                result
            } else if let Some(session) = self.claude.get_mut(agent) {
                let prior = session.state.clone();
                let result = session.poll();
                if result.is_err() {
                    session.quarantine(prior);
                }
                result
            } else {
                Ok(None)
            };
            match result {
                Ok(None) => {}
                Ok(Some((job, decision))) => {
                    self.complete(&job, &decision)?;
                    self.kernel.store.commit()?;
                    return self.fail_reload("unexpected_provider_work");
                }
                Err(error) => {
                    self.record_provider_fault(agent, "poll", &error)?;
                    return self.fail_reload("provider_retirement_protocol_error");
                }
            }
            let owned = self.owned_view(agent);
            if !owned.is_null() && owned["closed"] != true {
                return Ok(());
            }
        }
        if !blockers(&mut self.kernel, &affected)?.is_empty() {
            return self.fail_reload("work_appeared_before_publish");
        }
        let checkpoint = self.kernel.memory_checkpoint();
        if let Err(error) = self.publish_reload() {
            self.kernel.store.rollback();
            self.kernel.restore_memory(checkpoint);
            // Commit errors are fatal to the actor, never an excuse to resume
            // the old configuration against potentially committed new records.
            return Err(error);
        }
        Ok(())
    }
    fn publish_reload(&mut self) -> Result<()> {
        let pending = self.reload.as_ref().unwrap();
        let mut desired = pending.config.clone();
        // This belongs to the process boot, not to the newly supplied document.
        desired.as_object_mut().unwrap().remove("startup_id");
        if let Some(id) = self.config.get("startup_id") {
            desired["startup_id"] = id.clone();
        }
        let next = configuration(&self.kernel, &desired)?;
        let mut record = pending.record.clone();
        record["status"] = json!("applied");
        record["finished_at"] = json!(now());
        let names: Vec<_> = desired["agents"]
            .as_object()
            .unwrap()
            .keys()
            .cloned()
            .collect();
        let mut rows = json!({});
        let mut order = json!({});
        for agent in &names {
            rows[agent] = records::dispatch(
                &mut self.kernel.store,
                "list",
                &json!({"stream":"jobs","agent_name":agent}),
            )?;
            let inbox = self
                .kernel
                .dispatch("inbox.list", &json!({"agent_name":agent}))?;
            order[agent] = json!(inbox
                .as_array()
                .unwrap()
                .iter()
                .filter(|e| ["created", "queued", "delivering"]
                    .contains(&e["status"].as_str().unwrap_or("")))
                .filter_map(|e| super::payload(&e["payload_ref"], "delivery:")
                    .or_else(|| super::payload(&e["payload_ref"], "job:")))
                .collect::<Vec<_>>());
        }
        self.kernel
            .dispatch("scheduler.drop", &json!({"state_id":self.state}))?;
        self.kernel.dispatch(
            "scheduler.create",
            &json!({"state_id":self.state,"agent_names":names}),
        )?;
        self.kernel.dispatch("scheduler.rebuild", &json!({"state_id":self.state,"agent_names":names,"records":rows,"mailbox_order":order}))?;
        self.reset_session_bindings(&next)?;
        self.kernel.store.save("configuration.json", &next)?;
        self.kernel.store.save("reload.json", &record)?;
        let lease = self
            .kernel
            .store
            .json("lease.json")?
            .context("missing lease")?;
        crate::lifecycle::dispatch(
            &mut self.kernel,
            "signature",
            &json!({"expected_pid":std::process::id(),
            "expected_daemon_instance_id":self.state,"expected_generation":lease["generation"],
            "config_signature":next["config_fingerprint"],"now":now()}),
        )?;
        self.kernel.store.commit()?;
        for agent in &self.reload.as_ref().unwrap().affected {
            self.pi.remove(agent);
            self.codex.remove(agent);
            self.claude.remove(agent);
        }
        self.config = desired;
        self.known_names = next["history_agents"]
            .as_object()
            .unwrap()
            .keys()
            .cloned()
            .collect();
        self.reload = None;
        Ok(())
    }
}
