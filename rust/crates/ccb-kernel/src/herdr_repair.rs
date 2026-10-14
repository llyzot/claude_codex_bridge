//! Operator-directed release of an actor held for missing recovery evidence.
//! The request is recorded offline and consumed by the next stopped-server
//! recovery: unfinished sent work closes as incomplete without replay, then the
//! agent launches a fresh native session. Nothing is inferred or resent here.
use super::{read, RECORD};
use anyhow::{ensure, Context, Result};
use serde_json::{json, Value};
use std::{fs::OpenOptions, path::Path};

pub(super) const REPAIRS: &str = "operator-repairs.json";
pub(super) const AUDIT: &str = "operator-repairs.jsonl";

pub(super) fn fresh_session(project: &Path, agent: &str, dry_run: bool) -> Result<Value> {
    crate::models::validate_agent(agent)?;
    let runtime = crate::config::state_root(project)?;
    let root = runtime.join("herdr");
    crate::herdr_namespace::unlinked(&runtime)?;
    crate::herdr_namespace::unlinked(&root)?;
    let lock_path = root.join("native-open.lock");
    crate::herdr_namespace::unlinked(&lock_path)?;
    let lock = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(lock_path)?;
    ensure!(
        lock.try_lock().is_ok(),
        "a native Herdr opener is still active; run ccb kill before repair"
    );
    crate::daemon::verify_owner(&runtime)
        .context("project runtime must be stopped before repair")?;
    let keeper = crate::keeper::status(&runtime)?;
    ensure!(
        keeper["alive"] == false
            || ["stopped", "failed"].contains(&keeper["keeper"]["phase"].as_str().unwrap_or("")),
        "project Keeper is still supervising; run ccb kill before repair"
    );
    let open = read(&root, RECORD)?.context("project has no native Herdr open record")?;
    ensure!(
        open["project_id"] == crate::config::project_id(project)?,
        "Herdr open record belongs to another project"
    );
    let prior_error = open["initialization_errors"][agent]
        .as_str()
        .with_context(|| format!("agent {agent} is not held for recovery evidence"))?
        .to_owned();
    let kernel = crate::Kernel::open(&runtime)?;
    let committed = kernel
        .store
        .json("configuration.json")?
        .context("committed configuration missing")?;
    let prior = crate::restart_config::restore(&committed)?;
    let provider = crate::records::string(&prior["agents"][agent], "provider")
        .with_context(|| format!("agent {agent} is not in the committed configuration"))?;
    // A terminal fault is settled by stopped-history reconciliation; a native
    // host fault concerns processes CCB owns and needs provider recovery.
    if prior["agents"][agent]["driver_kind"] != "herdr_tui" {
        if let Some(fault) = read(&runtime, &crate::provider_fault::path(provider, agent))? {
            ensure!(
                fault["status"] != "blocked",
                "provider fault requires explicit provider recovery, not a fresh session"
            );
        }
    }
    let mut kernel = kernel;
    let rows = kernel.dispatch(
        "records.latest",
        &json!({"stream":"jobs","identity":"job_id","agent_name":agent}),
    )?;
    let mut abandoned = Vec::new();
    for job in rows.as_array().context("invalid job history")? {
        if job["status"] != "running" {
            continue;
        }
        ensure!(
            job["request"]["message_type"] != "reply_delivery"
                && job["provider_options"]["reply_delivery"] != true,
            "uncertain reply delivery requires explicit recovery; a fresh session cannot close it"
        );
        abandoned.push(json!({"job_id":job["job_id"],
            "execution_instance_id":job["provider_options"]["native_execution_instance"],
            "prompt_permitted":!job["provider_options"]["herdr_dispatch"].is_null()}));
    }
    let mut repairs = read(&root, REPAIRS)?.unwrap_or_else(|| json!({}));
    ensure!(repairs.is_object(), "invalid operator repair record");
    if let Some(existing) = repairs.get(agent).filter(|v| v.is_object()) {
        ensure!(
            existing["prior_error"] == prior_error.as_str(),
            "a different repair for this agent is already pending"
        );
        return Ok(
            json!({"status":"requested","agent":agent,"operation_id":existing["operation_id"],
            "prior_error":prior_error,"abandoned_executions":existing["abandoned_executions"],"mutations":false}),
        );
    }
    if dry_run {
        return Ok(
            json!({"status":"planned","agent":agent,"mode":"fresh_session","prior_error":prior_error,
            "abandoned_executions":abandoned,"automatic_replay":false,"mutations":false}),
        );
    }
    let intent = json!({"schema_version":1,"operation_id":uuid::Uuid::new_v4().to_string(),"agent":agent,
        "provider":provider,"mode":"fresh_session","prior_error":prior_error,"session":open["session"],
        "abandoned_executions":abandoned,"requested_at":crate::daemon::now()});
    repairs[agent] = intent.clone();
    let mut audit = intent.clone();
    audit["phase"] = json!("requested");
    let mut store = crate::store::Store::open(&root)?;
    store.save(REPAIRS, &repairs)?;
    store.append(AUDIT, &audit)?;
    store.commit()?;
    Ok(
        json!({"status":"requested","agent":agent,"operation_id":intent["operation_id"],
        "prior_error":intent["prior_error"],"abandoned_executions":intent["abandoned_executions"],
        "next":"run ccb herdr open --recover-stopped"}),
    )
}

/// Pending fresh-session intents keyed by agent, as read by recovery.
pub(super) fn pending(root: &Path) -> Result<Value> {
    let repairs = read(root, REPAIRS)?.unwrap_or_else(|| json!({}));
    ensure!(repairs.is_object(), "invalid operator repair record");
    Ok(repairs)
}
