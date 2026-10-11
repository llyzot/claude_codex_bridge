use std::io::Read;
#[cfg(unix)]
use std::io::Write;
#[cfg(unix)]
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::Duration;

use serde_json::json;

use crate::model::ProjectViewResponse;

#[derive(Debug)]
pub struct CcbdClient {
    socket_path: std::path::PathBuf,
    timeout: Duration,
    native: Option<(std::path::PathBuf, std::path::PathBuf)>,
}

impl CcbdClient {
    pub fn new(socket_path: impl Into<std::path::PathBuf>) -> Self {
        Self {
            socket_path: socket_path.into(),
            timeout: Duration::from_secs(3),
            native: None,
        }
    }
    pub fn from_args(args: &crate::args::Args) -> Result<Self, String> {
        let Some(root) = &args.native_state_root else {
            return Ok(Self::new(args.ccbd_socket.clone()));
        };
        let binary = if let Some(path) = &args.native_bin {
            path.clone()
        } else {
            std::env::current_exe()
                .map_err(|_| "cannot locate native sidebar executable")?
                .parent()
                .ok_or("missing sidebar directory")?
                .join(if cfg!(windows) {
                    "ccb-native.exe"
                } else {
                    "ccb-native"
                })
        };
        if !binary.is_file() {
            return Err("native command helper is unavailable; specify --native-bin".into());
        }
        #[cfg(windows)]
        if !binary
            .extension()
            .is_some_and(|s| s.eq_ignore_ascii_case("exe"))
        {
            return Err("native helper must be an executable, not a shell script".into());
        }
        Ok(Self {
            socket_path: Default::default(),
            timeout: Duration::from_secs(3),
            native: Some((
                root.clone(),
                binary
                    .canonicalize()
                    .map_err(|_| "cannot resolve native helper")?,
            )),
        })
    }
    pub fn is_native(&self) -> bool {
        self.native.is_some()
    }
    pub fn project_view_json(&self) -> Result<serde_json::Value, String> {
        self.request("project_view", json!({"schema_version":1}))
    }
    pub fn stop_native(&self) -> Result<(), String> {
        if !self.is_native() {
            return Err("native stop requires a native state root".into());
        }
        self.request("shutdown", json!({})).map(|_| ())
    }

    pub fn project_view(&self) -> Result<ProjectViewResponse, String> {
        let payload = self.project_view_json()?;
        serde_json::from_value(payload)
            .map_err(|err| format!("invalid project_view response: {err}"))
    }

    pub fn focus_window(&self, window: &str, namespace_epoch: Option<u64>) -> Result<(), String> {
        let mut request = json!({"window": window});
        if let Some(epoch) = namespace_epoch {
            request["namespace_epoch"] = json!(epoch);
        }
        self.request("project_focus_window", request).map(|_| ())
    }

    pub fn focus_agent(&self, agent: &str, namespace_epoch: Option<u64>) -> Result<(), String> {
        let mut request = json!({"agent": agent});
        if let Some(epoch) = namespace_epoch {
            request["namespace_epoch"] = json!(epoch);
        }
        self.request("project_focus_agent", request).map(|_| ())
    }

    pub fn comms_recover(
        &self,
        job_id: &str,
        reply_delivery_job_id: Option<&str>,
        block_reason: Option<&str>,
    ) -> Result<(), String> {
        let mut request = json!({"job_id": job_id});
        if let Some(reply_job_id) = reply_delivery_job_id {
            request["reply_delivery_job_id"] = json!(reply_job_id);
        }
        if let Some(reason) = block_reason {
            request["block_reason"] = json!(reason);
        }
        self.request("comms_recover", request).map(|_| ())
    }

    pub fn cancel(&self, job_id: &str) -> Result<(), String> {
        self.request("cancel", json!({"job_id": job_id}))
            .map(|_| ())
    }

    pub fn dismiss_comms(&self, comms_id: &str) -> Result<(), String> {
        self.request("project_view_dismiss_comms", json!({"id": comms_id}))
            .map(|_| ())
    }

    pub fn reload_config(&self) -> Result<(), String> {
        let payload = self.request("project_reload_config", json!({"dry_run": false}))?;
        let status = payload
            .get("status")
            .and_then(|value| value.as_str())
            .unwrap_or("unknown");
        if matches!(status, "published" | "ok") || reload_is_no_change(&payload) {
            return Ok(());
        }
        Err(reload_error_message(&payload, status))
    }

    pub fn restart_panes(&self) -> Result<(), String> {
        self.request("project_restart_panes", json!({})).map(|_| ())
    }

    fn request(&self, op: &str, request: serde_json::Value) -> Result<serde_json::Value, String> {
        if let Some((root, binary)) = &self.native {
            if ![
                "project_view",
                "cancel",
                "project_view_dismiss_comms",
                "shutdown",
            ]
            .contains(&op)
            {
                return Err("This control is not available for the native runtime yet".into());
            }
            return request_native(root, binary, self.timeout, op, request);
        }
        request_unix_socket(&self.socket_path, self.timeout, op, request)
    }
}

// Reuse the native client's token ACL, endpoint/generation and loopback checks
// instead of duplicating its security implementation in the terminal renderer.
fn request_native(
    root: &Path,
    binary: &Path,
    timeout: Duration,
    op: &str,
    request: serde_json::Value,
) -> Result<serde_json::Value, String> {
    use std::{
        process::{Child, Command, Stdio},
        sync::mpsc,
        time::Instant,
    };
    struct Owned(Child);
    impl Drop for Owned {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    let mut command = Command::new(binary);
    command
        .args([
            std::ffi::OsStr::new("--state-root"),
            root.as_os_str(),
            std::ffi::OsStr::new("request"),
            std::ffi::OsStr::new(op),
        ])
        .arg(request.to_string())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x08000000);
    }
    let mut child = Owned(
        command
            .spawn()
            .map_err(|_| "cannot launch native command helper")?,
    );
    let (send, recv) = mpsc::channel();
    fn reader(
        mut input: impl Read + Send + 'static,
        send: std::sync::mpsc::Sender<(bool, Result<Vec<u8>, ()>)>,
        stderr: bool,
    ) {
        std::thread::spawn(move || {
            let mut bytes = Vec::new();
            let result = input.by_ref().take(1024 * 1024 + 1).read_to_end(&mut bytes);
            let _ = send.send((
                stderr,
                if result.is_ok() && bytes.len() <= 1024 * 1024 {
                    Ok(bytes)
                } else {
                    Err(())
                },
            ));
        });
    }
    reader(
        child.0.stdout.take().ok_or("missing native stdout")?,
        send.clone(),
        false,
    );
    reader(
        child.0.stderr.take().ok_or("missing native stderr")?,
        send,
        true,
    );
    let start = Instant::now();
    let (mut stdout, mut stderr, mut status) = (None, None, None);
    loop {
        while let Ok((err, bytes)) = recv.try_recv() {
            let bytes = bytes
                .map_err(|_| "native helper output exceeded its bound or could not be read")?;
            if err {
                stderr = Some(bytes)
            } else {
                stdout = Some(bytes)
            }
        }
        if status.is_none() {
            status = child
                .0
                .try_wait()
                .map_err(|_| "native helper status is unavailable")?;
        }
        if status.is_some() && stdout.is_some() && stderr.is_some() {
            break;
        }
        if start.elapsed() >= timeout {
            return Err("Native request timed out; its outcome is unknown. Refresh before retrying a control.".into());
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    if !status.unwrap().success() {
        let text = String::from_utf8_lossy(stderr.as_ref().unwrap());
        let safe = text
            .chars()
            .filter(|c| !c.is_control() || c.is_whitespace())
            .take(1024)
            .collect::<String>();
        return Err(if safe.trim().is_empty() {
            "native command failed".into()
        } else {
            safe.trim().into()
        });
    }
    let value: serde_json::Value = serde_json::from_slice(stdout.as_ref().unwrap())
        .map_err(|_| "invalid native helper response")?;
    if value["api_version"] != 2 || value["ok"] != true {
        return Err("invalid native response contract".into());
    }
    if op == "project_view" && value["view"]["schema_version"] != 1 {
        return Err("unsupported native ProjectView schema".into());
    }
    let mut payload = value
        .as_object()
        .cloned()
        .ok_or("native response is not an object")?;
    payload.remove("api_version");
    payload.remove("ok");
    Ok(serde_json::Value::Object(payload))
}

fn reload_error_message(payload: &serde_json::Value, status: &str) -> String {
    let reason = payload
        .get("diagnostics")
        .and_then(|value| value.get("reason"))
        .and_then(|value| value.as_str())
        .or_else(|| {
            payload
                .get("errors")
                .and_then(|value| value.as_array())
                .and_then(|values| values.iter().find_map(|value| value.as_str()))
        })
        .unwrap_or("reload rejected");
    format!("reload {status}: {reason}")
}

fn reload_is_no_change(payload: &serde_json::Value) -> bool {
    payload.get("plan_class").and_then(|value| value.as_str()) == Some("no_change")
}

#[cfg(unix)]
fn request_unix_socket(
    socket_path: &Path,
    timeout: Duration,
    op: &str,
    request: serde_json::Value,
) -> Result<serde_json::Value, String> {
    let mut stream = UnixStream::connect(socket_path)
        .map_err(|err| format!("connect {}: {err}", socket_path.display()))?;
    stream
        .set_read_timeout(Some(timeout))
        .map_err(|err| format!("set read timeout: {err}"))?;
    stream
        .set_write_timeout(Some(timeout))
        .map_err(|err| format!("set write timeout: {err}"))?;
    let request = json!({"api_version": 2, "op": op, "request": request});
    stream
        .write_all(format!("{request}\n").as_bytes())
        .map_err(|err| format!("write request: {err}"))?;
    let mut data = Vec::new();
    stream
        .read_to_end(&mut data)
        .map_err(|err| format!("read response: {err}"))?;
    let first_line = data
        .split(|byte| *byte == b'\n')
        .next()
        .filter(|line| !line.is_empty())
        .ok_or_else(|| "empty response from ccbd".to_string())?;
    let response: serde_json::Value =
        serde_json::from_slice(first_line).map_err(|err| format!("decode response: {err}"))?;
    if response.get("ok").and_then(|value| value.as_bool()) != Some(true) {
        let error = response
            .get("error")
            .and_then(|value| value.as_str())
            .unwrap_or("ccbd request failed");
        return Err(error.to_string());
    }
    let mut payload = response
        .as_object()
        .cloned()
        .ok_or_else(|| "ccbd response is not an object".to_string())?;
    payload.remove("api_version");
    payload.remove("ok");
    Ok(serde_json::Value::Object(payload))
}

#[cfg(not(unix))]
fn request_unix_socket(
    _socket_path: &Path,
    _timeout: Duration,
    _op: &str,
    _request: serde_json::Value,
) -> Result<serde_json::Value, String> {
    Err("Unix sockets are not supported on this platform".to_string())
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader};
    use std::os::unix::net::UnixListener;
    use std::thread;

    #[test]
    fn project_view_round_trips_over_ccbd_socket_protocol() {
        let (socket_path, handle) = spawn_one_response_server(|request| {
            assert_eq!(request["api_version"], 2);
            assert_eq!(request["op"], "project_view");
            assert_eq!(request["request"]["schema_version"], 1);
            json!({
                "api_version": 2,
                "ok": true,
                "view": {
                    "project": {"display_name": "repo"},
                    "namespace": {"epoch": 4, "active_window": "main"},
                    "windows": [{"name": "main", "active": true, "agents": ["agent1"]}],
                    "agents": [{"name": "agent1", "provider": "codex", "window": "main", "activity_state": "idle"}],
                    "comms": []
                },
                "cache": {"sequence": 9, "ttl_ms": 1000}
            })
        });
        let client = CcbdClient::new(socket_path);

        let response = client.project_view().unwrap();
        handle.join().unwrap();

        assert_eq!(response.cache.sequence, 9);
        assert_eq!(response.view.windows[0].name, "main");
        assert_eq!(response.view.agents[0].name, "agent1");
    }

    #[test]
    fn focus_window_sends_namespace_epoch() {
        let (socket_path, handle) = spawn_one_response_server(|request| {
            assert_eq!(request["op"], "project_focus_window");
            assert_eq!(request["request"]["window"], "ops");
            assert_eq!(request["request"]["namespace_epoch"], 7);
            json!({"api_version": 2, "ok": true, "focus": {"kind": "window"}})
        });
        let client = CcbdClient::new(socket_path);

        client.focus_window("ops", Some(7)).unwrap();
        handle.join().unwrap();
    }

    #[test]
    fn focus_agent_sends_namespace_epoch() {
        let (socket_path, handle) = spawn_one_response_server(|request| {
            assert_eq!(request["op"], "project_focus_agent");
            assert_eq!(request["request"]["agent"], "agent1");
            assert_eq!(request["request"]["namespace_epoch"], 8);
            json!({"api_version": 2, "ok": true, "focus": {"kind": "agent"}})
        });
        let client = CcbdClient::new(socket_path);

        client.focus_agent("agent1", Some(8)).unwrap();
        handle.join().unwrap();
    }

    #[test]
    fn comms_recover_sends_job_and_reply_delivery_target() {
        let (socket_path, handle) = spawn_one_response_server(|request| {
            assert_eq!(request["op"], "comms_recover");
            assert_eq!(request["request"]["job_id"], "job1");
            assert_eq!(request["request"]["reply_delivery_job_id"], "job2");
            assert!(request["request"].get("block_reason").is_none());
            json!({"api_version": 2, "ok": true, "status": "recovered"})
        });
        let client = CcbdClient::new(socket_path);

        client.comms_recover("job1", Some("job2"), None).unwrap();
        handle.join().unwrap();
    }

    #[test]
    fn comms_recover_sends_running_block_reason_hint() {
        let (socket_path, handle) = spawn_one_response_server(|request| {
            assert_eq!(request["op"], "comms_recover");
            assert_eq!(request["request"]["job_id"], "job1");
            assert_eq!(request["request"]["block_reason"], "provider_prompt_idle");
            json!({"api_version": 2, "ok": true, "status": "recovered"})
        });
        let client = CcbdClient::new(socket_path);

        client
            .comms_recover("job1", None, Some("provider_prompt_idle"))
            .unwrap();
        handle.join().unwrap();
    }

    #[test]
    fn cancel_sends_job_id() {
        let (socket_path, handle) = spawn_one_response_server(|request| {
            assert_eq!(request["op"], "cancel");
            assert_eq!(request["request"]["job_id"], "job1");
            json!({"api_version": 2, "ok": true, "status": "cancelled"})
        });
        let client = CcbdClient::new(socket_path);

        client.cancel("job1").unwrap();
        handle.join().unwrap();
    }

    #[test]
    fn dismiss_comms_sends_project_view_dismiss_request() {
        let (socket_path, handle) = spawn_one_response_server(|request| {
            assert_eq!(request["op"], "project_view_dismiss_comms");
            assert_eq!(request["request"]["id"], "job1");
            json!({"api_version": 2, "ok": true, "status": "dismissed"})
        });
        let client = CcbdClient::new(socket_path);

        client.dismiss_comms("job1").unwrap();
        handle.join().unwrap();
    }

    #[test]
    fn restart_panes_sends_project_restart_request() {
        let (socket_path, handle) = spawn_one_response_server(|request| {
            assert_eq!(request["op"], "project_restart_panes");
            assert_eq!(request["request"], json!({}));
            json!({"api_version": 2, "ok": true, "status": "scheduled"})
        });
        let client = CcbdClient::new(socket_path);

        client.restart_panes().unwrap();
        handle.join().unwrap();
    }

    #[test]
    fn reload_config_sends_non_dry_run_project_reload_request() {
        let (socket_path, handle) = spawn_one_response_server(|request| {
            assert_eq!(request["op"], "project_reload_config");
            assert_eq!(request["request"], json!({"dry_run": false}));
            json!({"api_version": 2, "ok": true, "status": "published"})
        });
        let client = CcbdClient::new(socket_path);

        client.reload_config().unwrap();
        handle.join().unwrap();
    }

    #[test]
    fn reload_config_reports_structured_rejection() {
        let (socket_path, handle) = spawn_one_response_server(|request| {
            assert_eq!(request["op"], "project_reload_config");
            json!({
                "api_version": 2,
                "ok": true,
                "status": "blocked",
                "diagnostics": {"reason": "unsupported_plan_class"}
            })
        });
        let client = CcbdClient::new(socket_path);

        let error = client.reload_config().unwrap_err();
        handle.join().unwrap();

        assert_eq!(error, "reload blocked: unsupported_plan_class");
    }

    #[test]
    fn reload_config_treats_no_change_as_refresh_success() {
        let (socket_path, handle) = spawn_one_response_server(|request| {
            assert_eq!(request["op"], "project_reload_config");
            json!({
                "api_version": 2,
                "ok": true,
                "status": "blocked",
                "plan_class": "no_change",
                "diagnostics": {"reason": "unsupported_plan_class"}
            })
        });
        let client = CcbdClient::new(socket_path);

        client.reload_config().unwrap();
        handle.join().unwrap();
    }

    fn spawn_one_response_server<F>(handler: F) -> (std::path::PathBuf, thread::JoinHandle<()>)
    where
        F: FnOnce(serde_json::Value) -> serde_json::Value + Send + 'static,
    {
        let dir = std::env::temp_dir().join(format!(
            "ccb-agent-sidebar-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let socket_path = dir.join("ccbd.sock");
        let listener = UnixListener::bind(&socket_path).unwrap();
        let path_for_thread = socket_path.clone();
        let handle = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut line = String::new();
            {
                let mut reader = BufReader::new(&stream);
                reader.read_line(&mut line).unwrap();
            }
            let request: serde_json::Value = serde_json::from_str(line.trim()).unwrap();
            let response = handler(request);
            stream
                .write_all(format!("{response}\n").as_bytes())
                .unwrap();
            drop(stream);
            let _ = std::fs::remove_file(path_for_thread);
            let _ = std::fs::remove_dir(dir);
        });
        (socket_path, handle)
    }
}
