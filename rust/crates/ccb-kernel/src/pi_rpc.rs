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
    abort_sent: bool,
    started: Instant,
    deadline_triggered: bool,
}
enum StreamEvent {
    Record(Value),
    End,
}
pub struct Session {
    child: Child,
    commands: mpsc::SyncSender<Value>,
    events: mpsc::Receiver<StreamEvent>,
    init: String,
    pub runtime: String,
    pub state: Value,
    pub ready: bool,
    pub closed: bool,
    stdout_closed: bool,
    input_closed: bool,
    saved_state: Value,
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
                            if sender.send(StreamEvent::Record(record)).is_err() {
                                break;
                            }
                        }
                    }
                    _ => break,
                }
            }
            let _ = sender.send(StreamEvent::End);
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
            closed: false,
            stdout_closed: false,
            input_closed: false,
            saved_state: saved["state"].clone(),
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
        self.input_closed |= !enqueue(
            &self.commands,
            json!({"id":id,"type":"prompt","message":message}),
        )?;
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
            abort_sent: false,
            started: Instant::now(),
            deadline_triggered: false,
        });
        self.ready = false;
        Ok(())
    }
    pub fn poll(&mut self) -> Result<Option<(Value, Value)>> {
        if self.closed && self.active.is_none() {
            return Ok(None);
        }
        for _ in 0..128 {
            let Ok(record) = self.events.try_recv() else {
                break;
            };
            match record {
                StreamEvent::Record(record) => self.ingest(&record)?,
                StreamEvent::End => self.stdout_closed = true,
            }
        }
        let exit = self.child.try_wait()?;
        if exit.is_some() || self.stdout_closed || self.input_closed {
            self.ready = false;
        }
        if exit.is_some() && self.state["process_state"] != "exited" {
            if self.state.is_null() {
                self.state = self.saved_state.clone();
            }
            self.state["process_state"] = json!("exited");
            self.dirty = true;
        }
        if self
            .active
            .as_ref()
            .is_some_and(|turn| turn.decision.is_some())
        {
            let turn = self.active.take().unwrap();
            self.closed = exit.is_some() && self.stdout_closed;
            self.ready = exit.is_none() && !self.stdout_closed && !self.input_closed;
            let mut decision = turn.decision.clone().unwrap();
            control_diagnostics(&turn, &mut decision);
            return Ok(Some((turn.job, decision)));
        }
        // Process exit alone does not prove that the bounded reader has drained
        // the last result or that inherited stdout handles have reached EOF.
        if let Some(exit) = exit.filter(|_| self.stdout_closed) {
            self.closed = true;
            if let Some(turn) = self.active.take() {
                let mut decision = failure(
                    "incomplete",
                    "pi_process_exit",
                    &format!("Pi exited before settled completion: {exit}"),
                );
                control_diagnostics(&turn, &mut decision);
                return Ok(Some((turn.job, decision)));
            }
            return Ok(None);
        }
        if exit.is_none() && !self.stdout_closed && !self.input_closed {
            if let Some(turn) = self.active.as_mut() {
                if turn.cancel.is_some() && !turn.abort_sent {
                    turn.abort_sent = enqueue(
                        &self.commands,
                        json!({"id":format!("abort-{}",turn.id),"type":"abort"}),
                    )?;
                    self.input_closed |= !turn.abort_sent;
                }
            }
        }
        Ok(None)
    }
    pub fn active_job_id(&self) -> Option<&str> {
        self.active
            .as_ref()
            .and_then(|turn| turn.job["job_id"].as_str())
    }
    pub fn deadline_due(&self) -> bool {
        self.active.as_ref().is_some_and(|turn| {
            self.timeout_s.is_finite()
                && self.timeout_s > 0.0
                && turn.started.elapsed().as_secs_f64() > self.timeout_s
                && turn.cancel.is_none()
                && turn.decision.is_none()
        })
    }
    pub fn validate_interrupt(&self, job_id: &str, record_reply: bool) -> Result<()> {
        let turn = self.active.as_ref().context("Pi has no active task")?;
        ensure!(
            turn.job["job_id"] == job_id,
            "Pi cancel task identity mismatch"
        );
        ensure!(
            turn.cancel.is_none_or(|prior| prior == record_reply),
            "Pi cancellation reply policy already fixed"
        );
        Ok(())
    }
    pub fn sync_job(&mut self, job: &Value) -> Result<()> {
        let turn = self.active.as_mut().context("Pi has no active task")?;
        ensure!(
            turn.job["job_id"] == job["job_id"]
                && turn.job["agent_name"] == job["agent_name"]
                && turn.job["provider"] == job["provider"]
                && turn.job["request"] == job["request"]
                && turn.job["provider_options"]["native_execution_instance"]
                    == job["provider_options"]["native_execution_instance"]
                && turn.job["provider_options"]["native_daemon_instance"]
                    == job["provider_options"]["native_daemon_instance"],
            "Pi active execution identity changed"
        );
        turn.job = job.clone();
        turn.deadline_triggered =
            job["provider_options"]["native_cancel_intent"]["deadline_triggered"] == true;
        if let Some(record_reply) =
            job["provider_options"]["native_cancel_intent"]["record_reply"].as_bool()
        {
            ensure!(
                turn.cancel.is_none_or(|prior| prior == record_reply),
                "Pi cancellation reply policy already fixed"
            );
            turn.cancel = Some(record_reply);
        }
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
                self.input_closed |=
                    !enqueue(&self.commands, json!({"id":self.init,"type":"get_state"}))?;
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
                        self.input_closed |=
                            !enqueue(&self.commands, json!({"id":turn.check,"type":"get_state"}))?;
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
            self.input_closed |=
                !enqueue(&self.commands, json!({"id":turn.check,"type":"get_state"}))?;
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
                turn.decision = Some(decision);
            }
        }
        Ok(())
    }
    pub fn pid(&self) -> u32 {
        self.child.id()
    }
}
fn enqueue(commands: &mpsc::SyncSender<Value>, value: Value) -> Result<bool> {
    match commands.try_send(value) {
        Ok(()) => Ok(true),
        // A dead writer is transport evidence, never task completion evidence.
        Err(mpsc::TrySendError::Disconnected(_)) => Ok(false),
        Err(error) => Err(error).context("Pi command queue unavailable"),
    }
}
fn control_diagnostics(turn: &Turn, decision: &mut Value) {
    if let Some(record_reply) = turn.cancel {
        decision["diagnostics"]["result_reply_enabled"] = json!(record_reply);
        decision["diagnostics"]["cancel_requested"] = json!(true);
    }
    if turn.deadline_triggered {
        decision["diagnostics"]["deadline_triggered"] = json!(true);
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
