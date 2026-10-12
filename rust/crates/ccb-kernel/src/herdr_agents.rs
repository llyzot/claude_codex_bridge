//! Original provider TUI launch in an already-owned Herdr namespace.
//! Startup/lifecycle evidence never completes a CCB task. No pane input fallback.
use crate::{herdr_namespace, herdr_transport, store::Store};
use anyhow::{ensure, Context, Result};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{io::Read, path::Path};
const RECORD: &str = "herdr-agents.json";
fn contract(name: &str, value: &Value) -> Result<()> {
    static VALIDATORS: std::sync::LazyLock<Vec<(String, jsonschema::Validator)>> =
        std::sync::LazyLock::new(|| {
            let base: Value = serde_json::from_str(include_str!("contracts/herdr-0.9.0.json"))
                .expect("embedded Herdr contract");
            ["AgentInfo", "PaneProcessInfo"]
                .into_iter()
                .map(|name| {
                    let mut schema = base.clone();
                    schema["$ref"] = json!(format!("#/schemas/success_response/$defs/{name}"));
                    (
                        name.into(),
                        jsonschema::validator_for(&schema).expect("valid embedded Herdr schema"),
                    )
                })
                .collect()
        });
    ensure!(
        VALIDATORS
            .iter()
            .find(|(n, _)| n == name)
            .context("unknown Herdr contract")?
            .1
            .is_valid(value),
        "Herdr response does not match the installed 0.9 Agent/process contract"
    );
    Ok(())
}
fn hash(value: &Value) -> Result<String> {
    Ok(format!("{:x}", Sha256::digest(serde_json::to_vec(value)?)))
}
fn pi_hook_ready(agent: &Value) -> bool {
    agent["agent"] == "pi"
        && agent["interactive_ready"].is_null()
        && agent["screen_detection_skipped"] == true
        && agent["agent_session"]["source"] == "herdr:pi"
        && ["idle", "done"].contains(&agent["agent_status"].as_str().unwrap_or(""))
}
fn text<'a>(value: &'a Value, key: &str) -> Result<&'a str> {
    value[key]
        .as_str()
        .context("missing Herdr agent identity field")
}
fn args(request: &Value, agent: &str) -> Result<Vec<String>> {
    let map = &request["interactive_args"];
    if map.is_null() {
        return Ok(Vec::new());
    }
    let map = map
        .as_object()
        .context("interactive_args must be an object")?;
    let args: Vec<String> = map
        .get(agent)
        .map(|v| serde_json::from_value(v.clone()))
        .transpose()?
        .unwrap_or_default();
    ensure!(
        args.len() <= 64
            && args.iter().all(|s| !s.contains('\0'))
            && args.iter().map(String::len).sum::<usize>() <= 16384,
        "invalid interactive provider arguments"
    );
    Ok(args)
}
fn plan(request: &Value) -> Result<Value> {
    let namespace = herdr_namespace::plan(request)?;
    if let Some(values) = request["interactive_args"].as_object() {
        ensure!(
            values
                .keys()
                .all(|name| request["config"]["agents"].get(name).is_some()),
            "interactive arguments target an unknown agent"
        );
    }
    let timeout = request["startup_timeout_ms"].as_u64().unwrap_or(30000);
    ensure!(
        request["startup_timeout_ms"].is_null() || request["startup_timeout_ms"].is_u64(),
        "invalid agent startup timeout"
    );
    ensure!(
        (3001..=60000).contains(&timeout),
        "agent startup timeout must be 3001..60000ms"
    );
    let mut agents = json!({});
    for (name, spec) in request["config"]["agents"].as_object().unwrap() {
        crate::permission_policy::validate_terminal_binding(request, name, spec)?;
        ensure!(
            ["pi", "codex", "claude"].contains(&spec["provider"].as_str().unwrap_or("")),
            "unsupported Windows native Herdr provider"
        );
        ensure!(
            spec["dispatch_disabled"] != true,
            "disabled agent cannot be mounted"
        );
        let arguments = args(request, name)?;
        agents[name] = json!({"provider":spec["provider"],"cwd":namespace["slots"][name]["cwd"],
            "arguments_hash":hash(&json!(arguments))?,"argument_count":arguments.len()});
        if !spec["tui_host_root"].is_null() {
            crate::codex_tui_host::validate(spec)?;
            agents[name]["native_host"] =
                json!({"root":spec["tui_host_root"],"command_hash":hash(&spec["command"])?});
        }
    }
    Ok(
        json!({"namespace_plan_hash":hash(&namespace)?,"agents":agents,"startup_timeout_ms":timeout}),
    )
}
pub(crate) fn rpc(request: &Value, arguments: Vec<String>) -> Result<Value> {
    let response = herdr_transport::execute(
        &json!({"schema_version":1,"operation":"agent_mount","command":request["command"],
        "session":request["session"],"args":arguments,"expect_json":true,"timeout_ms":request["timeout_ms"].as_u64().unwrap_or(10000)}),
    )?;
    ensure!(
        response["ok"] == true,
        "Herdr agent command outcome is unobserved: {} (exit {}, {} ms)",
        response["failure"].as_str().unwrap_or("unknown"),
        response["exit_code"],
        response["elapsed_ms"]
    );
    let value: Value = serde_json::from_str(text(&response, "stdout")?)?;
    Ok(if value["result"].is_object() {
        value["result"].clone()
    } else {
        value
    })
}
struct Manager<'a> {
    store: Option<&'a mut Store>,
    request: &'a Value,
    namespace: &'a Value,
    plan: Value,
    record: Value,
}
impl Manager<'_> {
    fn save(&mut self) -> Result<()> {
        let store = self
            .store
            .as_mut()
            .context("read-only Herdr observer cannot save")?;
        store.save(RECORD, &self.record)?;
        store.commit()
    }
    fn snapshot(&self) -> Result<Value> {
        let snapshot =
            rpc(self.request, vec!["api".into(), "snapshot".into()])?["snapshot"].clone();
        ensure!(
            snapshot["protocol"].is_u64() && snapshot["version"].is_string(),
            "Herdr snapshot contract missing"
        );
        let workspace = snapshot["workspaces"]
            .as_array()
            .context("Herdr workspaces missing")?
            .iter()
            .filter(|w| w["workspace_id"] == self.namespace["workspace_id"])
            .collect::<Vec<_>>();
        ensure!(
            workspace.len() == 1 && self.owns(workspace[0], None),
            "Herdr workspace ownership changed"
        );
        for (name, binding) in self.namespace["bindings"].as_object().unwrap() {
            let panes = snapshot["panes"]
                .as_array()
                .context("Herdr panes missing")?
                .iter()
                .filter(|p| {
                    p["pane_id"] == binding["pane_id"]
                        && p["workspace_id"] == self.namespace["workspace_id"]
                })
                .collect::<Vec<_>>();
            ensure!(
                panes.len() == 1 && self.owns(panes[0], Some(name)),
                "Herdr pane ownership changed"
            );
        }
        ensure!(
            snapshot["agents"].is_array(),
            "Herdr agent inventory missing"
        );
        if let Some(protocol) = self.record.get("protocol") {
            ensure!(*protocol == snapshot["protocol"], "Herdr protocol changed");
        }
        Ok(snapshot)
    }
    fn owns(&self, row: &Value, slot: Option<&str>) -> bool {
        row["tokens"]["ccb_native_owner"] == self.namespace["owner_id"]
            && row["tokens"]["ccb_project_id"] == self.namespace["project_id"]
            && row["tokens"]["ccb_namespace_id"] == self.namespace["workspace_id"]
            && slot.is_none_or(|s| row["tokens"]["ccb_slot"] == s)
    }
    fn pane<'a>(&self, snapshot: &'a Value, name: &str) -> Result<&'a Value> {
        snapshot["panes"]
            .as_array()
            .unwrap()
            .iter()
            .find(|p| p["pane_id"] == self.namespace["bindings"][name]["pane_id"])
            .context("owned Herdr pane missing")
    }
    fn foreground(&self, pane: &Value) -> Result<(Value, u64, Value)> {
        let result = rpc(
            self.request,
            vec![
                "pane".into(),
                "process-info".into(),
                "--pane".into(),
                text(pane, "pane_id")?.into(),
            ],
        )?;
        let info = &result["process_info"];
        contract("PaneProcessInfo", info)?;
        ensure!(
            info["pane_id"] == pane["pane_id"],
            "Herdr process snapshot targets another pane"
        );
        let shell = info["shell_pid"]
            .as_u64()
            .filter(|p| *p > 0 && *p <= u32::MAX as u64)
            .context("Herdr shell identity unavailable")?;
        let processes = info["foreground_processes"]
            .as_array()
            .context("Herdr foreground identity unavailable")?;
        ensure!(
            processes.len() == 1,
            "Herdr foreground identity is ambiguous"
        );
        let pid = processes[0]["pid"]
            .as_u64()
            .filter(|p| *p > 0 && *p <= u32::MAX as u64)
            .context("invalid Herdr foreground PID")?;
        let observed = crate::owned_process::observe_external_process(pid as u32)?;
        Ok((observed, shell, processes[0]["cwd"].clone()))
    }
    fn shell(&self, snapshot: &Value, name: &str) -> Result<Value> {
        let pane = self.pane(snapshot, name)?;
        ensure!(
            pane["agent"].is_null()
                && !snapshot["agents"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|a| a["pane_id"] == pane["pane_id"]),
            "Herdr pane already contains an agent; no takeover is performed"
        );
        let (process, shell, cwd) = self.foreground(pane)?;
        ensure!(
            process["pid"] == shell,
            "Herdr pane is not at its shell process"
        );
        let actual =
            Path::new(cwd.as_str().context("foreground shell cwd unavailable")?).canonicalize()?;
        ensure!(
            actual == Path::new(text(&self.plan["agents"][name], "cwd")?).canonicalize()?,
            "Herdr shell cwd differs from the requested workspace"
        );
        Ok(
            json!({"pane_id":pane["pane_id"],"terminal_id":text(pane,"terminal_id")?,"shell":process}),
        )
    }
    fn observe(&self, snapshot: &Value, name: &str) -> Result<Value> {
        let row = &self.record["agents"][name];
        let pane = self.pane(snapshot, name)?;
        ensure!(
            pane["terminal_id"] == row["terminal_id"],
            "Herdr terminal lifetime changed"
        );
        let matches = snapshot["agents"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|a| a["name"] == row["herdr_name"])
            .collect::<Vec<_>>();
        ensure!(matches.len() <= 1, "Herdr agent name is ambiguous");
        let Some(agent) = matches.first() else {
            return Ok(
                json!({"status":if row["binding"].is_object(){"lost"}else if row["sent"]==true{"uncertain"}else{"prepared"}}),
            );
        };
        contract("AgentInfo", agent)?;
        ensure!(
            row["sent"] == true,
            "Herdr launch name was occupied before its request"
        );
        ensure!(
            agent["pane_id"] == row["pane_id"]
                && agent["terminal_id"] == row["terminal_id"]
                && agent["workspace_id"] == self.namespace["workspace_id"]
                && self.owns(agent, Some(name)),
            "Herdr agent binding moved or changed owner"
        );
        if agent["launch_pending"] == true || agent["agent"].is_null() {
            ensure!(
                row["binding"].is_null(),
                "bound Herdr agent lost its detected identity"
            );
            return Ok(json!({"status":"starting"}));
        }
        ensure!(
            agent["agent"] == self.plan["agents"][name]["provider"],
            "Herdr detected a different provider"
        );
        let status = agent["agent_status"]
            .as_str()
            .context("Herdr agent lifecycle missing")?;
        ensure!(
            ["idle", "done", "working", "blocked", "unknown"].contains(&status),
            "unknown Herdr agent lifecycle"
        );
        let hosted = row["host_launch"].is_object();
        // Hosted input belongs to the RPC driver. A busy display is still a
        // usable attachment and must not hold open until the model becomes idle.
        let ready = hosted
            || (agent["interactive_ready"] == true || pi_hook_ready(agent))
                && ["idle", "done"].contains(&status);
        // A tool can temporarily own the PTY foreground while the recognized
        // provider remains alive. Keep its previously bound process identity;
        // fresh readiness still requires the same provider back in foreground.
        let process = if row["binding"].is_object() && (!ready || hosted) {
            crate::owned_process::observe_external_process(
                row["binding"]["process"]["pid"].as_u64().unwrap() as u32,
            )?
        } else {
            let (process, shell, _) = self.foreground(pane)?;
            ensure!(
                process["pid"] != shell,
                "Herdr provider still resolves to the shell"
            );
            process
        };
        if row["binding"].is_object() {
            ensure!(
                row["binding"]["process"] == process,
                "Herdr provider process lifetime changed"
            );
        }
        let session = if agent["agent_session"].is_object() {
            json!({"source":agent["agent_session"]["source"],"agent":agent["agent_session"]["agent"],
                "kind":agent["agent_session"]["kind"],"value":agent["agent_session"]["value"]})
        } else {
            Value::Null
        };
        Ok(
            json!({"status":if ready {"ready"} else if status=="blocked" {"blocked"} else if status=="working" {"working"} else {"starting"},
            "binding":{"process":process},"agent_session":session,"observed_lifecycle":status,"interactive_ready":if hosted {agent["interactive_ready"]==true && ["idle","done"].contains(&status)} else {ready}}),
        )
    }
    fn pi_bridge(&self, name: &str) -> Result<Option<Value>> {
        if self.plan["agents"][name]["provider"] != "pi" {
            return Ok(None);
        }
        let Some(store) = self.store.as_ref() else {
            return Ok(None);
        };
        let Some(bridge) = store.json(&format!("providers/pi/{name}/terminal-bridge.json"))? else {
            return Ok(None);
        };
        ensure!(
            bridge["schema_version"] == 1 && bridge["agent_name"] == name,
            "invalid native Pi bridge manifest"
        );
        uuid::Uuid::parse_str(text(&bridge, "owner")?)?;
        let extension = Path::new(text(&bridge, "extension")?);
        let native = args(self.request, name)?;
        if !native.windows(2).any(|pair| {
            ["-e", "--extension"].contains(&pair[0].as_str()) && Path::new(&pair[1]) == extension
        }) {
            return Ok(None);
        }
        Ok(Some(bridge))
    }
    fn repair_initial_pi_name(&mut self, snapshot: &Value, name: &str) -> Result<bool> {
        let row = self.record["agents"][name].clone();
        if row["sent"] != true || !row["binding"].is_null() || !row["name_repair"].is_null() {
            return Ok(false);
        }
        let Some(bridge) = self.pi_bridge(name)? else {
            return Ok(false);
        };
        let candidates = snapshot["agents"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|a| a["pane_id"] == row["pane_id"])
            .collect::<Vec<_>>();
        if candidates.len() != 1 {
            return Ok(false);
        }
        let agent = candidates[0];
        if !agent["name"].is_null() || agent["launch_pending"] == true || !pi_hook_ready(agent) {
            return Ok(false);
        }
        ensure!(
            !snapshot["agents"]
                .as_array()
                .unwrap()
                .iter()
                .any(|a| a["name"] == row["herdr_name"]),
            "Pi repair name is already occupied"
        );
        contract("AgentInfo", agent)?;
        ensure!(
            agent["terminal_id"] == row["terminal_id"] && self.owns(agent, Some(name)),
            "Pi initial repair target changed"
        );
        let pane = self.pane(snapshot, name)?;
        let (process, shell, cwd) = self.foreground(pane)?;
        ensure!(
            process["pid"] != shell
                && crate::pi_terminal::same_session_path(&cwd, &self.plan["agents"][name]["cwd"]),
            "Pi initial repair foreground/cwd differs from launch"
        );
        let root = std::path::PathBuf::from(text(&bridge, "event_root")?);
        let current = root.join("current.json");
        herdr_namespace::unlinked(&current)?;
        let mut bytes = Vec::new();
        std::fs::File::open(&current)?
            .take(65537)
            .read_to_end(&mut bytes)?;
        ensure!(
            bytes.len() <= 65536,
            "Pi initial repair state exceeds limit"
        );
        let state: Value = serde_json::from_slice(&bytes)?;
        let binding = crate::terminal_history::Binding {
            root,
            path: current,
            session_id: text(&state, "session_id")?.into(),
            cwd: std::path::PathBuf::from(text(&self.plan["agents"][name], "cwd")?),
        };
        let (_, producer) = crate::pi_terminal::resolve(&binding, text(&bridge, "owner")?, name)?;
        ensure!(
            producer["process"] == process,
            "Pi initial repair belongs to another process lifetime"
        );
        ensure!(
            agent["agent_session"]["kind"] == "id"
                && agent["agent_session"]["value"] == binding.session_id
                || agent["agent_session"]["kind"] == "path"
                    && crate::pi_terminal::same_session_path(
                        &agent["agent_session"]["value"],
                        &producer["session_path"]
                    ),
            "Pi initial repair session changed"
        );
        self.record["agents"][name]["name_repair"] = json!({"permitted":true,"process":process,"bridge_owner":bridge["owner"],"pane_id":row["pane_id"],"terminal_id":row["terminal_id"]});
        self.save()?;
        let _ = rpc(
            self.request,
            vec![
                "agent".into(),
                "rename".into(),
                text(&row, "pane_id")?.into(),
                text(&row, "herdr_name")?.into(),
            ],
        );
        let after = self.snapshot()?;
        let (current, _, _) = self.foreground(self.pane(&after, name)?)?;
        ensure!(
            current == process,
            "Pi process changed during initial name repair"
        );
        Ok(true)
    }
    fn refresh(&mut self, name: &str, repair: bool) -> Result<()> {
        let mut snapshot = self.snapshot()?;
        if repair
            && (self.repair_initial_pi_name(&snapshot, name)?
                || self.repair_initial_host_name(&snapshot, name)?)
        {
            snapshot = self.snapshot()?;
        }
        let observed = self.observe(&snapshot, name)?;
        self.record["agents"][name]
            .as_object_mut()
            .unwrap()
            .extend(observed.as_object().unwrap().clone());
        self.save()
    }
    fn repair_initial_host_name(&mut self, snapshot: &Value, name: &str) -> Result<bool> {
        let row = self.record["agents"][name].clone();
        if row["host_launch"].is_null()
            || row["sent"] != true
            || !row["binding"].is_null()
            || !row["name_repair"].is_null()
        {
            return Ok(false);
        }
        let candidates = snapshot["agents"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|a| a["pane_id"] == row["pane_id"])
            .collect::<Vec<_>>();
        if candidates.len() != 1 {
            return Ok(false);
        }
        let agent = candidates[0];
        if !agent["name"].is_null() || agent["agent"] != "codex" || agent["launch_pending"] == true
        {
            return Ok(false);
        }
        ensure!(
            agent["terminal_id"] == row["terminal_id"] && self.owns(agent, Some(name)),
            "Codex attachment target changed"
        );
        ensure!(
            !snapshot["agents"]
                .as_array()
                .unwrap()
                .iter()
                .any(|a| a["name"] == row["herdr_name"]),
            "Codex attachment name is occupied"
        );
        let runtime =
            crate::config::state_root(Path::new(text(&self.request["config"], "project_root")?))?;
        let host = crate::codex_tui_host::current(
            &runtime,
            name,
            Path::new(text(&self.plan["agents"][name]["native_host"], "root")?),
        )?
        .context("Codex host not ready")?;
        ensure!(host == row["host_launch"], "Codex attachment host changed");
        let info = rpc(
            self.request,
            vec![
                "pane".into(),
                "process-info".into(),
                "--pane".into(),
                text(&row, "pane_id")?.into(),
            ],
        )?["process_info"]
            .clone();
        let argv = info["foreground_processes"][0]["argv"]
            .as_array()
            .context("Codex attachment argv unavailable")?;
        for pair in [
            [json!("--remote"), host["endpoint"].clone()],
            [json!("resume"), host["thread_id"].clone()],
        ] {
            ensure!(
                argv.windows(2).any(|args| args == pair),
                "foreground Codex is not connected to the recorded host/thread"
            );
        }
        let (process, shell, cwd) = self.foreground(self.pane(snapshot, name)?)?;
        ensure!(
            process["pid"] != shell
                && crate::pi_terminal::same_session_path(&cwd, &self.plan["agents"][name]["cwd"]),
            "Codex attachment process/cwd mismatch"
        );
        self.record["agents"][name]["name_repair"] = json!({"kind":"codex_host","permitted":true,"process":process,"host_process":host["host_process"],"pane_id":row["pane_id"],"terminal_id":row["terminal_id"]});
        self.save()?;
        let _ = rpc(
            self.request,
            vec![
                "agent".into(),
                "rename".into(),
                text(&row, "pane_id")?.into(),
                text(&row, "herdr_name")?.into(),
            ],
        );
        let after = self.snapshot()?;
        ensure!(
            self.foreground(self.pane(&after, name)?)?.0 == process,
            "Codex attachment changed during naming"
        );
        Ok(true)
    }
    fn run(&mut self, mount: bool) -> Result<Value> {
        let names = self.plan["agents"]
            .as_object()
            .unwrap()
            .keys()
            .cloned()
            .collect::<Vec<_>>();
        if self.record.is_null() {
            ensure!(mount, "no Herdr agent launch ledger exists");
            let snapshot = self.snapshot()?;
            let mut rows = json!({});
            for name in &names {
                let mut row = self.shell(&snapshot, name)?;
                let operation = uuid::Uuid::new_v4();
                row["operation_id"] = json!(operation.to_string());
                row["herdr_name"] = json!(format!("ccb-{}", &operation.simple().to_string()[..24]));
                row["sent"] = json!(false);
                row["status"] = json!("prepared");
                row["binding"] = Value::Null;
                rows[name] = row;
            }
            self.record = json!({"schema_version":1,"record_type":"native_herdr_agents","project_id":self.namespace["project_id"],
                "namespace_owner":self.namespace["owner_id"],"workspace_id":self.namespace["workspace_id"],"session":self.request["session"],
                "plan":self.plan,"plan_hash":hash(&self.plan)?,"protocol":snapshot["protocol"],"agents":rows});
            self.save()?;
        }
        for name in &names {
            if mount && self.sync_host_display(name)? {
                continue;
            }
            if mount {
                self.reset_dead_host_display(name)?;
            }
            self.refresh(name, mount)?;
            if mount && self.record["agents"][name]["sent"] == false {
                let host = if !self.plan["agents"][name]["native_host"].is_null() {
                    let runtime = crate::config::state_root(Path::new(text(
                        &self.request["config"],
                        "project_root",
                    )?))?;
                    let Some(host) = crate::codex_tui_host::current(
                        &runtime,
                        name,
                        Path::new(text(&self.plan["agents"][name]["native_host"], "root")?),
                    )?
                    else {
                        continue;
                    };
                    Some(host)
                } else {
                    None
                };
                let snapshot = self.snapshot()?;
                let shell = self.shell(&snapshot, name)?;
                for key in ["pane_id", "terminal_id", "shell"] {
                    ensure!(
                        shell[key] == self.record["agents"][name][key],
                        "Herdr shell changed before launch"
                    );
                }
                if let Some(host) = host {
                    let row = self.record["agents"][name].clone();
                    let info = rpc(
                        self.request,
                        vec![
                            "pane".into(),
                            "process-info".into(),
                            "--pane".into(),
                            text(&row, "pane_id")?.into(),
                        ],
                    )?["process_info"]
                        .clone();
                    let shell = info["foreground_processes"][0]["name"]
                        .as_str()
                        .unwrap_or("")
                        .to_ascii_lowercase();
                    ensure!(
                        ["pwsh", "pwsh.exe", "powershell", "powershell.exe"]
                            .contains(&shell.as_str()),
                        "Codex native attachment requires a PowerShell pane"
                    );
                    let native = std::env::current_exe()?.with_file_name("ccb-native.exe");
                    let quote = |s: &str| format!("'{}'", s.replace('\'', "''"));
                    let line = format!(
                        "& {} codex-tui-attach {} {}",
                        quote(&native.to_string_lossy()),
                        quote(text(&host, "path")?),
                        quote(text(&row, "operation_id")?)
                    );
                    self.record["agents"][name]["host_launch"] = host;
                    self.record["agents"][name]["sent"] = json!(true);
                    self.save()?;
                    let _ = rpc(
                        self.request,
                        vec![
                            "pane".into(),
                            "run".into(),
                            text(&row, "pane_id")?.into(),
                            line,
                        ],
                    );
                    self.refresh(name, true)?;
                    continue;
                }
                let native_pi = self.pi_bridge(name)?.is_some();
                let timeout = if native_pi {
                    self.plan["startup_timeout_ms"].as_u64().unwrap().min(4000)
                } else {
                    self.plan["startup_timeout_ms"].as_u64().unwrap()
                };
                self.record["agents"][name]["effective_startup_timeout_ms"] = json!(timeout);
                self.record["agents"][name]["sent"] = json!(true);
                self.save()?;
                let row = &self.record["agents"][name];
                let mut arguments = vec![
                    "agent".into(),
                    "start".into(),
                    text(row, "herdr_name")?.into(),
                    "--kind".into(),
                    text(&self.plan["agents"][name], "provider")?.into(),
                    "--pane".into(),
                    text(row, "pane_id")?.into(),
                    "--timeout".into(),
                    timeout.to_string(),
                ];
                let native = args(self.request, name)?;
                if !native.is_empty() {
                    arguments.push("--".into());
                    arguments.extend(native);
                }
                let mut request = self.request.clone();
                if native_pi {
                    request["timeout_ms"] = json!(timeout + 1000);
                }
                let acknowledgement = rpc(&request, arguments);
                let acknowledged = acknowledgement.as_ref().is_ok_and(|a| {
                    a["agent"]["name"] == row["herdr_name"]
                        && a["agent"]["pane_id"] == row["pane_id"]
                        && a["agent"]["terminal_id"] == row["terminal_id"]
                });
                self.record["agents"][name]["launch_acknowledged"] = json!(acknowledged);
                self.save()?;
                // A timeout/error can mean Herdr already started the provider.
                // Only a fresh unique-name and process-identity observation can
                // bind it. Absence never authorizes a second launch request.
                self.refresh(name, true)?;
            }
        }
        let rows = self.record["agents"].as_object().unwrap();
        let status = if rows.values().all(|r| r["status"] == "ready") {
            "ready"
        } else if rows.values().any(|r| r["status"] == "lost") {
            "lost"
        } else if rows.values().any(|r| r["status"] == "uncertain") {
            "uncertain"
        } else if rows.values().any(|r| r["status"] == "blocked") {
            "blocked"
        } else {
            "starting"
        };
        self.record["status"] = json!(status);
        self.save()?;
        Ok(json!({"mount":self.record,"daemon_connected":false,"task_completion_supported":false}))
    }
    fn reset_dead_host_display(&mut self, name: &str) -> Result<()> {
        let row = self.record["agents"][name].clone();
        if row["host_launch"].is_null() || row["binding"].is_null() {
            return Ok(());
        }
        let previous = &row["binding"]["process"];
        let pid = previous["pid"]
            .as_u64()
            .context("missing prior display PID")? as u32;
        let ended = match crate::owned_process::observe_external_process(pid) {
            Ok(current) => current != *previous,
            Err(_) => crate::daemon::owner_stopped(pid),
        };
        if !ended {
            return Ok(());
        }
        let snapshot = self.snapshot()?;
        let shell = self.shell(&snapshot, name)?;
        for key in ["pane_id", "terminal_id", "shell"] {
            ensure!(
                shell[key] == row[key],
                "closed Codex display's original shell changed"
            );
        }
        let runtime =
            crate::config::state_root(Path::new(text(&self.request["config"], "project_root")?))?;
        let Some(host) = crate::codex_tui_host::current(
            &runtime,
            name,
            Path::new(text(&self.plan["agents"][name]["native_host"], "root")?),
        )?
        else {
            return Ok(());
        };
        if !crate::codex_tui_host::same_owner(&host, &row["host_launch"]) {
            for key in ["host_process", "server_process"] {
                let old = &row["host_launch"][key];
                let pid = old["pid"]
                    .as_u64()
                    .filter(|p| *p > 0 && *p <= u32::MAX as u64)
                    .context("invalid previous Codex host identity")?
                    as u32;
                match crate::owned_process::observe_external_process(pid) {
                    Ok(current) => ensure!(
                        current != *old,
                        "previous Codex host still owns this thread"
                    ),
                    Err(_) => ensure!(
                        crate::daemon::owner_stopped(pid),
                        "previous Codex host exit is unconfirmed"
                    ),
                }
            }
            ensure!(
                host["thread_id"] == row["host_launch"]["thread_id"]
                    && host["executable"] == row["host_launch"]["executable"]
                    && crate::pi_terminal::same_session_path(
                        &host["cwd"],
                        &row["host_launch"]["cwd"]
                    ),
                "replacement host changed the native thread or workspace"
            );
        }
        let operation = uuid::Uuid::new_v4();
        let mut next = shell;
        next["operation_id"] = json!(operation.to_string());
        next["herdr_name"] = json!(format!("ccb-{}", &operation.simple().to_string()[..24]));
        next["sent"] = json!(false);
        next["status"] = json!("prepared");
        next["binding"] = Value::Null;
        self.store.as_mut().context("read-only observer cannot reattach")?.append("herdr-attachments.jsonl",
            &json!({"schema_version":1,"agent_name":name,"namespace_owner":self.namespace["owner_id"],"previous":row,"next_operation_id":next["operation_id"]}))?;
        self.record["agents"][name] = next;
        self.save()
    }
    fn sync_host_display(&mut self, name: &str) -> Result<bool> {
        let row = self.record["agents"][name].clone();
        if row["host_launch"].is_null() || row["binding"].is_null() {
            return Ok(false);
        }
        let runtime =
            crate::config::state_root(Path::new(text(&self.request["config"], "project_root")?))?;
        let Some(host) = crate::codex_tui_host::current(
            &runtime,
            name,
            Path::new(text(&self.plan["agents"][name]["native_host"], "root")?),
        )?
        else {
            return Ok(false);
        };
        if host["thread_id"] == row["host_launch"]["thread_id"]
            || !crate::codex_tui_host::same_owner(&host, &row["host_launch"])
        {
            return Ok(false);
        }
        let path = Path::new(text(&host, "path")?)
            .join(format!("display-{}.json", text(&row, "operation_id")?));
        herdr_namespace::unlinked(&path)?;
        if !path.exists() {
            return Ok(false);
        }
        let proof: Value = serde_json::from_slice(&std::fs::read(path)?)?;
        ensure!(
            proof["schema_version"] == 1
                && proof["attachment_id"] == row["operation_id"]
                && proof["host_process"] == host["host_process"]
                && proof["server_process"] == host["server_process"],
            "Codex display switch proof changed owner"
        );
        let supervisor = proof["supervisor"]["pid"]
            .as_u64()
            .filter(|p| *p > 0 && *p <= u32::MAX as u64)
            .context("missing display supervisor")? as u32;
        match crate::owned_process::observe_external_process(supervisor) {
            Ok(current) if current != proof["supervisor"] => return Ok(false),
            Ok(_) => {}
            Err(_) if crate::daemon::owner_stopped(supervisor) => return Ok(false),
            Err(error) => return Err(error),
        }
        let snapshot = self.snapshot()?;
        let agents = snapshot["agents"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|a| a["pane_id"] == row["pane_id"])
            .collect::<Vec<_>>();
        if proof["thread_id"] != host["thread_id"] || agents.is_empty() {
            self.record["agents"][name]["status"] = json!("starting");
            self.save()?;
            return Ok(true);
        }
        ensure!(
            proof["status"] == "running" && proof["epoch"].as_u64().is_some_and(|n| n > 0),
            "invalid Codex display switch epoch"
        );
        let old = &row["binding"]["process"];
        let old_pid = old["pid"].as_u64().unwrap() as u32;
        match crate::owned_process::observe_external_process(old_pid) {
            Ok(current) => ensure!(current != *old, "previous Codex display is still alive"),
            Err(_) => ensure!(
                crate::daemon::owner_stopped(old_pid),
                "previous display exit cannot be confirmed"
            ),
        }
        ensure!(
            agents.len() == 1
                && agents[0]["agent"] == "codex"
                && (agents[0]["name"].is_null() || agents[0]["name"] == row["herdr_name"])
                && agents[0]["terminal_id"] == row["terminal_id"]
                && self.owns(agents[0], Some(name)),
            "Codex display switch has a foreign occupant"
        );
        ensure!(
            self.foreground(self.pane(&snapshot, name)?)?.0 == proof["process"],
            "Codex switched foreground does not match supervisor proof"
        );
        ensure!(
            proof["process"] != row["binding"]["process"],
            "Codex thread switch retained its old display process"
        );
        self.store.as_mut().context("read-only observer cannot bind display switch")?.append("herdr-attachments.jsonl",
            &json!({"schema_version":1,"reason":"context_switch","agent_name":name,"previous":row,"thread_id":host["thread_id"]}))?;
        self.record["agents"][name]["host_launch"] = host;
        self.record["agents"][name]["binding"] = Value::Null;
        self.record["agents"][name]["name_repair"] = Value::Null;
        self.record["agents"][name]["status"] = json!("starting");
        self.save()?;
        Ok(false)
    }
}

fn validate(record: &Value, plan: &Value, namespace: &Value, request: &Value) -> Result<()> {
    ensure!(
        record["schema_version"] == 1
            && record["record_type"] == "native_herdr_agents"
            && record["project_id"] == namespace["project_id"]
            && record["namespace_owner"] == namespace["owner_id"]
            && record["workspace_id"] == namespace["workspace_id"]
            && record["session"] == request["session"]
            && record["plan"] == *plan
            && record["plan_hash"] == hash(plan)?,
        "Herdr launch plan/namespace changed; explicit migration required"
    );
    let rows = record["agents"]
        .as_object()
        .context("invalid Herdr agent ledger")?;
    ensure!(
        rows.len() == plan["agents"].as_object().unwrap().len(),
        "incomplete Herdr agent ledger"
    );
    for (name, row) in rows {
        let operation = uuid::Uuid::parse_str(text(row, "operation_id")?)?;
        if !row["name_repair"].is_null() {
            let repair = &row["name_repair"];
            ensure!(
                repair["permitted"] == true
                    && process_identity(&repair["process"])
                    && repair["pane_id"] == row["pane_id"]
                    && repair["terminal_id"] == row["terminal_id"]
                    && (if repair["kind"] == "codex_host" {
                        process_identity(&repair["host_process"])
                            && repair["host_process"] == row["host_launch"]["host_process"]
                    } else {
                        uuid::Uuid::parse_str(text(repair, "bridge_owner")?).is_ok()
                    }),
                "invalid Pi name repair proof"
            );
        }
        ensure!(
            plan["agents"].get(name).is_some()
                && row["pane_id"] == namespace["bindings"][name]["pane_id"]
                && row["herdr_name"] == format!("ccb-{}", &operation.simple().to_string()[..24])
                && row["sent"].is_boolean()
                && row["terminal_id"].is_string()
                && process_identity(&row["shell"])
                && (row["sent"] == true || row["status"] == "prepared" && row["binding"].is_null())
                && (row["binding"].is_null() || process_identity(&row["binding"]["process"]))
                && [
                    "prepared",
                    "uncertain",
                    "starting",
                    "blocked",
                    "working",
                    "ready",
                    "lost"
                ]
                .contains(&row["status"].as_str().unwrap_or("")),
            "invalid Herdr launch identity"
        );
    }
    Ok(())
}
fn process_identity(value: &Value) -> bool {
    value["pid"]
        .as_u64()
        .is_some_and(|p| p > 0 && p <= u32::MAX as u64)
        && value["created"]
            .as_str()
            .is_some_and(|s| s.parse::<u64>().is_ok())
        && value["windows_session"].as_u64().is_some()
}
pub fn execute(request: &Value) -> Result<Value> {
    let plan = plan(request)?;
    let action = text(request, "action")?;
    if action == "plan" {
        return Ok(json!({"plan":plan,"mutations":false,"daemon_connected":false}));
    }
    ensure!(
        ["mount", "observe", "inspect"].contains(&action),
        "unsupported Herdr agent action"
    );
    if action == "inspect" {
        let mut inspect = request.clone();
        inspect["action"] = json!("inspect");
        let namespace = herdr_namespace::execute(&inspect)?["namespace"].clone();
        let path = crate::config::state_root(Path::new(text(&request["config"], "project_root")?))?
            .join("herdr")
            .join(RECORD);
        herdr_namespace::unlinked(&path)?;
        let record = match std::fs::File::open(path) {
            Ok(file) => {
                let mut bytes = Vec::new();
                file.take(2 * 1024 * 1024 + 1).read_to_end(&mut bytes)?;
                ensure!(
                    bytes.len() <= 2 * 1024 * 1024,
                    "Herdr launch ledger too large"
                );
                serde_json::from_slice(&bytes)?
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Value::Null,
            Err(e) => return Err(e.into()),
        };
        if !record.is_null() {
            validate(&record, &plan, &namespace, request)?;
        }
        return Ok(json!({"mount":record,"live_checked":false,"daemon_connected":false}));
    }
    ensure!(
        cfg!(windows),
        "native Herdr agent identity binding currently requires Windows"
    );
    herdr_namespace::with_prepared(request, |store, _, namespace| {
        let record = store.json(RECORD)?.unwrap_or(Value::Null);
        if !record.is_null() {
            validate(&record, &plan, namespace, request)?;
        }
        Manager {
            store: Some(store),
            request,
            namespace,
            plan,
            record,
        }
        .run(action == "mount")
    })
}

/// Observe a previously mounted process without acquiring a second journal writer.
/// The process remains owned by Herdr; callers never gain termination rights.
pub(crate) fn recorded_identity(record: &Value, agent: &str) -> Value {
    let row = &record["agents"][agent];
    json!({"namespace_owner":record["namespace_owner"],"workspace_id":record["workspace_id"],
        "herdr_name":row["herdr_name"],"pane_id":row["pane_id"],"terminal_id":row["terminal_id"],
        "process":row["binding"]["process"],"plan_hash":record["plan_hash"]})
}
pub(crate) fn bound(request: &Value, agent: &str) -> Result<Value> {
    let namespace = herdr_namespace::inspect_prepared(request)?;
    let mut inspect = request.clone();
    inspect["action"] = json!("inspect");
    let record = execute(&inspect)?["mount"].clone();
    let plan = plan(request)?;
    validate(&record, &plan, &namespace, request)?;
    let row = &record["agents"][agent];
    ensure!(
        row["binding"].is_object(),
        "Herdr agent has no saved process binding"
    );
    let identity = recorded_identity(&record, agent);
    let manager = Manager {
        store: None,
        request,
        namespace: &namespace,
        plan,
        record,
    };
    let mut observed = manager.observe(&manager.snapshot()?, agent)?;
    ensure!(
        observed["binding"].is_object(),
        "mounted Herdr provider is no longer present"
    );
    observed["identity"] = identity;
    Ok(observed)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn installed_schema_accepts_required_identity_fields_and_rejects_missing_ones() {
        let mut agent = json!({"terminal_id":"term","pane_id":"w1:p1","workspace_id":"w1","tab_id":"w1:t1",
            "agent_status":"idle","focused":false,"revision":1,"name":"test","agent":"pi","interactive_ready":true});
        contract("AgentInfo", &agent).unwrap();
        agent.as_object_mut().unwrap().remove("terminal_id");
        assert!(contract("AgentInfo", &agent).is_err());
        contract("PaneProcessInfo",&json!({"pane_id":"w1:p1","shell_pid":123,"foreground_processes":[{"pid":123,"name":"pwsh"}]})).unwrap();
        assert!(contract(
            "PaneProcessInfo",
            &json!({"pane_id":"w1:p1","shell_pid":"123"})
        )
        .is_err());
    }
}
