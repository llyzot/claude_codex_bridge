//! Native context controls for Herdr terminal agents. The provider's own slash
//! command runs only after the authority commits a permit; a restarted
//! observer never resends it. Clearing starts a new provider session, so the
//! observed successor becomes a durable rotation of the opener's binding.
use crate::{herdr_agents, herdr_worker::Event, terminal_history::Binding};
use anyhow::{bail, ensure, Result};
use serde_json::{json, Value};
use std::{
    path::Path,
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    time::{Duration, Instant},
};

pub(crate) fn command(provider: &str, action: &str) -> Result<&'static str> {
    Ok(match (provider, action) {
        ("claude", "clear") => "/clear",
        // Pi has no /clear; /new starts a fresh session in the same process.
        ("pi", "clear") => "/new",
        ("claude" | "pi", "compact") => "/compact",
        _ => bail!("no verified terminal context command for {provider} {action}"),
    })
}

fn decision(status: &str, reason: &str, diagnostics: Value) -> Value {
    json!({"schema_version":2,"record_type":"completion_decision","terminal":true,"status":status,
        "reason":reason,"confidence":"observed","anchor_seen":false,"reply_started":false,"reply_stable":false,
        "reply":"","provider_turn_ref":null,"source_cursor":null,"finished_at":crate::daemon::now(),
        "diagnostics":diagnostics})
}

/// The provider's current native session as the terminal reports it now.
fn session(provider: &str, spec: &Value, live: &Value) -> Result<Value> {
    if provider == "pi" {
        let history: Binding = serde_json::from_value(spec["herdr_history"].clone())?;
        let state: Value =
            serde_json::from_slice(&std::fs::read(history.root.join("current.json"))?)?;
        ensure!(
            state["owner"] == spec["pi_bridge_owner"] && state["agent_name"].is_string(),
            "Pi current state belongs to another bridge"
        );
        return Ok(state["session_id"].clone());
    }
    ensure!(
        live["agent_session"]["agent"] == provider && live["agent_session"]["kind"] == "id",
        "terminal does not report its native session"
    );
    Ok(live["agent_session"]["value"].clone())
}

/// The successor transcript under the binding's own root (the root itself or
/// one project directory below it), keeping that root unchanged.
fn claude_history(spec: &Value, session_id: &str) -> Result<Binding> {
    uuid::Uuid::parse_str(session_id)?;
    let (root, cwd) = if spec["herdr_discovery"].is_object() {
        let d = &spec["herdr_discovery"];
        (
            Path::new(crate::records::string(d, "root")?).to_path_buf(),
            d["cwd"].clone(),
        )
    } else {
        let h: Binding = serde_json::from_value(spec["herdr_history"].clone())?;
        (h.root, json!(h.cwd))
    };
    let name = format!("{session_id}.jsonl");
    let mut found: Vec<_> = Some(root.join(&name))
        .into_iter()
        .filter(|p| p.is_file())
        .collect();
    for entry in std::fs::read_dir(&root)? {
        let dir = entry?.path();
        if dir.is_dir() && dir.join(&name).is_file() {
            found.push(dir.join(&name));
        }
    }
    ensure!(
        found.len() == 1,
        "new Claude transcript is missing or ambiguous"
    );
    let binding: Binding = serde_json::from_value(
        json!({"root":root,"path":found.pop().unwrap(),"cwd":cwd,"session_id":session_id}),
    )?;
    binding.validate()?;
    crate::terminal_history::provider_baseline("claude", &binding, true)?;
    Ok(binding)
}

/// The successor binding and, for Claude, its exact transcript cache.
fn rotation(
    provider: &str,
    agent: &str,
    spec: &Value,
    identity: &Value,
    to: &str,
) -> Result<Value> {
    if provider == "pi" {
        let mut history = spec["herdr_history"].clone();
        history["session_id"] = json!(to);
        let binding: Binding = serde_json::from_value(history.clone())?;
        crate::pi_terminal::resolve(
            &binding,
            crate::records::string(spec, "pi_bridge_owner")?,
            agent,
        )?;
        return Ok(json!({"to_session":to,
            "binding":{"history":history,"pi_bridge_owner":spec["pi_bridge_owner"]}}));
    }
    let history = claude_history(spec, to)?;
    if spec["herdr_discovery"].is_object() {
        // A deferred discovery binding stays deferred; its cache carries the
        // successor transcript. A session-scoped one names the new session.
        let mut discovery = spec["herdr_discovery"].clone();
        if !discovery["session_id"].is_null() {
            discovery["session_id"] = json!(to);
        }
        return Ok(json!({"to_session":to,"binding":{"discovery":discovery},
            "cache":{"schema_version":1,"identity":identity,"discovery":discovery,"history":history}}));
    }
    Ok(json!({"to_session":to,"binding":{"history":history},
        "cache":{"schema_version":1,"identity":identity,"discovery":null,"history":history}}))
}

fn ready(
    request: &Value,
    agent: &str,
    alive: &AtomicBool,
    events: &mpsc::Sender<Event>,
) -> Result<()> {
    let started = Instant::now();
    while alive.load(Ordering::Acquire) {
        if herdr_agents::bound(request, agent)?["interactive_ready"] == true {
            events.send(Event::Ready)?;
            return Ok(());
        }
        ensure!(
            started.elapsed() < Duration::from_secs(600),
            "terminal did not return to readiness after context control"
        );
        std::thread::sleep(Duration::from_millis(300));
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn run(
    request: &Value,
    agent: &str,
    spec: &Value,
    task: &str,
    execution: &str,
    action: &str,
    saved: &Value,
    events: &mpsc::Sender<Event>,
    granted: &mpsc::Receiver<()>,
    alive: &AtomicBool,
    permitted: &mut bool,
) -> Result<()> {
    let provider = crate::records::string(spec, "provider")?;
    let command = command(provider, action)?;
    if !saved.is_null() {
        // A previous observer may already have sent the command; never resend.
        // A clear whose successor session is already observable still rotates.
        let before = &saved["before_session"];
        let started = Instant::now();
        while action == "clear"
            && alive.load(Ordering::Acquire)
            && started.elapsed() < Duration::from_secs(30)
        {
            let live = herdr_agents::bound(request, agent)?;
            let now = session(provider, spec, &live)?;
            if now.is_string() && now != *before && live["identity"] == saved["identity"] {
                if let Ok(next) = rotation(
                    provider,
                    agent,
                    spec,
                    &saved["identity"],
                    now.as_str().unwrap(),
                ) {
                    events.send(Event::Rotated(next))?;
                    events.send(Event::Completed(decision("completed", "herdr_context_cleared", json!({
                        "automatic_replay":false,"context_command":command,"before_session":before,
                        "after_session":now,"session_rotated":true,"confirmed":"session_rotated_after_restart",
                        "native_execution_instance":execution}))))?;
                    return ready(request, agent, alive, events);
                }
            }
            std::thread::sleep(Duration::from_millis(300));
        }
        events.send(Event::Completed(decision("incomplete", "herdr_context_restarted",
            json!({"automatic_replay":false,"context_command":command,"native_execution_instance":execution}))))?;
        return ready(request, agent, alive, events);
    }
    let started = Instant::now();
    let live = loop {
        if !alive.load(Ordering::Acquire) {
            return Ok(());
        }
        let live = herdr_agents::bound(request, agent)?;
        if live["interactive_ready"] == true {
            break live;
        }
        ensure!(
            started.elapsed() < Duration::from_secs(60),
            "terminal did not become idle for context control"
        );
        std::thread::sleep(Duration::from_millis(200));
    };
    let before = session(provider, spec, &live)?;
    let proof = json!({"schema_version":1,"job_id":task,"execution_instance_id":execution,
        "request_anchor":format!("{task}:{execution}"),"identity":live["identity"],
        "context_command":command,"before_session":before});
    events.send(Event::Prepared(proof.clone()))?;
    loop {
        if !alive.load(Ordering::Acquire) {
            return Ok(());
        }
        match granted.recv_timeout(Duration::from_millis(100)) {
            Ok(()) => break,
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(_) => return Ok(()),
        }
    }
    *permitted = true;
    let live = herdr_agents::bound(request, agent)?;
    ensure!(
        live["identity"] == proof["identity"] && live["interactive_ready"] == true,
        "terminal changed before context control"
    );
    let name = crate::records::string(&proof["identity"], "herdr_name")?;
    // ACK loss is ambiguous: observe, never resend.
    let _ = herdr_agents::rpc(
        request,
        vec!["agent".into(), "prompt".into(), name.into(), command.into()],
    );
    let sent = Instant::now();
    let (mut busy_seen, mut idle_since) = (false, None::<Instant>);
    let diagnostics = |extra: Value| {
        let mut d = json!({"context_command":command,"before_session":before,
            "native_execution_instance":execution,"automatic_replay":false});
        for (k, v) in extra.as_object().unwrap() {
            d[k] = v.clone();
        }
        d
    };
    while alive.load(Ordering::Acquire) {
        let live = herdr_agents::bound(request, agent)?;
        ensure!(
            live["identity"] == proof["identity"],
            "terminal process changed during context control"
        );
        if live["interactive_ready"] == true {
            idle_since.get_or_insert_with(Instant::now);
        } else {
            busy_seen = true;
            idle_since = None;
        }
        let idle = idle_since.map(|t| t.elapsed()).unwrap_or_default();
        let now = session(provider, spec, &live)?;
        let rotated = now.is_string() && now != before;
        let settled = idle >= Duration::from_millis(1500)
            && (busy_seen || rotated || sent.elapsed() >= Duration::from_secs(3));
        if settled && (action == "compact" || rotated) {
            if rotated {
                // The terminal may report the new session before its
                // transcript is complete; keep observing until it is.
                match rotation(
                    provider,
                    agent,
                    spec,
                    &proof["identity"],
                    now.as_str().unwrap(),
                ) {
                    Ok(next) => events.send(Event::Rotated(next))?,
                    Err(_) if sent.elapsed() < Duration::from_secs(30) => {
                        std::thread::sleep(Duration::from_millis(300));
                        continue;
                    }
                    Err(error) => return Err(error),
                }
            }
            let reason = if action == "clear" {
                "herdr_context_cleared"
            } else {
                "herdr_context_compacted"
            };
            events.send(Event::Completed(decision("completed", reason, diagnostics(json!({
                "after_session":now,"session_rotated":rotated,
                "confirmed":if rotated {"session_rotated"} else if busy_seen {"terminal_returned_idle"} else {"input_delivered"}})))))?;
            events.send(Event::Ready)?;
            return Ok(());
        }
        let limit = if action == "clear" { 30 } else { 600 };
        if sent.elapsed() >= Duration::from_secs(limit) {
            let reason = if action == "clear" {
                "herdr_context_clear_unconfirmed"
            } else {
                "herdr_context_compact_unconfirmed"
            };
            events.send(Event::Completed(decision(
                "incomplete",
                reason,
                diagnostics(json!({"after_session":now})),
            )))?;
            return ready(request, agent, alive, events);
        }
        std::thread::sleep(Duration::from_millis(300));
    }
    Ok(())
}
