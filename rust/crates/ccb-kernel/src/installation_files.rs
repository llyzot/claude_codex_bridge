//! Windows file identities and exclusion for a stopped native installation.
use anyhow::{ensure, Context, Result};
use serde_json::{json, Value};
use std::{
    collections::BTreeMap,
    fs,
    io::{Read, Seek, SeekFrom},
    path::{Path, PathBuf},
};
type Identity = (u32, u32, u32);

pub(super) const FILES: [&str; 7] = [
    "bin/ccb.exe",
    "bin/ask.exe",
    "bin/ccb-native.exe",
    "NATIVE_INSTALLATION.json",
    "LICENSE",
    "VERSION",
    "README-native.txt",
];
pub(super) fn inventory(root: &Path) -> Result<()> {
    super::super::unlinked(root)?;
    for (directory, expected) in [
        (
            root.to_path_buf(),
            vec![
                "bin",
                "NATIVE_INSTALLATION.json",
                "LICENSE",
                "VERSION",
                "README-native.txt",
            ],
        ),
        (
            root.join("bin"),
            vec!["ccb.exe", "ask.exe", "ccb-native.exe"],
        ),
    ] {
        super::super::unlinked(&directory)?;
        let entries = fs::read_dir(directory)?
            .map(|entry| Ok(entry?.file_name()))
            .collect::<Result<Vec<_>>>()?;
        ensure!(
            entries.len() == expected.len()
                && expected
                    .iter()
                    .all(|name| entries.iter().any(|entry| entry == name)),
            "native upgrade refuses additional or missing installation entries"
        );
    }
    Ok(())
}
#[cfg(windows)]
fn metadata_handle(path: &Path) -> Result<fs::File> {
    use std::os::windows::fs::OpenOptionsExt;
    use windows_sys::Win32::Storage::FileSystem::*;
    let file = fs::OpenOptions::new()
        .access_mode(FILE_READ_ATTRIBUTES)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT | FILE_FLAG_BACKUP_SEMANTICS)
        .open(path)?;
    use std::os::windows::fs::MetadataExt;
    ensure!(
        file.metadata()?.file_attributes() & 0x400 == 0,
        "linked native upgrade entry"
    );
    Ok(file)
}
#[cfg(not(windows))]
fn metadata_handle(_: &Path) -> Result<fs::File> {
    anyhow::bail!("native upgrades require Windows")
}
#[cfg(windows)]
fn identity(file: &fs::File) -> Result<Identity> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Storage::FileSystem::{
        GetFileInformationByHandle, BY_HANDLE_FILE_INFORMATION,
    };
    let mut info: BY_HANDLE_FILE_INFORMATION = unsafe { std::mem::zeroed() };
    ensure!(
        unsafe { GetFileInformationByHandle(file.as_raw_handle(), &mut info) } != 0,
        "cannot identify installation entry"
    );
    Ok((
        info.dwVolumeSerialNumber,
        info.nFileIndexHigh,
        info.nFileIndexLow,
    ))
}
#[cfg(not(windows))]
fn identity(_: &fs::File) -> Result<Identity> {
    anyhow::bail!("native upgrades require Windows")
}
fn path_identity(path: &Path) -> Result<Identity> {
    identity(&metadata_handle(path)?)
}
fn exclusive(path: &Path) -> Result<fs::File> {
    let mut options = fs::OpenOptions::new();
    options.read(true).write(true);
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        use windows_sys::Win32::Storage::FileSystem::{
            FILE_FLAG_OPEN_REPARSE_POINT, FILE_SHARE_DELETE,
        };
        // Write access fails against mapped executable images. Denying read and
        // write sharing also prevents a new launch or edit until cutover ends.
        options
            .share_mode(FILE_SHARE_DELETE)
            .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
    }
    let file=options.open(path).context("native installation files are running, locked or not writable; stop its commands/services before upgrading")?;
    ensure!(
        file.metadata()?.is_file(),
        "invalid native installation file"
    );
    Ok(file)
}
fn mapped_read(path: &Path) -> Result<fs::File> {
    let mut options = fs::OpenOptions::new();
    options.read(true);
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        use windows_sys::Win32::Storage::FileSystem::*;
        options
            .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
            .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
    }
    Ok(options.open(path)?)
}
#[derive(Clone)]
struct Root {
    path: PathBuf,
    entries: BTreeMap<String, Identity>,
    expected: Value,
}
#[cfg(windows)]
struct ImageMapping {
    handle: windows_sys::Win32::Foundation::HANDLE,
    view: windows_sys::Win32::System::Memory::MEMORY_MAPPED_VIEW_ADDRESS,
}
#[cfg(windows)]
impl ImageMapping {
    fn new(file: &fs::File) -> Result<Self> {
        use std::os::windows::io::AsRawHandle;
        use windows_sys::Win32::System::Memory::*;
        let handle = unsafe {
            CreateFileMappingW(
                file.as_raw_handle(),
                std::ptr::null(),
                PAGE_READWRITE,
                0,
                0,
                std::ptr::null(),
            )
        };
        ensure!(
            !handle.is_null(),
            "cannot establish native executable launch exclusion"
        );
        let view = unsafe { MapViewOfFile(handle, FILE_MAP_WRITE, 0, 0, 0) };
        if view.Value.is_null() {
            unsafe {
                windows_sys::Win32::Foundation::CloseHandle(handle);
            }
            anyhow::bail!("cannot map native executable for launch exclusion");
        }
        Ok(Self { handle, view })
    }
}
#[cfg(windows)]
impl Drop for ImageMapping {
    fn drop(&mut self) {
        unsafe {
            windows_sys::Win32::System::Memory::UnmapViewOfFile(self.view);
            windows_sys::Win32::Foundation::CloseHandle(self.handle);
        }
    }
}
#[cfg(not(windows))]
struct ImageMapping;
#[cfg(not(windows))]
impl ImageMapping {
    fn new(_: &fs::File) -> Result<Self> {
        anyhow::bail!("native upgrades require Windows")
    }
}
pub(super) struct Guards {
    files: Vec<fs::File>,
    identities: BTreeMap<Identity, usize>,
    roots: Vec<Root>,
    mappings: BTreeMap<Identity, ImageMapping>,
}
impl Guards {
    pub fn new() -> Self {
        Self {
            files: Vec::new(),
            identities: BTreeMap::new(),
            roots: Vec::new(),
            mappings: BTreeMap::new(),
        }
    }
    pub fn add(&mut self, path: &Path, expected: &Value) -> Result<()> {
        inventory(path)?;
        let record = super::super::read_manifest(path)?;
        ensure!(
            super::describe(path)? == *expected,
            "installation changed before exclusive access"
        );
        let mut entries = BTreeMap::new();
        for relative in [".", "bin"] {
            entries.insert(relative.into(), path_identity(&path.join(relative))?);
        }
        for relative in FILES {
            let full = path.join(relative);
            let id = path_identity(&full)?;
            let index = if let Some(index) = self.identities.get(&id) {
                *index
            } else {
                // Our writable section itself keeps a write reference alive.
                // Re-open compatibly for hashing after rename; the mapping
                // continues to exclude executable launches throughout.
                let file = if self.mappings.contains_key(&id) {
                    mapped_read(&full)?
                } else {
                    exclusive(&full)?
                };
                ensure!(
                    identity(&file)? == id,
                    "installation file was replaced during exclusion"
                );
                let index = self.files.len();
                self.files.push(file);
                self.identities.insert(id, index);
                index
            };
            let file = &mut self.files[index];
            if relative.ends_with(".exe") && !self.mappings.contains_key(&id) {
                self.mappings.insert(id, ImageMapping::new(file)?);
            }
            file.seek(SeekFrom::Start(0))?;
            let mut bytes = Vec::new();
            file.read_to_end(&mut bytes)?;
            let expected_hash = if relative == "NATIVE_INSTALLATION.json" {
                &expected["manifest_sha256"]
            } else {
                &record["files"][relative]
            };
            ensure!(
                *expected_hash == super::super::digest(&bytes),
                "installation file changed before cutover"
            );
            entries.insert(relative.into(), id);
        }
        self.roots.push(Root {
            path: path.to_path_buf(),
            entries,
            expected: expected.clone(),
        });
        self.check(path)
    }
    pub fn check(&self, path: &Path) -> Result<()> {
        let root = self
            .roots
            .iter()
            .find(|root| root.path == path)
            .context("unguarded installation directory")?;
        inventory(path)?;
        for (relative, id) in &root.entries {
            ensure!(
                path_identity(&path.join(relative))? == *id,
                "guarded installation entry was replaced"
            );
        }
        Ok(())
    }
    pub fn rename(&mut self, from: &Path, to: &Path, parent: &Path) -> Result<()> {
        ensure!(
            from.parent() == Some(parent) && to.parent() == Some(parent),
            "upgrade rename escaped its selected parent"
        );
        self.check(from)?;
        ensure!(!super::present(to)?, "upgrade destination already exists");
        // Windows refuses directory rename with child file handles, even with
        // FILE_SHARE_DELETE. Writable mappings preserve launch exclusion while
        // those handles are closed. No mapped byte is ever modified.
        self.files.clear();
        self.identities.clear();
        #[cfg(windows)]
        {
            use std::os::windows::ffi::OsStrExt;
            let from_w: Vec<_> = from.as_os_str().encode_wide().chain(Some(0)).collect();
            let to_w: Vec<_> = to.as_os_str().encode_wide().chain(Some(0)).collect();
            if unsafe {
                windows_sys::Win32::Storage::FileSystem::MoveFileExW(
                    from_w.as_ptr(),
                    to_w.as_ptr(),
                    0,
                )
            } == 0
            {
                return Err(std::io::Error::last_os_error()).context(
                    "native directory cutover failed; inspect installation status before recovery",
                );
            }
        }
        #[cfg(not(windows))]
        {
            anyhow::bail!("native upgrades require Windows");
        }
        let root = self
            .roots
            .iter_mut()
            .find(|root| root.path == from)
            .unwrap();
        root.path = to.to_path_buf();
        let roots = std::mem::take(&mut self.roots);
        for root in roots {
            self.add(&root.path, &root.expected)?;
            ensure!(
                self.roots.last().unwrap().entries == root.entries,
                "installation identity changed across directory cutover"
            );
        }
        self.check(to)
    }
}
pub(super) fn descriptor(prefix: &Path) -> Result<Value> {
    inventory(prefix)?;
    let verified = super::super::verify(prefix)?;
    Ok(
        json!({"version":verified["version"],"binary_sha256":verified["binary_sha256"],
        "manifest_sha256":super::super::file_digest(&prefix.join("NATIVE_INSTALLATION.json"))?}),
    )
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;
    #[test]
    fn writable_mapping_excludes_launch_but_allows_directory_rename_without_changing_bytes() {
        use std::os::windows::process::CommandExt;
        let temp = tempfile::tempdir().unwrap();
        let parent = temp.path().canonicalize().unwrap();
        let from = parent.join("from");
        let to = parent.join("to");
        fs::create_dir(&from).unwrap();
        let program = from.join("fixture.exe");
        fs::copy(std::env::current_exe().unwrap(), &program).unwrap();
        let before = super::super::super::file_digest(&program).unwrap();
        let file = exclusive(&program).unwrap();
        let mapping = ImageMapping::new(&file).unwrap();
        drop(file);
        let launch = |path: &Path| {
            std::process::Command::new(path)
                .arg("--list")
                .creation_flags(0x08000000)
                .output()
        };
        assert_eq!(launch(&program).unwrap_err().raw_os_error(), Some(32));
        assert_eq!(from.parent(), Some(parent.as_path()));
        assert_eq!(to.parent(), Some(parent.as_path()));
        fs::rename(&from, &to).unwrap();
        let program = to.join("fixture.exe");
        assert_eq!(launch(&program).unwrap_err().raw_os_error(), Some(32));
        assert_eq!(super::super::super::file_digest(&program).unwrap(), before);
        drop(mapping);
        assert!(launch(&program).unwrap().status.success());
    }
}
