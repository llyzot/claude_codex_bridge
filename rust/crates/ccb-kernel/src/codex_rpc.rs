//! Owned Codex app-server stdio session with thread/turn identity fences.
use anyhow::{ensure, Context, Result};
use serde_json::{json, Value};
use std::{
    io::{BufRead, BufReader, Read, Write},
    process::{Command, Stdio},
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
    requests: Vec<Pending>,
}
struct Pending {
    handle: String,
    wire_id: Value,
    method: String,
    params: Value,
    answer: Option<Value>,
}
enum StreamEvent {
    Record(Value),
    Fault,
    End,
}
pub struct Session {
    child: crate::owned_process::OwnedChild,
    commands: mpsc::SyncSender<Value>,
    events: mpsc::Receiver<StreamEvent>,
    init: String,
    bind: String,
    spec: Value,
    expected: String,
    pub runtime: String,
    pub state: Value,
    pub ready: bool,
    pub closed: bool,
    pub quarantined: bool,
    forced: bool,
    stdout_closed: bool,
    input_closed: bool,
    saved_state: Value,
    pub dirty: bool,
    active: Option<Active>,
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
        let runtime = uuid::Uuid::new_v4().to_string();
        let init = format!("init-{runtime}");
        let bind = format!("thread-{runtime}");
        ensure!(
            spec["capabilities"].is_null() || spec["capabilities"].is_object(),
            "Codex capabilities must be an object"
        );
        let mut capabilities = spec["capabilities"]
            .as_object()
            .cloned()
            .unwrap_or_default();
        capabilities
            .entry("experimentalApi".to_owned())
            .or_insert(json!(false));
        let params = json!({"clientInfo":{"name":"ccb-native","version":env!("CARGO_PKG_VERSION")},"capabilities":capabilities});
        crate::codex_requests::validate_initialize(&params)?;
        let initialize = json!({"id":init,"method":"initialize","params":params});
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
        let mut child = crate::owned_process::OwnedChild::spawn(&mut command)?;
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
                        if let Ok(value) = serde_json::from_slice::<Value>(&line).and_then(|v| {
                            if v.is_object() {
                                Ok(v)
                            } else {
                                Err(serde::de::Error::custom("non-object provider record"))
                            }
                        }) {
                            if sender.send(StreamEvent::Record(value)).is_err() {
                                break;
                            }
                        } else if sender.send(StreamEvent::Fault).is_err() {
                            break;
                        }
                    }
                    _ => {
                        let _ = sender.send(StreamEvent::Fault);
                        break;
                    }
                }
            }
            let _ = sender.send(StreamEvent::End);
        });
        commands
            .try_send(initialize)
            .context("Codex initialization queue unavailable")?;
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
            closed: false,
            quarantined: false,
            forced: false,
            stdout_closed: false,
            input_closed: false,
            saved_state: saved["state"].clone(),
            dirty: false,
            active: None,
        })
    }
    fn send(&mut self, value: Value) -> Result<()> {
        match self.commands.try_send(value) {
            Ok(()) => Ok(()),
            Err(mpsc::TrySendError::Disconnected(_)) => {
                self.input_closed = true;
                Ok(())
            }
            Err(error) => Err(error).context("Codex command queue unavailable"),
        }
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
            requests: Vec::new(),
        });
        self.ready = false;
        self.state["control"] = Value::Null;
        self.dirty = true;
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
                StreamEvent::Record(record) => {
                    if !self.quarantined {
                        self.ingest(record)?;
                    }
                }
                StreamEvent::Fault => {
                    if !self.quarantined {
                        anyhow::bail!("invalid provider protocol frame");
                    }
                }
                StreamEvent::End => self.stdout_closed = true,
            }
        }
        self.refresh_pending_metadata();
        let exit = self.child.try_wait()?;
        let tree_empty = exit.is_some() && self.child.tree_empty()?;
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
        if self.active.as_ref().is_some_and(|a| a.decision.is_some()) {
            let active = self.active.take().unwrap();
            self.refresh_pending_metadata();
            self.closed = exit.is_some() && self.stdout_closed && tree_empty;
            self.ready = !self.quarantined
                && !self.forced
                && exit.is_none()
                && !self.stdout_closed
                && !self.input_closed;
            let mut decision = active.decision.clone().unwrap();
            control_diagnostics(&active, &mut decision);
            return Ok(Some((active.job, decision)));
        }
        if let Some(exit) = exit.filter(|_| self.stdout_closed && tree_empty) {
            self.closed = true;
            if let Some(active) = self.active.take() {
                let mut decision = active.decision.clone().unwrap_or_else(|| {
                    failure(
                        if self.forced {
                            "cancelled"
                        } else if self.quarantined {
                            "failed"
                        } else {
                            "incomplete"
                        },
                        if self.forced {
                            "provider_terminated"
                        } else if self.quarantined {
                            "provider_protocol_error"
                        } else {
                            "codex_process_exit"
                        },
                        &format!("Codex exited before turn completion: {exit}"),
                    )
                });
                if self.forced {
                    decision["diagnostics"]["completion_authority"] =
                        json!("owned_windows_job_exit");
                    decision["diagnostics"]["owned_tree_empty"] = json!(true);
                    decision["diagnostics"]["scope_id"] =
                        self.child.scope_info()?["scope_id"].clone();
                }
                control_diagnostics(&active, &mut decision);
                self.refresh_pending_metadata();
                return Ok(Some((active.job, decision)));
            }
            return Ok(None);
        }
        if !self.quarantined
            && !self.forced
            && exit.is_none()
            && !self.stdout_closed
            && !self.input_closed
        {
            self.send_pending_interrupt()?;
        }
        Ok(None)
    }
    pub fn active_job_id(&self) -> Option<&str> {
        self.active.as_ref().and_then(|a| a.job["job_id"].as_str())
    }
    pub fn pending_requests(&self) -> Value {
        let rows = self.active.as_ref().map(|active| active.requests.iter().map(|p|
            json!({"request_id":p.handle,"method":p.method,"params":p.params,
                "job_id":active.job["job_id"],"execution_instance_id":active.job["provider_options"]["native_execution_instance"],
                "runtime_instance_id":self.runtime,"status":if p.answer.is_some(){"response_queued"}else{"awaiting_user"}}))
            .collect::<Vec<_>>()).unwrap_or_default();
        json!(rows)
    }
    fn refresh_pending_metadata(&mut self) {
        if self.state["thread_id"].as_str().is_none_or(str::is_empty) {
            return;
        }
        let summaries = self.active.as_ref().map(|active| active.requests.iter().map(|p|
            json!({"request_id":p.handle,"method":p.method,"job_id":active.job["job_id"],
                "turn_id":active.turn,"status":if p.answer.is_some(){"response_queued"}else{"awaiting_user"}}))
            .collect::<Vec<_>>()).unwrap_or_default();
        let summaries = json!(summaries);
        if self.state["pending_requests"] != summaries {
            self.state["pending_requests"] = summaries;
            self.dirty = true;
        }
    }
    pub fn validate_response(&self, payload: &Value) -> Result<bool> {
        ensure!(
            payload["runtime_instance_id"] == self.runtime,
            "Codex response runtime identity mismatch"
        );
        let active = self.active.as_ref().context("Codex has no active task")?;
        ensure!(
            payload["job_id"] == active.job["job_id"]
                && payload["execution_instance_id"]
                    == active.job["provider_options"]["native_execution_instance"],
            "Codex response execution identity mismatch"
        );
        ensure!(
            active.cancel.is_none() && active.decision.is_none() && active.completed.is_null(),
            "Codex task no longer accepts user responses"
        );
        let request = active
            .requests
            .iter()
            .find(|r| payload["request_id"] == r.handle)
            .context("unknown or resolved Codex request")?;
        crate::codex_requests::validate_answer(
            &request.method,
            &request.params,
            &payload["result"],
        )?;
        if let Some(prior) = &request.answer {
            ensure!(
                prior == &payload["result"],
                "Codex request already has a different answer"
            );
            return Ok(false);
        }
        Ok(true)
    }
    pub fn respond(&mut self, payload: &Value) -> Result<()> {
        if !self.validate_response(payload)? {
            return Ok(());
        }
        let pending = self
            .active
            .as_ref()
            .unwrap()
            .requests
            .iter()
            .find(|r| payload["request_id"] == r.handle)
            .unwrap();
        self.send(json!({"id":pending.wire_id,"result":payload["result"]}))?;
        let pending = self
            .active
            .as_mut()
            .unwrap()
            .requests
            .iter_mut()
            .find(|r| payload["request_id"] == r.handle)
            .unwrap();
        pending.answer = Some(payload["result"].clone());
        self.dirty = true;
        Ok(())
    }
    fn server_request(&mut self, record: Value) -> Result<()> {
        let method = record["method"].as_str().unwrap_or("");
        let reject =
            |message: &str| json!({"id":record["id"],"error":{"code":-32602,"message":message}});
        if !crate::codex_requests::supported(method) {
            return self.send(json!({"id":record["id"],"error":{"code":-32601,"message":"Unsupported native Codex server request"}}));
        }
        if record["id"].as_i64().is_none() && !record["id"].is_string() {
            return self.send(reject("Invalid Codex server request identity"));
        }
        if crate::codex_requests::validate_params(method, &record["params"]).is_err() {
            return self.send(reject("Invalid Codex server request shape"));
        }
        let Some(active) = self.active.as_mut() else {
            return self.send(reject("No native task owns this request"));
        };
        if active.turn.is_empty() {
            ensure!(
                active.buffer.len() < 256,
                "Codex pre-acceptance event overflow"
            );
            active.buffer.push(record);
            return Ok(());
        }
        let params = &record["params"];
        if params["threadId"] != self.state["thread_id"]
            || (!params["turnId"].is_null() && params["turnId"] != active.turn)
            || !active.completed.is_null()
            || active.decision.is_some()
            || active.cancel.is_some()
        {
            return self.send(reject("Codex request does not belong to the active turn"));
        }
        if let Some(prior) = active.requests.iter().find(|r| r.wire_id == record["id"]) {
            if prior.method == method && prior.params == *params {
                return Ok(());
            }
            return self.send(reject("Codex server request identity was reused"));
        }
        let total_bytes = active
            .requests
            .iter()
            .map(|r| r.params.to_string().len())
            .sum::<usize>();
        if active.requests.len() >= 32 || total_bytes + params.to_string().len() > 512 * 1024 {
            return self.send(reject("Native Codex pending request capacity exceeded"));
        }
        active.requests.push(Pending {
            handle: format!("prq-{}", uuid::Uuid::new_v4().simple()),
            wire_id: record["id"].clone(),
            method: method.to_owned(),
            params: params.clone(),
            answer: None,
        });
        self.dirty = true;
        Ok(())
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
        let active = self.active.as_mut().unwrap();
        active.cancel = Some(record_reply);
        active.requests.clear();
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
        if record["method"].is_string() && !record["id"].is_null() {
            return self.server_request(record);
        }
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
        if record["method"] == "serverRequest/resolved" {
            active.requests.retain(|r| r.wire_id != params["requestId"]);
            self.dirty = true;
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
            active.requests.clear();
            active.check = format!("completed-{}", active.start);
            let check = active.check.clone();
            let thread = self.state["thread_id"].clone();
            self.send(json!({"id":check,"method":"thread/read","params":{"threadId":thread,"includeTurns":true}}))?;
        }
        Ok(())
    }
    pub fn quarantine(&mut self, previous_state: Value) {
        self.state = previous_state;
        if let Some(active) = self.active.as_mut() {
            active.requests.clear();
        }
        self.quarantined = true;
        self.ready = false;
        self.dirty = false;
    }
    pub fn terminate_tree(&mut self) -> Result<()> {
        ensure!(
            self.child.scope_info()?["supported"] == true,
            "provider tree termination is unsupported on this platform"
        );
        self.forced = true;
        self.ready = false;
        self.child.kill()?;
        Ok(())
    }
    pub fn scope_info(&self) -> Result<Value> {
        self.child.scope_info()
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
