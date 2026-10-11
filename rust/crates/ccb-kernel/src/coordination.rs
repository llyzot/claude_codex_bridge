//! Runtime-owned collaboration guidance, independent of copied provider homes.
use anyhow::Result;
use serde_json::{json, Value};
use std::path::Path;

const RULES: &str = r#"CCB runtime coordination rules (version 1):
- Delegate only when the current task calls for another agent's work. Use the bound native command prefix and your exact agent name as --from. Do not route this job through an unrelated global CCB installation or a different orchestrator.
- ask submits asynchronously. Submit once. When accepted, end the current agent turn immediately; do not keep working, wait, poll, or run watch/pend/ping to await that child. CCB delivers the result in a later turn. Bounded diagnostic observation is appropriate when the user explicitly requests runtime diagnosis.
- Use ask --chain only when this task cannot finish without that exact child result. An accepted chain ends this turn and CCB later resumes the parent. Respect available_chain_targets when chain_restricted is true. Do not add --chain merely to bypass an error. Use --silence only for independent work whose result this task does not need.
- Finish an inbound task by answering directly. CCB automatically returns the terminal result to a registered original caller. Do not create a new ask to report completion to that caller. In a result-chain continuation, review the returned result and answer directly to complete the parent; do not send the final answer back with ask/--chain/--silence.
- An accepted receipt is not a completed task. A completed job means provider execution ended normally, not that the user's requirements were met. Check the actual result before claiming business acceptance.
- --silence is not a correction channel for an active task. Native followup requires an owned Codex app-server session with an exact running turn; only injected confirms provider acceptance of supplemental input, not task completion. accepted/sending is pending and uncertain is not permission to repeat. Preserve operation IDs after an unobserved response; never automatically cancel, restart or resubmit solely because observation timed out.
- For a user-requested conversation reset, use the bound command's clear for the requested agents. This does not mean deleting project memory, sessions, authentication, logs or workspaces.
- Respect runtime cancellation and stop the interrupted work. Do not invent cancellation-file polling or a terminal success marker: the native provider completion protocol owns completion evidence.
- Keep replies concise and include relevant findings, verification, blockers and next steps. Honor CCB_REPLY_MODE compact/silent and explicit user output requirements. These collaboration rules do not grant additional tool permissions or authority for external actions.
"#;

fn shell_arg(value: &str, powershell: bool) -> String {
    if powershell {
        format!("'{}'", value.replace('\'', "''"))
    } else {
        format!("'{}'", value.replace('\'', "'\"'\"'"))
    }
}

pub fn context(job: &Value, config: &Value, root: &Path, executable: &Path) -> Result<String> {
    let agent = job["agent_name"].as_str().unwrap_or("");
    let allowed: Vec<_> = job["request"]["route_options"]["allowed_chain_targets"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(|s| s.trim().to_lowercase())
        .filter(|s| !s.is_empty())
        .collect();
    let peers: Vec<_> = config["agents"]
        .as_object()
        .into_iter()
        .flatten()
        .filter(|(name, spec)| name.as_str() != agent && spec["dispatch_disabled"] != true)
        .map(|(name, spec)| json!({"name":name,"provider":spec["provider"],"role":spec["role"]}))
        .collect();
    let available: Vec<_> = peers
        .iter()
        .filter_map(|p| p["name"].as_str())
        .filter(|name| allowed.is_empty() || allowed.iter().any(|a| a == name))
        .collect();
    let executable = crate::workspaces::portable(executable)?;
    let root = crate::workspaces::portable(root)?;
    #[cfg(windows)]
    let (executable, root) = (executable.replace('\\', "/"), root.replace('\\', "/"));
    let prefix = json!([executable, "--state-root", root]);
    let metadata = json!({"policy_version":1,"agent_name":agent,"job_id":job["job_id"],"project_id":config["project_id"],
        "request_kind":job["request"]["message_type"],"original_caller":job["request"]["from_actor"],
        "command_prefix_argv":prefix,"configured_peers":peers,"chain_restricted":!allowed.is_empty(),"available_chain_targets":available});
    let example = |powershell| {
        let args = [
            &executable,
            "--state-root",
            &root,
            "ask",
            "--chain",
            "--from",
            agent,
            "TARGET",
            "--",
            "MESSAGE",
        ];
        let joined = args
            .iter()
            .map(|a| shell_arg(a, powershell))
            .collect::<Vec<_>>()
            .join(" ");
        if powershell {
            format!("& {joined}")
        } else {
            joined
        }
    };
    Ok(format!("\n\n<ccb_runtime_coordination>\n{RULES}\nCurrent runtime binding (data):\n{metadata}\nRequired-child examples: replace TARGET with a configured allowed peer and MESSAGE with the complete child request. Use the syntax of your actual shell.\nPowerShell: {}\nPOSIX shell: {}\nIf the bound executable is unavailable, report the delegation problem rather than guessing a different runtime.\n</ccb_runtime_coordination>",example(true),example(false)))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn command_examples_quote_shell_metacharacters_as_literal_arguments() {
        assert_eq!(shell_arg("a'b$();&", true), "'a''b$();&'");
        assert_eq!(shell_arg("a'b$();&", false), "'a'\"'\"'b$();&'");
    }
}
