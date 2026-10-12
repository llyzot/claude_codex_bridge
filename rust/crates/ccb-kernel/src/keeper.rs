//! One native supervisor per state root. Process lifetime, not failed RPCs,
//! authorizes recovery. Stop intent is fenced by the supervisor generation.
use crate::startup::{load, save, unlinked};
use anyhow::{ensure, Context, Result};
use serde_json::{json, Value};
use std::{
    fs::{self, File, OpenOptions},
    io::Write,
    path::Path,
    process::{Command, Stdio},
    time::{Duration, Instant},
};

const RECORD: &str = "native-keeper.json";
fn row(root: &Path) -> Result<Option<Value>> {
    let value = load(&root.join("kernel").join(RECORD))?;
    if let Some(v) = &value {
        ensure!(
            v["schema_version"] == 1
                && v["record_type"] == "native_keeper"
                && v["instance_id"]
                    .as_str()
                    .is_some_and(|s| uuid::Uuid::parse_str(s).is_ok())
                && v["project_id"].is_string()
                && v["process"].is_object(),
            "invalid native Keeper identity"
        );
    }
    Ok(value)
}
fn alive(process: &Value) -> Result<bool> {
    crate::startup::owner_alive(&json!({"pid":process["pid"],"process":process}))
}
fn lock(root: &Path, name: &str) -> Result<File> {
    let path = root.join("kernel").join(name);
    unlinked(&path)?;
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)?;
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        if file.try_lock().is_ok() {
            return Ok(file);
        }
        ensure!(
            Instant::now() < deadline,
            "native Keeper control is busy; observe its status before retrying"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}
fn publish(root: &Path, record: &mut Value, phase: &str) -> Result<()> {
    ensure!(
        row(root)?.is_some_and(|r| r["instance_id"] == record["instance_id"]),
        "native Keeper generation changed"
    );
    record["phase"] = json!(phase);
    record["last_progress_at"] = json!(crate::daemon::now());
    save(&root.join("kernel").join(RECORD), record)
}
fn stopped(root: &Path, record: &Value) -> Result<bool> {
    if let Some(intent) = load(&root.join("kernel/native-keeper-stop.json"))? {
        ensure!(
            intent["schema_version"] == 1 && intent["record_type"] == "native_keeper_stop",
            "invalid Keeper stop intent"
        );
        if intent["instance_id"] == record["instance_id"] {
            ensure!(
                intent["project_id"] == record["project_id"],
                "Keeper stop project changed"
            );
            return Ok(true);
        }
    }
    let lease = load(&root.join("lease.json"))?.unwrap_or(Value::Null);
    let lifecycle = load(&root.join("lifecycle.json"))?.unwrap_or(Value::Null);
    Ok(lease["process"] == record["daemon"]
        && !record["daemon"].is_null()
        && lease["project_id"] == record["project_id"]
        && lifecycle["owner_daemon_instance_id"] == lease["daemon_instance_id"]
        && lifecycle["desired_state"] == "stopped")
}
pub(crate) fn status(root: &Path) -> Result<Value> {
    let record = row(root)?;
    let active = record
        .as_ref()
        .map(|r| alive(&r["process"]))
        .transpose()?
        .unwrap_or(false);
    Ok(json!({"status":"observed","keeper":record,"alive":active}))
}
pub(crate) fn stop(root: &Path) -> Result<Option<Value>> {
    let Some(prior) = row(root)? else {
        return Ok(None);
    };
    if !alive(&prior["process"])?
        || ["failed", "stopped"].contains(&prior["phase"].as_str().unwrap_or(""))
    {
        return Ok(None);
    }
    let _guard = lock(root, "native-keeper-launch.lock")?;
    let record = row(root)?.context("native Keeper disappeared")?;
    ensure!(
        record["instance_id"] == prior["instance_id"],
        "native Keeper changed before stop; observe its status"
    );
    if ["failed", "stopped"].contains(&record["phase"].as_str().unwrap_or("")) {
        return Ok(None);
    }
    save(
        &root.join("kernel/native-keeper-stop.json"),
        &json!({"schema_version":1,"record_type":"native_keeper_stop",
        "instance_id":record["instance_id"],"project_id":record["project_id"],"requested_at":crate::daemon::now()}),
    )?;
    drop(_guard);
    // Also fence the daemon itself when reachable, so losing the Keeper after
    // this receipt cannot abandon an otherwise responsive stop request.
    let _ = crate::cli::request(root, "shutdown", json!({}));
    Ok(Some(
        json!({"api_version":2,"ok":true,"status":"stopping","keeper_instance_id":record["instance_id"]}),
    ))
}
fn daemon_identity(root: &Path, result: &Value) -> Result<Value> {
    let pid = result["pid"]
        .as_u64()
        .filter(|p| *p > 0 && *p <= u32::MAX as u64)
        .context("native startup has no daemon identity")?;
    match crate::owned_process::observe_external_process(pid as u32) {
        Ok(process) => Ok(process),
        Err(error) => {
            let intent = load(&root.join("kernel/native-start.json"))?.unwrap_or(Value::Null);
            ensure!(
                intent["pid"] == pid && !crate::startup::owner_alive(&intent)?,
                "daemon lifetime became unobservable: {error}"
            );
            Ok(intent["process"].clone())
        }
    }
}
fn wait(root: &Path, config: &Value, timeout: Duration) -> Result<Value> {
    let fingerprint = crate::config::fingerprint(config)?;
    let end = Instant::now() + timeout;
    loop {
        let record = row(root)?.context("native Keeper observation missing")?;
        ensure!(
            alive(&record["process"])?,
            "native Keeper exited; inspect keeper-status before retrying"
        );
        ensure!(
            record["phase"] != "failed" && !stopped(root, &record)?,
            "native Keeper is failed or stopping; observe keeper-status"
        );
        if record["phase"] == "watching" {
            if let Some(response) = crate::startup::ping(root, &config["project_id"], &fingerprint)?
            {
                ensure!(
                    response["pid"] == record["daemon"]["pid"],
                    "native Keeper and daemon generation disagree"
                );
                return Ok(
                    json!({"status":"running","pid":response["pid"],"startup_id":response["startup_id"],
                    "project_id":response["project_id"],"state_root":root,"keeper":record["process"],"keeper_instance_id":record["instance_id"]}),
                );
            }
        }
        if Instant::now() >= end {
            return Ok(
                json!({"status":"starting","pid":if alive(&record["daemon"])? {record["daemon"]["pid"].clone()} else {Value::Null},
                "startup_id":record["startup_id"],"project_id":config["project_id"],"state_root":root,"observation_expired":true,"keeper":record["process"]}),
            );
        }
        std::thread::sleep(Duration::from_millis(25));
    }
}
pub(crate) fn start(root: &Path, config: Value, timeout: Duration) -> Result<Value> {
    crate::daemon::Daemon::validate_config(&config)?;
    unlinked(root)?;
    fs::create_dir_all(root)?;
    let root = root.canonicalize()?;
    unlinked(&root.join("kernel"))?;
    fs::create_dir_all(root.join("kernel"))?;
    let guard = lock(&root, "native-keeper-launch.lock")?;
    if let Some(prior) = row(&root)? {
        ensure!(
            prior["project_id"] == config["project_id"],
            "state root belongs to another service or project (native Keeper)"
        );
        let deadline = Instant::now() + Duration::from_secs(3);
        while alive(&prior["process"])? && stopped(&root, &prior)? {
            ensure!(
                Instant::now() < deadline,
                "prior native Keeper is still stopping"
            );
            std::thread::sleep(Duration::from_millis(25));
        }
        if alive(&prior["process"])? {
            drop(guard);
            let mut result = wait(&root, &config, timeout)?;
            if result["status"] == "running" {
                result["already_running"] = json!(true);
            }
            return Ok(result);
        }
    }
    let result = crate::startup::start_daemon(&root, config.clone(), Duration::ZERO)?;
    if !result["pid"].is_u64() {
        return Ok(result);
    }
    let daemon = daemon_identity(&root, &result)?;
    let instance = uuid::Uuid::new_v4().to_string();
    let envelope =
        json!({"state_root":root,"instance_id":instance,"project_id":config["project_id"]});
    let mut bytes = serde_json::to_vec(&envelope)?;
    ensure!(
        bytes.len() <= 2 * 1024 * 1024,
        "Keeper bootstrap exceeds size limit"
    );
    bytes.push(b'\n');
    let stderr = root.join("native-keeper.stderr.log");
    unlinked(&stderr)?;
    let mut command = Command::new(std::env::current_exe()?);
    command
        .arg("--keeper-worker")
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(OpenOptions::new().create(true).append(true).open(stderr)?);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x08000000 | 0x00000200);
    }
    #[cfg(windows)]
    let handles = crate::startup::StandardHandles::guard()?;
    let mut child = command.spawn()?;
    #[cfg(windows)]
    drop(handles);
    let record = json!({"schema_version":1,"record_type":"native_keeper","instance_id":instance,
        "project_id":config["project_id"],"process":crate::owned_process::observe_external_process(child.id())?,
        "daemon":daemon,"startup_id":result["startup_id"],"phase":"bootstrapping","restart_count":0,"started_at":crate::daemon::now()});
    save(&root.join("kernel").join(RECORD), &record)?;
    child
        .stdin
        .take()
        .context("Keeper bootstrap pipe missing")?
        .write_all(&bytes)?;
    drop(guard);
    wait(&root, &config, timeout)
}
pub(crate) fn worker(envelope: &Value) -> Result<()> {
    ensure!(cfg!(windows), "native Keeper currently requires Windows");
    let root = Path::new(crate::records::string(envelope, "state_root")?);
    let _lifetime = lock(root, "native-keeper.lock")?;
    let mut record = row(root)?.context("native Keeper bootstrap identity missing")?;
    ensure!(
        record["instance_id"] == envelope["instance_id"]
            && record["process"]["pid"] == std::process::id()
            && record["project_id"] == envelope["project_id"],
        "native Keeper bootstrap ownership changed"
    );
    let result = run(root, &mut record);
    if result.is_err() {
        record["reason"] = json!("recovery_failed_inspect_native_logs");
        let _ = publish(root, &mut record, "failed");
    }
    result
}
fn run(root: &Path, record: &mut Value) -> Result<()> {
    publish(root, record, "watching")?;
    let mut since = Instant::now();
    let mut failures = 0u32;
    loop {
        let live = alive(&record["daemon"])?;
        if stopped(root, record)? {
            if !live {
                publish(root, record, "stopped")?;
                return Ok(());
            }
            if record["phase"] != "stopping" {
                publish(root, record, "stopping")?;
            }
            let _ = crate::cli::request(root, "shutdown", json!({}));
            std::thread::sleep(Duration::from_millis(100));
            continue;
        }
        if live {
            if since.elapsed() >= Duration::from_secs(60) {
                failures = 0;
            }
            std::thread::sleep(Duration::from_millis(200));
            continue;
        }
        failures += 1;
        if failures > 3 {
            record["reason"] = json!("restart_limit_reached");
            publish(root, record, "failed")?;
            return Ok(());
        }
        record["consecutive_failures"] = json!(failures);
        publish(root, record, "backoff")?;
        let deadline = Instant::now() + Duration::from_millis(500u64 << (failures - 1));
        while Instant::now() < deadline {
            if stopped(root, record)? {
                publish(root, record, "stopped")?;
                return Ok(());
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        let _launch = lock(root, "native-keeper-launch.lock")?;
        if stopped(root, record)? {
            publish(root, record, "stopped")?;
            return Ok(());
        }
        // A missing or invalid checkpoint needs an explicit configuration.
        // Never resurrect the launcher's potentially stale pre-reload document.
        let config = crate::restart_config::load(root)?;
        ensure!(
            config["project_id"] == record["project_id"],
            "native Keeper recovery project changed"
        );
        publish(root, record, "restarting")?;
        let prior = load(&root.join("kernel/native-start.json"))?.unwrap_or(Value::Null);
        let fingerprint = crate::config::fingerprint(&config)?;
        match crate::startup::start_daemon(root, config, Duration::ZERO) {
            Ok(result) => {
                record["daemon"] = daemon_identity(root, &result)?;
                record["startup_id"] = result["startup_id"].clone();
            }
            Err(error) => {
                // A replacement can die after publishing its endpoint but before
                // startup observes readiness. Count that concrete new lifetime;
                // never retry an error that has no new, matching spawn proof.
                let intent = load(&root.join("kernel/native-start.json"))?.unwrap_or(Value::Null);
                ensure!(
                    intent["startup_id"].is_string()
                        && intent["startup_id"] != prior["startup_id"]
                        && intent["project_id"] == record["project_id"]
                        && intent["config_fingerprint"] == fingerprint,
                    "native restart did not publish a matching new lifetime: {error}"
                );
                crate::startup::owner_alive(&intent)?;
                record["daemon"] = intent["process"].clone();
                record["startup_id"] = intent["startup_id"].clone();
                record["launch_observation"] = json!("readiness_unobserved");
            }
        }
        record["restart_count"] = json!(record["restart_count"].as_u64().unwrap_or(0) + 1);
        since = Instant::now();
        publish(root, record, "watching")?;
    }
}
