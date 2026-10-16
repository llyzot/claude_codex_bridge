//! One resumable native entry point for Herdr topology, TUIs and the authority.
use anyhow::{ensure, Context, Result};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{
    fs::{self, OpenOptions},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::{Duration, Instant},
};
const RECORD: &str = "herdr-open.json";
const STARTUP_CONFIG: &str = "open-configuration.json";

fn cancelled(root: &Path, instance: &Value) -> Result<bool> {
    let intent = read(root, "open-cancel.json")?.unwrap_or(Value::Null);
    Ok(!instance.is_null()
        && intent["instance"] == *instance
        && intent["process"]["pid"] == std::process::id())
}

pub(crate) fn cancel_start(runtime: &Path, project: Option<&Path>) -> Result<Option<Value>> {
    let root = runtime.join("herdr");
    if !root.exists() {
        return Ok(None);
    }
    let path = root.join("native-open-launch.lock");
    crate::herdr_namespace::unlinked(&path)?;
    let lock = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(path)?;
    lock.try_lock()
        .context("another Herdr launch request is being recorded")?;
    let Some(intent) = read(&root, "herdr-open-start.json")? else {
        return Ok(None);
    };
    if intent["status"] != "opening" {
        return Ok(None);
    }
    if let Some(project) = project {
        ensure!(
            intent["project_id"] == crate::config::project_id(project)?,
            "Herdr opener belongs to another project or lacks startup ownership"
        );
    }
    if !crate::startup::owner_alive(
        &json!({"pid":intent["process"]["pid"],"process":intent["process"]}),
    )? {
        return Ok(None);
    }
    // The namespace controller holds its writer across bounded Herdr RPCs.
    // A cancellation request must reach it without acquiring that writer.
    let path = root.join("open-cancel.json");
    crate::herdr_namespace::unlinked(&path)?;
    let mut file = tempfile::NamedTempFile::new_in(&root)?;
    serde_json::to_writer(
        &mut file,
        &json!({"schema_version":1,"project_id":intent["project_id"],"instance":intent["instance"],"process":intent["process"]}),
    )?;
    file.as_file().sync_all()?;
    file.persist(path)?;
    Ok(Some(intent["process"].clone()))
}

pub(crate) fn prepared_configuration(runtime: &Path) -> Result<Option<Value>> {
    read(&runtime.join("herdr"), STARTUP_CONFIG)?
        .map(|record| crate::restart_config::restore(&record))
        .transpose()
}
pub(crate) fn record_preparation(store: &mut crate::store::Store, request: &Value) -> Result<()> {
    if !cfg!(windows) {
        return Ok(());
    }
    let mut config = request["config"].clone();
    let mut mounted = request.clone();
    mounted["action"] = json!("mount");
    config["herdr"] = mounted;
    let fingerprint = crate::config::fingerprint(&config)?;
    if let Some(prior) = store.json(STARTUP_CONFIG)? {
        if prior["config_fingerprint"] == fingerprint {
            return Ok(());
        }
        ensure!(
            store
                .json("namespace.json")?
                .is_none_or(|n| n["status"] == "retired"),
            "startup ownership configuration changed while its namespace exists"
        );
    }
    store.save(
        STARTUP_CONFIG,
        &json!({"schema_version":1,"record_type":"native_configuration",
        "project_id":config["project_id"],"config_fingerprint":fingerprint,
        "restart_checkpoint":crate::restart_config::seal(&config)?}),
    )?;
    store.commit()
}
fn hosted(spec: &Value) -> bool {
    spec["provider"] == "codex" && !spec["tui_host_root"].is_null()
}
fn readiness(config: &Value, record: &Value, live: &Value) -> Value {
    config["agents"].as_object().unwrap().iter().map(|(name, spec)| {
        let row = &record["agents"][name];
        let mounted = record["daemon_agents"].as_array().is_some_and(|agents| agents.contains(&json!(name)));
        // A lost display is never interactive, whatever its last observation said.
        let lost = row["status"] == "lost";
        let interface = if lost { "lost" } else if row["observed_lifecycle"] == "blocked" { "waiting_input" } else if row["interactive_ready"] == true { "ready" } else if row["sent"] != true { "not_started" } else { "starting" };
        let mut error = record["initialization_errors"].get(name).or_else(||record["binding_errors"].get(name)).cloned().unwrap_or(Value::Null);
        // A native host's session belongs to the authority, not to the Herdr record.
        let host = &live[name];
        let verified = error.is_null() && if hosted(spec) { row["host_launch"]["thread_id"].is_string() && (host.is_null() || host["session_verified"] == true) } else { record["bindings"][name].is_object() };
        if hosted(spec) && error.is_null() && host["session_verified"] == false {
            error = host["error"].clone();
        }
        (name.clone(),json!({"provider":spec["provider"],"startup_state":row["status"],"registered":mounted,"session_verified":verified,"initialization_error":error,"backend_mounted":mounted,"interface_state":interface,"interactive_ready":!lost && row["interactive_ready"]==true}))
    }).collect::<serde_json::Map<String,Value>>().into()
}
/// Best-effort activation of native hosts from a running authority.
fn host_activation(runtime: &Path, config: &Value, record: &Value) -> Value {
    if record["daemon"].is_null() || !config["agents"].as_object().unwrap().values().any(hosted) {
        return Value::Null;
    }
    crate::cli::request(runtime, "agents", json!({}))
        .ok()
        .and_then(|r| r["agents"].as_array().cloned())
        .map(|rows| {
            rows.iter()
                .filter_map(|a| {
                    Some((
                        a["agent_name"].as_str()?.to_owned(),
                        a["activation"].clone(),
                    ))
                })
                .collect::<serde_json::Map<String, Value>>()
                .into()
        })
        .unwrap_or(Value::Null)
}
#[path = "herdr_recovery.rs"]
mod recovery;
#[path = "herdr_repair.rs"]
mod repair;

pub(crate) fn repair_fresh_session(project: &Path, agent: &str, dry_run: bool) -> Result<Value> {
    ensure!(
        cfg!(windows),
        "native Herdr repair currently requires Windows"
    );
    repair::fresh_session(project, agent, dry_run)
}
pub(crate) fn recovery_handoff_allowed(kernel: &mut crate::Kernel, config: &Value) -> Result<bool> {
    recovery::handoff_allowed(kernel, config)
}
fn hash(v: &Value) -> Result<String> {
    Ok(format!("{:x}", Sha256::digest(serde_json::to_vec(v)?)))
}
fn save(root: &Path, record: &Value) -> Result<()> {
    let mut store = crate::store::Store::open(root)?;
    store.save(RECORD, record)?;
    store.commit()
}
fn read(root: &Path, name: &str) -> Result<Option<Value>> {
    let deadline = Instant::now() + Duration::from_millis(200);
    loop {
        let result = read_once(root, name);
        let retry = cfg!(windows)
            && result.as_ref().err().is_some_and(|error| {
                error
                    .downcast_ref::<std::io::Error>()
                    .is_some_and(|io| matches!(io.raw_os_error(), Some(5 | 32)))
            });
        if !retry || Instant::now() >= deadline {
            return result.with_context(|| format!("reading Herdr state {name}"));
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}
fn read_once(root: &Path, name: &str) -> Result<Option<Value>> {
    use std::io::Read;
    let path = root.join(name);
    crate::herdr_namespace::unlinked(&path)?;
    match fs::File::open(path) {
        Ok(file) => {
            let mut bytes = Vec::new();
            file.take(2 * 1024 * 1024 + 1).read_to_end(&mut bytes)?;
            ensure!(
                bytes.len() <= 2 * 1024 * 1024,
                "Herdr state exceeds read limit"
            );
            Ok(Some(serde_json::from_slice(&bytes)?))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.into()),
    }
}
pub(crate) fn command(options: &Value) -> Result<Vec<String>> {
    if let Some(command) = options.get("command") {
        let command: Vec<String> =
            serde_json::from_value(command.clone()).context("Herdr command must be argv")?;
        ensure!(
            !command.is_empty() && command.iter().all(|s| !s.is_empty()),
            "Herdr command must be nonempty argv"
        );
        return Ok(command);
    }
    let mut candidates = Vec::new();
    if let Some(exe) = std::env::var_os("CCB_HERDR_EXE") {
        let exe = PathBuf::from(exe);
        ensure!(
            exe.is_absolute() && exe.is_file(),
            "CCB_HERDR_EXE must name an existing absolute executable"
        );
        return Ok(vec![crate::workspaces::portable(&exe.canonicalize()?)?]);
    }
    // Herdr's activated standalone release is authoritative even when PATH
    // retains a previous version. Never pick a release by scanning versions.
    if let Some(home) = std::env::var_os("USERPROFILE") {
        candidates.push(PathBuf::from(home).join(".herdr/packages/standalone/current/herdr.exe"));
    }
    if let Some(path) = std::env::var_os("PATH") {
        candidates.extend(std::env::split_paths(&path).map(|p| p.join("herdr.exe")));
    }
    for (key, tail) in [
        ("LOCALAPPDATA", "Programs/Herdr/herdr.exe"),
        ("ProgramFiles", "Herdr/herdr.exe"),
    ] {
        if let Some(root) = std::env::var_os(key) {
            candidates.push(PathBuf::from(root).join(tail));
        }
    }
    let exe = candidates
        .into_iter()
        .find(|p| p.is_file())
        .context("Herdr executable not found; pass --herdr-exe or set CCB_HERDR_EXE")?;
    Ok(vec![crate::workspaces::portable(&exe.canonicalize()?)?])
}

pub(crate) fn default_config(project: &Path, source: Value) -> Result<Value> {
    let executables = crate::config::discover_executables(project, &source)?;
    let mut config = crate::config::compile(project, source, &executables)?;
    let hosts = crate::config::state_root(project)?.join("herdr/codex-hosts");
    for (name, spec) in config["agents"].as_object_mut().context("missing agents")? {
        if spec["provider"] == "codex" {
            ensure!(spec["startup_args"].is_null() || spec["startup_args"].as_array().is_some_and(|a| a.is_empty()),
                "default Codex host uses thread_options/turn_options; explicit terminal startup_args require --options configuration");
            if spec["tui_host_root"].is_null() {
                spec["tui_host_root"] = json!(hosts.join(name));
            }
        }
    }
    Ok(config)
}
fn launch_command(request: &Value) -> Result<Command> {
    let argv: Vec<String> = serde_json::from_value(request["command"].clone())?;
    let mut command = Command::new(&argv[0]);
    command.args(&argv[1..]);
    crate::herdr_transport::configure_env(&mut command);
    Ok(command)
}
fn server(request: &Value, root: &Path, record: &mut Value) -> Result<bool> {
    let response = crate::herdr_agents::rpc(
        request,
        vec!["status".into(), "server".into(), "--json".into()],
    )?;
    let state = response.get("server").unwrap_or(&response);
    ensure!(
        state["session"].is_null() || state["session"] == request["session"],
        "Herdr status belongs to another session"
    );
    if state["running"] == true || state["status"] == "running" {
        ensure!(
            state["compatible"] != false,
            "Herdr server protocol is incompatible; no namespace changes were made"
        );
        return Ok(true);
    }
    ensure!(
        state["running"] == false,
        "Herdr server absence is unconfirmed; no automatic start"
    );
    if record["server_start_permitted"] != true {
        record["server_start_permitted"] = json!(true);
        save(root, record)?;
        let mut command = launch_command(request)?;
        command
            .args([
                "--session",
                crate::records::string(request, "session")?,
                "server",
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            command.creation_flags(0x08000000 | 0x00000200);
        }
        let child = command
            .spawn()
            .context("Herdr server launch failed after recorded intent; inspect before retrying")?;
        record["server_process"] = crate::owned_process::observe_external_process(child.id())?;
        save(root, record)?;
    }
    // A failed/expired observation never clears the launch permission.
    if let Some(pid) = record["server_process"]["pid"].as_u64() {
        ensure!(
            crate::owned_process::observe_external_process(pid as u32)? == record["server_process"],
            "Herdr startup process lifetime changed"
        );
    }
    Ok(false)
}
fn path_env(spec: &mut Value, native: &Path) -> Result<()> {
    let parent = native
        .parent()
        .context("native executable has no directory")?;
    let path_keys = spec["env"]
        .as_object()
        .map(|env| {
            env.keys()
                .filter(|key| key.eq_ignore_ascii_case("PATH"))
                .cloned()
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    ensure!(path_keys.len() <= 1, "ambiguous PATH environment override");
    let current = path_keys
        .first()
        .map(|key| &spec["env"][key])
        .unwrap_or(&Value::Null)
        .as_str()
        .map(std::ffi::OsString::from)
        .unwrap_or_else(|| std::env::var_os("PATH").unwrap_or_default());
    let mut paths = vec![parent.to_path_buf()];
    paths.extend(std::env::split_paths(&current).filter(|p| p != parent));
    if spec["env"].is_null() {
        spec["env"] = json!({});
    }
    for key in path_keys {
        spec["env"].as_object_mut().unwrap().remove(&key);
    }
    spec["env"]["PATH"] = json!(std::env::join_paths(paths)?.to_string_lossy());
    Ok(())
}
fn prepare_request(
    project: &Path,
    config: &Value,
    options: &Value,
    resume: &Value,
) -> Result<(Value, Value)> {
    let mut base = config.clone();
    let mut arguments = json!({});
    let mut bridges = json!({});
    let native = std::env::current_exe()?;
    for (name, spec) in base["agents"].as_object_mut().context("missing agents")? {
        let provider = crate::records::string(spec, "provider")?.to_owned();
        ensure!(
            ["pi", "codex", "claude"].contains(&provider.as_str()),
            "native herdr open supports Pi/Codex/Claude terminal adapters"
        );
        ensure!(
            hosted(spec) || spec["thread_options"].is_null() && spec["turn_options"].is_null(),
            "headless protocol options require explicit terminal startup_args"
        );
        let mut args: Vec<String> = if hosted(spec) {
            spec["startup_args"]
                .as_array()
                .map(|v| v.iter().map(|s| s.as_str().unwrap().to_owned()).collect())
                .unwrap_or_default()
        } else {
            crate::permission_policy::terminal(spec, crate::launch_options::terminal_args(spec)?)?
        };
        if provider == "pi" {
            let bridge = crate::pi_terminal::execute(
                &json!({"schema_version":1,"project_root":project,"agent_name":name,
                    "coordination":crate::coordination::interactive(name,config,&crate::config::state_root(project)?,&native)?}),
            )?;
            args.extend(serde_json::from_value::<Vec<String>>(
                bridge["interactive_args"].clone(),
            )?);
            bridges[name] = bridge["bridge"].clone();
        }
        if let Some(extra) = resume.get(name) {
            args.extend(serde_json::from_value::<Vec<String>>(extra.clone())?);
        }
        if !hosted(spec) {
            spec["command"] = Value::Null;
            spec["driver_kind"] = Value::Null;
        } else {
            crate::codex_tui_host::validate(spec)?;
            ensure!(
                spec["thread_options"]["ephemeral"] != true,
                "Herdr Codex attachment requires a persistent native thread"
            );
            ensure!(
                args.is_empty(),
                "native Codex host uses thread_options rather than terminal startup_args"
            );
        }
        path_env(spec, &native)?;
        let runtime = crate::config::state_root(project)?;
        for (key, value) in [
            ("CCB_AGENT_NAME", json!(name)),
            ("CCB_NATIVE_STATE_ROOT", json!(runtime)),
            ("CCB_NATIVE_TOKEN_FILE", json!("")),
            ("CCB_NATIVE_ENDPOINT", json!("")),
        ] {
            if let Some(existing) = spec["env"]
                .as_object()
                .unwrap()
                .keys()
                .find(|k| k.eq_ignore_ascii_case(key))
                .cloned()
            {
                ensure!(
                    spec["env"][&existing] == value,
                    "terminal environment overrides controller routing"
                );
                spec["env"].as_object_mut().unwrap().remove(&existing);
            }
            spec["env"][key] = value;
        }
        arguments[name] = json!(args);
    }
    let session = options["session"]
        .as_str()
        .map(str::to_owned)
        .unwrap_or_else(|| {
            format!(
                "ccb-native-{}",
                &config["project_id"].as_str().unwrap()[..12]
            )
        });
    let request = json!({"schema_version":1,"action":"mount","session":session,"command":command(options)?,"config":base,
        "interactive_args":arguments,"timeout_ms":options["transport_timeout_ms"].as_u64().unwrap_or(3000)});
    crate::herdr_namespace::plan(&request)?;
    Ok((request, bridges))
}
fn codex_root(spec: &Value, options: &Value, agent: &str) -> Result<PathBuf> {
    if let Some(root) = options["history_roots"][agent].as_str() {
        return Ok(root.into());
    }
    if spec["provider"] == "claude" {
        let home = spec["env"]["CLAUDE_CONFIG_DIR"]
            .as_str()
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("CLAUDE_CONFIG_DIR").map(PathBuf::from))
            .or_else(|| std::env::var_os("USERPROFILE").map(|p| PathBuf::from(p).join(".claude")))
            .context("Claude home is unknown; configure history_roots")?;
        return Ok(home.join("projects"));
    }
    let home = spec["env"]["CODEX_HOME"]
        .as_str()
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("CODEX_HOME").map(PathBuf::from))
        .or_else(|| std::env::var_os("USERPROFILE").map(|p| PathBuf::from(p).join(".codex")))
        .context("Codex home is unknown; configure history_roots")?;
    Ok(home.join("sessions"))
}
fn find_history(provider: &str, root: &Path, id: &str, cwd: &Path) -> Result<Option<Value>> {
    ensure!(root.is_absolute(), "Codex history root must be absolute");
    if !root.exists() {
        return Ok(None);
    }
    let mut pending = vec![(root.to_path_buf(), 0)];
    let mut count = 0;
    let mut found = None;
    while let Some((directory, depth)) = pending.pop() {
        crate::herdr_namespace::unlinked(&directory)?;
        for item in fs::read_dir(directory)? {
            count += 1;
            ensure!(
                count <= 20000,
                "Codex history discovery limit exceeded; provide an explicit binding"
            );
            let item = item?;
            let kind = item.file_type()?;
            if kind.is_symlink() {
                continue;
            }
            if kind.is_dir() && depth < 4 {
                pending.push((item.path(), depth + 1));
            }
            if kind.is_file()
                && item
                    .file_name()
                    .to_string_lossy()
                    .ends_with(&format!("{id}.jsonl"))
            {
                let binding = crate::terminal_history::Binding {
                    root: root.into(),
                    path: item.path(),
                    session_id: id.into(),
                    cwd: cwd.into(),
                };
                if provider == "claude" && crate::claude_history::baseline(&binding, false).is_err()
                {
                    continue;
                }
                let baseline =
                    crate::terminal_history::provider_baseline(provider, &binding, false)?;
                ensure!(
                    baseline.producer.is_none(),
                    "Codex discovery found another provider's history"
                );
                ensure!(
                    found.is_none(),
                    "multiple Codex histories match the exact native session"
                );
                found = Some(serde_json::to_value(binding)?);
            }
        }
    }
    Ok(found)
}
// Only consume the live mount result from the current open iteration here.
// Disk inspection is not a substitute for this observation. A mutation such
// as reporting a Pi session still performs its own fresh bound() observation.
fn mounted_binding(mounted: &Value, agent: &str) -> Result<Value> {
    let mut live = mounted["agents"][agent].clone();
    ensure!(
        live["binding"].is_object()
            && ["ready", "working", "blocked", "starting"]
                .contains(&live["status"].as_str().unwrap_or("")),
        "mounted Herdr provider is no longer present"
    );
    live["identity"] = crate::herdr_agents::recorded_identity(mounted, agent);
    Ok(live)
}
fn binding(
    request: &Value,
    bridges: &Value,
    options: &Value,
    agent: &str,
    journal: &Path,
    record: &mut Value,
    mounted: Option<&Value>,
) -> Result<Option<Value>> {
    let mut live = match mounted {
        Some(mounted) => mounted_binding(mounted, agent)?,
        None => crate::herdr_agents::bound(request, agent)?,
    };
    let spec = &request["config"]["agents"][agent];
    if spec["provider"] == "pi" {
        use std::io::Read;
        let bridge = &bridges[agent];
        let root = PathBuf::from(crate::records::string(bridge, "event_root")?);
        let current = root.join("current.json");
        crate::herdr_namespace::unlinked(&current)?;
        let mut bytes = Vec::new();
        match fs::File::open(&current) {
            Ok(file) => {
                file.take(65537).read_to_end(&mut bytes)?;
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e.into()),
        }
        ensure!(bytes.len() <= 65536, "Pi binding state exceeds limit");
        let state: Value = serde_json::from_slice(&bytes)?;
        let configured = crate::terminal_history::Binding {
            root,
            path: current,
            session_id: crate::records::string(&state, "session_id")?.into(),
            cwd: PathBuf::from(crate::records::string(spec, "workspace_path")?),
        };
        let (resolved, producer) = crate::pi_terminal::resolve(
            &configured,
            crate::records::string(bridge, "owner")?,
            agent,
        )?;
        ensure!(
            producer["process"] == live["identity"]["process"],
            "Pi session belongs to another process lifetime"
        );
        if live["agent_session"].is_null() {
            let proof = json!({"identity":live["identity"],"owner":bridge["owner"],"path":producer["session_path"],"sent":true});
            if record["session_reports"].is_null() {
                record["session_reports"] = json!({});
            }
            if record["session_reports"][agent].is_null() {
                record["session_reports"][agent] = proof.clone();
                save(journal, record)?;
                let _ = crate::herdr_transport::execute(
                    &json!({"schema_version":1,"operation":"pi_session_report","command":request["command"],"session":request["session"],
                    "args":["pane","report-agent-session",live["identity"]["pane_id"],"--source","ccb-native","--agent","pi","--agent-session-path",producer["session_path"]],
                    "expect_json":false,"timeout_ms":request["timeout_ms"]}),
                );
            } else {
                ensure!(
                    record["session_reports"][agent] == proof,
                    "Pi session reporting identity changed"
                );
            }
            let refreshed = crate::herdr_agents::bound(request, agent)?;
            ensure!(
                refreshed["identity"] == live["identity"],
                "Pi process changed during session reporting"
            );
            live = refreshed;
            if live["agent_session"].is_null() {
                return Ok(None);
            }
        }
        ensure!(
            live["agent_session"]["agent"] == "pi"
                && (live["agent_session"]["kind"] == "id"
                    && live["agent_session"]["value"] == resolved.session_id
                    || live["agent_session"]["kind"] == "path"
                        && crate::pi_terminal::same_session_path(
                            &live["agent_session"]["value"],
                            &producer["session_path"]
                        )),
            "Pi native session is not independently reported by Herdr"
        );
        return Ok(Some(
            json!({"history":configured,"pi_bridge_owner":bridge["owner"]}),
        ));
    }
    let native = &live["agent_session"];
    let provider = crate::records::string(spec, "provider")?;
    let cwd = Path::new(crate::records::string(spec, "workspace_path")?);
    if native["agent"] != provider || native["kind"] != "id" {
        ensure!(
            native.is_null(),
            "unexpected Codex native session reference"
        );
        let discovery = if let Some(explicit) = options["bindings"].get(agent) {
            let binding: crate::terminal_history::Binding =
                serde_json::from_value(explicit.clone())?;
            ensure!(
                binding.cwd == cwd,
                "explicit deferred history workspace mismatch"
            );
            json!({"root":binding.root,"cwd":cwd,"session_id":binding.session_id})
        } else {
            json!({"root":codex_root(spec,options,agent)?,"cwd":cwd})
        };
        return Ok(Some(json!({"discovery":discovery})));
    }
    let id = crate::records::string(native, "value")?;
    uuid::Uuid::parse_str(id).context("Herdr did not report a native Codex session UUID")?;
    if let Some(explicit) = options["bindings"].get(agent) {
        let binding: crate::terminal_history::Binding = serde_json::from_value(explicit.clone())?;
        ensure!(
            binding.session_id == id && binding.cwd == cwd,
            "explicit Codex history does not match live session/workspace"
        );
        crate::terminal_history::provider_baseline(provider, &binding, false)?;
        return Ok(Some(json!({"history":binding})));
    }
    let root = codex_root(spec, options, agent)?;
    Ok(Some(
        if let Some(history) = find_history(provider, &root, id, cwd)? {
            json!({"history":history})
        } else {
            json!({"discovery":{"root":root,"cwd":cwd,"session_id":id}})
        },
    ))
}
fn daemon_configuration(request: &Value, record: &Value) -> Value {
    let mut daemon = request["config"].clone();
    daemon["herdr"] = request.clone();
    daemon["herdr_activation"] = json!({"namespace_owner":record["namespace_owner"],"workspace_id":record["workspace_id"],"blocked_agents":record["initialization_errors"]});
    for spec in daemon["agents"].as_object_mut().unwrap().values_mut() {
        if hosted(spec) {
            continue;
        }
        spec["driver_kind"] = json!("herdr_tui");
        spec["profile_family"] = json!("protocol_turn");
        spec["herdr_binding_mode"] = json!("managed");
        for key in ["herdr_history", "herdr_discovery", "pi_bridge_owner"] {
            spec.as_object_mut().unwrap().remove(key);
        }
    }
    daemon
}

fn sync_daemon(request: &Value, root: &Path, record: &mut Value, wait: Duration) -> Result<bool> {
    let daemon = daemon_configuration(request, record);
    let runtime =
        crate::config::state_root(Path::new(crate::records::string(&daemon, "project_root")?))?;
    let desired = json!(crate::config::fingerprint(&daemon)?);
    if record["daemon"].is_null() {
        // Configuration contains the entire roster. Session activation is a
        // separate durable observation and never changes this configuration.
        record["daemon_agents"] = json!(daemon["agents"]
            .as_object()
            .unwrap()
            .keys()
            .collect::<Vec<_>>());
        record["daemon_fingerprint"] = desired.clone();
        save(root, record)?;
        recovery::authorize_start(&runtime, root, record, &daemon)?;
    } else {
        ensure!(
            record["daemon_fingerprint"] == desired,
            "running startup configuration changed"
        );
    }
    record["daemon"] =
        crate::startup::start(&runtime, daemon, wait.min(Duration::from_millis(250)))?;
    save(root, record)?;
    Ok(record["daemon"]["status"] == "running")
}

pub(crate) fn verify_activation(
    root: &Path,
    open: &Value,
    spec: &Value,
    binding: &Value,
    agent: &str,
) -> Result<()> {
    let expected = recovery::expected_session(root, open, agent)?;
    ensure!(
        recovery::verify_session(expected.as_ref(), spec, binding, agent)?,
        "recovered native session is not confirmed"
    );
    Ok(())
}

fn retain_inherited_paths(runtime: &Path, source: &Value, request: &mut Value) -> Result<()> {
    let prior = if let Some(committed) = read(runtime, "configuration.json")? {
        crate::restart_config::restore(&committed)?
    } else if let Some(prepared) = prepared_configuration(runtime)? {
        prepared
    } else {
        return Ok(());
    };
    if prior["project_id"] != source["project_id"] || !prior["herdr"].is_object() {
        return Ok(());
    }
    let mut adjusted = request.clone();
    for (name, spec) in source["agents"].as_object().context("missing agents")? {
        let explicit = spec["env"]
            .as_object()
            .is_some_and(|env| env.keys().any(|key| key.eq_ignore_ascii_case("PATH")));
        if !explicit && prior["herdr"]["config"]["agents"][name]["env"]["PATH"].is_string() {
            adjusted["config"]["agents"][name]["env"]["PATH"] =
                prior["herdr"]["config"]["agents"][name]["env"]["PATH"].clone();
        }
    }
    // Retain an inherited launch environment only when every other request
    // field matches. Explicit PATH/model/home/policy changes remain fenced.
    if adjusted == prior["herdr"] {
        *request = adjusted;
    }
    Ok(())
}

pub(crate) fn open(
    project: &Path,
    mut config: Value,
    options: &Value,
    wait: Duration,
) -> Result<Value> {
    ensure!(
        cfg!(windows),
        "native Herdr open currently requires Windows"
    );
    ensure!(
        config["project_id"] == crate::config::project_id(project)?
            && Path::new(crate::records::string(&config, "project_root")?).canonicalize()?
                == project.canonicalize()?,
        "Herdr open project/config mismatch"
    );
    // Headless argv is deliberately not reused for terminal launch.
    for spec in config["agents"]
        .as_object_mut()
        .context("missing agents")?
        .values_mut()
    {
        *spec = crate::launch_options::normalize(spec)?;
        crate::permission_policy::validate(spec, Some(project))?;
        if spec["driver_kind"] == "codex_app_server" && !hosted(spec) {
            if spec["thread_options"]
                .as_object()
                .is_some_and(|v| v.keys().all(|k| k == "model"))
            {
                spec.as_object_mut().unwrap().remove("thread_options");
            }
            if spec["turn_options"]
                .as_object()
                .is_some_and(|v| v.keys().all(|k| k == "effort"))
            {
                spec.as_object_mut().unwrap().remove("turn_options");
            }
        }
    }
    config["project_root"] = json!(project);
    crate::workspaces::validate(&config)?;
    for spec in config["agents"].as_object().unwrap().values() {
        crate::codex_tui_host::validate(spec)?;
        ensure!(
            !hosted(spec) || spec["thread_options"]["ephemeral"] != true,
            "Herdr Codex attachment requires a persistent native thread"
        );
        ensure!(
            ["pi", "codex", "claude"].contains(&spec["provider"].as_str().unwrap_or("")),
            "native herdr open supports Pi/Codex/Claude terminal adapters"
        );
        ensure!(
            hosted(spec) || spec["thread_options"].is_null() && spec["turn_options"].is_null(),
            "headless protocol options require explicit terminal startup_args"
        );
    }
    let runtime = crate::config::state_root(project)?;
    let has_host = config["agents"].as_object().unwrap().values().any(hosted);
    let root = runtime.join("herdr");
    if options["dry_run"] == true {
        let agents=config["agents"].as_object().unwrap().iter().map(|(name,spec)|json!({"name":name,"provider":spec["provider"],"workspace_path":spec["workspace_path"],
            "program":spec["command"][0],"input_transport":if hosted(spec) {"authenticated_websocket"} else {"herdr_agent_prompt"},"tui_host_root":spec["tui_host_root"]})).collect::<Vec<_>>();
        return Ok(
            json!({"status":"planned","project_id":config["project_id"],"agents":agents,"herdr_executable":command(options)?[0],"mutations":false}),
        );
    }
    crate::herdr_namespace::unlinked(&runtime)?;
    crate::herdr_namespace::unlinked(&root)?;
    fs::create_dir_all(&root)?;
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
        "native Herdr open is already in progress"
    );
    recovery::restore_retired_checkpoint(&runtime, &root)?;
    let mut record = crate::store::Store::open(&root)?
        .json(RECORD)?
        .unwrap_or(Value::Null);
    if record.is_null() {
        crate::daemon::verify_owner(&runtime)?;
    } else if let Some(lease) = read(&runtime, "lease.json")? {
        if lease["mount_state"] != "unmounted" {
            let pid = lease["ccbd_pid"]
                .as_u64()
                .filter(|p| *p > 0 && *p <= u32::MAX as u64)
                .context("invalid existing daemon ownership")? as u32;
            if !crate::daemon::owner_stopped(pid) {
                let ping = crate::cli::request(&runtime, "ping", json!({}))?;
                ensure!(record["daemon_fingerprint"].is_string() && ping["runtime"]=="rust" && ping["project_id"]==config["project_id"]
                    && ping["config_fingerprint"]==record["daemon_fingerprint"],"existing daemon does not match this Herdr workflow; explicit migration is required");
            }
        }
    }
    let (mut request, mut bridges) =
        prepare_request(project, &config, options, &record["resume_args"])?;
    if !record.is_null() {
        retain_inherited_paths(&runtime, &config, &mut request)?;
    }
    let signature = hash(&json!([
        request,
        options["history_roots"],
        options["bindings"]
    ]))?;
    if record.is_null() {
        record = json!({"schema_version":1,"project_id":config["project_id"],"request_hash":signature,"session":request["session"],"phase":"opening","bindings":{}});
        save(&root, &record)?;
    }
    ensure!(
        record["schema_version"] == 1
            && record["project_id"] == config["project_id"]
            && record["request_hash"] == signature,
        "Herdr open configuration changed; explicit migration is required"
    );
    if crate::herdr_namespace::reopen_pre_backend(&request)? {
        record["bindings"] = json!({});
        record["session_reports"] = Value::Null;
        record["daemon"] = Value::Null;
        record["daemon_fingerprint"] = Value::Null;
        record["daemon_agents"] = Value::Null;
        record["binding_reload"] = Value::Null;
        record["phase"] = json!("opening");
        save(&root, &record)?;
    }
    {
        let mut store = crate::store::Store::open(&root)?;
        record_preparation(&mut store, &request)?;
    }
    if record["observer_instance"] != options["opener_instance"] {
        record["observer_instance"] = options["opener_instance"].clone();
        record["phase"] = json!("opening");
        save(&root, &record)?;
    }
    let deadline = Instant::now() + wait;
    loop {
        if cancelled(&root, &options["opener_instance"])? {
            record["phase"] = json!("cancelled");
            save(&root, &record)?;
            break;
        }
        if !recovery::step(
            project,
            &config,
            options,
            &root,
            &mut record,
            &mut request,
            &mut bridges,
        )? {
            save(&root, &record)?;
            if Instant::now() >= deadline {
                break;
            }
            std::thread::sleep(Duration::from_millis(250));
            continue;
        }
        if !server(&request, &root, &mut record)? {
            record["phase"] = json!("server_starting");
        } else {
            {
                let mut store = crate::store::Store::open(&root)?;
                record_preparation(&mut store, &request)?;
            }
            crate::workspaces::materialize(&request["config"])?;
            let mut prepare = request.clone();
            prepare["action"] = json!("prepare");
            let namespace = crate::herdr_namespace::execute(&prepare)?["namespace"].clone();
            record["workspace_id"] = namespace["workspace_id"].clone();
            record["namespace_owner"] = namespace["owner_id"].clone();
            record["phase"] = json!("agents_starting");
            save(&root, &record)?;
            if namespace["status"] != "prepared" {
                record["phase"] = json!("namespace_uncertain");
            } else {
                if cancelled(&root, &options["opener_instance"])? {
                    continue;
                }
                // Hosted providers can initialize before any terminal needs a
                // trust decision or publishes its session identity.
                for (name, spec) in request["config"]["agents"].as_object().unwrap() {
                    if hosted(spec) {
                        record["bindings"][name] = json!({"native_host":true});
                    }
                }
                sync_daemon(
                    &request,
                    &root,
                    &mut record,
                    deadline.saturating_duration_since(Instant::now()),
                )?;
                if cancelled(&root, &options["opener_instance"])? {
                    continue;
                }
                let mut mount = request.clone();
                mount["startup_holds"] = record["initialization_errors"].clone();
                mount["cancel_control"] = json!({"path":root.join("open-cancel.json"),"instance":options["opener_instance"]});
                let mounted = crate::herdr_agents::execute(&mount)?["mount"].clone();
                record["agents"] = mounted["agents"].clone();
                record["phase"] = json!(if mounted["status"] == "ready" {
                    "binding_sessions"
                } else {
                    "agents_starting"
                });
                {
                    for name in request["config"]["agents"].as_object().unwrap().keys() {
                        if hosted(&request["config"]["agents"][name]) {
                            record["bindings"][name] = json!({"native_host":true});
                            continue;
                        }
                        if mounted["agents"][name]["status"] != "ready"
                            && record["bindings"][name].is_null()
                        {
                            continue;
                        }
                        let activation = (|| -> Result<()> {
                            let expected_session =
                                recovery::expected_session(&root, &record, name)?;
                            if record["bindings"][name].is_null()
                                || expected_session.is_some()
                                    && record["bindings"][name]["discovery"].is_object()
                            {
                                record["bindings"].as_object_mut().unwrap().remove(name);
                                if let Some(bound) = binding(
                                    &request,
                                    &bridges,
                                    options,
                                    name,
                                    &root,
                                    &mut record,
                                    Some(&mounted),
                                )? {
                                    if recovery::verify_session(
                                        expected_session.as_ref(),
                                        &request["config"]["agents"][name],
                                        &bound,
                                        name,
                                    )? {
                                        record["bindings"][name] = bound;
                                    }
                                    save(&root, &record)?;
                                }
                            } else {
                                // A context clear may have rotated this session.
                                let saved = &crate::herdr_activation::rotated(
                                    &runtime,
                                    request["config"]["agents"][name]["provider"]
                                        .as_str()
                                        .unwrap_or(""),
                                    name,
                                    &record["bindings"][name],
                                )?;
                                ensure!(
                                    recovery::verify_session(
                                        expected_session.as_ref(),
                                        &request["config"]["agents"][name],
                                        saved,
                                        name
                                    )?,
                                    "recovered native session has not been confirmed"
                                );
                                let live = mounted_binding(&mounted, name)?;
                                if request["config"]["agents"][name]["provider"] == "pi" {
                                    let configured: crate::terminal_history::Binding =
                                        serde_json::from_value(saved["history"].clone())?;
                                    let (_, producer) = crate::pi_terminal::resolve(
                                        &configured,
                                        crate::records::string(saved, "pi_bridge_owner")?,
                                        name,
                                    )?;
                                    ensure!(
                                        producer["process"] == live["identity"]["process"],
                                        "cached Pi binding process changed"
                                    );
                                    ensure!(
                                        live["agent_session"]["agent"] == "pi"
                                            && (live["agent_session"]["kind"] == "id"
                                                && live["agent_session"]["value"]
                                                    == configured.session_id
                                                || live["agent_session"]["kind"] == "path"
                                                    && crate::pi_terminal::same_session_path(
                                                        &live["agent_session"]["value"],
                                                        &producer["session_path"]
                                                    )),
                                        "cached Pi session changed"
                                    );
                                } else if saved["discovery"].is_object() {
                                    ensure!(
                                        live["agent_session"].is_null()
                                            || live["agent_session"]["agent"]
                                                == request["config"]["agents"][name]["provider"]
                                                && live["agent_session"]["kind"] == "id"
                                                && (saved["discovery"]["session_id"].is_null()
                                                    || live["agent_session"]["value"]
                                                        == saved["discovery"]["session_id"]),
                                        "deferred Codex session scope changed"
                                    );
                                } else {
                                    ensure!(
                                        live["agent_session"]["agent"]
                                            == request["config"]["agents"][name]["provider"]
                                            && live["agent_session"]["kind"] == "id"
                                            && live["agent_session"]["value"]
                                                == saved["history"]["session_id"],
                                        "cached Codex session changed"
                                    );
                                }
                            }
                            Ok(())
                        })();
                        if let Err(error) = activation {
                            // Stop activation, but keep the verified session as
                            // evidence for a later stopped recovery.
                            if let Some(lost) =
                                record["bindings"].as_object_mut().unwrap().remove(name)
                            {
                                if !lost["history"].is_null() || !lost["discovery"].is_null() {
                                    record["retained_bindings"][name] = lost;
                                }
                            }
                            record["binding_errors"][name] = json!(error.to_string());
                        } else if let Some(errors) = record["binding_errors"].as_object_mut() {
                            errors.remove(name);
                        }
                        save(&root, &record)?;
                    }
                    if !cancelled(&root, &options["opener_instance"])? {
                        let started = sync_daemon(
                            &request,
                            &root,
                            &mut record,
                            deadline.saturating_duration_since(Instant::now()),
                        )?;
                        record["phase"] = json!(if started
                            && record["bindings"].as_object().unwrap().len()
                                == request["config"]["agents"].as_object().unwrap().len()
                            && (!has_host
                                || request["config"]["agents"]
                                    .as_object()
                                    .unwrap()
                                    .iter()
                                    .all(|(name, spec)| !hosted(spec)
                                        || mounted["agents"][name]["status"] == "ready"))
                        {
                            "running"
                        } else if mounted["status"] == "blocked" {
                            "waiting_input"
                        } else if started {
                            "native_hosts_starting"
                        } else if record["daemon"].is_null() && mounted["status"] == "ready" {
                            "binding_sessions"
                        } else if record["daemon"].is_null() {
                            "agents_starting"
                        } else {
                            "daemon_starting"
                        });
                    }
                }
            }
        }
        if record["phase"] == "running" {
            // Publish recovery completion before observers can see running.
            // finish writes the recovery record before the matching open record.
            recovery::finish(&root, &record)?;
        }
        save(&root, &record)?;
        if record["phase"] == "running"
            || record["phase"] == "namespace_uncertain"
            || Instant::now() >= deadline
        {
            break;
        }
        std::thread::sleep(Duration::from_millis(250));
    }
    if record["phase"] != "running"
        && record["phase"] != "cancelled"
        && record["phase"] != "namespace_uncertain"
    {
        record["last_startup_phase"] = record["phase"].clone();
        record["phase"] = json!("startup_timed_out");
        save(&root, &record)?;
    }
    Ok(
        json!({"status":record["phase"],"last_startup_phase":record["last_startup_phase"],"agents":readiness(&config,&record,&host_activation(&runtime,&config,&record)),"project_id":record["project_id"],"session":record["session"],"workspace_id":record["workspace_id"],"state_root":runtime,"daemon":record["daemon"],"observation_expired":record["phase"]!="running"}),
    )
}
pub(crate) fn launch(
    project: &Path,
    config: Value,
    options: &Value,
    timeout: Duration,
) -> Result<Value> {
    use std::io::Write;
    ensure!(options.is_object(), "Herdr options must be an object");
    for (key, valid) in [
        ("dry_run", options["dry_run"].is_boolean()),
        ("recover_stopped", options["recover_stopped"].is_boolean()),
        ("startup_timeout_ms", options["startup_timeout_ms"].is_u64()),
        ("session", options["session"].is_string()),
        (
            "transport_timeout_ms",
            options["transport_timeout_ms"].is_u64(),
        ),
        ("config", options["config"].is_object()),
        ("history_roots", options["history_roots"].is_object()),
        ("bindings", options["bindings"].is_object()),
    ] {
        ensure!(
            options[key].is_null() || valid,
            "invalid Herdr option type: {key}"
        );
    }
    ensure!(
        options["startup_timeout_ms"].is_null()
            || options["startup_timeout_ms"]
                .as_u64()
                .is_some_and(|ms| (1000..=600000).contains(&ms)),
        "Herdr background startup budget must be 1000..600000 ms"
    );
    if options["dry_run"] == true {
        return open(project, config, options, Duration::ZERO);
    }
    let runtime = crate::config::state_root(project)?;
    let root = runtime.join("herdr");
    for path in root.ancestors() {
        crate::herdr_namespace::unlinked(path)?;
    }
    fs::create_dir_all(&root)?;
    let lock_path = root.join("native-open-launch.lock");
    crate::herdr_namespace::unlinked(&lock_path)?;
    let lock = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(lock_path)?;
    ensure!(
        lock.try_lock().is_ok(),
        "another Herdr launch request is being recorded"
    );
    let request_hash = hash(&json!([project, config, options]))?;
    let prior = read(&root, "herdr-open-start.json")?.unwrap_or(Value::Null);
    let cancellation = read(&root, "open-cancel.json")?.unwrap_or(Value::Null);
    let shutdown_requested = prior["cancel_requested"] == true
        || !prior["instance"].is_null() && cancellation["instance"] == prior["instance"];
    if shutdown_requested {
        ensure!(read(&root, "namespace.json")?.is_none_or(|n| n["status"] == "retired"), "Herdr startup cancellation is still closing its namespace; finish ccb kill before reopening");
    }
    let process = prior["process"].clone();
    let live = process["pid"]
        .as_u64()
        .filter(|p| *p > 0 && *p <= u32::MAX as u64)
        .map(|p| crate::owned_process::observe_external_process(p as u32));
    let reusable = match live {
        Some(Ok(observed)) => {
            ensure!(
                observed == process,
                "Herdr opener PID belongs to another process lifetime"
            );
            true
        }
        Some(Err(_)) => {
            ensure!(
                crate::daemon::owner_stopped(process["pid"].as_u64().unwrap() as u32),
                "Herdr opener ownership is uncertain"
            );
            false
        }
        None => false,
    };
    ensure!(
        !shutdown_requested || !reusable,
        "Herdr startup cancellation is still in progress; finish ccb kill before reopening"
    );
    let intent = if reusable {
        ensure!(
            prior["request_hash"] == request_hash,
            "a live Herdr opener has another configuration"
        );
        prior
    } else {
        let instance = uuid::Uuid::new_v4().to_string();
        let envelope =
            json!({"project_root":project,"config":config,"options":options,"instance":instance});
        let mut bytes = serde_json::to_vec(&envelope)?;
        ensure!(
            bytes.len() < 1024 * 1024,
            "Herdr opener request exceeds limit"
        );
        bytes.push(b'\n');
        let mut child = Command::new(std::env::current_exe()?);
        child
            .arg("--herdr-open-worker")
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            child.creation_flags(0x08000000 | 0x00000200);
        }
        #[cfg(windows)]
        let guard = crate::startup::StandardHandles::guard()?;
        let mut child = child.spawn()?;
        #[cfg(windows)]
        drop(guard);
        let intent = json!({"schema_version":1,"project_id":config["project_id"],"instance":instance,"request_hash":request_hash,"process":crate::owned_process::observe_external_process(child.id())?,"status":"opening",
            "startup_timeout_ms":options["startup_timeout_ms"].as_u64().unwrap_or(60000)});
        let mut store = crate::store::Store::open(&root)?;
        store.save("herdr-open-start.json", &intent)?;
        store.commit()?;
        drop(store);
        child
            .stdin
            .take()
            .context("missing opener bootstrap input")?
            .write_all(&bytes)?;
        intent
    };
    drop(lock);
    let deadline = Instant::now() + timeout;
    loop {
        let state =
            read(&root, "herdr-open-start.json")?.context("Herdr open observation disappeared")?;
        ensure!(
            state["instance"] == intent["instance"],
            "Herdr open observation generation changed"
        );
        if state["result"].is_object() {
            return Ok(state["result"].clone());
        }
        if state["status"] == "failed" {
            anyhow::bail!(
                "Herdr open failed: {}",
                state["error"].as_str().unwrap_or("unknown stage")
            );
        }
        if Instant::now() >= deadline {
            let record = read(&root, RECORD)?
                .filter(|r| r["observer_instance"] == intent["instance"])
                .unwrap_or(Value::Null);
            return Ok(
                json!({"status":record["phase"].as_str().unwrap_or("opening"),"agents":readiness(&config,&record,&host_activation(&runtime,&config,&record)),"daemon":record["daemon"],"session":record["session"],"workspace_id":record["workspace_id"],"state_root":runtime,"opener":intent["process"],"observation_expired":true}),
            );
        }
        if crate::daemon::owner_stopped(intent["process"]["pid"].as_u64().unwrap() as u32) {
            anyhow::bail!(
                "Herdr opener exited without a recorded result; repeat open to resume its journals"
            );
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}
pub(crate) fn worker(envelope: &Value) -> Result<()> {
    let project = Path::new(crate::records::string(envelope, "project_root")?);
    let root = crate::config::state_root(project)?.join("herdr");
    let mut options = envelope["options"].clone();
    options["opener_instance"] = envelope["instance"].clone();
    let outcome = open(
        project,
        envelope["config"].clone(),
        &options,
        Duration::from_millis(options["startup_timeout_ms"].as_u64().unwrap_or(60000)),
    );
    let mut store = crate::store::Store::open(&root)?;
    let mut state = store
        .json("herdr-open-start.json")?
        .context("missing opener intent")?;
    ensure!(
        state["instance"] == envelope["instance"] && state["process"]["pid"] == std::process::id(),
        "opener result ownership changed"
    );
    match outcome {
        Ok(result) => {
            state["status"] = json!("finished");
            if result["status"] == "cancelled" {
                state["cancel_requested"] = json!(true);
            }
            state["result"] = result;
        }
        Err(error) => {
            state["status"] = json!("failed");
            state["error"] = json!(error.to_string());
        }
    }
    store.save("herdr-open-start.json", &state)?;
    store.commit()
}
pub(crate) fn attach(options: &Value, result: &Value) -> Result<()> {
    let request = json!({"command":command(options)?,"session":result["session"]});
    let focused = crate::herdr_transport::execute(
        &json!({"schema_version":1,"operation":"attach_focus","command":request["command"],"session":request["session"],
        "args":["workspace","focus",crate::records::string(result,"workspace_id")?],"expect_json":false,"timeout_ms":10000}),
    )?;
    ensure!(focused["ok"] == true, "Herdr workspace focus failed");
    let status = launch_command(&request)?
        .args([
            "session",
            "attach",
            crate::records::string(result, "session")?,
        ])
        .status()?;
    ensure!(status.success(), "Herdr foreground attach failed");
    Ok(())
}
