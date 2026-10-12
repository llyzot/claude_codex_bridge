//! Provision the small original-TUI Pi event adapter without installing a profile.
use crate::terminal_history::Binding;
use anyhow::{ensure, Context, Result};
use serde_json::{json, Value};
use std::path::Path;

pub(crate) fn validate_producer(value: &Value) -> Result<()> {
    ensure!(
        value["schema_version"] == 1 && value.as_object().is_some_and(|v| v.len() == 5),
        "invalid Pi bridge producer schema"
    );
    for field in ["owner", "instance"] {
        uuid::Uuid::parse_str(crate::records::string(value, field)?)?;
    }
    ensure!(
        value["process"]["pid"]
            .as_u64()
            .is_some_and(|p| p > 0 && p <= u32::MAX as u64),
        "invalid Pi producer PID"
    );
    ensure!(
        value["process"].as_object().is_some_and(|v| v.len() == 3)
            && value["process"]["created"]
                .as_str()
                .is_some_and(|v| v.parse::<u64>().is_ok_and(|t| t > 0))
            && value["process"]["windows_session"]
                .as_u64()
                .is_some_and(|n| n <= u32::MAX as u64),
        "invalid Pi process lifetime identity"
    );
    ensure!(
        Path::new(crate::records::string(value, "session_path")?).is_absolute(),
        "Pi session path must be absolute"
    );
    Ok(())
}
pub(crate) fn same_session_path(a: &Value, b: &Value) -> bool {
    let (Some(a), Some(b)) = (a.as_str(), b.as_str()) else {
        return false;
    };
    let (a, b) = (Path::new(a), Path::new(b));
    if !a.is_absolute() || !b.is_absolute() {
        return false;
    }
    if let (Ok(a), Ok(b)) = (a.canonicalize(), b.canonicalize()) {
        return a == b;
    }
    let normalized = |p: &Path| {
        let path = p.to_string_lossy().replace('\\', "/");
        let path = path.strip_prefix("//?/").unwrap_or(&path);
        if cfg!(windows) {
            path.to_lowercase()
        } else {
            path.into()
        }
    };
    normalized(a) == normalized(b)
}
pub(crate) fn resolve(binding: &Binding, owner: &str, agent: &str) -> Result<(Binding, Value)> {
    use std::io::Read;
    binding.validate()?;
    ensure!(
        binding.path == binding.root.join("current.json"),
        "Pi current-state path mismatch"
    );
    let mut bytes = Vec::new();
    std::fs::File::open(&binding.path)?
        .take(65537)
        .read_to_end(&mut bytes)?;
    ensure!(bytes.len() <= 65536, "Pi current state exceeds limit");
    let state: Value = serde_json::from_slice(&bytes)?;
    let producer = &state["bridge"];
    validate_producer(producer)?;
    ensure!(
        state["schema_version"] == 1
            && state["provider"] == "pi"
            && state["ready"] == true
            && state["owner"] == owner
            && state["agent_name"] == agent
            && producer["owner"] == owner
            && state["session_id"] == binding.session_id
            && same_session_path(&state["cwd"], &json!(binding.cwd)),
        "Pi current-state scope mismatch"
    );
    let path = binding.root.join(format!(
        "{}.jsonl",
        crate::records::string(producer, "instance")?
    ));
    ensure!(
        state["path"].as_str().map(Path::new) == Some(path.as_path()),
        "Pi event path does not match its generation"
    );
    let mut resolved = binding.clone();
    resolved.path = path;
    resolved.validate()?;
    Ok((resolved, producer.clone()))
}
pub(crate) fn normalize(
    event: &Value,
    producer: &Value,
    sequence: &mut Option<u64>,
) -> Result<Option<Value>> {
    ensure!(
        event["type"] == "pi_event" && event["bridge"] == *producer,
        "Pi event producer/generation changed"
    );
    let seq = event["seq"]
        .as_u64()
        .filter(|v| *v > 0)
        .context("invalid Pi event sequence")?;
    ensure!(
        sequence.is_none_or(|last| last.checked_add(1) == Some(seq)),
        "Pi event sequence is discontinuous"
    );
    *sequence = Some(seq);
    let p = &event["payload"];
    let kind = crate::records::string(p, "type")?;
    ensure!(
        !["session_changed", "bridge_shutdown", "request_superseded"].contains(&kind),
        "Pi execution/session was superseded"
    );
    let mut mapped = json!({"turn_id":p["turn_id"]});
    match kind {
        "agent_start" => {
            mapped["type"] = json!("task_started");
        }
        "user_message" => {
            mapped["type"] = json!("user_message");
            mapped["message"] = json!(format!(
                "CCB_REQ_ID: {}",
                p["request_anchor"].as_str().unwrap_or("")
            ));
        }
        "agent_settled" => {
            ensure!(
                p["idle"] == true && p["aborted"].is_boolean(),
                "Pi settlement has no idle/abort proof"
            );
            mapped["type"] = json!(if p["aborted"] == true {
                "turn_aborted"
            } else {
                "task_complete"
            });
            if p["aborted"] != true
                && (p["assistant"]["stop_reason"] != "stop" || p["assistant"]["error"] != false)
            {
                mapped["error"] = json!({"reason":"pi_nonfinal_or_error"});
            }
            mapped["last_agent_message"] = p["assistant"]["text"].clone();
        }
        _ => return Ok(None),
    }
    Ok(Some(json!({"type":"event_msg","payload":mapped})))
}

pub(crate) fn execute(request: &Value) -> Result<Value> {
    ensure!(
        request["schema_version"] == 1,
        "invalid Pi terminal bridge request"
    );
    let project = Path::new(crate::records::string(request, "project_root")?);
    ensure!(
        project.is_absolute() && project.is_dir(),
        "Pi bridge requires an absolute project directory"
    );
    let agent = crate::records::string(request, "agent_name")?;
    crate::models::validate_agent(agent)?;
    let runtime = crate::config::state_root(project)?;
    let state = runtime.join("herdr");
    let events = runtime.join("pi-terminal-events").join(agent);
    for path in state.ancestors().chain(events.ancestors()) {
        crate::herdr_namespace::unlinked(path)?;
    }
    let mut store = crate::store::Store::open(&state)?;
    let manifest = format!("providers/pi/{agent}/terminal-bridge.json");
    let script = format!("providers/pi/{agent}/terminal-bridge.mjs");
    let implementation = format!("providers/pi/{agent}/terminal-bridge-impl.mjs");
    let old = store.json(&manifest)?.unwrap_or(Value::Null);
    let owner = if old.is_null() {
        uuid::Uuid::new_v4().to_string()
    } else {
        ensure!(
            old["schema_version"] == 1
                && old["agent_name"] == agent
                && old["event_root"] == json!(events),
            "Pi bridge manifest scope changed"
        );
        let owner = crate::records::string(&old, "owner")?;
        uuid::Uuid::parse_str(owner).context("invalid Pi bridge owner")?;
        owner.into()
    };
    std::fs::create_dir_all(&events)?;
    let options = json!({"owner":owner,"agent_name":agent,"event_root":events,"native":std::env::current_exe()?});
    let source = include_str!("pi_terminal_bridge.mjs").replace(
        "__CCB_PI_TERMINAL_OPTIONS__",
        &serde_json::to_string(&options)?,
    );
    let path = store.owned_path(&script)?;
    let record = json!({"schema_version":1,"owner":owner,"agent_name":agent,"event_root":events,"extension":path,
        "implementation":store.owned_path(&implementation)?});
    if store.bytes(&implementation)?.as_deref() != Some(source.as_bytes()) {
        store.save_bytes(&implementation, source.as_bytes())?;
    }
    let loader = include_str!("pi_terminal_loader.mjs");
    if store.bytes(&script)?.as_deref() != Some(loader.as_bytes()) {
        store.save_bytes(&script, loader.as_bytes())?;
    }
    if old != record {
        store.save(&manifest, &record)?;
    }
    store.commit()?;
    Ok(json!({"bridge":record,"interactive_args":["-e",path],"provider_started":false}))
}
