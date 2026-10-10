//! Persistent Claude Code stream-json input/output with replay/session fences.
use anyhow::{ensure, Context, Result};
use serde_json::{json, Value};
use std::{
    collections::HashSet,
    io::{BufRead, BufReader, Read, Write},
    process::{Child, Command, Stdio},
    sync::mpsc,
};
struct Active {
    job: Value,
    uuid: String,
    anchor: bool,
    reply: String,
    decision: Option<Value>,
}
pub struct Session {
    child: Child,
    commands: mpsc::SyncSender<Value>,
    events: mpsc::Receiver<Value>,
    session_id: String,
    pub runtime: String,
    pub state: Value,
    pub ready: bool,
    pub dirty: bool,
    pub closed: bool,
    consumed: HashSet<String>,
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
        let mut child = command.spawn()?;
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
        let consumed = saved["state"]["last_result_id"]
            .as_str()
            .map(|id| HashSet::from([id.to_owned()]))
            .unwrap_or_default();
        Ok(Self {
            child,
            commands,
            events,
            session_id: session_id.clone(),
            runtime: uuid::Uuid::new_v4().to_string(),
            state: json!({"session_id":session_id,"initialized":false}),
            ready: true,
            dirty: true,
            closed: false,
            consumed,
            active: None,
        })
    }
    pub fn start(&mut self, job: &Value) -> Result<()> {
        ensure!(
            self.ready && self.active.is_none(),
            "Claude session is not idle"
        );
        let uuid = uuid::Uuid::new_v4().to_string();
        self.commands.try_send(json!({"type":"user","uuid":uuid,"session_id":self.session_id,"parent_tool_use_id":null,
            "message":{"role":"user","content":format!("CCB_REQ_ID: {}\n{}",job["job_id"].as_str().context("missing Claude job")?,job["request"]["body"].as_str().context("missing Claude prompt")?)}})).context("Claude input queue unavailable")?;
        self.active = Some(Active {
            job: job.clone(),
            uuid,
            anchor: false,
            reply: String::new(),
            decision: None,
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
            self.ingest(&record)?;
        }
        if self.active.as_ref().is_some_and(|a| a.decision.is_some()) {
            let active = self.active.take().unwrap();
            self.ready = true;
            return Ok(Some((active.job, active.decision.unwrap())));
        }
        if let Some(exit) = self.child.try_wait()? {
            self.ready = false;
            self.closed = true;
            self.state["process_state"] = json!("exited");
            self.dirty = true;
            if let Some(active) = self.active.take() {
                return Ok(Some((
                    active.job,
                    failure(
                        "incomplete",
                        "claude_process_exit",
                        &format!("Claude exited without a correlated result: {exit}"),
                    ),
                )));
            }
            return Ok(None);
        }
        Ok(None)
    }
    fn ingest(&mut self, r: &Value) -> Result<()> {
        if r["type"] == "system" && r["subtype"] == "init" {
            ensure!(
                r["session_id"] == self.session_id,
                "Claude initialized wrong session"
            );
            self.state["initialized"] = json!(true);
            self.dirty = true;
            return Ok(());
        }
        let Some(active) = self.active.as_mut() else {
            return Ok(());
        };
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
            let id = r["uuid"]
                .as_str()
                .filter(|id| !id.is_empty())
                .context("Claude result identity missing")?;
            if !self.consumed.insert(id.to_owned()) {
                return Ok(());
            }
            let success = r["subtype"] == "success" && r["is_error"] == false;
            let reply = r["result"]
                .as_str()
                .filter(|s| !s.trim().is_empty())
                .unwrap_or(&active.reply);
            let mut decision = failure(
                if success { "completed" } else { "failed" },
                if success {
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
            decision["diagnostics"] = json!({"completion_authority":"claude_stream_result","runtime_instance_id":self.runtime,"session_id":self.session_id,"user_message_id":active.uuid,"result_id":r["uuid"],"result_subtype":r["subtype"]});
            if !success {
                decision["diagnostics"]["error_message"] = r["result"].clone();
            }
            active.decision = Some(decision);
            self.state["last_result_id"] = r["uuid"].clone();
            self.dirty = true;
        }
        Ok(())
    }
    pub fn pid(&self) -> u32 {
        self.child.id()
    }
}
fn failure(status: &str, reason: &str, text: &str) -> Value {
    json!({"schema_version":2,"record_type":"completion_decision","terminal":true,"status":status,"reason":reason,"confidence":"degraded","reply":text,"anchor_seen":false,"reply_started":false,"reply_stable":false,"provider_turn_ref":null,"source_cursor":null,"finished_at":crate::daemon::now(),"diagnostics":{}})
}
