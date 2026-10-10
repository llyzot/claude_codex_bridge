//! Persistent Pi RPC: acceptance/agent_end are not completion authority.
use anyhow::{ensure, Context, Result};
use serde_json::{json, Value};
use std::{
    io::{BufRead, BufReader, Read, Write},
    process::{Child, Command, Stdio},
    sync::mpsc,
    time::Instant,
};
struct Turn {
    job: Value,
    id: String,
    accepted: bool,
    anchor: bool,
    assistant: Value,
    settled: bool,
    check: String,
    decision: Option<Value>,
    session_id: String,
    cancel: Option<bool>,
    started: Instant,
    deadline_triggered: bool,
}
pub struct Session {
    child: Child,
    commands: mpsc::SyncSender<Value>,
    events: mpsc::Receiver<Value>,
    init: String,
    pub runtime: String,
    pub state: Value,
    pub ready: bool,
    active: Option<Turn>,
    pub dirty: bool,
    expected_session: String,
    timeout_s: f64,
}
impl Drop for Session {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
impl Session {
    pub fn spawn(
        agent: &str,
        spec: &Value,
        endpoint: &Value,
        token: &std::path::Path,
        saved: &Value,
    ) -> Result<Self> {
        let argv = spec["command"]
            .as_array()
            .context("Pi RPC requires command argv")?;
        ensure!(
            !argv.is_empty() && argv.iter().all(Value::is_string),
            "invalid Pi RPC argv"
        );
        let mut command = Command::new(argv[0].as_str().unwrap());
        for arg in &argv[1..] {
            command.arg(arg.as_str().unwrap());
        }
        let expected_session = if spec["resume_session"] != false {
            saved["state"]["sessionId"]
                .as_str()
                .unwrap_or("")
                .to_owned()
        } else {
            String::new()
        };
        if !expected_session.is_empty() {
            let file = saved["state"]["sessionFile"]
                .as_str()
                .filter(|s| !s.is_empty())
                .context("Pi exact session resume requires a saved session file")?;
            ensure!(
                std::path::Path::new(file).is_file(),
                "Pi saved session file is missing"
            );
            command.arg("--session").arg(file);
        }
        if let Some(cwd) = spec["workspace_path"].as_str() {
            command.current_dir(cwd);
        }
        if let Some(env) = spec["env"].as_object() {
            for (key, value) in env {
                command.env(
                    key,
                    value.as_str().context("Pi env values must be strings")?,
                );
            }
        }
        command
            .env("CCB_AGENT_NAME", agent)
            .env("CCB_NATIVE_ENDPOINT", endpoint.to_string())
            .env("CCB_NATIVE_TOKEN_FILE", token);
        command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            command.creation_flags(0x08000000);
        }
        let mut child = command.spawn()?;
        let mut stdin = child.stdin.take().context("missing Pi stdin")?;
        let stdout = child.stdout.take().context("missing Pi stdout")?;
        let (commands, input) = mpsc::sync_channel::<Value>(16);
        std::thread::spawn(move || {
            for value in input {
                let Ok(mut bytes) = serde_json::to_vec(&value) else {
                    break;
                };
                bytes.push(b'\n');
                if stdin.write_all(&bytes).and_then(|_| stdin.flush()).is_err() {
                    break;
                }
            }
        });
        let (sender, events) = mpsc::sync_channel(64);
        std::thread::spawn(move || {
            let mut reader = BufReader::new(stdout);
            loop {
                let mut line = Vec::new();
                match (&mut reader)
                    .take(8 * 1024 * 1024 + 1)
                    .read_until(b'\n', &mut line)
                {
                    Ok(0) => break,
                    Ok(_) if line.len() <= 8 * 1024 * 1024 => {
                        if let Ok(record) = serde_json::from_slice::<Value>(&line) {
                            if sender.send(record).is_err() {
                                break;
                            }
                        }
                    }
                    _ => break,
                }
            }
        });
        let runtime = uuid::Uuid::new_v4().to_string();
        let init = format!("init-{runtime}");
        commands
            .try_send(json!({"id":init,"type":"get_state"}))
            .context("Pi initialization queue unavailable")?;
        Ok(Self {
            child,
            commands,
            events,
            init,
            runtime,
            state: Value::Null,
            ready: false,
            active: None,
            dirty: false,
            expected_session,
            timeout_s: spec["timeout_s"].as_f64().unwrap_or(3600.0),
        })
    }
    pub fn start(&mut self, job: &Value) -> Result<()> {
        ensure!(
            self.ready && self.active.is_none(),
            "Pi session is not idle"
        );
        let id = format!(
            "prompt-{}",
            job["provider_options"]["native_execution_instance"]
                .as_str()
                .context("missing Pi execution instance")?
        );
        let message = format!(
            "CCB_REQ_ID: {}\n{}",
            job["job_id"].as_str().context("missing Pi job id")?,
            job["request"]["body"]
                .as_str()
                .context("missing Pi prompt")?
        );
        self.commands
            .try_send(json!({"id":id,"type":"prompt","message":message}))
            .context("Pi prompt queue unavailable")?;
        self.active = Some(Turn {
            job: job.clone(),
            id,
            accepted: false,
            anchor: false,
            assistant: Value::Null,
            settled: false,
            check: String::new(),
            decision: None,
            session_id: self.state["sessionId"]
                .as_str()
                .context("Pi session identity unavailable")?
                .to_owned(),
            cancel: None,
            started: Instant::now(),
            deadline_triggered: false,
        });
        self.ready = false;
        Ok(())
    }
    pub fn poll(&mut self) -> Result<Option<(Value, Value)>> {
        if self.timeout_s > 0.0
            && self.active.as_ref().is_some_and(|turn| {
                turn.cancel.is_none() && turn.started.elapsed().as_secs_f64() > self.timeout_s
            })
        {
            let id = self.active.as_ref().unwrap().job["job_id"]
                .as_str()
                .unwrap()
                .to_owned();
            self.abort(&id, true)?;
            self.active.as_mut().unwrap().deadline_triggered = true;
        }
        for _ in 0..128 {
            let Ok(record) = self.events.try_recv() else {
                break;
            };
            self.ingest(&record)?;
        }
        if let Some(exit) = self.child.try_wait()? {
            self.ready = false;
            if let Some(turn) = self.active.take() {
                return Ok(Some((
                    turn.job,
                    failure(
                        "incomplete",
                        "pi_process_exit",
                        &format!("Pi exited before settled completion: {exit}"),
                    ),
                )));
            }
            anyhow::bail!("Pi RPC session exited");
        }
        if self
            .active
            .as_ref()
            .is_some_and(|turn| turn.decision.is_some())
        {
            let turn = self.active.take().unwrap();
            self.ready = true;
            return Ok(Some((turn.job, turn.decision.unwrap())));
        }
        Ok(None)
    }
    pub fn abort(&mut self, job_id: &str, record_reply: bool) -> Result<()> {
        let turn = self.active.as_mut().context("Pi has no active task")?;
        ensure!(
            turn.job["job_id"] == job_id,
            "Pi cancel task identity mismatch"
        );
        self.commands
            .try_send(json!({"id":format!("abort-{}",turn.id),"type":"abort"}))
            .context("Pi abort queue unavailable")?;
        turn.cancel = Some(record_reply);
        Ok(())
    }
    fn ingest(&mut self, record: &Value) -> Result<()> {
        if record["type"] == "response" && record["id"] == self.init {
            ensure!(
                record["command"] == "get_state" && record["success"] == true,
                "Pi get_state initialization failed"
            );
            self.state = state_view(&record["data"]);
            ensure!(
                !self.state["sessionId"].as_str().unwrap_or("").is_empty(),
                "Pi initialization session identity missing"
            );
            ensure!(
                self.expected_session.is_empty()
                    || self.state["sessionId"] == self.expected_session,
                "Pi resumed wrong session"
            );
            self.ready = idle(&self.state);
            self.dirty = true;
            return Ok(());
        }
        let Some(turn) = self.active.as_mut() else {
            if record["type"] == "agent_settled" {
                self.commands
                    .try_send(json!({"id":self.init,"type":"get_state"}))
                    .context("Pi idle-state query unavailable")?;
            }
            return Ok(());
        };
        if record["type"] == "response" && record["id"] == turn.id {
            ensure!(
                record["command"] == "prompt",
                "Pi prompt response command mismatch"
            );
            if record["success"] != true {
                turn.decision = Some(failure(
                    "failed",
                    "pi_prompt_rejected",
                    record["error"].as_str().unwrap_or("Pi rejected prompt"),
                ));
            } else {
                let disposition = record["data"]["disposition"].as_str().unwrap_or("");
                if disposition == "handled" {
                    turn.decision = Some(failure(
                        "incomplete",
                        "pi_prompt_handled",
                        "Pi handled the input without starting a run",
                    ));
                } else {
                    ensure!(
                        ["started", "queued"].contains(&disposition),
                        "unknown Pi prompt disposition"
                    );
                    turn.accepted = true;
                    if turn.settled && !turn.check.is_empty() {
                        self.commands
                            .try_send(json!({"id":turn.check,"type":"get_state"}))
                            .context("Pi accepted-state query unavailable")?;
                    }
                }
            }
            return Ok(());
        }
        if record["type"] == "message_start" || record["type"] == "message_end" {
            let message = &record["message"];
            if message["role"] == "user" {
                let text = content(message);
                let expected = format!("CCB_REQ_ID: {}", turn.job["job_id"].as_str().unwrap_or(""));
                if text
                    .lines()
                    .next()
                    .is_some_and(|line| line.trim() == expected)
                {
                    turn.anchor = true;
                }
            }
            if record["type"] == "message_end" && message["role"] == "assistant" && turn.anchor {
                turn.assistant = message.clone();
            }
        }
        if record["type"] == "agent_settled" && turn.anchor {
            turn.settled = true;
            turn.check = format!("settled-{}", turn.id);
            self.commands
                .try_send(json!({"id":turn.check,"type":"get_state"}))
                .context("Pi settled-state query unavailable")?;
        }
        if record["type"] == "response" && !turn.check.is_empty() && record["id"] == turn.check {
            ensure!(
                record["command"] == "get_state" && record["success"] == true,
                "Pi settled-state query failed"
            );
            self.state = state_view(&record["data"]);
            self.dirty = true;
            ensure!(
                self.state["sessionId"] == turn.session_id,
                "Pi completion session identity changed"
            );
            if turn.accepted && turn.settled && idle(&self.state) {
                let text = content(&turn.assistant);
                let stop = turn.assistant["stopReason"]
                    .as_str()
                    .unwrap_or("")
                    .to_lowercase();
                let (status, reason) = if stop == "aborted" {
                    ("cancelled", "pi_aborted")
                } else if stop == "error" {
                    ("failed", "api_error")
                } else if text.trim().is_empty() || ["tooluse", "tool_use"].contains(&stop.as_str())
                {
                    ("incomplete", "pi_empty_reply")
                } else {
                    ("completed", "pi_agent_settled")
                };
                let mut decision = failure(status, reason, &text);
                decision["confidence"] = json!("exact");
                decision["anchor_seen"] = json!(true);
                decision["reply_started"] = json!(!text.is_empty());
                decision["reply_stable"] = json!(true);
                decision["provider_turn_ref"] = self.state["sessionId"].clone();
                decision["diagnostics"] = json!({"completion_authority":"pi_rpc_agent_settled","runtime_instance_id":self.runtime,"prompt_id":turn.id,"session_id":self.state["sessionId"],"session_file":self.state["sessionFile"]});
                if stop == "error" {
                    decision["diagnostics"]["error_message"] =
                        turn.assistant["errorMessage"].clone();
                }
                if let Some(record_reply) = turn.cancel {
                    decision["diagnostics"]["result_reply_enabled"] = json!(record_reply);
                    decision["diagnostics"]["cancel_requested"] = json!(true);
                }
                if turn.deadline_triggered {
                    decision["diagnostics"]["deadline_triggered"] = json!(true);
                }
                turn.decision = Some(decision);
            }
        }
        Ok(())
    }
    pub fn pid(&self) -> u32 {
        self.child.id()
    }
}
fn idle(state: &Value) -> bool {
    state["isStreaming"] == false
        && state["isCompacting"] == false
        && state["pendingMessageCount"].as_u64() == Some(0)
}
fn state_view(state: &Value) -> Value {
    json!({"sessionId":state["sessionId"],"sessionFile":state["sessionFile"],"isStreaming":state["isStreaming"],"isCompacting":state["isCompacting"],"pendingMessageCount":state["pendingMessageCount"],"messageCount":state["messageCount"]})
}
fn content(message: &Value) -> String {
    if let Some(text) = message["content"].as_str() {
        return text.into();
    }
    message["content"]
        .as_array()
        .map(|parts| {
            parts
                .iter()
                .filter(|p| p["type"] == "text")
                .filter_map(|p| p["text"].as_str())
                .collect::<Vec<_>>()
                .join("\n")
        })
        .unwrap_or_default()
}
fn failure(status: &str, reason: &str, text: &str) -> Value {
    json!({"schema_version":2,"record_type":"completion_decision","terminal":true,"status":status,"reason":reason,"confidence":"degraded","reply":text,"anchor_seen":false,"reply_started":false,"reply_stable":false,"provider_turn_ref":null,"source_cursor":null,"finished_at":crate::daemon::now(),"diagnostics":{}})
}
