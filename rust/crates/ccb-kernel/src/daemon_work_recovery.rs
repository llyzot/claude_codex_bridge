//! Offline terminal reconciliation. No provider is spawned or prompted here.
use super::{now, Daemon};
use crate::{
    admission, records,
    terminal_history::{Baseline, Binding, ProviderReader},
    Kernel,
};
use anyhow::{ensure, Context, Result};
use serde_json::{json, Value};

impl Daemon {
    pub(crate) fn recover_closed_work(
        kernel: Kernel,
        config: Value,
        agents: &Value,
    ) -> Result<(Self, Value)> {
        let mut daemon = Self::new(
            kernel,
            config,
            &format!("recovery-{}", uuid::Uuid::new_v4()),
        )?;
        // Terminal faults only lost observation; their stopped work is settled
        // from history below. Other faults still need explicit provider repair.
        let mut terminal_faults = Vec::new();
        for agent in daemon.faults.keys().cloned().collect::<Vec<_>>() {
            if daemon.config["herdr_activation"]["blocked_agents"][&agent].is_string() {
                continue;
            }
            ensure!(
                daemon.registered_spec(&agent)?["driver_kind"] == "herdr_tui",
                "unfinished recovery requires explicit provider fault repair"
            );
            terminal_faults.push(agent);
        }
        let mut rows = Vec::new();
        for agent in daemon.config["agents"].as_object().unwrap().keys() {
            let jobs = records::dispatch(
                &mut daemon.kernel.store,
                "latest",
                &json!({"stream":"jobs","identity":"job_id","agent_name":agent}),
            )?;
            rows.extend(
                jobs.as_array()
                    .context("invalid recovery jobs")?
                    .iter()
                    .cloned(),
            );
        }
        let mut plans = Vec::new();
        for job in &rows {
            ensure!(
                job["request"]["project_id"] == daemon.config["project_id"],
                "recovery job belongs to another project"
            );
            let agent = records::string(job, "agent_name")?;
            if daemon.config["herdr_activation"]["blocked_agents"][agent].is_string() {
                continue;
            }
            let spec = daemon.registered_spec(agent)?;
            if job["status"] == "running" {
                if spec["driver_kind"] == "herdr_tui" {
                    plans.push((
                        job.clone(),
                        Some(daemon.stopped_terminal_decision(job, &spec, agents)?),
                    ));
                } else {
                    let observed = daemon.recovery_view(job);
                    ensure!(
                        observed["status"] == "stopped",
                        "unfinished owned execution lacks stopped-scope proof"
                    );
                    plans.push((job.clone(), None));
                }
            } else if !["completed", "failed", "incomplete", "cancelled"]
                .contains(&job["status"].as_str().unwrap_or(""))
            {
                ensure!(
                    ["accepted", "queued", "pending"]
                        .contains(&job["status"].as_str().unwrap_or(""))
                        && job["provider_options"]["native_execution_instance"].is_null()
                        && job["provider_options"]["herdr_dispatch"].is_null(),
                    "unfinished queued execution may already have been sent"
                );
            }
        }
        // Validate every old execution before committing any reconciliation.
        let mut recovered = Vec::new();
        for (job, decision) in plans {
            if let Some(decision) = decision {
                let checkpoint = daemon.kernel.memory_checkpoint();
                let outcome = (|| -> Result<Value> {
                    let terminal = daemon.complete(&job, &decision)?;
                    admission::event(
                        &mut daemon.kernel,
                        &terminal,
                        "job_execution_reconciled",
                        json!({"completion_authority":"stopped_herdr_history","automatic_replay":false}),
                        &now(),
                    )?;
                    daemon.kernel.store.commit()?;
                    Ok(terminal)
                })();
                match outcome {
                    Ok(terminal) => recovered
                        .push(json!({"job_id":terminal["job_id"],"status":terminal["status"]})),
                    Err(error) => {
                        daemon.kernel.store.rollback();
                        daemon.kernel.restore_memory(checkpoint);
                        return Err(error);
                    }
                }
            } else {
                let result = daemon.reconcile_execution(records::string(&job, "job_id")?)?;
                ensure!(
                    result["status"] == "reconciled",
                    "owned execution reconciliation did not complete"
                );
                recovered.push(json!({"job_id":job["job_id"],"status":result["job"]["status"]}));
            }
        }
        let mut cleared = Vec::new();
        for agent in terminal_faults {
            let mut fault = daemon.faults[&agent].clone();
            fault["status"] = json!("recovered");
            fault["recovered_at"] = json!(now());
            fault["recovered_by"] = json!("stopped_terminal_reconciliation");
            let provider = records::string(&fault, "provider")?.to_owned();
            daemon
                .kernel
                .store
                .save(&crate::provider_fault::path(&provider, &agent), &fault)?;
            daemon.kernel.store.commit()?;
            daemon.faults.remove(&agent);
            cleared.push(json!({"agent_name":agent,"fault_id":fault["fault_id"]}));
        }
        let report = json!({"schema_version":1,"provider_io":false,"automatic_replay":false,"jobs":recovered,"cleared_terminal_faults":cleared});
        Ok((daemon, report))
    }

    fn stopped_terminal_decision(
        &mut self,
        job: &Value,
        spec: &Value,
        agents: &Value,
    ) -> Result<Value> {
        let agent = records::string(job, "agent_name")?;
        let permit = &job["provider_options"]["herdr_dispatch"];
        let cancel = &job["provider_options"]["native_cancel_intent"];
        ensure!(
            cancel.is_null() || cancel["record_reply"].is_boolean(),
            "unfinished execution has invalid cancellation intent"
        );
        let execution = records::string(&job["provider_options"], "native_execution_instance")?;
        uuid::Uuid::parse_str(execution)?;
        if job["provider_options"]["native_context_control"].is_object() {
            // A stopped context command is never resent; its effect is unknown.
            return Ok(
                json!({"schema_version":2,"record_type":"completion_decision","terminal":true,
                "status":"incomplete","reason":"herdr_context_interrupted","confidence":"degraded",
                "reply":"","anchor_seen":false,"reply_started":false,"reply_stable":false,
                "provider_turn_ref":null,"source_cursor":null,"finished_at":now(),
                "diagnostics":{"automatic_replay":false,"recovery_no_replay":true,
                    "provider_prompt_permitted":!permit.is_null()}}),
            );
        }
        let operator = &self.config["herdr_activation"]["operator_fresh_session"][agent];
        if operator.is_string() {
            // The operator accepted that this session's evidence is unavailable.
            // Only the process that received the prompt must be proven gone.
            let prompted = &permit["identity"]["process"];
            ensure!(
                permit.is_null()
                    || prompted.is_object()
                        && !crate::startup::owner_alive(
                            &json!({"pid":prompted["pid"],"process":prompted})
                        )?,
                "prompted terminal process is live or unconfirmed"
            );
            let mut decision = json!({"schema_version":2,"record_type":"completion_decision",
                "terminal":true,"status":"incomplete","reason":"operator_fresh_session","confidence":"degraded",
                "reply":"The operator started a fresh provider session because the previous native session evidence was unavailable. Work may already have taken effect. Inspect the project before explicitly retrying.",
                "anchor_seen":false,"reply_started":false,"reply_stable":false,"provider_turn_ref":null,"source_cursor":null,
                "finished_at":now(),"diagnostics":{"operator_repair":operator,"recovery_no_replay":true,"automatic_replay":false,
                "native_execution_instance":execution,"provider_prompt_permitted":!permit.is_null()}});
            if let Some(reply) = cancel["record_reply"].as_bool() {
                decision["status"] = json!("cancelled");
                decision["diagnostics"]["result_reply_enabled"] = json!(reply);
                decision["diagnostics"]["cancel_requested"] = json!(true);
            }
            return Ok(decision);
        }
        let mut identity = crate::herdr_agents::recorded_identity(agents, agent);
        if identity["process"].is_null() && agents["agents"][agent]["sent"] != true {
            // Held in the latest generation, so never relaunched there. The
            // process that received this prompt is its only terminal owner.
            identity = permit["identity"].clone();
        }
        ensure!(
            identity["process"].is_object()
                && !crate::startup::owner_alive(
                    &json!({"pid":identity["process"]["pid"],"process":identity["process"]})
                )?,
            "old terminal process is live or unconfirmed"
        );
        let anchor = format!("{}:{execution}", records::string(job, "job_id")?);
        let mut decision = None;
        if !permit.is_null() {
            ensure!(
                permit["schema_version"] == 1
                    && permit["job_id"] == job["job_id"]
                    && permit["execution_instance_id"] == execution
                    && permit["request_anchor"] == anchor
                    && permit["identity"] == identity,
                "stopped Herdr execution permit does not match its recorded owner"
            );
            if spec["herdr_discovery"].is_object() {
                if let Some(recovered) = crate::herdr_discovery::recover(spec, job, &identity)? {
                    if job["provider"] == "claude" {
                        decision = crate::herdr_terminal_evidence::recover(
                            job,
                            spec,
                            &identity,
                            &recovered.binding,
                            recovered.decision,
                        )?;
                    } else {
                        decision = recovered.decision;
                    }
                }
            } else {
                // Without newer evidence, the permit records exactly what was sent.
                ensure!(
                    spec["herdr_history"].is_null()
                        || permit["history_binding"] == spec["herdr_history"],
                    "stopped terminal history binding changed"
                );
                let configured: Binding =
                    serde_json::from_value(permit["history_binding"].clone())?;
                let resolved: Binding =
                    serde_json::from_value(if permit["resolved_history_binding"].is_null() {
                        permit["history_binding"].clone()
                    } else {
                        permit["resolved_history_binding"].clone()
                    })?;
                ensure!(
                    configured.root == resolved.root
                        && configured.cwd == resolved.cwd
                        && configured.session_id == resolved.session_id,
                    "stopped terminal history scope changed"
                );
                let baseline: Baseline = serde_json::from_value(permit["baseline"].clone())?;
                if job["provider"] == "pi" {
                    let producer = baseline
                        .producer
                        .as_ref()
                        .context("Pi execution producer missing")?;
                    ensure!(
                        (spec["pi_bridge_owner"].is_null()
                            || producer["owner"] == spec["pi_bridge_owner"])
                            && producer["process"] == identity["process"],
                        "stopped Pi producer differs from permitted input"
                    );
                }
                let mut reader = ProviderReader::new(
                    records::string(job, "provider")?,
                    resolved,
                    baseline,
                    anchor.clone(),
                )?;
                let candidate = reader.poll_stopped()?;
                if job["provider"] == "claude" {
                    decision = crate::herdr_terminal_evidence::recover(
                        job,
                        spec,
                        &identity,
                        &configured,
                        candidate,
                    )?;
                } else {
                    decision = candidate;
                }
            }
        }
        let delivery = job["request"]["message_type"] == "reply_delivery"
            || job["provider_options"]["reply_delivery"] == true;
        if delivery
            && !permit.is_null()
            && decision.as_ref().is_none_or(|d| d["status"] != "completed")
        {
            // A delivery prompt merged with a person's unsent draft no longer
            // starts with its anchor, so its turn is unattributable. The exact
            // execution anchor inside a submitted user message still proves
            // the result reached the agent, so it is settled, not redelivered.
            if let Some(path) = native_session(job, &permit) {
                if anchor_submitted(&path, records::string(job, "provider")?, &anchor)? {
                    decision = Some(json!({"schema_version":2,"record_type":"completion_decision",
                        "terminal":true,"status":"completed","reason":"reply_delivery_anchor_submitted","confidence":"exact",
                        "reply":"","anchor_seen":true,"reply_started":false,"reply_stable":true,"provider_turn_ref":null,
                        "source_cursor":null,"finished_at":now(),
                        "diagnostics":{"completion_authority":"native_session_anchor_scan","provider_prompt_sent":true}}));
                }
            }
        }
        let mut decision=decision.unwrap_or_else(||json!({"schema_version":2,"record_type":"completion_decision",
            "terminal":true,"status":"incomplete","reason":"herdr_owner_stopped","confidence":"observed",
            "reply":"The previous terminal process stopped without a confirmed terminal result. Work may already have taken effect. Inspect the saved session before explicitly retrying.",
            "anchor_seen":false,"reply_started":false,"reply_stable":false,"provider_turn_ref":null,"source_cursor":null,
            "finished_at":now(),"diagnostics":{}}));
        decision["diagnostics"]["recovery_no_replay"] = json!(true);
        decision["diagnostics"]["automatic_replay"] = json!(false);
        decision["diagnostics"]["native_execution_instance"] = json!(execution);
        decision["diagnostics"]["old_terminal_identity"] = identity;
        decision["diagnostics"]["provider_prompt_permitted"] = json!(!permit.is_null());
        if let Some(reply) =
            job["provider_options"]["native_cancel_intent"]["record_reply"].as_bool()
        {
            decision["diagnostics"]["result_reply_enabled"] = json!(reply);
            decision["diagnostics"]["cancel_requested"] = json!(true);
            if decision["status"] != "completed" {
                decision["status"] = json!("cancelled");
            }
        }
        ensure!((job["request"]["message_type"]!="reply_delivery" && job["provider_options"]["reply_delivery"] != true) || decision["status"]=="completed",
            "uncertain reply delivery requires explicit recovery; it cannot be redelivered automatically");
        Ok(decision)
    }
}

/// The provider's own session file that received a permitted prompt.
fn native_session(job: &Value, permit: &Value) -> Option<std::path::PathBuf> {
    let path = if job["provider"] == "pi" {
        &permit["baseline"]["producer"]["session_path"]
    } else if permit["resolved_history_binding"].is_object() {
        &permit["resolved_history_binding"]["path"]
    } else {
        &permit["history_binding"]["path"]
    };
    path.as_str()
        .map(std::path::PathBuf::from)
        .filter(|p| p.is_absolute())
}

/// Whether a user message in the provider's session contains this exact
/// execution anchor. The anchor (`job:execution`) is unique per prompt.
fn anchor_submitted(path: &std::path::Path, provider: &str, anchor: &str) -> Result<bool> {
    use std::io::BufRead;
    let needle = format!("CCB_REQ_ID: {anchor}");
    let file = match std::fs::File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error.into()),
    };
    for line in std::io::BufReader::new(file).lines() {
        let line = line?;
        if !line.contains(anchor) {
            continue;
        }
        let Ok(row) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        let message = &row["message"];
        let user = match provider {
            "pi" => row["type"] == "message" && message["role"] == "user",
            _ => row["type"] == "user" && row["isSidechain"] != true && row["isMeta"] != true,
        };
        if !user {
            continue;
        }
        let text = match &message["content"] {
            Value::String(text) => text.clone(),
            Value::Array(parts) => parts
                .iter()
                .filter_map(|p| p["text"].as_str())
                .collect::<Vec<_>>()
                .join("
"),
            _ => String::new(),
        };
        if text.contains(&needle) {
            return Ok(true);
        }
    }
    Ok(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn session(rows: &[Value]) -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session.jsonl");
        let text: String = rows.iter().map(|r| format!("{r}\n")).collect();
        std::fs::write(&path, format!("not json job_1:exec\n{text}")).unwrap();
        (dir, path)
    }
    #[test]
    fn a_submitted_anchor_proves_a_merged_delivery() {
        let anchor = "job_1:11111111-1111-4111-8111-111111111111";
        let merged = format!("unsent draftCCB_REQ_ID: {anchor}\nCCB_REPLY from=codex");
        let (_d, pi) = session(&[
            json!({"type":"message","message":{"role":"assistant","content":[{"type":"text","text":format!("CCB_REQ_ID: {anchor}")}]}}),
            json!({"type":"message","message":{"role":"user","content":[{"type":"text","text":merged}]}}),
        ]);
        assert!(anchor_submitted(&pi, "pi", anchor).unwrap());
        let (_d, claude) = session(&[
            json!({"type":"user","isSidechain":false,"message":{"role":"user","content":format!("<pasted_content id=\"1\">\nCCB_REQ_ID: {anchor}\ntask")}}),
        ]);
        assert!(anchor_submitted(&claude, "claude", anchor).unwrap());
    }
    #[test]
    fn other_roles_executions_and_missing_files_prove_nothing() {
        let anchor = "job_1:11111111-1111-4111-8111-111111111111";
        let (_d, pi) = session(&[
            json!({"type":"message","message":{"role":"assistant","content":[{"type":"text","text":format!("CCB_REQ_ID: {anchor}")}]}}),
            json!({"type":"message","message":{"role":"user","content":"CCB_REQ_ID: job_1:22222222-2222-4222-8222-222222222222"}}),
        ]);
        assert!(!anchor_submitted(&pi, "pi", anchor).unwrap());
        let (_d, claude) = session(&[
            json!({"type":"user","isSidechain":true,"message":{"role":"user","content":format!("CCB_REQ_ID: {anchor}")}}),
            json!({"type":"user","isMeta":true,"message":{"role":"user","content":format!("CCB_REQ_ID: {anchor}")}}),
        ]);
        assert!(!anchor_submitted(&claude, "claude", anchor).unwrap());
        let dir = tempfile::tempdir().unwrap();
        assert!(!anchor_submitted(&dir.path().join("absent.jsonl"), "pi", anchor).unwrap());
    }
}
