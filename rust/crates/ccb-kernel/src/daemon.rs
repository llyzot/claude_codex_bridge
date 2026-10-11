//! Public API-2 actor and autonomous queue execution. Provider processes are
//! runtime-owned handles; this module never controls an external Agent.
use crate::{
    admission, bureau, callback, callback_recovery, finalization, records, result_policy, retry,
    Kernel,
};
use anyhow::{bail, ensure, Context, Result};
use chrono::{DateTime, SecondsFormat, Utc};
use serde_json::{json, Value};
use std::{
    collections::HashMap,
    io::{BufRead, BufReader, Read, Write},
    process::{Child, Command, Stdio},
    sync::mpsc,
    time::{Instant, SystemTime, UNIX_EPOCH},
};

#[path = "daemon_context.rs"]
mod context;
#[path = "project_view.rs"]
mod project_view;
#[path = "daemon_reload.rs"]
mod reload;

pub fn validate_startup(kernel: &mut Kernel, config: &Value) -> Result<()> {
    reload::startup_guard(kernel, config)
}

pub fn now() -> String {
    let time = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    DateTime::<Utc>::from_timestamp(time.as_secs() as i64, time.subsec_nanos())
        .unwrap()
        .to_rfc3339_opts(SecondsFormat::AutoSi, true)
}
pub fn verify_owner(root: &std::path::Path) -> Result<()> {
    let raw = match std::fs::read(root.join("lease.json")) {
        Ok(v) => v,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e.into()),
    };
    let lease: Value = serde_json::from_slice(&raw)?;
    if lease["mount_state"] != "unmounted" {
        let pid = lease["ccbd_pid"].as_u64().unwrap_or(0);
        ensure!(
            pid > 0 && pid <= u32::MAX as u64 && owner_stopped(pid as u32),
            "existing daemon ownership is live or cannot be proven stopped"
        );
    }
    Ok(())
}
struct Running {
    child: Child,
    job: Value,
    output: mpsc::Receiver<DriverEvent>,
    result: Option<Value>,
    started: Instant,
    exited: Option<bool>,
    eof: bool,
}
enum DriverEvent {
    Record(Value),
    Error,
    End,
}
pub struct Daemon {
    pub kernel: Kernel,
    config: Value,
    known_names: Vec<String>,
    reload: Option<reload::Pending>,
    state: String,
    running: HashMap<String, Running>,
    pi: HashMap<String, crate::pi_rpc::Session>,
    codex: HashMap<String, crate::codex_rpc::Session>,
    claude: HashMap<String, crate::claude_stream::Session>,
    faults: HashMap<String, Value>,
    pub stopping: bool,
    heartbeat: Instant,
    view_sequence: u64,
    view_digest: String,
}
impl Drop for Daemon {
    fn drop(&mut self) {
        for running in self.running.values_mut() {
            let _ = running.child.kill();
            let _ = running.child.wait();
        }
    }
}
impl Daemon {
    pub fn validate_config(config: &Value) -> Result<()> {
        crate::workspaces::validate(config)?;
        ensure!(
            config["schema_version"] == 2 && !records::string(config, "project_id")?.is_empty(),
            "invalid native daemon config"
        );
        let agents = config["agents"]
            .as_object()
            .context("daemon agents must be an object")?;
        ensure!(
            !agents.is_empty() && agents.len() <= 32,
            "daemon requires 1 to 32 agents"
        );
        for (name, spec) in agents {
            crate::models::validate_agent(name)?;
            crate::roles::normalized(&spec["role"])?;
            ensure!(
                !records::string(spec, "provider")?.is_empty(),
                "empty agent provider"
            );
            ensure!(
                spec["env"].is_null()
                    || spec["env"]
                        .as_object()
                        .is_some_and(|env| env.values().all(Value::is_string)),
                "native provider environment must contain string values"
            );
            ensure!(
                spec["command"].is_null() || spec["command"].is_array(),
                "command must be an argv array"
            );
            ensure!(
                spec["driver_kind"].is_null()
                    || ["command", "pi_rpc", "codex_app_server", "claude_stream"]
                        .contains(&spec["driver_kind"].as_str().unwrap_or("")),
                "unknown native driver kind"
            );
            if spec["driver_kind"] == "pi_rpc" {
                ensure!(
                    spec["provider"] == "pi" && spec["command"].is_array(),
                    "Pi RPC driver requires pi provider and command argv"
                );
            }
            ensure!(
                spec["profile_family"].is_null()
                    || [
                        "protocol_turn",
                        "structured_result",
                        "session_boundary",
                        "anchored_session_stability",
                        "terminal_text_quiet"
                    ]
                    .contains(&spec["profile_family"].as_str().unwrap_or("")),
                "invalid native completion family"
            );
            if let Some(argv) = spec["command"].as_array() {
                if spec["driver_kind"] == "codex_app_server" {
                    ensure!(
                        spec["provider"] == "codex",
                        "Codex app-server requires codex provider"
                    );
                }
                if spec["driver_kind"] == "claude_stream" {
                    ensure!(
                        spec["provider"] == "claude",
                        "Claude stream requires claude provider"
                    );
                }
                ensure!(
                    !argv.is_empty()
                        && argv.iter().all(Value::is_string)
                        && !argv[0].as_str().unwrap().is_empty(),
                    "invalid command driver argv"
                );
            }
        }
        Ok(())
    }
    pub fn new(mut kernel: Kernel, config: Value, generation: &str) -> Result<Self> {
        Self::validate_config(&config)?;
        let agents = config["agents"].as_object().unwrap();
        let prior = kernel.store.json("lease.json")?.unwrap_or(Value::Null);
        if !prior.is_null() && prior["mount_state"] != "unmounted" {
            let pid = prior["ccbd_pid"].as_u64().unwrap_or(0);
            ensure!(
                pid > 0 && pid <= u32::MAX as u64 && owner_stopped(pid as u32),
                "existing daemon ownership is live or cannot be proven stopped"
            );
        }
        let state = format!("daemon-{generation}");
        let names = agents.keys().cloned().collect::<Vec<_>>();
        kernel.dispatch(
            "scheduler.create",
            &json!({"state_id":state,"agent_names":names}),
        )?;
        let mut rows = json!({});
        let mut order = json!({});
        for agent in &names {
            rows[agent] = records::dispatch(
                &mut kernel.store,
                "list",
                &json!({"stream":"jobs","agent_name":agent}),
            )?;
            let inbox = kernel.dispatch("inbox.list", &json!({"agent_name":agent}))?;
            order[agent] = json!(inbox
                .as_array()
                .unwrap()
                .iter()
                .filter(|e| ["created", "queued", "delivering"]
                    .contains(&e["status"].as_str().unwrap_or("")))
                .filter_map(|e| payload(&e["payload_ref"], "delivery:")
                    .or_else(|| payload(&e["payload_ref"], "job:")))
                .collect::<Vec<_>>());
        }
        kernel.dispatch(
            "scheduler.rebuild",
            &json!({"state_id":state,"agent_names":names,"records":rows,"mailbox_order":order}),
        )?;
        let mut faults = HashMap::new();
        for agent in &names {
            let provider = &config["agents"][agent]["provider"];
            if ["pi", "codex", "claude"].contains(&provider.as_str().unwrap_or("")) {
                if let Some(row) = kernel.store.json(&crate::provider_fault::path(
                    provider.as_str().unwrap(),
                    agent,
                ))? {
                    crate::provider_fault::validate(&row, provider, agent, &config["project_id"])?;
                    if row["status"] == "blocked" {
                        faults.insert(agent.clone(), row);
                    }
                }
            }
        }
        Ok(Self {
            kernel,
            config,
            known_names: names,
            reload: None,
            state,
            running: HashMap::new(),
            pi: HashMap::new(),
            codex: HashMap::new(),
            claude: HashMap::new(),
            faults,
            stopping: false,
            heartbeat: Instant::now(),
            view_sequence: 0,
            view_digest: String::new(),
        })
    }
    pub fn mount(&mut self, endpoint: &Value) -> Result<()> {
        self.initialize_configuration()?;
        let timestamp = now();
        let prior = self.kernel.store.json("lease.json")?.unwrap_or(Value::Null);
        let generation = prior["generation"].as_u64().unwrap_or(0) + 1;
        let address = format!(
            "{}:{}",
            endpoint["host"].as_str().unwrap_or("127.0.0.1"),
            endpoint["port"]
        );
        let descriptor = json!({"kind":"tcp_loopback","host":endpoint["host"],"port":endpoint["port"],"address":address,"display":address,"legacy_socket_path":null,"token_ref":endpoint["token_ref"],"generation":endpoint["generation"]});
        crate::lifecycle::dispatch(
            &mut self.kernel,
            "save",
            &json!({"record":{"schema_version":2,"record_type":"ccbd_lease","api_version":2,"project_id":self.config["project_id"],
            "ccbd_pid":std::process::id(),"socket_path":address,"socket_inode":null,"boot_id":self.state,"generation":generation,"daemon_instance_id":self.state,
            "started_at":timestamp,"last_heartbeat_at":timestamp,"mount_state":"mounted","control_plane_endpoint":descriptor,
            "config_signature":crate::config::fingerprint(&self.config)?}}),
        )?;
        crate::lifecycle::dispatch(
            &mut self.kernel,
            "lifecycle_save",
            &json!({"record":{"schema_version":2,"record_type":"ccbd_lifecycle","project_id":self.config["project_id"],"desired_state":"running","phase":"mounted",
            "generation":generation,"phase_started_at":timestamp,"last_progress_at":timestamp,"owner_pid":std::process::id(),"owner_daemon_instance_id":self.state,"keeper_pid":null,"socket_path":address,"socket_inode":null,"namespace_epoch":null,"control_plane_endpoint":descriptor}}),
        )?;
        self.kernel.store.commit()?;
        Ok(())
    }
    pub fn unmount(&mut self) -> Result<()> {
        self.pi.clear();
        self.codex.clear();
        self.claude.clear();
        for running in self.running.values_mut() {
            let _ = running.child.kill();
            let _ = running.child.wait();
        }
        crate::lifecycle::dispatch(
            &mut self.kernel,
            "release",
            &json!({"expected_pid":std::process::id(),"expected_daemon_instance_id":self.state,"now":now()}),
        )?;
        let mut lifecycle = self
            .kernel
            .store
            .json("lifecycle.json")?
            .unwrap_or(Value::Null);
        if !lifecycle.is_null() {
            lifecycle["desired_state"] = json!("stopped");
            lifecycle["phase"] = json!("unmounted");
            lifecycle["phase_started_at"] = json!(now());
            crate::lifecycle::dispatch(
                &mut self.kernel,
                "lifecycle_save",
                &json!({"record":lifecycle}),
            )?;
        }
        self.kernel.store.commit()?;
        Ok(())
    }
    fn names(&self) -> Vec<String> {
        self.config["agents"]
            .as_object()
            .unwrap()
            .keys()
            .cloned()
            .collect()
    }
    fn get_job(&mut self, id: &str) -> Result<Value> {
        for agent in self.known_names.clone() {
            let found = records::dispatch(
                &mut self.kernel.store,
                "find_last",
                &json!({"stream":"jobs","agent_name":agent,"fields":{"job_id":id}}),
            )?;
            if !found.is_null() {
                return Ok(found);
            }
        }
        bail!("unknown job: {id}")
    }
    pub fn execute(&mut self, line: &str) -> Result<Value> {
        let request: Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(e) => return Ok(json!({"api_version":2,"ok":false,"error":e.to_string()})),
        };
        if request["api_version"] == 1 {
            return crate::rpc::execute(&mut self.kernel, line);
        }
        let checkpoint = self.kernel.memory_checkpoint();
        let outcome = (|| {
            ensure!(request["api_version"] == 2, "unsupported api_version");
            ensure!(
                request["request"].is_object() || request["request"].is_null(),
                "request must be an object"
            );
            self.dispatch(request["op"].as_str().unwrap_or(""), &request["request"])
        })();
        match outcome {
            Ok(result) => {
                self.kernel.store.commit()?;
                let mut response = json!({"api_version":2,"ok":true});
                response.as_object_mut().unwrap().extend(
                    result
                        .as_object()
                        .context("invalid daemon response")?
                        .clone(),
                );
                Ok(response)
            }
            Err(error) => {
                self.kernel.store.rollback();
                self.kernel.restore_memory(checkpoint);
                Ok(json!({"api_version":2,"ok":false,"error":error.to_string()}))
            }
        }
    }
    fn dispatch(&mut self, op: &str, p: &Value) -> Result<Value> {
        match op {
            "project_view" => self.project_view(p),
            "project_view_dismiss_comms" => self.dismiss_comms(p),
            "reload-plan" => Ok(json!({"plan":self.reload_plan(&p["config"])?})),
            "reload" => self.request_reload(p),
            "reload-status" => Ok(json!({"reload":self.kernel.store.json("reload.json")?})),
            "project_restart_panes" | "restart" => self.request_restart(p),
            "restart-status" => self.restart_status(p),
            "clear" | "project_clear_context" => self.context_control(p, "clear"),
            "compact" | "project_compact_context" => self.context_control(p, "compact"),
            "context-status" => self.context_status(p),
            "ping" => Ok(
                json!({"service":"ccbd","runtime":"rust","project_id":self.config["project_id"],"pid":std::process::id(),"status":"running",
                    "startup_id":self.config["startup_id"],"config_fingerprint":crate::config::fingerprint(&self.config)?,"degraded":!self.faults.is_empty()}),
            ),
            "provider-status" => {
                let agent = records::string(p, "agent_name")?;
                self.spec(agent)?;
                let owned = self.owned_view(agent);
                let scope = self.owned_scope(agent)?;
                let active = self.active_provider_view(agent)?;
                let fault = self.faults.get(agent).cloned().unwrap_or(Value::Null);
                let state = if !owned.is_null() {
                    if owned["closed"] == true {
                        "closed"
                    } else if !fault.is_null() {
                        "quarantined"
                    } else {
                        "live"
                    }
                } else if fault["pid"].is_null() {
                    "not_started"
                } else if owner_stopped(fault["pid"].as_u64().unwrap_or(0) as u32) {
                    "closed"
                } else {
                    "unknown"
                };
                Ok(
                    json!({"agent_name":agent,"fault":fault,"owned":owned,"scope":scope,"active":active,"process_state":state}),
                )
            }
            "provider-recover" => self.recover_provider(p),
            "provider-terminate" => self.terminate_provider(p),
            "job-recovery" => {
                let job = self.get_job(records::string(p, "target")?)?;
                Ok(self.recovery_view(&job))
            }
            "job-reconcile" => self.reconcile_execution(records::string(p, "target")?),
            "agents" => Ok(json!({"agents":self.names().iter().map(|name| {
                let spec=&self.config["agents"][name];
                json!({"agent_name":name,"provider":spec["provider"],"role":crate::roles::normalized(&spec["role"]).unwrap(),
                    "dispatch_disabled":spec["dispatch_disabled"]==true})
            }).collect::<Vec<_>>()})),
            "submit" => self.submit(p),
            "watch" => self.watch(p),
            "queue" => {
                let target = p["target"].as_str().unwrap_or("all");
                let mut queues = Vec::new();
                for agent in self
                    .names()
                    .into_iter()
                    .filter(|a| target == "all" || a == target)
                {
                    let slot =
                        json!({"state_id":self.state,"target_kind":"agent","target_name":agent});
                    queues.push(json!({"agent_name":agent,"target_kind":"agent","target_name":agent,"active_job_id":self.kernel.dispatch("scheduler.active_job_for",&slot)?,"queued_job_ids":self.kernel.dispatch("scheduler.queued_items_for",&slot)?}));
                }
                ensure!(!queues.is_empty(), "unknown queue target");
                Ok(json!({"target":target,"queues":queues}))
            }
            "provider-requests" => {
                let agent = records::string(p, "agent_name")?;
                let driver = self.spec(agent)?["driver_kind"].clone();
                ensure!(
                    driver == "codex_app_server" || driver == "claude_stream",
                    "interactive requests require an owned Codex or Claude session"
                );
                let requests = if self.faults.contains_key(agent) {
                    json!([])
                } else if driver == "codex_app_server" {
                    self.codex
                        .get(agent)
                        .map(|s| s.pending_requests())
                        .unwrap_or_else(|| json!([]))
                } else {
                    self.claude
                        .get(agent)
                        .map(|s| s.pending_requests())
                        .unwrap_or_else(|| json!([]))
                };
                Ok(json!({"agent_name":agent,"requests":requests}))
            }
            "provider-respond" => {
                let agent = records::string(p, "agent_name")?;
                ensure!(!self.faults.contains_key(agent), "provider is faulted");
                let spec = self.spec(agent)?;
                let fresh = if spec["driver_kind"] == "codex_app_server" {
                    self.codex
                        .get(agent)
                        .context("Codex session is not available")?
                        .validate_response(p)?
                } else if spec["driver_kind"] == "claude_stream" {
                    self.claude
                        .get(agent)
                        .context("Claude session is not available")?
                        .validate_response(p)?
                } else {
                    bail!("provider does not support interactive responses")
                };
                let job = self.get_job(records::string(p, "job_id")?)?;
                ensure!(
                    job["status"] == "running"
                        && job["agent_name"] == agent
                        && job["provider"] == spec["provider"]
                        && job["provider_options"]["native_daemon_instance"] == self.state
                        && job["provider_options"]["native_execution_instance"]
                            == p["execution_instance_id"]
                        && job["provider_options"]["native_cancel_intent"].is_null(),
                    "provider task no longer accepts user responses"
                );
                if fresh {
                    admission::event(
                        &mut self.kernel,
                        &job,
                        "provider_request_answer_requested",
                        json!({"request_id":p["request_id"],"runtime_instance_id":p["runtime_instance_id"],
                            "execution_instance_id":p["execution_instance_id"]}),
                        &now(),
                    )?;
                    if spec["driver_kind"] == "claude_stream"
                        && p["result"]["behavior"] == "deny"
                        && p["result"]["interrupt"] == true
                    {
                        self.mark_provider_cancel(job.clone(), true, false)?;
                    }
                    // Audit the explicit action before it can reach the provider.
                    self.kernel.store.commit()?;
                }
                if spec["driver_kind"] == "codex_app_server" {
                    self.codex.get_mut(agent).unwrap().respond(p)?;
                } else {
                    self.claude.get_mut(agent).unwrap().respond(p)?;
                }
                Ok(json!({"request_id":p["request_id"],"status":"response_queued"}))
            }
            "retry" => {
                let target = records::string(p, "target")?;
                let attempt = retry::source(&mut self.kernel, target)?;
                let retry_agent = records::string(&attempt, "agent_name")?;
                self.reload_guard(retry_agent)?;
                self.spec(retry_agent)?;
                let prior_job = self.get_job(records::string(&attempt, "job_id")?)?;
                if let Some(actor) = prior_job["request"]["from_actor"].as_str() {
                    self.reload_guard(actor)?;
                    ensure!(
                        !self.known_names.iter().any(|name| name == actor)
                            || self.config["agents"][actor].is_object(),
                        "retry caller agent was removed; submit a new task explicitly"
                    );
                }
                ensure!(
                    !self
                        .faults
                        .contains_key(records::string(&attempt, "agent_name")?),
                    "provider is faulted"
                );
                let spec = &self.config["agents"][records::string(&attempt, "agent_name")?];
                ensure!(
                    prior_job["provider"] == spec["provider"]
                        && prior_job["workspace_path"] == spec["workspace_path"],
                    "retry target configuration changed; submit a new task explicitly"
                );
                retry::admit(
                    &mut self.kernel,
                    &json!({"target":target,"state_id":self.state,"accepted_at":now(),"dispatch_disabled":spec["dispatch_disabled"]}),
                )
            }
            "inbox" => {
                let agent = records::string(p, "agent_name")?;
                self.spec(agent)?;
                Ok(
                    json!({"agent_name":agent,"events":self.kernel.dispatch("latest_events",&json!({"agent_name":agent}))?}),
                )
            }
            "mailbox-head" => {
                let agent = records::string(p, "agent_name")?;
                self.spec(agent)?;
                Ok(
                    json!({"agent_name":agent,"head":self.kernel.dispatch("peek_next",&json!({"agent_name":agent}))?}),
                )
            }
            "work" => {
                let agent = records::string(p, "agent_name")?;
                self.reload_guard(agent)?;
                let spec = self.spec(agent)?;
                ensure!(
                    spec["command"].is_null(),
                    "command-driven agents cannot claim external work"
                );
                Ok(json!({"job":self.start(agent)?}))
            }
            "complete" => {
                let job = self.get_job(records::string(p, "job_id")?)?;
                ensure!(
                    job["agent_name"] == p["agent_name"],
                    "completion agent identity mismatch"
                );
                ensure!(
                    !self.running.contains_key(records::string(&job, "job_id")?),
                    "command completion requires its process exit evidence"
                );
                ensure!(job["status"] == "running", "job is not running");
                ensure!(
                    self.config["agents"][job["agent_name"].as_str().unwrap()]["command"].is_null(),
                    "managed completion requires provider evidence"
                );
                ensure!(
                    job["provider_options"]["native_daemon_instance"] == self.state
                        && job["provider_options"]["native_execution_instance"]
                            == p["execution_instance_id"],
                    "completion execution instance mismatch"
                );
                let result = self.complete(&job, &p["decision"])?;
                Ok(json!({"job":result}))
            }
            "cancel" => {
                let id = records::string(p, "job_id")?;
                let job = self.get_job(id)?;
                let agent = records::string(&job, "agent_name")?.to_owned();
                if job["status"] == "cancelled" {
                    return Ok(json!({"job_id":id,"agent_name":agent,"status":"cancelled"}));
                }
                if ["accepted", "queued"].contains(&job["status"].as_str().unwrap_or("")) {
                    let marked =
                        self.mark_provider_cancel(job, p["record_reply"] != false, false)?;
                    self.kernel.dispatch("scheduler.remove_queued_for",&json!({"state_id":self.state,"target_kind":"agent","target_name":agent,"job_id":id}))?;
                    let mut decision = failure(
                        "cancelled",
                        "cancel_info",
                        "Cancelled before provider execution.",
                    );
                    decision["diagnostics"] = json!({"cancel_requested":true,"result_reply_enabled":p["record_reply"]!=false,
                        "provider_prompt_sent":false,"completion_authority":"queued_cancellation"});
                    self.complete(&marked, &decision)?;
                    return Ok(json!({"job_id":id,"agent_name":agent,"status":"cancelled"}));
                }
                ensure!(
                    job["status"] == "running",
                    "native cancel requires a running owned provider task"
                );
                if let Some(session) = self.pi.get(&agent) {
                    let record_reply = p["record_reply"] != false;
                    session.validate_interrupt(id, record_reply)?;
                    self.mark_provider_cancel(job, record_reply, false)?;
                } else if let Some(session) = self.codex.get(&agent) {
                    let record_reply = p["record_reply"] != false;
                    session.validate_interrupt(id, record_reply)?;
                    self.mark_provider_cancel(job, record_reply, false)?;
                } else if let Some(session) = self.claude.get(&agent) {
                    let record_reply = p["record_reply"] != false;
                    session.validate_interrupt(id, record_reply)?;
                    self.mark_provider_cancel(job, record_reply, false)?;
                } else {
                    bail!("native cancellation control is not yet implemented for this provider");
                }
                Ok(json!({"job_id":id,"agent_name":agent,"status":"cancellation_requested"}))
            }
            "shutdown" => {
                ensure!(
                    self.reload.is_none(),
                    "configuration reload in progress; observe reload-status before shutdown"
                );
                self.stopping = true;
                Ok(json!({"status":"stopping"}))
            }
            _ => bail!("unsupported native daemon operation: {op}"),
        }
    }
    fn mark_provider_cancel(
        &mut self,
        mut job: Value,
        record_reply: bool,
        deadline: bool,
    ) -> Result<Value> {
        let prior = &job["provider_options"]["native_cancel_intent"];
        if !prior.is_null() {
            ensure!(
                prior["record_reply"] == record_reply,
                "{} cancellation reply policy already fixed",
                job["provider"].as_str().unwrap_or("Provider")
            );
            return Ok(job);
        }
        let timestamp = now();
        job["cancel_requested_at"] = json!(timestamp);
        job["updated_at"] = json!(timestamp);
        job["provider_options"]["native_cancel_intent"] = json!({"record_reply":record_reply,"requested_at":timestamp,"deadline_triggered":deadline});
        records::dispatch(
            &mut self.kernel.store,
            "append",
            &json!({"stream":"jobs","agent_name":job["agent_name"],"record":job}),
        )?;
        admission::event(
            &mut self.kernel,
            &job,
            "job_cancel_requested",
            json!({"status":job["status"],"record_reply":record_reply,"deadline_triggered":deadline}),
            &timestamp,
        )?;
        Ok(job)
    }
    fn spec(&self, agent: &str) -> Result<Value> {
        let spec = &self.config["agents"][agent];
        ensure!(spec.is_object(), "unknown target agent: {agent}");
        ensure!(
            spec["dispatch_disabled"] != true,
            "agent {agent} is dispatch-disabled"
        );
        Ok(spec.clone())
    }
    fn submit(&mut self, p: &Value) -> Result<Value> {
        let mut request = p.clone();
        ensure!(request.is_object(), "submit requires an object");
        request["to_agent"] = json!(crate::roles::resolve(
            records::string(&request, "to_agent")?,
            &self.config["agents"]
        )?);
        self.reload_guard(records::string(&request, "to_agent")?)?;
        self.reload_guard(records::string(&request, "from_actor")?)?;
        ensure!(
            !self
                .faults
                .contains_key(records::string(&request, "from_actor")?),
            "caller provider is faulted"
        );
        for (k, v) in [
            ("message_type", json!("ask")),
            ("delivery_scope", json!("single")),
            ("silence_on_success", json!(false)),
            ("route_options", json!({})),
            ("body_artifact", Value::Null),
            ("task_id", Value::Null),
            ("reply_to", Value::Null),
        ] {
            if request.get(k).is_none() {
                request[k] = v;
            }
        }
        ensure!(
            request["project_id"] == self.config["project_id"],
            "project identity mismatch"
        );
        ensure!(
            request["message_type"] != "reply_delivery",
            "reply_delivery is reserved for the native mailbox"
        );
        ensure!(
            request["message_type"] != "context_control",
            "context_control requires the native management API"
        );
        let context = json!({"request":request,"state_id":self.state,
            "agent_names":self.names(),"bureau_enabled":true,"max_callback_depth":self.config["max_callback_depth"].as_u64().unwrap_or(5)});
        let parent = crate::callback_policy::validate(&mut self.kernel, &context)?;
        // These claims belong to the authority, never to the submitting worker.
        if let Some(options) = request["route_options"].as_object_mut() {
            options.remove("review_workspace_path");
            options.remove("review_tree_digest");
        }
        if !parent.is_null()
            && (records::truthy(&parent["request"]["route_options"]["bind_chain_workspace_tree"])
                || records::truthy(&request["route_options"]["bind_chain_workspace_tree"]))
        {
            let agent = records::string(&parent, "agent_name")?;
            let spec = self.spec(agent)?;
            let workspace = parent["workspace_path"]
                .as_str()
                .or(spec["workspace_path"].as_str())
                .context("restricted review chain requires a parent workspace")?;
            let observed = crate::git_observation::current(std::path::Path::new(workspace), true)?;
            request["route_options"]["bind_chain_workspace_tree"] = json!(true);
            request["route_options"]["review_workspace_path"] = observed["workspace_path"].clone();
            request["route_options"]["review_tree_digest"] = observed["digest"].clone();
        }
        let transport = request["body_transport"]
            .as_str()
            .unwrap_or("inline")
            .to_owned();
        ensure!(
            ["inline", "auto", "artifact"].contains(&transport.as_str()),
            "invalid body transport"
        );
        let body = records::string(&request, "body")?.to_owned();
        ensure!(
            body.len() <= 512 * 1024,
            "native request body exceeds 512 KiB"
        );
        if !request["body_artifact"].is_null() {
            ensure!(
                transport == "inline",
                "artifact reference cannot be combined with body transport generation"
            );
            request["body_artifact"] =
                crate::artifacts::validate_reference(&self.kernel, &request["body_artifact"])?;
        } else if transport == "artifact" || transport == "auto" && body.len() > 4096 {
            let owner = format!(
                "{}-to-{}",
                request["from_actor"].as_str().unwrap_or("user"),
                request["to_agent"].as_str().unwrap_or("agent")
            );
            let artifact =
                crate::artifacts::write(&mut self.kernel, &body, "ask-request", &owner, &now())?;
            request["body"] = json!(crate::artifacts::stub(
                "CCB ask request was stored as a text artifact.",
                &artifact,
                false,
                None
            ));
            request["body_artifact"] = artifact;
        }
        request.as_object_mut().unwrap().remove("body_transport");
        ensure!(
            request["body"].as_str().context("body must be text")?.len() <= 4096,
            "large native requests require artifact transport"
        );
        let targets = if request["to_agent"] == "all" {
            self.names()
                .into_iter()
                .filter(|a| self.config["agents"][a]["dispatch_disabled"] != true)
                .collect::<Vec<_>>()
        } else {
            vec![records::string(&request, "to_agent")?.to_owned()]
        };
        let mut drafts = Vec::new();
        for agent in &targets {
            self.reload_guard(agent)?;
            ensure!(!self.faults.contains_key(agent), "provider is faulted: {agent}; inspect provider-status and recover its fault identity");
            let spec = self.spec(agent)?;
            let mut child = request.clone();
            child["to_agent"] = json!(agent);
            child["delivery_scope"] = json!("single");
            drafts.push(json!({"agent_name":agent,"provider":spec["provider"],"target_kind":"agent","target_name":agent,"request":child,"workspace_path":spec["workspace_path"],"provider_options":{}}));
        }
        let submission = if request["delivery_scope"] == "broadcast" {
            json!(admission::new_id("sub"))
        } else {
            Value::Null
        };
        let names = self.names();
        let result = admission::submit(
            &mut self.kernel,
            &json!({"state_id":self.state,"agent_names":names,"bureau_enabled":true,"max_callback_depth":self.config["max_callback_depth"].as_u64().unwrap_or(5),"accepted_at":now(),
            "plan":{"project_id":request["project_id"],"from_actor":request["from_actor"],"task_id":request["task_id"],"request":request,"drafts":drafts,"submission_id":submission,"target_scope":"all","origin_message_id":null}}),
        )?;
        Ok(result["receipt"].clone())
    }
    fn watch(&mut self, p: &Value) -> Result<Value> {
        let target = records::string(p, "target")?;
        let cursor = p["cursor"].as_u64().unwrap_or(0);
        ensure!(
            p["cursor"].is_null() || p["cursor"].as_u64().is_some(),
            "watch cursor cannot be negative"
        );
        let job = if self.config["agents"][target].is_object() {
            let rows = records::dispatch(
                &mut self.kernel.store,
                "list",
                &json!({"stream":"jobs","agent_name":target}),
            )?;
            rows.as_array()
                .unwrap()
                .last()
                .cloned()
                .context("agent has no jobs to watch")?
        } else {
            self.get_job(target)?
        };
        let events = records::dispatch(
            &mut self.kernel.store,
            "read_since",
            &json!({"stream":"job_events","agent_name":job["agent_name"],"start_line":cursor}),
        )?;
        let attempt = records::dispatch(
            &mut self.kernel.store,
            "find_last",
            &json!({"stream":"attempts","fields":{"job_id":job["job_id"]}}),
        )?;
        let mut reply = job["terminal_decision"]["reply"].clone();
        let mut status = job["status"].as_str().unwrap_or("").to_owned();
        let mut reason = job["terminal_decision"]["reason"].clone();
        let mut source = "job_terminal_decision";
        let mut reply_id = Value::Null;
        if records::truthy(&job["terminal_decision"]["delegated"]) {
            let replies =
                records::dispatch(&mut self.kernel.store, "list", &json!({"stream":"replies"}))?;
            let found = replies
                .as_array()
                .unwrap()
                .iter()
                .rev()
                .find(|r| r["message_id"] == attempt["message_id"]);
            if let Some(found) = found {
                reply = found["reply"].clone();
                reason = found["diagnostics"]["reason"].clone();
                reply_id = found["reply_id"].clone();
                status = records::string(found, "terminal_status")?.to_owned();
                source = "message_bureau_reply";
            } else {
                reply = json!("");
                reason = json!("callback_pending");
                source = "callback_delegated_pending";
                status = "waiting_callback".into();
            }
        }
        Ok(
            json!({"target":target,"job_id":job["job_id"],"agent_name":job["agent_name"],"target_kind":"agent","target_name":job["agent_name"],"provider":job["provider"],"provider_instance":null,
            "cursor":events["line_no"],"terminal":(["completed","cancelled","failed","incomplete"].contains(&status.as_str())),"status":status,"job_status":job["status"],"reply":reply.as_str().unwrap_or(""),"completion_reason":reason,
            "visible_reply_source":source,"visible_reply_id":reply_id,"message_id":attempt["message_id"],"events":events["rows"].as_array().unwrap().iter().filter(|e|e["job_id"]==job["job_id"]).cloned().collect::<Vec<_>>()}),
        )
    }
    fn materialize_reply(&mut self, agent: &str, head: &Value) -> Result<Value> {
        if let Some(id) = payload(&head["payload_ref"], "delivery:") {
            return self.get_job(&id);
        }
        let id =
            payload(&head["payload_ref"], "reply:").context("reply head has no reply identity")?;
        let reply = records::dispatch(
            &mut self.kernel.store,
            "find_last",
            &json!({"stream":"replies","fields":{"reply_id":id}}),
        )?;
        ensure!(!reply.is_null(), "reply record is missing");
        let spec = self.spec(agent)?;
        let timestamp = now();
        let job_id = admission::new_id("job");
        let message_id = admission::new_id("msg");
        let attempt_id = admission::new_id("att");
        let body = format!(
            "CCB_REPLY from={} reply={id} status={}\n\n{}",
            reply["agent_name"].as_str().unwrap_or(""),
            reply["terminal_status"].as_str().unwrap_or(""),
            reply["reply"]
                .as_str()
                .filter(|s| !s.is_empty())
                .unwrap_or("(empty reply)")
        );
        let job = json!({"schema_version":2,"record_type":"job_record","job_id":job_id,"submission_id":null,"agent_name":agent,"provider":spec["provider"],"target_kind":"agent","target_name":agent,"provider_instance":null,
            "provider_options":{"no_wrap":true,"reply_delivery":true,"reply_delivery_inbound_event_id":head["inbound_event_id"],"reply_delivery_reply_id":id},"workspace_path":spec["workspace_path"],"status":"accepted","terminal_decision":null,"cancel_requested_at":null,"created_at":timestamp,"updated_at":timestamp,
            "request":{"project_id":self.config["project_id"],"to_agent":agent,"from_actor":"system","body":body,"task_id":format!("reply:{id}"),"reply_to":null,"message_type":"reply_delivery","delivery_scope":"single","silence_on_success":false,"route_options":{},"body_artifact":null}});
        admission::append(&mut self.kernel, "jobs", &job)?;
        admission::event(
            &mut self.kernel,
            &job,
            "job_accepted",
            json!({"status":"accepted"}),
            &timestamp,
        )?;
        admission::append(
            &mut self.kernel,
            "messages",
            &json!({"schema_version":1,"record_type":"message_record","message_id":message_id,"origin_message_id":reply["message_id"],"from_actor":"system","target_scope":"single","target_agents":[agent],"message_class":"reply_delivery","reply_policy":{"mode":"none","expected_reply_count":0},"retry_policy":{"mode":"manual"},"priority":10,"payload_ref":format!("reply:{id}"),"submission_id":null,"created_at":timestamp,"updated_at":timestamp,"message_state":"queued"}),
        )?;
        admission::append(
            &mut self.kernel,
            "attempts",
            &json!({"schema_version":1,"record_type":"attempt_record","attempt_id":attempt_id,"message_id":message_id,"agent_name":agent,"provider":spec["provider"],"job_id":job_id,"retry_index":0,"health_snapshot_ref":null,"started_at":timestamp,"updated_at":timestamp,"attempt_state":"pending"}),
        )?;
        self.kernel.dispatch("rewrite_head",&json!({"agent_name":agent,"inbound_event_id":head["inbound_event_id"],"payload_ref":format!("reply:{id} delivery:{job_id}"),"status":"queued","timestamp":timestamp,"clear_progress":true}))?;
        self.kernel.dispatch(
            "scheduler.admit",
            &json!({"state_id":self.state,"jobs":[job]}),
        )?;
        Ok(job)
    }
    fn recovery_view(&self, job: &Value) -> Value {
        let agent = job["agent_name"].as_str().unwrap_or("");
        let mut view = if self.config["agents"][agent]["provider"] != job["provider"] {
            json!({"status":"unavailable","reason":"provider_changed"})
        } else if self.config["agents"][agent]["dispatch_disabled"] == true {
            json!({"status":"unavailable","reason":"agent_disabled"})
        } else {
            crate::execution_recovery::inspect(job, &self.state)
        };
        view["job_id"] = job["job_id"].clone();
        view
    }
    fn reconcile_execution(&mut self, id: &str) -> Result<Value> {
        let current = self.get_job(id)?;
        let mut view = self.recovery_view(&current);
        if view["status"] != "stopped" {
            return Ok(view);
        }
        let proof = &current["provider_options"]["native_scope_proof"];
        let snapshot =
            crate::completion::snapshot(&mut self.kernel, "load", &json!({"job_id":id}))?;
        let candidate = &snapshot["latest_decision"];
        let reusable = snapshot["job_id"] == current["job_id"]
            && snapshot["agent_name"] == current["agent_name"]
            && candidate["schema_version"] == 2
            && candidate["record_type"] == "completion_decision"
            && candidate["terminal"] == true
            && ["completed", "failed", "incomplete", "cancelled"]
                .contains(&candidate["status"].as_str().unwrap_or(""))
            && candidate["diagnostics"]["runtime_instance_id"] == proof["runtime_instance_id"]
            && candidate["diagnostics"]["native_execution_instance"]
                == proof["execution_instance_id"];
        let timestamp = now();
        let mut decision = if reusable {
            candidate.clone()
        } else {
            crate::execution_recovery::interrupted(&current, &view, &timestamp)
        };
        decision["diagnostics"]["recovery_no_replay"] = json!(true);
        decision["diagnostics"]["automatic_replay"] = json!(false);
        decision["diagnostics"]["owner_observation"] = view.clone();
        decision["diagnostics"]["reused_terminal_snapshot"] = json!(reusable);
        if reusable {
            decision["diagnostics"]["recovered_original_finished_at"] =
                decision["finished_at"].clone();
            decision["finished_at"] = json!(timestamp);
            if let Some(reply) =
                current["provider_options"]["native_cancel_intent"]["record_reply"].as_bool()
            {
                decision["diagnostics"]["result_reply_enabled"] = json!(reply);
                decision["diagnostics"]["cancel_requested"] = json!(true);
            }
        }
        let uncertain_delivery = decision["status"] != "completed"
            && (current["request"]["message_type"] == "reply_delivery"
                || current["provider_options"]["reply_delivery"] == true);
        let agent = records::string(&current, "agent_name")?;
        let recovery_fault = if uncertain_delivery && !self.faults.contains_key(agent) {
            Some(
                json!({"schema_version":1,"record_type":"native_provider_fault","project_id":current["request"]["project_id"],
                "agent_name":agent,"provider":current["provider"],"fault_id":uuid::Uuid::new_v4().to_string(),"status":"blocked",
                "phase":"recovery","reason":"result_delivery_uncertain","pid":proof["provider_pid"],"runtime_instance_id":proof["runtime_instance_id"],
                "daemon_instance_id":proof["daemon_instance_id"],"job_id":id,"execution_instance_id":proof["execution_instance_id"],
                "observed_at":timestamp,"io_error_kind":null,"recovery_scope":proof}),
            )
        } else {
            None
        };
        let checkpoint = self.kernel.memory_checkpoint();
        let result = (|| -> Result<Value> {
            let terminal = self.complete(&current, &decision)?;
            admission::event(
                &mut self.kernel,
                &terminal,
                "job_execution_reconciled",
                json!({"scope_id":proof["scope_id"],"owner_observation":view,"reused_terminal_snapshot":reusable,"automatic_replay":false}),
                &timestamp,
            )?;
            if let Some(fault) = &recovery_fault {
                self.kernel.store.save(
                    &crate::provider_fault::path(current["provider"].as_str().unwrap(), agent),
                    fault,
                )?;
            }
            self.kernel.store.commit()?;
            Ok(terminal)
        })();
        match result {
            Ok(job) => {
                if let Some(fault) = recovery_fault {
                    self.faults.insert(agent.to_owned(), fault);
                }
                view["status"] = json!("reconciled");
                view["job"] = job;
                Ok(view)
            }
            Err(error) => {
                self.kernel.store.rollback();
                self.kernel.restore_memory(checkpoint);
                Err(error)
            }
        }
    }
    fn owned_view(&self, agent: &str) -> Value {
        if let Some(s) = self.pi.get(agent) {
            return json!({"pid":s.pid(),"runtime_instance_id":s.runtime,"closed":s.closed,"ready":s.ready});
        }
        if let Some(s) = self.codex.get(agent) {
            return json!({"pid":s.pid(),"runtime_instance_id":s.runtime,"closed":s.closed,"ready":s.ready});
        }
        if let Some(s) = self.claude.get(agent) {
            return json!({"pid":s.pid(),"runtime_instance_id":s.runtime,"closed":s.closed,"ready":s.ready});
        }
        Value::Null
    }
    fn owned_scope(&self, agent: &str) -> Result<Value> {
        if let Some(s) = self.pi.get(agent) {
            return s.scope_info();
        }
        if let Some(s) = self.codex.get(agent) {
            return s.scope_info();
        }
        if let Some(s) = self.claude.get(agent) {
            return s.scope_info();
        }
        Ok(Value::Null)
    }
    fn active_provider_view(&mut self, agent: &str) -> Result<Value> {
        let slot = json!({"state_id":self.state,"target_kind":"agent","target_name":agent});
        let id = self.kernel.dispatch("scheduler.active_job_for", &slot)?;
        if id.is_null() {
            return Ok(Value::Null);
        }
        let job = self.get_job(id.as_str().context("invalid active job identity")?)?;
        Ok(
            json!({"job_id":id,"execution_instance_id":job["provider_options"]["native_execution_instance"],
            "daemon_instance_id":job["provider_options"]["native_daemon_instance"]}),
        )
    }
    fn send_tree_termination(&mut self, agent: &str) -> Result<()> {
        if let Some(session) = self.pi.get_mut(agent) {
            return session.terminate_tree();
        }
        if let Some(session) = self.codex.get_mut(agent) {
            return session.terminate_tree();
        }
        if let Some(session) = self.claude.get_mut(agent) {
            return session.terminate_tree();
        }
        bail!("owned provider process is unavailable")
    }
    fn terminate_provider(&mut self, p: &Value) -> Result<Value> {
        let agent = records::string(p, "agent_name")?;
        self.reload_guard(agent)?;
        let spec = self.spec(agent)?;
        let owned = self.owned_view(agent);
        let scope = self.owned_scope(agent)?;
        ensure!(
            !owned.is_null() && scope["supported"] == true,
            "no owned provider process tree is available"
        );
        ensure!(
            owned["runtime_instance_id"] == p["runtime_instance_id"]
                && owned["pid"] == p["pid"]
                && scope["scope_id"] == p["scope_id"],
            "provider termination runtime identity mismatch"
        );
        let requested = json!({"runtime_instance_id":p["runtime_instance_id"],"pid":p["pid"],"scope_id":p["scope_id"],
            "job_id":p["job_id"],"execution_instance_id":p["execution_instance_id"],"record_reply":p["record_reply"]!=false});
        let prior = self
            .faults
            .get(agent)
            .map(|f| &f["termination_intent"])
            .cloned()
            .unwrap_or(Value::Null);
        if !prior.is_null() {
            ensure!(
                prior["binding"] == requested,
                "provider termination intent already fixed"
            );
            if owned["closed"] != true {
                self.send_tree_termination(agent)?;
            }
            return Ok(
                json!({"agent_name":agent,"termination_id":prior["termination_id"],
                "status":if owned["closed"] == true {"terminated"} else {"termination_requested"}}),
            );
        }
        let active = self.active_provider_view(agent)?;
        ensure!(
            active["job_id"] == p["job_id"]
                && active["execution_instance_id"] == p["execution_instance_id"],
            "provider termination active execution mismatch"
        );
        ensure!(
            active.is_null() || active["daemon_instance_id"] == self.state,
            "provider termination cannot control a prior uncertain execution"
        );
        let termination_id = uuid::Uuid::new_v4().to_string();
        let timestamp = now();
        let intent =
            json!({"termination_id":termination_id,"requested_at":timestamp,"binding":requested});
        if !active.is_null() {
            let job = self.get_job(records::string(&active, "job_id")?)?;
            let mut job = self.mark_provider_cancel(job, p["record_reply"] != false, false)?;
            job["provider_options"]["native_termination_intent"] = intent.clone();
            job["updated_at"] = json!(timestamp);
            admission::append(&mut self.kernel, "jobs", &job)?;
            admission::event(
                &mut self.kernel,
                &job,
                "provider_termination_requested",
                json!({"termination_id":termination_id,"scope_id":scope["scope_id"]}),
                &timestamp,
            )?;
        }
        let mut fault = self.faults.get(agent).cloned().unwrap_or_else(||json!({"schema_version":1,"record_type":"native_provider_fault",
            "project_id":self.config["project_id"],"agent_name":agent,"provider":spec["provider"],"fault_id":uuid::Uuid::new_v4().to_string(),
            "status":"blocked","phase":"terminate","reason":"provider_terminated","pid":owned["pid"],"runtime_instance_id":owned["runtime_instance_id"],
            "daemon_instance_id":self.state,"job_id":active["job_id"],"execution_instance_id":active["execution_instance_id"],"observed_at":timestamp,"io_error_kind":null}));
        fault["termination_intent"] = intent;
        self.kernel.store.save(
            &crate::provider_fault::path(spec["provider"].as_str().unwrap(), agent),
            &fault,
        )?;
        self.kernel.store.commit()?;
        self.faults.insert(agent.to_owned(), fault);
        // No process control occurs before Job/Event/fault intent is durable.
        self.send_tree_termination(agent)?;
        Ok(
            json!({"agent_name":agent,"termination_id":termination_id,"status":"termination_requested"}),
        )
    }
    fn record_provider_fault(
        &mut self,
        agent: &str,
        phase: &str,
        error: &anyhow::Error,
    ) -> Result<()> {
        if self.faults.contains_key(agent) {
            return Ok(());
        }
        let spec = self.spec(agent)?;
        let owned = self.owned_view(agent);
        let slot = json!({"state_id":self.state,"target_kind":"agent","target_name":agent});
        let active = self.kernel.dispatch("scheduler.active_job_for", &slot)?;
        let job = if active.is_null() {
            Value::Null
        } else {
            self.get_job(active.as_str().context("invalid active identity")?)?
        };
        let row = json!({"schema_version":1,"record_type":"native_provider_fault","project_id":self.config["project_id"],
            "agent_name":agent,"provider":spec["provider"],"fault_id":uuid::Uuid::new_v4().to_string(),
            "status":"blocked","phase":phase,"reason":if phase == "startup" { "provider_start_failed" } else { "provider_protocol_error" },
            "pid":owned["pid"],"runtime_instance_id":owned["runtime_instance_id"],"daemon_instance_id":self.state,
            "job_id":job["job_id"],"execution_instance_id":job["provider_options"]["native_execution_instance"],"observed_at":now(),
            "io_error_kind":error.chain().find_map(|e| e.downcast_ref::<std::io::Error>()).map(|e|format!("{:?}", e.kind()))});
        self.kernel.store.save(
            &crate::provider_fault::path(spec["provider"].as_str().unwrap(), agent),
            &row,
        )?;
        if !job.is_null() {
            admission::event(
                &mut self.kernel,
                &job,
                "provider_fault_observed",
                json!({"fault_id":row["fault_id"],"phase":phase,"reason":row["reason"]}),
                &now(),
            )?;
        }
        self.kernel.store.commit()?;
        self.faults.insert(agent.to_owned(), row);
        Ok(())
    }
    fn recover_provider(&mut self, p: &Value) -> Result<Value> {
        let agent = records::string(p, "agent_name")?;
        self.reload_guard(agent)?;
        let spec = self.spec(agent)?;
        let mut fault = self
            .faults
            .get(agent)
            .cloned()
            .context("provider has no blocked fault")?;
        ensure!(
            fault["fault_id"] == p["fault_id"],
            "provider fault identity mismatch"
        );
        let durable = self
            .kernel
            .store
            .json(&crate::provider_fault::path(
                spec["provider"].as_str().unwrap(),
                agent,
            ))?
            .context("durable provider fault is missing")?;
        crate::provider_fault::validate(
            &durable,
            &spec["provider"],
            agent,
            &self.config["project_id"],
        )?;
        ensure!(durable == fault, "durable provider fault identity changed");
        let slot = json!({"state_id":self.state,"target_kind":"agent","target_name":agent});
        ensure!(
            self.kernel
                .dispatch("scheduler.active_job_for", &slot)?
                .is_null(),
            "active execution must be reconciled before provider recovery"
        );
        let owned = self.owned_view(agent);
        let keep_live = if fault["idle_recovery"] == true
            && fault["daemon_instance_id"] == self.state
            && owned["ready"] == true
            && owned["closed"] == false
            && owned["pid"] == fault["pid"]
            && owned["runtime_instance_id"] == fault["runtime_instance_id"]
        {
            let job = self.get_job(records::string(&fault, "job_id")?)?;
            known_unsent_role_delivery(&job)
                && job["agent_name"] == agent
                && job["provider_options"]["native_daemon_instance"] == self.state
                && job["provider_options"]["native_execution_instance"]
                    == fault["execution_instance_id"]
        } else {
            false
        };
        if !owned.is_null() && !keep_live {
            ensure!(
                owned["closed"] == true
                    && owned["pid"] == fault["pid"]
                    && owned["runtime_instance_id"] == fault["runtime_instance_id"],
                "owned provider is not confirmed closed"
            );
        } else if owned.is_null() && !fault["recovery_scope"].is_null() {
            ensure!(
                crate::owned_process::probe_recovery(&fault["recovery_scope"])["status"]
                    == "stopped",
                "recovery scope is not confirmed stopped"
            );
        } else if owned.is_null() && !fault["pid"].is_null() {
            ensure!(
                owner_stopped(fault["pid"].as_u64().context("invalid fault PID")? as u32),
                "prior owned provider is not confirmed closed"
            );
        }
        fault["status"] = json!("recovered");
        fault["recovered_at"] = json!(now());
        self.kernel.store.save(
            &crate::provider_fault::path(spec["provider"].as_str().unwrap(), agent),
            &fault,
        )?;
        // Clear authority only after the recovery identity is durable.
        self.kernel.store.commit()?;
        self.faults.remove(agent);
        if !keep_live {
            self.pi.remove(agent);
            self.codex.remove(agent);
            self.claude.remove(agent);
        }
        Ok(
            json!({"agent_name":agent,"fault_id":fault["fault_id"],"status":"recovered","kept_runtime":keep_live}),
        )
    }
    fn fail_pending_provider_job(&mut self, agent: &str) -> Result<()> {
        if !self.has_dispatchable_work(agent)? {
            return Ok(());
        }
        let head = self
            .kernel
            .dispatch("peek_next", &json!({"agent_name":agent}))?;
        // Result delivery keeps its existing head until the receiver recovers.
        if ["task_reply", "completion_notice"].contains(&head["event_type"].as_str().unwrap_or(""))
        {
            return Ok(());
        }
        let pending = self.get_job(
            &payload(&head["payload_ref"], "job:").context("faulted pending head has no job")?,
        )?;
        if pending["request"]["message_type"] == "reply_delivery" {
            return Ok(());
        }
        let checkpoint = self.kernel.memory_checkpoint();
        let job = match self.start(agent) {
            Ok(job) => {
                self.kernel.store.commit()?;
                job
            }
            Err(error) => {
                self.kernel.store.rollback();
                self.kernel.restore_memory(checkpoint);
                return Err(error);
            }
        };
        if job.is_null() {
            return Ok(());
        }
        let fault = &self.faults[agent];
        let mut decision = failure("failed", fault["reason"].as_str().unwrap(), "Provider is faulted; inspect provider-status and recover its fault identity before resubmitting.");
        decision["diagnostics"] = json!({"completion_authority":"native_provider_not_started","fault_id":fault["fault_id"],"provider_prompt_sent":false});
        let checkpoint = self.kernel.memory_checkpoint();
        if let Err(error) = self.complete(&job, &decision) {
            self.kernel.store.rollback();
            self.kernel.restore_memory(checkpoint);
            return Err(error);
        }
        self.kernel.store.commit()?;
        Ok(())
    }
    fn has_dispatchable_work(&mut self, agent: &str) -> Result<bool> {
        let slot = json!({"state_id":self.state,"target_kind":"agent","target_name":agent});
        if !self
            .kernel
            .dispatch("scheduler.active_job_for", &slot)?
            .is_null()
        {
            return Ok(false);
        }
        // Replies materialize their receiver Job only when execution starts.
        // Queue depth alone therefore misses a closed Lead's pending inbox.
        Ok(!self
            .kernel
            .dispatch("peek_next", &json!({"agent_name":agent}))?
            .is_null())
    }
    fn start(&mut self, agent: &str) -> Result<Value> {
        let slot = json!({"state_id":self.state,"target_kind":"agent","target_name":agent});
        if !self
            .kernel
            .dispatch("scheduler.active_job_for", &slot)?
            .is_null()
        {
            return Ok(Value::Null);
        }
        let head = self
            .kernel
            .dispatch("peek_next", &json!({"agent_name":agent}))?;
        if head.is_null() {
            return Ok(Value::Null);
        }
        let mut job =
            if head["event_type"] == "task_reply" || head["event_type"] == "completion_notice" {
                self.materialize_reply(agent, &head)?
            } else {
                self.get_job(
                    &payload(&head["payload_ref"], "job:")
                        .context("request head has no job identity")?,
                )?
            };
        ensure!(
            ["accepted", "queued"].contains(&job["status"].as_str().unwrap_or("")),
            "mailbox head job is not pending"
        );
        ensure!(
            job["agent_name"] == agent
                && job["request"]["to_agent"] == agent
                && job["provider"] == self.config["agents"][agent]["provider"],
            "mailbox job target/provider mismatch"
        );
        if let Some(provider) = job["provider"]
            .as_str()
            .filter(|p| ["pi", "codex"].contains(p))
        {
            let path = format!("providers/{provider}/{agent}/session.json");
            if let Some(mut metadata) = self.kernel.store.json(&path)? {
                if let Some(state) = metadata["state"].as_object_mut() {
                    if state.remove("native_empty_context").is_some() {
                        // Commit consumption with job_started, before any new
                        // input can reach the provider. A later crash cannot
                        // recreate an empty context over an uncertain prompt.
                        self.kernel.store.save(&path, &metadata)?;
                    }
                }
            }
        }
        let timestamp = now();
        job["status"] = json!("running");
        job["updated_at"] = json!(timestamp);
        job["provider_options"]["native_execution_instance"] =
            json!(uuid::Uuid::new_v4().to_string());
        job["provider_options"]["native_daemon_instance"] = json!(self.state);
        let scope = self.owned_scope(agent)?;
        let proof = crate::execution_recovery::seal(&job, &scope, &self.owned_view(agent));
        if !proof.is_null() {
            job["provider_options"]["native_scope_proof"] = proof;
        }
        admission::append(&mut self.kernel, "jobs", &job)?;
        admission::event(
            &mut self.kernel,
            &job,
            "job_started",
            json!({"status":"running"}),
            &timestamp,
        )?;
        bureau::attempt_transition(
            &mut self.kernel,
            "started",
            &json!({"job":job,"timestamp":timestamp,"lease_ttl_seconds":0}),
        )?;
        self.kernel.dispatch("scheduler.remove_queued_for",&json!({"state_id":self.state,"target_kind":"agent","target_name":agent,"job_id":job["job_id"]}))?;
        self.kernel.dispatch("scheduler.mark_active_for",&json!({"state_id":self.state,"target_kind":"agent","target_name":agent,"job_id":job["job_id"]}))?;
        Ok(job)
    }
    fn provider_prompt(
        &mut self,
        agent: &str,
        job: &Value,
    ) -> Result<Option<crate::roles::Prepared>> {
        if job["provider_options"]["native_context_control"].is_object() {
            return Ok(Some(crate::roles::Prepared {
                text: String::new(),
                audit: Value::Null,
                skill_roots: None,
            }));
        }
        let spec = self.spec(agent)?;
        let project = self.config["project_root"]
            .as_str()
            .or(spec["workspace_path"].as_str())
            .map(std::path::Path::new);
        let mut body = records::string(&job["request"], "body")?.to_owned();
        let review = &job["request"]["route_options"];
        if review["review_workspace_path"].is_string() && review["review_tree_digest"].is_string() {
            body.push_str(&format!("\n\nCCB bound review workspace:\n{}\nReview this workspace and verify its tree identity before reporting; the observation does not freeze files.",
                json!({"workspace_path":review["review_workspace_path"],"tree_digest":review["review_tree_digest"]})));
        }
        let prepared = crate::roles::prompt(&spec, project, &body).and_then(|prompt| {
            if spec["driver_kind"] == "claude_stream" {
                if let Some(roots) = &prompt.skill_roots {
                    self.claude
                        .get_mut(agent)
                        .context("Claude session missing for role preparation")?
                        .stage_role_paths(job, roots)?;
                }
            }
            Ok(prompt)
        });
        match prepared {
            Ok(prompt) => {
                if spec["driver_kind"] == "pi_rpc" {
                    if let Some(roots) = &prompt.skill_roots {
                        self.pi
                            .get(agent)
                            .context("Pi session missing for role preparation")?
                            .stage_role_plan(&mut self.kernel.store, job, roots)?;
                    }
                }
                if !prompt.audit.is_null() {
                    admission::event(
                        &mut self.kernel,
                        job,
                        "role_context_loaded",
                        prompt.audit.clone(),
                        &now(),
                    )?;
                    self.kernel.store.commit()?;
                }
                Ok(Some(prompt))
            }
            Err(_) => {
                // A bad role file is a task configuration failure, not a broken
                // provider session. Never log parser excerpts or memory content.
                let mut decision = failure("failed", "role_context_invalid",
                    "Role context could not be loaded. Check the installed role manifest, declared files, UTF-8 encoding and size limits, then submit again.");
                decision["diagnostics"] = json!({"completion_authority":"native_role_context",
                    "provider_prompt_sent":false,"configuration_error":true});
                self.complete(job, &decision)?;
                self.kernel.store.commit()?;
                Ok(None)
            }
        }
    }
    fn complete(&mut self, current: &Value, decision: &Value) -> Result<Value> {
        let agent = records::string(current, "agent_name")?;
        let spec = self.spec(agent)?;
        let timestamp = decision["finished_at"]
            .as_str()
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
            .unwrap_or_else(now);
        let mut decision = decision.clone();
        let proof = &current["provider_options"]["native_scope_proof"];
        if !proof.is_null() {
            decision["diagnostics"]["native_execution_instance"] =
                proof["execution_instance_id"].clone();
            decision["diagnostics"]["runtime_instance_id"] = proof["runtime_instance_id"].clone();
        }
        let persisted = finalization::persist(
            &mut self.kernel,
            &json!({"current":current,"decision":decision,"finished_at":timestamp,"state_id":self.state,"bureau_enabled":true,"tracker_group":null,"tracker_view":null,"profile_family":spec["profile_family"].as_str().unwrap_or("structured_result")}),
            false,
        )?;
        let mut terminal = persisted["job"].clone();
        let base = persisted["decision"].clone();
        let args =
            json!({"job":terminal,"decision":base,"supports_resume":spec["supports_resume"]==true});
        let mut plan = result_policy::plan(&mut self.kernel, &args)?;
        if plan["action"] == "retry" {
            let receipt = retry::admit(
                &mut self.kernel,
                &json!({"target":current["job_id"],"state_id":self.state,"accepted_at":timestamp,"dispatch_disabled":spec["dispatch_disabled"]}),
            )?;
            admission::event(
                &mut self.kernel,
                &terminal,
                "job_retry_scheduled",
                json!({"message_id":receipt["message_id"],"original_attempt_id":receipt["original_attempt_id"],"attempt_id":receipt["attempt_id"],"job_id":receipt["job_id"],"agent_name":receipt["agent_name"],"attempt_number":plan["context"]["attempt_number"].as_i64().unwrap_or(1)+1,"max_attempts":plan["context"]["max_attempts"],"reason":plan["context"]["reason"]}),
                &timestamp,
            )?;
        } else if plan["action"] == "reply" {
            for event in plan["events"].as_array().unwrap() {
                admission::event(
                    &mut self.kernel,
                    &terminal,
                    records::string(event, "type")?,
                    event["payload"].clone(),
                    &timestamp,
                )?;
            }
            if plan["decision"] != base {
                let revised = finalization::persist(
                    &mut self.kernel,
                    &json!({"current":current,"decision":plan["decision"],"finished_at":timestamp,"state_id":self.state,"prior_snapshot":persisted["prior"],"profile_family":spec["profile_family"].as_str().unwrap_or("structured_result")}),
                    true,
                )?;
                terminal = revised["job"].clone();
                plan["decision"] = revised["decision"].clone();
            }
            let names = self.names();
            let record = result_policy::record(
                &mut self.kernel,
                &json!({"job":terminal,"decision":plan["decision"],"finished_at":timestamp,"deliver_to_caller":plan["deliver_to_caller"],"known_mailboxes":names}),
            )?;
            if record["continuation"]["action"] == "submit" {
                self.enqueue_continuation(
                    &record["continuation"]["edge"],
                    &terminal,
                    &plan["decision"],
                    &timestamp,
                )?;
            }
        } else if plan["action"] == "delivery" {
            let event = &current["provider_options"]["reply_delivery_inbound_event_id"];
            if terminal["status"] == "completed" {
                self.kernel.dispatch(
                    "consume",
                    &json!({"agent_name":agent,"inbound_event_id":event,"timestamp":timestamp}),
                )?;
            } else {
                self.kernel.dispatch("rewrite_head",&json!({"agent_name":agent,"inbound_event_id":event,"payload_ref":format!("reply:{}",current["provider_options"]["reply_delivery_reply_id"].as_str().unwrap_or("")),"status":"queued","timestamp":timestamp,"clear_progress":true}))?;
                if !self.faults.contains_key(agent)
                    && current["provider_options"]["native_daemon_instance"] == self.state
                    && ["pi_rpc", "codex_app_server", "claude_stream"]
                        .contains(&spec["driver_kind"].as_str().unwrap_or(""))
                {
                    let owned = self.owned_view(agent);
                    let fault = json!({"schema_version":1,"record_type":"native_provider_fault",
                        "project_id":self.config["project_id"],"agent_name":agent,"provider":spec["provider"],
                        "fault_id":uuid::Uuid::new_v4().to_string(),"status":"blocked","phase":"recovery","reason":"result_delivery_uncertain","delivery_failure":true,
                        "pid":owned["pid"],"runtime_instance_id":owned["runtime_instance_id"],"daemon_instance_id":self.state,
                        "job_id":current["job_id"],"execution_instance_id":current["provider_options"]["native_execution_instance"],
                        "recovery_scope":current["provider_options"]["native_scope_proof"],
                        "idle_recovery":known_unsent_role_delivery(&terminal),"observed_at":timestamp,"io_error_kind":null});
                    self.kernel.store.save(
                        &crate::provider_fault::path(spec["provider"].as_str().unwrap(), agent),
                        &fault,
                    )?;
                    // Publish the terminal attempt, retained mailbox head and
                    // hold together; the next tick must not rematerialize it.
                    self.kernel.store.commit()?;
                    self.faults.insert(agent.to_owned(), fault);
                }
            }
        }
        Ok(terminal)
    }
    fn enqueue_continuation(
        &mut self,
        edge: &Value,
        child: &Value,
        decision: &Value,
        timestamp: &str,
    ) -> Result<()> {
        let target = records::string(edge, "callback_target_agent")?;
        let spec = self.spec(target)?;
        let parent = self.get_job(records::string(edge, "parent_job_id")?)?;
        callback::enqueue(
            &mut self.kernel,
            &json!({"edge_id":edge["edge_id"],"child_job":child,"decision":decision,"provider":spec["provider"],"workspace_path":if records::truthy(&parent["workspace_path"]) {&parent["workspace_path"]} else {&spec["workspace_path"]},"state_id":self.state,"finished_at":timestamp}),
        )?;
        Ok(())
    }
    pub fn tick(&mut self, endpoint: &Value, token_path: &std::path::Path) -> Result<()> {
        self.tick_reload()?;
        if self.heartbeat.elapsed().as_secs() >= 2 {
            crate::lifecycle::dispatch(
                &mut self.kernel,
                "heartbeat",
                &json!({"expected_pid":std::process::id(),"expected_daemon_instance_id":self.state,"now":now(),"interval_s":2}),
            )?;
            self.kernel.store.commit()?;
            self.heartbeat = Instant::now();
        }
        // Reconcile prior owners before spawning sessions or claiming further work.
        let active = self
            .kernel
            .dispatch("scheduler.active_items", &json!({"state_id":self.state}))?;
        for slot in active.as_array().unwrap() {
            let id = slot[2]
                .as_str()
                .context("invalid active execution identity")?;
            self.reconcile_execution(id)?;
        }
        let mut finished = Vec::new();
        for (id, running) in &mut self.running {
            for _ in 0..64 {
                let Ok(event) = running.output.try_recv() else {
                    break;
                };
                match event {
                    DriverEvent::End => running.eof = true,
                    DriverEvent::Error => {}
                    DriverEvent::Record(value) => {
                        if value["job_id"] == *id
                            && value["agent_name"] == running.job["agent_name"]
                            && value["provider"] == running.job["provider"]
                            && value["execution_instance_id"]
                                == running.job["provider_options"]["native_execution_instance"]
                        {
                            running.result = Some(value["decision"].clone());
                        }
                    }
                }
            }
            let timeout = self.config["agents"][running.job["agent_name"].as_str().unwrap()]
                ["timeout_s"]
                .as_f64()
                .unwrap_or(3600.0);
            if timeout > 0.0 && running.started.elapsed().as_secs_f64() > timeout {
                let _ = running.child.kill();
            }
            if running.exited.is_none() {
                running.exited = running.child.try_wait()?.map(|s| s.success());
            }
            if let Some(success) = running.exited {
                if running.eof {
                    finished.push((id.clone(), success));
                }
            }
        }
        for (id, success) in finished {
            let run = self.running.remove(&id).unwrap();
            let decision = if success {
                run.result.unwrap_or_else(|| {
                    failure(
                        "incomplete",
                        "missing_structured_result",
                        "Command exited without a correlated completion result",
                    )
                })
            } else {
                failure("failed", "provider_process_error", "Command process failed")
            };
            let checkpoint = self.kernel.memory_checkpoint();
            match self.complete(&run.job, &decision) {
                Ok(_) => self.kernel.store.commit()?,
                Err(error) => {
                    self.kernel.store.rollback();
                    self.kernel.restore_memory(checkpoint);
                    return Err(error);
                }
            }
        }
        let names = self.names();
        let recovery = callback_recovery::repair(
            &mut self.kernel,
            &json!({"state_id":self.state,"agent_names":names}),
        )?;
        for action in recovery["actions"].as_array().unwrap() {
            if action["kind"] == "continue" {
                self.enqueue_continuation(
                    &action["edge"],
                    &action["child_job"],
                    &action["decision"],
                    records::string(action, "finished_at")?,
                )?;
            } else {
                self.complete(&action["child_job"], &action["decision"])?;
            }
        }
        self.kernel.store.commit()?;
        for agent in self.names() {
            if self.reload_frozen(&agent) {
                continue;
            }
            if self.config["agents"][&agent]["dispatch_disabled"] == true {
                continue;
            }
            let active = self.kernel.dispatch(
                "scheduler.active_job_for",
                &json!({"state_id":self.state,"target_kind":"agent","target_name":agent}),
            )?;
            if let Some(id) = active.as_str() {
                if self.get_job(id)?["provider_options"]["native_daemon_instance"] != self.state {
                    continue;
                }
            }
            let spec = self.spec(&agent)?;
            if spec["driver_kind"] == "pi_rpc" {
                if self.pi.get(&agent).is_some_and(|session| session.closed)
                    && !self.faults.contains_key(&agent)
                    && self.has_dispatchable_work(&agent)?
                {
                    self.pi.remove(&agent);
                }
                if !self.pi.contains_key(&agent) && self.faults.contains_key(&agent) {
                    self.fail_pending_provider_job(&agent)?;
                    continue;
                }
                if !self.pi.contains_key(&agent) {
                    let saved = self
                        .kernel
                        .store
                        .json(&format!("providers/pi/{agent}/session.json"))?
                        .unwrap_or(Value::Null);
                    let saved = if self.config["agents"][&agent]["resume_session"] == false
                        || saved["state"]["sessionFile"].is_null()
                    {
                        Value::Null
                    } else {
                        saved
                    };
                    let bridge = if !spec["role"].is_null() {
                        Some(crate::pi_skills::Bridge::provision(
                            &mut self.kernel.store,
                            &agent,
                        )?)
                    } else {
                        None
                    };
                    let session = match crate::pi_rpc::Session::spawn(
                        &agent, &spec, endpoint, token_path, &saved, bridge,
                    ) {
                        Ok(session) => session,
                        Err(error) => {
                            self.record_provider_fault(&agent, "startup", &error)?;
                            self.fail_pending_provider_job(&agent)?;
                            continue;
                        }
                    };
                    self.pi.insert(agent.clone(), session);
                }
                if let Some(id) = self.pi[&agent].active_job_id().map(str::to_owned) {
                    let job = self.get_job(&id)?;
                    self.pi.get_mut(&agent).unwrap().sync_job(&job)?;
                }
                let prior_state = self.pi[&agent].state.clone();
                let completion = match self.pi.get_mut(&agent).unwrap().poll() {
                    Ok(result) => result,
                    Err(error) => {
                        self.record_provider_fault(&agent, "poll", &error)?;
                        let session = self.pi.get_mut(&agent).unwrap();
                        session.quarantine(prior_state);
                        None
                    }
                };
                let (completion, metadata, ready) = {
                    let session = self.pi.get_mut(&agent).unwrap();
                    let metadata = if session.dirty {
                        session.dirty = false;
                        Some(
                            json!({"schema_version":1,"record_type":"pi_rpc_session","agent_name":agent,"runtime_instance_id":session.runtime,"pid":session.pid(),"state":session.state,"observed_at":now()}),
                        )
                    } else {
                        None
                    };
                    (completion, metadata, session.ready)
                };
                if let Some(metadata) = metadata {
                    self.kernel
                        .store
                        .save(&format!("providers/pi/{agent}/session.json"), &metadata)?;
                    self.kernel.store.commit()?;
                }
                if let Some((job, decision)) = completion {
                    let checkpoint = self.kernel.memory_checkpoint();
                    if let Err(error) = self.complete(&job, &decision) {
                        self.kernel.store.rollback();
                        self.kernel.restore_memory(checkpoint);
                        return Err(error);
                    }
                    self.kernel.store.commit()?;
                } else if self.pi[&agent].deadline_due() {
                    let id = self.pi[&agent].active_job_id().unwrap().to_owned();
                    let job = self.get_job(&id)?;
                    let marked = self.mark_provider_cancel(job, true, true)?;
                    self.kernel.store.commit()?;
                    self.pi.get_mut(&agent).unwrap().sync_job(&marked)?;
                }
                if self.faults.contains_key(&agent) {
                    self.fail_pending_provider_job(&agent)?;
                }
                if ready && !self.faults.contains_key(&agent) {
                    let checkpoint = self.kernel.memory_checkpoint();
                    let job = match self.start(&agent) {
                        Ok(job) => {
                            self.kernel.store.commit()?;
                            job
                        }
                        Err(error) => {
                            self.kernel.store.rollback();
                            self.kernel.restore_memory(checkpoint);
                            return Err(error);
                        }
                    };
                    if !job.is_null() {
                        let Some(prompt) = self.provider_prompt(&agent, &job)? else {
                            continue;
                        };
                        if let Err(error) = self.pi.get_mut(&agent).unwrap().start(
                            &job,
                            &prompt.text,
                            prompt.skill_roots,
                        ) {
                            self.record_provider_fault(&agent, "start", &error)?;
                            let session = self.pi.get_mut(&agent).unwrap();
                            session.quarantine(session.state.clone());
                            let decision = failure(
                                "failed",
                                "provider_protocol_error",
                                "Provider could not admit the prompt into its owned input queue.",
                            );
                            self.complete(&job, &decision)?;
                            self.kernel.store.commit()?;
                        }
                    }
                }
                continue;
            }
            if spec["driver_kind"] == "codex_app_server" {
                if self.codex.get(&agent).is_some_and(|session| session.closed)
                    && !self.faults.contains_key(&agent)
                    && self.has_dispatchable_work(&agent)?
                {
                    self.codex.remove(&agent);
                }
                if !self.codex.contains_key(&agent) && self.faults.contains_key(&agent) {
                    self.fail_pending_provider_job(&agent)?;
                    continue;
                }
                if !self.codex.contains_key(&agent) {
                    let saved = self
                        .kernel
                        .store
                        .json(&format!("providers/codex/{agent}/session.json"))?
                        .unwrap_or(Value::Null);
                    let session = match crate::codex_rpc::Session::spawn(
                        &agent, &spec, endpoint, token_path, &saved,
                    ) {
                        Ok(session) => session,
                        Err(error) => {
                            self.record_provider_fault(&agent, "startup", &error)?;
                            self.fail_pending_provider_job(&agent)?;
                            continue;
                        }
                    };
                    self.codex.insert(agent.clone(), session);
                }
                if let Some(id) = self.codex[&agent].active_job_id().map(str::to_owned) {
                    let job = self.get_job(&id)?;
                    self.codex.get_mut(&agent).unwrap().sync_job(&job)?;
                }
                let prior_state = self.codex[&agent].state.clone();
                let completion = match self.codex.get_mut(&agent).unwrap().poll() {
                    Ok(result) => result,
                    Err(error) => {
                        self.record_provider_fault(&agent, "poll", &error)?;
                        let session = self.codex.get_mut(&agent).unwrap();
                        session.quarantine(prior_state);
                        None
                    }
                };
                let (completion, metadata, ready) = {
                    let session = self.codex.get_mut(&agent).unwrap();
                    let metadata = if session.dirty {
                        session.dirty = false;
                        Some(
                            json!({"schema_version":1,"record_type":"codex_app_server_session","agent_name":agent,"runtime_instance_id":session.runtime,"pid":session.pid(),"state":session.state,"observed_at":now()}),
                        )
                    } else {
                        None
                    };
                    (completion, metadata, session.ready)
                };
                if let Some(metadata) = metadata {
                    self.kernel
                        .store
                        .save(&format!("providers/codex/{agent}/session.json"), &metadata)?;
                    self.kernel.store.commit()?;
                }
                if let Some((job, decision)) = completion {
                    let checkpoint = self.kernel.memory_checkpoint();
                    if let Err(error) = self.complete(&job, &decision) {
                        self.kernel.store.rollback();
                        self.kernel.restore_memory(checkpoint);
                        return Err(error);
                    }
                    self.kernel.store.commit()?;
                } else if self.codex[&agent].deadline_due() {
                    let id = self.codex[&agent].active_job_id().unwrap().to_owned();
                    let job = self.get_job(&id)?;
                    let marked = self.mark_provider_cancel(job, true, true)?;
                    // Provider IO happens only after the intent is durable.
                    self.kernel.store.commit()?;
                    self.codex.get_mut(&agent).unwrap().sync_job(&marked)?;
                }
                if self.faults.contains_key(&agent) {
                    self.fail_pending_provider_job(&agent)?;
                }
                if ready && !self.faults.contains_key(&agent) {
                    let checkpoint = self.kernel.memory_checkpoint();
                    let job = match self.start(&agent) {
                        Ok(job) => {
                            self.kernel.store.commit()?;
                            job
                        }
                        Err(error) => {
                            self.kernel.store.rollback();
                            self.kernel.restore_memory(checkpoint);
                            return Err(error);
                        }
                    };
                    if !job.is_null() {
                        let Some(prompt) = self.provider_prompt(&agent, &job)? else {
                            continue;
                        };
                        if let Err(error) = self.codex.get_mut(&agent).unwrap().start(
                            &job,
                            &prompt.text,
                            prompt.skill_roots,
                        ) {
                            self.record_provider_fault(&agent, "start", &error)?;
                            let session = self.codex.get_mut(&agent).unwrap();
                            session.quarantine(session.state.clone());
                            let decision = failure(
                                "failed",
                                "provider_protocol_error",
                                "Provider could not admit the prompt into its owned input queue.",
                            );
                            self.complete(&job, &decision)?;
                            self.kernel.store.commit()?;
                        }
                    }
                }
                continue;
            }
            if spec["driver_kind"] == "claude_stream" {
                if self
                    .claude
                    .get(&agent)
                    .is_some_and(|session| session.closed)
                    && !self.faults.contains_key(&agent)
                    && self.has_dispatchable_work(&agent)?
                {
                    self.claude.remove(&agent);
                }
                if !self.claude.contains_key(&agent) && self.faults.contains_key(&agent) {
                    self.fail_pending_provider_job(&agent)?;
                    continue;
                }
                if !self.claude.contains_key(&agent) {
                    let saved = self
                        .kernel
                        .store
                        .json(&format!("providers/claude/{agent}/session.json"))?
                        .unwrap_or(Value::Null);
                    let session = match (|| {
                        let surface = if !spec["role"].is_null() {
                            Some(crate::skill_surface::Surface::provision(
                                &self.kernel.store,
                                &agent,
                            )?)
                        } else {
                            None
                        };
                        crate::claude_stream::Session::spawn(
                            &agent, &spec, endpoint, token_path, &saved, surface,
                        )
                    })() {
                        Ok(session) => session,
                        Err(error) => {
                            self.record_provider_fault(&agent, "startup", &error)?;
                            self.fail_pending_provider_job(&agent)?;
                            continue;
                        }
                    };
                    self.claude.insert(agent.clone(), session);
                }
                if let Some(id) = self.claude[&agent].active_job_id().map(str::to_owned) {
                    let job = self.get_job(&id)?;
                    self.claude.get_mut(&agent).unwrap().sync_job(&job)?;
                }
                let prior_state = self.claude[&agent].state.clone();
                let completion = match self.claude.get_mut(&agent).unwrap().poll() {
                    Ok(result) => result,
                    Err(error) => {
                        self.record_provider_fault(&agent, "poll", &error)?;
                        let session = self.claude.get_mut(&agent).unwrap();
                        session.quarantine(prior_state);
                        None
                    }
                };
                let (completion, metadata, ready) = {
                    let session = self.claude.get_mut(&agent).unwrap();
                    let metadata = if session.dirty {
                        session.dirty = false;
                        Some(
                            json!({"schema_version":1,"record_type":"claude_stream_session","agent_name":agent,"runtime_instance_id":session.runtime,"pid":session.pid(),"state":session.state,"observed_at":now()}),
                        )
                    } else {
                        None
                    };
                    (completion, metadata, session.ready)
                };
                if let Some(metadata) = metadata {
                    self.kernel
                        .store
                        .save(&format!("providers/claude/{agent}/session.json"), &metadata)?;
                    self.kernel.store.commit()?;
                }
                if let Some((job, decision)) = completion {
                    let checkpoint = self.kernel.memory_checkpoint();
                    if let Err(error) = self.complete(&job, &decision) {
                        self.kernel.store.rollback();
                        self.kernel.restore_memory(checkpoint);
                        return Err(error);
                    }
                    self.kernel.store.commit()?;
                } else if self.claude[&agent].deadline_due() {
                    let id = self.claude[&agent].active_job_id().unwrap().to_owned();
                    let job = self.get_job(&id)?;
                    let marked = self.mark_provider_cancel(job, true, true)?;
                    self.kernel.store.commit()?;
                    self.claude.get_mut(&agent).unwrap().sync_job(&marked)?;
                }
                if self.faults.contains_key(&agent) {
                    self.fail_pending_provider_job(&agent)?;
                }
                if ready && !self.faults.contains_key(&agent) {
                    let checkpoint = self.kernel.memory_checkpoint();
                    let job = match self.start(&agent) {
                        Ok(job) => {
                            self.kernel.store.commit()?;
                            job
                        }
                        Err(error) => {
                            self.kernel.store.rollback();
                            self.kernel.restore_memory(checkpoint);
                            return Err(error);
                        }
                    };
                    if !job.is_null() {
                        let Some(prompt) = self.provider_prompt(&agent, &job)? else {
                            continue;
                        };
                        if let Err(error) = self
                            .claude
                            .get_mut(&agent)
                            .unwrap()
                            .start(&job, &prompt.text)
                        {
                            self.record_provider_fault(&agent, "start", &error)?;
                            let session = self.claude.get_mut(&agent).unwrap();
                            session.quarantine(session.state.clone());
                            let decision = failure(
                                "failed",
                                "provider_protocol_error",
                                "Provider could not admit the prompt into its owned input queue.",
                            );
                            self.complete(&job, &decision)?;
                            self.kernel.store.commit()?;
                        }
                    }
                }
                continue;
            }
            if spec["command"].is_null() {
                continue;
            }
            let checkpoint = self.kernel.memory_checkpoint();
            let job = match self.start(&agent) {
                Ok(job) => {
                    self.kernel.store.commit()?;
                    job
                }
                Err(error) => {
                    self.kernel.store.rollback();
                    self.kernel.restore_memory(checkpoint);
                    return Err(error);
                }
            };
            if job.is_null() {
                continue;
            }
            let argv = spec["command"].as_array().unwrap();
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
                        value
                            .as_str()
                            .context("command environment must contain strings")?,
                    );
                }
            }
            crate::installation::provider_commands(&mut command, &spec)?;
            command
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::null());
            command
                .env("CCB_AGENT_NAME", &agent)
                .env("CCB_NATIVE_ENDPOINT", endpoint.to_string())
                .env("CCB_NATIVE_TOKEN_FILE", token_path);
            #[cfg(windows)]
            {
                use std::os::windows::process::CommandExt;
                command.creation_flags(0x08000000);
            }
            match command.spawn() {
                Ok(mut child) => {
                    let stdout = child.stdout.take().context("missing driver stdout")?;
                    let (sender, receiver) = mpsc::sync_channel(16);
                    std::thread::spawn(move || {
                        let mut reader = BufReader::new(stdout);
                        loop {
                            let mut line = Vec::new();
                            match (&mut reader)
                                .take(1024 * 1024 + 1)
                                .read_until(b'\n', &mut line)
                            {
                                Ok(0) => break,
                                Ok(_) if line.len() <= 1024 * 1024 => {
                                    let parsed = serde_json::from_slice::<Value>(&line)
                                        .map(DriverEvent::Record)
                                        .unwrap_or(DriverEvent::Error);
                                    if sender.send(parsed).is_err() {
                                        break;
                                    }
                                }
                                _ => {
                                    let _ = sender.send(DriverEvent::Error);
                                    break;
                                }
                            }
                        }
                        let _ = sender.send(DriverEvent::End);
                    });
                    if let Some(mut input) = child.stdin.take() {
                        let mut bytes = serde_json::to_vec(&job)?;
                        bytes.push(b'\n');
                        std::thread::spawn(move || {
                            let _ = input.write_all(&bytes);
                        });
                    }
                    self.running.insert(
                        records::string(&job, "job_id")?.into(),
                        Running {
                            child,
                            job,
                            output: receiver,
                            result: None,
                            started: Instant::now(),
                            exited: None,
                            eof: false,
                        },
                    );
                }
                Err(error) => {
                    self.complete(
                        &job,
                        &failure("failed", "runtime_unavailable", &error.to_string()),
                    )?;
                    self.kernel.store.commit()?;
                }
            }
        }
        Ok(())
    }
}
fn known_unsent_role_delivery(job: &Value) -> bool {
    let diagnostics = &job["terminal_decision"]["diagnostics"];
    job["request"]["message_type"] == "reply_delivery"
        && ["failed", "cancelled"].contains(&job["status"].as_str().unwrap_or(""))
        && diagnostics["provider_prompt_sent"] == false
        && [
            "native_role_context",
            "pi_role_skill_preflight",
            "codex_role_skill_preflight",
            "claude_role_skill_preflight",
        ]
        .contains(&diagnostics["completion_authority"].as_str().unwrap_or(""))
}
#[cfg(windows)]
pub(crate) fn owner_stopped(pid: u32) -> bool {
    use std::ffi::c_void;
    #[link(name = "kernel32")]
    extern "system" {
        fn OpenProcess(access: u32, inherit: i32, pid: u32) -> *mut c_void;
        fn WaitForSingleObject(handle: *mut c_void, millis: u32) -> u32;
        fn CloseHandle(handle: *mut c_void) -> i32;
        fn GetLastError() -> u32;
    }
    // Observation only. Access denied and wait failures cannot prove exit;
    // reused live PIDs remain conservatively fenced.
    unsafe {
        let handle = OpenProcess(0x00100000, 0, pid);
        if handle.is_null() {
            return GetLastError() == 87;
        }
        let stopped = WaitForSingleObject(handle, 0) == 0;
        CloseHandle(handle);
        stopped
    }
}
#[cfg(not(windows))]
pub(crate) fn owner_stopped(pid: u32) -> bool {
    std::path::Path::new("/proc").is_dir()
        && std::fs::metadata(format!("/proc/{pid}"))
            .is_err_and(|e| e.kind() == std::io::ErrorKind::NotFound)
}
fn payload(value: &Value, prefix: &str) -> Option<String> {
    value
        .as_str()?
        .replace(';', " ")
        .split_whitespace()
        .find_map(|s| s.strip_prefix(prefix).map(str::to_owned))
}
fn failure(status: &str, reason: &str, body: &str) -> Value {
    json!({"schema_version":2,"record_type":"completion_decision","terminal":true,"status":status,"reason":reason,"confidence":"degraded","reply":body,"anchor_seen":false,"reply_started":false,"reply_stable":false,"provider_turn_ref":null,"source_cursor":null,"finished_at":now(),"diagnostics":{}})
}
