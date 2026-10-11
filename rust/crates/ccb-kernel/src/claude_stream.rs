//! Persistent Claude Code stream-json input/output with replay/session fences.
use anyhow::{ensure, Context, Result};
use serde_json::{json, Value};
use std::{
    collections::HashSet,
    io::{BufRead, BufReader, Read, Write},
    process::{Command, Stdio},
    sync::mpsc,
    time::Instant,
};
enum StreamEvent {
    Record(Value),
    Fault,
    End,
}
struct Active {
    skills: Option<crate::claude_skills::Load>,
    job: Value,
    uuid: String,
    anchor: bool,
    reply: String,
    decision: Option<Value>,
    cancel: Option<bool>,
    interrupt_id: String,
    queue_cancel_id: String,
    interrupt_acknowledged: bool,
    control_error: Value,
    deadline_triggered: bool,
    started: Instant,
}
pub struct Session {
    surface: Option<crate::skill_surface::Surface>,
    staged_roles: Option<(String, crate::skill_surface::Routes)>,
    initialize: Option<String>,
    child: crate::owned_process::OwnedChild,
    commands: mpsc::SyncSender<Value>,
    events: mpsc::Receiver<StreamEvent>,
    session_id: String,
    pub runtime: String,
    pub state: Value,
    pub ready: bool,
    pub dirty: bool,
    pub closed: bool,
    pub quarantined: bool,
    forced: bool,
    consumed: HashSet<String>,
    active: Option<Active>,
    stdout_closed: bool,
    require_result_identity: bool,
    timeout_s: f64,
}
impl Session {
    pub fn spawn(
        agent: &str,
        spec: &Value,
        endpoint: &Value,
        token: &std::path::Path,
        saved: &Value,
        surface: Option<crate::skill_surface::Surface>,
    ) -> Result<Self> {
        let argv = spec["command"]
            .as_array()
            .context("Claude requires stream-json argv")?;
        ensure!(
            !argv.is_empty() && argv.iter().all(Value::is_string),
            "invalid Claude argv"
        );
        let saved_id = if spec["resume_session"] != false {
            saved["state"]["session_id"].as_str().unwrap_or("")
        } else {
            ""
        };
        let session_id = if saved_id.is_empty() {
            uuid::Uuid::new_v4().to_string()
        } else {
            saved_id.to_owned()
        };
        let mut command = Command::new(argv[0].as_str().unwrap());
        for arg in &argv[1..] {
            command.arg(arg.as_str().unwrap());
        }
        command
            .arg(if saved_id.is_empty() {
                "--session-id"
            } else {
                "--resume"
            })
            .arg(&session_id);
        if let Some(surface) = &surface {
            command.arg("--add-dir").arg(&surface.root);
        }
        if let Some(cwd) = spec["workspace_path"].as_str() {
            command.current_dir(cwd);
        }
        if let Some(env) = spec["env"].as_object() {
            for (key, value) in env {
                command.env(
                    key,
                    value
                        .as_str()
                        .context("Claude env values must be strings")?,
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
        let mut stdin = child.stdin.take().context("missing Claude stdin")?;
        let stdout = child.stdout.take().context("missing Claude stdout")?;
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
        let consumed = saved["state"]["last_result_id"]
            .as_str()
            .map(|id| HashSet::from([id.to_owned()]))
            .unwrap_or_default();
        let initialize = surface
            .as_ref()
            .map(|_| format!("role-init-{}", uuid::Uuid::new_v4()));
        if let Some(id) = &initialize {
            commands.try_send(json!({"type":"control_request","request_id":id,"request":{"subtype":"initialize"}}))
                .context("Claude role initialization unavailable")?;
        }
        Ok(Self {
            staged_roles: None,
            ready: initialize.is_none(),
            initialize,
            surface,
            child,
            commands,
            events,
            session_id: session_id.clone(),
            runtime: uuid::Uuid::new_v4().to_string(),
            state: json!({"session_id":session_id,"initialized":false,
                "require_result_identity":saved["state"]["require_result_identity"]==true}),
            dirty: true,
            closed: false,
            quarantined: false,
            forced: false,
            consumed,
            active: None,
            stdout_closed: false,
            require_result_identity: saved["state"]["require_result_identity"] == true,
            timeout_s: spec["timeout_s"].as_f64().unwrap_or(0.0),
        })
    }
    pub fn stage_role_paths(&mut self, job: &Value, roots: &[std::path::PathBuf]) -> Result<()> {
        ensure!(
            self.ready && self.active.is_none(),
            "Claude is not idle for role projection"
        );
        let routes = self
            .surface
            .as_ref()
            .context("Claude role view missing")?
            .sync(roots)?;
        self.staged_roles = Some((
            crate::records::string(&job["provider_options"], "native_execution_instance")?.into(),
            routes,
        ));
        Ok(())
    }
    pub fn start(&mut self, job: &Value, prompt: &str) -> Result<()> {
        ensure!(
            self.ready && self.active.is_none(),
            "Claude session is not idle"
        );
        let uuid = uuid::Uuid::new_v4().to_string();
        let input = json!({"type":"user","uuid":uuid,"session_id":self.session_id,"parent_tool_use_id":null,
            "message":{"role":"user","content":format!("CCB_REQ_ID: {}\n{}",job["job_id"].as_str().context("missing Claude job")?,prompt)}});
        let skills = if self.surface.is_some() {
            let (generation, routes) = self
                .staged_roles
                .take()
                .context("Claude role routes were not prepared")?;
            ensure!(
                generation
                    == crate::records::string(
                        &job["provider_options"],
                        "native_execution_instance"
                    )?,
                "Claude role execution mismatch"
            );
            let load =
                crate::claude_skills::Load::new(&generation, &self.session_id, routes, input);
            self.commands
                .try_send(load.request())
                .context("Claude skill refresh unavailable")?;
            Some(load)
        } else {
            self.commands
                .try_send(input)
                .context("Claude input queue unavailable")?;
            None
        };
        self.active = Some(Active {
            skills,
            job: job.clone(),
            uuid,
            anchor: false,
            reply: String::new(),
            decision: None,
            cancel: None,
            interrupt_id: String::new(),
            queue_cancel_id: String::new(),
            interrupt_acknowledged: false,
            control_error: Value::Null,
            deadline_triggered: false,
            started: Instant::now(),
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
                        self.ingest(&record)?;
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
        let exit = self.child.try_wait()?;
        let tree_empty = exit.is_some() && self.child.tree_empty()?;
        if exit.is_some() {
            self.ready = false;
            if self.state["process_state"] != "exited" {
                self.state["process_state"] = json!("exited");
                self.dirty = true;
            }
        }
        if self.active.as_ref().is_some_and(|a| a.decision.is_some()) {
            let active = self.active.take().unwrap();
            self.closed = exit.is_some() && self.stdout_closed && tree_empty;
            self.ready = !self.quarantined && !self.forced && exit.is_none() && !self.stdout_closed;
            let mut decision = active.decision.clone().unwrap();
            control_diagnostics(&active, &mut decision);
            return Ok(Some((active.job, decision)));
        }
        if let Some(exit) = exit.filter(|_| self.stdout_closed && tree_empty) {
            self.ready = false;
            self.closed = true;
            self.state["process_state"] = json!("exited");
            self.dirty = true;
            ensure!(
                self.initialize.is_none(),
                "Claude exited before role initialization"
            );
            if let Some(active) = self.active.take() {
                let mut decision = failure(
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
                        "claude_process_exit"
                    },
                    &format!("Claude exited without a correlated result: {exit}"),
                );
                if self.forced {
                    decision["diagnostics"]["completion_authority"] =
                        json!("owned_windows_job_exit");
                    decision["diagnostics"]["owned_tree_empty"] = json!(true);
                    decision["diagnostics"]["scope_id"] =
                        self.child.scope_info()?["scope_id"].clone();
                }
                control_diagnostics(&active, &mut decision);
                return Ok(Some((active.job, decision)));
            }
            return Ok(None);
        }
        if !self.quarantined && !self.forced && exit.is_none() && !self.stdout_closed {
            if let Some(active) = self.active.as_mut() {
                if active.cancel.is_some()
                    && active.interrupt_id.is_empty()
                    && active.skills.is_none()
                {
                    let id = format!("interrupt-{}-{}", self.runtime, active.uuid);
                    self.commands
                        .try_send(json!({"type":"control_request","request_id":id,
                        "request":{"subtype":"interrupt","cancel_queued":true}}))
                        .context("Claude interrupt queue unavailable")?;
                    active.interrupt_id = id;
                }
            }
        }
        Ok(None)
    }
    pub fn active_job_id(&self) -> Option<&str> {
        self.active.as_ref().and_then(|a| a.job["job_id"].as_str())
    }
    pub fn deadline_due(&self) -> bool {
        !self.quarantined
            && !self.forced
            && self.active.as_ref().is_some_and(|a| {
                self.timeout_s.is_finite()
                    && self.timeout_s > 0.0
                    && a.started.elapsed().as_secs_f64() > self.timeout_s
                    && a.cancel.is_none()
                    && a.decision.is_none()
                    && self.state["process_state"] != "exited"
            })
    }
    pub fn validate_interrupt(&self, id: &str, record_reply: bool) -> Result<()> {
        let active = self.active.as_ref().context("Claude has no active task")?;
        ensure!(
            active.job["job_id"] == id,
            "Claude cancel task identity mismatch"
        );
        ensure!(
            active.cancel.is_none_or(|prior| prior == record_reply),
            "Claude cancellation reply policy already fixed"
        );
        Ok(())
    }
    pub fn sync_job(&mut self, job: &Value) -> Result<()> {
        let active = self.active.as_mut().context("Claude has no active task")?;
        ensure!(
            active.job["job_id"] == job["job_id"]
                && active.job["agent_name"] == job["agent_name"]
                && active.job["provider"] == job["provider"]
                && active.job["request"] == job["request"]
                && active.job["provider_options"]["native_execution_instance"]
                    == job["provider_options"]["native_execution_instance"]
                && active.job["provider_options"]["native_daemon_instance"]
                    == job["provider_options"]["native_daemon_instance"],
            "Claude active execution identity changed"
        );
        active.job = job.clone();
        active.deadline_triggered =
            job["provider_options"]["native_cancel_intent"]["deadline_triggered"] == true;
        if let Some(reply) =
            job["provider_options"]["native_cancel_intent"]["record_reply"].as_bool()
        {
            self.validate_interrupt(job["job_id"].as_str().unwrap(), reply)?;
            self.active.as_mut().unwrap().cancel = Some(reply);
        }
        Ok(())
    }
    fn ingest(&mut self, r: &Value) -> Result<()> {
        if r["type"] == "control_response"
            && self
                .initialize
                .as_ref()
                .is_some_and(|id| r["response"]["request_id"] == *id)
        {
            ensure!(
                (r["session_id"].is_null() || r["session_id"] == self.session_id)
                    && r["response"]["subtype"] == "success"
                    && r["response"]["response"].is_object(),
                "Claude role initialization failed"
            );
            self.initialize = None;
            self.ready = true;
            self.state["control_initialized"] = json!(true);
            self.dirty = true;
            return Ok(());
        }
        if r["type"] == "system" && r["subtype"] == "init" {
            ensure!(
                r["session_id"] == self.session_id,
                "Claude initialized wrong session"
            );
            self.state["initialized"] = json!(true);
            self.state["capabilities"] = json!(r["capabilities"]
                .as_array()
                .map(|values| values.iter().filter_map(Value::as_str).collect::<Vec<_>>())
                .unwrap_or_default());
            self.dirty = true;
            return Ok(());
        }
        let Some(active) = self.active.as_mut() else {
            return Ok(());
        };
        if let Some(load) = active.skills.as_mut() {
            match load.ingest(r, active.cancel.is_some())? {
                Some(crate::claude_skills::Step::Ready(input, bindings)) => {
                    self.state["role_skills"] =
                        json!({"verified":true,"bindings":bindings,"job_id":active.job["job_id"]});
                    self.dirty = true;
                    active.skills = None;
                    self.commands
                        .try_send(input)
                        .context("Claude prepared input unavailable")?;
                }
                Some(crate::claude_skills::Step::Stop(reason)) => {
                    let cancelled = active.cancel.is_some();
                    let mut decision = failure(
                        if cancelled { "cancelled" } else { "failed" },
                        reason,
                        if cancelled {
                            "Cancelled before Claude task input."
                        } else {
                            "Claude could not verify the configured role skills. No task prompt was sent."
                        },
                    );
                    decision["diagnostics"] = json!({"completion_authority":"claude_role_skill_preflight","provider_prompt_sent":false,"configuration_error":!cancelled});
                    active.decision = Some(decision);
                    active.skills = None;
                    self.state["role_skills"] =
                        json!({"verified":false,"job_id":active.job["job_id"]});
                    self.dirty = true;
                }
                None => {}
            }
            return Ok(());
        }
        if r["type"] == "control_response"
            && (r["session_id"].is_null() || r["session_id"] == self.session_id)
        {
            let response = &r["response"];
            let interrupt =
                !active.interrupt_id.is_empty() && response["request_id"] == active.interrupt_id;
            let queue_cancel = !active.queue_cancel_id.is_empty()
                && response["request_id"] == active.queue_cancel_id;
            if !interrupt && !queue_cancel {
                return Ok(());
            }
            let success = response["subtype"] == "success";
            if interrupt {
                active.interrupt_acknowledged = success;
            }
            if !success {
                active.control_error = response["error"].clone();
            }
            let payload = &response["response"];
            let cancelled = success
                && (queue_cancel && payload["cancelled"] == true
                    || interrupt
                        && payload["cancelled"]
                            .as_array()
                            .is_some_and(|ids| ids.iter().any(|id| id == &active.uuid)));
            if cancelled && active.decision.is_none() {
                self.require_result_identity = true;
                self.state["require_result_identity"] = json!(true);
                let mut decision = failure("cancelled", "claude_queued_cancelled", "");
                decision["confidence"] = json!("exact");
                decision["provider_turn_ref"] = json!(self.session_id);
                decision["diagnostics"] = json!({"completion_authority":"claude_control_queue_cancel_receipt",
                    "runtime_instance_id":self.runtime,"session_id":self.session_id,"user_message_id":active.uuid});
                active.decision = Some(decision);
            } else if !cancelled
                && success
                && interrupt
                && payload["still_queued"]
                    .as_array()
                    .is_some_and(|ids| ids.iter().any(|id| id == &active.uuid))
            {
                let id = format!("cancel-queue-{}-{}", self.runtime, active.uuid);
                self.commands
                    .try_send(json!({"type":"control_request","request_id":id,
                    "request":{"subtype":"cancel_async_message","message_uuid":active.uuid}}))
                    .context("Claude queued-message cancel unavailable")?;
                active.queue_cancel_id = id;
            }
            self.state["control"] = json!({"job_id":active.job["job_id"],
                "interrupt_acknowledged":active.interrupt_acknowledged,"control_error":active.control_error});
            self.dirty = true;
            return Ok(());
        }
        if r["session_id"] != self.session_id || !r["parent_tool_use_id"].is_null() {
            return Ok(());
        }
        if r["type"] == "user" && r["uuid"] == active.uuid {
            let content = &r["message"]["content"];
            let text = if let Some(s) = content.as_str() {
                s.to_owned()
            } else {
                content
                    .as_array()
                    .map(|p| {
                        p.iter()
                            .filter_map(|p| p["text"].as_str())
                            .collect::<Vec<_>>()
                            .join("\n")
                    })
                    .unwrap_or_default()
            };
            let expected = format!(
                "CCB_REQ_ID: {}",
                active.job["job_id"].as_str().unwrap_or("")
            );
            if text
                .lines()
                .next()
                .is_some_and(|line| line.trim() == expected)
            {
                active.anchor = true;
            }
        }
        if r["type"] == "assistant" && active.anchor {
            active.reply = r["message"]["content"]
                .as_array()
                .map(|p| {
                    p.iter()
                        .filter(|p| p["type"] == "text")
                        .filter_map(|p| p["text"].as_str())
                        .collect::<Vec<_>>()
                        .join("\n")
                })
                .unwrap_or_default();
        }
        if r["type"] == "result" && active.anchor {
            let attributed =
                r.get("user_message_uuid").is_some() || r.get("user_message_uuids").is_some();
            if (attributed || self.require_result_identity)
                && !(r["user_message_uuid"] == active.uuid
                    || r["user_message_uuids"]
                        .as_array()
                        .is_some_and(|ids| ids.iter().any(|id| id == &active.uuid)))
            {
                return Ok(());
            }
            let id = r["uuid"]
                .as_str()
                .filter(|id| !id.is_empty())
                .context("Claude result identity missing")?;
            if !self.consumed.insert(id.to_owned()) {
                return Ok(());
            }
            let success = r["subtype"] == "success" && r["is_error"] == false;
            let interrupted = ["aborted_streaming", "aborted_tools"]
                .contains(&r["terminal_reason"].as_str().unwrap_or(""));
            let errors = r["errors"]
                .as_array()
                .map(|values| {
                    values
                        .iter()
                        .filter_map(Value::as_str)
                        .collect::<Vec<_>>()
                        .join("\n")
                })
                .unwrap_or_default();
            let reply = r["result"]
                .as_str()
                .filter(|s| !s.trim().is_empty())
                .unwrap_or(&active.reply);
            let mut decision = failure(
                if interrupted {
                    "cancelled"
                } else if success {
                    "completed"
                } else {
                    "failed"
                },
                if interrupted {
                    "claude_interrupted"
                } else if success {
                    "claude_stream_result"
                } else {
                    "api_error"
                },
                reply,
            );
            decision["confidence"] = json!("exact");
            decision["anchor_seen"] = json!(true);
            decision["reply_started"] = json!(!reply.is_empty());
            decision["reply_stable"] = json!(true);
            decision["provider_turn_ref"] = json!(self.session_id);
            decision["diagnostics"] = json!({"completion_authority":"claude_stream_result","runtime_instance_id":self.runtime,"session_id":self.session_id,"user_message_id":active.uuid,"result_id":r["uuid"],"result_subtype":r["subtype"],"terminal_reason":r["terminal_reason"]});
            if !success {
                decision["diagnostics"]["error_message"] = if !errors.is_empty() {
                    json!(errors)
                } else {
                    r["result"].clone()
                };
            }
            active.decision = Some(decision);
            self.state["last_result_id"] = r["uuid"].clone();
            self.dirty = true;
        }
        Ok(())
    }
    pub fn quarantine(&mut self, previous_state: Value) {
        self.state = previous_state;
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
    if let Some(reply) = active.cancel {
        decision["diagnostics"]["result_reply_enabled"] = json!(reply);
        decision["diagnostics"]["cancel_requested"] = json!(true);
        decision["diagnostics"]["interrupt_acknowledged"] = json!(active.interrupt_acknowledged);
        decision["diagnostics"]["control_error"] = active.control_error.clone();
    }
    if active.deadline_triggered {
        decision["diagnostics"]["deadline_triggered"] = json!(true);
    }
}
fn failure(status: &str, reason: &str, text: &str) -> Value {
    json!({"schema_version":2,"record_type":"completion_decision","terminal":true,"status":status,"reason":reason,"confidence":"degraded","reply":text,"anchor_seen":false,"reply_started":false,"reply_stable":false,"provider_turn_ref":null,"source_cursor":null,"finished_at":crate::daemon::now(),"diagnostics":{}})
}
