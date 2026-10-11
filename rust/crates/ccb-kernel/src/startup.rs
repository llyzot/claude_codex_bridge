//! Background native startup. Observation expiry never restarts a live owner.
use anyhow::{ensure, Context, Result};
use serde_json::{json, Value};
use std::{
    fs::{self, OpenOptions},
    io::Write,
    path::Path,
    process::{Command, Stdio},
    time::{Duration, Instant},
};
#[cfg(windows)]
struct StandardHandles(Vec<(windows_sys::Win32::Foundation::HANDLE, u32)>);
#[cfg(windows)]
impl StandardHandles {
    fn guard() -> Result<Self> {
        use windows_sys::Win32::{Foundation::*, System::Console::*};
        let mut guard = Self(Vec::new());
        for kind in [STD_INPUT_HANDLE, STD_OUTPUT_HANDLE, STD_ERROR_HANDLE] {
            unsafe {
                let handle = GetStdHandle(kind);
                if handle.is_null() || handle == INVALID_HANDLE_VALUE {
                    continue;
                }
                let mut flags = 0;
                ensure!(
                    GetHandleInformation(handle, &mut flags) != 0,
                    "cannot inspect startup standard handle"
                );
                if flags & HANDLE_FLAG_INHERIT != 0 {
                    ensure!(
                        SetHandleInformation(handle, HANDLE_FLAG_INHERIT, 0) != 0,
                        "cannot fence startup standard handle"
                    );
                    guard.0.push((handle, flags));
                }
            }
        }
        Ok(guard)
    }
}
#[cfg(windows)]
impl Drop for StandardHandles {
    fn drop(&mut self) {
        use windows_sys::Win32::Foundation::*;
        for (handle, flags) in &self.0 {
            unsafe {
                SetHandleInformation(*handle, HANDLE_FLAG_INHERIT, *flags & HANDLE_FLAG_INHERIT);
            }
        }
    }
}

fn unlinked(path: &Path) -> Result<()> {
    if let Ok(metadata) = fs::symlink_metadata(path) {
        ensure!(
            !metadata.file_type().is_symlink(),
            "linked native startup path"
        );
        #[cfg(windows)]
        {
            use std::os::windows::fs::MetadataExt;
            ensure!(
                metadata.file_attributes() & 0x400 == 0,
                "reparse native startup path"
            );
        }
    }
    Ok(())
}
fn load(path: &Path) -> Result<Option<Value>> {
    unlinked(path)?;
    match fs::read(path) {
        Ok(bytes) => Ok(Some(serde_json::from_slice(&bytes)?)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.into()),
    }
}
fn save(path: &Path, value: &Value) -> Result<()> {
    unlinked(path)?;
    let temporary = path.with_extension(format!("{}.tmp", uuid::Uuid::new_v4().simple()));
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temporary)?;
    file.write_all(&serde_json::to_vec(value)?)?;
    file.sync_all()?;
    fs::rename(temporary, path)?;
    Ok(())
}
fn ping(root: &Path, project: &Value, fingerprint: &str) -> Result<Option<Value>> {
    if !root.join("kernel/control-plane.json").is_file() {
        return Ok(None);
    }
    if let Ok(response) = crate::cli::request(root, "ping", json!({})) {
        ensure!(
            response["runtime"] == "rust" && response["project_id"] == *project,
            "state root belongs to another service or project"
        );
        ensure!(
            response["config_fingerprint"] == fingerprint,
            "native service configuration changed; explicit reload/restart is required"
        );
        return Ok(Some(response));
    }
    Ok(None)
}
fn wait(root: &Path, intent: &Value, fingerprint: &str, timeout: Duration) -> Result<Value> {
    let started = Instant::now();
    let pid = intent["pid"]
        .as_u64()
        .filter(|p| *p > 0 && *p <= u32::MAX as u64)
        .context("invalid native startup owner")? as u32;
    loop {
        if let Some(response) = ping(root, &intent["project_id"], fingerprint)? {
            ensure!(
                response["pid"] == pid && response["startup_id"] == intent["startup_id"],
                "native startup readiness belongs to another process generation"
            );
            return Ok(
                json!({"status":"running","pid":pid,"project_id":intent["project_id"],
                "startup_id":intent["startup_id"],"state_root":root}),
            );
        }
        ensure!(
            !crate::daemon::owner_stopped(pid),
            "native startup owner exited before ready; inspect native.stderr.log"
        );
        if started.elapsed() >= timeout {
            return Ok(
                json!({"status":"starting","pid":pid,"project_id":intent["project_id"],
                "startup_id":intent["startup_id"],"state_root":root,"observation_expired":true}),
            );
        }
        std::thread::sleep(Duration::from_millis(25));
    }
}
pub fn start(root: &Path, mut config: Value, timeout: Duration) -> Result<Value> {
    crate::daemon::Daemon::validate_config(&config)?;
    let fingerprint = crate::config::fingerprint(&config)?;
    if let Some(response) = ping(root, &config["project_id"], &fingerprint)? {
        return Ok(
            json!({"status":"running","already_running":true,"pid":response["pid"],
            "project_id":response["project_id"],"startup_id":response["startup_id"],"state_root":root}),
        );
    }
    crate::daemon::verify_owner(root)?;
    fs::create_dir_all(root)?;
    let root = root.canonicalize()?;
    let kernel = root.join("kernel");
    unlinked(&kernel)?;
    fs::create_dir_all(&kernel)?;
    let lock_path = kernel.join("native-launch.lock");
    unlinked(&lock_path)?;
    let lock = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(&lock_path)?;
    if lock.try_lock().is_err() {
        return Ok(
            json!({"status":"startup_in_progress","project_id":config["project_id"],"state_root":root}),
        );
    }
    if let Some(response) = ping(&root, &config["project_id"], &fingerprint)? {
        return Ok(
            json!({"status":"running","already_running":true,"pid":response["pid"],
            "project_id":response["project_id"],"state_root":root}),
        );
    }
    let intent_path = kernel.join("native-start.json");
    if let Some(prior) = load(&intent_path)? {
        let pid = prior["pid"]
            .as_u64()
            .filter(|p| *p > 0 && *p <= u32::MAX as u64)
            .context("invalid native startup intent")? as u32;
        if !crate::daemon::owner_stopped(pid) {
            ensure!(
                prior["project_id"] == config["project_id"]
                    && prior["config_fingerprint"] == fingerprint,
                "pending native startup belongs to another project/configuration"
            );
            return wait(&root, &prior, &fingerprint, timeout);
        }
    }
    crate::daemon::verify_owner(&root)?;
    config["startup_id"] = json!(uuid::Uuid::new_v4().to_string());
    let mut bytes = serde_json::to_vec(&config)?;
    ensure!(
        bytes.len() <= 1024 * 1024,
        "native startup configuration is too large"
    );
    bytes.push(b'\n');
    let stdout = root.join("native.stdout.log");
    let stderr = root.join("native.stderr.log");
    unlinked(&stdout)?;
    unlinked(&stderr)?;
    let mut command = Command::new(std::env::current_exe()?);
    command.args([
        std::ffi::OsString::from("--state-root"),
        root.clone().into_os_string(),
        std::ffi::OsString::from("daemon"),
        std::ffi::OsString::from("--config-stdin"),
    ]);
    command
        .stdin(Stdio::piped())
        .stdout(OpenOptions::new().create(true).append(true).open(stdout)?)
        .stderr(OpenOptions::new().create(true).append(true).open(stderr)?);
    if let Some(cwd) = config["project_root"].as_str() {
        command.current_dir(cwd);
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x08000000 | 0x00000200);
    }
    #[cfg(windows)]
    let handle_guard = StandardHandles::guard()?;
    let mut child = command.spawn()?;
    #[cfg(windows)]
    drop(handle_guard);
    let intent = json!({"schema_version":1,"record_type":"native_start_intent","pid":child.id(),
        "startup_id":config["startup_id"],"project_id":config["project_id"],"config_fingerprint":fingerprint});
    save(&intent_path, &intent)?;
    let mut input = child
        .stdin
        .take()
        .context("missing native bootstrap input")?;
    input.write_all(&bytes)?;
    drop(input);
    // Child is standalone after bootstrap EOF. Keep its durable identity on timeout.
    wait(&root, &intent, &fingerprint, timeout)
}
