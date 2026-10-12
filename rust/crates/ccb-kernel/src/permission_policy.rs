//! Explicit CCB permission mode. Derived settings are applied only at launch,
//! so changing auto back to manual never retains generated argv in configuration.
use anyhow::{ensure, Context, Result};
use serde_json::{json, Value};
use std::path::Path;

pub(crate) fn automatic(spec: &Value) -> Result<bool> {
    let mode = if spec["permission"].is_null() {
        "manual"
    } else {
        spec["permission"]
            .as_str()
            .context("permission must be manual or auto")?
    };
    ensure!(
        ["manual", "auto"].contains(&mode),
        "permission must be manual or auto"
    );
    if mode == "auto" {
        ensure!(
            ["codex", "claude"].contains(&spec["provider"].as_str().unwrap_or("")),
            "provider does not support native auto permission"
        );
    }
    Ok(mode == "auto")
}
fn claude_mode(args: &[Value]) -> Result<bool> {
    let mut found = false;
    for (i, arg) in args.iter().enumerate() {
        let arg = arg
            .as_str()
            .context("permission argv must contain strings")?;
        let mode = if arg == "--permission-mode" {
            Some(
                args.get(i + 1)
                    .and_then(Value::as_str)
                    .context("permission-mode requires a value")?,
            )
        } else {
            arg.strip_prefix("--permission-mode=")
        };
        if let Some(mode) = mode {
            ensure!(
                mode == "bypassPermissions",
                "auto permission conflicts with explicit Claude permission mode"
            );
            found = true;
        }
        if arg == "--dangerously-skip-permissions" {
            found = true;
        }
    }
    Ok(found)
}
fn codex_has_config_override(args: &[String]) -> bool {
    args.iter().enumerate().any(|(i, arg)| {
        let value = if arg == "-c" || arg == "--config" {
            args.get(i + 1).map(String::as_str)
        } else {
            arg.strip_prefix("--config=")
                .or_else(|| arg.strip_prefix("-c"))
        };
        value.is_some_and(|v| {
            let key = v
                .split('=')
                .next()
                .unwrap_or("")
                .trim()
                .trim_matches(['\'', '"']);
            ["approval_policy", "sandbox_mode", "permissions"].contains(&key)
                || key.starts_with("permissions.")
        })
    })
}
fn codex_has_permission_flags(args: &[String]) -> bool {
    args.iter().any(|s| {
        [
            "--full-auto",
            "--yolo",
            "--dangerously-bypass-approvals-and-sandbox",
        ]
        .contains(&s.as_str())
            || s == "--sandbox"
            || s.starts_with("--sandbox=")
            || s.starts_with("-s")
            || s == "--ask-for-approval"
            || s.starts_with("--ask-for-approval=")
            || s.starts_with("-a")
    })
}
pub(crate) fn protocol(spec: &Value) -> Result<Value> {
    let mut effective = spec.clone();
    if !automatic(spec)? {
        return Ok(effective);
    }
    match spec["provider"].as_str().unwrap() {
        "codex" => {
            let argv: Vec<String> = serde_json::from_value(spec["command"].clone())
                .context("Codex permission mode requires command argv")?;
            ensure!(
                !codex_has_config_override(&argv) && !codex_has_permission_flags(&argv),
                "auto permission conflicts with Codex config overrides"
            );
            ensure!(
                spec["turn_options"].is_null() || spec["turn_options"].is_object(),
                "turn_options must be an object"
            );
            for (key, value) in [
                ("approvalPolicy", "never"),
                ("sandbox", "danger-full-access"),
            ] {
                ensure!(
                    effective["thread_options"].is_null()
                        || effective["thread_options"].is_object(),
                    "thread_options must be an object"
                );
                ensure!(
                    effective["thread_options"][key].is_null()
                        || effective["thread_options"][key] == value,
                    "auto permission conflicts with thread_options.{key}"
                );
                effective["thread_options"][key] = json!(value);
            }
            ensure!(
                spec["turn_options"]["approvalPolicy"].is_null()
                    || spec["turn_options"]["approvalPolicy"] == "never",
                "auto permission conflicts with turn approval policy"
            );
            ensure!(
                spec["turn_options"]["sandboxPolicy"].is_null()
                    || spec["turn_options"]["sandboxPolicy"] == json!({"type":"dangerFullAccess"}),
                "auto permission conflicts with turn sandbox policy"
            );
        }
        "claude" => {
            let command = effective["command"]
                .as_array_mut()
                .context("Claude permission mode requires command argv")?;
            if !claude_mode(command)? {
                command.extend([json!("--permission-mode"), json!("bypassPermissions")]);
            }
        }
        _ => unreachable!(),
    }
    Ok(effective)
}
pub(crate) fn terminal(spec: &Value, mut args: Vec<String>) -> Result<Vec<String>> {
    if !automatic(spec)? {
        return Ok(args);
    }
    if spec["provider"] == "claude" {
        if !claude_mode(&args.iter().map(|s| json!(s)).collect::<Vec<_>>())? {
            args.extend(["--permission-mode".into(), "bypassPermissions".into()]);
        }
    } else {
        ensure!(
            !codex_has_config_override(&args),
            "auto permission conflicts with Codex config overrides"
        );
        // Arbitrary terminal permission overrides must have one explicit source.
        ensure!(
            !codex_has_permission_flags(&args),
            "auto permission conflicts with terminal permission flags"
        );
        args.extend([
            "--ask-for-approval".into(),
            "never".into(),
            "--sandbox".into(),
            "danger-full-access".into(),
        ]);
    }
    Ok(args)
}
pub(crate) fn validate(spec: &Value, project: Option<&Path>) -> Result<()> {
    let auto = automatic(spec)?;
    crate::roles::validate_command_surface(
        spec,
        project.or_else(|| spec["workspace_path"].as_str().map(Path::new)),
    )?;
    ensure!(
        !auto
            || spec["driver_kind"].is_null()
            || matches!(
                spec["driver_kind"].as_str(),
                Some("codex_app_server" | "claude_stream" | "herdr_tui")
            ),
        "auto permission is unsupported for this driver"
    );
    if auto
        && matches!(
            spec["driver_kind"].as_str(),
            Some("codex_app_server" | "claude_stream")
        )
    {
        protocol(spec)?;
    }
    Ok(())
}

pub(crate) fn validate_terminal_binding(request: &Value, name: &str, spec: &Value) -> Result<()> {
    validate(
        spec,
        request["config"]["project_root"].as_str().map(Path::new),
    )?;
    if !automatic(spec)? || !spec["tui_host_root"].is_null() {
        return Ok(());
    }
    let args: Vec<String> = serde_json::from_value(request["interactive_args"][name].clone())
        .context("Herdr auto permission requires explicit launch arguments")?;
    let proven = if spec["provider"] == "claude" {
        claude_mode(&args.iter().map(|v| json!(v)).collect::<Vec<_>>())?
    } else {
        let mut remaining = Vec::new();
        let mut index = 0;
        let mut approval = false;
        let mut sandbox = false;
        while index < args.len() {
            if args[index] == "--ask-for-approval" || args[index] == "--sandbox" {
                let expected = if args[index] == "--sandbox" {
                    "danger-full-access"
                } else {
                    "never"
                };
                ensure!(
                    args.get(index + 1).map(String::as_str) == Some(expected),
                    "Herdr auto permission has conflicting launch arguments"
                );
                if args[index] == "--sandbox" {
                    sandbox = true
                } else {
                    approval = true
                }
                index += 2;
            } else {
                remaining.push(args[index].clone());
                index += 1;
            }
        }
        terminal(spec, remaining)?;
        approval && sandbox
    };
    ensure!(
        proven,
        "Herdr permission declaration does not match launch arguments"
    );
    Ok(())
}
