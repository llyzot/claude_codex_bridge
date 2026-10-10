//! Native same-user token creation and process-lifetime ownership.
use anyhow::{ensure, Context, Result};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{fs::File, io::Write, path::Path};

pub fn create_token(path: &Path) -> Result<Value> {
    let parent = path.parent().context("token requires a parent directory")?;
    std::fs::create_dir_all(parent)?;
    ensure!(
        !std::fs::symlink_metadata(parent)?.file_type().is_symlink(),
        "linked token parent"
    );
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        ensure!(
            std::fs::symlink_metadata(parent)?.file_attributes() & 0x400 == 0,
            "reparse token parent"
        );
    }
    let mut random = [0u8; 32];
    random_bytes(&mut random)?;
    let secret = random
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect::<String>();
    let generation = uuid::Uuid::new_v4().simple().to_string();
    let payload =
        json!({"schema":"ccbd-control-plane-token-v1","generation":generation,"token":secret});
    let mut bytes = serde_json::to_vec(&payload)?;
    bytes.push(b'\n');
    let mut created = false;
    let result = write_private(path, &bytes, &mut created).and_then(|_| verify_token(path));
    if result.is_err() && created {
        let _ = std::fs::remove_file(path);
    }
    result?;
    Ok(
        json!({"token_ref":path,"generation":generation,"acl_status":if cfg!(windows) {"windows-native-user-read"} else {"posix-0600"},
        "fingerprint":format!("{:x}",Sha256::digest(secret.as_bytes()))[..16].to_owned()}),
    )
}

#[cfg(not(windows))]
fn random_bytes(bytes: &mut [u8]) -> Result<()> {
    use std::io::Read;
    File::open("/dev/urandom")?.read_exact(bytes)?;
    Ok(())
}
#[cfg(not(windows))]
fn write_private(path: &Path, bytes: &[u8], created: &mut bool) -> Result<()> {
    use std::os::unix::fs::OpenOptionsExt;
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)?;
    *created = true;
    file.write_all(bytes)?;
    file.sync_all()?;
    Ok(())
}
#[cfg(not(windows))]
pub fn verify_token(path: &Path) -> Result<()> {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    extern "C" {
        fn geteuid() -> u32;
    }
    let metadata = std::fs::symlink_metadata(path)?;
    ensure!(
        metadata.is_file() && !metadata.file_type().is_symlink(),
        "invalid token file type"
    );
    ensure!(
        metadata.permissions().mode() & 0o077 == 0,
        "token grants other users access"
    );
    ensure!(
        metadata.uid() == unsafe { geteuid() },
        "token owner mismatch"
    );
    Ok(())
}
#[cfg(not(windows))]
pub fn own_process_tree() -> Result<()> {
    Ok(())
} // Unix lifetime adapter remains explicit migration work.

#[cfg(windows)]
mod windows {
    use super::*;
    use std::{
        ffi::c_void,
        mem::size_of,
        os::windows::{
            ffi::OsStrExt,
            io::{AsRawHandle, FromRawHandle},
        },
        ptr,
    };
    use windows_sys::Win32::{
        Foundation::*, Security::Authorization::*, Security::Cryptography::*, Security::*,
        Storage::FileSystem::*, System::JobObjects::*, System::Threading::*,
    };
    struct Handle(HANDLE);
    impl Drop for Handle {
        fn drop(&mut self) {
            unsafe {
                CloseHandle(self.0);
            }
        }
    }
    struct Local(*mut c_void);
    impl Drop for Local {
        fn drop(&mut self) {
            unsafe {
                LocalFree(self.0);
            }
        }
    }
    fn checked(ok: i32) -> Result<()> {
        ensure!(
            ok != 0,
            "Windows security operation failed: {}",
            std::io::Error::last_os_error()
        );
        Ok(())
    }
    fn sid_string(sid: PSID) -> Result<String> {
        unsafe {
            let mut out = ptr::null_mut();
            checked(ConvertSidToStringSidW(sid, &mut out))?;
            let _free = Local(out.cast());
            let mut len = 0;
            while *out.add(len) != 0 {
                len += 1;
            }
            Ok(String::from_utf16(std::slice::from_raw_parts(out, len))?)
        }
    }
    fn current_sid() -> Result<String> {
        unsafe {
            let mut raw = ptr::null_mut();
            checked(OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut raw))?;
            let token = Handle(raw);
            let mut needed = 0;
            GetTokenInformation(token.0, TokenUser, ptr::null_mut(), 0, &mut needed);
            ensure!(needed > 0, "token user size unavailable");
            let mut buffer = vec![0usize; (needed as usize).div_ceil(size_of::<usize>())];
            checked(GetTokenInformation(
                token.0,
                TokenUser,
                buffer.as_mut_ptr().cast(),
                needed,
                &mut needed,
            ))?;
            sid_string((*(buffer.as_ptr().cast::<TOKEN_USER>())).User.Sid)
        }
    }
    fn descriptor(sddl: &str) -> Result<Local> {
        let wide = sddl.encode_utf16().chain([0]).collect::<Vec<_>>();
        unsafe {
            let mut sd = ptr::null_mut();
            checked(ConvertStringSecurityDescriptorToSecurityDescriptorW(
                wide.as_ptr(),
                1,
                &mut sd,
                ptr::null_mut(),
            ))?;
            Ok(Local(sd))
        }
    }
    pub(super) fn random_bytes(bytes: &mut [u8]) -> Result<()> {
        let status = unsafe {
            BCryptGenRandom(
                ptr::null_mut(),
                bytes.as_mut_ptr(),
                bytes.len() as u32,
                BCRYPT_USE_SYSTEM_PREFERRED_RNG,
            )
        };
        ensure!(status >= 0, "system random source failed");
        Ok(())
    }
    pub(super) fn write_private(path: &Path, bytes: &[u8], created: &mut bool) -> Result<()> {
        let sid = current_sid()?;
        let creation = descriptor(&format!("O:{sid}D:P(A;;FRFW;;;{sid})"))?;
        let wide = path
            .as_os_str()
            .encode_wide()
            .chain([0])
            .collect::<Vec<_>>();
        let attrs = SECURITY_ATTRIBUTES {
            nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: creation.0,
            bInheritHandle: 0,
        };
        let raw = unsafe {
            CreateFileW(
                wide.as_ptr(),
                GENERIC_READ | GENERIC_WRITE | WRITE_DAC,
                0,
                &attrs,
                CREATE_NEW,
                FILE_ATTRIBUTE_NORMAL,
                ptr::null_mut(),
            )
        };
        ensure!(
            raw != INVALID_HANDLE_VALUE,
            "cannot create private token: {}",
            std::io::Error::last_os_error()
        );
        *created = true;
        let mut file = unsafe { File::from_raw_handle(raw) };
        file.write_all(bytes)?;
        file.sync_all()?;
        let readonly = descriptor(&format!("O:{sid}D:P(A;;FR;;;{sid})"))?;
        let mut present = 0;
        let mut defaulted = 0;
        let mut dacl = ptr::null_mut();
        unsafe {
            checked(GetSecurityDescriptorDacl(
                readonly.0,
                &mut present,
                &mut dacl,
                &mut defaulted,
            ))?;
            ensure!(
                present != 0 && !dacl.is_null(),
                "private token DACL unavailable"
            );
            ensure!(
                SetSecurityInfo(
                    file.as_raw_handle(),
                    SE_FILE_OBJECT,
                    DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
                    ptr::null_mut(),
                    ptr::null_mut(),
                    dacl,
                    ptr::null_mut()
                ) == 0,
                "cannot protect token ACL"
            );
        }
        Ok(())
    }
    pub(super) fn verify_token(path: &Path) -> Result<()> {
        use std::os::windows::fs::MetadataExt;
        let metadata = std::fs::symlink_metadata(path)?;
        ensure!(
            metadata.is_file() && metadata.file_attributes() & 0x400 == 0,
            "invalid token file type"
        );
        let file = File::open(path)?;
        let mut owner = ptr::null_mut();
        let mut dacl = ptr::null_mut();
        let mut sd = ptr::null_mut();
        unsafe {
            ensure!(
                GetSecurityInfo(
                    file.as_raw_handle(),
                    SE_FILE_OBJECT,
                    OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION,
                    &mut owner,
                    ptr::null_mut(),
                    &mut dacl,
                    ptr::null_mut(),
                    &mut sd
                ) == 0,
                "cannot inspect token ACL"
            );
            let _free = Local(sd);
            let current = current_sid()?;
            ensure!(
                !owner.is_null() && sid_string(owner)? == current,
                "token owner mismatch"
            );
            let mut control = 0;
            let mut revision = 0;
            checked(GetSecurityDescriptorControl(
                sd,
                &mut control,
                &mut revision,
            ))?;
            ensure!(
                control & SE_DACL_PROTECTED != 0 && !dacl.is_null() && (*dacl).AceCount == 1,
                "token ACL must contain one protected same-user rule"
            );
            let mut raw = ptr::null_mut();
            checked(GetAce(dacl, 0, &mut raw))?;
            let ace = &*(raw.cast::<ACCESS_ALLOWED_ACE>());
            ensure!(
                ace.Header.AceType == 0 && ace.Header.AceFlags == 0,
                "invalid token ACE"
            );
            ensure!(
                ace.Mask & !FILE_GENERIC_READ == 0 && ace.Mask & FILE_READ_DATA != 0,
                "token grants excessive or insufficient access"
            );
            ensure!(
                sid_string((&ace.SidStart as *const u32).cast_mut().cast())? == current,
                "token ACE is not current user"
            );
        }
        Ok(())
    }
    pub(super) fn own_process_tree() -> Result<()> {
        unsafe {
            let job = CreateJobObjectW(ptr::null(), ptr::null());
            ensure!(!job.is_null(), "cannot create runtime job object");
            let guard = Handle(job);
            let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
            limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            checked(SetInformationJobObject(
                guard.0,
                JobObjectExtendedLimitInformation,
                (&limits as *const JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast(),
                size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            ))?;
            checked(AssignProcessToJobObject(guard.0, GetCurrentProcess()))?;
            // Retain this one handle until OS process teardown. Closing it in a
            // Rust destructor would terminate the daemon itself before cleanup.
            std::mem::forget(guard);
            Ok(())
        }
    }
}
#[cfg(windows)]
use windows::{random_bytes, write_private};
#[cfg(windows)]
pub fn verify_token(path: &Path) -> Result<()> {
    windows::verify_token(path)
}
#[cfg(windows)]
pub fn own_process_tree() -> Result<()> {
    windows::own_process_tree()
}
