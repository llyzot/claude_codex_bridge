//! Runtime session bindings are evidence, not agent configuration. The complete
//! roster can admit durable work before any terminal is ready. Only the target's
//! verified binding permits execution; results may be queued for an offline peer.
use anyhow::{ensure, Context, Result};
use serde_json::{json, Value};
use std::{
    collections::HashMap,
    path::Path,
    time::{Duration, Instant},
};

pub(crate) fn managed(spec: &Value) -> bool {
    spec["herdr_binding_mode"] == "managed"
}

pub(crate) fn effective(spec: &Value, binding: &Value) -> Value {
    let mut result = spec.clone();
    result.as_object_mut().unwrap().remove("herdr_binding_mode");
    result["herdr_history"] = binding["history"].clone();
    result["herdr_discovery"] = binding["discovery"].clone();
    if spec["provider"] == "pi" {
        result["pi_bridge_owner"] = binding["pi_bridge_owner"].clone();
    }
    result
}

pub(crate) fn historical(config: &Value, bindings: &Value) -> Value {
    let mut result = config.clone();
    for (name, spec) in result["agents"].as_object_mut().unwrap() {
        if managed(spec) && bindings[name].is_object() {
            *spec = effective(spec, &bindings[name]);
        }
    }
    result
}

pub(crate) fn blocked_sessions(sessions: &Value) -> Value {
    sessions
        .as_object()
        .map(|sessions| {
            sessions
                .iter()
                .filter_map(|(name, s)| {
                    s["blocked_reason"]
                        .as_str()
                        .map(|reason| (name.clone(), json!(reason)))
                })
                .collect::<serde_json::Map<String, Value>>()
        })
        .unwrap_or_default()
        .into()
}

pub(crate) struct State {
    bindings: HashMap<String, Value>,
    held: HashMap<String, String>,
    errors: HashMap<String, String>,
    next_poll: Instant,
}
impl Default for State {
    fn default() -> Self {
        Self {
            bindings: HashMap::new(),
            held: HashMap::new(),
            errors: HashMap::new(),
            next_poll: Instant::now(),
        }
    }
}
impl State {
    pub(crate) fn new(config: &Value) -> Self {
        let held = config["herdr_activation"]["blocked_agents"]
            .as_object()
            .map(|m| {
                m.iter()
                    .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.into())))
                    .collect()
            })
            .unwrap_or_default();
        Self {
            held,
            ..Self::default()
        }
    }
    pub(crate) fn ready(&self, name: &str, spec: &Value) -> bool {
        !self.held.contains_key(name) && (!managed(spec) || self.bindings.contains_key(name))
    }
    pub(crate) fn held(&self, name: &str) -> bool {
        self.held.contains_key(name)
    }
    pub(crate) fn spec(&self, name: &str, spec: &Value) -> Value {
        match self.bindings.get(name) {
            Some(binding) if managed(spec) => effective(spec, binding),
            _ => spec.clone(),
        }
    }
    pub(crate) fn view(&self, name: &str, spec: &Value) -> Value {
        if let Some(reason) = self.held.get(name) {
            return json!({"registered":true,"session_verified":false,"session_gate_open":false,"reason":"recovery_evidence_required","error":reason});
        }
        if !managed(spec) {
            return Value::Null;
        }
        json!({"registered":true,"session_verified":self.ready(name,spec),
            "session_gate_open":self.ready(name,spec) && spec["dispatch_disabled"] != true,
            "reason":if self.ready(name,spec) {"session_verified"} else if self.errors.contains_key(name) {"session_rejected"} else {"awaiting_session"},
            "error":self.errors.get(name)})
    }
    pub(crate) fn refresh(&mut self, runtime: &Path, config: &Value) -> Result<()> {
        if Instant::now() < self.next_poll {
            return Ok(());
        }
        self.next_poll = Instant::now() + Duration::from_millis(200);
        self.held = config["herdr_activation"]["blocked_agents"]
            .as_object()
            .map(|m| {
                m.iter()
                    .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.into())))
                    .collect()
            })
            .unwrap_or_default();
        let names: Vec<_> = config["agents"]
            .as_object()
            .unwrap()
            .iter()
            .filter(|(_, spec)| managed(spec))
            .map(|(name, _)| name.clone())
            .collect();
        if names.is_empty() {
            return Ok(());
        }
        let root = runtime.join("herdr");
        let open = crate::startup::load(&root.join("herdr-open.json"))?.unwrap_or(Value::Null);
        let namespace = crate::startup::load(&root.join("namespace.json"))?.unwrap_or(Value::Null);
        let agents = crate::startup::load(&root.join("herdr-agents.json"))?.unwrap_or(Value::Null);
        for name in names {
            let spec = &config["agents"][&name];
            let binding = &open["bindings"][&name];
            // Epoch mismatch invalidates evidence even if its native session id
            // was reused. No stale binding can release a task in a new namespace.
            let scope_ok = namespace["owner_id"] == config["herdr_activation"]["namespace_owner"]
                && namespace["workspace_id"] == config["herdr_activation"]["workspace_id"]
                && namespace["status"] == "prepared"
                && open["project_id"] == config["project_id"]
                && open["workspace_id"] == namespace["workspace_id"]
                && agents["namespace_owner"] == namespace["owner_id"];
            if !scope_ok || !binding.is_object() {
                self.bindings.remove(&name);
                if let Some(error) = open["binding_errors"][&name].as_str() {
                    self.errors.insert(name, error.into());
                }
                continue;
            }
            if self.bindings.get(&name) == Some(binding) {
                continue;
            }
            let checked = (|| -> Result<()> {
                let row = &agents["agents"][&name];
                let process = &row["binding"]["process"];
                let pid = process["pid"]
                    .as_u64()
                    .filter(|p| *p > 0 && *p <= u32::MAX as u64)
                    .context("terminal session process is unconfirmed")?;
                ensure!(
                    crate::owned_process::observe_external_process(pid as u32)? == *process,
                    "terminal session process lifetime changed"
                );
                crate::herdr_open::verify_activation(&root, &open, spec, binding, &name)?;
                crate::herdr_worker::validate(config, &name, &effective(spec, binding))
            })();
            match checked {
                Ok(()) => {
                    self.bindings.insert(name.clone(), binding.clone());
                    self.errors.remove(&name);
                }
                Err(error) => {
                    self.bindings.remove(&name);
                    self.errors.insert(name, error.to_string());
                }
            }
        }
        Ok(())
    }
}
