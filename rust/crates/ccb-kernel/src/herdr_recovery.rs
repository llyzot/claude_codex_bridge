//! Recovery of a quiescent stopped server or deliberately retired namespace.
//! Preserve journals/history; retire only the uniquely recorded restored workspace.
use super::{hash, prepare_request, read, server, RECORD};
use anyhow::{ensure, Context, Result};
use serde_json::{json, Value};
use std::{
    fs,
    io::{BufRead, BufReader, Read},
    path::{Path, PathBuf},
};
const STATE: &str = "herdr-recovery.json";
pub(super) fn expected_session(root: &Path, open: &Value, agent: &str) -> Result<Option<Value>> {
    let Some(recovery) = read(root, STATE)? else {
        return Ok(None);
    };
    if recovery["phase"] != "reset" {
        return Ok(None);
    }
    ensure!(
        recovery["operation_id"] == open["last_recovery_id"]
            && recovery["project_id"] == open["project_id"]
            && recovery["session"] == open["session"],
        "recovered native session belongs to another recovery operation"
    );
    let saved = recovery["sessions"]
        .get(agent)
        .context("recovered native session evidence is missing")?;
    if saved["fresh"] == true {
        return Ok(None);
    }
    ensure!(
        saved["session_id"].is_string() && saved["path"].is_string(),
        "recovered native session identity is incomplete"
    );
    Ok(Some(saved.clone()))
}
pub(super) fn verify_session(
    expected: Option<&Value>,
    spec: &Value,
    bound: &Value,
    agent: &str,
) -> Result<bool> {
    let Some(expected) = expected else {
        return Ok(true);
    };
    ensure!(
        expected["provider"] == spec["provider"],
        "recovered native session provider changed"
    );
    if bound["history"].is_null() {
        ensure!(
            bound["discovery"]["session_id"].is_null()
                || bound["discovery"]["session_id"] == expected["session_id"],
            "recovered native session differs from the saved session"
        );
        // A launch argument or an unbound discovery root is not confirmation.
        return Ok(false);
    }
    let history: crate::terminal_history::Binding =
        serde_json::from_value(bound["history"].clone())?;
    ensure!(
        history.session_id == crate::records::string(expected, "session_id")?,
        "recovered native session differs from the saved session"
    );
    let path = if spec["provider"] == "pi" {
        let (_, producer) = crate::pi_terminal::resolve(
            &history,
            crate::records::string(bound, "pi_bridge_owner")?,
            agent,
        )?;
        producer["session_path"].clone()
    } else {
        json!(history.path)
    };
    ensure!(
        crate::pi_terminal::same_session_path(&path, &expected["path"]),
        "recovered native session history path changed"
    );
    Ok(true)
}
fn put(root: &Path, recovery: &Value, open: &Value) -> Result<()> {
    let mut store = crate::store::Store::open(root)?;
    store.save(STATE, recovery)?;
    store.save(RECORD, open)?;
    store.commit()
}
fn ended(identity: &Value) -> Result<()> {
    let pid = identity["pid"]
        .as_u64()
        .filter(|p| *p > 0 && *p <= u32::MAX as u64)
        .context("missing prior process identity")? as u32;
    ensure!(
        identity["created"].is_string() && identity["windows_session"].is_u64(),
        "incomplete prior process lifetime"
    );
    match crate::owned_process::observe_external_process(pid) {
        Ok(current) => ensure!(
            current != *identity,
            "a prior Herdr/provider process is still alive"
        ),
        Err(_) => ensure!(
            crate::daemon::owner_stopped(pid),
            "prior process exit cannot be confirmed"
        ),
    }
    Ok(())
}
pub(super) fn quiet(runtime: &Path, config: &Value) -> Result<crate::Kernel> {
    crate::daemon::verify_owner(runtime)?;
    let mut kernel = crate::Kernel::open(runtime)?;
    let names = config["agents"]
        .as_object()
        .context("missing recovery agents")?
        .keys()
        .cloned()
        .collect::<Vec<_>>();
    let blockers = crate::daemon::recovery_blockers(&mut kernel, &names)?;
    ensure!(blockers.is_empty(),"stopped-server recovery refuses unfinished jobs, mailboxes, callbacks, replies or provider faults");
    Ok(kernel)
}
fn native_header(path: &Path) -> Result<Value> {
    ensure!(
        path.is_absolute(),
        "native recovery history must be absolute"
    );
    for part in path.ancestors() {
        crate::herdr_namespace::unlinked(part)?;
    }
    let mut bytes = Vec::new();
    BufReader::new(fs::File::open(path)?)
        .take(4 * 1024 * 1024 + 1)
        .read_until(b'\n', &mut bytes)?;
    ensure!(
        bytes.len() <= 4 * 1024 * 1024 && bytes.last() == Some(&b'\n'),
        "native history header is incomplete or oversized"
    );
    Ok(serde_json::from_slice(&bytes)?)
}
fn resumes(
    root: &Path,
    runtime: &Path,
    config: &Value,
    open: &Value,
    agents: &Value,
    kernel: &mut crate::Kernel,
) -> Result<Value> {
    let mut result = json!({});
    for (name, spec) in config["agents"].as_object().unwrap() {
        let native = spec["startup_args"].as_array().cloned().unwrap_or_default();
        let selectors = if spec["provider"] == "pi" {
            vec!["--session", "--continue", "--resume", "-c", "-r"]
        } else if spec["provider"] == "claude" {
            vec![
                "--session-id",
                "--continue",
                "--resume",
                "--fork-session",
                "-c",
                "-r",
            ]
        } else {
            vec!["resume", "fork"]
        };
        ensure!(
            !native
                .iter()
                .any(|a| a.as_str().is_some_and(|s| selectors.contains(&s))),
            "explicit startup session selector requires a manual recovery plan"
        );
        let rows = kernel.dispatch(
            "records.latest",
            &json!({"stream":"jobs","identity":"job_id","agent_name":name}),
        )?;
        ensure!(
            rows.as_array()
                .context("invalid recovery job history")?
                .iter()
                .all(|r| r["request"]["project_id"] == config["project_id"]),
            "recovery history belongs to another project"
        );
        if spec["provider"] == "pi" {
            let bridge = read(root, &format!("providers/pi/{name}/terminal-bridge.json"))?
                .context("Pi recovery bridge is missing")?;
            let events = PathBuf::from(crate::records::string(&bridge, "event_root")?);
            let state =
                read(&events, "current.json")?.context("Pi native session evidence is missing")?;
            crate::pi_terminal::validate_producer(&state["bridge"])?;
            ensure!(
                state["owner"] == bridge["owner"]
                    && state["bridge"]["owner"] == bridge["owner"]
                    && state["agent_name"] == *name
                    && state["bridge"]["process"] == agents["agents"][name]["binding"]["process"],
                "Pi recovery producer does not match the recorded process"
            );
            let path = PathBuf::from(crate::records::string(&state["bridge"], "session_path")?);
            let header = native_header(&path)?;
            ensure!(
                header["type"] == "session"
                    && header["id"] == state["session_id"]
                    && crate::pi_terminal::same_session_path(
                        &header["cwd"],
                        &spec["workspace_path"]
                    ),
                "Pi recovery history/session/workspace mismatch"
            );
            result[name] = json!({"provider":"pi","session_id":state["session_id"],"path":path,"args":["--session",path]});
        } else if super::hosted(spec) {
            result[name] = native_host_session(runtime, spec, name, agents)?;
        } else {
            let provider = crate::records::string(spec, "provider")?;
            let cache = read(
                runtime,
                &format!("providers/{provider}/{name}/herdr-history.json"),
            )?
            .unwrap_or(Value::Null);
            let history = if cache.is_object() {
                ensure!(
                    cache["identity"]["process"] == agents["agents"][name]["binding"]["process"],
                    "Codex recovery cache belongs to another process"
                );
                cache["history"].clone()
            } else {
                open["bindings"][name]["history"].clone()
            };
            if history.is_null() {
                ensure!(
                    rows.as_array().unwrap().is_empty(),
                    "Codex has task history but no verified native session binding"
                );
                result[name] = json!({"provider":provider,"fresh":true,"args":[]});
            } else {
                let binding: crate::terminal_history::Binding = serde_json::from_value(history)?;
                ensure!(
                    crate::pi_terminal::same_session_path(
                        &json!(binding.cwd),
                        &spec["workspace_path"]
                    ),
                    "Codex recovery workspace changed"
                );
                crate::terminal_history::provider_baseline(provider, &binding, true)?;
                uuid::Uuid::parse_str(&binding.session_id)?;
                result[name] = json!({"provider":provider,"session_id":binding.session_id,"path":binding.path,"args":[if provider=="claude"{"--resume"}else{"resume"},binding.session_id]});
            }
        }
    }
    Ok(result)
}
pub(super) fn native_host_session(
    runtime: &Path,
    spec: &Value,
    name: &str,
    agents: &Value,
) -> Result<Value> {
    ensure!(
        spec["resume_session"] != false,
        "hosted recovery requires session resumption"
    );
    let saved = read(runtime, &format!("providers/codex/{name}/session.json"))?
        .context("native Codex session record is missing")?;
    let host = &agents["agents"][name]["host_launch"];
    ensure!(
        saved["schema_version"] == 1
            && saved["record_type"] == "codex_app_server_session"
            && saved["agent_name"] == name
            && saved["pid"] == host["host_process"]["pid"]
            && saved["state"]["tui_host"]["path"] == host["path"],
        "native Codex recovery belongs to another host"
    );
    ended(&host["host_process"])?;
    ended(&host["server_process"])?;
    let path = PathBuf::from(crate::records::string(&saved["state"], "path")?);
    let header = native_header(&path)?;
    let id = crate::records::string(&saved["state"], "thread_id")?;
    uuid::Uuid::parse_str(id)?;
    ensure!(
        header["type"] == "session_meta"
            && header["payload"]["id"] == id
            && crate::pi_terminal::same_session_path(
                &header["payload"]["cwd"],
                &spec["workspace_path"]
            ),
        "native Codex recovery history/session/workspace mismatch"
    );
    Ok(json!({"provider":"codex","native_host":true,"session_id":id,"path":path,"args":[]}))
}

fn matches_session(agent: &Value, saved: &Value) -> bool {
    agent["agent"] == saved["provider"]
        && (agent["agent_session"]["kind"] == "id"
            && agent["agent_session"]["value"] == saved["session_id"]
            || agent["agent_session"]["kind"] == "path"
                && crate::pi_terminal::same_session_path(
                    &agent["agent_session"]["value"],
                    &saved["path"],
                ))
}
fn snapshot(request: &Value) -> Result<Value> {
    let value = crate::herdr_agents::rpc(request, vec!["api".into(), "snapshot".into()])?
        ["snapshot"]
        .clone();
    ensure!(
        value["workspaces"].is_array() && value["panes"].is_array() && value["agents"].is_array(),
        "invalid recovery snapshot"
    );
    Ok(value)
}
fn target(snapshot: &Value, recovery: &Value) -> Result<bool> {
    let old = &recovery["namespace"];
    let labelled = snapshot["workspaces"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|w| w["label"] == old["label"])
        .collect::<Vec<_>>();
    ensure!(
        labelled.len() <= 1
            && labelled
                .first()
                .is_none_or(|w| w["workspace_id"] == old["workspace_id"]),
        "restored workspace label/identity is ambiguous"
    );
    let same = snapshot["workspaces"]
        .as_array()
        .unwrap()
        .iter()
        .find(|w| w["workspace_id"] == old["workspace_id"]);
    if let Some(workspace) = same {
        ensure!(
            workspace["label"] == old["label"]
                && (workspace["tokens"]["ccb_native_owner"].is_null()
                    || workspace["tokens"]["ccb_native_owner"] == old["owner_id"]),
            "restored workspace is owned by another operation"
        );
        Ok(true)
    } else {
        Ok(false)
    }
}
fn safe_to_retire(request: &Value, snapshot: &Value, recovery: &Value) -> Result<()> {
    let old = &recovery["namespace"];
    let panes = snapshot["panes"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|p| p["workspace_id"] == old["workspace_id"])
        .collect::<Vec<_>>();
    let bindings = old["bindings"]
        .as_object()
        .context("missing prior pane bindings")?;
    ensure!(
        panes.len() == bindings.len(),
        "restored workspace contains extra or missing panes"
    );
    for (name, binding) in bindings {
        let pane = panes
            .iter()
            .find(|p| p["pane_id"] == binding["pane_id"])
            .context("restored pane IDs changed")?;
        ensure!(
            pane["tokens"]["ccb_native_owner"].is_null()
                || pane["tokens"]["ccb_native_owner"] == old["owner_id"],
            "restored pane has another owner"
        );
        let agents = snapshot["agents"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|a| a["pane_id"] == pane["pane_id"])
            .collect::<Vec<_>>();
        ensure!(agents.len() <= 1, "ambiguous restored pane occupant");
        if let Some(agent) = agents.first() {
            let saved = &recovery["sessions"][name];
            ensure!(
                matches_session(agent, saved)
                    && ["idle", "done"].contains(&agent["agent_status"].as_str().unwrap_or("")),
                "restored Agent is busy, blocked or has an unverified native session"
            );
        } else {
            let info = crate::herdr_agents::rpc(
                request,
                vec![
                    "pane".into(),
                    "process-info".into(),
                    "--pane".into(),
                    crate::records::string(pane, "pane_id")?.into(),
                ],
            )?["process_info"]
                .clone();
            ensure!(
                pane["agent"].is_null()
                    && info["foreground_processes"]
                        .as_array()
                        .is_some_and(|p| p.len() == 1
                            && p[0]["pid"] == info["shell_pid"]
                            && p[0]["pid"].as_u64().is_some_and(|n| n > 0)),
                "restored pane is not an idle shell"
            );
        }
    }
    no_other_session_users(snapshot, recovery)?;
    Ok(())
}
fn no_other_session_users(snapshot: &Value, recovery: &Value) -> Result<()> {
    let old = &recovery["namespace"];
    for agent in snapshot["agents"].as_array().unwrap() {
        if agent["workspace_id"] != old["workspace_id"] {
            ensure!(
                !recovery["sessions"]
                    .as_object()
                    .unwrap()
                    .values()
                    .any(|s| matches_session(agent, s)),
                "a native session is already active in another workspace"
            );
        }
    }
    Ok(())
}
#[allow(clippy::too_many_arguments)]
pub(super) fn step(
    project: &Path,
    config: &Value,
    options: &Value,
    root: &Path,
    open: &mut Value,
    request: &mut Value,
    bridges: &mut Value,
) -> Result<bool> {
    let runtime = crate::config::state_root(project)?;
    let mut recovery = read(root, STATE)?.unwrap_or(Value::Null);
    if recovery.is_null() || recovery["phase"] == "complete" {
        let namespace = read(root, "namespace.json")?.unwrap_or(Value::Null);
        let retired = namespace["status"] == "retired";
        if !retired && options["recover_stopped"] != true {
            return Ok(true);
        }
        let status = crate::herdr_agents::rpc(
            request,
            vec!["status".into(), "server".into(), "--json".into()],
        )?;
        let status = status.get("server").unwrap_or(&status);
        if !retired && status["running"] == true {
            return Ok(true);
        }
        ensure!(
            status["running"].is_boolean(),
            "recovery requires confirmed server state"
        );
        if namespace.is_null() {
            return Ok(true);
        }
        if !retired || status["running"] == false {
            ended(&open["server_process"])?;
        }
        let mut inspect = request.clone();
        inspect["action"] = json!("inspect");
        if retired {
            crate::herdr_namespace::inspect_retired(request)?;
        } else {
            crate::herdr_namespace::inspect_prepared(request)?;
        }
        let agents = crate::herdr_agents::execute(&inspect)?["mount"].clone();
        for row in agents["agents"]
            .as_object()
            .context("missing mounted recovery agents")?
            .values()
        {
            if retired
                && crate::startup::owner_alive(&json!({
                    "pid":row["binding"]["process"]["pid"],"process":row["binding"]["process"]
                }))?
            {
                open["phase"] = json!("waiting_for_retired_processes");
                return Ok(false);
            }
            ended(&row["binding"]["process"])?;
        }
        let mut kernel = quiet(&runtime, config)?;
        let sessions = resumes(root, &runtime, config, open, &agents, &mut kernel)?;
        recovery = json!({"schema_version":1,"operation_id":uuid::Uuid::new_v4().to_string(),"project_id":config["project_id"],"session":request["session"],
            "kind":if retired {"retired_namespace"} else {"stopped_server"},
            "phase":"starting_server","source_hash":open["request_hash"],"previous_open":open,"namespace":namespace,"agents":agents,"sessions":sessions,"close_permitted":false});
        if !retired || status["running"] == false {
            open["server_start_permitted"] = json!(false);
            open["server_process"] = Value::Null;
        }
        open["workspace_id"] = Value::Null;
        open["phase"] = json!("recovering_server");
        put(root, &recovery, open)?;
    }
    ensure!(
        recovery["schema_version"] == 1
            && recovery["project_id"] == config["project_id"]
            && recovery["session"] == request["session"],
        "recovery operation scope changed"
    );
    if recovery["phase"] == "reset" {
        ensure!(
            open["last_recovery_id"] == recovery["operation_id"],
            "recovery reset identity mismatch"
        );
        return Ok(true);
    }
    ensure!(
        recovery["source_hash"] == open["request_hash"],
        "recovery source configuration changed"
    );
    let _kernel = quiet(&runtime, config)?;
    if !server(request, root, open)? {
        return Ok(false);
    }
    let retired = recovery["kind"] == "retired_namespace";
    let identity = &open["server_process"];
    if !retired || identity.is_object() {
        ensure!(
            crate::owned_process::observe_external_process(
                identity["pid"]
                    .as_u64()
                    .context("recovery has no server process")? as u32
            )? == *identity,
            "recovery server lifetime changed"
        );
    }
    let mut observed = snapshot(request)?;
    no_other_session_users(&observed, &recovery)?;
    if target(&observed, &recovery)? {
        ensure!(
            !retired,
            "retired workspace reappeared; refusing another close or replacement"
        );
        safe_to_retire(request, &observed, &recovery)?;
        if recovery["close_permitted"] != true {
            recovery["close_permitted"] = json!(true);
            recovery["phase"] = json!("retiring_workspace");
            open["phase"] = json!("recovering_workspace");
            put(root, &recovery, open)?;
            let _ = crate::herdr_agents::rpc(
                request,
                vec![
                    "workspace".into(),
                    "close".into(),
                    crate::records::string(&recovery["namespace"], "workspace_id")?.into(),
                ],
            );
        }
        observed = snapshot(request)?;
        if target(&observed, &recovery)? {
            return Ok(false);
        }
    }
    no_other_session_users(&observed, &recovery)?;
    let mut resume = json!({});
    for (name, session) in recovery["sessions"].as_object().unwrap() {
        resume[name] = session["args"].clone();
    }
    let (next, next_bridges) = prepare_request(project, config, options, &resume)?;
    let next_hash = hash(&json!([
        next,
        options["history_roots"],
        options["bindings"]
    ]))?;
    let next_open = json!({"schema_version":1,"project_id":config["project_id"],"session":request["session"],"request_hash":next_hash,
        "server_start_permitted":true,"server_process":open["server_process"],"phase":"opening","bindings":{},"resume_args":resume,
        "last_recovery_id":recovery["operation_id"],"observer_instance":options["opener_instance"]});
    recovery["phase"] = json!("reset");
    let mut store = crate::store::Store::open(root)?;
    store.append("herdr-recoveries.jsonl", &recovery)?;
    store.remove("namespace.json")?;
    store.remove("herdr-agents.json")?;
    store.save(RECORD, &next_open)?;
    store.save(STATE, &recovery)?;
    store.commit()?;
    *request = next;
    *bridges = next_bridges;
    *open = next_open;
    Ok(true)
}
pub(super) fn finish(root: &Path, open: &Value) -> Result<()> {
    let Some(mut recovery) = read(root, STATE)? else {
        return Ok(());
    };
    if recovery["phase"] == "reset" && open["last_recovery_id"] == recovery["operation_id"] {
        recovery["phase"] = json!("complete");
        recovery["new_workspace_id"] = open["workspace_id"].clone();
        put(root, &recovery, open)?;
    }
    Ok(())
}
