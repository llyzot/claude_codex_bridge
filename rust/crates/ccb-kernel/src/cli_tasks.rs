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
const STDIN_LIMIT: usize = 512 * 1024;
const STDIN_IDLE_TIMEOUT: Duration = Duration::from_secs(15);

/// Reads an ask message from stdin until EOF. A stdin that stops producing
/// data without closing is refused instead of blocking the caller forever.
fn read_message_stdin<R: Read + Send + 'static>(mut input: R, idle: Duration) -> Result<Vec<u8>> {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut chunk = [0u8; 8192];
        loop {
            let read = input.read(&mut chunk).map(|n| chunk[..n].to_vec());
            let done = !matches!(&read, Ok(data) if !data.is_empty());
            if tx.send(read).is_err() || done {
                return;
            }
        }
    });
    let mut bytes = Vec::new();
    loop {
        match rx.recv_timeout(idle) {
            Ok(Ok(data)) if data.is_empty() => return Ok(bytes),
            Ok(Ok(data)) => {
                bytes.extend_from_slice(&data);
                ensure!(bytes.len() <= STDIN_LIMIT, "ask stdin exceeds 512 KiB");
            }
            Ok(Err(error)) => return Err(error).context("ask could not read stdin"),
            Err(_) => bail!(
                "ask stdin stayed open without input for {}s; pass the message as arguments after --, or close stdin (for example with a heredoc)",
                idle.as_secs()
            ),
        }
    }
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
    // A message given as arguments is the whole message. Agent shells (Claude
    // Code's Bash tool, IDE terminals, piped scripts) hand children a stdin
    // that may never reach EOF or that carries unrelated text, so stdin is
    // read only when no message argument was given.
    let mut body = body.join(" ").trim().to_owned();
    if body.is_empty() && !std::io::stdin().is_terminal() {
        let bytes = read_message_stdin(std::io::stdin(), STDIN_IDLE_TIMEOUT)?;
        let bytes = bytes.strip_prefix(&[0xef, 0xbb, 0xbf]).unwrap_or(&bytes);
        body = std::str::from_utf8(bytes)
            .context("ask stdin must be UTF-8")?
            .trim()
            .to_owned();
    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    /// Yields its chunks with a delay, then blocks forever without EOF.
    struct Stalled(Vec<&'static [u8]>, Duration);
    impl Read for Stalled {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            if self.0.is_empty() {
                std::thread::sleep(Duration::from_secs(3600));
                return Ok(0);
            }
            std::thread::sleep(self.1);
            let chunk = self.0.remove(0);
            buf[..chunk.len()].copy_from_slice(chunk);
            Ok(chunk.len())
        }
    }
    /// Yields its chunks with a delay between them, then reports EOF.
    struct Slow(Vec<&'static [u8]>, Duration);
    impl Read for Slow {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            std::thread::sleep(self.1);
            if self.0.is_empty() {
                return Ok(0);
            }
            let chunk = self.0.remove(0);
            buf[..chunk.len()].copy_from_slice(chunk);
            Ok(chunk.len())
        }
    }

    #[test]
    fn closed_stdin_is_read_to_the_end() {
        let text = "第一行\nsecond line\n".repeat(2000);
        let read = read_message_stdin(Cursor::new(text.clone().into_bytes()), Duration::from_secs(5));
        assert_eq!(read.unwrap(), text.into_bytes());
    }
    #[test]
    fn stdin_that_never_closes_is_refused_after_the_idle_timeout() {
        let start = Instant::now();
        let error = read_message_stdin(Stalled(vec![b"partial"], Duration::ZERO), Duration::from_millis(300))
            .unwrap_err()
            .to_string();
        assert!(error.contains("stayed open without input"), "{error}");
        assert!(start.elapsed() < Duration::from_secs(5));
        assert!(read_message_stdin(Stalled(vec![], Duration::ZERO), Duration::from_millis(300)).is_err());
    }
    #[test]
    fn slow_producer_within_the_idle_timeout_still_completes() {
        let read = read_message_stdin(Slow(vec![b"a", b"b", b"c"], Duration::from_millis(150)), Duration::from_millis(600));
        assert_eq!(read.unwrap(), b"abc");
    }
    #[test]
    fn oversized_stdin_is_refused() {
        let read = read_message_stdin(Cursor::new(vec![b'x'; STDIN_LIMIT + 1]), Duration::from_secs(5));
        assert!(read.unwrap_err().to_string().contains("exceeds 512 KiB"));
    }
}
