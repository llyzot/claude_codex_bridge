//! Original Claude terminal transcripts, fenced by session, file and parent chain.
//! A final assistant record is only a candidate: Herdr must also observe idle.
use crate::terminal_history::{boundary_digest, identity, line, Baseline, Binding};
use anyhow::{ensure, Result};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{
    collections::HashMap,
    fs::File,
    io::{BufReader, Read, Seek, SeekFrom},
};

fn fingerprint(row: &Value) -> String {
    format!("{:x}", Sha256::digest(row.to_string().as_bytes()))
}

fn scope(binding: &Binding, row: &Value) -> Result<()> {
    if !row["sessionId"].is_null() {
        ensure!(
            row["sessionId"] == binding.session_id,
            "Claude transcript session changed"
        );
    }
    if !row["cwd"].is_null() {
        ensure!(
            crate::pi_terminal::same_session_path(&row["cwd"], &json!(binding.cwd)),
            "Claude transcript workspace changed"
        );
    }
    Ok(())
}

fn validate_file(binding: &Binding, file: &mut File) -> Result<()> {
    binding.validate()?;
    uuid::Uuid::parse_str(&binding.session_id)?;
    ensure!(
        binding.path.file_stem().and_then(|s| s.to_str()) == Some(&binding.session_id),
        "Claude transcript filename/session mismatch"
    );
    file.seek(SeekFrom::Start(0))?;
    let mut reader = BufReader::new(file);
    let mut bytes = 0;
    for _ in 0..256 {
        let Some(record) = line(&mut reader)? else {
            break;
        };
        bytes += record.len();
        ensure!(
            bytes <= 1024 * 1024,
            "Claude transcript scope header exceeds limit"
        );
        let row: Value = serde_json::from_slice(&record)?;
        scope(binding, &row)?;
        if row["sessionId"] == binding.session_id
            && row["cwd"].is_string()
            && row["isSidechain"] != true
        {
            return Ok(());
        }
    }
    anyhow::bail!("Claude transcript has no complete session/workspace evidence")
}
pub(crate) fn baseline(binding: &Binding, from_start: bool) -> Result<Baseline> {
    binding.validate()?;
    let mut file = File::open(&binding.path)?;
    validate_file(binding, &mut file)?;
    let offset = if from_start {
        0
    } else {
        file.metadata()?.len()
    };
    if offset > 0 {
        file.seek(SeekFrom::Start(offset - 1))?;
        let mut tail = [0];
        file.read_exact(&mut tail)?;
        ensure!(
            tail[0] == b'\n',
            "Claude transcript has a partial baseline record"
        );
    }
    Ok(Baseline {
        identity: identity(&file)?,
        offset,
        digest: boundary_digest(&mut file, offset)?,
        producer: None,
    })
}
fn text(content: &Value) -> String {
    content.as_str().map(str::to_owned).unwrap_or_else(|| {
        content
            .as_array()
            .map(|parts| {
                parts
                    .iter()
                    .filter(|p| p["type"] == "text")
                    .filter_map(|p| p["text"].as_str())
                    .collect::<Vec<_>>()
                    .join("\n")
            })
            .unwrap_or_default()
    })
}

// Match the native single-envelope transport normalization in
// lib/provider_core/claude_prompt.py. Never recursively activate quoted input.
fn prompt_body(text: &str) -> &str {
    let stripped = text.trim();
    let Some(rest) = stripped.strip_prefix("<pasted_content id=\"") else {
        return text;
    };
    let Some((id, rest)) = rest.split_once("\">") else {
        return text;
    };
    if id.is_empty()
        || id
            .chars()
            .any(|c| c.is_whitespace() || ['"', '<', '>'].contains(&c))
    {
        return text;
    }
    let closing = format!("</pasted_content id=\"{id}\">");
    let Some(body) = rest.strip_suffix(&closing) else {
        return text;
    };
    let lower = body.to_lowercase();
    for tag in ["<pasted_content", "</pasted_content"] {
        if lower.match_indices(tag).any(|(index, _)| {
            lower[index + tag.len()..]
                .chars()
                .next()
                .is_none_or(|c| !c.is_alphanumeric() && c != '_')
        }) {
            return text;
        }
    }
    body
}
pub(crate) struct Reader {
    binding: Binding,
    baseline: Baseline,
    offset: u64,
    digest: String,
    opened: bool,
    request: String,
    anchor: Option<String>,
    seen: HashMap<String, String>,
    message: String,
    reply: String,
    last_assistant: String,
    stop: String,
    tool: bool,
    terminal: Option<Value>,
}
impl Reader {
    pub(crate) fn new(binding: Binding, baseline: Baseline, request: String) -> Result<Self> {
        binding.validate()?;
        ensure!(
            baseline.producer.is_none() && !baseline.identity.is_null(),
            "Claude requires an identified transcript baseline"
        );
        ensure!(
            !request.is_empty() && request.len() <= 256 && !request.contains(['\n', '\r', '\0']),
            "invalid Claude request anchor"
        );
        Ok(Self {
            binding,
            offset: baseline.offset,
            digest: baseline.digest.clone(),
            baseline,
            opened: false,
            request,
            anchor: None,
            seen: HashMap::new(),
            message: String::new(),
            reply: String::new(),
            last_assistant: String::new(),
            stop: String::new(),
            tool: false,
            terminal: None,
        })
    }
    pub(crate) fn anchored(&self) -> bool {
        self.anchor.is_some()
    }
    pub(crate) fn position(&self) -> u64 {
        self.offset
    }
    pub(crate) fn poll(&mut self) -> Result<Option<Value>> {
        self.binding.validate()?;
        let mut file = File::open(&self.binding.path)?;
        ensure!(
            identity(&file)? == self.baseline.identity,
            "Claude transcript file replaced"
        );
        ensure!(
            boundary_digest(&mut file, self.offset)? == self.digest,
            "Claude transcript boundary rewritten"
        );
        if !self.opened {
            validate_file(&self.binding, &mut file)?;
            self.opened = true;
        }
        file.seek(SeekFrom::Start(self.offset))?;
        let start = self.offset;
        let mut reader = BufReader::new(file);
        while self.offset - start < 1024 * 1024 {
            let Some(bytes) = line(&mut reader)? else {
                break;
            };
            self.offset += bytes.len() as u64;
            self.event(&serde_json::from_slice::<Value>(&bytes)?)?;
        }
        self.digest = boundary_digest(reader.get_mut(), self.offset)?;
        // Do not release a candidate while more complete events may supersede it.
        if reader.get_ref().metadata()?.len() > self.offset {
            return Ok(None);
        }
        Ok(self.terminal.clone().map(|mut decision| {
            decision["source_cursor"] = json!({"schema_version":2,"record_type":"completion_cursor",
                "source_kind":"session_event_log","session_path":self.binding.path,"identity":self.baseline.identity,
                "offset":self.offset,"digest":self.digest,"line_no":null,"event_seq":null,"opaque_cursor":null,
                "updated_at":crate::daemon::now()});
            decision
        }))
    }
    fn event(&mut self, row: &Value) -> Result<()> {
        scope(&self.binding, row)?;
        if row["isSidechain"] == true || !row["agentId"].is_null() {
            return Ok(());
        }
        let kind = row["type"].as_str().unwrap_or("");
        if kind == "queue-operation" {
            return Ok(());
        } // Enqueued is not activated.
        let user = kind == "user"
            && row["isMeta"] != true
            && row["toolUseResult"].is_null()
            && !row["message"]["content"]
                .as_array()
                .is_some_and(|v| v.iter().any(|b| b["type"] == "tool_result"));
        if user {
            let body = text(&row["message"]["content"]);
            let exact = prompt_body(&body).trim_start().lines().next()
                == Some(&format!("CCB_REQ_ID: {}", self.request));
            if let Some(anchor) = &self.anchor {
                ensure!(
                    row["uuid"].as_str() == self.anchor.as_deref()
                        && self.seen.get(anchor) == Some(&fingerprint(row)),
                    "Claude request superseded before idle completion"
                );
            } else if exact {
                ensure!(
                    row["sessionId"] == self.binding.session_id && row["cwd"].is_string(),
                    "Claude request has no session/workspace identity"
                );
                let id = crate::records::string(row, "uuid")?;
                ensure!(
                    !id.is_empty() && id.len() <= 256,
                    "invalid Claude record UUID"
                );
                self.anchor = Some(id.into());
                self.seen
                    .insert(self.anchor.clone().unwrap(), fingerprint(row));
            }
            return Ok(());
        }
        if self.anchor.is_none() {
            return Ok(());
        }
        let Some(id) = row["uuid"].as_str() else {
            return Ok(());
        };
        ensure!(
            !id.is_empty() && id.len() <= 256,
            "invalid Claude record UUID"
        );
        if let Some(prior) = self.seen.get(id) {
            ensure!(*prior == fingerprint(row), "Claude record UUID reused");
            return Ok(());
        }
        let Some(parent) = row["parentUuid"].as_str() else {
            return Ok(());
        };
        if !self.seen.contains_key(parent) {
            return Ok(());
        }
        ensure!(self.seen.len() < 4096, "Claude turn record limit exceeded");
        self.seen.insert(id.into(), fingerprint(row));
        if kind == "assistant" {
            self.terminal = None;
            let message = crate::records::string(&row["message"], "id")?;
            if self.message != message {
                self.message = message.into();
                self.reply.clear();
                self.tool = false;
                self.stop.clear();
            }
            self.reply.push_str(&text(&row["message"]["content"]));
            ensure!(
                self.reply.len() <= 4 * 1024 * 1024,
                "Claude reply exceeds limit"
            );
            self.tool |= row["message"]["content"]
                .as_array()
                .is_some_and(|v| v.iter().any(|b| b["type"] == "tool_use"));
            self.stop = row["message"]["stop_reason"].as_str().unwrap_or("").into();
            self.last_assistant = id.into();
            if row["isApiErrorMessage"] == true {
                self.finish("failed", "claude_api_error");
            } else if self.stop == "end_turn" && !self.tool {
                self.finish("completed", "claude_assistant_end_turn");
            }
        } else if kind == "system"
            && row["subtype"] == "turn_duration"
            && parent == self.last_assistant
            && !self.tool
        {
            if self.stop.is_empty() || self.stop == "end_turn" {
                self.finish("completed", "claude_turn_duration");
            }
        } else if kind == "user" {
            self.terminal = None;
            // Hook feedback/tool results reopen work; an older duration record
            // cannot revalidate the assistant that preceded this input.
            self.last_assistant.clear();
        }
        Ok(())
    }
    fn finish(&mut self, status: &str, reason: &str) {
        self.terminal = Some(
            json!({"schema_version":2,"record_type":"completion_decision","terminal":true,"status":status,"reason":reason,
            "confidence":"exact","anchor_seen":true,"reply_started":!self.reply.is_empty(),"reply_stable":true,
            "reply":if status=="completed"{&self.reply}else{""},"provider_turn_ref":self.anchor,"source_cursor":null,"finished_at":crate::daemon::now(),
            "diagnostics":{"completion_authority":"claude_transcript_candidate","session_id":self.binding.session_id,"request_anchor":self.request,"assistant_uuid":self.last_assistant}}),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    fn setup() -> (tempfile::TempDir, Binding) {
        let dir = tempfile::tempdir().unwrap();
        let id = uuid::Uuid::new_v4().to_string();
        let binding = Binding {
            root: dir.path().into(),
            path: dir.path().join(format!("{id}.jsonl")),
            cwd: dir.path().into(),
            session_id: id,
        };
        append(
            &binding,
            &[row(
                &binding,
                "user",
                "old",
                None,
                json!({"role":"user","content":"historical"}),
            )],
        );
        (dir, binding)
    }
    fn row(b: &Binding, kind: &str, id: &str, parent: Option<&str>, message: Value) -> Value {
        json!({"type":kind,"uuid":id,"parentUuid":parent,"sessionId":b.session_id,"cwd":b.cwd,"isSidechain":false,"message":message})
    }
    fn user(b: &Binding) -> Value {
        row(
            b,
            "user",
            "request",
            Some("old"),
            json!({"role":"user","content":"CCB_REQ_ID: job:execution\nTask"}),
        )
    }
    fn assistant(b: &Binding, id: &str, parent: &str, stop: Value, reply: &str) -> Value {
        row(
            b,
            "assistant",
            id,
            Some(parent),
            json!({"id":format!("msg-{id}"),"role":"assistant","content":[{"type":"text","text":reply}],"stop_reason":stop}),
        )
    }
    fn append(b: &Binding, rows: &[Value]) {
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&b.path)
            .unwrap();
        for row in rows {
            writeln!(file, "{row}").unwrap();
        }
    }
    fn reader(b: &Binding) -> Reader {
        Reader::new(
            b.clone(),
            baseline(b, false).unwrap(),
            "job:execution".into(),
        )
        .unwrap()
    }
    #[test]
    fn exact_activation_and_parent_chain_survive_cold_resume() {
        let (_d, b) = setup();
        let checkpoint = baseline(&b, false).unwrap();
        let mut r = reader(&b);
        append(
            &b,
            &[
                json!({"type":"queue-operation","sessionId":b.session_id,"operation":"enqueue","content":"CCB_REQ_ID: job:execution"}),
                assistant(&b, "foreign", "old", json!("end_turn"), "wrong"),
            ],
        );
        assert!(r.poll().unwrap().is_none());
        assert!(!r.anchored());
        let attachment = row(&b, "attachment", "attach", Some("request"), Value::Null);
        append(
            &b,
            &[
                user(&b),
                attachment,
                assistant(&b, "result", "attach", json!("end_turn"), "正确 ✓"),
            ],
        );
        assert_eq!(r.poll().unwrap().unwrap()["reply"], "正确 ✓");
        let mut cold = Reader::new(b.clone(), checkpoint, "job:execution".into()).unwrap();
        assert_eq!(cold.poll().unwrap().unwrap()["reply"], "正确 ✓");
    }
    #[test]
    fn tools_foreign_parents_and_sidechains_cannot_complete() {
        let (_d, b) = setup();
        let mut r = reader(&b);
        let mut tool = assistant(&b, "tool", "request", json!("tool_use"), "working");
        tool["message"]["content"]
            .as_array_mut()
            .unwrap()
            .push(json!({"type":"tool_use","id":"call"}));
        let mut side = assistant(&b, "side", "request", json!("end_turn"), "wrong");
        side["isSidechain"] = json!(true);
        append(
            &b,
            &[
                user(&b),
                tool,
                side,
                assistant(&b, "foreign", "unrelated", json!("end_turn"), "wrong"),
            ],
        );
        assert!(r.poll().unwrap().is_none());
        let result = row(
            &b,
            "user",
            "tool-result",
            Some("tool"),
            json!({"role":"user","content":[{"type":"tool_result","tool_use_id":"call","content":"done"}]}),
        );
        append(
            &b,
            &[
                result,
                assistant(&b, "final", "tool-result", json!("end_turn"), "done"),
            ],
        );
        assert_eq!(r.poll().unwrap().unwrap()["reply"], "done");
    }
    #[test]
    fn hook_continuation_replaces_candidate_and_new_user_supersedes() {
        let (_d, b) = setup();
        let mut r = reader(&b);
        append(
            &b,
            &[
                user(&b),
                assistant(&b, "first", "request", json!("end_turn"), "provisional"),
            ],
        );
        assert!(r.poll().unwrap().is_some());
        let mut hook = row(
            &b,
            "user",
            "hook",
            Some("first"),
            json!({"role":"user","content":"continue"}),
        );
        hook["isMeta"] = json!(true);
        append(&b, &[hook]);
        assert!(r.poll().unwrap().is_none());
        append(
            &b,
            &[assistant(&b, "second", "hook", json!("end_turn"), "final")],
        );
        assert_eq!(r.poll().unwrap().unwrap()["reply"], "final");
        append(
            &b,
            &[row(
                &b,
                "user",
                "new",
                Some("second"),
                json!({"role":"user","content":"unrelated input"}),
            )],
        );
        assert!(r.poll().is_err());
    }
    #[test]
    fn partial_utf8_and_rewritten_history_do_not_complete() {
        let (_d, b) = setup();
        let mut r = reader(&b);
        append(&b, &[user(&b)]);
        let bytes = serde_json::to_vec(&assistant(
            &b,
            "final",
            "request",
            json!("end_turn"),
            "结果",
        ))
        .unwrap();
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&b.path)
            .unwrap();
        file.write_all(&bytes[..bytes.len() - 2]).unwrap();
        file.flush().unwrap();
        assert!(r.poll().unwrap().is_none());
        file.write_all(&bytes[bytes.len() - 2..]).unwrap();
        file.write_all(b"\n").unwrap();
        file.flush().unwrap();
        assert_eq!(r.poll().unwrap().unwrap()["reply"], "结果");
        drop(file);
        std::fs::write(&b.path, b"{}\n").unwrap();
        assert!(r.poll().is_err());
    }
    #[test]
    fn api_errors_and_identity_changes_never_return_success() {
        let (_d, b) = setup();
        let mut r = reader(&b);
        let mut error = assistant(&b, "error", "request", json!("end_turn"), "API error");
        error["isApiErrorMessage"] = json!(true);
        append(&b, &[user(&b), error]);
        assert_eq!(r.poll().unwrap().unwrap()["status"], "failed");
        let mut foreign = assistant(&b, "foreign", "error", json!("end_turn"), "wrong");
        foreign["sessionId"] = json!(uuid::Uuid::new_v4().to_string());
        append(&b, &[foreign]);
        assert!(r.poll().is_err());
    }

    #[test]
    fn turn_duration_must_not_revive_answer_invalidated_by_hook_feedback() {
        let (_d, b) = setup();
        let mut r = reader(&b);
        append(
            &b,
            &[
                user(&b),
                assistant(&b, "candidate", "request", Value::Null, "candidate answer"),
            ],
        );
        assert!(r.poll().unwrap().is_none());
        let mut hook = row(
            &b,
            "user",
            "hook",
            Some("candidate"),
            json!({"role":"user","content":"continue"}),
        );
        hook["isMeta"] = json!(true);
        let mut stale = row(&b, "system", "old-duration", Some("candidate"), Value::Null);
        stale["subtype"] = json!("turn_duration");
        append(&b, &[hook, stale]);
        assert!(r.poll().unwrap().is_none());
        let mut finished = row(&b, "system", "duration", Some("final"), Value::Null);
        finished["subtype"] = json!("turn_duration");
        append(
            &b,
            &[
                assistant(&b, "final", "hook", Value::Null, "final answer"),
                finished,
            ],
        );
        assert_eq!(r.poll().unwrap().unwrap()["reply"], "final answer");
    }

    #[test]
    fn native_paste_envelope_is_unwrapped_once_without_activating_quotes() {
        for (body,accepted) in [
            ("<pasted_content id=\"p1\">\nCCB_REQ_ID: job:execution\nTask</pasted_content id=\"p1\">",true),
            ("  CCB_REQ_ID: job:execution\nTask",true),
            ("<pasted_content id=\"p1\">CCB_REQ_ID: job:execution</pasted_content id=\"p2\">",false),
            ("<pasted_content id=\"p1\"><pasted_content id=\"p2\">CCB_REQ_ID: job:execution</pasted_content id=\"p2\"></pasted_content id=\"p1\">",false),
            ("quoted <pasted_content id=\"p1\">CCB_REQ_ID: job:execution</pasted_content id=\"p1\">",false),
            ("<pasted_content id=\"p1\">CCB_REQ_ID: job:execution</pasted_content id=\"p1\"><pasted_content id=\"p1\">other</pasted_content id=\"p1\">",false),
        ] {
            let (_d,b)=setup();let mut r=reader(&b);let mut prompt=user(&b);prompt["message"]["content"]=json!(body);
            append(&b,&[prompt,assistant(&b,"answer","request",json!("end_turn"),"answer")]);
            assert_eq!(r.poll().unwrap().is_some(),accepted,"{body}");
        }
    }
    #[test]
    fn stopped_reader_drains_chunks_but_never_accepts_a_partial_tail() {
        let (_dir, binding) = setup();
        let checkpoint = baseline(&binding, false).unwrap();
        append(
            &binding,
            &[
                user(&binding),
                assistant(
                    &binding,
                    "large",
                    "request",
                    json!("end_turn"),
                    &"x".repeat(1100000),
                ),
                row(&binding, "attachment", "later", Some("large"), Value::Null),
            ],
        );
        let mut reader = crate::terminal_history::ProviderReader::new(
            "claude",
            binding.clone(),
            checkpoint.clone(),
            "job:execution".into(),
        )
        .unwrap();
        assert!(reader.poll().unwrap().is_none());
        assert_eq!(
            reader.poll_stopped().unwrap().unwrap()["reply"]
                .as_str()
                .unwrap()
                .len(),
            1100000
        );
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&binding.path)
            .unwrap();
        write!(file, "{{\"type\":\"user\"").unwrap();
        drop(file);
        let mut reader = crate::terminal_history::ProviderReader::new(
            "claude",
            binding,
            checkpoint,
            "job:execution".into(),
        )
        .unwrap();
        assert!(reader.poll_stopped().unwrap().is_none());
    }
}
