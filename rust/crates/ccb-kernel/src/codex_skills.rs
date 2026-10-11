//! A turn may start only after the owned Codex process registers and verifies
//! its role skill roots. No provider settings or skill files are written.
use anyhow::{ensure, Result};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

pub enum Step {
    Send(Value),
    Ready(Value, Vec<PathBuf>),
    Stop(&'static str),
}

pub struct Load {
    id: String,
    roots: Vec<PathBuf>,
    cwd: PathBuf,
    turn: Value,
    listing: bool,
}
impl Load {
    pub fn new(id: &str, roots: Vec<PathBuf>, cwd: PathBuf, turn: Value) -> Self {
        Self {
            id: format!("skills-set-{id}"),
            roots,
            cwd,
            turn,
            listing: false,
        }
    }
    pub fn request(&self) -> Value {
        json!({"id":self.id,"method":"skills/extraRoots/set","params":{"extraRoots":self.roots}})
    }
    pub fn ingest(&mut self, record: &Value, cancelled: bool) -> Result<Option<Step>> {
        if record["id"] != self.id {
            return Ok(None);
        }
        ensure!(
            (record["result"].is_object() && record["error"].is_null())
                || (record["result"].is_null() && record["error"].is_object()),
            "invalid Codex role skill response envelope"
        );
        // A matching response closes the outstanding operation. Cancellation
        // waits for that response so a late mutation cannot affect a later Job.
        if cancelled {
            return Ok(Some(Step::Stop("codex_cancelled_before_turn")));
        }
        if !record["error"].is_null() || !record["result"].is_object() {
            return Ok(Some(Step::Stop("codex_role_skills_rejected")));
        }
        if !self.listing {
            self.listing = true;
            self.id = format!("skills-list-{}", self.id);
            return Ok(Some(Step::Send(
                json!({"id":self.id,"method":"skills/list",
                "params":{"cwds":[self.cwd],"forceReload":true}}),
            )));
        }
        if !self.verified(&record["result"]) {
            return Ok(Some(Step::Stop("codex_role_skills_not_loaded")));
        }
        Ok(Some(Step::Ready(self.turn.take(), self.roots.clone())))
    }
    fn verified(&self, result: &Value) -> bool {
        let Some(rows) = result["data"].as_array() else {
            return false;
        };
        let Some(row) = rows.iter().find(|r| same_path(&r["cwd"], &self.cwd)) else {
            return false;
        };
        let Some(skills) = row["skills"].as_array() else {
            return false;
        };
        let discovered = skills
            .iter()
            .filter(|skill| skill["enabled"] == true)
            .filter_map(|skill| {
                skill["path"]
                    .as_str()
                    .and_then(|path| Path::new(path).canonicalize().ok())
            })
            .collect::<std::collections::HashSet<_>>();
        self.roots.iter().all(|root| {
            root.join("SKILL.md")
                .canonicalize()
                .ok()
                .is_some_and(|path| discovered.contains(&path))
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
