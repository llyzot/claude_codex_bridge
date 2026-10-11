//! Versioned Codex wire validation. No network/file schema resolution or actions.
use anyhow::{ensure, Context, Result};
use jsonschema::Validator;
use serde_json::Value;
use std::{collections::HashMap, sync::OnceLock};

macro_rules! wire {
    ($stem:literal) => {
        (
            include_str!(concat!("../schemas/codex/", $stem, "Params.json")),
            include_str!(concat!("../schemas/codex/", $stem, "Response.json")),
        )
    };
}
const SOURCES: [(&str, (&str, &str)); 6] = [
    (
        "item/commandExecution/requestApproval",
        wire!("CommandExecutionRequestApproval"),
    ),
    (
        "item/fileChange/requestApproval",
        wire!("FileChangeRequestApproval"),
    ),
    (
        "item/permissions/requestApproval",
        wire!("PermissionsRequestApproval"),
    ),
    ("item/tool/requestUserInput", wire!("ToolRequestUserInput")),
    (
        "mcpServer/elicitation/request",
        wire!("McpServerElicitationRequest"),
    ),
    ("item/tool/call", wire!("DynamicToolCall")),
];
struct Wire {
    params: Validator,
    response: Validator,
    response_keys: Value,
}
static SCHEMAS: OnceLock<std::result::Result<HashMap<&'static str, Wire>, String>> =
    OnceLock::new();

static INITIALIZE: OnceLock<std::result::Result<Validator, String>> = OnceLock::new();

pub fn validate_initialize(params: &Value) -> Result<()> {
    let validator = INITIALIZE
        .get_or_init(|| {
            let schema: Value =
                serde_json::from_str(include_str!("../schemas/codex/InitializeParams.json"))
                    .map_err(|_| "invalid embedded initialization schema".to_owned())?;
            jsonschema::validator_for(&schema)
                .map_err(|_| "invalid embedded initialization schema".to_owned())
        })
        .as_ref()
        .map_err(|e| anyhow::anyhow!(e))?;
    ensure!(
        validator.is_valid(params),
        "invalid Codex initialization capabilities"
    );
    if let Some(optouts) = params["capabilities"]["optOutNotificationMethods"].as_array() {
        ensure!(
            !optouts.iter().any(|method| [
                "turn/completed",
                "thread/status/changed",
                "serverRequest/resolved"
            ]
            .iter()
            .any(|required| method == required)),
            "Codex initialization cannot suppress required native lifecycle notifications"
        );
    }
    Ok(())
}

fn schemas() -> Result<&'static HashMap<&'static str, Wire>> {
    SCHEMAS
        .get_or_init(|| {
            let compile = || -> Result<_> {
                let mut schemas = HashMap::new();
                for (method, (params, response)) in SOURCES {
                    let params: Value = serde_json::from_str(params)?;
                    let response: Value = serde_json::from_str(response)?;
                    schemas.insert(
                        method,
                        Wire {
                            params: jsonschema::validator_for(&params)?,
                            response: jsonschema::validator_for(&response)?,
                            response_keys: response["properties"].clone(),
                        },
                    );
                }
                Ok(schemas)
            };
            compile().map_err(|_| "embedded Codex wire schema initialization failed".into())
        })
        .as_ref()
        .map_err(|e| anyhow::anyhow!(e))
}
pub fn supported(method: &str) -> bool {
    SOURCES.iter().any(|(name, _)| *name == method)
}
pub fn validate_params(method: &str, params: &Value) -> Result<()> {
    let schema = schemas()?
        .get(method)
        .context("unsupported Codex server request")?;
    ensure!(
        schema.params.is_valid(params),
        "invalid Codex server request shape"
    );
    Ok(())
}
pub fn validate_answer(method: &str, params: &Value, response: &Value) -> Result<()> {
    let schema = schemas()?
        .get(method)
        .context("unsupported Codex server request")?;
    // Validation errors must not echo secret user answers or tool output.
    ensure!(
        schema.response.is_valid(response),
        "invalid Codex response shape"
    );
    ensure!(
        response
            .as_object()
            .unwrap()
            .keys()
            .all(|k| schema.response_keys.get(k).is_some()),
        "unknown Codex response field"
    );
    match method {
        "item/commandExecution/requestApproval" => {
            if let Some(choices) = params["availableDecisions"].as_array() {
                ensure!(
                    choices.contains(&response["decision"]),
                    "Codex decision was not offered"
                );
            }
        }
        "item/tool/requestUserInput" => {
            let questions = params["questions"]
                .as_array()
                .context("missing questions")?;
            ensure!(
                response["answers"]
                    .as_object()
                    .unwrap()
                    .keys()
                    .all(|key| questions.iter().any(|q| q["id"].as_str() == Some(key))),
                "answer refers to an unknown question"
            );
        }
        "item/permissions/requestApproval" => {
            ensure!(
                permission_subset(&params["permissions"], &response["permissions"]),
                "permission answer exceeds the requested permissions"
            );
        }
        "mcpServer/elicitation/request" => {
            if response["action"] == "accept" && params["mode"] != "url" {
                let validator = jsonschema::options()
                    .should_validate_formats(true)
                    .build(&params["requestedSchema"])
                    .map_err(|_| anyhow::anyhow!("invalid MCP form schema"))?;
                ensure!(
                    validator.is_valid(&response["content"]),
                    "invalid MCP form answer"
                );
            } else if response["action"] != "accept" {
                ensure!(
                    response["content"].is_null(),
                    "declined MCP request must have no content"
                );
            }
        }
        _ => {}
    }
    Ok(())
}
fn permission_subset(request: &Value, grant: &Value) -> bool {
    if grant.is_null() || grant == &Value::Bool(false) {
        return true;
    }
    match grant {
        Value::Object(fields) => fields
            .iter()
            .all(|(key, value)| permission_subset(&request[key], value)),
        Value::Array(values) => values.iter().all(|value| {
            request.as_array().is_some_and(|requested| {
                requested.iter().any(|r| {
                    r == value
                        || r["path"].is_object()
                            && r["path"] == value["path"]
                            && r["access"] == "write"
                            && value["access"] == "read"
                })
            })
        }),
        _ => request == grant,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    #[test]
    fn embedded_contracts_compile_and_secret_failures_do_not_echo_values() {
        assert_eq!(schemas().unwrap().len(), 6);
        assert!(
            validate_initialize(&json!({"clientInfo":{"name":"ccb","version":"test"},
            "capabilities":{"experimentalApi":true,"mcpServerOpenaiFormElicitation":true}}))
            .is_ok()
        );
        assert!(
            validate_initialize(&json!({"clientInfo":{"name":"ccb","version":"test"},
            "capabilities":{"optOutNotificationMethods":["turn/completed"]}}))
            .is_err()
        );
        let params = json!({"questions":[{"id":"q"}]});
        let error = validate_answer(
            "item/tool/requestUserInput",
            &params,
            &json!({"answers":{"q":{"answers":"secret-answer"}}}),
        )
        .unwrap_err();
        assert!(!error.to_string().contains("secret-answer"));
        let external = json!({"mode":"openai/form","requestedSchema":{"$ref":"https://example.invalid/must-not-fetch"}});
        assert!(validate_answer(
            "mcpServer/elicitation/request",
            &external,
            &json!({"action":"accept","content":{}})
        )
        .is_err());
    }
    #[test]
    fn permissions_cannot_expand_paths_network_or_write_access() {
        let request = json!({"network":{"enabled":false},"fileSystem":{"entries":[
            {"path":{"type":"path","path":"C:/test"},"access":"write"}]}});
        assert!(permission_subset(&request, &json!({})));
        assert!(permission_subset(
            &request,
            &json!({"fileSystem":{"entries":[
            {"path":{"type":"path","path":"C:/test"},"access":"read"}]}})
        ));
        assert!(!permission_subset(
            &request,
            &json!({"network":{"enabled":true}})
        ));
        assert!(!permission_subset(
            &request,
            &json!({"fileSystem":{"entries":[
            {"path":{"type":"path","path":"C:/other"},"access":"write"}]}})
        ));
    }
}
