//! A single Herdr terminal task, observed off the authority actor.
//! Only the actor grants sending, after it commits the exact observation baseline.
use crate::{
    herdr_agents,
    terminal_history::{self, Baseline, Binding, ProviderReader},
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
    Bound(Value),
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
        ["codex", "pi", "claude"].contains(&spec["provider"].as_str().unwrap_or(""))
            && spec["command"].is_null(),
        "Herdr terminal driver requires Codex, Pi or Claude, with no owned command"
    );
    let request = &config["herdr"];
    crate::permission_policy::validate_terminal_binding(request, name, spec)?;
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
    if spec["herdr_discovery"].is_object() {
        ensure!(
            (spec["provider"] == "codex" || spec["provider"] == "claude")
                && spec["herdr_history"].is_null(),
            "deferred history requires unbound Codex or Claude"
        );
        crate::herdr_discovery::validate(&spec["herdr_discovery"], &spec["workspace_path"])?;
        ensure!(
            spec["timeout_s"].is_null()
                || spec["timeout_s"]
                    .as_f64()
                    .is_some_and(|t| t.is_finite() && t > 0.0 && t <= 86400.0),
            "invalid deferred history timeout"
        );
        return Ok(());
    }
    let binding: Binding = serde_json::from_value(spec["herdr_history"].clone())
        .context("invalid Herdr Codex history binding")?;
    binding.validate()?;
    if spec["provider"] == "pi" {
        uuid::Uuid::parse_str(crate::records::string(spec, "pi_bridge_owner")?)?;
        ensure!(
            binding.path == binding.root.join("current.json"),
            "Pi history must name the bridge current.json file"
        );
    }
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
    provider: &str,
    producer: Option<&Value>,
    expected: Option<&Value>,
) -> Result<Value> {
    let live = herdr_agents::bound(request, agent)?;
    ensure!(
        live["agent_session"]["agent"] == provider
            && (live["agent_session"]["kind"] == "id"
                && live["agent_session"]["value"] == binding.session_id
                || provider == "pi"
                    && live["agent_session"]["kind"] == "path"
                    && producer.is_some_and(|p| crate::pi_terminal::same_session_path(
                        &live["agent_session"]["value"],
                        &p["session_path"]
                    ))),
        "Herdr session does not match the declared provider history"
    );
    if let Some(producer) = producer {
        ensure!(
            producer["process"] == live["identity"]["process"],
            "Pi extension belongs to another process lifetime"
        );
    }
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
        let binding: Option<Binding> = if spec["herdr_discovery"].is_object() {
            None
        } else {
            Some(serde_json::from_value(spec["herdr_history"].clone())?)
        };
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
                let result = if let Some(binding) = binding {
                    run(
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
                } else {
                    crate::herdr_discovery::run(
                        &request,
                        &agent,
                        &spec,
                        &task,
                        &execution,
                        &saved,
                        &prompt,
                        &send,
                        &granted,
                        &thread_alive,
                        &mut permitted,
                    )
                };
                if result.is_err() {
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
    let provider = crate::records::string(spec, "provider")?;
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
        let (live, observed_binding, producer) = loop {
            if !alive.load(Ordering::Acquire) {
                return Ok(());
            }
            let (observed_binding, producer) = if provider == "pi" {
                let (resolved, producer) = crate::pi_terminal::resolve(
                    binding,
                    crate::records::string(spec, "pi_bridge_owner")?,
                    agent,
                )?;
                (resolved, Some(producer))
            } else {
                (binding.clone(), None)
            };
            let live = observe(
                request,
                agent,
                &observed_binding,
                provider,
                producer.as_ref(),
                None,
            )?;
            if live["interactive_ready"] == true {
                break (live, observed_binding, producer);
            }
            // Also covers restart after completion was committed but before the
            // previous worker's in-memory readiness barrier was released.
            ensure!(
                waiting.elapsed().as_secs_f64() < spec["timeout_s"].as_f64().unwrap_or(3600.0),
                "Herdr worker did not become ready before the preflight deadline"
            );
            std::thread::sleep(Duration::from_millis(100));
        };
        let baseline = terminal_history::provider_baseline(provider, &observed_binding, false)?;
        ensure!(
            baseline.producer == producer,
            "history producer differs from mounted provider"
        );
        let proof = json!({"schema_version":1,"job_id":task,"execution_instance_id":execution,"request_anchor":anchor,
            "identity":live["identity"],"history_binding":binding,"resolved_history_binding":observed_binding,"baseline":baseline});
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
        if provider == "pi" {
            let (_, current) = crate::pi_terminal::resolve(
                binding,
                crate::records::string(spec, "pi_bridge_owner")?,
                agent,
            )?;
            ensure!(
                Some(current) == producer,
                "Pi bridge reloaded before prompt dispatch"
            );
        }
        let live = observe(
            request,
            agent,
            &observed_binding,
            provider,
            producer.as_ref(),
            Some(&proof["identity"]),
        )?;
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
    let observed_binding: Binding = if proof["resolved_history_binding"].is_null() {
        binding.clone()
    } else {
        serde_json::from_value(proof["resolved_history_binding"].clone())?
    };
    ensure!(
        observed_binding.root == binding.root
            && observed_binding.session_id == binding.session_id
            && observed_binding.cwd == binding.cwd,
        "resolved history scope changed"
    );
    let producer = baseline.producer.clone();
    ensure!(
        (provider == "pi") == producer.is_some(),
        "history authority does not match provider"
    );
    if let Some(p) = &producer {
        ensure!(
            p["owner"] == spec["pi_bridge_owner"],
            "Pi bridge owner changed"
        );
    }
    let mut history = ProviderReader::new(provider, observed_binding.clone(), baseline, anchor)?;
    let started = Instant::now();
    let timeout = spec["timeout_s"].as_f64().unwrap_or(3600.0);
    let mut completed = false;
    while alive.load(Ordering::Acquire) {
        // Read terminal evidence even if the Agent just exited: a known result
        // remains deliverable, but no next input is allowed without live readiness.
        if !completed {
            if let Some(mut decision) = history.poll()? {
                if provider == "claude" {
                    let live = observe(
                        request,
                        agent,
                        &observed_binding,
                        provider,
                        None,
                        Some(&proof["identity"]),
                    )?;
                    if live["interactive_ready"] != true {
                        ensure!(
                            started.elapsed().as_secs_f64() < timeout,
                            "Claude transcript candidate is not idle"
                        );
                        std::thread::sleep(Duration::from_millis(100));
                        continue;
                    }
                    let Some(latest) = history.poll()? else {
                        continue;
                    };
                    decision = latest;
                    crate::herdr_terminal_evidence::attach(&mut decision, &live, &observed_binding);
                }
                decision["diagnostics"]["native_execution_instance"] = json!(execution);
                events.send(Event::Completed(decision))?;
                completed = true;
            }
        }
        let live = observe(
            request,
            agent,
            &observed_binding,
            provider,
            producer.as_ref(),
            Some(&proof["identity"]),
        )?;
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
