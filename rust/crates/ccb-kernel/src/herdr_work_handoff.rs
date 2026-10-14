//! A stopped generation may carry work only to the same configuration and
//! verified native sessions. This receipt cannot authorize arbitrary reloads.
use super::{read, STATE};
use anyhow::{ensure, Context, Result};
use serde_json::{json, Value};
use std::path::Path;

pub(super) fn recoverable(kernel: &mut crate::Kernel, config: &Value) -> Result<()> {
    let agents = config["agents"]
        .as_object()
        .context("recovery agents missing")?;
    for path in kernel.store.list("mailboxes")? {
        ensure!(
            agents.contains_key(path.split('/').nth(1).context("invalid mailbox path")?),
            "recovery mailbox agent configuration changed"
        );
    }
    let mut jobs = Vec::new();
    for agent in agents.keys() {
        let rows = kernel.dispatch(
            "records.latest",
            &json!({"stream":"jobs","identity":"job_id","agent_name":agent}),
        )?;
        jobs.extend(
            rows.as_array()
                .context("invalid recovery journal")?
                .iter()
                .cloned(),
        );
    }
    for job in &jobs {
        let agent = crate::records::string(job, "agent_name")?;
        ensure!(
            job["request"]["project_id"] == config["project_id"]
                && agents
                    .get(agent)
                    .is_some_and(|spec| spec["provider"] == job["provider"]),
            "recovery job configuration changed"
        );
        if !["completed", "failed", "incomplete", "cancelled"]
            .contains(&job["status"].as_str().unwrap_or(""))
        {
            ensure!(
                ["accepted", "queued", "pending"].contains(&job["status"].as_str().unwrap_or(""))
                    && job["provider_options"]["native_execution_instance"].is_null()
                    && job["provider_options"]["herdr_dispatch"].is_null(),
                "unfinished execution still lacks safe reconciliation"
            );
        }
    }
    let names = agents.keys().cloned().collect::<Vec<_>>();
    ensure!(
        crate::daemon::recovery_blockers(kernel, &names)?
            .iter()
            .all(|b| b["reason"] != "provider_fault"),
        "recovery requires explicit provider fault repair"
    );
    Ok(())
}

fn normalized(config: &Value, resume: &Value) -> Result<Value> {
    let mut result = config.clone();
    result
        .as_object_mut()
        .context("invalid recovery configuration")?
        .remove("startup_id");
    for spec in result["agents"]
        .as_object_mut()
        .context("recovery agents missing")?
        .values_mut()
    {
        for key in ["herdr_history", "herdr_discovery", "pi_bridge_owner"] {
            spec.as_object_mut()
                .context("invalid recovery agent")?
                .remove(key);
        }
    }
    for (name, args) in result["herdr"]["interactive_args"]
        .as_object_mut()
        .context("recovery terminal arguments missing")?
    {
        let args = args.as_array_mut().context("invalid terminal arguments")?;
        if let Some(suffix) = resume.get(name) {
            let suffix = suffix.as_array().context("invalid resume arguments")?;
            ensure!(args.ends_with(suffix), "native resume arguments changed");
            args.truncate(args.len() - suffix.len());
        }
    }
    Ok(result)
}

fn quiet_generation(kernel: &mut crate::Kernel, prior: &Value) -> Result<bool> {
    let names = prior["agents"]
        .as_object()
        .context("recovery agents missing")?
        .keys()
        .cloned()
        .collect::<Vec<_>>();
    Ok(crate::daemon::recovery_blockers(kernel, &names)?.is_empty())
}

pub(in crate::herdr_open) fn partial_start_allowed(runtime: &Path, prior: &Value) -> Result<bool> {
    crate::daemon::verify_owner(runtime)?;
    quiet_generation(&mut crate::Kernel::open(runtime)?, prior)
}

fn validate(
    kernel: &mut crate::Kernel,
    root: &Path,
    recovery: &Value,
    open: &Value,
    desired: &Value,
) -> Result<Value> {
    ensure!(
        recovery["schema_version"] == 1
            && recovery["phase"] == "reset"
            && recovery["operation_id"].is_string()
            && open["last_recovery_id"] == recovery["operation_id"]
            && open["project_id"] == recovery["project_id"]
            && open["session"] == recovery["session"]
            && desired["project_id"] == recovery["project_id"]
            && desired["herdr"]["session"] == recovery["session"],
        "native work handoff recovery identity changed"
    );
    let committed = kernel
        .store
        .json("configuration.json")?
        .context("recovery configuration missing")?;
    ensure!(
        committed["config_fingerprint"] == recovery["prior_config_fingerprint"],
        "native work handoff old generation changed"
    );
    let prior = crate::restart_config::restore(&committed)?;
    let partial = prior["agents"]
        .as_object()
        .context("recovery agents missing")?
        .keys()
        .any(|name| desired["agents"].get(name).is_none());
    if partial {
        // Recheck under the writer used to authorize this generation. A queued
        // job, pending reply or callback must still take the full handoff path.
        ensure!(
            quiet_generation(kernel, &prior)?,
            "partial recovery would strand unfinished work"
        );
    }
    let mut resume = json!({});
    for (name, saved) in recovery["sessions"]
        .as_object()
        .context("recovery sessions missing")?
    {
        resume[name] = saved["args"].clone();
        let spec = &desired["agents"][name];
        if partial && spec.is_null() {
            continue;
        }
        if !super::super::hosted(spec) {
            ensure!(
                super::verify_session(
                    if saved["fresh"] == true {
                        None
                    } else {
                        Some(saved)
                    },
                    spec,
                    &open["bindings"][name],
                    name
                )?,
                "native work handoff session unconfirmed"
            );
        }
    }
    let mut expected = normalized(&prior, &recovery["previous_open"]["resume_args"])?;
    if partial {
        expected["agents"]
            .as_object_mut()
            .unwrap()
            .retain(|name, _| desired["agents"].get(name).is_some());
    }
    ensure!(
        expected == normalized(desired, &resume)?,
        "native work handoff substantive configuration changed"
    );
    ensure!(
        crate::config::fingerprint(&super::super::daemon_configuration(&desired["herdr"], open))?
            == crate::config::fingerprint(desired)?,
        "native work handoff bindings changed"
    );
    let namespace = read(root, "namespace.json")?.context("recovery namespace missing")?;
    let agents = read(root, "herdr-agents.json")?.unwrap_or(Value::Null);
    ensure!(
        namespace["status"] == "prepared"
            && namespace["owner_id"] != recovery["namespace"]["owner_id"]
            && namespace["workspace_id"] == open["workspace_id"],
        "native work handoff namespace changed"
    );
    if agents.is_null() {
        ensure!(
            partial
                && desired["agents"]
                    .as_object()
                    .is_some_and(|a| !a.is_empty() && a.values().all(super::super::hosted)),
            "recovery mounted agents missing"
        );
    } else {
        ensure!(
            agents["namespace_owner"] == namespace["owner_id"]
                && agents["workspace_id"] == namespace["workspace_id"]
                && agents["plan"]["namespace_plan_hash"] == namespace["plan_hash"],
            "native work handoff namespace changed"
        );
    }
    recoverable(kernel, if partial { &prior } else { desired })?;
    Ok(
        json!({"schema_version":1,"operation_id":recovery["operation_id"],
        "prior_config_fingerprint":committed["config_fingerprint"],
        "desired_config_fingerprint":crate::config::fingerprint(desired)?,"partial_quiet_start":partial,
        "namespace_owner":namespace["owner_id"],"workspace_id":namespace["workspace_id"],"plan_hash":namespace["plan_hash"],"agents_plan_hash":agents["plan_hash"]}),
    )
}

pub(in crate::herdr_open) fn authorize_start(
    runtime: &Path,
    root: &Path,
    open: &Value,
    config: &Value,
) -> Result<()> {
    let Some(mut recovery) = read(root, STATE)? else {
        return Ok(());
    };
    if recovery["phase"] != "reset" {
        return Ok(());
    }
    let desired = json!(crate::config::fingerprint(config)?);
    if recovery["work_handoff"]["desired_config_fingerprint"] == desired
        && read(runtime, "configuration.json")?.is_some_and(|v| v["config_fingerprint"] == desired)
    {
        // The authorized generation already started; hosted panes may still
        // be attaching. Do not reacquire its writer or authorize a new owner.
        return Ok(());
    }
    crate::daemon::verify_owner(runtime)?;
    let mut kernel = crate::Kernel::open(runtime)?;
    recovery["work_handoff"] = validate(&mut kernel, root, &recovery, open, config)?;
    let mut store = crate::store::Store::open(root)?;
    store.save(STATE, &recovery)?;
    store.commit()
}

pub(in crate::herdr_open) fn handoff_allowed(
    kernel: &mut crate::Kernel,
    config: &Value,
) -> Result<bool> {
    let root = kernel.store.root().join("herdr");
    let Some(recovery) = read(&root, STATE)? else {
        return Ok(false);
    };
    if recovery["phase"] != "reset" || recovery["work_handoff"].is_null() {
        return Ok(false);
    }
    let open = read(&root, super::RECORD)?.context("recovery open record missing")?;
    Ok(validate(kernel, &root, &recovery, &open, config)? == recovery["work_handoff"])
}
