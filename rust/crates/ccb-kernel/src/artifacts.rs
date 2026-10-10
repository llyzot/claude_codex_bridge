//! Text artifacts participate in the same journal as their owning records.
use crate::{
    parse_time,
    records::{string, truthy},
    Kernel,
};
use anyhow::{Context, Result};
use chrono::{Duration, SecondsFormat};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

pub fn preview(text: &str, limit: usize) -> String {
    let body = text.trim();
    if body.chars().count() <= limit {
        body.to_owned()
    } else {
        format!(
            "{}\n...[truncated]",
            body.chars().take(limit).collect::<String>().trim_end()
        )
    }
}
pub fn validate_reference(kernel: &Kernel, reference: &Value) -> Result<Value> {
    let mut reference = reference.clone();
    anyhow::ensure!(reference.is_object(), "body_artifact must be an object");
    let candidate = std::path::Path::new(string(&reference, "path")?)
        .canonicalize()
        .context("text artifact path does not exist")?;
    let probe = kernel
        .store
        .owned_path("artifacts/text/reference/probe.txt")?;
    let base = probe.parent().unwrap().parent().unwrap();
    let relative = candidate
        .strip_prefix(base)
        .context("text artifact path escapes CCB artifact directory")?;
    let owned = kernel.store.owned_path(&format!(
        "artifacts/text/{}",
        relative.to_string_lossy().replace('\\', "/")
    ))?;
    let data = std::fs::read(&owned)?;
    if !reference["bytes"].is_null() {
        let size = reference["bytes"]
            .as_u64()
            .or_else(|| reference["bytes"].as_str().and_then(|s| s.parse().ok()))
            .context("invalid text artifact byte size")?;
        anyhow::ensure!(
            size == data.len() as u64,
            "text artifact byte size mismatch"
        );
    }
    let digest = format!("{:x}", Sha256::digest(&data));
    if let Some(expected) = reference["sha256"].as_str().filter(|s| !s.is_empty()) {
        anyhow::ensure!(expected == digest, "text artifact sha256 mismatch");
    }
    let path = owned.to_string_lossy().into_owned();
    #[cfg(windows)]
    let path = if let Some(unc) = path.strip_prefix("\\\\?\\UNC\\") {
        format!("\\\\{unc}")
    } else {
        path.strip_prefix("\\\\?\\").unwrap_or(&path).to_owned()
    };
    reference["path"] = json!(path);
    reference["bytes"] = json!(data.len());
    reference["sha256"] = json!(digest);
    if reference.get("encoding").is_none() {
        reference["encoding"] = json!("utf-8");
    }
    if reference.get("preview").is_none() {
        reference["preview"] = json!(preview(&String::from_utf8_lossy(&data), 1200));
    }
    Ok(reference)
}
fn safe(value: &str, fallback: &str) -> String {
    let s = value
        .trim()
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '-'
            }
        })
        .collect::<String>();
    let s = s.trim_matches(['-', '_']).to_lowercase();
    if s.is_empty() {
        fallback.into()
    } else {
        s
    }
}
pub fn write(
    kernel: &mut Kernel,
    body: &str,
    kind: &str,
    owner: &str,
    timestamp: &str,
) -> Result<Value> {
    let kind = safe(kind, "text");
    let id = format!("art_{}", &uuid::Uuid::new_v4().simple().to_string()[..16]);
    let relative = format!("artifacts/text/{kind}/{}-{id}.txt", safe(owner, "unknown"));
    let expires = parse_time(timestamp)
        .context("invalid artifact timestamp")?
        .checked_add_signed(Duration::seconds(86400))
        .context("artifact timestamp overflow")?
        .to_rfc3339_opts(SecondsFormat::AutoSi, true);
    let path = kernel.store.owned_path(&relative)?;
    let path = path.to_string_lossy().into_owned();
    #[cfg(windows)]
    let path = if let Some(unc) = path.strip_prefix("\\\\?\\UNC\\") {
        format!("\\\\{unc}")
    } else {
        path.strip_prefix("\\\\?\\").unwrap_or(&path).to_owned()
    };
    kernel.store.save_bytes(&relative, body.as_bytes())?;
    Ok(
        json!({"schema_version":1,"kind":kind,"artifact_id":id,"path":path,"bytes":body.len(),
        "sha256":format!("{:x}",Sha256::digest(body.as_bytes())),"encoding":"utf-8","preview":preview(body,1200),
        "created_at":timestamp,"expires_at":expires}),
    )
}
pub fn stub(prefix: &str, artifact: &Value, show: bool, instruction: Option<&str>) -> String {
    let mut lines = vec![
        prefix.trim_end().to_owned(),
        format!("Full text: {}", artifact["path"].as_str().unwrap_or("")),
        format!("Bytes: {}", artifact["bytes"]),
        format!("SHA256: {}", artifact["sha256"].as_str().unwrap_or("")),
    ];
    let text = artifact["preview"].as_str().unwrap_or("").trim_end();
    let preview = show && !text.is_empty();
    if preview {
        lines.extend([String::new(), "Preview:".into(), text.into()]);
    }
    lines.push(String::new());
    lines.push(instruction.unwrap_or(if preview {"Instruction: read the full text file above before acting when the preview is insufficient."}
        else {"Instruction: read the full text file above before acting."}).into());
    lines.join("\n").trim_end().to_owned()
}
pub fn spill_completion(
    kernel: &mut Kernel,
    job: &Value,
    decision: &Value,
    timestamp: &str,
) -> Result<Value> {
    let mut result = decision.clone();
    if decision["diagnostics"]["reply_artifact"].is_object() {
        return Ok(result);
    }
    let forced = truthy(&job["request"]["route_options"]["artifact_reply"]);
    let text = decision["reply"].as_str().unwrap_or("");
    if !forced && text.len() <= 4096 {
        return Ok(result);
    }
    let id = string(job, "job_id")?;
    let artifact = write(kernel, text, "completion-reply", id, timestamp)?;
    let diagnostics = result["diagnostics"]
        .as_object_mut()
        .context("diagnostics must be an object")?;
    let provider = job["provider"].as_str().unwrap_or("provider").trim();
    let kimi_empty = provider.eq_ignore_ascii_case("kimi")
        && decision["reason"] == "kimi_native_turn_timeout"
        && decision["diagnostics"]["reply_chars"]
            .as_i64()
            .or_else(|| {
                decision["diagnostics"]["reply_chars"]
                    .as_str()
                    .and_then(|s| s.parse().ok())
            })
            .unwrap_or(0)
            == 0
        && (truthy(&decision["diagnostics"]["no_captured_reply"])
            || truthy(&decision["diagnostics"]["provider_no_reply"])
            || decision["diagnostics"]["receipt_class"] == "no_captured_reply");
    let reply = if forced {
        diagnostics.insert("artifact_reply_forced".into(), json!(true));
        if kimi_empty || text.trim().is_empty() {
            diagnostics.insert(
                "artifact_instruction".into(),
                json!("no_provider_reply_captured"),
            );
            diagnostics.insert("artifact_empty_no_provider_reply".into(), json!(true));
            if !kimi_empty {
                diagnostics.insert("no_captured_reply".into(), json!(true));
            }
            let prefix = if kimi_empty {
                format!("CCB completion reply for job {id} has no captured Kimi provider reply; --artifact-reply stored an empty artifact for transport metadata only.")
            } else {
                format!("CCB completion reply for job {id} has no captured {provider} provider reply; --artifact-reply stored an empty artifact for transport metadata only.")
            };
            stub(&prefix,&artifact,false,Some("Instruction: no provider reply was captured; do not treat this artifact as task evidence."))
        } else {
            stub(&format!("CCB completion reply for job {id} was stored as an artifact by --artifact-reply."),&artifact,false,None)
        }
    } else {
        stub(&format!("CCB completion reply for job {id} is larger than 4 KiB and was stored as an artifact."),&artifact,true,None)
    };
    diagnostics.insert("reply_artifact".into(), artifact);
    result["reply"] = json!(reply);
    Ok(result)
}
