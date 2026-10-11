//! Explicit, recoverable removal of one verified native program directory.
//! Retained upgrade versions and project/account data are separate resources.
use super::{files, present, read as upgrade_record, terminal, Guards, Paths};
use anyhow::{ensure, Context, Result};
use serde_json::{json, Value};
use std::{
    fs,
    io::{Read, Write},
    path::{Path, PathBuf},
};

fn journal(paths: &Paths) -> PathBuf {
    paths
        .parent
        .join(format!(".{}.ccb-native-remove.json", paths.name))
}
fn retired(paths: &Paths, id: &str) -> PathBuf {
    paths
        .parent
        .join(format!(".{}.ccb-native-remove-{id}.retired", paths.name))
}
fn receipt(paths: &Paths, id: &str) -> PathBuf {
    paths
        .parent
        .join(format!(".{}.ccb-native-remove-{id}.json", paths.name))
}
fn read(paths: &Paths) -> Result<Option<Value>> {
    read_file(paths, &journal(paths))
}
fn read_file(paths: &Paths, path: &Path) -> Result<Option<Value>> {
    if !present(path)? {
        return Ok(None);
    }
    super::super::unlinked(path)?;
    let mut bytes = Vec::new();
    fs::File::open(path)?.take(65537).read_to_end(&mut bytes)?;
    ensure!(bytes.len() <= 65536, "removal journal exceeds size bound");
    let row: Value = serde_json::from_slice(&bytes)?;
    let id = row["operation_id"]
        .as_str()
        .context("removal identity missing")?;
    ensure!(
        uuid::Uuid::parse_str(id)?.to_string() == id
            && row["prefix_name"] == paths.name
            && row["schema_version"] == 1
            && row["record_type"] == "ccb_native_removal",
        "removal journal does not belong to this prefix"
    );
    ensure!(
        ["prepared", "retired", "deleting", "removed"]
            .contains(&row["phase"].as_str().unwrap_or("")),
        "unknown removal phase"
    );
    let proof = &row["proof"];
    for id in std::iter::once(&proof["root_id"])
        .chain(std::iter::once(&proof["bin_id"]))
        .chain(
            files::FILES
                .iter()
                .map(|p| &proof["entries"][*p]["file_id"]),
        )
    {
        ensure!(
            id.as_array().is_some_and(|a| a.len() == 3
                && a.iter()
                    .all(|v| v.as_u64().is_some_and(|v| v <= u32::MAX as u64))),
            "invalid removal file identity"
        );
    }
    ensure!(
        proof["entries"]
            .as_object()
            .is_some_and(|o| o.len() == files::FILES.len()),
        "invalid removal inventory"
    );
    for hash in files::FILES
        .iter()
        .map(|p| &proof["entries"][*p]["sha256"])
        .chain([
            &proof["identity"]["manifest_sha256"],
            &proof["identity"]["binary_sha256"],
        ])
    {
        ensure!(
            hash.as_str()
                .is_some_and(|s| s.len() == 64 && s.bytes().all(|b| b.is_ascii_hexdigit())),
            "invalid removal fingerprint"
        );
    }
    ensure!(
        proof["identity"]["version"].is_string(),
        "missing removal version"
    );
    Ok(Some(row))
}
fn archive(paths: &Paths, row: &Value) -> Result<()> {
    ensure!(
        row["phase"] == "removed",
        "only completed removal can be archived"
    );
    let path = receipt(paths, row["operation_id"].as_str().unwrap());
    if let Some(prior) = read_file(paths, &path)? {
        ensure!(prior == *row, "completed removal receipt changed");
    } else {
        let mut file = tempfile::NamedTempFile::new_in(&paths.parent)?;
        file.write_all(&serde_json::to_vec_pretty(row)?)?;
        file.as_file().sync_all()?;
        file.persist_noclobber(path).map_err(|e| e.error)?;
    }
    Ok(())
}
fn save(paths: &Paths, row: &Value) -> Result<()> {
    let path = journal(paths);
    if present(&path)? {
        super::super::unlinked(&path)?;
    }
    let mut file = tempfile::NamedTempFile::new_in(&paths.parent)?;
    file.write_all(&serde_json::to_vec_pretty(row)?)?;
    file.as_file().sync_all()?;
    file.persist(path).map_err(|e| e.error)?;
    Ok(())
}
pub(super) fn check_pending(paths: &Paths) -> Result<()> {
    ensure!(
        read(paths)?.is_none_or(|r| r["phase"] == "removed"),
        "unfinished native removal; repeat installation remove with its operation identity"
    );
    Ok(())
}
pub(super) fn status(paths: &Paths) -> Result<Value> {
    Ok(read(paths)?.unwrap_or(Value::Null))
}
fn response(paths: &Paths, row: &Value, status: &str) -> Value {
    json!({"status":status,"prefix":paths.target,"retired":retired(paths,row["operation_id"].as_str().unwrap()),
        "removal":row,"runtime_state_changed":false,"path_modified":false,"provider_settings_modified":false,
        "retained_upgrade_versions_changed":false})
}
fn finish(paths: &Paths, row: &mut Value) -> Result<Value> {
    let gone = retired(paths, row["operation_id"].as_str().unwrap());
    if row["phase"] == "removed" {
        archive(paths, row)?;
        return Ok(response(paths, row, "already_removed"));
    }
    if row["phase"] == "prepared" {
        let target = present(&paths.target)?;
        let staged = present(&gone)?;
        ensure!(
            target != staged,
            "removal directories are ambiguous; preserve them for repair"
        );
        let current = if target { &paths.target } else { &gone };
        files::matches_removal_root(current, &row["proof"])?;
        let mut guards = Guards::new();
        guards.add(current, &row["proof"]["identity"])?;
        if target {
            guards.rename(&paths.target, &gone, &paths.parent)?;
        }
        row["phase"] = json!("retired");
        save(paths, row)?;
    }
    ensure!(
        !present(&paths.target)?,
        "a prefix appeared during removal; preserve it for repair"
    );
    if row["phase"] == "retired" {
        files::matches_removal_root(&gone, &row["proof"])?;
        ensure!(
            super::describe(&gone)? == row["proof"]["identity"],
            "retired installation changed before deletion"
        );
        row["phase"] = json!("deleting");
        save(paths, row)?;
        super::checkpoint("removal_deleting");
    }
    if present(&gone)? {
        files::purge_removal(&gone, &row["proof"])?;
    }
    super::checkpoint("removal_deleted");
    row["phase"] = json!("removed");
    row["finished_at"] = json!(crate::daemon::now());
    save(paths, row)?;
    archive(paths, row)?;
    Ok(response(paths, row, "removed"))
}
pub(super) fn manage(
    prefix: &Path,
    dry_run: bool,
    operation: Option<&str>,
    expected: Option<&str>,
) -> Result<Value> {
    let paths = Paths::new(prefix)?;
    let supplied = operation
        .map(|s| uuid::Uuid::parse_str(s).map(|u| u.to_string()))
        .transpose()?;
    if let Some(id) = &supplied {
        if let Some(row) = read_file(&paths, &receipt(&paths, id))? {
            ensure!(
                row["phase"] == "removed" && row["operation_id"] == *id,
                "invalid completed removal receipt"
            );
            ensure!(
                expected.is_none_or(|h| row["proof"]["identity"]["manifest_sha256"] == h),
                "removal expected manifest mismatch"
            );
            return Ok(response(&paths, &row, "already_removed"));
        }
    }
    let prior = read(&paths)?;
    if let Some(row) = &prior {
        if row["phase"] != "removed" || supplied.as_deref() == row["operation_id"].as_str() {
            ensure!(
                supplied.as_deref() == row["operation_id"].as_str(),
                "pending removal requires its exact --operation-id"
            );
            ensure!(
                expected.is_none_or(|h| row["proof"]["identity"]["manifest_sha256"] == h),
                "removal expected manifest mismatch"
            );
            if dry_run {
                return Ok(response(&paths, row, "planned_recovery"));
            }
            let _lock = super::super::installation_lock(&paths.lock)?;
            ensure!(
                read(&paths)?.as_ref() == Some(row),
                "removal journal changed before lock"
            );
            return finish(&paths, &mut row.clone());
        }
    }
    let upgrade = upgrade_record(&paths)?;
    ensure!(
        upgrade.as_ref().is_none_or(terminal),
        "finish the pending native upgrade before removal"
    );
    let proof = files::removal_proof(&paths.target)?;
    ensure!(
        expected.is_none_or(|h| proof["identity"]["manifest_sha256"] == h),
        "removal expected manifest mismatch"
    );
    let id = supplied.unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
    ensure!(
        !present(&retired(&paths, &id))?,
        "removal retirement path already exists"
    );
    let mut row = json!({"schema_version":1,"record_type":"ccb_native_removal","prefix_name":paths.name,
        "operation_id":id,"phase":"prepared","proof":proof,"started_at":crate::daemon::now()});
    if dry_run {
        let mut result = response(&paths, &row, "planned");
        result["identity"] = proof["identity"].clone();
        result["requires_stopped_native_commands"] = json!(true);
        return Ok(result);
    }
    let _lock = super::super::installation_lock(&paths.lock)?;
    ensure!(
        read(&paths)? == prior && upgrade_record(&paths)? == upgrade,
        "installation journal changed before removal lock"
    );
    ensure!(
        files::removal_proof(&paths.target)? == proof,
        "installation changed before removal lock"
    );
    if let Some(prior) = &prior {
        archive(&paths, prior)?;
    }
    let mut guards = Guards::new();
    guards.add(&paths.target, &proof["identity"])?;
    save(&paths, &row)?;
    guards.rename(&paths.target, &retired(&paths, &id), &paths.parent)?;
    super::checkpoint("removal_renamed");
    row["phase"] = json!("retired");
    save(&paths, &row)?;
    drop(guards);
    finish(&paths, &mut row)
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;
    #[test]
    #[ignore = "subprocess worker for native removal process-death test"]
    fn crash_worker() {
        let prefix = PathBuf::from(std::env::var_os("CCB_REMOVE_TEST_PREFIX").unwrap());
        let id = std::env::var("CCB_REMOVE_TEST_ID").unwrap();
        manage(&prefix, false, Some(&id), None).unwrap();
    }
    #[test]
    fn process_death_at_removal_boundaries_recovers_only_the_recorded_prefix() {
        use std::{
            io::{BufRead, BufReader},
            os::windows::process::CommandExt,
            process::{Command, Stdio},
            sync::mpsc,
            time::Duration,
        };
        for phase in [
            "removal_renamed",
            "removal_deleting",
            "removal_first_file",
            "removal_deleted",
        ] {
            let temp = tempfile::tempdir().unwrap();
            let prefix = temp.path().canonicalize().unwrap().join("native");
            fs::create_dir(&prefix).unwrap();
            let bytes = fs::read(std::env::current_exe().unwrap()).unwrap();
            super::super::super::populate(&prefix, &bytes, &super::super::super::digest(&bytes))
                .unwrap();
            let id = uuid::Uuid::new_v4().to_string();
            let mut child = Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "installation::upgrade::removal::tests::crash_worker",
                    "--ignored",
                    "--nocapture",
                ])
                .env("CCB_REMOVE_TEST_PREFIX", &prefix)
                .env("CCB_REMOVE_TEST_ID", &id)
                .env("CCB_UPGRADE_TEST_PHASE", phase)
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .creation_flags(0x08000000)
                .spawn()
                .unwrap();
            let output = child.stdout.take().unwrap();
            let (tx, rx) = mpsc::channel();
            std::thread::spawn(move || {
                let found = BufReader::new(output)
                    .lines()
                    .map_while(std::result::Result::ok)
                    .any(|line| line.trim() == format!("CCB_UPGRADE_PHASE={phase}"));
                let _ = tx.send(found);
            });
            let observed = rx.recv_timeout(Duration::from_secs(30)).unwrap_or(false);
            let _ = child.kill();
            child.wait().unwrap();
            let mut error = String::new();
            child
                .stderr
                .take()
                .unwrap()
                .read_to_string(&mut error)
                .unwrap();
            assert!(observed, "removal did not reach {phase}: {error}");
            let result = manage(&prefix, false, Some(&id), None).unwrap();
            assert_eq!(result["status"], "removed");
            assert!(!prefix.exists());
            assert!(!Path::new(result["retired"].as_str().unwrap()).exists());
            assert_eq!(
                manage(&prefix, false, Some(&id), None).unwrap()["status"],
                "already_removed"
            );
        }
    }
}
