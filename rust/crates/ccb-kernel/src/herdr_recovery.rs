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
#[path = "herdr_work_handoff.rs"]
mod work_handoff;
pub(super) use work_handoff::{authorize_start, handoff_allowed};

// An uncommitted recovery may have prepared a layout but transferred neither
// authority nor work. Once that layout is explicitly retired, restore the last
// retired checkpoint transactionally. Never roll back a sent launch or a live
// daemon, and never reconstruct the checkpoint from the caller's environment.
pub(super) fn restore_retired_checkpoint(runtime: &Path, root: &Path) -> Result<()> {
    let Some(mut recovery) = read(root, STATE)? else {
        return Ok(());
    };
    if recovery["phase"] != "reset"
        || !recovery["work_handoff"].is_null()
        || recovery["namespace"]["status"] != "retired"
    {
        return Ok(());
    }
    let Some(namespace) = read(root, "namespace.json")? else {
        return Ok(());
    };
    if namespace["status"] != "retired"
        || namespace["owner_id"] == recovery["namespace"]["owner_id"]
    {
        return Ok(());
    }
    let opened = read(root, RECORD)?.context("uncommitted recovery open record missing")?;
    ensure!(
        opened["last_recovery_id"] == recovery["operation_id"] && opened["daemon"].is_null(),
        "recovery authority may already have transferred"
    );
    if let Some(agents) = read(root, "herdr-agents.json")? {
        ensure!(
            agents["agents"]
                .as_object()
                .is_some_and(|rows| rows.values().all(|r| r["sent"] == false)),
            "uncommitted recovery contains a sent launch; observation is required"
        );
    }
    crate::daemon::verify_owner(runtime)?;
    ensure!(
        crate::keeper::status(runtime)?["alive"] == false,
        "recovery Keeper is still active"
    );
    let authority = crate::Kernel::open(runtime)?;
    let committed = authority
        .store
        .json("configuration.json")?
        .context("recovery configuration missing")?;
    ensure!(
        committed["config_fingerprint"] == recovery["prior_config_fingerprint"],
        "recovery configuration has already committed"
    );
    let prepared = super::prepared_configuration(runtime)?
        .context("uncommitted namespace configuration missing")?;
    crate::herdr_namespace::inspect_retired(&prepared["herdr"])?;
    let observed = snapshot(&prepared["herdr"])?;
    ensure!(
        !target(&observed, &recovery)?,
        "previous retired namespace reappeared"
    );
    let mut previous = recovery["previous_open"].clone();
    previous["server_process"] = opened["server_process"].clone();
    let owner = crate::records::string(&namespace, "owner_id")?;
    uuid::Uuid::parse_str(owner)?;
    let mut store = crate::store::Store::open(root)?;
    store.save(
        &format!("archived-partial/{owner}/namespace.json"),
        &namespace,
    )?;
    recovery["phase"] = json!("rolled_back_before_activation");
    recovery["discarded_open"] = opened;
    store.append("herdr-recoveries.jsonl", &recovery)?;
    store.save("namespace.json", &recovery["namespace"])?;
    store.save("herdr-agents.json", &recovery["agents"])?;
    store.save(RECORD, &previous)?;
    store.remove("open-configuration.json")?;
    store.remove(STATE)?;
    store.commit()
}
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
fn unused_pi_session(state: &Value, events: &Path, rows: &Value) -> Result<bool> {
    if state["native_unwritten_empty"] != true
        || !rows.as_array().is_some_and(|rows| rows.is_empty())
    {
        return Ok(false);
    }
    let path = events.join(format!(
        "{}.jsonl",
        crate::records::string(&state["bridge"], "instance")?
    ));
    ensure!(
        state["path"].as_str().map(Path::new) == Some(path.as_path()),
        "Pi empty-session event path changed"
    );
    crate::herdr_namespace::unlinked(&path)?;
    let mut bytes = Vec::new();
    fs::File::open(&path)?.take(65537).read_to_end(&mut bytes)?;
    if bytes.len() > 65536 || !bytes.ends_with(b"\n") {
        return Ok(false);
    }
    let mut lines = std::str::from_utf8(&bytes)?.lines();
    let header: Value = serde_json::from_str(lines.next().context("missing Pi event header")?)?;
    ensure!(
        header["type"] == "session_meta"
            && header["payload"]["id"] == state["session_id"]
            && header["payload"]["ccb_bridge"] == state["bridge"],
        "Pi empty-session evidence changed"
    );
    for (index, line) in lines.enumerate() {
        let event: Value = serde_json::from_str(line)?;
        if event["type"] != "pi_event"
            || event["bridge"] != state["bridge"]
            || event["seq"] != json!(index + 1)
            || event["payload"]["type"] != "bridge_shutdown"
        {
            return Ok(false);
        }
    }
    Ok(true)
}
fn resumes(
    root: &Path,
    runtime: &Path,
    config: &Value,
    open: &Value,
    agents: &Value,
    kernel: &mut crate::Kernel,
    prior: &Value,
) -> Result<Value> {
    let mut result = json!({});
    let repairs = super::repair::pending(root)?;
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
        let session = (|| -> Result<()> {
            // A terminal fault only lost observation; stopped-history
            // reconciliation settles its work. A native host owns processes.
            if super::hosted(spec)
                && read(
                    runtime,
                    &crate::provider_fault::path(crate::records::string(spec, "provider")?, name),
                )?
                .is_some_and(|fault| fault["status"] == "blocked")
            {
                anyhow::bail!("provider fault requires explicit recovery");
            }
            if spec["provider"] == "pi" {
                let bridge = read(root, &format!("providers/pi/{name}/terminal-bridge.json"))?
                    .context("Pi recovery bridge is missing")?;
                let events = PathBuf::from(crate::records::string(&bridge, "event_root")?);
                let state = read(&events, "current.json")?
                    .context("Pi native session evidence is missing")?;
                crate::pi_terminal::validate_producer(&state["bridge"])?;
                ensure!(
                    state["owner"] == bridge["owner"]
                        && state["bridge"]["owner"] == bridge["owner"]
                        && state["agent_name"] == *name
                        && (state["bridge"]["process"]
                            == agents["agents"][name]["binding"]["process"]
                            || unlaunched_and_gone(agents, name, &state["bridge"]["process"])?),
                    "Pi recovery producer does not match the recorded process"
                );
                let path = PathBuf::from(crate::records::string(&state["bridge"], "session_path")?);
                if matches!(fs::symlink_metadata(&path),Err(ref e) if e.kind()==std::io::ErrorKind::NotFound)
                    && unused_pi_session(&state, &events, &rows)?
                {
                    result[name] = json!({"provider":"pi","fresh":true,"args":[],"reason":"unused_native_session_never_persisted"});
                    return Ok(());
                }
                let header = native_header(&path).with_context(||format!("Pi recovery history is unavailable: {}. A used or unverified session will not be replaced with a fresh one",path.display()))?;
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
                let mut history = if cache.is_object() {
                    ensure!(
                        cache["identity"]["process"]
                            == agents["agents"][name]["binding"]["process"]
                            || unlaunched_and_gone(agents, name, &cache["identity"]["process"])?
                            || resumed_same_session(root, open, name, &cache["history"])?,
                        "Codex recovery cache belongs to another process"
                    );
                    cache["history"].clone()
                } else {
                    open["bindings"][name]["history"].clone()
                };
                if prior["agents"][name]["herdr_discovery"].is_object() {
                    let running = rows
                        .as_array()
                        .unwrap()
                        .iter()
                        .filter(|row| row["status"] == "running")
                        .collect::<Vec<_>>();
                    ensure!(
                        running.len() <= 1,
                        "multiple unfinished deferred executions for one agent"
                    );
                    if let Some(job) = running.first() {
                        let recovered = crate::herdr_discovery::recover(
                            &prior["agents"][name],
                            job,
                            &crate::herdr_agents::recorded_identity(agents, name),
                        )?
                        .context(
                            "unfinished deferred execution has no uniquely verified native session",
                        )?;
                        history = json!(recovered.binding);
                        kernel.store.save(&format!("providers/{provider}/{name}/herdr-history.json"),
                            &json!({"schema_version":1,"identity":crate::herdr_agents::recorded_identity(agents,name),
                            "discovery":prior["agents"][name]["herdr_discovery"],"history":history}))?;
                    }
                }
                if history.is_null() {
                    ensure!(
                        rows.as_array().unwrap().is_empty(),
                        "Codex has task history but no verified native session binding"
                    );
                    result[name] = json!({"provider":provider,"fresh":true,"args":[]});
                } else {
                    let binding: crate::terminal_history::Binding =
                        serde_json::from_value(history)?;
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
            Ok(())
        })();
        let faulted = super::hosted(spec)
            && read(
                runtime,
                &crate::provider_fault::path(crate::records::string(spec, "provider")?, name),
            )?
            .is_some_and(|fault| fault["status"] == "blocked");
        if let (Err(error), Some(repair)) = (&session, repairs.get(name).filter(|r| r.is_object()))
        {
            if !faulted {
                // The operator explicitly accepted losing the unverifiable session.
                // Its sent work closes without replay; the next launch is fresh.
                ensure!(
                    repair["mode"] == "fresh_session"
                        && repair["provider"] == spec["provider"]
                        && repair["session"] == open["session"],
                    "operator repair does not match this agent"
                );
                let provider = crate::records::string(spec, "provider")?;
                let cache = format!("providers/{provider}/{name}/herdr-history.json");
                if kernel.store.json(&cache)?.is_some() {
                    kernel.store.remove(&cache)?;
                }
                result[name] = json!({"provider":provider,"fresh":true,"args":[],"reason":"operator_fresh_session",
                    "operator_repair":repair["operation_id"],"abandoned_error":error.to_string()});
                continue;
            }
        }
        if let Err(error) = session {
            // Retain the last confirmed identity for ownership checks. Missing
            // execution evidence prevents activation, not recognition of an
            // already-owned restored pane. Never infer an id from a filename.
            let row = &agents["agents"][name];
            let reference = &row["agent_session"];
            let id = if super::hosted(spec) {
                row["host_launch"]["thread_id"].clone()
            } else if reference["kind"] == "id" {
                reference["value"].clone()
            } else {
                open["bindings"][name]["history"]["session_id"].clone()
            };
            let path = if reference["kind"] == "path" {
                reference["value"].clone()
            } else {
                open["bindings"][name]["history"]["path"].clone()
            };
            result[name] = json!({"provider":spec["provider"],"args":[],"session_id":id,"path":path,
                "history":open["bindings"][name]["history"],"binding":open["bindings"][name],"blocked_reason":error.to_string()});
        }
    }
    Ok(result)
}
/// Evidence from an earlier generation still binds an agent that the latest
/// generation never launched, once that earlier process has exited.
fn unlaunched_and_gone(agents: &Value, name: &str, process: &Value) -> Result<bool> {
    let row = &agents["agents"][name];
    Ok(row["binding"]["process"].is_null()
        && row["sent"] != true
        && process.is_object()
        && !crate::startup::owner_alive(&json!({"pid":process["pid"],"process":process}))?)
}
/// The latest generation resumed and verified this exact session after the
/// cache was written; only the terminal process changed.
fn resumed_same_session(root: &Path, open: &Value, name: &str, history: &Value) -> Result<bool> {
    let Some(previous) = read(root, STATE)? else {
        return Ok(false);
    };
    Ok(previous["operation_id"] == open["last_recovery_id"]
        && open["bindings"][name].is_object()
        && open["initialization_errors"][name].is_null()
        && history["session_id"].is_string()
        && previous["sessions"][name]["session_id"] == history["session_id"])
}
fn operator_sessions(sessions: &Value) -> Value {
    sessions
        .as_object()
        .map(|sessions| {
            sessions
                .iter()
                .filter_map(|(name, s)| {
                    s["operator_repair"]
                        .as_str()
                        .map(|id| (name.clone(), json!(id)))
                })
                .collect::<serde_json::Map<String, Value>>()
        })
        .unwrap_or_default()
        .into()
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
            && saved["session_id"]
                .as_str()
                .is_some_and(|id| !id.is_empty())
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
    if recovery["phase"] == "reset"
        && read(root, "namespace.json")?.is_some_and(|n| n["status"] == "retired")
    {
        // A registered generation can be stopped before all sessions activate.
        // Its next open is a new recovery operation, not a replay of the old one.
        recovery = Value::Null;
    } else if recovery["phase"] == "reset"
        && !recovery["work_handoff"].is_null()
        && options["recover_stopped"] == true
        && open["last_recovery_id"] == recovery["operation_id"]
    {
        // A held actor keeps its generation from reaching running, so the
        // recovery never completes. Once that generation took authority and its
        // server has stopped, the next explicit recovery is a new operation.
        let status = crate::herdr_agents::rpc(
            request,
            vec!["status".into(), "server".into(), "--json".into()],
        )?;
        if status.get("server").unwrap_or(&status)["running"] == false {
            recovery = Value::Null;
        }
    }
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
            // A held agent was never launched in its generation; it has no process.
            if retired
                && row["binding"]["process"].is_object()
                && crate::startup::owner_alive(&json!({
                    "pid":row["binding"]["process"]["pid"],"process":row["binding"]["process"]
                }))?
            {
                open["phase"] = json!("waiting_for_retired_processes");
                return Ok(false);
            }
            if row["binding"]["process"].is_object() {
                ended(&row["binding"]["process"])?;
            } else {
                ensure!(
                    row["sent"] == false,
                    "unconfirmed provider launch cannot be retired"
                );
            }
        }
        crate::daemon::verify_owner(&runtime)?;
        let mut kernel = crate::Kernel::open(&runtime)?;
        let committed = kernel
            .store
            .json("configuration.json")?
            .context("recovery configuration missing")?;
        let lease = kernel
            .store
            .json("lease.json")?
            .context("recovery owner record missing")?;
        ensure!(
            lease["project_id"] == committed["project_id"]
                && lease["config_signature"] == committed["config_fingerprint"],
            "recovery configuration and owner record disagree"
        );
        let prior = crate::restart_config::restore(&committed)?;
        ensure!(
            prior["herdr"] == *request,
            "recovery source configuration changed"
        );
        // A held generation never binds its agent. Carry the last verified
        // history binding forward so restored evidence resumes that session.
        let mut carried = open.clone();
        let previous = read(root, STATE)?.filter(|r| r["operation_id"] == open["last_recovery_id"]);
        for name in config["agents"].as_object().unwrap().keys() {
            if !carried["bindings"][name].is_null() {
                continue;
            }
            // A binding dropped after its provider died, or one carried
            // through a held generation, still identifies the session.
            let held = &previous
                .as_ref()
                .filter(|_| open["initialization_errors"][name].is_string())
                .map(|p| p["sessions"][name].clone())
                .unwrap_or(Value::Null);
            if open["retained_bindings"][name].is_object() {
                carried["bindings"][name] = open["retained_bindings"][name].clone();
            } else if held["binding"].is_object() {
                carried["bindings"][name] = held["binding"].clone();
            } else if held["history"].is_object() {
                carried["bindings"][name] = json!({"history":held["history"]});
            }
        }
        let mut historical = crate::herdr_activation::historical(&prior, &carried["bindings"]);
        let sessions = resumes(
            root,
            &runtime,
            config,
            &carried,
            &agents,
            &mut kernel,
            &historical,
        )?;
        historical["herdr_activation"]["blocked_agents"] =
            crate::herdr_activation::blocked_sessions(&sessions);
        historical["herdr_activation"]["operator_fresh_session"] = operator_sessions(&sessions);
        let (mut offline, work) =
            crate::daemon::Daemon::recover_closed_work(kernel, historical, &agents)?;
        let mut recovery_config = config.clone();
        recovery_config["herdr_activation"]["blocked_agents"] =
            crate::herdr_activation::blocked_sessions(&sessions);
        work_handoff::recoverable(&mut offline.kernel, &recovery_config)?;
        recovery = json!({"schema_version":1,"operation_id":uuid::Uuid::new_v4().to_string(),"project_id":config["project_id"],"session":request["session"],
            "kind":if retired {"retired_namespace"} else {"stopped_server"},
            "phase":"starting_server","source_hash":open["request_hash"],"previous_open":open,"namespace":namespace,"agents":agents,"sessions":sessions,"close_permitted":false,
            "prior_config_fingerprint":committed["config_fingerprint"],"work_recovery":work});
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
    crate::daemon::verify_owner(&runtime)?;
    let mut authority = crate::Kernel::open(&runtime)?;
    let mut recovery_config = config.clone();
    recovery_config["herdr_activation"]["blocked_agents"] =
        crate::herdr_activation::blocked_sessions(&recovery["sessions"]);
    work_handoff::recoverable(&mut authority, &recovery_config)?;
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
    let (mut next, next_bridges) = prepare_request(project, config, options, &resume)?;
    // The request above was matched to the committed owner configuration.
    // Recovery changes session selectors, not its resolved launch environment.
    for (name, spec) in next["config"]["agents"].as_object_mut().unwrap() {
        spec["env"]["PATH"] = request["config"]["agents"][name]["env"]["PATH"].clone();
    }
    let next_hash = hash(&json!([
        next,
        options["history_roots"],
        options["bindings"]
    ]))?;
    let next_open = json!({"schema_version":1,"project_id":config["project_id"],"session":request["session"],"request_hash":next_hash,
        "server_start_permitted":true,"server_process":open["server_process"],"phase":"opening","bindings":{},"resume_args":resume,
        "initialization_errors":crate::herdr_activation::blocked_sessions(&recovery["sessions"]),
        "last_recovery_id":recovery["operation_id"],"observer_instance":options["opener_instance"]});
    recovery["phase"] = json!("reset");
    let mut store = crate::store::Store::open(root)?;
    let mut repairs = super::repair::pending(root)?;
    if repairs.as_object().is_some_and(|m| !m.is_empty()) {
        let names: Vec<String> = repairs.as_object().unwrap().keys().cloned().collect();
        for name in names {
            let session = &recovery["sessions"][&name];
            // Applied here, or superseded because verified evidence resumed.
            // An agent that is still held keeps its pending decision.
            let phase = if session["operator_repair"] == repairs[&name]["operation_id"] {
                "applied"
            } else if session.is_object() && session["blocked_reason"].is_null() {
                "superseded"
            } else {
                continue;
            };
            let mut audit = repairs[&name].clone();
            audit["phase"] = json!(phase);
            audit["recovery_id"] = recovery["operation_id"].clone();
            audit["recorded_at"] = json!(crate::daemon::now());
            store.append(super::repair::AUDIT, &audit)?;
            repairs.as_object_mut().unwrap().remove(&name);
        }
        store.save(super::repair::REPAIRS, &repairs)?;
    }
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
