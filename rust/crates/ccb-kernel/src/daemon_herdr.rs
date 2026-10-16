//! Herdr terminal dispatch joins the same durable queue and result-delivery actor.
use super::{now, Daemon};
use crate::{
    admission,
    herdr_worker::{Event, Worker},
};
use anyhow::{ensure, Result};
use serde_json::{json, Value};

impl Daemon {
    pub(super) fn tick_herdr(&mut self, agent: &str, active: &Value) -> Result<()> {
        if self.faults.contains_key(agent) {
            return Ok(());
        }
        let events: Vec<_> = self
            .herdr
            .get(agent)
            .map(|w| w.events.try_iter().collect())
            .unwrap_or_default();
        for event in events {
            let id = self.herdr[agent].job.clone();
            let mut job = self.get_job(&id)?;
            match event {
                Event::Bound(binding) => {
                    ensure!(
                        job["status"] == "running"
                            && binding["identity"]
                                == job["provider_options"]["herdr_dispatch"]["identity"],
                        "discovered history execution mismatch"
                    );
                    self.kernel.store.save(
                        &format!(
                            "providers/{}/{agent}/herdr-history.json",
                            job["provider"].as_str().unwrap()
                        ),
                        &binding,
                    )?;
                    self.kernel.store.commit()?;
                }
                Event::Prepared(proof) => {
                    ensure!(
                        job["status"] == "running"
                            && job["provider_options"]["herdr_dispatch"].is_null(),
                        "Herdr prompt permit already exists"
                    );
                    ensure!(
                        proof["job_id"] == id
                            && proof["execution_instance_id"]
                                == job["provider_options"]["native_execution_instance"],
                        "Herdr permit execution mismatch"
                    );
                    job["provider_options"]["herdr_dispatch"] = proof.clone();
                    admission::append(&mut self.kernel, "jobs", &job)?;
                    admission::event(
                        &mut self.kernel,
                        &job,
                        "herdr_prompt_permitted",
                        json!({"execution_instance_id":proof["execution_instance_id"]}),
                        &now(),
                    )?;
                    self.kernel.store.commit()?;
                    let worker = self.herdr.get_mut(agent).unwrap();
                    worker.view["pid"] = proof["identity"]["process"]["pid"].clone();
                    worker.view["runtime_instance_id"] = proof["execution_instance_id"].clone();
                    worker.view["last_observed_at"] = json!(super::now());
                    worker.grant.send(())?;
                }
                Event::Completed(mut decision) => {
                    ensure!(
                        job["status"] == "running",
                        "Herdr completion job is not running"
                    );
                    if let Some(reply) =
                        job["provider_options"]["native_cancel_intent"]["record_reply"].as_bool()
                    {
                        // A turn that finished before Esc still completes.
                        decision["diagnostics"]["cancel_requested"] = json!(true);
                        decision["diagnostics"]["result_reply_enabled"] = json!(reply);
                        if decision["status"] != "completed" {
                            decision["status"] = json!("cancelled");
                        }
                    }
                    // Only an anchored transcript result is preserved; interruptions
                    // and context controls carry no reply to recover.
                    if job["provider"] == "claude"
                        && decision["diagnostics"]["completion_authority"]
                            == "herdr_idle_and_anchored_transcript"
                    {
                        // Preserve independently observed idle before callback /
                        // finalization. A dead owner can recover this exact result.
                        let spec = self.spec(agent)?;
                        let receipt =
                            match crate::herdr_terminal_evidence::prepare(&job, &spec, &decision) {
                                Ok(receipt) => receipt,
                                Err(error) => {
                                    self.record_provider_fault(
                                        agent,
                                        "terminal_observation",
                                        &error,
                                    )?;
                                    return Ok(());
                                }
                            };
                        job["provider_options"]["herdr_terminal_observation"] = receipt;
                        admission::append(&mut self.kernel, "jobs", &job)?;
                        self.kernel.store.commit()?;
                    }
                    let checkpoint = self.kernel.memory_checkpoint();
                    if let Err(error) = self.complete(&job, &decision) {
                        self.kernel.store.rollback();
                        self.kernel.restore_memory(checkpoint);
                        return Err(error);
                    }
                    self.kernel.store.commit()?;
                }
                Event::Rotated(next) => {
                    ensure!(
                        job["status"] == "running"
                            && job["provider_options"]["native_context_control"]["action"]
                                .is_string(),
                        "session rotation without a running context control"
                    );
                    let provider = job["provider"].as_str().unwrap().to_owned();
                    let opener = self
                        .activation
                        .opener_binding(agent, &self.config["agents"][agent]);
                    let from = crate::herdr_activation::session_of(&opener);
                    // A deferred discovery binding names no session: only its
                    // cache moves to the successor transcript.
                    if from.is_string() {
                        let rotation = json!({"schema_version":1,"from_session":from,"to_session":next["to_session"],
                            "binding":next["binding"],"job_id":id,
                            "operation_id":job["provider_options"]["native_context_control"]["operation_id"],
                            "rotated_at":super::now()});
                        self.kernel.store.save(
                            &crate::herdr_activation::rotation_path(&provider, agent),
                            &rotation,
                        )?;
                    }
                    if next["cache"].is_object() {
                        self.kernel.store.save(
                            &format!("providers/{provider}/{agent}/herdr-history.json"),
                            &next["cache"],
                        )?;
                    }
                    admission::event(
                        &mut self.kernel,
                        &job,
                        "herdr_session_rotated",
                        json!({"to_session":next["to_session"]}),
                        &now(),
                    )?;
                    self.kernel.store.commit()?;
                    if from.is_string() {
                        self.activation.rotate(agent, next["binding"].clone());
                    }
                }
                Event::Ready => {
                    self.herdr.get_mut(agent).unwrap().view["ready"] = json!(true);
                    self.herdr.get_mut(agent).unwrap().view["last_observed_at"] =
                        json!(super::now());
                }
                Event::Fault => {
                    self.record_provider_fault(agent, "poll", &anyhow::anyhow!("Herdr terminal observation failed; no prompt replay or process termination was performed"))?;
                    return Ok(());
                }
                Event::Unsent => {
                    ensure!(
                        job["provider_options"]["herdr_dispatch"].is_null(),
                        "cannot classify a permitted Herdr prompt as unsent"
                    );
                    let mut decision = super::failure("failed", "herdr_preflight_failed", "Herdr provider/session/history preflight rejected this task before any prompt was permitted.");
                    decision["diagnostics"] = json!({"provider_prompt_sent":false,"configuration_error":true,"completion_authority":"herdr_preflight"});
                    self.complete(&job, &decision)?;
                    self.kernel.store.commit()?;
                    self.herdr.remove(agent);
                    return Ok(());
                }
            }
        }
        if self
            .herdr
            .get(agent)
            .is_some_and(|w| w.view["ready"] != true)
        {
            return Ok(());
        }
        // Collect already-running observations above, but never claim or
        // reattach work until this generation has verified its target session.
        if !self.activation.ready(agent, &self.config["agents"][agent]) {
            return Ok(());
        }
        let mut spec = self.spec(agent)?;
        if spec["herdr_discovery"].is_object() {
            spec["_herdr_history_cache"] = self
                .kernel
                .store
                .json(&format!(
                    "providers/{}/{agent}/herdr-history.json",
                    spec["provider"].as_str().unwrap()
                ))?
                .unwrap_or(Value::Null);
        }
        let job = if let Some(id) = active.as_str() {
            let job = self.get_job(id)?;
            // `active` is the start-of-tick snapshot and may have completed above.
            if job["status"] != "running" {
                return Ok(());
            }
            job
        } else {
            let checkpoint = self.kernel.memory_checkpoint();
            match self.start(agent) {
                Ok(job) => {
                    self.kernel.store.commit()?;
                    job
                }
                Err(error) => {
                    self.kernel.store.rollback();
                    self.kernel.restore_memory(checkpoint);
                    return Err(error);
                }
            }
        };
        if job.is_null() {
            return Ok(());
        }
        if job["provider_options"]["native_context_control"].is_object() {
            let worker =
                Worker::spawn_context(self.config["herdr"].clone(), agent.into(), spec, &job)?;
            self.herdr.insert(agent.into(), worker);
            return Ok(());
        }
        let prompt = if job["provider_options"]["herdr_dispatch"].is_null() {
            let Some(prompt) = self.provider_prompt(agent, &job)? else {
                return Ok(());
            };
            if prompt
                .skill_roots
                .as_ref()
                .is_some_and(|roots| !roots.is_empty())
            {
                let mut decision = super::failure(
                    "failed",
                    "herdr_role_skills_unsupported",
                    "Native Herdr terminal role-skill loading is not implemented.",
                );
                decision["diagnostics"] =
                    json!({"provider_prompt_sent":false,"configuration_error":true});
                self.complete(&job, &decision)?;
                self.kernel.store.commit()?;
                return Ok(());
            }
            prompt.text
        } else {
            String::new()
        };
        let worker = Worker::spawn(
            self.config["herdr"].clone(),
            agent.into(),
            spec,
            &job,
            prompt,
        )?;
        self.herdr.insert(agent.into(), worker);
        Ok(())
    }
    pub(super) fn reobserve_herdr(&mut self, p: &Value) -> Result<Value> {
        let agent = crate::records::string(p, "agent_name")?;
        self.reload_guard(agent)?;
        ensure!(
            self.spec(agent)?["driver_kind"] == "herdr_tui",
            "agent is not a Herdr terminal worker"
        );
        let job = self.get_job(crate::records::string(p, "job_id")?)?;
        ensure!(
            job["agent_name"] == agent
                && job["status"] == "running"
                && job["provider_options"]["herdr_dispatch"].is_object()
                && job["provider_options"]["native_execution_instance"]
                    == p["execution_instance_id"],
            "re-observation requires the exact running permitted execution"
        );
        ensure!(
            self.faults.contains_key(agent),
            "Herdr observation is not faulted"
        );
        let mut fault = self.faults[agent].clone();
        fault["status"] = json!("recovered");
        fault["recovered_at"] = json!(now());
        self.kernel.store.save(
            &crate::provider_fault::path(crate::records::string(&job, "provider")?, agent),
            &fault,
        )?;
        admission::event(
            &mut self.kernel,
            &job,
            "herdr_observation_resumed",
            json!({"prompt_replayed":false}),
            &now(),
        )?;
        self.kernel.store.commit()?;
        self.faults.remove(agent);
        self.herdr.remove(agent);
        Ok(json!({"status":"observation_requested","job_id":job["job_id"],"prompt_replayed":false}))
    }
}
