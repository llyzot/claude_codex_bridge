//! Retry classification and caller-visible result policy; no provider IO.
use crate::{callback, records, Kernel};
use anyhow::Result;
use serde_json::{json, Value};
fn text(v: &Value) -> String {
    if !records::truthy(v) {
        String::new()
    } else {
        v.as_str()
            .map(str::to_owned)
            .unwrap_or_else(|| v.to_string())
    }
}
fn norm(v: &Value) -> String {
    text(v).trim().to_lowercase()
}
fn policy_bool(v: &Value, default: bool) -> bool {
    if let Some(value) = v.as_bool() {
        return value;
    }
    match norm(v).as_str() {
        "1" | "true" | "yes" | "on" => true,
        "0" | "false" | "no" | "off" => false,
        _ => default,
    }
}
fn reason(decision: &Value) -> String {
    let r = norm(&decision["reason"]);
    if r.is_empty() {
        "api_error".into()
    } else {
        r
    }
}
fn nonretryable(d: &Value) -> Option<&'static str> {
    if d["status"] != "failed" {
        return None;
    }
    let tokens = [
        &d["reason"],
        &d["diagnostics"]["error_type"],
        &d["diagnostics"]["error_code"],
    ]
    .map(|v| {
        norm(v)
            .chars()
            .filter(|c| c.is_alphanumeric())
            .collect::<String>()
    });
    let message = ["error_message", "fault_message", "error", "message", "text"]
        .iter()
        .map(|k| norm(&d["diagnostics"][*k]))
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join(" ");
    let categories = [
        (
            "authentication",
            &[
                "unauthorized",
                "authenticationerror",
                "authenticationfailed",
                "invalidapikey",
                "invalidauthtoken",
                "invalidtoken",
                "notauthenticated",
                "notloggedin",
                "loginrequired",
                "missingapikey",
            ][..],
            &[
                "unauthorized",
                "not logged in",
                "login required",
                "authentication failed",
                "invalid api key",
                "invalid auth",
                "invalid token",
                "run codex login",
                "run login",
            ][..],
        ),
        (
            "permission",
            &[
                "permissiondenied",
                "accessdenied",
                "forbidden",
                "notauthorized",
            ][..],
            &[
                "permission denied",
                "access denied",
                "forbidden",
                "not authorized",
            ][..],
        ),
        (
            "billing",
            &[
                "insufficientquota",
                "quotaexceeded",
                "paymentrequired",
                "billinghardlimitreached",
                "billingnotactive",
                "insufficientbalance",
                "balanceexhausted",
                "creditbalancetoolow",
            ][..],
            &[
                "insufficient quota",
                "quota exceeded",
                "payment required",
                "insufficient balance",
                "billing",
                "credit balance too low",
            ][..],
        ),
    ];
    for (kind, codes, _) in categories {
        if tokens.iter().any(|t| codes.contains(&t.as_str())) {
            return Some(kind);
        }
    }
    for (kind, _, markers) in categories {
        if markers.iter().any(|m| message.contains(m)) {
            return Some(kind);
        }
    }
    None
}
pub fn record(kernel: &mut Kernel, args: &Value) -> Result<Value> {
    let mut job = args["job"].clone();
    let attempt = records::dispatch(
        &mut kernel.store,
        "find_last",
        &json!({"stream":"attempts","fields":{"job_id":job["job_id"]}}),
    )?;
    if attempt.is_null() {
        return Ok(json!({"reply_id":null,"continuation":null}));
    }
    let mut edge = callback::lookup(kernel, json!({"child_job_id":job["job_id"]}))?;
    if edge.is_null() {
        edge = callback::lookup(kernel, json!({"child_message_id":attempt["message_id"]}))?;
    }
    if !edge.is_null() {
        job["request"]["silence_on_success"] = json!(false);
    }
    let existing = records::dispatch(
        &mut kernel.store,
        "find_last",
        &json!({"stream":"replies","fields":{"attempt_id":attempt["attempt_id"],"diagnostics.result_authority":"terminal_job"}}),
    )?;
    let reply_id = if !existing.is_null() {
        existing["reply_id"].clone()
    } else {
        let mut reply_args = args.clone();
        reply_args["job"] = job.clone();
        reply_args["result_authority"] = json!("terminal_job");
        if !edge.is_null() {
            reply_args["deliver_to_caller"] = json!(false);
        }
        crate::bureau::reply(kernel, &reply_args)?
    };
    let continuation = if edge.is_null() {
        Value::Null
    } else {
        callback::prepare(
            kernel,
            &json!({"edge":edge,"child_job":job,"child_reply_id":reply_id,"finished_at":args["finished_at"]}),
        )?
    };
    if records::truthy(&job["request"]["route_options"]["chain_edge_id"]) {
        callback::done(
            kernel,
            &json!({"edge_id":job["request"]["route_options"]["chain_edge_id"],"finished_at":args["finished_at"]}),
        )?;
    }
    Ok(json!({"reply_id":reply_id,"continuation":continuation}))
}
fn configured(v: &Value, defaults: &[&str], needle: &str) -> bool {
    let values = v
        .as_array()
        .map(|a| {
            a.iter()
                .map(norm)
                .filter(|s| !s.is_empty())
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    if values.is_empty() {
        defaults.contains(&needle)
    } else {
        values.iter().any(|s| s == needle)
    }
}
fn runtime_reason(d: &Value) -> bool {
    [
        "pane_dead",
        "pane_unavailable",
        "runtime_unavailable",
        "backend_unavailable",
    ]
    .contains(&norm(&d["reason"]).as_str())
}
fn retryable(d: &Value, p: &Value, resume: bool) -> bool {
    let diag = &d["diagnostics"];
    if d["status"] == "incomplete" {
        if [
            "task_complete_empty_reply",
            "hook_stop_empty_reply",
            "pi_empty_reply",
            "omp_empty_reply",
            "cursor_empty_reply",
            "grok_empty_reply",
        ]
        .contains(&norm(&d["reason"]).as_str())
            || norm(&diag["error_type"]) == "empty_provider_reply"
        {
            return false;
        }
        return policy_bool(&diag["delivery_retryable"], false);
    }
    d["status"] == "failed"
        && nonretryable(d).is_none()
        && (policy_bool(&diag["delivery_retryable"], false)
            || configured(
                &p["retryable_reasons"],
                &["api_error", "transport_error"],
                &norm(&d["reason"]),
            )
            || resume
                && policy_bool(&p["retry_runtime_when_resume_supported"], true)
                && configured(
                    &p["retryable_runtime_reasons"],
                    &[
                        "pane_dead",
                        "pane_unavailable",
                        "runtime_unavailable",
                        "backend_unavailable",
                    ],
                    &norm(&d["reason"]),
                )
            || ["api_error", "transport_error", "provider_api_error"]
                .contains(&norm(&diag["error_type"]).as_str()))
}
fn detail(d: &Value) -> String {
    let error = ["error_message", "fault_message", "error"]
        .iter()
        .map(|k| text(&d["diagnostics"][*k]))
        .find(|s| !s.is_empty())
        .unwrap_or_default();
    let entries = [
        ("reason", text(&d["reason"])),
        ("error_type", text(&d["diagnostics"]["error_type"])),
        ("error_code", text(&d["diagnostics"]["error_code"])),
        ("error_message", error),
        ("fault_rule_id", text(&d["diagnostics"]["fault_rule_id"])),
    ];
    let parts = entries
        .into_iter()
        .filter(|(_, s)| !s.trim().is_empty())
        .map(|(k, s)| format!("{k}={}", s.trim()))
        .collect::<Vec<_>>();
    if parts.is_empty() {
        "reason=api_error".into()
    } else {
        parts.join(", ")
    }
}
fn quote(s: &str) -> String {
    if !s.is_empty()
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"_@%+=:,./-".contains(&b))
    {
        s.into()
    } else {
        format!("'{}'", s.replace('\'', "'\"'\"'"))
    }
}
fn guidance(job: &Value) -> String {
    include_str!("caller_inspection.txt")
        .trim_end()
        .replace(
            "__AGENT__",
            &quote(job["agent_name"].as_str().unwrap_or("")),
        )
        .replace("__JOB__", &quote(job["job_id"].as_str().unwrap_or("")))
}
pub fn plan(kernel: &mut Kernel, args: &Value) -> Result<Value> {
    let job = &args["job"];
    let mut d = args["decision"].clone();
    let outgoing = callback::lookup(kernel, json!({"parent_job_id":job["job_id"]}))?;
    if job["request"]["message_type"] == "reply_delivery"
        || records::truthy(&job["provider_options"]["reply_delivery"])
    {
        return Ok(json!({"action":"delivery","decision":d,"events":[]}));
    }
    if !outgoing.is_null()
        && !["failed", "timed_out", "done"].contains(&outgoing["state"].as_str().unwrap_or(""))
    {
        return Ok(json!({"action":"delegated","decision":d,"events":[]}));
    }
    let attempt = records::dispatch(
        &mut kernel.store,
        "find_last",
        &json!({"stream":"attempts","fields":{"job_id":job["job_id"]}}),
    )?;
    let message = records::dispatch(
        &mut kernel.store,
        "find_last",
        &json!({"stream":"messages","fields":{"message_id":attempt["message_id"]}}),
    )?;
    let p = &message["retry_policy"];
    let number = attempt["retry_index"].as_i64().unwrap_or(0) + 1;
    let max = p["max_attempts"]
        .as_i64()
        .or_else(|| p["max_attempts"].as_str().and_then(|s| s.parse().ok()))
        .unwrap_or(1)
        .max(1);
    let eligible = !attempt.is_null()
        && !message.is_null()
        && norm(&p["mode"]) == "auto"
        && retryable(&d, p, args["supports_resume"] == true);
    let context = json!({"message_id":attempt["message_id"],"attempt_id":attempt["attempt_id"],"attempt_number":number,"max_attempts":max,"reason":reason(&d)});
    if eligible && number < max && !records::truthy(&args["scheduling_error"]) {
        return Ok(json!({"action":"retry","decision":d,"events":[],"context":context}));
    }
    let mut events = Vec::new();
    let agent = job["agent_name"].as_str().unwrap_or("");
    let id = job["job_id"].as_str().unwrap_or("");
    if eligible {
        let error = text(&args["scheduling_error"]);
        let drill = norm(&d["diagnostics"]["error_type"]) == "fault_injection";
        let runtime = runtime_reason(&d);
        let detail = detail(&d);
        let tail = "Execute this task locally or resend it to another healthy registered agent.";
        let body = if !error.is_empty() {
            let start = if drill {
                format!("Fault injection drill forced agent '{agent}' to fail on attempt")
            } else if runtime {
                format!(
                    "Delivery to agent '{agent}' hit a retryable runtime/pane failure on attempt"
                )
            } else {
                format!("Delivery to agent '{agent}' hit a retryable provider/API error on attempt")
            };
            format!("{start} {number}/{max} ({detail}), but automatic retry could not be scheduled ({error}). {tail}")
        } else if drill {
            format!("Fault injection drill forced agent '{agent}' to fail after {number} attempts ({detail}). {tail}")
        } else if runtime {
            format!("Delivery to agent '{agent}' failed after {number} attempts because its runtime/pane could not be recovered ({detail}). {tail}")
        } else {
            format!("Delivery to agent '{agent}' failed after {number} attempts because the provider/API connection kept failing ({detail}). {tail}")
        };
        d["reply"] = json!(body);
        d["diagnostics"]["auto_retry"] = json!({"attempt_number":number,"max_attempts":max,"scheduling_error":if error.is_empty() {Value::Null} else {json!(error)}});
        let mut payload = context.clone();
        if !error.is_empty() {
            payload["error"] = json!(error);
        }
        events.push(json!({"type":if error.is_empty() {"job_retry_exhausted"} else {"job_retry_failed"},"payload":payload}));
    } else if d["status"] == "incomplete" && norm(&d["reason"]) == "timeout" {
        d["diagnostics"]["timeout_notice"] =
            json!({"job_id":id,"agent_name":agent,"action":"inspect_running_attempt"});
        d["reply"]=json!(format!("Delivery to agent '{agent}' timed out before a confirmed terminal reply. The task may still be running in that agent session. Inspect job '{id}' in the agent pane or queue/trace views before deciding whether to continue or retry."));
    } else if let Some(kind) = nonretryable(&d) {
        let (category, remediation) = match kind {
            "authentication" => (
                "a non-retryable authentication/login error",
                "Fix the provider login/session before retrying locally or resending it.",
            ),
            "permission" => (
                "a non-retryable permission/access error",
                "Fix the provider account permissions before retrying locally or resending it.",
            ),
            _ => (
                "a non-retryable quota/billing error",
                "Fix the provider billing/quota before retrying locally or resending it.",
            ),
        };
        d["reply"]=json!(format!("Delivery to agent '{agent}' failed because the provider/API reported {category} ({}). {remediation} You can also execute this task locally or resend it to another healthy registered agent.",detail(&d)));
        d["diagnostics"]["auto_retry"] =
            json!({"nonretryable_api_failure":true,"classification":kind});
        events.push(json!({"type":"job_retry_skipped_nonretryable","payload":{"reason":reason(&d),"classification":kind,"error_code":if text(&d["diagnostics"]["error_code"]).trim().is_empty() {Value::Null} else {json!(text(&d["diagnostics"]["error_code"]).trim())}}}));
    }
    let mut child = callback::lookup(kernel, json!({"child_job_id":job["job_id"]}))?;
    if child.is_null() {
        child = callback::lookup(kernel, json!({"child_message_id":attempt["message_id"]}))?;
    }
    let deliver =
        !(job["status"] == "completed" && records::truthy(&job["request"]["silence_on_success"]));
    if child.is_null() && deliver {
        if text(&d["reply"]).trim().is_empty()
            && !records::truthy(&d["diagnostics"]["reply_artifact"])
            && ["completed", "incomplete"].contains(&d["status"].as_str().unwrap_or(""))
        {
            let reason = text(&d["reason"]);
            let r = if reason.trim().is_empty() {
                "unknown"
            } else {
                reason.trim()
            };
            let mut lines=vec![format!("This execution of job {id} on agent {agent} has ended, but CCB received no usable reply body."),"This does not mean the task was not executed.".into(),format!("Recorded terminal status/reason: {}/{}.",d["status"].as_str().unwrap_or(""),r),guidance(job)];
            if !text(&d["provider_turn_ref"]).trim().is_empty() {
                lines.push(format!(
                    "Session/turn reference: {}",
                    text(&d["provider_turn_ref"]).trim()
                ));
            }
            d["reply"] = json!(lines.join("\n"));
            for (key, value) in [
                ("notice", json!(true)),
                ("notice_kind", json!("empty_result")),
                ("notice_job_id", job["job_id"].clone()),
                ("notice_agent", job["agent_name"].clone()),
                ("terminal_status", job["status"].clone()),
                ("terminal_reason", json!(reason)),
            ] {
                d["diagnostics"][key] = value;
            }
        } else if ["failed", "incomplete"].contains(&d["status"].as_str().unwrap_or(""))
            && !records::truthy(&d["diagnostics"]["caller_inspection"])
            && d["diagnostics"]["notice_kind"] != "empty_result"
        {
            let body = text(&d["reply"]).trim_end().to_owned();
            let identity = format!(
                "CCB job {id}, agent {agent}, status/reason: {}/{}.",
                job["status"].as_str().unwrap_or(""),
                d["reason"]
                    .as_str()
                    .filter(|s| !s.is_empty())
                    .unwrap_or("unknown")
            );
            d["reply"] = json!([body, identity, guidance(job)]
                .into_iter()
                .filter(|s| !s.is_empty())
                .collect::<Vec<_>>()
                .join("\n\n"));
            d["diagnostics"]["caller_inspection"] = json!(true);
        }
    }
    Ok(
        json!({"action":"reply","decision":d,"events":events,"child_edge":child,"deliver_to_caller":deliver,"context":context}),
    )
}
