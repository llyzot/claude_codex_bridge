//! Dispatcher queues and active slots. Persistent Job/Mailbox history is the
//! source of recovery; this index is rebuilt before dispatch resumes.
use anyhow::{bail, ensure, Result};
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet, VecDeque};

#[derive(Clone, Default)]
struct Slot {
    name: String,
    queue: VecDeque<String>,
    active: Option<String>,
}
#[derive(Clone, Default)]
struct State {
    slots: Vec<Slot>,
    index: HashMap<String, String>,
    active_order: Vec<String>,
}
#[derive(Default)]
pub struct Scheduler {
    states: HashMap<String, State>,
}
impl Scheduler {
    pub fn dispatch(&mut self, op: &str, args: &Value) -> Result<Value> {
        if op == "stats" {
            return Ok(json!({"state_count":self.states.len()}));
        }
        let id = string(args, "state_id")?;
        if op == "drop" {
            self.states.remove(id);
            return Ok(Value::Null);
        }
        if op == "create" {
            ensure!(
                !self.states.contains_key(id),
                "scheduler state already exists"
            );
            let mut state = State::default();
            for name in strings(&args["agent_names"])? {
                state.slot(&name);
            }
            self.states.insert(id.into(), state);
            return Ok(Value::Null);
        }
        let mut state = self
            .states
            .get(id)
            .ok_or_else(|| anyhow::anyhow!("unknown scheduler state"))?
            .clone();
        let result = state.dispatch(op, args)?;
        self.states.insert(id.into(), state);
        Ok(result)
    }
}
impl State {
    fn slot(&mut self, name: &str) -> &mut Slot {
        let index = self
            .slots
            .iter()
            .position(|s| s.name == name)
            .unwrap_or_else(|| {
                self.slots.push(Slot {
                    name: name.into(),
                    ..Default::default()
                });
                self.slots.len() - 1
            });
        &mut self.slots[index]
    }
    fn dispatch(&mut self, op: &str, args: &Value) -> Result<Value> {
        match op {
            "active_items" => {
                return Ok(json!(self
                    .active_order
                    .iter()
                    .filter_map(|name| self
                        .slots
                        .iter()
                        .find(|s| &s.name == name)
                        .and_then(|s| s.active.as_ref().map(|id| json!(["agent", s.name, id]))))
                    .collect::<Vec<_>>()))
            }
            "slots" => {
                return Ok(json!(self
                    .slots
                    .iter()
                    .map(|s| json!(["agent", s.name]))
                    .collect::<Vec<_>>()))
            }
            "queues" => {
                return Ok(json!(self
                    .slots
                    .iter()
                    .map(|s| json!({"target_kind":"agent","target_name":s.name,"items":s.queue}))
                    .collect::<Vec<_>>()))
            }
            "target_for_job" => {
                return Ok(self
                    .index
                    .get(string(args, "job_id")?)
                    .map(|n| json!(["agent", n]))
                    .unwrap_or(Value::Null))
            }
            "rebuild" => return self.rebuild(args),
            "record" => {
                let record = &args["record"];
                ensure!(record["target_kind"] == "agent", "invalid target kind");
                let name = string(record, "target_name")?;
                self.index
                    .insert(string(record, "job_id")?.into(), name.into());
                self.slot(name);
                return Ok(Value::Null);
            }
            _ => {}
        }
        ensure!(args["target_kind"] == "agent", "invalid target kind");
        let name = string(args, "target_name")?;
        if op == "remember_job" {
            self.index
                .insert(string(args, "job_id")?.into(), name.into());
            self.slot(name);
            return Ok(Value::Null);
        }
        if op == "active_job_for" {
            return Ok(json!(self
                .slots
                .iter()
                .find(|s| s.name == name)
                .and_then(|s| s.active.as_ref())));
        }
        if op == "clear_active_for" {
            if let Some(slot) = self.slots.iter_mut().find(|s| s.name == name) {
                if args["job_id"].is_null() || slot.active.as_deref() == args["job_id"].as_str() {
                    slot.active = None;
                    self.active_order.retain(|n| n != name);
                }
            }
            return Ok(Value::Null);
        }
        if op == "mark_active_for" && !self.active_order.iter().any(|n| n == name) {
            self.active_order.push(name.into());
        }
        let slot = self.slot(name);
        Ok(match op {
            "queue_depth_for" => json!(slot.queue.len() + usize::from(slot.active.is_some())),
            "has_outstanding_for" => json!(!slot.queue.is_empty() || slot.active.is_some()),
            "enqueue_for" => {
                slot.queue.push_back(string(args, "job_id")?.into());
                Value::Null
            }
            "pop_next_for" => json!(slot.queue.pop_front()),
            "queued_items_for" => json!(slot.queue),
            "remove_queued_for" => {
                let id = string(args, "job_id")?;
                if let Some(index) = slot.queue.iter().position(|j| j == id) {
                    slot.queue.remove(index);
                    json!(true)
                } else {
                    json!(false)
                }
            }
            "mark_active_for" => {
                slot.active = Some(string(args, "job_id")?.into());
                Value::Null
            }
            _ => bail!("unsupported scheduler operation: {op}"),
        })
    }
    fn rebuild(&mut self, args: &Value) -> Result<Value> {
        self.index.clear();
        self.active_order.clear();
        for slot in &mut self.slots {
            slot.queue.clear();
            slot.active = None;
        }
        for agent in strings(&args["agent_names"])? {
            let rows = args["records"][&agent]
                .as_array()
                .ok_or_else(|| anyhow::anyhow!("missing agent records"))?;
            let mut latest = HashMap::new();
            let mut order = Vec::new();
            for row in rows {
                ensure!(row["target_kind"] == "agent", "invalid target kind");
                let id = string(row, "job_id")?;
                let target = string(row, "target_name")?;
                if !latest.contains_key(id) {
                    order.push(id);
                }
                latest.insert(id, row);
                self.index.insert(id.into(), target.into());
                self.slot(target);
            }
            let pending: Vec<_> = order
                .iter()
                .copied()
                .filter(|id| {
                    ["accepted", "queued"].contains(&latest[id]["status"].as_str().unwrap_or(""))
                })
                .collect();
            let pending_set: HashSet<_> = pending.iter().copied().collect();
            let admission = args["mailbox_order"][&agent]
                .as_array()
                .cloned()
                .unwrap_or_default();
            let mut seen = HashSet::new();
            for id in admission
                .iter()
                .filter_map(Value::as_str)
                .chain(pending.iter().copied())
            {
                if pending_set.contains(id) && seen.insert(id) {
                    self.slot(string(latest[id], "target_name")?)
                        .queue
                        .push_back(id.into());
                }
            }
            for id in order {
                if latest[id]["status"] == "running" {
                    let target = string(latest[id], "target_name")?;
                    if !self.active_order.iter().any(|n| n == target) {
                        self.active_order.push(target.into());
                    }
                    self.slot(target).active = Some(id.into());
                }
            }
        }
        Ok(Value::Null)
    }
}
fn string<'a>(args: &'a Value, key: &str) -> Result<&'a str> {
    args[key]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("missing string: {key}"))
}
fn strings(value: &Value) -> Result<Vec<String>> {
    value
        .as_array()
        .ok_or_else(|| anyhow::anyhow!("expected string array"))?
        .iter()
        .map(|v| {
            v.as_str()
                .map(str::to_owned)
                .ok_or_else(|| anyhow::anyhow!("expected string"))
        })
        .collect()
}
