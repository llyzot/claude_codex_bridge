//! One writer, redo journal, and idempotent append recovery.
use anyhow::{bail, ensure, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Component, Path, PathBuf};
use std::time::SystemTime;

type Signature = Option<(u64, SystemTime)>;
#[derive(Clone)]
struct LatestIndex {
    signature: Signature,
    rows: Vec<Value>,
    indices: HashMap<String, usize>,
}
impl LatestIndex {
    fn insert(&mut self, identity: &str, row: Value) -> Result<()> {
        let id = row[identity]
            .as_str()
            .context("missing record identity")?
            .to_owned();
        if let Some(index) = self.indices.get(&id) {
            self.rows[*index] = row;
        } else {
            self.indices.insert(id, self.rows.len());
            self.rows.push(row);
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum WriteOp {
    Save {
        path: String,
        bytes: Vec<u8>,
    },
    Append {
        path: String,
        offset: u64,
        bytes: Vec<u8>,
    },
    Remove {
        path: String,
    },
}
pub struct Store {
    root: PathBuf,
    agent_journal_lock: RefCell<Option<File>>,
    known_controller: bool,
    _lock: File,
    staged: HashMap<String, Option<Vec<u8>>>,
    writes: Vec<WriteOp>,
    latest_indices: RefCell<HashMap<(String, String), LatestIndex>>,
    index_reads: Cell<u64>,
    index_rows: Cell<u64>,
}
impl Store {
    pub fn open(root: &Path) -> Result<Self> {
        fs::create_dir_all(root)?;
        let root = root.canonicalize()?;
        fs::create_dir_all(root.join("kernel"))?;
        reject_link(&root.join("kernel"))?;
        reject_link(&root.join("kernel/writer.lock"))?;
        reject_link(&root.join("kernel/pending.json"))?;
        let known_controller = [
            "kernel/pending.json",
            "lease.json",
            "configuration.json",
            "messages/messages.jsonl",
        ]
        .iter()
        .any(|path| root.join(path).exists());
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(root.join("kernel/writer.lock"))?;
        lock.try_lock()
            .context("kernel state already has a writer")?;
        let mut store = Self {
            root,
            _lock: lock,
            agent_journal_lock: RefCell::new(None),
            known_controller,
            staged: HashMap::new(),
            writes: Vec::new(),
            latest_indices: RefCell::new(HashMap::new()),
            index_reads: Cell::new(0),
            index_rows: Cell::new(0),
        };
        store.recover()?;
        Ok(store)
    }
    fn path(&self, relative: &str) -> Result<PathBuf> {
        let path = Path::new(relative);
        ensure!(
            path.components().all(|c| matches!(c, Component::Normal(_))),
            "invalid state path"
        );
        let parts: Vec<_> = path.iter().filter_map(|p| p.to_str()).collect();
        let agent_log = parts.len() == 3
            && parts[0] == "agents"
            && ["jobs.jsonl", "events.jsonl"].contains(&parts[2]);
        if agent_log {
            crate::models::validate_agent(parts[1])?;
        }
        let stream = [
            "messages/messages.jsonl",
            "attempts/attempts.jsonl",
            "replies/replies.jsonl",
            "callbacks/edges.jsonl",
            "submissions.jsonl",
        ]
        .contains(&relative);
        let artifact = parts.len() == 4
            && parts[0] == "artifacts"
            && parts[1] == "text"
            && parts[3].ends_with(".txt");
        let provider_state = parts.len() == 4
            && parts[0] == "providers"
            && ["pi", "codex", "claude"].contains(&parts[1])
            && (["session.json", "fault.json"].contains(&parts[3])
                || parts[1] == "pi"
                    && [
                        "role-plan.json",
                        "native-role-bridge.mjs",
                        "terminal-bridge.json",
                        "terminal-bridge.mjs",
                    ]
                    .contains(&parts[3])
                || parts[1] == "claude" && ["role-view.json", "role-view"].contains(&parts[3]));
        if provider_state {
            crate::models::validate_agent(parts[2])?;
        }
        ensure!(
            relative.starts_with("mailboxes/")
                || relative.starts_with("leases/")
                || parts.len() == 2 && parts[0] == "snapshots" && parts[1].ends_with(".json")
                || agent_log
                || stream
                || artifact
                || provider_state
                || [
                    "lease.json",
                    "lifecycle.json",
                    "configuration.json",
                    "reload.json",
                    "namespace.json",
                    "herdr-agents.json",
                    "project-view.json",
                    "runtime-controls.jsonl",
                    "context-operations.jsonl"
                ]
                .contains(&relative),
            "unowned state path"
        );
        let base = if agent_log {
            self.root.parent().context("missing runtime state parent")?
        } else {
            &self.root
        };
        let full = base.join(path);
        let mut prefix = base.to_path_buf();
        for part in path.components() {
            prefix.push(part);
            if let Ok(meta) = fs::symlink_metadata(&prefix) {
                ensure!(!meta.file_type().is_symlink(), "linked kernel state path");
                #[cfg(windows)]
                {
                    use std::os::windows::fs::MetadataExt;
                    ensure!(
                        meta.file_attributes() & 0x400 == 0,
                        "reparse kernel state path"
                    );
                }
            }
        }
        if agent_log && self.agent_journal_lock.borrow().is_none() {
            let held = crate::agent_journal::claim(&self.root, self.known_controller)?;
            *self.agent_journal_lock.borrow_mut() = Some(held);
        }
        Ok(full)
    }
    pub fn bytes(&self, path: &str) -> Result<Option<Vec<u8>>> {
        if let Some(value) = self.staged.get(path) {
            return Ok(value.clone());
        }
        match fs::read(self.path(path)?) {
            Ok(bytes) => Ok(Some(bytes)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e.into()),
        }
    }
    pub fn json(&self, path: &str) -> Result<Option<Value>> {
        self.bytes(path)?
            .map(|b| serde_json::from_slice(&b).map_err(Into::into))
            .transpose()
    }
    pub fn rows(&self, path: &str) -> Result<Vec<Value>> {
        let bytes = self.bytes(path)?.unwrap_or_default();
        std::str::from_utf8(&bytes)?
            .lines()
            .filter(|s| !s.trim().is_empty())
            .map(|s| {
                let v: Value = serde_json::from_str(s)?;
                ensure!(v.is_object(), "expected JSON object row");
                Ok(v)
            })
            .collect()
    }
    fn signature(&self, path: &str) -> Result<Signature> {
        match fs::metadata(self.path(path)?) {
            Ok(metadata) => Ok(Some((metadata.len(), metadata.modified()?))),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error.into()),
        }
    }
    pub fn latest_rows(&self, path: &str, identity: &str) -> Result<Vec<Value>> {
        let key = (path.to_owned(), identity.to_owned());
        let signature = self.signature(path)?;
        let cached = self
            .latest_indices
            .borrow()
            .get(&key)
            .filter(|index| index.signature == signature)
            .cloned();
        if let Some(mut index) = cached {
            let mut append_only = true;
            for op in &self.writes {
                match op {
                    WriteOp::Append {
                        path: target,
                        bytes,
                        ..
                    } if target == path => {
                        index.insert(identity, serde_json::from_slice(bytes)?)?
                    }
                    WriteOp::Save { path: target, .. } | WriteOp::Remove { path: target }
                        if target == path =>
                    {
                        append_only = false;
                        break;
                    }
                    _ => {}
                }
            }
            if append_only {
                return Ok(index.rows);
            }
        }
        let rows = self.rows(path)?;
        self.index_reads.set(self.index_reads.get() + 1);
        self.index_rows
            .set(self.index_rows.get() + rows.len() as u64);
        let mut index = LatestIndex {
            signature,
            rows: Vec::new(),
            indices: HashMap::new(),
        };
        for row in rows {
            index.insert(identity, row)?;
        }
        let result = index.rows.clone();
        if !self.staged.contains_key(path) {
            self.latest_indices.borrow_mut().insert(key, index);
        }
        Ok(result)
    }
    pub fn index_metrics(&self) -> Value {
        serde_json::json!({"history_reads":self.index_reads.get(),"history_rows":self.index_rows.get()})
    }
    fn projection_index(&self, path: &str, identity: &str) -> Result<(String, String)> {
        ensure!(
            !self.staged.contains_key(path),
            "projection requires committed records"
        );
        let key = (path.to_owned(), identity.to_owned());
        let signature = self.signature(path)?;
        if !self
            .latest_indices
            .borrow()
            .get(&key)
            .is_some_and(|index| index.signature == signature)
        {
            self.latest_rows(path, identity)?;
        }
        Ok(key)
    }
    pub(crate) fn project_latest<T>(
        &self,
        path: &str,
        identity: &str,
        project: impl FnOnce(&[Value]) -> T,
    ) -> Result<T> {
        let key = self.projection_index(path, identity)?;
        Ok(project(&self.latest_indices.borrow()[&key].rows))
    }
    pub(crate) fn latest_by_id(&self, path: &str, identity: &str, id: &str) -> Result<Value> {
        let key = self.projection_index(path, identity)?;
        let indices = self.latest_indices.borrow();
        let index = &indices[&key];
        Ok(index
            .indices
            .get(id)
            .map(|at| index.rows[*at].clone())
            .unwrap_or(Value::Null))
    }
    pub fn save(&mut self, path: &str, value: &Value) -> Result<()> {
        self.path(path)?;
        let mut bytes = serde_json::to_vec_pretty(value)?;
        bytes.push(b'\n');
        self.staged.insert(path.into(), Some(bytes.clone()));
        self.writes.push(WriteOp::Save {
            path: path.into(),
            bytes,
        });
        Ok(())
    }
    pub(crate) fn save_bytes(&mut self, path: &str, bytes: &[u8]) -> Result<()> {
        self.path(path)?;
        self.staged.insert(path.into(), Some(bytes.to_vec()));
        self.writes.push(WriteOp::Save {
            path: path.into(),
            bytes: bytes.to_vec(),
        });
        Ok(())
    }
    pub(crate) fn owned_path(&self, path: &str) -> Result<PathBuf> {
        self.path(path)
    }
    pub fn append(&mut self, path: &str, value: &Value) -> Result<()> {
        self.path(path)?;
        let mut current = self.bytes(path)?.unwrap_or_default();
        let offset = current.len() as u64;
        ensure!(
            current.is_empty() || current.last() == Some(&b'\n'),
            "incomplete existing JSONL tail"
        );
        let mut bytes = serde_json::to_vec(value)?;
        bytes.push(b'\n');
        current.extend_from_slice(&bytes);
        self.staged.insert(path.into(), Some(current));
        self.writes.push(WriteOp::Append {
            path: path.into(),
            offset,
            bytes,
        });
        Ok(())
    }
    pub fn remove(&mut self, path: &str) -> Result<()> {
        self.path(path)?;
        self.staged.insert(path.into(), None);
        self.writes.push(WriteOp::Remove { path: path.into() });
        Ok(())
    }
    pub fn list(&self, category: &str) -> Result<Vec<String>> {
        ensure!(
            ["mailboxes", "leases"].contains(&category),
            "invalid state category"
        );
        let dir = self.root.join(category);
        let mut paths = Vec::new();
        if dir.is_dir() {
            for entry in fs::read_dir(dir)? {
                let entry = entry?;
                let name = entry
                    .file_name()
                    .into_string()
                    .map_err(|_| anyhow::anyhow!("invalid state filename"))?;
                let path = if category == "mailboxes" {
                    format!("mailboxes/{name}/mailbox.json")
                } else {
                    format!("leases/{name}")
                };
                if self.path(&path)?.is_file() {
                    paths.push(path);
                }
            }
        }
        paths.sort();
        Ok(paths)
    }
    pub fn rollback(&mut self) {
        self.staged.clear();
        self.writes.clear();
    }
    #[cfg(test)]
    pub(crate) fn interrupt_for_test(&self, boundary: usize) -> Result<usize> {
        atomic_write(
            &self.root.join("kernel/pending.json"),
            &serde_json::to_vec(&self.writes)?,
        )?;
        for op in self.writes.iter().take(boundary) {
            self.apply(op)?;
        }
        Ok(self.writes.len())
    }
    pub fn commit(&mut self) -> Result<()> {
        if self.writes.is_empty() {
            return Ok(());
        }
        let journal = self.root.join("kernel/pending.json");
        atomic_write(&journal, &serde_json::to_vec(&self.writes)?)?;
        // Once journaled, failures are fatal to the serving process: recovery
        // must finish the accepted transaction before another command runs.
        for op in &self.writes {
            let path = match op {
                WriteOp::Save { path, .. }
                | WriteOp::Append { path, .. }
                | WriteOp::Remove { path } => path,
            };
            let before = self.signature(path)?;
            self.apply(op)?;
            let after = self.signature(path)?;
            self.latest_indices
                .borrow_mut()
                .retain(|(cached_path, identity), index| {
                    if cached_path != path {
                        return true;
                    }
                    if index.signature != before {
                        return false;
                    }
                    match op {
                        WriteOp::Append { bytes, .. } => {
                            if let Ok(row) = serde_json::from_slice(bytes) {
                                if index.insert(identity, row).is_ok() {
                                    index.signature = after;
                                    return true;
                                }
                            }
                            false
                        }
                        _ => false,
                    }
                });
        }
        fs::remove_file(&journal)?;
        sync_parent(&journal)?;
        self.rollback();
        Ok(())
    }
    fn recover(&mut self) -> Result<()> {
        let path = self.root.join("kernel/pending.json");
        if path.exists() {
            let ops: Vec<WriteOp> = serde_json::from_slice(&fs::read(&path)?)?;
            // Validate every ownership boundary before replaying any member of
            // the transaction, including a prior message write ahead of a Job.
            for op in &ops {
                let target = match op {
                    WriteOp::Save { path, .. }
                    | WriteOp::Append { path, .. }
                    | WriteOp::Remove { path } => path,
                };
                self.path(target)?;
            }
            for op in &ops {
                self.apply(op)?;
            }
            fs::remove_file(&path)?;
            sync_parent(&path)?;
        }
        Ok(())
    }
    fn apply(&self, op: &WriteOp) -> Result<()> {
        match op {
            WriteOp::Save { path, bytes } => atomic_write(&self.path(path)?, bytes),
            WriteOp::Remove { path } => {
                let path = self.path(path)?;
                match fs::remove_file(&path) {
                    Ok(()) => sync_parent(&path),
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
                    Err(e) => Err(e.into()),
                }
            }
            WriteOp::Append {
                path,
                offset,
                bytes,
            } => {
                let path = self.path(path)?;
                fs::create_dir_all(path.parent().unwrap())?;
                let mut file = OpenOptions::new()
                    .create(true)
                    .truncate(false)
                    .read(true)
                    .write(true)
                    .open(&path)?;
                let length = file.metadata()?.len();
                ensure!(
                    length >= *offset,
                    "journal append offset precedes missing data"
                );
                file.seek(SeekFrom::Start(*offset))?;
                let mut tail = Vec::new();
                file.read_to_end(&mut tail)?;
                if tail.starts_with(bytes) {
                    return Ok(());
                }
                if !bytes.starts_with(&tail) {
                    bail!("journal append conflicts with existing data");
                }
                file.set_len(*offset)?;
                file.seek(SeekFrom::Start(*offset))?;
                file.write_all(bytes)?;
                file.sync_all()?;
                sync_parent(&path)
            }
        }
    }
}
fn atomic_write(path: &Path, bytes: &[u8]) -> Result<()> {
    fs::create_dir_all(path.parent().unwrap())?;
    let temp = path.with_extension("kernel-tmp");
    reject_link(&temp)?;
    let mut file = File::create(&temp)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        file.set_permissions(fs::Permissions::from_mode(0o600))?;
    }
    file.write_all(bytes)?;
    file.sync_all()?;
    drop(file);
    fs::rename(&temp, path)?;
    sync_parent(path)
}
fn reject_link(path: &Path) -> Result<()> {
    if let Ok(meta) = fs::symlink_metadata(path) {
        ensure!(
            !meta.file_type().is_symlink(),
            "linked kernel authority path"
        );
        #[cfg(windows)]
        {
            use std::os::windows::fs::MetadataExt;
            ensure!(
                meta.file_attributes() & 0x400 == 0,
                "reparse kernel authority path"
            );
        }
    }
    Ok(())
}
fn sync_parent(path: &Path) -> Result<()> {
    #[cfg(unix)]
    File::open(path.parent().unwrap())?.sync_all()?;
    #[cfg(not(unix))]
    let _ = path; // Windows directory flush is not provided by std.
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    #[test]
    fn exclusive_writer_and_invalid_paths() {
        let root = tempfile::tempdir().unwrap();
        let mut store = Store::open(root.path()).unwrap();
        assert!(Store::open(root.path()).is_err());
        assert!(store.save("mailboxes/../escape", &json!({})).is_err());
        assert!(store.save("unowned/file", &json!({})).is_err());
    }
    #[test]
    fn pi_role_assets_have_exact_owned_paths() {
        let root = tempfile::tempdir().unwrap();
        let mut store = Store::open(root.path()).unwrap();
        store
            .save_bytes(
                "providers/pi/worker/native-role-bridge.mjs",
                b"export default () => {};",
            )
            .unwrap();
        store
            .save("providers/pi/worker/role-plan.json", &json!({"roots":[]}))
            .unwrap();
        for path in [
            "providers/codex/worker/role-plan.json",
            "providers/claude/worker/native-role-bridge.mjs",
            "providers/pi/worker/other.mjs",
            "providers/pi/../role-plan.json",
        ] {
            assert!(store.save_bytes(path, b"x").is_err());
        }
        store.commit().unwrap();
        drop(store);
        let restored = Store::open(root.path()).unwrap();
        assert_eq!(
            restored.json("providers/pi/worker/role-plan.json").unwrap(),
            Some(json!({"roots":[]}))
        );
        assert!(restored
            .bytes("providers/pi/worker/native-role-bridge.mjs")
            .unwrap()
            .is_some());
    }
    #[test]
    fn journal_replays_partial_append_once() {
        let root = tempfile::tempdir().unwrap();
        let mut store = Store::open(root.path()).unwrap();
        store
            .append("mailboxes/pi/inbox.jsonl", &json!({"id":1}))
            .unwrap();
        store
            .save("mailboxes/pi/mailbox.json", &json!({"head":1}))
            .unwrap();
        let journal = serde_json::to_vec(&store.writes).unwrap();
        fs::write(root.path().join("kernel/pending.json"), journal).unwrap();
        fs::create_dir_all(root.path().join("mailboxes/pi")).unwrap();
        fs::write(root.path().join("mailboxes/pi/inbox.jsonl"), b"{\"id\"").unwrap();
        drop(store);
        let restored = Store::open(root.path()).unwrap();
        assert_eq!(
            restored.rows("mailboxes/pi/inbox.jsonl").unwrap(),
            vec![json!({"id":1})]
        );
        assert_eq!(
            restored.json("mailboxes/pi/mailbox.json").unwrap(),
            Some(json!({"head":1}))
        );
        drop(restored);
        let restored = Store::open(root.path()).unwrap();
        assert_eq!(restored.rows("mailboxes/pi/inbox.jsonl").unwrap().len(), 1);
    }
    #[test]
    fn cross_stream_journal_recovers_every_write_boundary() {
        for boundary in 0..=6 {
            let runtime = tempfile::tempdir().unwrap();
            let root = runtime.path().join("ccbd");
            let mut store = Store::open(&root).unwrap();
            store
                .append(
                    "messages/messages.jsonl",
                    &serde_json::json!({"message_id":"parent"}),
                )
                .unwrap();
            store
                .append(
                    "agents/pi/jobs.jsonl",
                    &serde_json::json!({"job_id":"continuation"}),
                )
                .unwrap();
            store
                .append(
                    "attempts/attempts.jsonl",
                    &serde_json::json!({"attempt_id":"once"}),
                )
                .unwrap();
            store
                .append(
                    "mailboxes/pi/inbox.jsonl",
                    &serde_json::json!({"inbound_event_id":"once"}),
                )
                .unwrap();
            store
                .save(
                    "mailboxes/pi/mailbox.json",
                    &serde_json::json!({"queue_depth":1}),
                )
                .unwrap();
            store
                .append(
                    "callbacks/edges.jsonl",
                    &serde_json::json!({"edge_id":"edge","continuation_job_id":"continuation"}),
                )
                .unwrap();
            atomic_write(
                &root.join("kernel/pending.json"),
                &serde_json::to_vec(&store.writes).unwrap(),
            )
            .unwrap();
            for op in store.writes.iter().take(boundary) {
                store.apply(op).unwrap();
            }
            drop(store);
            let recovered = Store::open(&root).unwrap();
            for path in [
                "messages/messages.jsonl",
                "agents/pi/jobs.jsonl",
                "attempts/attempts.jsonl",
                "mailboxes/pi/inbox.jsonl",
                "callbacks/edges.jsonl",
            ] {
                assert_eq!(
                    recovered.rows(path).unwrap().len(),
                    1,
                    "boundary {boundary}: {path}"
                );
            }
            assert_eq!(
                recovered.json("mailboxes/pi/mailbox.json").unwrap(),
                Some(serde_json::json!({"queue_depth":1}))
            );
            assert!(!root.join("kernel/pending.json").exists());
        }
    }
}
