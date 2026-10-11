//! One controller directory owns the Agent journals beside it. This complements
//! the controller-local writer lock; no path, age or PID guess transfers ownership.
use anyhow::{ensure, Context, Result};
use serde_json::{json, Value};
use std::{
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    path::Path,
};

const OWNER: &str = ".ccb-journal-owner.json";
const LOCK: &str = ".ccb-journal-owner.lock";
fn unlinked(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(meta) => {
            ensure!(
                !meta.file_type().is_symlink(),
                "linked Agent journal authority path"
            );
            #[cfg(windows)]
            {
                use std::os::windows::fs::MetadataExt;
                ensure!(
                    meta.file_attributes() & 0x400 == 0,
                    "reparse Agent journal authority path"
                );
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    Ok(())
}
fn lock(path: &Path) -> Result<File> {
    unlinked(path)?;
    let mut options = OpenOptions::new();
    options.read(true).write(true).create(true).truncate(false);
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        use windows_sys::Win32::Storage::FileSystem::FILE_FLAG_OPEN_REPARSE_POINT;
        options.custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
    }
    let file = options.open(path)?;
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        ensure!(
            file.metadata()?.file_attributes() & 0x400 == 0,
            "reparse Agent journal lock"
        );
    }
    file.try_lock()
        .context("Agent journal parent has an active writer")?;
    Ok(file)
}
fn peer_roots(root: &Path, agents: &Path, known_controller: bool) -> Result<()> {
    let parent = root.parent().context("missing runtime parent")?;
    let mut has_logs = false;
    for entry in fs::read_dir(agents)? {
        let entry = entry?;
        if entry.file_type()?.is_dir() {
            unlinked(&entry.path())?;
            if entry.path().join("jobs.jsonl").exists()
                || entry.path().join("events.jsonl").exists()
            {
                has_logs = true;
            }
        }
    }
    ensure!(
        !has_logs || known_controller,
        "legacy Agent journals lack a known controller; explicit migration is required"
    );
    // First binding only. Existing log data and another controller/journal are
    // ambiguous; an unused, stopped kernel probe alone does not own Agent data.
    let mut count = 0;
    for entry in fs::read_dir(parent)? {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        count += 1;
        ensure!(
            count <= 4096,
            "too many runtime-parent entries to establish journal ownership"
        );
        let peer = entry.path();
        unlinked(&peer)?;
        if peer.canonicalize()? == root || peer == agents {
            continue;
        }
        let writer = peer.join("kernel/writer.lock");
        let evidence = [
            "lease.json",
            "configuration.json",
            "submissions.jsonl",
            "messages/messages.jsonl",
            "attempts/attempts.jsonl",
            "callbacks/edges.jsonl",
            "kernel/pending.json",
        ]
        .iter()
        .any(|name| peer.join(name).exists());
        if writer.exists() {
            unlinked(&peer.join("kernel"))?;
            let _probe =
                lock(&writer).context("another controller in this runtime parent is active")?;
            ensure!(
                !has_logs && !evidence,
                "ambiguous legacy Agent journal parent; use its original controller directory"
            );
        } else {
            ensure!(
                !evidence,
                "another controller already owns data in this runtime parent"
            );
        }
    }
    Ok(())
}
pub(crate) fn claim(root: &Path, known_controller: bool) -> Result<File> {
    let parent = root.parent().context("missing Agent journal parent")?;
    let name = root
        .file_name()
        .and_then(|s| s.to_str())
        .context("controller directory must have a UTF-8 name")?;
    let agents = parent.join("agents");
    unlinked(&agents)?;
    fs::create_dir_all(&agents)?;
    unlinked(&agents)?;
    let held = lock(&agents.join(LOCK))?;
    let owner = agents.join(OWNER);
    unlinked(&owner)?;
    if owner.exists() {
        let mut bytes = Vec::new();
        File::open(&owner)?.take(8193).read_to_end(&mut bytes)?;
        ensure!(
            bytes.len() <= 8192,
            "Agent journal authority record too large"
        );
        let record: Value =
            serde_json::from_slice(&bytes).context("invalid Agent journal authority record")?;
        ensure!(
            record["schema_version"] == 1
                && record["record_type"] == "ccb_agent_journal_owner"
                && record["controller_dir"] == name,
            "Agent journals belong to another controller directory; use a separate runtime parent"
        );
    } else {
        peer_roots(root, &agents, known_controller)?;
        let record = json!({"schema_version":1,"record_type":"ccb_agent_journal_owner","controller_dir":name});
        let mut temp = tempfile::NamedTempFile::new_in(&agents)?;
        temp.write_all(&serde_json::to_vec(&record)?)?;
        temp.as_file().sync_all()?;
        temp.persist_noclobber(&owner)
            .map_err(|e| e.error)
            .context("Agent journal owner appeared during claim")?;
        #[cfg(unix)]
        File::open(&agents)?.sync_all()?;
    }
    Ok(held)
}
