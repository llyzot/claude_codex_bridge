//! Live Pi resources through its extension API, fenced by native execution IDs.
use anyhow::{ensure, Context, Result};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

#[derive(Clone)]
pub struct Bridge {
    pub script: PathBuf,
    pub plan: PathBuf,
    pub owner: String,
    command: String,
    relative: String,
}
impl Bridge {
    pub fn provision(store: &mut crate::store::Store, agent: &str) -> Result<Self> {
        let relative = format!("providers/pi/{agent}/role-plan.json");
        let script = format!("providers/pi/{agent}/native-role-bridge.mjs");
        let owner = uuid::Uuid::new_v4().simple().to_string();
        let bridge = Self {
            script: store.owned_path(&script)?,
            plan: store.owned_path(&relative)?,
            command: format!("ccb-native-role-{owner}"),
            owner,
            relative,
        };
        store.save_bytes(&script, include_bytes!("pi_role_bridge.mjs"))?;
        bridge.stage(store, "initial", &[])?;
        store.commit()?;
        Ok(bridge)
    }
    pub fn stage(
        &self,
        store: &mut crate::store::Store,
        generation: &str,
        roots: &[PathBuf],
    ) -> Result<()> {
        store.save(
            &self.relative,
            &json!({"schema_version":1,"owner":self.owner,
            "command":self.command,"generation":generation,"roots":roots}),
        )
    }
    fn description(&self, generation: &str) -> String {
        format!("CCB native role bridge {} {generation}", self.owner)
    }
}

enum Phase {
    Inspect,
    Reload,
    Verify,
    State,
}
pub enum Step {
    Send(Value),
    Ready(Value, Vec<PathBuf>, Value),
    Stop(&'static str),
}
pub struct Load {
    bridge: Bridge,
    generation: String,
    session: String,
    roots: Vec<PathBuf>,
    prompt: Value,
    phase: Phase,
    id: String,
}
impl Load {
    pub fn new(
        bridge: Bridge,
        generation: String,
        session: String,
        roots: Vec<PathBuf>,
        prompt: Value,
    ) -> Self {
        Self {
            id: format!("role-inspect-{generation}"),
            generation,
            session,
            roots,
            prompt,
            bridge,
            phase: Phase::Inspect,
        }
    }
    pub fn request(&self) -> Value {
        json!({"id":self.id,"type":"get_commands"})
    }
    pub fn ingest(&mut self, record: &Value, cancelled: bool) -> Result<Option<Step>> {
        if record["id"] != self.id {
            return Ok(None);
        }
        let command = match self.phase {
            Phase::Reload => "prompt",
            Phase::State => "get_state",
            _ => "get_commands",
        };
        ensure!(
            record["type"] == "response"
                && record["command"] == command
                && record["success"].is_boolean(),
            "invalid Pi role preparation response envelope"
        );
        if record["success"] != true {
            return Ok(Some(Step::Stop(if cancelled {
                "pi_cancelled_before_prompt"
            } else {
                "pi_role_skills_rejected"
            })));
        }
        match self.phase {
            Phase::Inspect => {
                if cancelled {
                    return Ok(Some(Step::Stop("pi_cancelled_before_prompt")));
                }
                if !self.bridge_found(record, false) {
                    return Ok(Some(Step::Stop("pi_role_bridge_missing")));
                }
                self.phase = Phase::Reload;
                self.id = format!("role-reload-{}", self.generation);
                Ok(Some(Step::Send(json!({"id":self.id,"type":"prompt",
                    "message":format!("/{} {}", self.bridge.command, self.generation)}))))
            }
            Phase::Reload => {
                // A control command accepted as model input is an uncertain run,
                // not evidence that resources were reloaded or a Job completed.
                ensure!(
                    record["data"]["disposition"] == "handled",
                    "Pi role reload was not handled as an extension command"
                );
                self.phase = Phase::Verify;
                self.id = format!("role-verify-{}", self.generation);
                Ok(Some(Step::Send(self.request())))
            }
            Phase::Verify => {
                if !self.bridge_found(record, true) || !self.skills_found(record) {
                    return Ok(Some(Step::Stop(if cancelled {
                        "pi_cancelled_before_prompt"
                    } else {
                        "pi_role_skills_not_loaded"
                    })));
                }
                self.phase = Phase::State;
                self.id = format!("role-state-{}", self.generation);
                Ok(Some(Step::Send(json!({"id":self.id,"type":"get_state"}))))
            }
            Phase::State => {
                let data = &record["data"];
                ensure!(
                    data["sessionId"] == self.session && crate::pi_rpc::idle(data),
                    "Pi role reload changed session or did not return idle"
                );
                if cancelled {
                    return Ok(Some(Step::Stop("pi_cancelled_before_prompt")));
                }
                Ok(Some(Step::Ready(
                    self.prompt.take(),
                    self.roots.clone(),
                    data.clone(),
                )))
            }
        }
    }
    fn bridge_found(&self, record: &Value, exact: bool) -> bool {
        record["data"]["commands"].as_array().is_some_and(|rows| {
            rows.iter().any(|r| {
                r["source"] == "extension"
                    && r["name"] == self.bridge.command
                    && same_path(&r["sourceInfo"]["path"], &self.bridge.script)
                    && r["description"].as_str().is_some_and(|s| {
                        if exact {
                            s == self.bridge.description(&self.generation)
                        } else {
                            s.starts_with(&self.bridge.description(""))
                        }
                    })
            })
        })
    }
    fn skills_found(&self, record: &Value) -> bool {
        let Some(rows) = record["data"]["commands"].as_array() else {
            return false;
        };
        let paths = rows
            .iter()
            .filter(|r| r["source"] == "skill")
            .filter_map(|r| {
                r["sourceInfo"]["path"]
                    .as_str()
                    .and_then(|p| Path::new(p).canonicalize().ok())
            })
            .collect::<std::collections::HashSet<_>>();
        self.roots.iter().all(|root| {
            root.join("SKILL.md")
                .canonicalize()
                .ok()
                .is_some_and(|p| paths.contains(&p))
        })
    }
}
fn same_path(value: &Value, expected: &Path) -> bool {
    value
        .as_str()
        .and_then(|p| Path::new(p).canonicalize().ok())
        .zip(expected.canonicalize().ok())
        .is_some_and(|(a, b)| a == b)
}
pub fn generation(job: &Value) -> Result<&str> {
    job["provider_options"]["native_execution_instance"]
        .as_str()
        .context("missing Pi role execution identity")
}
