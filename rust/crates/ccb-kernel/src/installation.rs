//! Self-contained native prefix installation. No package manager, shell, account
//! settings, existing prefix replacement, or service controls are involved.
use anyhow::{ensure, Context, Result};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{
    ffi::OsString,
    fs,
    io::{Read, Write},
    path::{Path, PathBuf},
    process::Command,
};

const MANIFEST: &str = "NATIVE_INSTALLATION.json";
const BINARIES: [&str; 3] = ["bin/ccb.exe", "bin/ask.exe", "bin/ccb-native.exe"];
const LICENSE: &str = include_str!("../../../../LICENSE");
const GUIDE: &str = "CCB Rust runtime\n\nUse bin/ccb.exe and bin/ask.exe from PowerShell.\nNo Python runtime, venv, WezTerm or Git Bash is installed or required by this core.\nProviders retain their own runtime/authentication requirements; Git features require Git.\n\nFrom a project containing .ccb/ccb.config:\n  ccb start\n  ask worker task description\n  ccb watch <job-id> --wait\n  ccb reload --dry-run\n  ccb stop\n\nPATH and provider account settings are not modified. You can invoke the executables by full path.\nThis native command surface does not yet include Herdr panes, every legacy command, or production migration parity.\nInstall a new version to a separate directory; this installer never overwrites an existing installation.\nKeep the previous prefix for explicit rollback after stopping its owned services and reviewing state compatibility.\nUse ccb installation verify --prefix <directory> to check the installed files.\n";

pub fn version() -> &'static str {
    include_str!("../../../../VERSION").trim()
}
fn digest(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}
fn unlinked(path: &Path) -> Result<()> {
    let meta = fs::symlink_metadata(path)?;
    ensure!(
        !meta.file_type().is_symlink(),
        "linked native installation path"
    );
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        ensure!(
            meta.file_attributes() & 0x400 == 0,
            "reparse native installation path"
        );
    }
    Ok(())
}
fn read_manifest(prefix: &Path) -> Result<Value> {
    unlinked(prefix)?;
    let path = prefix.join(MANIFEST);
    unlinked(&path)?;
    let mut bytes = Vec::new();
    fs::File::open(path)?.take(65537).read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() <= 65536,
        "native installation manifest too large"
    );
    let record: Value = serde_json::from_slice(&bytes)?;
    ensure!(
        record["schema_version"] == 1
            && record["record_type"] == "ccb_native_installation"
            && record["runtime"] == "rust"
            && record["platform"] == "windows"
            && record["arch"] == "x86_64",
        "invalid native installation identity"
    );
    Ok(record)
}
fn file_digest(path: &Path) -> Result<String> {
    unlinked(path)?;
    let mut file = fs::File::open(path)?;
    let mut hash = Sha256::new();
    let mut buffer = [0; 65536];
    loop {
        let count = file.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        hash.update(&buffer[..count]);
    }
    Ok(format!("{hash:x}", hash = hash.finalize()))
}
pub fn verify(prefix: &Path) -> Result<Value> {
    let record = read_manifest(prefix)?;
    unlinked(&prefix.join("bin"))?;
    let files = record["files"]
        .as_object()
        .context("missing native installation files")?;
    let expected: Vec<_> = BINARIES
        .into_iter()
        .chain(["LICENSE", "VERSION", "README-native.txt"])
        .collect();
    ensure!(
        files.len() == expected.len() && expected.iter().all(|p| files.contains_key(*p)),
        "unexpected native installation file inventory"
    );
    for relative in expected {
        ensure!(
            files[relative] == file_digest(&prefix.join(relative))?,
            "native installation file changed: {relative}"
        );
    }
    ensure!(
        BINARIES
            .iter()
            .all(|p| files[*p] == record["binary_sha256"]),
        "native command binary identities differ"
    );
    ensure!(
        fs::read_to_string(prefix.join("VERSION"))?.trim()
            == record["version"].as_str().unwrap_or(""),
        "native installation version mismatch"
    );
    Ok(
        json!({"status":"verified","runtime":"rust","prefix":prefix.canonicalize()?,
        "version":record["version"],"binary_sha256":record["binary_sha256"]}),
    )
}
fn write(path: &Path, bytes: &[u8]) -> Result<()> {
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    Ok(())
}
pub fn install(prefix: &Path, dry_run: bool) -> Result<Value> {
    ensure!(
        cfg!(all(windows, target_arch = "x86_64")),
        "native installation currently supports Windows x64"
    );
    ensure!(
        prefix.is_absolute(),
        "native installation prefix must be absolute"
    );
    let name = prefix
        .file_name()
        .context("native installation prefix must name a new directory")?;
    let parent = prefix
        .parent()
        .context("missing installation parent")?
        .canonicalize()
        .context("native installation parent directory must already exist")?;
    let target = parent.join(name);
    let executable = std::env::current_exe()?;
    let bytes = fs::read(&executable)?;
    ensure!(
        bytes.starts_with(b"MZ"),
        "native installer is not a Windows executable"
    );
    let sha = digest(&bytes);
    if fs::symlink_metadata(&target).is_ok() {
        let verified = verify(&target)
            .context("destination exists; refusing to replace an unverified native installation")?;
        ensure!(verified["binary_sha256"] == sha && verified["version"] == version(),
            "destination contains another native build; install to a new prefix for an explicit upgrade");
        return Ok(
            json!({"status":"already_installed","prefix":target,"runtime":"rust","version":version(),"binary_sha256":sha}),
        );
    }
    let planned = json!({"status":if dry_run {"planned"} else {"installed"},"prefix":target,
        "runtime":"rust","version":version(),"core_version":env!("CARGO_PKG_VERSION"),"binary_sha256":sha,
        "path_modified":false,"provider_settings_modified":false,"python_required":false});
    if dry_run {
        return Ok(planned);
    }
    // A same-directory exclusive creation lock prevents competing installers
    // from racing publication. It is a regular file, never recursively removed.
    let lock_path = parent.join(format!(
        ".{}.ccb-native-install.lock",
        name.to_string_lossy()
    ));
    let lock = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&lock_path)
        .context("installation is already in progress or its creation lock requires inspection")?;
    let result = (|| {
        ensure!(
            fs::symlink_metadata(&target).is_err(),
            "native installation destination appeared during preparation"
        );
        let stage = tempfile::Builder::new()
            .prefix(".ccb-native-install-")
            .tempdir_in(&parent)?;
        fs::create_dir(stage.path().join("bin"))?;
        let mut files = json!({});
        let primary = stage.path().join("bin/ccb-native.exe");
        write(&primary, &bytes)?;
        for relative in BINARIES {
            let destination = stage.path().join(relative);
            if destination != primary && fs::hard_link(&primary, &destination).is_err() {
                fs::copy(&primary, &destination)?;
                fs::OpenOptions::new()
                    .write(true)
                    .open(&destination)?
                    .sync_all()?;
            }
            files[relative] = json!(sha);
        }
        for (relative, content) in [
            ("LICENSE", LICENSE),
            ("VERSION", version()),
            ("README-native.txt", GUIDE),
        ] {
            write(&stage.path().join(relative), content.as_bytes())?;
            files[relative] = json!(digest(content.as_bytes()));
        }
        let record = json!({"schema_version":1,"record_type":"ccb_native_installation","runtime":"rust",
            "platform":"windows","arch":"x86_64","version":version(),"core_version":env!("CARGO_PKG_VERSION"),
            "binary_sha256":sha,"files":files});
        write(
            &stage.path().join(MANIFEST),
            &serde_json::to_vec_pretty(&record)?,
        )?;
        verify(stage.path())?;
        // Windows refuses to rename onto an existing directory. Publication has
        // no overwrite path and requires no deletion of an existing prefix.
        fs::rename(stage.path(), &target)?;
        // TempDir now points at the vanished stage name, not at the published prefix.
        drop(stage);
        Ok(planned)
    })();
    drop(lock);
    fs::remove_file(&lock_path)?;
    result
}

/// Provider tools should resolve commands from this installed native prefix,
/// even if an older Python installation appears first in the account PATH.
pub fn provider_commands(command: &mut Command, spec: &Value) -> Result<()> {
    let executable = std::env::current_exe()?;
    let Some(bin) = executable.parent() else {
        return Ok(());
    };
    let Some(prefix) = bin.parent() else {
        return Ok(());
    };
    if !prefix.join(MANIFEST).exists() {
        return Ok(());
    }
    read_manifest(prefix)?;
    ensure!(
        bin.file_name().is_some_and(|n| n == "bin"),
        "invalid native installation command directory"
    );
    let inherited = spec["env"]
        .as_object()
        .and_then(|values| {
            values
                .iter()
                .filter(|(key, _)| key.eq_ignore_ascii_case("PATH"))
                .map(|(_, value)| value)
                .next_back()
        })
        .and_then(Value::as_str)
        .map(OsString::from)
        .or_else(|| std::env::var_os("PATH"))
        .unwrap_or_default();
    let paths = std::iter::once(PathBuf::from(bin)).chain(std::env::split_paths(&inherited));
    command
        .env("PATH", std::env::join_paths(paths)?)
        .env("CCB_INSTALL_PREFIX", prefix)
        .env("CCB_NATIVE_CLI", "1");
    Ok(())
}
