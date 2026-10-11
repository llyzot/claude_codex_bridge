//! Native task syntax and observation. Submission stays asynchronous by default.
use anyhow::{bail, ensure, Context, Result};
use serde_json::{json, Value};
use std::{
    ffi::OsString,
    io::{IsTerminal, Read},
    path::Path,
    time::{Duration, Instant},
};

fn value<'a>(args: &'a [&str], at: &mut usize) -> Result<&'a str> {
    *at += 1;
    args.get(*at)
        .copied()
        .context("ask option requires a value")
}
fn guidance(body: String, compact: bool, silence: bool, mode: &str) -> String {
    if mode != "ask" || !compact && !silence {
        return body;
    }
    let lower = body.to_lowercase();
    let explicit = [
        "ccb reply guidance:",
        "ccb_reply_mode:",
        "output requirements",
        "reply format",
        "response format",
        "format:",
        "only reply",
        "reply only",
        "full report",
        "full output",
        "detailed report",
        "complete output",
        "include everything",
        "all details",
        "leave nothing out",
        "verbatim",
        "do not summarize",
        "do not abbreviate",
        "完整输出",
        "不要总结",
        "不要压缩",
        "不要精简",
        "不要省略",
        "逐字返回",
        "逐字",
        "原样返回",
        "保留原文",
        "完整日志",
        "完整报告",
        "详细报告",
        "全文",
    ];
    if explicit.iter().any(|hint| lower.contains(hint)) {
        return body;
    }
    format!(
        "{}\n\nCCB_REPLY_MODE: {}",
        body.trim_end(),
        if silence { "silent" } else { "compact" }
    )
}
pub fn ask(args: &[OsString]) -> Result<Value> {
    let args = args
        .iter()
        .map(|s| s.to_str().context("ask requires UTF-8 arguments"))
        .collect::<Result<Vec<_>>>()?;
    let (mut target, mut project, mut task, mut reply_to) = (None, None, None, None);
    let mut actor = std::env::var("CCB_AGENT_NAME").unwrap_or_else(|_| "user".into());
    let mut sender_set = false;
    let (mut chain, mut compact, mut silence, mut artifact, mut inline, mut artifact_reply) =
        (false, false, false, false, false, false);
    let mut mode = "ask".to_owned();
    let mut body = Vec::new();
    let mut i = 0;
    while i < args.len() {
        let token = args[i];
        if token == "--" {
            i += 1;
            if target.is_none() {
                target = Some(*args.get(i).context("ask requires a target")?);
                i += 1;
            }
            body.extend_from_slice(&args[i..]);
            break;
        }
        if token.starts_with("--") {
            match token {
                "--chain" => chain = true,
                "--compact" => compact = true,
                "--silence" => silence = true,
                "--artifact-request" => artifact = true,
                "--inline-request" => inline = true,
                "--artifact-reply" => artifact_reply = true,
                "--artifact-io" => {
                    artifact = true;
                    artifact_reply = true;
                }
                "--from" => {
                    actor = value(&args, &mut i)?.to_owned();
                    sender_set = true;
                }
                "--project" => project = Some(value(&args, &mut i)?.to_owned()),
                "--task-id" => task = Some(value(&args, &mut i)?.to_owned()),
                "--reply-to" => reply_to = Some(value(&args, &mut i)?.to_owned()),
                "--mode" => mode = value(&args, &mut i)?.to_owned(),
                "--sync" | "--async" => {
                    bail!("ask submits asynchronously by default; omit {token}")
                }
                _ => bail!("unknown ask option: {token}"),
            }
        } else if target.is_none() {
            target = Some(token);
        } else if token == "from" && !sender_set {
            actor = value(&args, &mut i)?.to_owned();
            sender_set = true;
        } else {
            body.extend_from_slice(&args[i..]);
            break;
        }
        i += 1;
    }
    let target = target
        .context("ask requires a target")?
        .trim()
        .to_lowercase();
    ensure!(!target.is_empty(), "ask target cannot be empty");
    ensure!(
        !(artifact && inline),
        "--artifact-request and --inline-request cannot be combined"
    );
    let mut body = body.join(" ");
    if !std::io::stdin().is_terminal() {
        let mut bytes = Vec::new();
        std::io::stdin()
            .take(512 * 1024 + 1)
            .read_to_end(&mut bytes)?;
        ensure!(bytes.len() <= 512 * 1024, "ask stdin exceeds 512 KiB");
        let bytes = bytes.strip_prefix(&[0xef, 0xbb, 0xbf]).unwrap_or(&bytes);
        let text = std::str::from_utf8(bytes)
            .context("ask stdin must be UTF-8")?
            .trim();
        if !text.is_empty() {
            if !body.is_empty() {
                body.push(' ');
            }
            body.push_str(text);
        }
    }
    let body = body.trim().to_owned();
    ensure!(!body.is_empty(), "ask requires a nonempty message or stdin");
    let body = guidance(body, compact, silence, &mode);
    let mut route = json!({});
    if chain {
        route["mode"] = json!("chain");
    }
    if artifact {
        route["artifact_request"] = json!(true);
    }
    if artifact_reply {
        route["artifact_reply"] = json!(true);
    }
    Ok(
        json!({"project_id":project,"to_agent":target,"from_actor":actor.trim().to_lowercase(),"body":body,
        "task_id":task,"reply_to":reply_to,"message_type":mode,"delivery_scope":if target=="all" {"broadcast"} else {"single"},
        "silence_on_success":silence,"route_options":route,"body_transport":if inline {"inline"} else if artifact {"artifact"} else {"auto"}}),
    )
}
pub fn watch(root: &Path, args: &[OsString]) -> Result<()> {
    let values = args
        .iter()
        .map(|s| s.to_str().context("invalid watch argument"))
        .collect::<Result<Vec<_>>>()?;
    let mut target = values
        .first()
        .context("watch requires a target")?
        .to_string();
    let (mut wait, mut cursor, mut timeout) = (false, 0u64, 60000u64);
    let mut i = 1;
    while i < values.len() {
        match values[i] {
            "--wait" => wait = true,
            "--cursor" => {
                cursor = value(&values, &mut i)?
                    .parse()
                    .context("watch cursor must be a nonnegative integer")?
            }
            "--timeout-ms" => {
                timeout = value(&values, &mut i)?
                    .parse()
                    .context("watch timeout must be a nonnegative integer")?
            }
            option => bail!("unknown watch option: {option}"),
        }
        i += 1;
    }
    ensure!(timeout <= 86400000, "watch timeout exceeds one day");
    let start = Instant::now();
    loop {
        let mut row = crate::cli::request(root, "watch", json!({"target":target,"cursor":cursor}))?;
        target = row["job_id"]
            .as_str()
            .context("watch response has no Job identity")?
            .to_owned();
        cursor = row["cursor"]
            .as_u64()
            .context("watch response has no cursor")?;
        if !wait || row["terminal"] == true {
            println!("{}", crate::cli::request_preview(&row));
            return Ok(());
        }
        if start.elapsed() >= Duration::from_millis(timeout) {
            row["observation_timed_out"] = json!(true);
            println!("{}", crate::cli::request_preview(&row));
            bail!("watch observation timed out; the Job remains available under its existing identity");
        }
        std::thread::sleep(
            Duration::from_millis(100)
                .min(Duration::from_millis(timeout).saturating_sub(start.elapsed())),
        );
    }
}
