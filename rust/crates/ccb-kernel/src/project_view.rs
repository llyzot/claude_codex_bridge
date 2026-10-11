//! Read-only native UI projection; observations never complete or recover work.
use super::Daemon;
use anyhow::{ensure, Context, Result};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
const STATE: &str = "project-view.json";
fn terminal(s: &str) -> bool {
    ["completed", "failed", "incomplete", "cancelled"].contains(&s)
}
fn edge_done(e: &Value) -> bool {
    ["done", "failed", "timed_out"].contains(&e["state"].as_str().unwrap_or(""))
}
fn internal(j: &Value) -> bool {
    ["reply_delivery", "chain_continuation"]
        .contains(&j["request"]["message_type"].as_str().unwrap_or(""))
}
fn preview(v: &Value, limit: usize) -> String {
    let s=v.as_str().unwrap_or("").chars().filter(|c|!c.is_control()||c.is_whitespace())
        .filter(|c|!matches!(c,'\u{061c}'|'\u{200b}'..='\u{200f}'|'\u{2028}'..='\u{202e}'|'\u{2060}'..='\u{2069}'|'\u{feff}')).collect::<String>();
    let s = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if s.chars().count() > limit {
        format!(
            "{}...",
            s.chars().take(limit.saturating_sub(3)).collect::<String>()
        )
    } else {
        s
    }
}
impl Daemon {
    fn view_job(&self, agent: &str, id: &str) -> Result<Value> {
        crate::models::validate_agent(agent)?;
        let job =
            self.kernel
                .store
                .latest_by_id(&format!("agents/{agent}/jobs.jsonl"), "job_id", id)?;
        ensure!(
            job.is_null() || job["request"]["project_id"] == self.config["project_id"],
            "view job belongs to another project"
        );
        Ok(job)
    }
    fn view_state(&self) -> Result<Value> {
        let state=self.kernel.store.json(STATE)?.unwrap_or_else(||json!({"schema_version":1,"record_type":"native_project_view_state","project_id":self.config["project_id"],"dismissed":[]}));
        ensure!(
            state["schema_version"] == 1
                && state["record_type"] == "native_project_view_state"
                && state["project_id"] == self.config["project_id"]
                && state["dismissed"]
                    .as_array()
                    .is_some_and(|v| v.len() <= 4096 && v.iter().all(Value::is_string)),
            "invalid native project-view state"
        );
        Ok(state)
    }
    fn callback_status(&self, job: &Value, depth: usize) -> Result<(String, String)> {
        if job["terminal_decision"]["delegated"] != true {
            return Ok((
                job["status"].as_str().unwrap_or("unknown").into(),
                preview(&job["terminal_decision"]["reason"], 128),
            ));
        }
        if depth >= 16 {
            return Ok(("unknown".into(), "callback_depth_unavailable".into()));
        }
        let edge = self.kernel.store.latest_by_id(
            "callbacks/edges.jsonl",
            "parent_job_id",
            job["job_id"].as_str().unwrap_or(""),
        )?;
        if let (Some(id), Some(agent)) = (
            edge["continuation_job_id"].as_str(),
            edge["callback_target_agent"].as_str(),
        ) {
            let next = self.view_job(agent, id)?;
            if !next.is_null() {
                return self.callback_status(&next, depth + 1);
            }
        }
        if ["failed", "timed_out"].contains(&edge["state"].as_str().unwrap_or("")) {
            return Ok(("incomplete".into(), "callback_failed".into()));
        }
        Ok(("waiting_callback".into(), "callback_pending".into()))
    }
    fn child_edge(&self, id: &str) -> Result<Value> {
        self.kernel
            .store
            .project_latest("callbacks/edges.jsonl", "edge_id", |rows| {
                rows.iter()
                    .rev()
                    .find(|e| e["child_job_id"] == id)
                    .cloned()
                    .unwrap_or(Value::Null)
            })
    }
    fn comm_row(&self, job: &Value, known: &BTreeSet<String>) -> Result<Value> {
        ensure!(
            job["request"]["project_id"] == self.config["project_id"],
            "view job belongs to another project"
        );
        let id = job["job_id"].as_str().context("invalid view Job")?;
        let (status, reason) = self.callback_status(job, 0)?;
        let (mut business, mut label, mut phase) = match status.as_str() {
            "accepted" | "queued" => ("sending", "send", "queued"),
            "running" => ("replying", "work", "running"),
            "waiting_callback" => ("waiting_callback", "wait", "waiting_callback"),
            "completed" => ("replied", "done", "terminal"),
            "failed" => ("failed", "fail", "terminal"),
            "incomplete" => ("incomplete", "fail", "terminal"),
            "cancelled" => ("cancelled", "fail", "terminal"),
            _ => ("unknown", "unknown", "unknown"),
        };
        let mut reply_status = Value::Null;
        let mut delivery_id = Value::Null;
        let mut block_reason = Value::Null;
        let sender_name = job["request"]["from_actor"]
            .as_str()
            .unwrap_or("")
            .trim()
            .to_ascii_lowercase();
        let sender = sender_name.as_str();
        let agent = job["agent_name"].as_str().unwrap_or("");
        if status == "completed" && job["request"]["silence_on_success"] == true {
            business = "completed";
        }
        if terminal(&status)
            && !(status == "completed" && job["request"]["silence_on_success"] == true)
            && job["provider_options"]["native_cancel_intent"]["record_reply"] != false
            && known.contains(sender)
            && sender != agent
        {
            let edge = self.child_edge(id)?;
            if !edge.is_null() {
                if !edge_done(&edge) {
                    business = "delivering";
                    label = "back";
                    phase = "waiting_callback";
                } else if edge["state"] != "done" {
                    business = "delivery_failed";
                    label = "fail";
                    phase = "blocked";
                }
            } else {
                let attempt =
                    self.kernel
                        .store
                        .latest_by_id("attempts/attempts.jsonl", "job_id", id)?;
                // A continuation replies to the original logical message using
                // its own attempt. Match message + producing agent, not the
                // original attempt, and keep broadcast peers distinct.
                let reply = self.kernel.store.project_latest(
                    "replies/replies.jsonl",
                    "reply_id",
                    |rows| {
                        rows.iter()
                            .rev()
                            .find(|r| {
                                !attempt.is_null()
                                    && r["message_id"] == attempt["message_id"]
                                    && r["agent_name"] == agent
                            })
                            .cloned()
                            .unwrap_or(Value::Null)
                    },
                )?;
                let reply_id = reply["reply_id"].as_str();
                let event = self.kernel.store.project_latest(
                    &format!("mailboxes/{sender}/inbox.jsonl"),
                    "inbound_event_id",
                    |rows| {
                        rows.iter()
                            .rev()
                            .find(|r| {
                                reply_id.is_some()
                                    && super::payload(&r["payload_ref"], "reply:").as_deref()
                                        == reply_id
                                    && ["task_reply", "completion_notice"]
                                        .contains(&r["event_type"].as_str().unwrap_or(""))
                            })
                            .cloned()
                            .unwrap_or(Value::Null)
                    },
                )?;
                if let Some(delivery) = super::payload(&event["payload_ref"], "delivery:") {
                    delivery_id = json!(delivery);
                    let record = self.view_job(sender, &delivery)?;
                    reply_status = record["status"].clone();
                    if ["failed", "cancelled", "incomplete"]
                        .contains(&record["status"].as_str().unwrap_or(""))
                    {
                        business = "delivery_failed";
                        label = "fail";
                        phase = "blocked";
                    } else if record["status"] != "completed" {
                        business = "delivering";
                        label = "back";
                        phase = "delivering_reply";
                    }
                } else if event["status"] != "consumed"
                    && (!reply.is_null() || status == "completed")
                {
                    business = "delivering";
                    label = "back";
                    phase = "delivering_reply";
                }
            }
            if ["delivering", "delivery_failed"].contains(&business)
                && self.faults.contains_key(sender)
            {
                business = "blocked";
                label = "stuck";
                phase = "blocked";
                block_reason = json!("receiver_provider_fault");
            }
        }
        let cancellable = ["accepted", "queued", "running"]
            .contains(&job["status"].as_str().unwrap_or(""))
            && (job["status"] != "running"
                || ["pi_rpc", "codex_app_server", "claude_stream"].contains(
                    &self.config["agents"][agent]["driver_kind"]
                        .as_str()
                        .unwrap_or(""),
                ));
        Ok(
            json!({"id":id,"short_id":id.chars().rev().take(8).collect::<String>().chars().rev().collect::<String>(),"sender":preview(&job["request"]["from_actor"],64),"target":agent,
            "status":status,"job_status":job["status"],"business_status":business,"status_label":label,"execution_phase":phase,"execution_phase_reason":reason,
            "body_preview":preview(&job["request"]["body"],48),"created_at":job["created_at"],"updated_at":job["updated_at"],
            "reply_status":reply_status,"reply_delivery_job_id":delivery_id,"callback":job["terminal_decision"]["delegated"]==true,
            "recoverable":false,"recover_target":null,"cancel_enabled":cancellable,"short_reason":reason,"block_reason":block_reason}),
        )
    }
    pub(super) fn project_view(&mut self, request: &Value) -> Result<Value> {
        ensure!(
            request["schema_version"].is_null() || request["schema_version"] == 1,
            "unsupported project-view schema"
        );
        let limit = if let Some(value) = request.get("comms_limit") {
            value.as_u64().context("invalid project-view limit")?
        } else {
            8
        } as usize;
        ensure!(
            (1..=64).contains(&limit),
            "project-view comms limit must be 1..64"
        );
        let known = self.names().into_iter().collect::<BTreeSet<_>>();
        let dismissed = self.view_state()?["dismissed"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(Value::as_str)
            .map(str::to_owned)
            .collect::<BTreeSet<_>>();
        let waiting =
            self.kernel
                .store
                .project_latest("callbacks/edges.jsonl", "edge_id", |rows| {
                    let mut map = BTreeMap::<String, Value>::new();
                    for edge in rows.iter().rev() {
                        if !edge_done(edge) {
                            if let Some(agent) = edge["parent_agent"].as_str() {
                                map.entry(agent.into()).or_insert_with(|| edge.clone());
                            }
                        }
                    }
                    map
                })?;
        let reload = self
            .kernel
            .store
            .json("reload.json")?
            .unwrap_or(Value::Null);
        let mut agents = Vec::new();
        let mut candidates = BTreeMap::<String, Value>::new();
        for (order, name) in known.iter().enumerate() {
            let slot = json!({"state_id":self.state,"target_kind":"agent","target_name":name});
            let active = self.kernel.dispatch("scheduler.active_job_for", &slot)?;
            let queue = self.kernel.dispatch("scheduler.queued_items_for", &slot)?;
            let queued = queue.as_array().context("invalid native queue")?;
            let mailbox = self
                .kernel
                .dispatch("mailbox.load", &json!({"agent_name":name}))?;
            let current = active
                .as_str()
                .map(|id| self.view_job(name, id))
                .transpose()?
                .unwrap_or(Value::Null);
            let recent = self.kernel.store.project_latest(
                &format!("agents/{name}/jobs.jsonl"),
                "job_id",
                |rows| {
                    rows.iter()
                        .rev()
                        .filter(|j| !internal(j))
                        .take(64)
                        .cloned()
                        .collect::<Vec<_>>()
                },
            )?;
            for job in recent {
                candidates.insert(job["job_id"].as_str().unwrap().into(), job);
            }
            for id in active
                .as_str()
                .into_iter()
                .chain(queued.iter().take(8).filter_map(Value::as_str))
                .chain(waiting.get(name).and_then(|e| e["parent_job_id"].as_str()))
            {
                let job = self.view_job(name, id)?;
                if !job.is_null() && !internal(&job) {
                    candidates.insert(id.into(), job);
                }
            }
            let owned = self.owned_view(name);
            let spec = &self.config["agents"][name];
            let fault = self.faults.get(name);
            let requests = self
                .codex
                .get(name)
                .map(|s| s.pending_requests().as_array().map(Vec::len).unwrap_or(0))
                .or_else(|| {
                    self.claude
                        .get(name)
                        .map(|s| s.pending_requests().as_array().map(Vec::len).unwrap_or(0))
                })
                .unwrap_or(0);
            let (activity, reason) = if fault.is_some() {
                ("failed", "provider_fault")
            } else if spec["dispatch_disabled"] == true {
                ("offline", "dispatch_disabled")
            } else if self.reload_frozen(name) {
                (
                    "pending",
                    if reload["action"] == "restart" {
                        "provider_restart"
                    } else {
                        "configuration_reload"
                    },
                )
            } else if requests > 0 {
                ("pending", "provider_user_input")
            } else if !active.is_null()
                && current["provider_options"]["native_daemon_instance"] != self.state
                && spec["driver_kind"] != "herdr_tui"
            {
                ("pending", "execution_recovery_required")
            } else if !active.is_null() {
                ("active", "job_running")
            } else if !queued.is_empty() || mailbox["queue_depth"].as_u64().unwrap_or(0) > 0 {
                ("pending", "mailbox_pending")
            } else if waiting.contains_key(name) {
                ("pending", "waiting_callback")
            } else if owned["closed"] == true {
                ("offline", "provider_closed")
            } else if !owned.is_null() && owned["ready"] != true
                || owned.is_null()
                    && ["pi_rpc", "codex_app_server", "claude_stream"]
                        .contains(&spec["driver_kind"].as_str().unwrap_or(""))
            {
                ("pending", "provider_starting")
            } else if spec["driver_kind"] == "herdr_tui" && owned["ready"] == true {
                ("idle", "herdr_readiness_last_observed")
            } else if spec["command"].is_null() {
                ("offline", "external_worker_unobserved")
            } else {
                ("idle", "ready")
            };
            let (symbol, color) = match activity {
                "active" => ("●", "green"),
                "pending" => ("◐", "yellow"),
                "failed" => ("✕", "red"),
                "offline" => ("·", "gray"),
                _ => ("○", "blue"),
            };
            agents.push(json!({"name":name,"display_name":name,"provider":preview(&spec["provider"],64),"provider_display_name":preview(&spec["provider"],64),"window":"native","order":order,"pane_id":null,"active":false,
                "activity_state":activity,"activity_symbol":symbol,"activity_color":color,"activity_source":"native_authority","activity_reason":reason,"current_job_id":active,"queue_depth":queued.len().max(mailbox["queue_depth"].as_u64().unwrap_or(0) as usize),"pending_requests":requests,
                "callback_parent_job_id":waiting.get(name).map(|e|e["parent_job_id"].clone()),"dispatch_blocked_by_reload_drain":self.reload_frozen(name),
                "reload_drain":if self.reload_frozen(name){json!({"intent_kind":if reload["action"] == "restart" {"restart"} else {"configuration"},"phase":reload["status"],"status":reload["status"]})}else{Value::Null},
                "fault_id":fault.map(|f|f["fault_id"].clone()),"last_progress_at":current["updated_at"]}));
        }
        let mut lineages = BTreeMap::<(String, String), (u64, String)>::new();
        for (id, job) in &candidates {
            let attempt =
                self.kernel
                    .store
                    .latest_by_id("attempts/attempts.jsonl", "job_id", id)?;
            let key = (
                attempt["message_id"].as_str().unwrap_or(id).into(),
                job["agent_name"].as_str().unwrap().into(),
            );
            let value = (attempt["retry_index"].as_u64().unwrap_or(0), id.clone());
            if lineages.get(&key).is_none_or(|previous| previous < &value) {
                lineages.insert(key, value);
            }
        }
        let mut comms = Vec::new();
        for (_, (_, id)) in lineages {
            if !dismissed.contains(&id) {
                comms.push(self.comm_row(&candidates[&id], &known)?);
            }
        }
        comms.sort_by(|a, b| {
            let pending = |r: &Value| {
                !terminal(r["status"].as_str().unwrap_or(""))
                    || ["delivering", "delivery_failed", "blocked"]
                        .contains(&r["business_status"].as_str().unwrap_or(""))
            };
            pending(b)
                .cmp(&pending(a))
                .then_with(|| b["updated_at"].as_str().cmp(&a["updated_at"].as_str()))
                .then_with(|| b["id"].as_str().cmp(&a["id"].as_str()))
        });
        comms.truncate(limit);
        let lease = self.kernel.store.json("lease.json")?.unwrap_or(Value::Null);
        let root = self.config["project_root"].as_str().unwrap_or("");
        let display = std::path::Path::new(root)
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or_else(|| self.config["project_id"].as_str().unwrap_or("CCB"));
        let view = json!({"schema_version":1,"project":{"project_id":self.config["project_id"],"root":root,"display_name":preview(&json!(display),128)},
            "namespace":{"epoch":lease["generation"],"runtime":"rust","active_window":null,"entry_window":"native","backend_family":"native",
                "sidebar":{"view":{"comms_limit":limit,"tips":["Native runtime view","Cancel queued or owned active tasks","Dismiss resolved terminal rows","Restart idle providers with configured session recovery; busy targets block the operation"]}}},
            "windows":[{"name":"native","label":"Native agents","kind":"native_runtime","order":0,"active":false,"show_in_sidebar":true,"agents":known}],"agents":agents,"comms":comms,
            "controls":{"cancel":true,"dismiss_terminal":true,"stop":true,"focus":false,"restart":true,"config_ui":false}});
        let digest = format!("{:x}", Sha256::digest(serde_json::to_vec(&view)?));
        if digest != self.view_digest {
            self.view_digest = digest;
            self.view_sequence += 1;
        }
        Ok(json!({"view":view,"cache":{"sequence":self.view_sequence,"ttl_ms":1000}}))
    }
    pub(super) fn dismiss_comms(&mut self, request: &Value) -> Result<Value> {
        let id = crate::records::string(request, "id")?;
        let job = self.get_job(id)?;
        let (status, _) = self.callback_status(&job, 0)?;
        ensure!(
            terminal(&status),
            "only terminal communication can be dismissed"
        );
        let row = self.comm_row(&job, &self.names().into_iter().collect())?;
        ensure!(
            !["delivering", "delivery_failed", "blocked"]
                .contains(&row["business_status"].as_str().unwrap_or("")),
            "reply delivery remains unresolved"
        );
        let mut state = self.view_state()?;
        let rows = state["dismissed"].as_array_mut().unwrap();
        if !rows.contains(&json!(id)) {
            ensure!(rows.len() < 4096, "dismissed communication limit reached");
            rows.push(json!(id));
        }
        let count = rows.len();
        self.kernel.store.save(STATE, &state)?;
        Ok(json!({"status":"dismissed","id":id,"dismissed_count":count}))
    }
}
