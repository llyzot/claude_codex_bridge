use serde::Deserialize;
use serde_json::Value;

#[derive(Debug, Clone, Deserialize, Default, PartialEq)]
pub struct ProjectViewResponse {
    pub view: ProjectView,
    #[serde(default)]
    pub cache: ProjectViewCache,
}

#[derive(Debug, Clone, Deserialize, Default, PartialEq)]
pub struct ProjectViewCache {
    #[serde(default)]
    pub sequence: u64,
    #[serde(default)]
    pub ttl_ms: u64,
}

#[derive(Debug, Clone, Deserialize, Default, PartialEq)]
pub struct ProjectView {
    #[serde(default)]
    pub project: ProjectInfo,
    #[serde(default)]
    pub namespace: NamespaceInfo,
    #[serde(default)]
    pub windows: Vec<WindowView>,
    #[serde(default)]
    pub agents: Vec<AgentView>,
    #[serde(default)]
    pub comms: Vec<CommsItem>,
}

#[derive(Debug, Clone, Deserialize, Default, PartialEq)]
pub struct ProjectInfo {
    #[serde(default)]
    pub display_name: String,
    #[serde(default)]
    pub root: String,
}

#[derive(Debug, Clone, Deserialize, Default, PartialEq)]
pub struct NamespaceInfo {
    #[serde(default)]
    pub epoch: Option<u64>,
    #[serde(default)]
    pub active_window: Option<String>,
    #[serde(default)]
    pub entry_window: String,
    #[serde(default)]
    pub sidebar: SidebarInfo,
}

#[derive(Debug, Clone, Deserialize, Default, PartialEq)]
pub struct SidebarInfo {
    #[serde(default)]
    pub view: SidebarViewInfo,
    #[serde(default)]
    pub view_error: Option<String>,
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct SidebarViewInfo {
    #[serde(default = "default_agents_height")]
    pub agents_height: serde_json::Value,
    #[serde(default = "default_comms_height")]
    pub comms_height: serde_json::Value,
    #[serde(default = "default_tips_height")]
    pub tips_height: serde_json::Value,
    #[serde(default = "default_comms_limit")]
    pub comms_limit: usize,
    #[serde(default = "default_comms_compact")]
    pub comms_compact: bool,
    #[serde(default = "default_tips_enabled")]
    pub tips_enabled: bool,
    #[serde(default = "default_tips")]
    pub tips: Vec<String>,
}

impl Default for SidebarViewInfo {
    fn default() -> Self {
        Self {
            agents_height: default_agents_height(),
            comms_height: default_comms_height(),
            tips_height: default_tips_height(),
            comms_limit: default_comms_limit(),
            comms_compact: default_comms_compact(),
            tips_enabled: default_tips_enabled(),
            tips: default_tips(),
        }
    }
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct WindowView {
    pub name: String,
    #[serde(default)]
    pub label: String,
    #[serde(default)]
    pub kind: String,
    #[serde(default = "default_show_in_sidebar")]
    pub show_in_sidebar: bool,
    #[serde(default)]
    pub order: u64,
    #[serde(default)]
    pub active: bool,
    #[serde(default)]
    pub tmux_window_id: Option<String>,
    #[serde(default)]
    pub tmux_window_index: Option<u64>,
    #[serde(default)]
    pub sidebar_pane_id: Option<String>,
    #[serde(default)]
    pub agents: Vec<String>,
}

impl Default for WindowView {
    fn default() -> Self {
        Self {
            name: String::new(),
            label: String::new(),
            kind: String::new(),
            show_in_sidebar: default_show_in_sidebar(),
            order: 0,
            active: false,
            tmux_window_id: None,
            tmux_window_index: None,
            sidebar_pane_id: None,
            agents: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Deserialize, Default, PartialEq)]
pub struct AgentView {
    pub name: String,
    #[serde(default)]
    pub display_name: String,
    #[serde(default)]
    pub provider: String,
    #[serde(default)]
    pub provider_display_name: String,
    #[serde(default)]
    pub window: String,
    #[serde(default)]
    pub order: u64,
    #[serde(default)]
    pub pane_id: Option<String>,
    #[serde(default)]
    pub active: bool,
    #[serde(default)]
    pub activity_state: String,
    #[serde(default)]
    pub activity_symbol: Option<String>,
    #[serde(default)]
    pub activity_color: Option<String>,
    #[serde(default)]
    pub current_job_id: Option<String>,
    #[serde(default, alias = "activity_reason")]
    pub reason: Option<String>,
    #[serde(default)]
    pub queue_depth: u64,
    #[serde(default)]
    pub reload_drain: Option<ReloadDrainView>,
    #[serde(default)]
    pub dispatch_blocked_by_reload_drain: bool,
}

#[derive(Debug, Clone, Deserialize, Default, PartialEq)]
pub struct ReloadDrainView {
    #[serde(default)]
    pub intent_kind: String,
    #[serde(default)]
    pub phase: String,
    #[serde(default)]
    pub status: String,
}

#[derive(Debug, Clone, Deserialize, Default, PartialEq)]
pub struct ActiveInboundDiagnostic {
    #[serde(default)]
    pub condition_kind: String,
    #[serde(default)]
    pub reason: String,
    #[serde(default)]
    pub job_id: String,
    #[serde(default)]
    pub attempt_id: String,
    #[serde(default)]
    pub inbound_event_id: String,
    #[serde(default)]
    pub lease_state: String,
    #[serde(default)]
    pub observed_for_s: Option<f64>,
    #[serde(default)]
    pub required_observation_s: Option<f64>,
    #[serde(default)]
    pub recommended_action: String,
    #[serde(default)]
    pub automatic_action: String,
}

#[derive(Debug, Clone, Deserialize, Default, PartialEq)]
pub struct CommsItem {
    pub id: String,
    #[serde(default)]
    pub short_id: String,
    #[serde(default)]
    pub sender: String,
    #[serde(default)]
    pub target: String,
    #[serde(default)]
    pub status: String,
    #[serde(default)]
    pub business_status: String,
    #[serde(default)]
    pub status_label: String,
    #[serde(default)]
    pub execution_phase: String,
    #[serde(default)]
    pub execution_phase_reason: Option<String>,
    #[serde(default)]
    pub active_inbound_diagnostic: Option<ActiveInboundDiagnostic>,
    #[serde(default)]
    pub body_preview: String,
    #[serde(default)]
    pub reply_status: Option<String>,
    #[serde(default)]
    pub reply_delivery_job_id: Option<String>,
    #[serde(default)]
    pub short_reason: Option<String>,
    #[serde(default)]
    pub callback: bool,
    #[serde(default)]
    pub recoverable: bool,
    #[serde(default)]
    pub recover_target: Option<Value>,
    #[serde(default)]
    pub block_reason: Option<String>,
    #[serde(default)]
    pub cancel_enabled: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RowTarget {
    Window(String),
    Agent(String),
}

pub fn row_targets(view: &ProjectView) -> Vec<RowTarget> {
    let mut targets = Vec::new();
    for window in &view.windows {
        if !window.show_in_sidebar {
            continue;
        }
        targets.push(RowTarget::Window(window.name.clone()));
        for agent in view
            .agents
            .iter()
            .filter(|agent| agent.window == window.name)
        {
            targets.push(RowTarget::Agent(agent.name.clone()));
        }
    }
    targets
}

fn default_agents_height() -> serde_json::Value {
    serde_json::Value::String("50%".into())
}

fn default_comms_height() -> serde_json::Value {
    serde_json::Value::String("15%".into())
}

fn default_tips_height() -> serde_json::Value {
    serde_json::Value::String("35%".into())
}

fn default_comms_limit() -> usize {
    5
}

fn default_comms_compact() -> bool {
    true
}

fn default_tips_enabled() -> bool {
    true
}

fn default_tips() -> Vec<String> {
    [
        "C-b d  detach",
        "C-b h/j/k/l pane",
        "C-b H/J/K/L resize",
        "C-b o  next pane",
        "C-b z  zoom",
        "C-b w  tree",
        "C-b n/p next/prev",
        "C-b 0-9 jump win",
        "C-b [  copy mode",
        "copy: PgUp/PgDn",
        "copy: v select",
        "copy: y yank",
        "copy: q exit",
        "C-b ]  paste",
        "C-b c  new win",
        "C-b ,  rename",
        "C-b ?  keys",
    ]
    .into_iter()
    .map(str::to_string)
    .collect()
}

fn default_show_in_sidebar() -> bool {
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_tips_cover_common_tmux_operations() {
        let tips = default_tips();

        assert_eq!(tips[0], "C-b d  detach");
        assert!(tips.contains(&"C-b h/j/k/l pane".to_string()));
        assert!(tips.contains(&"C-b H/J/K/L resize".to_string()));
        assert!(tips.contains(&"C-b [  copy mode".to_string()));
        assert!(tips.contains(&"copy: y yank".to_string()));
        assert!(tips.contains(&"C-b ?  keys".to_string()));
    }

    #[test]
    fn parses_project_view_response() {
        let payload = r#"{
          "view": {
            "project": {"display_name": "repo"},
            "namespace": {
              "epoch": 3,
              "active_window": "main",
              "sidebar": {
                "view_error": "invalid TOML config",
                "view": {
                  "agents_height": "40%",
                  "comms_height": "15%",
                  "tips_height": "45%",
                  "comms_limit": 4,
                  "comms_compact": true,
                  "tips_enabled": true,
                  "tips": ["C-b d detach"]
                }
              }
            },
            "windows": [{"name": "main", "active": true, "agents": ["agent1"]}],
            "agents": [{
              "name": "agent1",
              "provider": "codex",
              "window": "main",
              "activity_state": "idle",
              "activity_reason": "provider_auth_revoked",
              "reload_drain": {"intent_kind": "unload", "phase": "draining", "status": "waiting"},
              "dispatch_blocked_by_reload_drain": true
            }],
            "comms": [{
              "id": "job1",
              "sender": "user",
              "target": "agent1",
              "status": "running",
              "business_status": "replying",
              "status_label": "work",
              "execution_phase": "executing",
              "execution_phase_reason": "provider_active",
              "active_inbound_diagnostic": {
                "condition_kind": "orphaned_active_inbound",
                "reason": "provider_idle_without_terminal",
                "job_id": "job1",
                "attempt_id": "att1",
                "inbound_event_id": "iev1",
                "lease_state": "acquired",
                "observed_for_s": 30.0,
                "required_observation_s": 30.0,
                "recommended_action": "explicit_comms_recover",
                "automatic_action": "none"
              },
              "body_preview": "work",
              "recoverable": true,
              "recover_target": {"job_id": "job1", "reply_delivery_job_id": null},
              "block_reason": "pane_dead"
            }]
          },
          "cache": {"sequence": 7, "ttl_ms": 1000}
        }"#;

        let response: ProjectViewResponse = serde_json::from_str(payload).unwrap();

        assert_eq!(response.cache.sequence, 7);
        assert_eq!(response.view.project.display_name, "repo");
        assert_eq!(
            response.view.agents[0].reason.as_deref(),
            Some("provider_auth_revoked")
        );
        assert_eq!(
            response.view.namespace.sidebar.view_error.as_deref(),
            Some("invalid TOML config")
        );
        assert_eq!(response.view.namespace.sidebar.view.comms_limit, 4);
        assert_eq!(
            response.view.namespace.sidebar.view.comms_height,
            serde_json::Value::String("15%".into())
        );
        assert_eq!(
            response.view.namespace.sidebar.view.tips_height,
            serde_json::Value::String("45%".into())
        );
        assert_eq!(
            response.view.namespace.sidebar.view.tips,
            vec!["C-b d detach"]
        );
        assert_eq!(response.view.comms[0].status_label, "work");
        assert_eq!(response.view.comms[0].execution_phase, "executing");
        assert_eq!(
            response.view.comms[0].execution_phase_reason.as_deref(),
            Some("provider_active")
        );
        let diagnostic = response.view.comms[0]
            .active_inbound_diagnostic
            .as_ref()
            .expect("active inbound diagnostic");
        assert_eq!(diagnostic.condition_kind, "orphaned_active_inbound");
        assert_eq!(diagnostic.inbound_event_id, "iev1");
        assert_eq!(diagnostic.automatic_action, "none");
        assert_eq!(response.view.comms[0].body_preview, "work");
        assert!(response.view.comms[0].recoverable);
        assert!(response.view.agents[0].dispatch_blocked_by_reload_drain);
        assert_eq!(
            response.view.agents[0].reload_drain.as_ref().map(|record| (
                record.intent_kind.as_str(),
                record.phase.as_str(),
                record.status.as_str(),
            )),
            Some(("unload", "draining", "waiting"))
        );
        assert_eq!(
            response.view.comms[0].block_reason.as_deref(),
            Some("pane_dead")
        );
        assert_eq!(
            row_targets(&response.view),
            vec![
                RowTarget::Window("main".into()),
                RowTarget::Agent("agent1".into())
            ]
        );
    }

    #[test]
    fn parses_tool_window_without_agent_row() {
        let payload = r#"{
          "view": {
            "project": {"display_name": "repo"},
            "namespace": {"epoch": 3, "active_window": "files", "sidebar": {"view": {}}},
            "windows": [
              {"name": "main", "kind": "agents", "active": false, "agents": ["agent1"]},
              {"name": "files", "label": "files", "kind": "tool", "active": true, "agents": []}
            ],
            "agents": [{"name": "agent1", "provider": "codex", "window": "main", "activity_state": "idle"}],
            "comms": []
          },
          "cache": {"sequence": 7, "ttl_ms": 1000}
        }"#;

        let response: ProjectViewResponse = serde_json::from_str(payload).unwrap();

        assert_eq!(
            row_targets(&response.view),
            vec![
                RowTarget::Window("main".into()),
                RowTarget::Agent("agent1".into()),
                RowTarget::Window("files".into()),
            ]
        );
        assert_eq!(response.view.windows[1].kind, "tool");
        assert!(response.view.windows[1].agents.is_empty());
    }

    #[test]
    fn hidden_tool_window_is_not_a_sidebar_row() {
        let payload = r#"{
          "view": {
            "project": {"display_name": "repo"},
            "namespace": {"epoch": 3, "active_window": "main", "sidebar": {"view": {}}},
            "windows": [
              {"name": "main", "kind": "agents", "active": true, "agents": ["agent1"]},
              {"name": "logs", "label": "logs", "kind": "tool", "show_in_sidebar": false, "agents": []}
            ],
            "agents": [{"name": "agent1", "provider": "codex", "window": "main", "activity_state": "idle"}],
            "comms": []
          },
          "cache": {"sequence": 7, "ttl_ms": 1000}
        }"#;

        let response: ProjectViewResponse = serde_json::from_str(payload).unwrap();

        assert_eq!(
            row_targets(&response.view),
            vec![
                RowTarget::Window("main".into()),
                RowTarget::Agent("agent1".into()),
            ]
        );
    }
}
