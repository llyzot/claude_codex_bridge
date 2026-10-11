//! First real Codex input can precede rollout persistence and native session reporting.
//! Route by the mounted process, then bind only a structured execution-specific anchor.
use crate::{
    herdr_agents,
    herdr_worker::Event,
    terminal_history::{self, Binding, Reader},
};
use anyhow::{ensure, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    collections::BTreeMap,
    fs,
    io::{BufRead, BufReader, Read},
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Discovery {
    root: PathBuf,
    cwd: PathBuf,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    session_id: Option<String>,
}
pub(crate) fn validate(value: &Value, cwd: &Value) -> Result<()> {
    let d: Discovery = serde_json::from_value(value.clone())?;
    ensure!(
        d.root.is_absolute() && d.cwd.is_absolute() && d.cwd.is_dir() && json!(d.cwd) == *cwd,
        "invalid deferred history scope"
    );
    for path in d.root.ancestors() {
        crate::herdr_namespace::unlinked(path)?;
    }
    if let Some(id) = d.session_id {
        uuid::Uuid::parse_str(&id)?;
    }
    Ok(())
}
fn millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}
fn candidates(d: &Discovery, since: u64) -> Result<Vec<Binding>> {
    if !d.root.exists() {
        return Ok(vec![]);
    }
    let mut dirs = vec![(d.root.clone(), 0)];
    let mut count = 0;
    let mut found = Vec::new();
    while let Some((dir, depth)) = dirs.pop() {
        crate::herdr_namespace::unlinked(&dir)?;
        for item in fs::read_dir(dir)? {
            count += 1;
            ensure!(
                count <= 20000,
                "Codex deferred discovery entry limit exceeded"
            );
            let item = item?;
            let kind = item.file_type()?;
            if kind.is_symlink() {
                continue;
            }
            if kind.is_dir() && depth < 4 {
                dirs.push((item.path(), depth + 1));
                continue;
            }
            if !kind.is_file() || item.path().extension().is_none_or(|e| e != "jsonl") {
                continue;
            }
            let modified = item
                .metadata()?
                .modified()?
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis() as u64;
            if modified < since {
                continue;
            }
            crate::herdr_namespace::unlinked(&item.path())?;
            let mut first = Vec::new();
            BufReader::new(fs::File::open(item.path())?)
                .take(4 * 1024 * 1024 + 1)
                .read_until(b'\n', &mut first)?;
            ensure!(
                first.len() <= 4 * 1024 * 1024,
                "Codex discovery header exceeds limit"
            );
            if first.last() != Some(&b'\n') {
                continue;
            }
            let Ok(header) = serde_json::from_slice::<Value>(&first) else {
                continue;
            };
            let p = &header["payload"];
            if header["type"] != "session_meta" || !p["source"]["subagent"].is_null() {
                continue;
            }
            let Some(cwd) = p["cwd"].as_str() else {
                continue;
            };
            if !crate::pi_terminal::same_session_path(&json!(cwd), &json!(d.cwd)) {
                continue;
            }
            let Some(id) = p["id"]
                .as_str()
                .filter(|id| uuid::Uuid::parse_str(id).is_ok())
            else {
                continue;
            };
            if d.session_id.as_ref().is_some_and(|expected| expected != id) {
                continue;
            }
            found.push(Binding {
                root: d.root.clone(),
                path: item.path(),
                cwd: d.cwd.clone(),
                session_id: id.into(),
            });
            ensure!(
                found.len() <= 32,
                "ambiguous recent Codex histories exceed candidate limit"
            );
        }
    }
    Ok(found)
}
fn observed(request: &Value, agent: &str, identity: Option<&Value>) -> Result<Value> {
    let live = herdr_agents::bound(request, agent)?;
    if let Some(identity) = identity {
        ensure!(
            live["identity"] == *identity,
            "Codex dispatch process/namespace changed"
        );
    }
    Ok(live)
}
#[allow(clippy::too_many_arguments)]
pub(crate) fn run(
    request: &Value,
    agent: &str,
    spec: &Value,
    task: &str,
    execution: &str,
    saved: &Value,
    prompt: &str,
    events: &mpsc::Sender<Event>,
    grant: &mpsc::Receiver<()>,
    alive: &AtomicBool,
    permitted: &mut bool,
) -> Result<()> {
    let d: Discovery = serde_json::from_value(spec["herdr_discovery"].clone())?;
    validate(&spec["herdr_discovery"], &spec["workspace_path"])?;
    let anchor = format!("{task}:{execution}");
    let input = format!("CCB_REQ_ID: {anchor}\n{prompt}");
    let deadline = Duration::from_secs_f64(
        spec["timeout_s"]
            .as_f64()
            .filter(|t| t.is_finite() && *t > 0.0)
            .unwrap_or(3600.0),
    );
    let proof = if saved.is_null() {
        ensure!(
            !input.contains('\0')
                && input.encode_utf16().count() <= 8000
                && request["command"].to_string().encode_utf16().count() <= 8000,
            "deferred prompt exceeds argv limit"
        );
        let start = Instant::now();
        let live = loop {
            if !alive.load(Ordering::Acquire) {
                return Ok(());
            }
            let live = observed(request, agent, None)?;
            if live["interactive_ready"] == true {
                break live;
            }
            ensure!(start.elapsed() < deadline, "Codex did not become ready");
            std::thread::sleep(Duration::from_millis(100));
        };
        let cache = &spec["_herdr_history_cache"];
        let mut proof = json!({"schema_version":1,"job_id":task,"execution_instance_id":execution,"request_anchor":anchor,"fixed_history":false,
            "identity":live["identity"],"discovery":spec["herdr_discovery"],"not_before_ms":millis().saturating_sub(10000),"history":null,"baseline":null});
        if cache["schema_version"] == 1
            && cache["identity"] == live["identity"]
            && cache["discovery"] == spec["herdr_discovery"]
        {
            let binding: Binding = serde_json::from_value(cache["history"].clone())?;
            ensure!(
                binding.root == d.root && binding.cwd == d.cwd,
                "cached history scope changed"
            );
            if binding.path.is_file()
                && (live["agent_session"].is_null()
                    || live["agent_session"]["kind"] == "id"
                        && live["agent_session"]["value"] == binding.session_id)
            {
                proof["history"] = json!(binding);
                proof["baseline"] = json!(terminal_history::baseline(&binding)?);
                proof["fixed_history"] = json!(!live["agent_session"].is_null());
            }
        }
        events.send(Event::Prepared(proof.clone()))?;
        loop {
            if !alive.load(Ordering::Acquire) {
                return Ok(());
            }
            match grant.recv_timeout(Duration::from_millis(100)) {
                Ok(()) => {
                    *permitted = true;
                    break;
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(_) => return Ok(()),
            }
        }
        if !alive.load(Ordering::Acquire) {
            return Ok(());
        }
        let live = observed(request, agent, Some(&proof["identity"]))?;
        ensure!(
            live["interactive_ready"] == true,
            "Codex readiness changed before prompt"
        );
        let _ = herdr_agents::rpc(
            request,
            vec![
                "agent".into(),
                "prompt".into(),
                crate::records::string(&proof["identity"], "herdr_name")?.into(),
                input,
            ],
        );
        proof
    } else {
        ensure!(
            saved["schema_version"] == 1
                && saved["job_id"] == task
                && saved["execution_instance_id"] == execution
                && saved["request_anchor"] == anchor
                && saved["discovery"] == spec["herdr_discovery"],
            "invalid deferred dispatch proof"
        );
        saved.clone()
    };
    let since = proof["not_before_ms"]
        .as_u64()
        .context("missing discovery time boundary")?;
    let mut readers: BTreeMap<PathBuf, (Binding, Reader)> = BTreeMap::new();
    let fixed = proof["fixed_history"] == true;
    if proof["history"].is_object() {
        let binding: Binding = serde_json::from_value(proof["history"].clone())?;
        ensure!(
            binding.root == d.root && binding.cwd == d.cwd,
            "saved history scope changed"
        );
        let reader = Reader::new(
            binding.clone(),
            serde_json::from_value(proof["baseline"].clone())?,
            anchor.clone(),
        )?;
        readers.insert(binding.path.clone(), (binding, reader));
    }
    let start = Instant::now();
    let mut scan = Instant::now().checked_sub(Duration::from_secs(2)).unwrap();
    let mut completed = false;
    while alive.load(Ordering::Acquire) {
        if !completed {
            if !fixed && scan.elapsed() >= Duration::from_secs(1) {
                for binding in candidates(&d, since)? {
                    if !readers.contains_key(&binding.path) {
                        let baseline = terminal_history::from_header(&binding)?;
                        let reader = Reader::new(binding.clone(), baseline, anchor.clone())?;
                        readers.insert(binding.path.clone(), (binding, reader));
                    }
                }
                ensure!(readers.len() <= 32, "Codex candidate limit exceeded");
                scan = Instant::now();
            }
            let mut matches = 0;
            let mut result = None;
            for (binding, reader) in readers.values_mut() {
                let terminal = reader.poll()?;
                if reader.anchored() {
                    matches += 1;
                }
                if let Some(decision) = terminal {
                    result = Some((binding.clone(), decision));
                }
            }
            ensure!(
                matches <= 1,
                "request anchor appears in multiple Codex histories"
            );
            if let Some((binding, mut decision)) = result {
                decision["diagnostics"]["binding_authority"] =
                    json!("mounted_process_and_execution_anchor");
                decision["diagnostics"]["native_execution_instance"] = json!(execution);
                events.send(Event::Bound(json!({"schema_version":1,"identity":proof["identity"],"discovery":spec["herdr_discovery"],"history":binding})))?;
                events.send(Event::Completed(decision))?;
                completed = true;
            }
        }
        let live = observed(request, agent, Some(&proof["identity"]))?;
        if completed && live["interactive_ready"] == true {
            events.send(Event::Ready)?;
            return Ok(());
        }
        ensure!(
            start.elapsed() < deadline,
            "Codex history/terminal outcome is still unknown"
        );
        std::thread::sleep(Duration::from_millis(100));
    }
    Ok(())
}
