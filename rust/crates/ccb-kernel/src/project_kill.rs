//! Public project shutdown: stop native authority before retiring its topology.
use anyhow::{ensure, Context, Result};
use serde_json::{json, Value};
use std::{
    path::Path,
    time::{Duration, Instant},
};

pub(crate) const HELP: &str = "ccb-native [--project <directory>|--state-root <dir>] kill [--timeout-ms <0..60000>]\nStops the native daemon/Keeper and closes its owned Herdr workspace. Preserves configuration, histories and unrelated workspaces. Force cleanup is not supported.";

fn live_daemon(root: &Path) -> Result<bool> {
    let lease = crate::startup::load(&root.join("lease.json"))?.unwrap_or(Value::Null);
    let start =
        crate::startup::load(&root.join("kernel/native-start.json"))?.unwrap_or(Value::Null);
    if !start.is_null() && start["process"] != lease["process"] {
        ensure!(!crate::startup::owner_alive(&start)?,
            "native startup is still in progress; observe the same startup before killing its runtime");
    }
    if lease.is_null() {
        return Ok(false);
    }
    ensure!(
        lease["process"].is_object(),
        "legacy or unverified daemon ownership; use its original shutdown path before migration"
    );
    crate::startup::owner_alive(&json!({"pid":lease["ccbd_pid"],"process":lease["process"]}))
}

pub(crate) fn execute(root: &Path, project: Option<&Path>, timeout: Duration) -> Result<Value> {
    if !root.exists() {
        return Ok(json!({"status":"not_running","mutations":false}));
    }
    ensure!(
        cfg!(windows),
        "native project kill currently requires Windows process identities"
    );
    crate::startup::unlinked(root)?;
    let deadline = Instant::now() + timeout;
    if let Some(process) = crate::herdr_open::cancel_start(root, project)? {
        while crate::startup::owner_alive(&json!({"pid":process["pid"],"process":process}))? {
            if Instant::now() >= deadline {
                return Ok(
                    json!({"status":"stopping","startup_cancel_requested":true,"observation_expired":true}),
                );
            }
            std::thread::sleep(Duration::from_millis(25));
        }
    }
    if let Some(project) = project {
        let expected = crate::config::project_id(project)?;
        for record in [
            crate::startup::load(&root.join("lease.json"))?,
            crate::startup::load(&root.join("kernel/native-start.json"))?,
            crate::keeper::status(root)?
                .get("keeper")
                .filter(|v| !v.is_null())
                .cloned(),
        ]
        .into_iter()
        .flatten()
        {
            ensure!(
                record["project_id"] == expected,
                "runtime ownership belongs to another project; refusing shutdown"
            );
        }
    }
    let daemon_live = live_daemon(root)?;
    let keeper_live = crate::keeper::status(root)?["alive"] == true;
    let checkpoint = root.join("configuration.json").is_file();
    ensure!(
        checkpoint || !daemon_live && !keeper_live,
        "native runtime has no committed configuration; explicit startup recovery is required"
    );
    if (daemon_live || keeper_live) && crate::keeper::stop(root)?.is_none() && daemon_live {
        crate::cli::request(root, "shutdown", json!({}))?;
    }
    loop {
        let daemon_live = live_daemon(root)?;
        let keeper_live = crate::keeper::status(root)?["alive"] == true;
        if !daemon_live && !keeper_live {
            break;
        }
        if Instant::now() >= deadline {
            return Ok(json!({"status":"stopping","observation_expired":true,
                "daemon_stopped":!daemon_live,"keeper_stopped":!keeper_live}));
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    if !root.join("herdr/namespace.json").exists() {
        return Ok(
            json!({"status":if checkpoint {"stopped"} else {"not_running"},
            "daemon_stopped":true,"keeper_stopped":true}),
        );
    }
    // Do not rebuild a request from edited/missing ccb.config or expose the
    // protected launch environment. The original committed request owns this
    // namespace; retirement rechecks its plan and all live ownership metadata.
    let initial = !checkpoint;
    let config = match crate::herdr_open::prepared_configuration(root)? {
        Some(config) => Some(config),
        None if checkpoint => Some(crate::restart_config::load(root)?),
        None => None,
    };
    let config = config.context("initial startup ownership configuration is missing; refusing to reconstruct it from the current environment")?;
    let project = Path::new(crate::records::string(&config, "project_root")?);
    ensure!(
        crate::config::state_root(project)?.canonicalize()? == root.canonicalize()?,
        "project runtime reference changed; refusing to close another runtime's namespace"
    );
    let mut request = config
        .get("herdr")
        .filter(|v| v.is_object())
        .context("namespace has no committed Herdr request; explicit recovery is required")?
        .clone();
    request["action"] = json!("retire");
    let result = if initial {
        crate::herdr_namespace::retire_initial(&request)?
    } else {
        crate::herdr_namespace::execute(&request)?
    };
    closed_result(result)
}
fn closed_result(result: Value) -> Result<Value> {
    let retired = result["namespace"]["status"] == "retired";
    Ok(json!({"status":if retired {"closed"} else {"closing"},
        "daemon_stopped":true,"keeper_stopped":true,"namespace_status":result["namespace"]["status"],
        "retirement_operation_id":result["namespace"]["retirement"]["operation_id"],
        "close_outcome_pending":!retired}))
}
