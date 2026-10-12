//! One interpretation of config shortcuts for protocol and original terminal launch.
use anyhow::{ensure, Context, Result};
use serde_json::{json, Value};

fn optional(spec: &Value, key: &str) -> Result<Option<String>> {
    if spec[key].is_null() {
        return Ok(None);
    }
    let text = spec[key]
        .as_str()
        .context(format!("{key} must be a string"))?
        .trim();
    ensure!(!text.is_empty() && !text.contains('\0'), "invalid {key}");
    Ok(Some(text.into()))
}
fn model_prefix(provider: &str, model: &str) -> Vec<String> {
    vec![
        if provider == "codex" { "-m" } else { "--model" }.into(),
        model.into(),
    ]
}
fn thinking_prefix(provider: &str, thinking: &str) -> Vec<String> {
    if provider == "codex" {
        vec![
            "-c".into(),
            format!("model_reasoning_effort={}", json!(thinking)),
        ]
    } else {
        vec![
            if provider == "pi" {
                "--thinking"
            } else {
                "--effort"
            }
            .into(),
            thinking.into(),
        ]
    }
}
fn has_model(provider: &str, args: &[String]) -> bool {
    args.iter().any(|v| {
        v == "--model" || v.starts_with("--model=") || provider == "codex" && v.starts_with("-m")
    })
}
fn has_thinking(provider: &str, args: &[String]) -> bool {
    let key = if provider == "pi" {
        "--thinking"
    } else {
        "--effort"
    };
    if provider != "codex" {
        return args
            .iter()
            .any(|v| v == key || v.starts_with(&format!("{key}=")));
    }
    args.iter().enumerate().any(|(i, arg)| {
        let value = if arg == "-c" || arg == "--config" {
            args.get(i + 1).map(String::as_str)
        } else {
            arg.strip_prefix("--config=")
                .or_else(|| arg.strip_prefix("-c"))
        };
        value.is_some_and(|v| v.trim_start().starts_with("model_reasoning_effort="))
    })
}
pub(crate) fn normalize(raw: &Value) -> Result<Value> {
    let mut spec = raw.clone();
    ensure!(spec.is_object(), "agent launch options must be an object");
    let provider = spec["provider"]
        .as_str()
        .context("provider missing")?
        .to_owned();
    ensure!(
        ["pi", "codex", "claude"].contains(&provider.as_str()),
        "unsupported native launch provider"
    );
    let model = optional(&spec, "model")?;
    let thinking = optional(&spec, "thinking")?.map(|s| s.to_lowercase());
    if let Some(value) = &thinking {
        let levels: &[&str] = match provider.as_str() {
            "pi" => &["off", "minimal", "low", "medium", "high", "xhigh", "max"],
            "claude" => &["low", "medium", "high", "xhigh", "max"],
            _ => &[
                "none", "minimal", "low", "medium", "high", "xhigh", "max", "ultra",
            ],
        };
        ensure!(
            levels.contains(&value.as_str()),
            "invalid thinking level for {provider}"
        );
        spec["thinking"] = json!(value);
    }
    if let Some(value) = &model {
        spec["model"] = json!(value);
    }
    let mut args: Vec<String> = if spec["startup_args"].is_null() {
        vec![]
    } else {
        serde_json::from_value(spec["startup_args"].clone())
            .context("startup_args must be a string array")?
    };
    ensure!(
        args.iter().all(|v| !v.contains('\0')),
        "startup_args contains NUL"
    );
    // Legacy AgentSpec records include generated prefixes. Remove only the exact
    // canonical prefix once; any remaining shortcut switch is a conflict.
    for prefix in [
        model.as_ref().map(|v| model_prefix(&provider, v)),
        thinking.as_ref().map(|v| thinking_prefix(&provider, v)),
    ]
    .into_iter()
    .flatten()
    {
        if args.starts_with(&prefix) {
            args.drain(..prefix.len());
        }
    }
    ensure!(
        model.is_none() || !has_model(&provider, &args),
        "model conflicts with startup_args"
    );
    ensure!(
        thinking.is_none() || !has_thinking(&provider, &args),
        "thinking conflicts with startup_args"
    );
    if !spec["startup_args"].is_null() {
        spec["startup_args"] = json!(args);
    }
    if !spec["env"].is_null() {
        let env = spec["env"].as_object().context("env must be an object")?;
        ensure!(
            env.iter().all(|(key, value)| !key.is_empty()
                && !key.contains(['=', '\0'])
                && value.as_str().is_some_and(|v| !v.contains('\0'))),
            "env must contain valid string keys and values"
        );
    }
    if provider == "codex" {
        for key in ["thread_options", "turn_options"] {
            ensure!(
                spec[key].is_null() || spec[key].is_object(),
                "{key} must be an object"
            );
        }
        for (field, key, value) in [
            ("thread_options", "model", model),
            ("turn_options", "effort", thinking),
        ] {
            if let Some(value) = value {
                ensure!(
                    spec[field][key].is_null() || spec[field][key] == value,
                    "{field}.{key} conflicts with shortcut"
                );
            }
        }
    }
    Ok(spec)
}
pub(crate) fn terminal_args(spec: &Value) -> Result<Vec<String>> {
    let spec = normalize(spec)?;
    let provider = spec["provider"].as_str().unwrap();
    let mut args: Vec<String> = spec["startup_args"]
        .as_array()
        .map(|v| v.iter().map(|s| s.as_str().unwrap().to_owned()).collect())
        .unwrap_or_default();
    if let Some(model) = spec["model"].as_str() {
        args.extend(["--model".into(), model.into()]);
    }
    if let Some(thinking) = spec["thinking"].as_str() {
        args.extend(thinking_prefix(provider, thinking));
    }
    Ok(args)
}
