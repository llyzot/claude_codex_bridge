//! Correlated native skill refresh; aliases bind catalogue entries to owned links.
use crate::skill_surface::Routes;
use anyhow::{ensure, Result};
use serde_json::{json, Value};

pub enum Step {
    Ready(Value, Value),
    Stop(&'static str),
}
pub struct Load {
    id: String,
    session: String,
    routes: Routes,
    input: Value,
}
impl Load {
    pub fn new(generation: &str, session: &str, routes: Routes, input: Value) -> Self {
        Self {
            id: format!("role-skills-{generation}"),
            session: session.into(),
            routes,
            input,
        }
    }
    pub fn request(&self) -> Value {
        json!({"type":"control_request","request_id":self.id,"request":{"subtype":"reload_skills"}})
    }
    pub fn ingest(&mut self, record: &Value, cancelled: bool) -> Result<Option<Step>> {
        if record["type"] != "control_response"
            || record["response"]["request_id"] != self.id
            || (!record["session_id"].is_null() && record["session_id"] != self.session)
        {
            return Ok(None);
        }
        let response = &record["response"];
        ensure!(
            ["success", "error"].contains(&response["subtype"].as_str().unwrap_or("")),
            "invalid Claude skill refresh response"
        );
        if cancelled {
            return Ok(Some(Step::Stop("claude_cancelled_before_prompt")));
        }
        if response["subtype"] == "error" {
            return Ok(Some(Step::Stop("claude_role_skills_rejected")));
        }
        let Some(rows) = response["response"]["skills"].as_array() else {
            return Ok(Some(Step::Stop("claude_role_skills_not_loaded")));
        };
        if self
            .routes
            .removed
            .iter()
            .any(|alias| rows.iter().any(|row| matches(row, alias)))
        {
            return Ok(Some(Step::Stop("claude_role_skills_stale")));
        }
        let mut bindings = Vec::new();
        for alias in &self.routes.aliases {
            let matched: Vec<_> = rows.iter().filter(|row| matches(row, alias)).collect();
            if matched.len() != 1
                || matched[0]["builtin"] == true
                || matched[0]["name"].as_str().is_none_or(str::is_empty)
            {
                return Ok(Some(Step::Stop("claude_role_skills_not_loaded")));
            }
            if rows
                .iter()
                .filter(|row| row["name"] == matched[0]["name"])
                .count()
                != 1
            {
                return Ok(Some(Step::Stop("claude_role_skills_ambiguous")));
            }
            bindings.push(json!({"name":matched[0]["name"],"alias":alias}));
        }
        // Discovery metadata is not another prompt catalogue: native visibility
        // and invocation policy (including user-only skills) stay with Claude.
        Ok(Some(Step::Ready(self.input.take(), json!(bindings))))
    }
}
fn matches(row: &Value, alias: &str) -> bool {
    row["name"] == alias
        || row["aliases"]
            .as_array()
            .is_some_and(|values| values.iter().any(|v| v == alias))
}
