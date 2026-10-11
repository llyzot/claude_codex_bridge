//! Append-only Codex rollout and native Pi extension observation.
//! The caller persists the baseline before granting permission to send input.
use anyhow::{ensure, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{
    fs::File,
    io::{BufRead, BufReader, Read, Seek, SeekFrom},
    path::{Path, PathBuf},
};

const MAX_LINE: usize = 4 * 1024 * 1024;
const BOUNDARY_BYTES: u64 = 64 * 1024;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Binding {
    pub root: PathBuf,
    pub path: PathBuf,
    pub session_id: String,
    pub cwd: PathBuf,
}
impl Binding {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.root.is_absolute() && self.path.is_absolute() && self.cwd.is_absolute(),
            "history paths must be absolute"
        );
        ensure!(
            !self.session_id.is_empty() && self.session_id.len() <= 128,
            "invalid history session id"
        );
        ensure!(
            self.path.starts_with(&self.root) && self.path != self.root,
            "history path is outside its declared root"
        );
        for path in self.path.ancestors() {
            ensure!(
                !path
                    .components()
                    .any(|p| matches!(p, std::path::Component::ParentDir)),
                "history path contains parent traversal"
            );
            crate::herdr_namespace::unlinked(path)?;
        }
        ensure!(
            self.root.is_dir() && self.cwd.is_dir(),
            "history root/workspace missing"
        );
        Ok(())
    }
    fn header(&self, value: &Value) -> Result<()> {
        let p = &value["payload"];
        ensure!(
            value["type"] == "session_meta" && p["id"] == self.session_id,
            "Codex history session differs from binding"
        );
        ensure!(
            p["source"]["subagent"].is_null(),
            "subagent history cannot complete a parent request"
        );
        let cwd = Path::new(p["cwd"].as_str().context("history workspace missing")?);
        ensure!(
            cwd.is_absolute() && cwd.canonicalize()? == self.cwd.canonicalize()?,
            "Codex history workspace differs from binding"
        );
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Baseline {
    pub identity: Value,
    pub offset: u64,
    pub digest: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub producer: Option<Value>,
}
#[cfg(windows)]
fn identity(file: &File) -> Result<Value> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Storage::FileSystem::{
        GetFileInformationByHandle, BY_HANDLE_FILE_INFORMATION,
    };
    let mut info: BY_HANDLE_FILE_INFORMATION = unsafe { std::mem::zeroed() };
    ensure!(
        unsafe { GetFileInformationByHandle(file.as_raw_handle(), &mut info) } != 0,
        "cannot identify history file"
    );
    ensure!(info.dwFileAttributes & 0x400 == 0, "linked history file");
    Ok(json!([
        info.dwVolumeSerialNumber,
        info.nFileIndexHigh,
        info.nFileIndexLow
    ]))
}
#[cfg(unix)]
fn identity(file: &File) -> Result<Value> {
    use std::os::unix::fs::MetadataExt;
    let m = file.metadata()?;
    Ok(json!([m.dev(), m.ino()]))
}
#[cfg(not(any(windows, unix)))]
fn identity(_: &File) -> Result<Value> {
    anyhow::bail!("unsupported history file identity")
}

fn line(reader: &mut impl BufRead) -> Result<Option<Vec<u8>>> {
    let mut bytes = Vec::new();
    reader
        .take(MAX_LINE as u64 + 1)
        .read_until(b'\n', &mut bytes)?;
    ensure!(
        bytes.len() <= MAX_LINE,
        "Codex history record exceeds limit"
    );
    Ok(if bytes.last() == Some(&b'\n') {
        Some(bytes)
    } else {
        None
    })
}
fn boundary_digest(file: &mut File, size: u64) -> Result<String> {
    ensure!(
        file.metadata()?.len() >= size,
        "history shrank below the observation baseline"
    );
    // Earlier turns are not read again. File identity, header binding and the
    // bytes immediately before the saved offset fence append-only resumption.
    let start = size.saturating_sub(BOUNDARY_BYTES);
    file.seek(SeekFrom::Start(start))?;
    let mut digest = Sha256::new();
    digest.update(size.to_le_bytes());
    let mut bounded = file.take(size - start);
    let mut buffer = [0; 65536];
    loop {
        let n = bounded.read(&mut buffer)?;
        if n == 0 {
            break;
        }
        digest.update(&buffer[..n]);
    }
    ensure!(bounded.limit() == 0, "history baseline became truncated");
    Ok(format!("{:x}", digest.finalize()))
}
pub(crate) fn baseline(binding: &Binding) -> Result<Baseline> {
    binding.validate()?;
    let mut file = match File::open(&binding.path) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Ok(Baseline {
                identity: Value::Null,
                offset: 0,
                digest: String::new(),
                producer: None,
            })
        }
        Err(e) => return Err(e.into()),
    };
    let size = file.metadata()?.len();
    let id = identity(&file)?;
    let first = line(&mut BufReader::new(&mut file))?.context("history header is incomplete")?;
    let header: Value = serde_json::from_slice(&first)?;
    binding.header(&header)?;
    let producer = if header["payload"]["source"] == "ccb_native_pi" {
        let producer = header["payload"]["ccb_bridge"].clone();
        crate::pi_terminal::validate_producer(&producer)?;
        Some(producer)
    } else {
        None
    };
    file.seek(SeekFrom::Start(
        size.checked_sub(1).context("empty history")?,
    ))?;
    let mut last = [0];
    file.read_exact(&mut last)?;
    ensure!(
        last[0] == b'\n',
        "wait for a complete history record before dispatch"
    );
    Ok(Baseline {
        identity: id,
        offset: size,
        digest: boundary_digest(&mut file, size)?,
        producer,
    })
}
pub(crate) fn from_header(binding: &Binding) -> Result<Baseline> {
    binding.validate()?;
    let mut file = File::open(&binding.path)?;
    let first = line(&mut BufReader::new(&mut file))?.context("history header is incomplete")?;
    let value: Value = serde_json::from_slice(&first)?;
    binding.header(&value)?;
    ensure!(
        value["payload"]["source"] != "ccb_native_pi",
        "discovery requires a Codex rollout"
    );
    Ok(Baseline {
        identity: identity(&file)?,
        offset: first.len() as u64,
        digest: boundary_digest(&mut file, first.len() as u64)?,
        producer: None,
    })
}

pub(crate) struct Reader {
    binding: Binding,
    baseline: Baseline,
    file: Option<BufReader<File>>,
    identity: Value,
    offset: u64,
    request: String,
    turn: Option<String>,
    anchored: bool,
    final_answer: String,
    terminal: Option<Value>,
    sequence: Option<u64>,
}
impl Reader {
    pub(crate) fn anchored(&self) -> bool {
        self.anchored
    }
    pub fn new(binding: Binding, baseline: Baseline, request: String) -> Result<Self> {
        binding.validate()?;
        ensure!(
            !request.is_empty() && request.len() <= 256 && !request.contains(['\r', '\n', '\0']),
            "invalid request anchor"
        );
        ensure!(
            !baseline.identity.is_null() || baseline.offset == 0 && baseline.digest.is_empty(),
            "invalid absent history baseline"
        );
        Ok(Self {
            binding,
            baseline,
            file: None,
            identity: Value::Null,
            offset: 0,
            request,
            turn: None,
            anchored: false,
            final_answer: String::new(),
            terminal: None,
            sequence: None,
        })
    }
    pub fn poll(&mut self) -> Result<Option<Value>> {
        if self.terminal.is_some() {
            return Ok(self.terminal.clone());
        }
        self.binding.validate()?;
        let mut current = match File::open(&self.binding.path) {
            Ok(f) => f,
            Err(e)
                if e.kind() == std::io::ErrorKind::NotFound
                    && self.identity.is_null()
                    && self.baseline.identity.is_null() =>
            {
                return Ok(None)
            }
            Err(e) => return Err(e.into()),
        };
        let id = identity(&current)?;
        if self.file.is_none() {
            ensure!(
                self.baseline.identity.is_null() || id == self.baseline.identity,
                "Codex history file was replaced"
            );
            if !self.baseline.identity.is_null() {
                ensure!(
                    boundary_digest(&mut current, self.baseline.offset)? == self.baseline.digest,
                    "Codex history baseline was rewritten"
                );
            }
            current.seek(SeekFrom::Start(0))?;
            let Some(first) = line(&mut BufReader::new(&mut current))? else {
                return Ok(None);
            };
            let header: Value = serde_json::from_slice(&first)?;
            self.binding.header(&header)?;
            ensure!(
                (header["payload"]["source"] == "ccb_native_pi")
                    == self.baseline.producer.is_some(),
                "history provider format changed"
            );
            if let Some(producer) = &self.baseline.producer {
                crate::pi_terminal::validate_producer(producer)?;
                ensure!(
                    header["payload"]["source"] == "ccb_native_pi"
                        && header["payload"]["ccb_bridge"] == *producer,
                    "Pi event producer changed"
                );
            }
            self.offset = self.baseline.offset.max(first.len() as u64);
            current.seek(SeekFrom::Start(self.offset))?;
            self.identity = id;
            self.file = Some(BufReader::new(current));
        } else {
            ensure!(
                id == self.identity && current.metadata()?.len() >= self.offset,
                "Codex history file rotated or shrank"
            );
        }
        // Bound each poll; retain complete-record offsets, never parse partial JSON.
        let start = self.offset;
        while self.offset - start < 1024 * 1024 {
            let file = self.file.as_mut().unwrap();
            let Some(bytes) = line(file)? else {
                file.seek(SeekFrom::Start(self.offset))?;
                break;
            };
            self.offset += bytes.len() as u64;
            let value: Value =
                serde_json::from_slice(&bytes).context("invalid Codex history record")?;
            if let Some(producer) = self.baseline.producer.clone() {
                let normalized =
                    crate::pi_terminal::normalize(&value, &producer, &mut self.sequence)?;
                if let Some(normalized) = normalized {
                    self.event(&normalized)?;
                }
                if let Some(decision) = &mut self.terminal {
                    decision["diagnostics"]["completion_authority"] =
                        json!("pi_extension_agent_settled");
                    decision["diagnostics"]["pi_bridge"] = producer.clone();
                    decision["reason"] = json!(match decision["status"].as_str().unwrap_or("") {
                        "completed" => "pi_agent_settled",
                        "cancelled" => "pi_agent_aborted",
                        _ => "pi_agent_error",
                    });
                }
            } else {
                self.event(&value)?;
            }
            if self.terminal.is_some() {
                break;
            }
        }
        Ok(self.terminal.clone())
    }
    fn event(&mut self, value: &Value) -> Result<()> {
        ensure!(
            value["type"] != "session_meta",
            "Codex session changed inside the observed history"
        );
        let p = &value["payload"];
        let event = value["type"] == "event_msg";
        if event && p["type"] == "task_started" {
            let turn = p["turn_id"]
                .as_str()
                .filter(|s| !s.is_empty())
                .context("task_started has no turn id")?;
            ensure!(
                !self.anchored || self.turn.as_deref() == Some(turn),
                "another Codex turn started before completion"
            );
            self.turn = Some(turn.into());
            return Ok(());
        }
        if self.turn.is_none()
            || p["turn_id"]
                .as_str()
                .is_some_and(|id| Some(id) != self.turn.as_deref())
        {
            return Ok(());
        }
        let user = if event && p["type"] == "user_message" {
            p["message"].as_str().unwrap_or("").to_owned()
        } else if value["type"] == "response_item" && p["type"] == "message" && p["role"] == "user"
        {
            content(p, "input_text")
        } else {
            String::new()
        };
        if user.lines().next() == Some(&format!("CCB_REQ_ID: {}", self.request)) {
            self.anchored = true;
        }
        if !self.anchored {
            return Ok(());
        }
        if p["phase"] == "final_answer" {
            if value["type"] == "response_item"
                && p["type"] == "message"
                && p["role"] == "assistant"
            {
                self.final_answer = content(p, "output_text");
            } else if event && p["type"] == "agent_message" {
                self.final_answer = p["message"].as_str().unwrap_or("").to_owned();
            }
        }
        if event && ["task_complete", "turn_aborted"].contains(&p["type"].as_str().unwrap_or("")) {
            // A terminal record without the exact turn id is not completion proof.
            if p["turn_id"].as_str() != self.turn.as_deref() {
                return Ok(());
            }
            let failed = !p["error"].is_null() && p["error"] != json!({});
            let aborted = p["type"] == "turn_aborted";
            let reply = p["last_agent_message"]
                .as_str()
                .filter(|s| !s.trim().is_empty())
                .unwrap_or(&self.final_answer);
            self.terminal = Some(
                json!({"schema_version":2,"record_type":"completion_decision","terminal":true,
                "confidence":"exact","anchor_seen":true,"reply_started":!reply.is_empty(),"reply_stable":true,
                "provider_turn_ref":self.turn,"source_cursor":{"schema_version":2,"record_type":"completion_cursor",
                    "source_kind":"session_event_log","session_path":self.binding.path,"offset":self.offset,
                    "line_no":null,"event_seq":null,"opaque_cursor":null,"updated_at":crate::daemon::now()},"finished_at":crate::daemon::now(),
                "status":if failed {"failed"} else if aborted {"cancelled"} else {"completed"},
                "reply":if failed || aborted {""} else {reply},
                "reason":if failed {"codex_task_error"} else if aborted {"codex_turn_aborted"} else {"codex_task_complete"},
                "diagnostics":{"completion_authority":"codex_history_task_complete","session_id":self.binding.session_id,"turn_id":self.turn,"request_anchor":self.request}}),
            );
        }
        Ok(())
    }
}
fn content(payload: &Value, kind: &str) -> String {
    payload["content"]
        .as_array()
        .map(|items| {
            items
                .iter()
                .filter(|i| i["type"] == kind)
                .filter_map(|i| i["text"].as_str())
                .collect::<Vec<_>>()
                .join("\n")
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    fn setup() -> (tempfile::TempDir, Binding) {
        let dir = tempfile::tempdir().unwrap();
        let binding = Binding {
            root: dir.path().into(),
            path: dir.path().join("rollout.jsonl"),
            cwd: dir.path().into(),
            session_id: "session-1".into(),
        };
        (dir, binding)
    }
    fn append(binding: &Binding, rows: &[Value]) {
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&binding.path)
            .unwrap();
        for row in rows {
            writeln!(file, "{row}").unwrap();
        }
    }
    fn header(binding: &Binding) {
        append(
            binding,
            &[
                json!({"type":"session_meta","payload":{"id":"session-1","cwd":binding.cwd,"source":"cli"}}),
            ],
        );
    }
    fn e(kind: &str, extra: Value) -> Value {
        let mut p = json!({"type":kind});
        p.as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        json!({"type":"event_msg","payload":p})
    }
    fn start(binding: &Binding) {
        append(
            binding,
            &[
                e("task_started", json!({"turn_id":"t1"})),
                e(
                    "user_message",
                    json!({"message":"CCB_REQ_ID: request-1\nTask"}),
                ),
            ],
        );
    }
    fn terminal(reply: &str) -> Value {
        e(
            "task_complete",
            json!({"turn_id":"t1","last_agent_message":reply}),
        )
    }
    fn reader(binding: &Binding) -> Reader {
        Reader::new(
            binding.clone(),
            baseline(binding).unwrap(),
            "request-1".into(),
        )
        .unwrap()
    }
    #[test]
    fn historical_terminal_and_commentary_never_complete_new_request() {
        let (_dir, b) = setup();
        header(&b);
        start(&b);
        append(&b, &[terminal("old")]);
        let mut r = reader(&b);
        assert!(r.poll().unwrap().is_none());
        start(&b);
        append(
            &b,
            &[e(
                "agent_message",
                json!({"phase":"commentary","message":"only commentary"}),
            )],
        );
        assert!(r.poll().unwrap().is_none());
        append(&b, &[terminal("")]);
        let d = r.poll().unwrap().unwrap();
        assert_eq!(d["reply"], "");
        assert_eq!(d["provider_turn_ref"], "t1");
    }
    #[test]
    fn ignores_foreign_and_subagent_final_then_uses_explicit_final_answer() {
        let (_dir, b) = setup();
        header(&b);
        let mut r = reader(&b);
        start(&b);
        append(
            &b,
            &[
                e(
                    "task_complete",
                    json!({"turn_id":"foreign","last_agent_message":"wrong"}),
                ),
                json!({"type":"response_item","payload":{"type":"agent_message","phase":"final_answer","content":[{"type":"output_text","text":"child"}]}}),
                e(
                    "sub_agent_activity",
                    json!({"phase":"final_answer","message":"child"}),
                ),
                e(
                    "agent_message",
                    json!({"phase":"final_answer","message":"correct"}),
                ),
                terminal(""),
            ],
        );
        assert_eq!(r.poll().unwrap().unwrap()["reply"], "correct");
    }
    #[test]
    fn future_history_and_partial_utf8_records_wait_for_newline() {
        let (_dir, b) = setup();
        let mut r = reader(&b);
        assert!(r.poll().unwrap().is_none());
        header(&b);
        start(&b);
        let bytes = format!("{}\n", terminal("完成"));
        let offset = bytes.find('成').unwrap() + 1;
        std::fs::OpenOptions::new()
            .append(true)
            .open(&b.path)
            .unwrap()
            .write_all(&bytes.as_bytes()[..offset])
            .unwrap();
        assert!(r.poll().unwrap().is_none());
        std::fs::OpenOptions::new()
            .append(true)
            .open(&b.path)
            .unwrap()
            .write_all(&bytes.as_bytes()[offset..])
            .unwrap();
        assert_eq!(r.poll().unwrap().unwrap()["reply"], "完成");
    }
    #[test]
    fn cold_resume_verifies_baseline_then_observes_new_terminal() {
        let (_dir, b) = setup();
        header(&b);
        let saved = baseline(&b).unwrap();
        start(&b);
        append(&b, &[terminal("restored")]);
        let copy: Baseline = serde_json::from_value(json!(saved)).unwrap();
        assert_eq!(
            Reader::new(b.clone(), copy, "request-1".into())
                .unwrap()
                .poll()
                .unwrap()
                .unwrap()["reply"],
            "restored"
        );
        let bytes = std::fs::read(&b.path).unwrap();
        let changed = String::from_utf8(bytes)
            .unwrap()
            .replace("source", "SOURCE");
        std::fs::write(&b.path, changed).unwrap();
        assert!(Reader::new(b, saved, "request-1".into())
            .unwrap()
            .poll()
            .is_err());
    }
    #[test]
    fn replacement_truncation_wrong_session_and_path_escape_fail() {
        let (_dir, b) = setup();
        header(&b);
        let mut r = reader(&b);
        assert!(r.poll().unwrap().is_none());
        std::fs::rename(&b.path, b.path.with_extension("old")).unwrap();
        header(&b);
        assert!(r.poll().is_err());
        let mut r = reader(&b);
        assert!(r.poll().unwrap().is_none());
        std::fs::write(&b.path, "").unwrap();
        assert!(r.poll().is_err());
        let mut wrong = b.clone();
        wrong.session_id = "foreign".into();
        header(&b);
        assert!(baseline(&wrong).is_err());
        wrong.path = b.root.join("../escape.jsonl");
        assert!(wrong.validate().is_err());
    }
    #[test]
    fn error_and_abort_do_not_deliver_apparent_success_text() {
        for kind in ["task_complete", "turn_aborted"] {
            let (_dir, b) = setup();
            header(&b);
            let mut r = reader(&b);
            start(&b);
            append(
                &b,
                &[e(
                    kind,
                    json!({"turn_id":"t1","last_agent_message":"should not succeed","error":{"message":"failed"}}),
                )],
            );
            let result = r.poll().unwrap().unwrap();
            assert_eq!(result["status"], "failed");
            assert_eq!(result["reply"], "");
        }
    }
    #[test]
    fn new_turn_before_terminal_is_ambiguous_and_missing_turn_cannot_finish() {
        let (_dir, b) = setup();
        header(&b);
        let mut r = reader(&b);
        start(&b);
        append(
            &b,
            &[e(
                "task_complete",
                json!({"last_agent_message":"no identity"}),
            )],
        );
        assert!(r.poll().unwrap().is_none());
        append(&b, &[e("task_started", json!({"turn_id":"second"}))]);
        assert!(r.poll().is_err());
    }
    #[test]
    fn partial_baseline_and_oversized_record_are_rejected() {
        let (_dir, b) = setup();
        header(&b);
        std::fs::OpenOptions::new()
            .append(true)
            .open(&b.path)
            .unwrap()
            .write_all(b"{")
            .unwrap();
        assert!(baseline(&b).is_err());
        std::fs::write(&b.path, "").unwrap();
        header(&b);
        let mut r = reader(&b);
        std::fs::OpenOptions::new()
            .append(true)
            .open(&b.path)
            .unwrap()
            .write_all(&vec![b' '; MAX_LINE + 1])
            .unwrap();
        assert!(r.poll().is_err());
    }

    fn pi_header(b: &Binding) -> Value {
        let producer = json!({"schema_version":1,"owner":uuid::Uuid::new_v4().to_string(),"instance":uuid::Uuid::new_v4().to_string(),"process":{"pid":123,"created":"1","windows_session":1},"session_path":b.root.join("native-pi.jsonl")});
        append(
            b,
            &[
                json!({"type":"session_meta","payload":{"id":b.session_id,"cwd":b.cwd,"source":"ccb_native_pi","ccb_bridge":producer}}),
            ],
        );
        producer
    }
    fn pi_event(producer: &Value, seq: u64, kind: &str, extra: Value) -> Value {
        let mut payload = json!({"type":kind,"turn_id":"pi-turn"});
        payload
            .as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        json!({"type":"pi_event","bridge":producer,"seq":seq,"payload":payload})
    }
    #[test]
    fn pi_requires_settlement_and_classifies_final_stop_abort_and_error() {
        for (stop, aborted, error, status) in [
            ("stop", false, false, "completed"),
            ("toolUse", false, false, "failed"),
            ("length", false, false, "failed"),
            ("stop", false, true, "failed"),
            ("stop", true, false, "cancelled"),
        ] {
            let (_dir, b) = setup();
            let producer = pi_header(&b);
            let mut r = reader(&b);
            append(
                &b,
                &[
                    pi_event(&producer, 1, "agent_start", json!({})),
                    pi_event(
                        &producer,
                        2,
                        "user_message",
                        json!({"request_anchor":"request-1"}),
                    ),
                    pi_event(&producer, 3, "agent_end", json!({})),
                ],
            );
            assert!(r.poll().unwrap().is_none());
            append(
                &b,
                &[pi_event(
                    &producer,
                    4,
                    "agent_settled",
                    json!({"idle":true,"aborted":aborted,"assistant":{"stop_reason":stop,"text":"final","error":error}}),
                )],
            );
            let result = r.poll().unwrap().unwrap();
            assert_eq!(result["status"], status);
            assert_eq!(
                result["diagnostics"]["completion_authority"],
                "pi_extension_agent_settled"
            );
            assert_eq!(
                result["reply"],
                if status == "completed" { "final" } else { "" }
            );
        }
    }
    #[test]
    fn pi_rejects_generation_changes_gaps_nonidle_and_superseding_inputs() {
        for mode in ["generation", "gap", "idle", "superseded"] {
            let (_dir, b) = setup();
            let producer = pi_header(&b);
            let mut r = reader(&b);
            append(
                &b,
                &[
                    pi_event(&producer, 1, "agent_start", json!({})),
                    pi_event(
                        &producer,
                        2,
                        "user_message",
                        json!({"request_anchor":"request-1"}),
                    ),
                ],
            );
            assert!(r.poll().unwrap().is_none());
            let mut event = pi_event(
                &producer,
                3,
                "agent_settled",
                json!({"idle":true,"aborted":false,"assistant":{"stop_reason":"stop","error":false,"text":"final"}}),
            );
            match mode {
                "generation" => {
                    event["bridge"]["instance"] = json!(uuid::Uuid::new_v4().to_string())
                }
                "gap" => event["seq"] = json!(4),
                "idle" => event["payload"]["idle"] = json!(false),
                _ => event["payload"]["type"] = json!("request_superseded"),
            }
            append(&b, &[event]);
            assert!(r.poll().is_err());
        }
    }
    #[test]
    fn pi_foreign_request_marker_never_completes_the_observed_job() {
        let (_dir, b) = setup();
        let producer = pi_header(&b);
        let mut r = reader(&b);
        append(
            &b,
            &[
                pi_event(&producer, 1, "agent_start", json!({})),
                pi_event(
                    &producer,
                    2,
                    "user_message",
                    json!({"request_anchor":"foreign"}),
                ),
                pi_event(
                    &producer,
                    3,
                    "agent_settled",
                    json!({"idle":true,"aborted":false,"assistant":{"stop_reason":"stop","error":false,"text":"foreign result"}}),
                ),
            ],
        );
        assert!(r.poll().unwrap().is_none());
    }
}
