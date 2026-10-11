//! A single Herdr terminal task, observed off the authority actor.
//! Only the actor grants sending, after it commits the exact observation baseline.
use crate::{
    codex_history::{self, Baseline, Binding, Reader},
    herdr_agents,
};
use anyhow::{ensure, Context, Result};
use serde_json::{json, Value};
use std::{
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc, Arc,
    },
    time::{Duration, Instant},
};

pub(crate) enum Event {
    Prepared(Value),
    Completed(Value),
    Ready,
    Fault,
    Unsent,
}
pub(crate) struct Worker {
    pub events: mpsc::Receiver<Event>,
    pub grant: mpsc::Sender<()>,
    pub view: Value,
    pub job: String,
    alive: Arc<AtomicBool>,
}
impl Drop for Worker {
    fn drop(&mut self) {
        self.alive.store(false, Ordering::Release);
    }
}
pub(crate) fn validate(config: &Value, name: &str, spec: &Value) -> Result<()> {
    ensure!(
        cfg!(windows),
        "Herdr terminal workers currently require Windows"
    );
    ensure!(
        spec["provider"] == "codex" && spec["command"].is_null(),
        "Herdr terminal driver currently supports Codex only, with no owned command"
    );
    let request = &config["herdr"];
    ensure!(
        request["config"]["project_id"] == config["project_id"]
            && request["config"]["project_root"] == config["project_root"],
        "Herdr namespace must belong to the daemon project"
    );
    ensure!(
        request["config"]["agents"][name]["provider"] == spec["provider"]
            && request["config"]["agents"][name]["workspace_path"] == spec["workspace_path"],
        "Herdr mount and driver provider/workspace differ"
    );
    let binding: Binding = serde_json::from_value(spec["herdr_history"].clone())
        .context("invalid Herdr Codex history binding")?;
    binding.validate()?;
    ensure!(
        json!(binding.cwd) == spec["workspace_path"],
        "history and agent workspace differ"
    );
    if !spec["timeout_s"].is_null() {
        ensure!(
            spec["timeout_s"]
                .as_f64()
                .is_some_and(|t| t.is_finite() && t > 0.0 && t <= 86400.0),
            "invalid Herdr observation timeout"
        );
    }
    Ok(())
}
fn observe(
    request: &Value,
    agent: &str,
    binding: &Binding,
    expected: Option<&Value>,
) -> Result<Value> {
    let live = herdr_agents::bound(request, agent)?;
    ensure!(
        live["agent_session"]["kind"] == "id"
            && live["agent_session"]["agent"] == "codex"
            && live["agent_session"]["value"] == binding.session_id,
        "Herdr session does not match the declared Codex history"
    );
    if let Some(expected) = expected {
        ensure!(
            live["identity"] == *expected,
            "Herdr task process/namespace changed"
        );
    }
    Ok(live)
}
impl Worker {
    pub fn spawn(
        request: Value,
        agent: String,
        spec: Value,
        job: &Value,
        prompt: String,
    ) -> Result<Self> {
        let binding: Binding = serde_json::from_value(spec["herdr_history"].clone())?;
        let job_id = crate::records::string(job, "job_id")?.to_owned();
        let execution =
            crate::records::string(&job["provider_options"], "native_execution_instance")?
                .to_owned();
        let saved = job["provider_options"]["herdr_dispatch"].clone();
        let pid = saved["identity"]["process"]["pid"].clone();
        let (send, events) = mpsc::channel();
        let (grant, granted) = mpsc::channel();
        let alive = Arc::new(AtomicBool::new(true));
        let thread_alive = Arc::clone(&alive);
        let task = job_id.clone();
        let runtime = execution.clone();
        std::thread::Builder::new()
            .name(format!("herdr-{agent}"))
            .spawn(move || {
                let mut permitted = !saved.is_null();
                if run(
                    &request,
                    &agent,
                    &spec,
                    &binding,
                    &task,
                    &execution,
                    &saved,
                    &prompt,
                    &send,
                    &granted,
                    &thread_alive,
                    &mut permitted,
                )
                .is_err()
                {
                    let _ = send.send(if permitted {
                        Event::Fault
                    } else {
                        Event::Unsent
                    });
                }
            })?;
        Ok(Self {
            events,
            grant,
            job: job_id,
            alive,
            view: json!({"external":true,"closed":false,"ready":false,"pid":pid,"runtime_instance_id":if pid.is_null() {Value::Null} else {json!(runtime)}}),
        })
    }
}
#[allow(clippy::too_many_arguments)]
fn run(
    request: &Value,
    agent: &str,
    spec: &Value,
    binding: &Binding,
    task: &str,
    execution: &str,
    saved: &Value,
    prompt: &str,
    events: &mpsc::Sender<Event>,
    granted: &mpsc::Receiver<()>,
    alive: &AtomicBool,
    permitted: &mut bool,
) -> Result<()> {
    let anchor = format!("{task}:{execution}");
    let input = format!("CCB_REQ_ID: {anchor}\n{prompt}");
    let proof = if saved.is_null() {
        // Fail before granting input if native argv could exceed Windows limits.
        ensure!(
            !input.contains('\0') && input.encode_utf16().count() <= 8000,
            "Herdr terminal prompt exceeds supported argv limit"
        );
        ensure!(
            request["command"].to_string().encode_utf16().count() <= 8000,
            "Herdr command exceeds supported argv limit"
        );
        let waiting = Instant::now();
        let live = loop {
            if !alive.load(Ordering::Acquire) {
                return Ok(());
            }
            let live = observe(request, agent, binding, None)?;
            if live["interactive_ready"] == true {
                break live;
            }
            // Also covers restart after completion was committed but before the
            // previous worker's in-memory readiness barrier was released.
            ensure!(
                waiting.elapsed().as_secs_f64() < spec["timeout_s"].as_f64().unwrap_or(3600.0),
                "Herdr worker did not become ready before the preflight deadline"
            );
            std::thread::sleep(Duration::from_millis(100));
        };
        let baseline = codex_history::baseline(binding)?;
        let proof = json!({"schema_version":1,"job_id":task,"execution_instance_id":execution,"request_anchor":anchor,
            "identity":live["identity"],"history_binding":binding,"baseline":baseline});
        events.send(Event::Prepared(proof.clone()))?;
        loop {
            if !alive.load(Ordering::Acquire) {
                return Ok(());
            }
            match granted.recv_timeout(Duration::from_millis(100)) {
                Ok(()) => {
                    *permitted = true;
                    break;
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(_) => return Ok(()),
            }
        }
        if !alive.load(Ordering::Acquire) {
            return Ok(());
        }
        let live = observe(request, agent, binding, Some(&proof["identity"]))?;
        ensure!(
            live["interactive_ready"] == true,
            "Herdr worker changed readiness before send"
        );
        // ACK failure is ambiguous: preserve the permit and observe; never replay.
        let _ = herdr_agents::rpc(
            request,
            vec![
                "agent".into(),
                "prompt".into(),
                crate::records::string(&proof["identity"], "herdr_name")?.into(),
                input,
            ],
        );
        proof
    } else {
        ensure!(
            saved["schema_version"] == 1
                && saved["job_id"] == task
                && saved["execution_instance_id"] == execution
                && saved["request_anchor"] == anchor
                && saved["history_binding"] == json!(binding),
            "invalid durable Herdr dispatch proof"
        );
        saved.clone()
    };
    let baseline: Baseline = serde_json::from_value(proof["baseline"].clone())?;
    let mut history = Reader::new(binding.clone(), baseline, anchor)?;
    let started = Instant::now();
    let timeout = spec["timeout_s"].as_f64().unwrap_or(3600.0);
    let mut completed = false;
    while alive.load(Ordering::Acquire) {
        // Read terminal evidence even if the Agent just exited: a known result
        // remains deliverable, but no next input is allowed without live readiness.
        if !completed {
            if let Some(mut decision) = history.poll()? {
                decision["diagnostics"]["native_execution_instance"] = json!(execution);
                events.send(Event::Completed(decision))?;
                completed = true;
            }
        }
        let live = observe(request, agent, binding, Some(&proof["identity"]))?;
        if completed && live["interactive_ready"] == true {
            events.send(Event::Ready)?;
            return Ok(());
        }
        ensure!(
            started.elapsed().as_secs_f64() < timeout,
            "Herdr task outcome/readiness remains unknown at deadline"
        );
        std::thread::sleep(Duration::from_millis(100));
    }
    Ok(())
}
