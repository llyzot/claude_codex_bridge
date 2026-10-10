//! Owned Codex app-server stdio session with thread/turn identity fences.
use anyhow::{ensure, Context, Result};
use serde_json::{json, Value};
use std::{
    io::{BufRead, BufReader, Read, Write},
    process::{Child, Command, Stdio},
    sync::mpsc,
    time::Instant,
};
struct Active {
    job: Value,
    start: String,
    turn: String,
    buffer: Vec<Value>,
    final_text: String,
    legacy_text: String,
    completed: Value,
    check: String,
    decision: Option<Value>,
    cancel: Option<bool>,
    interrupt: String,
    interrupt_acknowledged: bool,
    interrupt_error: Value,
    started: Instant,
    deadline_triggered: bool,
}
pub struct Session {
    child: Child,
    commands: mpsc::SyncSender<Value>,
    events: mpsc::Receiver<Value>,
    init: String,
    bind: String,
    spec: Value,
    expected: String,
    pub runtime: String,
    pub state: Value,
    pub ready: bool,
    pub dirty: bool,
    active: Option<Active>,
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
            .context("Codex requires app-server argv")?;
        ensure!(
            !argv.is_empty() && argv.iter().all(Value::is_string),
            "invalid Codex argv"
        );
        let mut command = Command::new(argv[0].as_str().unwrap());
        for arg in &argv[1..] {
            command.arg(arg.as_str().unwrap());
        }
        if let Some(cwd) = spec["workspace_path"].as_str() {
            command.current_dir(cwd);
        }
        if let Some(env) = spec["env"].as_object() {
            for (key, value) in env {
                command.env(
                    key,
                    value.as_str().context("Codex env values must be strings")?,
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
        let mut stdin = child.stdin.take().context("missing Codex stdin")?;
        let stdout = child.stdout.take().context("missing Codex stdout")?;
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
                        if let Ok(value) = serde_json::from_slice::<Value>(&line) {
                            if sender.send(value).is_err() {
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
        let bind = format!("thread-{runtime}");
        commands.try_send(json!({"id":init,"method":"initialize","params":{"clientInfo":{"name":"ccb-native","version":env!("CARGO_PKG_VERSION")},"capabilities":{"experimentalApi":false}}})).context("Codex initialization queue unavailable")?;
        let expected =
            if spec["resume_session"] != false && spec["thread_options"]["ephemeral"] != true {
                saved["state"]["thread_id"]
                    .as_str()
                    .unwrap_or("")
                    .to_owned()
            } else {
                String::new()
            };
        Ok(Self {
            child,
            commands,
            events,
            init,
            bind,
            spec: spec.clone(),
            expected,
            runtime,
            state: Value::Null,
            ready: false,
            dirty: false,
            active: None,
        })
    }
    fn send(&self, value: Value) -> Result<()> {
        self.commands
            .try_send(value)
            .context("Codex command queue unavailable")?;
        Ok(())
    }
    pub fn start(&mut self, job: &Value) -> Result<()> {
        ensure!(
            self.ready && self.active.is_none(),
            "Codex session is not idle"
        );
        let start = format!(
            "turn-{}",
            job["provider_options"]["native_execution_instance"]
                .as_str()
                .context("missing execution instance")?
        );
        let mut params = self.spec["turn_options"]
            .as_object()
            .cloned()
            .unwrap_or_default();
        params.insert("threadId".into(), self.state["thread_id"].clone());
        params.insert("clientUserMessageId".into(), job["job_id"].clone());
        params.insert("input".into(),json!([{"type":"text","text":format!("CCB_REQ_ID: {}\n{}",job["job_id"].as_str().unwrap_or(""),job["request"]["body"].as_str().context("missing Codex prompt")?)}]));
        self.send(json!({"id":start,"method":"turn/start","params":params}))?;
        self.active = Some(Active {
            job: job.clone(),
            start,
            turn: String::new(),
            buffer: Vec::new(),
            final_text: String::new(),
            legacy_text: String::new(),
            completed: Value::Null,
            check: String::new(),
            decision: None,
            cancel: None,
            interrupt: String::new(),
            interrupt_acknowledged: false,
            interrupt_error: Value::Null,
            started: Instant::now(),
            deadline_triggered: false,
        });
        self.ready = false;
        self.state["control"] = Value::Null;
        self.dirty = true;
        Ok(())
    }
    pub fn poll(&mut self) -> Result<Option<(Value, Value)>> {
        for _ in 0..128 {
            let Ok(record) = self.events.try_recv() else {
                break;
            };
            self.ingest(record)?;
        }
        self.send_pending_interrupt()?;
        if let Some(exit) = self.child.try_wait()? {
            self.ready = false;
            if let Some(active) = self.active.take() {
                let mut decision = active.decision.clone().unwrap_or_else(|| {
                    failure(
                        "incomplete",
                        "codex_process_exit",
                        &format!("Codex exited before turn completion: {exit}"),
                    )
                });
                control_diagnostics(&active, &mut decision);
                return Ok(Some((active.job, decision)));
            }
            anyhow::bail!("Codex app-server exited");
        }
        if self.active.as_ref().is_some_and(|a| a.decision.is_some()) {
            let active = self.active.take().unwrap();
            self.ready = true;
            let mut decision = active.decision.clone().unwrap();
            control_diagnostics(&active, &mut decision);
            return Ok(Some((active.job, decision)));
        }
        Ok(None)
    }
    pub fn active_job_id(&self) -> Option<&str> {
        self.active.as_ref().and_then(|a| a.job["job_id"].as_str())
    }
    pub fn deadline_due(&self) -> bool {
        let timeout = self.spec["timeout_s"].as_f64().unwrap_or(0.0);
        self.active.as_ref().is_some_and(|a| {
            timeout.is_finite()
                && timeout > 0.0
                && a.started.elapsed().as_secs_f64() > timeout
                && a.cancel.is_none()
                && a.decision.is_none()
        })
    }
    pub fn validate_interrupt(&self, job_id: &str, record_reply: bool) -> Result<()> {
        let active = self.active.as_ref().context("Codex has no active task")?;
        ensure!(
            active.job["job_id"] == job_id,
            "Codex cancel task identity mismatch"
        );
        ensure!(
            active.cancel.is_none_or(|prior| prior == record_reply),
            "Codex cancellation reply policy already fixed"
        );
        Ok(())
    }
    // The actor commits explicit cancellation intent before applying it here.
    pub fn sync_job(&mut self, job: &Value) -> Result<()> {
        let active = self.active.as_mut().context("Codex has no active task")?;
        ensure!(
            active.job["job_id"] == job["job_id"]
                && active.job["agent_name"] == job["agent_name"]
                && active.job["provider"] == job["provider"]
                && active.job["request"] == job["request"]
                && active.job["provider_options"]["native_execution_instance"]
                    == job["provider_options"]["native_execution_instance"]
                && active.job["provider_options"]["native_daemon_instance"]
                    == job["provider_options"]["native_daemon_instance"],
            "Codex active execution identity changed"
        );
        active.job = job.clone();
        active.deadline_triggered =
            job["provider_options"]["native_cancel_intent"]["deadline_triggered"] == true;
        if let Some(record_reply) =
            job["provider_options"]["native_cancel_intent"]["record_reply"].as_bool()
        {
            self.interrupt(
                job["job_id"].as_str().context("missing job identity")?,
                record_reply,
            )?;
        }
        Ok(())
    }
    fn interrupt(&mut self, job_id: &str, record_reply: bool) -> Result<()> {
        self.validate_interrupt(job_id, record_reply)?;
        self.active.as_mut().unwrap().cancel = Some(record_reply);
        Ok(())
    }
    fn send_pending_interrupt(&mut self) -> Result<()> {
        let Some(active) = self.active.as_ref() else {
            return Ok(());
        };
        if active.cancel.is_none()
            || active.turn.is_empty()
            || !active.interrupt.is_empty()
            || active.decision.is_some()
        {
            return Ok(());
        }
        let id = format!("interrupt-{}", active.start);
        self.send(json!({"id":id,"method":"turn/interrupt","params":{
            "threadId":self.state["thread_id"],"turnId":active.turn}}))?;
        self.active.as_mut().unwrap().interrupt = id;
        Ok(())
    }
    fn ingest(&mut self, record: Value) -> Result<()> {
        if record["id"] == self.init {
            ensure!(
                record["error"].is_null() && record["result"].is_object(),
                "Codex initialization failed"
            );
            self.send(json!({"method":"initialized","params":{}}))?;
            let mut params = self.spec["thread_options"]
                .as_object()
                .cloned()
                .unwrap_or_default();
            if let Some(cwd) = self.spec["workspace_path"].as_str() {
                params.insert("cwd".into(), json!(cwd));
            }
            let method = if self.expected.is_empty() {
                "thread/start"
            } else {
                params.insert("threadId".into(), json!(self.expected));
                params.insert("excludeTurns".into(), json!(true));
                "thread/resume"
            };
            self.send(json!({"id":self.bind,"method":method,"params":params}))?;
            return Ok(());
        }
        if record["id"] == self.bind {
            ensure!(record["error"].is_null(), "Codex thread binding failed");
            let thread = &record["result"]["thread"];
            let id = thread["id"]
                .as_str()
                .filter(|s| !s.is_empty())
                .context("Codex thread identity missing")?;
            ensure!(
                self.expected.is_empty() || self.expected == id,
                "Codex resumed wrong thread"
            );
            self.state = json!({"thread_id":id,"path":thread["path"],"status":thread["status"]});
            self.ready = thread["status"]["type"] == "idle";
            self.dirty = true;
            return Ok(());
        }
        let Some(active) = self.active.as_mut() else {
            return Ok(());
        };
        if !active.interrupt.is_empty() && record["id"] == active.interrupt {
            active.interrupt_acknowledged =
                record["error"].is_null() && record["result"].is_object();
            active.interrupt_error = record["error"].clone();
            self.state["control"] = json!({"job_id":active.job["job_id"],
                "turn_id":active.turn,"interrupt_acknowledged":active.interrupt_acknowledged,
                "interrupt_error":active.interrupt_error});
            self.dirty = true;
            return Ok(());
        }
        if record["id"] == active.start {
            if !record["error"].is_null() {
                active.decision = Some(failure(
                    "failed",
                    "codex_turn_rejected",
                    record["error"]["message"]
                        .as_str()
                        .unwrap_or("Codex rejected turn"),
                ));
                return Ok(());
            }
            let turn = record["result"]["turn"]["id"]
                .as_str()
                .filter(|s| !s.is_empty())
                .context("Codex turn identity missing")?;
            active.turn = turn.to_owned();
            let buffered = std::mem::take(&mut active.buffer);
            for value in buffered {
                self.ingest(value)?;
            }
            return Ok(());
        }
        if active.turn.is_empty() && record["method"].is_string() {
            ensure!(
                active.buffer.len() < 256,
                "Codex pre-acceptance event overflow"
            );
            active.buffer.push(record);
            return Ok(());
        }
        if !active.check.is_empty() && record["id"] == active.check {
            ensure!(
                record["error"].is_null(),
                "Codex completed-turn verification failed"
            );
            let thread = &record["result"]["thread"];
            ensure!(
                thread["id"] == self.state["thread_id"],
                "Codex verification thread mismatch"
            );
            let Some(turn) = thread["turns"]
                .as_array()
                .and_then(|turns| turns.iter().find(|t| t["id"] == active.turn))
            else {
                return Ok(());
            };
            if thread["status"]["type"] != "idle"
                || !["completed", "interrupted", "failed"]
                    .contains(&turn["status"].as_str().unwrap_or(""))
            {
                return Ok(());
            }
            if let Some(items) = turn["items"].as_array() {
                for item in items {
                    capture(active, item);
                }
            }
            let reply = if !active.final_text.is_empty() {
                &active.final_text
            } else {
                &active.legacy_text
            };
            let status = match turn["status"].as_str().unwrap_or("") {
                "completed" => "completed",
                "interrupted" => "cancelled",
                _ => "failed",
            };
            let mut decision = failure(
                status,
                if status == "completed" {
                    "codex_turn_completed"
                } else if status == "cancelled" {
                    "codex_turn_interrupted"
                } else {
                    "api_error"
                },
                reply,
            );
            decision["confidence"] = json!("exact");
            decision["anchor_seen"] = json!(true);
            decision["reply_started"] = json!(!reply.is_empty());
            decision["reply_stable"] = json!(true);
            decision["provider_turn_ref"] = json!(active.turn);
            decision["diagnostics"] = json!({"completion_authority":"codex_app_server_turn_completed","runtime_instance_id":self.runtime,"thread_id":self.state["thread_id"],"turn_id":active.turn,"error_message":turn["error"]["message"]});
            active.decision = Some(decision);
            self.state["status"] = thread["status"].clone();
            self.dirty = true;
            return Ok(());
        }
        let params = &record["params"];
        if params["threadId"] != self.state["thread_id"] {
            return Ok(());
        }
        if record["method"] == "thread/status/changed"
            && params["status"]["type"] == "idle"
            && !active.check.is_empty()
        {
            let check = active.check.clone();
            let thread = self.state["thread_id"].clone();
            self.send(json!({"id":check,"method":"thread/read","params":{"threadId":thread,"includeTurns":true}}))?;
            return Ok(());
        }
        if record["method"] == "item/completed" && params["turnId"] == active.turn {
            capture(active, &params["item"]);
        }
        if record["method"] == "turn/completed" && params["turn"]["id"] == active.turn {
            active.completed = params["turn"].clone();
            active.check = format!("completed-{}", active.start);
            let check = active.check.clone();
            let thread = self.state["thread_id"].clone();
            self.send(json!({"id":check,"method":"thread/read","params":{"threadId":thread,"includeTurns":true}}))?;
        }
        Ok(())
    }
    pub fn pid(&self) -> u32 {
        self.child.id()
    }
}
fn control_diagnostics(active: &Active, decision: &mut Value) {
    if let Some(record_reply) = active.cancel {
        decision["diagnostics"]["result_reply_enabled"] = json!(record_reply);
        decision["diagnostics"]["cancel_requested"] = json!(true);
        decision["diagnostics"]["interrupt_acknowledged"] = json!(active.interrupt_acknowledged);
        decision["diagnostics"]["interrupt_error"] = active.interrupt_error.clone();
    }
    if active.deadline_triggered {
        decision["diagnostics"]["deadline_triggered"] = json!(true);
    }
}
fn capture(active: &mut Active, item: &Value) {
    if item["type"] != "agentMessage" {
        return;
    }
    let text = item["text"].as_str().unwrap_or("").trim();
    if text.is_empty() {
        return;
    }
    if item["phase"] == "final_answer" {
        active.final_text = text.into();
    } else if item["phase"].is_null() {
        active.legacy_text = text.into();
    }
}
fn failure(status: &str, reason: &str, text: &str) -> Value {
    json!({"schema_version":2,"record_type":"completion_decision","terminal":true,"status":status,"reason":reason,"confidence":"degraded","reply":text,"anchor_seen":false,"reply_started":false,"reply_stable":false,"provider_turn_ref":null,"source_cursor":null,"finished_at":crate::daemon::now(),"diagnostics":{}})
}
