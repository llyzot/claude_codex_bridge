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
    state: String,
    running: HashMap<String, Running>,
    pi: HashMap<String, crate::pi_rpc::Session>,
    codex: HashMap<String, crate::codex_rpc::Session>,
    pub stopping: bool,
    heartbeat: Instant,
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
    pub fn new(mut kernel: Kernel, config: Value, generation: &str) -> Result<Self> {
        ensure!(
            config["schema_version"] == 2 && !records::string(&config, "project_id")?.is_empty(),
            "invalid native daemon config"
        );
        let agents = config["agents"]
            .as_object()
            .context("daemon agents must be an object")?;
        let prior = kernel.store.json("lease.json")?.unwrap_or(Value::Null);
        if !prior.is_null() && prior["mount_state"] != "unmounted" {
            let pid = prior["ccbd_pid"].as_u64().unwrap_or(0);
            ensure!(
                pid > 0 && pid <= u32::MAX as u64 && owner_stopped(pid as u32),
                "existing daemon ownership is live or cannot be proven stopped"
            );
        }
        ensure!(
            !agents.is_empty() && agents.len() <= 32,
            "daemon requires 1 to 32 agents"
        );
        for (name, spec) in agents {
            crate::models::validate_agent(name)?;
            ensure!(
                !records::string(spec, "provider")?.is_empty(),
                "empty agent provider"
            );
            ensure!(
                spec["command"].is_null() || spec["command"].is_array(),
                "command must be an argv array"
            );
            ensure!(
                spec["driver_kind"].is_null()
                    || ["command", "pi_rpc", "codex_app_server"]
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
                ensure!(
                    !argv.is_empty()
                        && argv.iter().all(Value::is_string)
                        && !argv[0].as_str().unwrap().is_empty(),
                    "invalid command driver argv"
                );
            }
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
        Ok(Self {
            kernel,
            config,
            state,
            running: HashMap::new(),
            pi: HashMap::new(),
            codex: HashMap::new(),
            stopping: false,
            heartbeat: Instant::now(),
        })
    }
    pub fn mount(&mut self, endpoint: &Value) -> Result<()> {
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
            "started_at":timestamp,"last_heartbeat_at":timestamp,"mount_state":"mounted","control_plane_endpoint":descriptor}}),
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
        for agent in self.names() {
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
            "ping" => Ok(
                json!({"service":"ccbd","runtime":"rust","project_id":self.config["project_id"],"pid":std::process::id(),"status":"running"}),
            ),
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
            "retry" => {
                let target = records::string(p, "target")?;
                let attempt = retry::source(&mut self.kernel, target)?;
                let spec = &self.config["agents"][records::string(&attempt, "agent_name")?];
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
                ensure!(
                    job["status"] == "running",
                    "native cancel requires a running owned provider task"
                );
                let agent = records::string(&job, "agent_name")?;
                if let Some(session) = self.pi.get_mut(agent) {
                    session.abort(id, p["record_reply"] != false)?;
                } else {
                    bail!("native cancellation control is not yet implemented for this provider");
                }
                Ok(json!({"job_id":id,"agent_name":agent,"status":"cancellation_requested"}))
            }
            "shutdown" => {
                self.stopping = true;
                Ok(json!({"status":"stopping"}))
            }
            _ => bail!("unsupported native daemon operation: {op}"),
        }
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
        if !request["body_artifact"].is_null() {
            request["body_artifact"] =
                crate::artifacts::validate_reference(&self.kernel, &request["body_artifact"])?;
        }
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
                source = "message_bureau_reply";
            } else {
                reply = json!("");
                reason = json!("callback_pending");
                source = "callback_delegated_pending";
            }
        }
        let status = job["status"].as_str().unwrap_or("");
        Ok(
            json!({"target":target,"job_id":job["job_id"],"agent_name":job["agent_name"],"target_kind":"agent","target_name":job["agent_name"],"provider":job["provider"],"provider_instance":null,
            "cursor":events["line_no"],"terminal":(["completed","cancelled","failed","incomplete"].contains(&status)),"status":status,"reply":reply.as_str().unwrap_or(""),"completion_reason":reason,
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
        let timestamp = now();
        job["status"] = json!("running");
        job["updated_at"] = json!(timestamp);
        job["provider_options"]["native_execution_instance"] =
            json!(uuid::Uuid::new_v4().to_string());
        job["provider_options"]["native_daemon_instance"] = json!(self.state);
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
    fn complete(&mut self, current: &Value, decision: &Value) -> Result<Value> {
        let agent = records::string(current, "agent_name")?;
        let spec = self.spec(agent)?;
        let timestamp = decision["finished_at"]
            .as_str()
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
            .unwrap_or_else(now);
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
        if self.heartbeat.elapsed().as_secs() >= 2 {
            crate::lifecycle::dispatch(
                &mut self.kernel,
                "heartbeat",
                &json!({"expected_pid":std::process::id(),"expected_daemon_instance_id":self.state,"now":now(),"interval_s":2}),
            )?;
            self.kernel.store.commit()?;
            self.heartbeat = Instant::now();
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
            if self.config["agents"][&agent]["dispatch_disabled"] == true {
                continue;
            }
            let spec = self.spec(&agent)?;
            if spec["driver_kind"] == "pi_rpc" {
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
                    let session =
                        crate::pi_rpc::Session::spawn(&agent, &spec, endpoint, token_path, &saved)?;
                    self.pi.insert(agent.clone(), session);
                }
                let (completion, metadata, ready) = {
                    let session = self.pi.get_mut(&agent).unwrap();
                    let completion = session.poll()?;
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
                }
                if ready {
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
                        self.pi.get_mut(&agent).unwrap().start(&job)?;
                    }
                }
                continue;
            }
            if spec["driver_kind"] == "codex_app_server" {
                if !self.codex.contains_key(&agent) {
                    let saved = self
                        .kernel
                        .store
                        .json(&format!("providers/codex/{agent}/session.json"))?
                        .unwrap_or(Value::Null);
                    let session = crate::codex_rpc::Session::spawn(
                        &agent, &spec, endpoint, token_path, &saved,
                    )?;
                    self.codex.insert(agent.clone(), session);
                }
                let (completion, metadata, ready) = {
                    let session = self.codex.get_mut(&agent).unwrap();
                    let completion = session.poll()?;
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
                }
                if ready {
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
                        self.codex.get_mut(&agent).unwrap().start(&job)?;
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
#[cfg(windows)]
fn owner_stopped(pid: u32) -> bool {
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
fn owner_stopped(pid: u32) -> bool {
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
