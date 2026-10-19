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
    /// A context clear started a new provider session.
    Rotated(Value),
    Completed(Value),
    Ready,
    Fault,
    Unsent,
}
#[derive(Default)]
struct PollMetrics {
    history_polls: u64,
    history_ms: u64,
    live_observations: u64,
    live_ms: u64,
}
impl PollMetrics {
    fn poll(&mut self, history: &mut ProviderReader) -> Result<Option<Value>> {
        let started = Instant::now();
        let result = history.poll();
        self.history_polls += 1;
        self.history_ms += started.elapsed().as_millis() as u64;
        result
    }
    fn observe(&mut self, action: impl FnOnce() -> Result<Value>) -> Result<Value> {
        let started = Instant::now();
        let result = action();
        self.live_observations += 1;
        self.live_ms += started.elapsed().as_millis() as u64;
        result
    }
    fn snapshot(&self) -> Value {
        json!({"history_polls":self.history_polls,"history_poll_ms":self.history_ms,
            "live_observations":self.live_observations,"live_observation_ms":self.live_ms,
            "history_interval_ms":100,"busy_liveness_interval_ms":1000,
            "scope":"until_terminal_observation"})
    }
}
pub(crate) struct Worker {
    pub events: mpsc::Receiver<Event>,
    pub grant: mpsc::Sender<()>,
    pub view: Value,
    pub job: String,
    alive: Arc<AtomicBool>,
    interrupt: Arc<AtomicBool>,
}
impl Worker {
    /// Run one native context control (clear/compact) in the terminal.
    pub fn spawn_context(request: Value, agent: String, spec: Value, job: &Value) -> Result<Self> {
        let job_id = crate::records::string(job, "job_id")?.to_owned();
        let execution =
            crate::records::string(&job["provider_options"], "native_execution_instance")?
                .to_owned();
        let action =
            crate::records::string(&job["provider_options"]["native_context_control"], "action")?
                .to_owned();
        crate::herdr_context::command(crate::records::string(&spec, "provider")?, &action)?;
        let saved = job["provider_options"]["herdr_dispatch"].clone();
        let (send, events) = mpsc::channel();
        let (grant, granted) = mpsc::channel();
        let alive = Arc::new(AtomicBool::new(true));
        let thread_alive = Arc::clone(&alive);
        let task = job_id.clone();
        std::thread::Builder::new()
            .name(format!("herdr-context-{agent}"))
            .spawn(move || {
                let mut permitted = !saved.is_null();
                let result = crate::herdr_context::run(
                    &request,
                    &agent,
                    &spec,
                    &task,
                    &execution,
                    &action,
                    &saved,
                    &send,
                    &granted,
                    &thread_alive,
                    &mut permitted,
                );
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
            interrupt: Arc::new(AtomicBool::new(false)),
            view: json!({"external":true,"closed":false,"ready":false,"pid":null,"runtime_instance_id":null}),
        })
    }
    /// Ask the observer to end the provider's current turn once.
    pub(crate) fn interrupt(&self) {
        self.interrupt.store(true, Ordering::Release);
    }
}
/// Esc ends a terminal provider's current turn. Its history then records the
/// outcome; a turn that finished first still completes. Only if history never
/// classifies the turn does a sustained idle terminal confirm cancellation.
pub(crate) struct Interrupt {
    requested: Arc<AtomicBool>,
    sent_at: Option<Instant>,
    idle_since: Option<Instant>,
}
impl Interrupt {
    fn new(requested: Arc<AtomicBool>) -> Self {
        Self {
            requested,
            sent_at: None,
            idle_since: None,
        }
    }
    pub(crate) fn send(&mut self, request: &Value, identity: &Value) -> Result<()> {
        if self.sent_at.is_none() && self.requested.load(Ordering::Acquire) {
            self.sent_at = Some(Instant::now());
            let _ = herdr_agents::rpc(
                request,
                vec![
                    "agent".into(),
                    "send-keys".into(),
                    crate::records::string(identity, "herdr_name")?.into(),
                    "esc".into(),
                ],
            );
        }
        Ok(())
    }
    pub(crate) fn settled(&mut self, live: &Value) -> Option<Value> {
        let sent = self.sent_at?;
        if live["interactive_ready"] != true {
            self.idle_since = None;
            return None;
        }
        let idle = *self.idle_since.get_or_insert_with(Instant::now);
        (sent.elapsed() >= Duration::from_secs(5) && idle.elapsed() >= Duration::from_secs(2)).then(|| {
            json!({"schema_version":2,"record_type":"completion_decision","terminal":true,"status":"cancelled",
                "reason":"herdr_terminal_interrupted","confidence":"observed","anchor_seen":false,"reply_started":false,
                "reply_stable":false,"reply":"","provider_turn_ref":null,"source_cursor":null,"finished_at":crate::daemon::now(),
                "diagnostics":{"completion_authority":"herdr_interrupt_idle","interrupt_sent":true}})
        })
    }
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
    if crate::herdr_activation::managed(spec) {
        ensure!(
            config["herdr_activation"]["namespace_owner"].is_string()
                && config["herdr_activation"]["workspace_id"].is_string()
                && spec["herdr_history"].is_null()
                && spec["herdr_discovery"].is_null(),
            "managed terminal requires a runtime activation scope, not a configuration binding"
        );
        return Ok(());
    }
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
        let interrupt = Arc::new(AtomicBool::new(false));
        let mut thread_interrupt = Interrupt::new(Arc::clone(&interrupt));
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
                        &mut thread_interrupt,
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
                        &mut thread_interrupt,
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
            interrupt,
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
    interrupt: &mut Interrupt,
    permitted: &mut bool,
) -> Result<()> {
    let dispatch_started = Instant::now();
    let mut dispatch_timings = json!({"resumed":!saved.is_null()});
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
        dispatch_timings["preflight_ms"] = json!(dispatch_started.elapsed().as_millis() as u64);
        let grant_started = Instant::now();
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
        dispatch_timings["permit_wait_ms"] = json!(grant_started.elapsed().as_millis() as u64);
        let recheck_started = Instant::now();
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
        dispatch_timings["recheck_ms"] = json!(recheck_started.elapsed().as_millis() as u64);
        let prompt_started = Instant::now();
        // Pi takes the prompt through its bridge, outside the editor, so a
        // person's unsent draft is never merged into it. Older bridges type.
        let inbox = match &producer {
            Some(producer) => crate::pi_terminal::deliver(
                binding,
                crate::records::string(spec, "pi_bridge_owner")?,
                agent,
                producer,
                execution,
                &anchor,
                &input,
                alive,
                waiting + Duration::from_secs_f64(spec["timeout_s"].as_f64().unwrap_or(3600.0)),
            )?,
            None => crate::pi_terminal::Inbox::Unsupported,
        };
        match inbox {
            crate::pi_terminal::Inbox::Claimed => {}
            crate::pi_terminal::Inbox::Withdrawn => return Ok(()),
            crate::pi_terminal::Inbox::Unsupported => {
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
            }
        }
        dispatch_timings["prompt_inbox"] = json!(inbox == crate::pi_terminal::Inbox::Claimed);
        dispatch_timings["prompt_rpc_ms"] = json!(prompt_started.elapsed().as_millis() as u64);
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
        if provider == "pi" {
            // A request the previous worker left unclaimed must never reach Pi later.
            crate::pi_terminal::withdraw_pending(binding, execution)?;
        }
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
    let mut next_live_observation = Instant::now();
    let mut metrics = PollMetrics::default();
    while alive.load(Ordering::Acquire) {
        if !completed {
            interrupt.send(request, &proof["identity"])?;
        }
        // Read terminal evidence even if the Agent just exited: a known result
        // remains deliverable, but no next input is allowed without live readiness.
        if !completed {
            if let Some(mut decision) = metrics.poll(&mut history)? {
                if provider == "claude" {
                    let live = metrics.observe(|| {
                        observe(
                            request,
                            agent,
                            &observed_binding,
                            provider,
                            None,
                            Some(&proof["identity"]),
                        )
                    })?;
                    if live["interactive_ready"] != true {
                        ensure!(
                            started.elapsed().as_secs_f64() < timeout,
                            "Claude transcript candidate is not idle"
                        );
                        std::thread::sleep(Duration::from_millis(100));
                        continue;
                    }
                    let Some(latest) = metrics.poll(&mut history)? else {
                        continue;
                    };
                    decision = latest;
                    crate::herdr_terminal_evidence::attach(&mut decision, &live, &observed_binding);
                }
                decision["diagnostics"]["native_execution_instance"] = json!(execution);
                decision["diagnostics"]["herdr_polling"] = metrics.snapshot();
                decision["diagnostics"]["herdr_dispatch_timings"] = dispatch_timings.clone();
                events.send(Event::Completed(decision))?;
                completed = true;
            }
        }
        // Native completion stays on the fast history cadence. Busy liveness
        // checks are bounded separately; completion and every new dispatch
        // still require fresh identity/readiness observations.
        if completed || Instant::now() >= next_live_observation {
            let live = metrics.observe(|| {
                observe(
                    request,
                    agent,
                    &observed_binding,
                    provider,
                    producer.as_ref(),
                    Some(&proof["identity"]),
                )
            })?;
            next_live_observation = Instant::now() + Duration::from_secs(1);
            if !completed {
                if let Some(mut decision) = interrupt.settled(&live) {
                    decision["diagnostics"]["native_execution_instance"] = json!(execution);
                    events.send(Event::Completed(decision))?;
                    completed = true;
                    continue;
                }
            }
            if completed && live["interactive_ready"] == true {
                events.send(Event::Ready)?;
                return Ok(());
            }
        }
        ensure!(
            started.elapsed().as_secs_f64() < timeout,
            "Herdr task outcome/readiness remains unknown at deadline"
        );
        std::thread::sleep(Duration::from_millis(100));
    }
    Ok(())
}
