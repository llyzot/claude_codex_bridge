//! Session-scoped Herdr CLI transport. Never starts a server or replays a call.
//! A successful CLI acknowledgement is not an Agent-turn completion signal.
use anyhow::{ensure, Context, Result};
use serde_json::{json, Value};
use std::{
    io::Read,
    process::{Command, Stdio},
    sync::mpsc,
    time::{Duration, Instant},
};

const LIMIT: usize = 8 * 1024 * 1024;
fn identifier(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"_-.:".contains(&b))
}
fn classify(args: &[String]) -> Result<bool> {
    let first = args.first().map(String::as_str).unwrap_or("");
    let second = args.get(1).map(String::as_str).unwrap_or("");
    let read_only = match (first, second) {
        ("--version", "") => true,
        ("status", "--json" | "server") => true,
        ("api", "snapshot" | "schema") => true,
        ("workspace", "list") | ("tab", "list") => true,
        ("pane", "list" | "read" | "get" | "layout" | "process-info") => true,
        ("agent", "list" | "get" | "read") => true,
        ("workspace", "create" | "close" | "focus" | "report-metadata") => false,
        ("tab", "rename" | "create" | "close") => false,
        ("pane", "split" | "run" | "close" | "move" | "resize" | "report-agent" | "report-agent-session" | "release-agent" | "report-metadata") => false,
        ("agent", "start" | "stop" | "prompt" | "send-keys") => false,
        _ => anyhow::bail!("unsupported Herdr transport command; server/session lifecycle and interactive attachment are separate"),
    };
    let control_args = if first == "agent" && second == "start" {
        &args[..args
            .iter()
            .position(|arg| arg == "--")
            .unwrap_or(args.len())]
    } else {
        args
    };
    for token in control_args {
        ensure!(
            !["--session", "--machine", "--remote", "--current"]
                .iter()
                .any(|flag| token == flag || token.starts_with(&format!("{flag}="))),
            "Herdr transport requires one explicit session and explicit targets"
        );
        ensure!(
            token != "--wait",
            "Herdr transport does not use lifecycle waits as task completion evidence"
        );
    }
    let positional_target = matches!(first, "pane" | "agent" | "tab" | "workspace")
        && !matches!(second, "list" | "create" | "process-info");
    if positional_target {
        ensure!(
            args.get(2)
                .is_some_and(|s| identifier(s) && !s.starts_with('-')),
            "Herdr command requires an explicit target"
        );
    }
    if first == "pane" && second == "process-info" {
        ensure!(
            args.get(2).is_some_and(|s| s == "--pane")
                && args
                    .get(3)
                    .is_some_and(|s| identifier(s) && !s.starts_with('-')),
            "Herdr process-info requires an explicit pane"
        );
    }
    Ok(read_only)
}
fn read_stream(
    mut input: impl Read + Send + 'static,
    sender: mpsc::Sender<(bool, Result<Vec<u8>, &'static str>)>,
    stderr: bool,
) {
    std::thread::spawn(move || {
        let mut bytes = Vec::new();
        let result = match input
            .by_ref()
            .take((LIMIT + 1) as u64)
            .read_to_end(&mut bytes)
        {
            Ok(_) if bytes.len() <= LIMIT => Ok(bytes),
            Ok(_) => Err("output_limit"),
            Err(_) => Err("output_read_failed"),
        };
        let _ = sender.send((stderr, result));
    });
}
fn failed_envelope(value: &Value) -> bool {
    if !value.is_object() {
        return true;
    }
    let failed = |v: &Value| {
        v["ok"] == false
            || v["success"] == false
            || !v["error"].is_null() && v["error"] != false && v["error"] != ""
            || !v["status"].is_null()
                && !matches!(v["status"].as_str().map(str::trim), Some("" | "ok"))
            || !v["command_status"].is_null()
                && !matches!(
                    v["command_status"].as_str().map(str::trim),
                    Some("" | "ok" | "success")
                )
    };
    failed(value) || value["result"].is_object() && failed(&value["result"])
}
pub fn execute(request: &Value) -> Result<Value> {
    ensure!(
        request["schema_version"] == 1,
        "invalid Herdr transport request schema"
    );
    let session = request["session"]
        .as_str()
        .context("Herdr session is required")?;
    ensure!(
        !session.is_empty()
            && session.len() <= 512
            && session.trim() == session
            && !session.starts_with('-')
            && !session.chars().any(char::is_control),
        "invalid explicit Herdr session"
    );
    let operation = request["operation"]
        .as_str()
        .context("Herdr operation is required")?;
    ensure!(identifier(operation), "invalid Herdr operation name");
    let command: Vec<String> = serde_json::from_value(request["command"].clone())
        .map_err(|_| anyhow::anyhow!("invalid Herdr executable argv schema"))?;
    ensure!(
        !command.is_empty() && !command[0].is_empty() && command.len() <= 16,
        "invalid Herdr executable argv"
    );
    let args: Vec<String> = serde_json::from_value(request["args"].clone())
        .map_err(|_| anyhow::anyhow!("invalid Herdr argument schema"))?;
    ensure!(
        !args.is_empty() && args.len() <= 256 && args.iter().all(|s| !s.contains('\0')),
        "invalid Herdr arguments"
    );
    let read_only = classify(&args)?;
    let timeout = if request.get("timeout_ms").is_none() {
        10000
    } else {
        request["timeout_ms"]
            .as_u64()
            .context("invalid Herdr timeout")?
    };
    ensure!(
        (50..=60000).contains(&timeout),
        "Herdr timeout must be between 50 and 60000 milliseconds"
    );
    ensure!(
        request["expect_json"].is_boolean(),
        "Herdr expect_json must be boolean"
    );
    let mut process = Command::new(&command[0]);
    process
        .args(&command[1..])
        .args(["--session", session])
        .args(&args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    configure_env(&mut process);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        process.creation_flags(0x08000000);
    }
    let started = Instant::now();
    let base = json!({"schema_version":1,"operation":operation,"session":session,"read_only":read_only,
        "task_completion":false,"automatic_replay":false});
    let mut child = match crate::owned_process::OwnedChild::capture(&mut process) {
        Ok(child) => child,
        Err(_) => return Ok(reply(base, false, "spawn_failed", None, None, started)),
    };
    drop(child.stdin.take());
    let (sender, output) = mpsc::channel();
    read_stream(
        child.stdout.take().context("missing Herdr stdout")?,
        sender.clone(),
        false,
    );
    read_stream(
        child.stderr.take().context("missing Herdr stderr")?,
        sender,
        true,
    );
    let (mut stdout, mut stderr, mut exit) = (None, None, None);
    let mut failure = None;
    loop {
        while let Ok((is_stderr, result)) = output.try_recv() {
            match result {
                Ok(bytes) => {
                    if is_stderr {
                        stderr = Some(bytes)
                    } else {
                        stdout = Some(bytes)
                    };
                }
                Err(reason) => failure = Some(reason),
            }
        }
        if failure.is_some() {
            break;
        }
        if exit.is_none() {
            exit = child.try_wait()?;
        }
        if exit.is_some() && stdout.is_some() && stderr.is_some() && child.tree_empty()? {
            break;
        }
        if started.elapsed() >= Duration::from_millis(timeout) {
            failure = Some("observation_timeout");
            break;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    let code = exit.and_then(|s| s.code());
    if let Some(reason) = failure {
        // Only our CLI subprocess tree is controlled; never the Herdr server.
        let _ = child.kill();
        return Ok(reply(base, false, reason, code, None, started));
    }
    if !exit.is_some_and(|s| s.success()) {
        return Ok(reply(base, false, "command_failed", code, None, started));
    }
    let stdout = match String::from_utf8(stdout.unwrap()) {
        Ok(text) => text,
        Err(_) => return Ok(reply(base, false, "invalid_utf8", code, None, started)),
    };
    if request["expect_json"] == true {
        match serde_json::from_str::<Value>(&stdout) {
            Ok(value) if !failed_envelope(&value) => {}
            Ok(_) => return Ok(reply(base, false, "server_rejected", code, None, started)),
            Err(_) => return Ok(reply(base, false, "invalid_json", code, None, started)),
        }
    } else if stdout.trim_start().starts_with('{') {
        // A provider rejection must not become success merely because the
        // legacy caller did not require JSON for that particular mutation.
        if serde_json::from_str::<Value>(&stdout).is_ok_and(|v| failed_envelope(&v)) {
            return Ok(reply(base, false, "server_rejected", code, None, started));
        }
    }
    Ok(reply(base, true, "", code, Some(stdout), started))
}
pub(crate) fn configure_env(process: &mut Command) {
    // Explicit session selection must not inherit another pane or remote socket.
    for (key, _) in std::env::vars_os() {
        let name = key.to_string_lossy().to_ascii_uppercase();
        if name.starts_with("HERDR_") && name != "HERDR_CONFIG_PATH" {
            process.env_remove(key);
        }
    }
    #[cfg(windows)]
    {
        for key in ["XDG_CONFIG_HOME", "XDG_STATE_HOME", "XDG_CACHE_HOME"] {
            process.env_remove(key);
        }
        if std::env::var_os("HERDR_CONFIG_PATH").is_none() {
            if let Some(home) = std::env::var_os("USERPROFILE") {
                process.env(
                    "HERDR_CONFIG_PATH",
                    std::path::PathBuf::from(home).join("AppData/Roaming/herdr/config.toml"),
                );
            }
        }
    }
}
fn reply(
    mut base: Value,
    ok: bool,
    reason: &str,
    code: Option<i32>,
    stdout: Option<String>,
    started: Instant,
) -> Value {
    base["ok"] = json!(ok);
    base["outcome"] = json!(if ok {
        "acknowledged"
    } else if base["read_only"] == true {
        "unobserved"
    } else {
        "unknown"
    });
    base["failure"] = if ok { Value::Null } else { json!(reason) };
    base["exit_code"] = json!(code);
    base["elapsed_ms"] = json!(started.elapsed().as_millis());
    // Error stdout/stderr can contain prompt text, credentials, or terminal
    // controls. They are deliberately absent from failures and audit evidence.
    base["stdout"] = json!(stdout);
    base
}
