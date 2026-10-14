//! Passive local-runtime health, distinct from daemon reachability and model access.
use super::Daemon;
use anyhow::{ensure, Result};
use serde_json::{json, Value};
use std::collections::BTreeMap;

impl Daemon {
    pub(super) fn ping_view(&mut self, p: &Value) -> Result<Value> {
        ensure!(
            p["target"].is_null() || p["target"].is_string(),
            "ping target must be a string"
        );
        let target = p["target"].as_str().unwrap_or("ccbd").trim().to_lowercase();
        if target.is_empty() || target == "ccbd" {
            let mut value = json!({"service":"ccbd","runtime":"rust","project_id":self.config["project_id"],
                "pid":std::process::id(),"status":"running","startup_id":self.config["startup_id"],
                "config_fingerprint":crate::config::fingerprint(&self.config)?,"degraded":!self.faults.is_empty(),
                "mount_state":"mounted","health":if self.faults.is_empty(){"healthy"}else{"degraded"},
                "diagnostics":{"scope":"controller_service_only","reason":"authenticated_request_handled"}});
            if let Some(nonce) = p["bootstrap_probe_nonce"]
                .as_str()
                .map(str::trim)
                .filter(|s| {
                    s.len() == 32
                        && s.bytes()
                            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
                })
            {
                value["bootstrap_probe_nonce"] = json!(nonce);
            }
            return Ok(value);
        }
        let names = self
            .names()
            .into_iter()
            .filter(|name| target == "all" || name == &target)
            .collect::<Vec<_>>();
        ensure!(!names.is_empty(), "unknown ping target");
        let mut agents = Vec::new();
        for name in names {
            let row=self.agent_ping(&name).unwrap_or_else(|_|json!({"project_id":self.config["project_id"],"agent_name":name,
                "provider":self.config["agents"][&name]["provider"],"ccbd_state":"running","mount_state":"unknown","runtime_state":"unknown","health":"unknown",
                "diagnostics":{"scope":"local_runtime","reason":"agent_observation_failed","model_access":"not_checked"}}));
            agents.push(row);
        }
        if target != "all" {
            return Ok(agents.remove(0));
        }
        let mut counts = BTreeMap::<String, usize>::new();
        for agent in &agents {
            *counts
                .entry(agent["health"].as_str().unwrap_or("unknown").to_owned())
                .or_default() += 1;
        }
        Ok(
            json!({"project_id":self.config["project_id"],"ccbd_state":"running","agents":agents,"health_counts":counts}),
        )
    }
    fn agent_ping(&mut self, agent: &str) -> Result<Value> {
        // Observation must remain available when dispatch has been disabled.
        let spec = self.config["agents"][agent].clone();
        ensure!(spec.is_object(), "unknown ping agent");
        let driver = spec["driver_kind"].as_str().unwrap_or("command");
        let owned = self.owned_view(agent);
        let scope_result = self.owned_scope(agent);
        let scope_error = scope_result.is_err();
        let scope = scope_result.unwrap_or(Value::Null);
        let active = self.active_provider_view(agent)?;
        let job = if let Some(id) = active["job_id"].as_str() {
            self.trace_job_record(id)?
        } else {
            Value::Null
        };
        let recovering = !job.is_null()
            && job["provider_options"]["native_daemon_instance"] != self.state
            && driver != "herdr_tui";
        let requests = self
            .codex
            .get(agent)
            .map(|s| s.pending_requests())
            .or_else(|| self.claude.get(agent).map(|s| s.pending_requests()))
            .unwrap_or(json!([]));
        let pending = requests
            .as_array()
            .map(|rows| {
                rows.iter()
                    .filter(|r| {
                        r["status"] == "awaiting_user" || r["status"] == "native_ui_required"
                    })
                    .count()
            })
            .unwrap_or(0);
        let fault = self.faults.get(agent);
        let protocol_job = self
            .pi
            .get(agent)
            .and_then(|s| s.active_job_id())
            .or_else(|| self.codex.get(agent).and_then(|s| s.active_job_id()))
            .or_else(|| self.claude.get(agent).and_then(|s| s.active_job_id()))
            .map(str::to_owned);
        let disabled = spec["dispatch_disabled"] == true;
        let frozen = self.reload_frozen(agent);
        let untracked_activity = active.is_null()
            && (self
                .codex
                .get(agent)
                .is_some_and(|s| s.state["status"]["type"] == "active")
                || self.pi.get(agent).is_some_and(|s| {
                    s.state["isStreaming"] == true || s.state["isCompacting"] == true
                }));
        let protocol_observed = self
            .pi
            .get(agent)
            .map(|s| s.state["sessionId"].as_str().is_some_and(|v| !v.is_empty()))
            .or_else(|| {
                self.codex
                    .get(agent)
                    .map(|s| s.state["thread_id"].as_str().is_some_and(|v| !v.is_empty()))
            })
            .or_else(|| {
                self.claude.get(agent).map(|s| {
                    s.state["initialized"] == true || s.state["control_initialized"] == true
                })
            })
            .unwrap_or(false);
        let mut process = "not_probed";
        if owned["closed"] == true || scope["active_processes"] == 0 {
            process = "gone";
        } else if driver != "herdr_tui" && scope["supported"] == true {
            process = match owned["pid"]
                .as_u64()
                .filter(|n| *n <= u32::MAX as u64)
                .and_then(|pid| crate::owned_process::observe_external_process(pid as u32).ok())
            {
                Some(identity) if identity["created"] == scope["recovery"]["provider_created"] => {
                    "live"
                }
                Some(_) => "identity_mismatch",
                None => "unavailable",
            };
        }
        let (runtime, health, reason) = if fault.is_some() {
            ("failed", "faulted", "provider_fault")
        } else if disabled {
            (
                if !active.is_null() || untracked_activity {
                    "busy"
                } else if process == "live" && owned["ready"] == true {
                    "idle"
                } else if process == "gone" {
                    "stopped"
                } else {
                    "unknown"
                },
                "disabled",
                "dispatch_disabled",
            )
        } else if frozen {
            ("degraded", "blocked", "reload_drain")
        } else if recovering {
            ("degraded", "unknown", "execution_recovery_required")
        } else if !self.activation.ready(agent, &spec) {
            ("starting", "blocked", "awaiting_verified_session")
        } else if driver == "herdr_tui" {
            ("unknown", "unknown", "external_provider_not_freshly_probed")
        } else if !["pi_rpc", "codex_app_server", "claude_stream"].contains(&driver) {
            (
                if active.is_null() { "unknown" } else { "busy" },
                "unknown",
                if spec["command"].is_null() {
                    "external_worker_unobserved"
                } else {
                    "on_demand_worker_unprobed"
                },
            )
        } else if process == "gone" {
            ("stopped", "stopped", "owned_process_gone")
        } else if scope_error || ["unavailable", "identity_mismatch"].contains(&process) {
            ("degraded", "unknown", "process_identity_unavailable")
        } else if owned.is_null() {
            ("starting", "starting", "provider_not_started")
        } else if owned["transport_open"] != true {
            ("degraded", "unknown", "provider_transport_unavailable")
        } else if !protocol_observed {
            ("starting", "starting", "protocol_not_observed")
        } else if protocol_job.as_deref() != active["job_id"].as_str() {
            ("degraded", "unknown", "execution_binding_unconfirmed")
        } else if untracked_activity {
            ("busy", "unknown", "untracked_provider_activity")
        } else if pending > 0 {
            ("busy", "blocked", "provider_user_input")
        } else if process != "live" {
            ("unknown", "unknown", "process_probe_unsupported")
        } else if !active.is_null() {
            ("busy", "healthy", "local_protocol_execution")
        } else if owned["ready"] == true {
            ("idle", "healthy", "local_protocol_ready")
        } else {
            ("starting", "starting", "protocol_initializing")
        };
        let slot = json!({"state_id":self.state,"target_kind":"agent","target_name":agent});
        let mount = if driver == "herdr_tui" || owned.is_null() && spec["command"].is_null() {
            "unknown"
        } else if owned.is_null() || process == "gone" {
            "unmounted"
        } else {
            "mounted"
        };
        let queued = self.kernel.dispatch("scheduler.queued_items_for", &slot)?;
        let session_id = self
            .pi
            .get(agent)
            .map(|s| s.state["sessionId"].clone())
            .or_else(|| self.codex.get(agent).map(|s| s.state["thread_id"].clone()))
            .or_else(|| {
                self.claude
                    .get(agent)
                    .map(|s| s.state["session_id"].clone())
            })
            .unwrap_or(Value::Null);
        Ok(
            json!({"project_id":self.config["project_id"],"agent_name":agent,"provider":spec["provider"],
            "ccbd_state":"running","mount_state":mount,"runtime_state":runtime,"health":health,"active_job_id":active["job_id"],
            "queued_job_count":queued.as_array().map(Vec::len).unwrap_or(0),
            "diagnostics":{"scope":"local_runtime","reason":reason,"driver_kind":driver,"model_access":"not_checked",
                "pid":owned["pid"],"runtime_instance_id":owned["runtime_instance_id"],"session_id":session_id,
                "process_observation":process,"protocol_idle_last_observed":owned["ready"],"pending_request_count":pending,
                "transport_open":owned["transport_open"],"protocol_job_id":protocol_job,
                "protocol_observed":protocol_observed,"activation":self.agent_activation(agent,&spec),
                "untracked_provider_activity":untracked_activity,
                "fault_id":fault.map(|v|v["fault_id"].clone()),"fault_phase":fault.map(|v|v["phase"].clone()),
                "dispatch_disabled":disabled,"reload_frozen":frozen,"admission_blocked":disabled || frozen || fault.is_some(),
                "external_last_observed_at":owned["last_observed_at"]}}),
        )
    }
}
