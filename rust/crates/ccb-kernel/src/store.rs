//! One writer, redo journal, and idempotent append recovery.
use anyhow::{bail, ensure, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Component, Path, PathBuf};

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
    _lock: File,
    staged: HashMap<String, Option<Vec<u8>>>,
    writes: Vec<WriteOp>,
}
impl Store {
    pub fn open(root: &Path) -> Result<Self> {
        fs::create_dir_all(root)?;
        let root = root.canonicalize()?;
        fs::create_dir_all(root.join("kernel"))?;
        reject_link(&root.join("kernel"))?;
        reject_link(&root.join("kernel/writer.lock"))?;
        reject_link(&root.join("kernel/pending.json"))?;
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
            staged: HashMap::new(),
            writes: Vec::new(),
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
        ensure!(
            relative.starts_with("mailboxes/") || relative.starts_with("leases/"),
            "unowned state path"
        );
        let full = self.root.join(path);
        let mut prefix = self.root.clone();
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
    pub fn commit(&mut self) -> Result<()> {
        if self.writes.is_empty() {
            return Ok(());
        }
        let journal = self.root.join("kernel/pending.json");
        atomic_write(&journal, &serde_json::to_vec(&self.writes)?)?;
        // Once journaled, failures are fatal to the serving process: recovery
        // must finish the accepted transaction before another command runs.
        for op in &self.writes {
            self.apply(op)?;
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
}
