//! Directory references only: unlinking these never traverses the source tree.
use anyhow::{ensure, Result};
use std::{fs, path::Path};

pub fn is_link(path: &Path) -> Result<bool> {
    let meta = fs::symlink_metadata(path)?;
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        Ok(meta.file_attributes() & 0x400 != 0)
    }
    #[cfg(not(windows))]
    {
        Ok(meta.file_type().is_symlink())
    }
}
pub fn verify(path: &Path, target: &Path) -> Result<()> {
    ensure!(is_link(path)?, "live directory reference was replaced");
    let observed = fs::read_link(path)?;
    #[cfg(windows)]
    let matches = match (observed.to_str(), target.to_str()) {
        (Some(a), Some(b)) => {
            a.strip_prefix(r"\\?\").unwrap_or(a) == b.strip_prefix(r"\\?\").unwrap_or(b)
        }
        _ => false,
    };
    #[cfg(not(windows))]
    let matches = observed == target;
    ensure!(
        matches,
        "live directory target mismatch: {observed:?} != {target:?}"
    );
    Ok(())
}
pub fn remove(path: &Path, target: &Path) -> Result<()> {
    verify(path, target)?;
    #[cfg(windows)]
    fs::remove_dir(path)?;
    #[cfg(not(windows))]
    fs::remove_file(path)?;
    Ok(())
}
#[cfg(not(windows))]
pub fn create(path: &Path, target: &Path) -> Result<()> {
    std::os::unix::fs::symlink(target, path)?;
    Ok(())
}
#[cfg(windows)]
pub fn create(path: &Path, target: &Path) -> Result<()> {
    use std::{
        fs::OpenOptions,
        os::windows::{fs::OpenOptionsExt, io::AsRawHandle},
    };
    use windows_sys::Win32::{
        Storage::FileSystem::{FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT},
        System::IO::DeviceIoControl,
    };
    let target = target
        .to_str()
        .ok_or_else(|| anyhow::anyhow!("non-UTF8 role target"))?;
    let dos = target.strip_prefix(r"\\?\").unwrap_or(target);
    ensure!(
        dos.as_bytes().get(1) == Some(&b':'),
        "directory junction requires a local absolute target"
    );
    let substitute: Vec<u16> = format!(r"\??\{dos}").encode_utf16().collect();
    let print: Vec<u16> = dos.encode_utf16().collect();
    let length = 8 + (substitute.len() + print.len() + 2) * 2;
    ensure!(length < 16376, "junction target exceeds reparse buffer");
    let mut data = Vec::new();
    data.extend(0xA0000003u32.to_le_bytes());
    for value in [
        length as u16,
        0,
        0,
        (substitute.len() * 2) as u16,
        ((substitute.len() + 1) * 2) as u16,
        (print.len() * 2) as u16,
    ] {
        data.extend(value.to_le_bytes());
    }
    for value in substitute.into_iter().chain([0]).chain(print).chain([0]) {
        data.extend(value.to_le_bytes());
    }
    // The caller journals this exact temporary entry before creation.
    fs::create_dir(path)?;
    let file = OpenOptions::new()
        .write(true)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
        .open(path)?;
    let mut returned = 0;
    let ok = unsafe {
        DeviceIoControl(
            file.as_raw_handle(),
            0x000900A4,
            data.as_ptr().cast(),
            data.len() as u32,
            std::ptr::null_mut(),
            0,
            &mut returned,
            std::ptr::null_mut(),
        )
    };
    if ok == 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    Ok(())
}
pub fn publish(temporary: &Path, target: &Path) -> Result<()> {
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        let from: Vec<u16> = temporary.as_os_str().encode_wide().chain([0]).collect();
        let to: Vec<u16> = target.as_os_str().encode_wide().chain([0]).collect();
        let ok = unsafe {
            windows_sys::Win32::Storage::FileSystem::MoveFileExW(from.as_ptr(), to.as_ptr(), 0)
        };
        if ok == 0 {
            return Err(std::io::Error::last_os_error().into());
        }
    }
    #[cfg(not(windows))]
    {
        // A symlink create is already atomic and must never replace an entry.
        std::os::unix::fs::symlink(fs::read_link(temporary)?, target)?;
        fs::remove_file(temporary)?;
    }
    Ok(())
}
