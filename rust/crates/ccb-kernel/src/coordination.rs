//! Runtime-owned collaboration guidance, independent of copied provider homes.
use anyhow::{ensure, Context, Result};
use serde_json::{json, Value};
use std::path::Path;

const RULES: &str = r#"CCB runtime coordination rules (version 1):
- Delegate only when the current task calls for another agent's work. Use the bound native command prefix and your exact agent name as --from. Do not route this job through an unrelated global CCB installation or a different orchestrator.
- ask submits asynchronously. Submit once. When accepted, end the current agent turn immediately; do not keep working, wait, poll, or run watch/pend/ping to await that child. CCB delivers the result in a later turn. Bounded diagnostic observation is appropriate when the user explicitly requests runtime diagnosis.
- Use required_result_mode in the current binding: interactive requests and reply_delivery use plain ask; active parent tasks use ask --chain when they need a child result. An accepted submission ends this turn and CCB later resumes you. Respect available_chain_targets when chain_restricted is true. Do not add --chain merely to bypass an error. Use --silence only for independent work whose result this task does not need.
- Review returned work and continue remaining user-requested stages using the current required_result_mode. When the task is finished, answer directly; CCB returns the terminal result to the original caller. Never use another ask just to send the final answer back to that caller.
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

fn example(executable: &str, root: &str, agent: &str, chain: bool, powershell: bool) -> String {
    let mut args = vec![executable, "--state-root", root, "ask"];
    if chain {
        args.push("--chain");
    }
    args.extend(["--from", agent, "TARGET", "--", "MESSAGE"]);
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
}

pub fn interactive(agent: &str, config: &Value, root: &Path, executable: &Path) -> Result<String> {
    crate::models::validate_agent(agent)?;
    // A hosted Codex process can start before the other terminal agents bind.
    // Its startup context must describe the configured team, not that temporary
    // subset. Per-task context still reports the currently mounted graph.
    let team = if config["herdr"]["config"].is_object() {
        let team = &config["herdr"]["config"];
        ensure!(
            team["project_id"] == config["project_id"] && team["agents"][agent].is_object(),
            "interactive coordination team belongs to another project or agent"
        );
        team
    } else {
        config
    };
    let peers: Vec<_> = team["agents"]
        .as_object()
        .context("missing CCB team")?
        .iter()
        .filter(|(name, spec)| name.as_str() != agent && spec["dispatch_disabled"] != true)
        .map(|(name, spec)| json!({"name":name,"provider":spec["provider"]}))
        .collect();
    let executable = crate::workspaces::portable(executable)?;
    let root = crate::workspaces::portable(root)?;
    #[cfg(windows)]
    let (executable, root) = (executable.replace('\\', "/"), root.replace('\\', "/"));
    let data = json!({"agent_name":agent,"project_id":config["project_id"],"configured_peers":peers,
        "command_prefix_argv":[executable,"--state-root",root],"required_result_mode":"plain"});
    Ok(format!("\n\n<ccb_interactive_coordination>\nYou are a member of the current CCB team. When the user asks a configured peer to analyze, implement or review, use CCB ask to contact that existing agent. Do not substitute pi-subagents, subagent or a separately launched CLI unless the user explicitly requests that orchestrator.\nFor a direct user request, submit plain ask with your exact --from name. Submit once, then end this turn; CCB delivers the result back to this conversation. Do not wait or poll for the result.\nPreserve the user's complete goal and remaining stages in the handoff. Review the returned report; a completed process does not prove successful analysis or repair. Continue the next requested stage, then report verification.\nA CCB-delivered task contains a newer ccb_runtime_coordination binding. Follow its required_result_mode and allowed targets: reply_delivery still uses plain ask; an active parent dependency uses --chain. Finish by replying directly, without asking the original caller to receive the final answer.\nUse the shell syntax of the tool you actually have. If CCB rejects dispatch, report the error instead of choosing another runtime.\nCurrent interactive binding (data):\n{data}\nReplace TARGET with a configured peer and MESSAGE with the complete request.\nPowerShell: {}\nPOSIX shell: {}\n</ccb_interactive_coordination>",
        example(&executable,&root,agent,false,true),example(&executable,&root,agent,false,false)))
}

pub fn claude_arguments(
    request: &Value,
    agent: &str,
    args: Vec<String>,
    store: &mut crate::store::Store,
) -> Result<Vec<String>> {
    let project = Path::new(crate::records::string(&request["config"], "project_root")?);
    let context = interactive(
        agent,
        &request["config"],
        &crate::config::state_root(project)?,
        &std::env::current_exe()?,
    )?;
    let mut result = Vec::new();
    let mut existing = Vec::new();
    let mut index = 0;
    while index < args.len() {
        let arg = &args[index];
        let (kind, value) =
            if ["--append-system-prompt", "--append-system-prompt-file"].contains(&arg.as_str()) {
                index += 1;
                (
                    arg.as_str(),
                    Some(
                        args.get(index)
                            .context("append system prompt requires a value")?
                            .clone(),
                    ),
                )
            } else if let Some(value) = arg.strip_prefix("--append-system-prompt=") {
                ("--append-system-prompt", Some(value.into()))
            } else if let Some(value) = arg.strip_prefix("--append-system-prompt-file=") {
                ("--append-system-prompt-file", Some(value.into()))
            } else {
                ("", None)
            };
        if let Some(value) = value {
            existing.push(if kind.ends_with("-file") {
                let cwd = request["config"]["agents"][agent]["workspace_path"]
                    .as_str()
                    .map(Path::new)
                    .unwrap_or(project);
                std::fs::read_to_string(cwd.join(value))
                    .context("reading configured Claude append-system-prompt file")?
            } else {
                value
            });
        } else {
            result.push(arg.clone());
        }
        index += 1;
    }
    existing.push(context);
    let prompt = existing.join("\n");
    ensure!(
        prompt.len() <= 256 * 1024,
        "Claude startup coordination and custom prompt exceed the supported size"
    );
    let relative = format!("providers/claude/{agent}/coordination.md");
    let path = store.owned_path(&relative)?;
    if store.bytes(&relative)?.as_deref() != Some(prompt.as_bytes()) {
        store.save_bytes(&relative, prompt.as_bytes())?;
        store.commit()?;
    }
    result.extend([
        "--append-system-prompt-file".into(),
        crate::workspaces::portable(&path)?,
    ]);
    Ok(result)
}

pub fn context(job: &Value, config: &Value, root: &Path, executable: &Path) -> Result<String> {
    let agent = job["agent_name"].as_str().unwrap_or("");
    let chain = job["job_id"].is_string()
        && job["request"]["message_type"] != "reply_delivery"
        && job["provider_options"]["reply_delivery"] != true;
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
        "request_kind":job["request"]["message_type"],"original_caller":job["request"]["from_actor"],"required_result_mode":if chain {"chain"} else {"plain"},
        "command_prefix_argv":prefix,"configured_peers":peers,"chain_restricted":!allowed.is_empty(),"available_chain_targets":available});
    let example = |powershell| example(&executable, &root, agent, chain, powershell);
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
    #[test]
    fn claude_append_preserves_custom_text_and_file_once() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("custom.txt"), "USER_FILE_RULE").unwrap();
        let request = json!({"config":{"project_root":root.path(),"project_id":"test",
            "agents":{"claude":{"provider":"claude","workspace_path":root.path()},"codex":{"provider":"codex"}}}});
        let args = vec![
            "--model",
            "user-model",
            "--append-system-prompt",
            "USER_INLINE_RULE",
            "--append-system-prompt-file",
            "custom.txt",
        ]
        .into_iter()
        .map(str::to_owned)
        .collect();
        let mut store = crate::store::Store::open(root.path()).unwrap();
        let result = claude_arguments(&request, "claude", args, &mut store).unwrap();
        assert_eq!(&result[..2], &["--model", "user-model"]);
        assert_eq!(
            result
                .iter()
                .filter(|s| s.as_str() == "--append-system-prompt-file")
                .count(),
            1
        );
        let text = std::fs::read_to_string(result.last().unwrap()).unwrap();
        assert!(text.starts_with("USER_INLINE_RULE\nUSER_FILE_RULE"));
        assert!(text.contains("<ccb_interactive_coordination>"));
        assert!(text.contains("codex"));
    }
    #[test]
    fn early_host_bootstrap_keeps_the_configured_team() {
        let root = tempfile::tempdir().unwrap();
        let config = json!({"project_id":"test","agents":{"codex":{"provider":"codex"}},
            "herdr":{"config":{"project_id":"test","agents":{"codex":{"provider":"codex"},"pi":{"provider":"pi"},"claude":{"provider":"claude"}}}}});
        let context = interactive(
            "codex",
            &config,
            root.path(),
            &std::env::current_exe().unwrap(),
        )
        .unwrap();
        assert!(context.contains("\"name\":\"pi\""));
        assert!(context.contains("\"name\":\"claude\""));
    }
}
