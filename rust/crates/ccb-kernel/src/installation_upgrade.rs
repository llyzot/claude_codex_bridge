//! Recoverable program-directory upgrades; project/runtime state is never moved.
use anyhow::{ensure, Context, Result};
use serde_json::{json, Value};
use std::{
    fs,
    io::{Read, Write},
    path::{Path, PathBuf},
};
#[path = "installation_files.rs"]
mod files;
use files::Guards;

struct Paths {
    parent: PathBuf,
    target: PathBuf,
    journal: PathBuf,
    lock: PathBuf,
    name: String,
}
impl Paths {
    fn new(prefix: &Path) -> Result<Self> {
        ensure!(
            cfg!(all(windows, target_arch = "x86_64")),
            "native upgrade requires Windows x64"
        );
        ensure!(
            prefix.is_absolute(),
            "native upgrade prefix must be absolute"
        );
        let input = prefix
            .file_name()
            .and_then(|s| s.to_str())
            .context("native prefix must name a directory")?;
        ensure!(
            !input.ends_with(['.', ' ']) && !input.contains(':'),
            "ambiguous native prefix name"
        );
        let parent = prefix
            .parent()
            .context("native prefix parent missing")?
            .canonicalize()?;
        let mut target = parent.join(input);
        if present(&target)? {
            super::unlinked(&target)?;
            target = target.canonicalize()?;
        }
        ensure!(
            target.parent() == Some(parent.as_path()),
            "native prefix resolution escaped its parent"
        );
        let name = target
            .file_name()
            .and_then(|s| s.to_str())
            .context("invalid native prefix name")?
            .to_owned();
        Ok(Self {
            journal: parent.join(format!(".{name}.ccb-native-upgrade.json")),
            lock: parent.join(format!(".{name}.ccb-native-install.lock")),
            parent,
            target,
            name,
        })
    }
    fn stage(&self, id: &str) -> PathBuf {
        self.parent
            .join(format!(".{}.ccb-native-upgrade-{id}.stage", self.name))
    }
    fn backup(&self, id: &str) -> PathBuf {
        self.parent
            .join(format!(".{}.ccb-native-upgrade-{id}.backup", self.name))
    }
}
pub(super) fn present(path: &Path) -> Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e.into()),
    }
}
pub(super) fn describe(path: &Path) -> Result<Value> {
    files::descriptor(path)
}
fn terminal(record: &Value) -> bool {
    matches!(record["phase"].as_str(), Some("committed" | "rolled_back"))
}
fn read(paths: &Paths) -> Result<Option<Value>> {
    if !present(&paths.journal)? {
        return Ok(None);
    }
    super::unlinked(&paths.journal)?;
    let mut bytes = Vec::new();
    fs::File::open(&paths.journal)?
        .take(65537)
        .read_to_end(&mut bytes)?;
    ensure!(bytes.len() <= 65536, "upgrade journal exceeds size bound");
    let record: Value = serde_json::from_slice(&bytes)?;
    let id = record["operation_id"]
        .as_str()
        .context("upgrade identity missing")?;
    ensure!(
        uuid::Uuid::parse_str(id)?.to_string() == id
            && record["prefix_name"] == paths.name
            && record["schema_version"] == 1
            && record["record_type"] == "ccb_native_upgrade",
        "upgrade journal does not belong to this prefix"
    );
    ensure!(
        [
            "prepared",
            "old_retired",
            "committed",
            "rollback_prepared",
            "new_retired",
            "rolled_back"
        ]
        .contains(&record["phase"].as_str().unwrap_or("")),
        "unknown upgrade phase"
    );
    for kind in ["from", "to"] {
        for key in ["binary_sha256", "manifest_sha256"] {
            let sha = record[kind][key]
                .as_str()
                .context("upgrade fingerprint missing")?;
            ensure!(
                sha.len() == 64 && sha.bytes().all(|c| c.is_ascii_hexdigit()),
                "invalid upgrade fingerprint"
            );
        }
        ensure!(
            record[kind]["version"].is_string(),
            "upgrade version missing"
        );
    }
    Ok(Some(record))
}
fn save(paths: &Paths, record: &Value) -> Result<()> {
    if present(&paths.journal)? {
        super::unlinked(&paths.journal)?;
    }
    let mut file = tempfile::NamedTempFile::new_in(&paths.parent)?;
    file.write_all(&serde_json::to_vec_pretty(record)?)?;
    file.as_file().sync_all()?;
    file.persist(&paths.journal).map_err(|e| e.error)?;
    Ok(())
}
pub(super) fn check_pending(prefix: &Path) -> Result<()> {
    let paths = Paths::new(prefix)?;
    if let Some(record) = read(&paths)? {
        ensure!(
            terminal(&record),
            "unfinished native upgrade; inspect installation status and recover it first"
        );
    }
    Ok(())
}
fn location(path: &Path) -> Result<Option<Value>> {
    if !present(path)? {
        return Ok(None);
    }
    Ok(Some(describe(path)?))
}
fn response(paths: &Paths, record: &Value, status: &str) -> Value {
    let id = record["operation_id"].as_str().unwrap();
    json!({"status":status,"prefix":paths.target,"upgrade":record,"backup":paths.backup(id),
        "stage":paths.stage(id),"runtime_state_changed":false,"state_compatibility":"not_checked",
        "path_modified":false,"provider_settings_modified":false})
}
pub fn status(prefix: &Path) -> Result<Value> {
    let paths = Paths::new(prefix)?;
    let Some(record) = read(&paths)? else {
        return Ok(json!({"status":"no_upgrade_record","prefix":paths.target}));
    };
    let mut result = response(&paths, &record, "observed");
    let id = record["operation_id"].as_str().unwrap();
    let mut observations = json!({});
    for (name, path) in [
        ("target", paths.target.clone()),
        ("backup", paths.backup(id)),
        ("stage", paths.stage(id)),
    ] {
        observations[name] = match location(&path) {
            Ok(None) => json!({"state":"absent"}),
            Ok(Some(value)) => {
                json!({"state":if value==record["from"] {"previous"} else if value==record["to"] {"replacement"} else {"unexpected"},"identity":value})
            }
            Err(_) => json!({"state":"unavailable_or_unverified"}),
        };
    }
    result["observed"] = observations;
    Ok(result)
}

// A process crash can leave the journal behind a completed rename. Recovery
// identifies whole verified directories, never guesses from file age or PID.
fn recover(paths: &Paths, record: &mut Value) -> Result<Value> {
    let id = record["operation_id"].as_str().unwrap();
    let stage = paths.stage(id);
    let backup = paths.backup(id);
    let target_value = location(&paths.target)?;
    let stage_value = location(&stage)?;
    let backup_value = location(&backup)?;
    let old = &record["from"];
    let new = &record["to"];
    let forward =
        ["prepared", "old_retired", "committed"].contains(&record["phase"].as_str().unwrap());
    let a = target_value.as_ref() == Some(old)
        && stage_value.as_ref() == Some(new)
        && backup_value.is_none();
    let b = target_value.is_none()
        && stage_value.as_ref() == Some(new)
        && backup_value.as_ref() == Some(old);
    let c = target_value.as_ref() == Some(new)
        && stage_value.is_none()
        && backup_value.as_ref() == Some(old);
    if forward {
        ensure!(a||b||c,"upgrade directories do not match the recorded cutover; preserve them for explicit repair");
        ensure!(
            record["phase"] != "committed" || c,
            "committed native upgrade was changed"
        );
    } else {
        ensure!(a||b||c,"rollback directories do not match the recorded cutover; preserve them for explicit repair");
        ensure!(
            record["phase"] != "rolled_back" || a,
            "rolled-back native installation was changed"
        );
    }
    if terminal(record) {
        return Ok(response(
            paths,
            record,
            if forward {
                "already_upgraded"
            } else {
                "already_rolled_back"
            },
        ));
    }
    let mut guards = Guards::new();
    for (path, value) in [
        (&paths.target, &target_value),
        (&stage, &stage_value),
        (&backup, &backup_value),
    ] {
        if let Some(value) = value {
            guards.add(path, value)?;
        }
    }
    finish(paths, record, guards, a, b, c)
}
fn finish(
    paths: &Paths,
    record: &mut Value,
    mut guards: Guards,
    a: bool,
    b: bool,
    c: bool,
) -> Result<Value> {
    let id = record["operation_id"].as_str().unwrap();
    let stage = paths.stage(id);
    let backup = paths.backup(id);
    let forward = ["prepared", "old_retired"].contains(&record["phase"].as_str().unwrap());
    if forward {
        if a {
            guards.rename(&paths.target, &backup, &paths.parent)?;
            checkpoint("old_renamed");
            record["phase"] = json!("old_retired");
            save(paths, record)?;
        }
        if a || b {
            guards.rename(&stage, &paths.target, &paths.parent)?;
            checkpoint("new_published");
        }
        record["phase"] = json!("committed");
    } else {
        if c {
            guards.rename(&paths.target, &stage, &paths.parent)?;
            record["phase"] = json!("new_retired");
            save(paths, record)?;
        }
        if c || b {
            guards.rename(&backup, &paths.target, &paths.parent)?;
        }
        record["phase"] = json!("rolled_back");
    }
    record["finished_at"] = json!(crate::daemon::now());
    save(paths, record)?;
    Ok(response(
        paths,
        record,
        if forward { "upgraded" } else { "rolled_back" },
    ))
}

#[cfg(not(test))]
fn checkpoint(_: &str) {}
#[cfg(test)]
fn checkpoint(phase: &str) {
    if std::env::var("CCB_UPGRADE_TEST_PHASE").as_deref() == Ok(phase) {
        println!("CCB_UPGRADE_PHASE={phase}");
        std::io::stdout().flush().unwrap();
        loop {
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
    }
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;
    #[test]
    #[ignore = "subprocess worker for the real process-death cutover test"]
    fn crash_worker() {
        let prefix = PathBuf::from(std::env::var_os("CCB_UPGRADE_TEST_PREFIX").unwrap());
        manage("upgrade", &prefix, false, None, None).unwrap();
    }
    #[test]
    fn process_death_releases_launch_maps_and_recovery_finishes_actual_renames() {
        use std::{
            io::{BufRead, BufReader},
            os::windows::process::CommandExt,
            process::{Command, Stdio},
        };
        for phase in ["old_renamed", "new_published"] {
            let temp = tempfile::tempdir().unwrap();
            let prefix = temp.path().canonicalize().unwrap().join("native");
            fs::create_dir(&prefix).unwrap();
            let mut old = fs::read(std::env::current_exe().unwrap()).unwrap();
            old.extend(b"OLD_FIXTURE");
            super::super::populate(&prefix, &old, &super::super::digest(&old)).unwrap();
            let mut child = Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "installation::upgrade::tests::crash_worker",
                    "--ignored",
                    "--nocapture",
                ])
                .env("CCB_UPGRADE_TEST_PREFIX", &prefix)
                .env("CCB_UPGRADE_TEST_PHASE", phase)
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .creation_flags(0x08000000)
                .spawn()
                .unwrap();
            let mut reader = BufReader::new(child.stdout.take().unwrap());
            let mut observed = false;
            loop {
                let mut line = String::new();
                if reader.read_line(&mut line).unwrap() == 0 {
                    break;
                }
                if line.trim() == format!("CCB_UPGRADE_PHASE={phase}") {
                    observed = true;
                    break;
                }
            }
            if !observed {
                let _ = child.kill();
                let _ = child.wait();
                let mut error = String::new();
                child
                    .stderr
                    .take()
                    .unwrap()
                    .read_to_string(&mut error)
                    .unwrap();
                panic!("cutover did not reach checkpoint: {error}");
            }
            child.kill().unwrap();
            child.wait().unwrap();
            let recovered = manage("recover", &prefix, false, None, None).unwrap();
            assert_eq!(recovered["status"], "upgraded");
            assert_eq!(
                super::super::verify(&prefix).unwrap()["binary_sha256"],
                super::super::digest(&fs::read(std::env::current_exe().unwrap()).unwrap())
            );
        }
    }
}
pub fn manage(
    action: &str,
    prefix: &Path,
    dry_run: bool,
    operation: Option<&str>,
    expected: Option<&str>,
) -> Result<Value> {
    ensure!(
        ["upgrade", "recover", "rollback"].contains(&action),
        "unknown native installation action"
    );
    let paths = Paths::new(prefix)?;
    let supplied = operation
        .map(|s| uuid::Uuid::parse_str(s).map(|u| u.to_string()))
        .transpose()?;
    if action == "upgrade" {
        let bytes = fs::read(std::env::current_exe()?)?;
        ensure!(
            bytes.starts_with(b"MZ"),
            "upgrade source must be a Windows executable"
        );
        let sha = super::digest(&bytes);
        let prior = read(&paths)?;
        if let Some(record) = &prior {
            if supplied.as_deref() == record["operation_id"].as_str() {
                ensure!(
                    record["to"]["binary_sha256"] == sha
                        && expected.is_none_or(|value| record["from"]["manifest_sha256"] == value),
                    "upgrade operation identity is bound to another build or precondition"
                );
                ensure!(
                    record["phase"] != "rolled_back",
                    "this operation was rolled back; use a new upgrade identity"
                );
                if record["phase"] == "committed" {
                    return recover(&paths, &mut record.clone());
                }
            }
            if !terminal(record) {
                ensure!(supplied.as_deref()==record["operation_id"].as_str() && record["to"]["binary_sha256"]==sha,
                    "unfinished upgrade; use installation recover or retry its exact operation identity");
                ensure!(
                    expected.is_none_or(|value| record["from"]["manifest_sha256"] == value),
                    "native upgrade expected manifest mismatch"
                );
                if dry_run {
                    return status(prefix);
                }
                let _lock = super::installation_lock(&paths.lock)?;
                let mut current = read(&paths)?.context("upgrade journal disappeared")?;
                ensure!(
                    current == *record,
                    "upgrade journal changed before recovery"
                );
                return recover(&paths, &mut current);
            }
        }
        let old = describe(&paths.target)?;
        if let Some(expected) = expected {
            ensure!(
                old["manifest_sha256"] == expected,
                "native upgrade expected manifest mismatch"
            );
        }
        if old["binary_sha256"] == sha && old["version"] == super::version() {
            return Ok(json!({"status":"already_current","prefix":paths.target,"identity":old}));
        }
        let id = supplied.unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
        let stage = paths.stage(&id);
        let backup = paths.backup(&id);
        ensure!(
            !present(&stage)? && !present(&backup)?,
            "upgrade operation paths already exist; inspect their recorded owner"
        );
        let plan = json!({"status":"planned","prefix":paths.target,"operation_id":id,"from":old,
            "to_binary_sha256":sha,"to_version":super::version(),"backup":backup,"stage":stage,
            "requires_stopped_native_commands":true,"active_use_check":"performed_on_apply","runtime_state_changed":false});
        if dry_run {
            return Ok(plan);
        }
        let _lock = super::installation_lock(&paths.lock)?;
        ensure!(
            read(&paths)? == prior,
            "upgrade journal changed before installation lock"
        );
        ensure!(
            describe(&paths.target)? == old,
            "installation changed before upgrade lock"
        );
        let mut guards = Guards::new();
        guards.add(&paths.target, &old)?;
        // Keep incomplete stages as clearly named evidence. Never delete an
        // existing directory merely because it resembles an abandoned stage.
        fs::create_dir(&stage)?;
        super::populate(&stage, &bytes, &sha)?;
        let new = describe(&stage)?;
        guards.add(&stage, &new)?;
        let mut record = json!({"schema_version":1,"record_type":"ccb_native_upgrade","operation_id":id,
            "prefix_name":paths.name,"phase":"prepared","from":old,"to":new,"started_at":crate::daemon::now(),
            "runtime_state_changed":false});
        save(&paths, &record)?;
        return finish(&paths, &mut record, guards, true, false, false);
    }
    let mut record = read(&paths)?.context("no native upgrade is recorded")?;
    if let Some(id) = supplied {
        ensure!(
            record["operation_id"] == id,
            "native upgrade operation mismatch"
        );
    }
    if let Some(expected) = expected {
        ensure!(
            describe(&paths.target)?["manifest_sha256"] == expected,
            "native upgrade expected manifest mismatch"
        );
    }
    if dry_run {
        let mut result = status(prefix)?;
        result["requested_action"] = json!(action);
        return Ok(result);
    }
    let _lock = super::installation_lock(&paths.lock)?;
    ensure!(
        read(&paths)?.as_ref() == Some(&record),
        "upgrade journal changed before recovery lock"
    );
    if action == "rollback" {
        if record["phase"] == "rolled_back" {
            return recover(&paths, &mut record);
        }
        ensure!(
            record["phase"] == "committed",
            "finish the pending upgrade before requesting rollback"
        );
        let id = record["operation_id"].as_str().unwrap();
        let backup = paths.backup(id);
        ensure!(
            !present(&paths.stage(id))?
                && describe(&paths.target)? == record["to"]
                && describe(&backup)? == record["from"],
            "rollback input no longer matches the completed upgrade"
        );
        let mut guards = Guards::new();
        guards.add(&paths.target, &record["to"])?;
        guards.add(&backup, &record["from"])?;
        record["phase"] = json!("rollback_prepared");
        record.as_object_mut().unwrap().remove("finished_at");
        save(&paths, &record)?;
        return finish(&paths, &mut record, guards, false, false, true);
    }
    recover(&paths, &mut record)
}
