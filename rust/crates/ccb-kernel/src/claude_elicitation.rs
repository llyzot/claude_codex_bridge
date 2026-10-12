//! Claude SDK 0.3.296 elicitation envelopes and MCP 2025-11-25 responses.
//! No navigation, schema retrieval or automatic consent. Form bodies stay transient.
use anyhow::{ensure, Context, Result};
use serde_json::{json, Value};

fn text(value: &Value, limit: usize) -> bool {
    value
        .as_str()
        .is_some_and(|s| !s.trim().is_empty() && s.len() <= limit && !s.contains('\0'))
}
fn local_schema(value: &Value, depth: usize) -> Result<()> {
    ensure!(depth <= 12, "MCP form schema exceeds nesting limit");
    match value {
        Value::Object(map) => {
            for (key, value) in map {
                ensure!(
                    !["$ref", "$dynamicRef", "$recursiveRef"].contains(&key.as_str()),
                    "MCP form schema references are unsupported"
                );
                local_schema(value, depth + 1)?;
            }
        }
        Value::Array(items) => {
            for item in items {
                local_schema(item, depth + 1)?;
            }
        }
        _ => {}
    }
    Ok(())
}
fn form(schema: &Value) -> Result<jsonschema::Validator> {
    ensure!(
        schema["type"] == "object",
        "MCP form requires an object schema"
    );
    let properties = schema["properties"]
        .as_object()
        .context("MCP form properties missing")?;
    ensure!(properties.len() <= 64, "too many MCP form properties");
    local_schema(schema, 0)?;
    for (key, spec) in properties {
        ensure!(text(&json!(key), 256), "invalid MCP form property name");
        ensure!(
            ["string", "number", "integer", "boolean", "array"]
                .contains(&spec["type"].as_str().unwrap_or("")),
            "MCP form requires primitive fields or selection arrays"
        );
        if spec["type"] == "array" {
            let items = &spec["items"];
            let simple = items["type"] == "string"
                && items["enum"].as_array().is_some_and(|a| {
                    !a.is_empty() && a.len() <= 64 && a.iter().all(Value::is_string)
                });
            let titled = items["anyOf"].as_array().is_some_and(|a| {
                !a.is_empty() && a.len() <= 64 && a.iter().all(|v| v["const"].is_string())
            });
            ensure!(
                simple || titled,
                "MCP selection array requires string choices"
            );
        }
    }
    jsonschema::options()
        .should_validate_formats(true)
        .build(schema)
        .map_err(|_| anyhow::anyhow!("invalid MCP form schema"))
}
pub(crate) fn validate(params: &Value) -> Result<()> {
    ensure!(
        params.to_string().len() <= 65536,
        "Claude elicitation exceeds size limit"
    );
    ensure!(
        text(&params["mcp_server_name"], 256) && text(&params["message"], 8192),
        "invalid Claude elicitation identity/message"
    );
    let mode = params["mode"].as_str().unwrap_or("form");
    ensure!(
        params["mode"].is_null() || params["mode"].is_string(),
        "invalid Claude elicitation mode"
    );
    match mode {
        "form" => {
            ensure!(
                params["url"].is_null(),
                "form elicitation cannot contain a URL"
            );
            form(&params["requested_schema"])?;
        }
        "url" => {
            ensure!(
                text(&params["url"], 8192)
                    && text(&params["elicitation_id"], 256)
                    && params["requested_schema"].is_null(),
                "invalid URL elicitation"
            );
            let uri: tungstenite::http::Uri = params["url"]
                .as_str()
                .unwrap()
                .parse()
                .map_err(|_| anyhow::anyhow!("invalid elicitation URL"))?;
            ensure!(
                matches!(uri.scheme_str(), Some("https" | "http"))
                    && uri.host().is_some_and(|h| !h.is_empty()),
                "elicitation requires an HTTP(S) URL"
            );
        }
        _ => anyhow::bail!("unsupported Claude elicitation mode"),
    }
    Ok(())
}
pub(crate) fn answer(params: &Value, result: &Value) -> Result<Value> {
    validate(params)?;
    let map = result
        .as_object()
        .context("Claude elicitation answer must be an object")?;
    ensure!(
        result.to_string().len() <= 65536
            && map
                .keys()
                .all(|k| ["action", "content"].contains(&k.as_str())),
        "invalid Claude elicitation answer fields/size"
    );
    let action = result["action"]
        .as_str()
        .context("Claude elicitation action missing")?;
    ensure!(
        ["accept", "decline", "cancel"].contains(&action),
        "invalid Claude elicitation action"
    );
    if action != "accept" || params["mode"] == "url" {
        ensure!(
            result["content"].is_null(),
            "declined or URL elicitation cannot transmit form content"
        );
        return Ok(json!({"action":action}));
    }
    let content = result["content"]
        .as_object()
        .context("accepted MCP form requires content")?;
    let schema = &params["requested_schema"];
    ensure!(
        content
            .keys()
            .all(|k| schema["properties"].get(k).is_some()),
        "MCP answer contains an unrequested field"
    );
    ensure!(
        form(schema)?.is_valid(&result["content"]),
        "invalid MCP form answer"
    );
    Ok(json!({"action":action,"content":result["content"]}))
}
