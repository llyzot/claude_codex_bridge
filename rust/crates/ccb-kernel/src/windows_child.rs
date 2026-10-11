//! Atomic per-provider Windows Job membership with an explicit stdio handle list.
//! Only piped stdin/stdout and null stderr are used by the three native drivers.
use anyhow::{ensure, Context, Result};
use std::{
    cmp::Ordering,
    ffi::{OsStr, OsString},
    fs::File,
    io,
    mem::{size_of, zeroed},
    os::windows::{
        ffi::{OsStrExt, OsStringExt},
        io::{AsRawHandle, FromRawHandle, OwnedHandle},
        process::ExitStatusExt,
    },
    path::{Path, PathBuf},
    process::{Command, ExitStatus},
    ptr,
};
use windows_sys::Win32::{
    Foundation::*,
    Globalization::*,
    Security::SECURITY_ATTRIBUTES,
    Storage::FileSystem::*,
    System::{
        Environment::*, JobObjects::*, Pipes::CreatePipe, SystemInformation::*, Threading::*,
    },
};

fn check(value: i32) -> io::Result<()> {
    if value == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}
fn wide(value: &OsStr) -> Result<Vec<u16>> {
    let mut out: Vec<_> = value.encode_wide().collect();
    ensure!(
        !out.contains(&0),
        "native process strings cannot contain NUL"
    );
    out.push(0);
    Ok(out)
}
fn compare(a: &[u16], b: &[u16]) -> Ordering {
    let value =
        unsafe { CompareStringOrdinal(a.as_ptr(), a.len() as i32, b.as_ptr(), b.len() as i32, 1) };
    match value {
        CSTR_LESS_THAN => Ordering::Less,
        CSTR_GREATER_THAN => Ordering::Greater,
        _ => Ordering::Equal,
    }
}
fn env(command: &Command) -> Result<Vec<u16>> {
    let mut rows = Vec::<(Vec<u16>, Vec<u16>)>::new();
    // Keep native drive-current-directory entries and unpaired UTF-16 exactly.
    unsafe {
        let raw = GetEnvironmentStringsW();
        ensure!(!raw.is_null(), "native inherited environment unavailable");
        let mut offset = 0;
        while *raw.add(offset) != 0 {
            let start = offset;
            while *raw.add(offset) != 0 {
                offset += 1;
            }
            let entry = std::slice::from_raw_parts(raw.add(start), offset - start);
            if let Some(at) = entry
                .iter()
                .enumerate()
                .skip(1)
                .find_map(|(i, c)| (*c == b'=' as u16).then_some(i))
            {
                rows.push((entry[..at].to_vec(), entry[at + 1..].to_vec()));
            }
            offset += 1;
        }
        FreeEnvironmentStringsW(raw);
    }
    for (key, value) in command.get_envs() {
        let key = wide(key)?;
        let key = key[..key.len() - 1].to_vec();
        ensure!(
            !key.is_empty() && !key.contains(&(b'=' as u16)),
            "invalid native environment name"
        );
        rows.retain(|(existing, _)| compare(existing, &key) != Ordering::Equal);
        if let Some(value) = value {
            let value = wide(value)?;
            rows.push((key, value[..value.len() - 1].to_vec()));
        }
    }
    rows.sort_by(|a, b| compare(&a.0, &b.0));
    let mut out = Vec::new();
    for (key, value) in rows {
        out.extend(key);
        out.push(b'=' as u16);
        out.extend(value);
        out.push(0);
    }
    if out.is_empty() {
        out.push(0);
    }
    out.push(0);
    Ok(out)
}
fn system_directory(system: bool) -> Option<PathBuf> {
    let mut buffer = vec![0u16; 32768];
    let count = unsafe {
        if system {
            GetSystemDirectoryW(buffer.as_mut_ptr(), buffer.len() as u32)
        } else {
            GetWindowsDirectoryW(buffer.as_mut_ptr(), buffer.len() as u32)
        }
    } as usize;
    (count > 0 && count < buffer.len())
        .then(|| PathBuf::from(OsString::from_wide(&buffer[..count])))
}
fn program(command: &Command) -> Result<PathBuf> {
    let supplied = command.get_program();
    let name = Path::new(supplied);
    ensure!(
        !supplied.is_empty()
            && name.file_name().is_some()
            && !supplied
                .encode_wide()
                .last()
                .is_some_and(|c| c == b'/' as u16 || c == b'\\' as u16),
        "native program path has no filename"
    );
    let explicit_path = name.is_absolute() || name.components().count() > 1;
    let has_exe = name
        .extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| e.eq_ignore_ascii_case("exe"));
    let mut exe = supplied.to_owned();
    exe.push(".exe");
    let suffixes = if explicit_path {
        if has_exe {
            vec![supplied.to_owned()]
        } else {
            vec![exe, supplied.to_owned()]
        }
    } else if supplied.encode_wide().any(|c| c == b'.' as u16) {
        vec![supplied.to_owned()]
    } else {
        vec![exe]
    };
    let mut candidates = Vec::new();
    if explicit_path {
        // Like std::process::Command, explicit paths are relative to the caller,
        // never silently rebound to the child's requested working directory.
        candidates.extend(suffixes.into_iter().map(PathBuf::from));
    } else {
        let mut directories = Vec::new();
        let path_key: Vec<_> = "PATH".encode_utf16().collect();
        for (key, value) in command.get_envs() {
            if compare(&key.encode_wide().collect::<Vec<_>>(), &path_key) == Ordering::Equal {
                if let Some(paths) = value {
                    directories
                        .extend(std::env::split_paths(paths).filter(|p| !p.as_os_str().is_empty()));
                }
            }
        }
        directories.push(
            std::env::current_exe()?
                .parent()
                .context("missing application directory")?
                .to_path_buf(),
        );
        directories.extend(system_directory(true));
        directories.extend(system_directory(false));
        if let Some(paths) = std::env::var_os("PATH") {
            directories.extend(std::env::split_paths(&paths).filter(|p| !p.as_os_str().is_empty()));
        }
        for directory in directories {
            for suffix in &suffixes {
                candidates.push(directory.join(suffix));
            }
        }
    }
    let path = candidates
        .into_iter()
        .find(|p| p.is_file())
        .ok_or_else(|| io::Error::from(io::ErrorKind::NotFound))?;
    ensure!(
        !["cmd", "bat"].contains(
            &path
                .extension()
                .and_then(|e| e.to_str())
                .unwrap_or("")
                .to_ascii_lowercase()
                .as_str()
        ),
        "native provider process requires an executable, not a batch script"
    );
    Ok(path.canonicalize()?)
}
fn quote(arg: &OsStr, out: &mut Vec<u16>) -> Result<()> {
    let arg = wide(arg)?;
    let arg = &arg[..arg.len() - 1];
    out.push(b'"' as u16);
    let mut slashes = 0;
    for &c in arg {
        if c == b'\\' as u16 {
            slashes += 1;
            continue;
        }
        if c == b'"' as u16 {
            out.extend(std::iter::repeat_n(b'\\' as u16, slashes * 2 + 1));
        } else {
            out.extend(std::iter::repeat_n(b'\\' as u16, slashes));
        }
        slashes = 0;
        out.push(c);
    }
    out.extend(std::iter::repeat_n(b'\\' as u16, slashes * 2));
    out.push(b'"' as u16);
    Ok(())
}
fn pipe() -> Result<(OwnedHandle, OwnedHandle)> {
    unsafe {
        let sa = SECURITY_ATTRIBUTES {
            nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: ptr::null_mut(),
            bInheritHandle: 1,
        };
        let (mut read, mut write) = (ptr::null_mut(), ptr::null_mut());
        check(CreatePipe(&mut read, &mut write, &sa, 0))?;
        Ok((
            OwnedHandle::from_raw_handle(read),
            OwnedHandle::from_raw_handle(write),
        ))
    }
}
struct Attributes {
    storage: Vec<usize>,
    initialized: bool,
}
impl Attributes {
    fn new() -> Result<Self> {
        unsafe {
            let mut bytes = 0;
            InitializeProcThreadAttributeList(ptr::null_mut(), 2, 0, &mut bytes);
            ensure!(bytes > 0, "native process attribute size unavailable");
            let mut this = Self {
                storage: vec![0; bytes.div_ceil(size_of::<usize>())],
                initialized: false,
            };
            check(InitializeProcThreadAttributeList(
                this.ptr(),
                2,
                0,
                &mut bytes,
            ))?;
            this.initialized = true;
            Ok(this)
        }
    }
    fn ptr(&mut self) -> LPPROC_THREAD_ATTRIBUTE_LIST {
        self.storage.as_mut_ptr().cast()
    }
}
impl Drop for Attributes {
    fn drop(&mut self) {
        unsafe {
            if self.initialized {
                DeleteProcThreadAttributeList(self.ptr());
            }
        }
    }
}

pub(crate) struct OwnedChild {
    pub stdin: Option<File>,
    pub stdout: Option<File>,
    process: OwnedHandle,
    job: OwnedHandle,
    pid: u32,
    scope: String,
    status: Option<ExitStatus>,
}
impl OwnedChild {
    pub fn spawn(command: &mut Command) -> Result<Self> {
        let scope = uuid::Uuid::new_v4().to_string();
        let environment = env(command)?;
        let application = program(command)?;
        let application = wide(application.as_os_str())?;
        let mut line = Vec::new();
        quote(command.get_program(), &mut line)?;
        for arg in command.get_args() {
            line.push(b' ' as u16);
            quote(arg, &mut line)?;
        }
        line.push(0);
        ensure!(line.len() <= 32767, "native process command line too long");
        let cwd = command
            .get_current_dir()
            .map(|v| wide(v.as_os_str()))
            .transpose()?;
        let (input_read, input_write) = pipe()?;
        let (output_read, output_write) = pipe()?;
        unsafe {
            check(SetHandleInformation(
                input_write.as_raw_handle(),
                HANDLE_FLAG_INHERIT,
                0,
            ))?;
            check(SetHandleInformation(
                output_read.as_raw_handle(),
                HANDLE_FLAG_INHERIT,
                0,
            ))?;
            let raw_job = CreateJobObjectW(ptr::null(), ptr::null());
            ensure!(!raw_job.is_null(), "native provider Job unavailable");
            let job = OwnedHandle::from_raw_handle(raw_job);
            let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = zeroed();
            limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            check(SetInformationJobObject(
                raw_job,
                JobObjectExtendedLimitInformation,
                (&limits as *const JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast(),
                size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            ))?;
            let sa = SECURITY_ATTRIBUTES {
                nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
                lpSecurityDescriptor: ptr::null_mut(),
                bInheritHandle: 1,
            };
            let null_name: Vec<_> = "NUL\0".encode_utf16().collect();
            let null = CreateFileW(
                null_name.as_ptr(),
                GENERIC_WRITE,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                &sa,
                OPEN_EXISTING,
                FILE_ATTRIBUTE_NORMAL,
                ptr::null_mut(),
            );
            if null == INVALID_HANDLE_VALUE {
                return Err(io::Error::last_os_error().into());
            }
            let null = OwnedHandle::from_raw_handle(null);
            let handles = [
                input_read.as_raw_handle(),
                output_write.as_raw_handle(),
                null.as_raw_handle(),
            ];
            let mut attributes = Attributes::new()?;
            check(UpdateProcThreadAttribute(
                attributes.ptr(),
                0,
                PROC_THREAD_ATTRIBUTE_HANDLE_LIST as usize,
                handles.as_ptr().cast(),
                size_of_val(&handles),
                ptr::null_mut(),
                ptr::null(),
            ))?;
            let jobs = [raw_job];
            check(UpdateProcThreadAttribute(
                attributes.ptr(),
                0,
                PROC_THREAD_ATTRIBUTE_JOB_LIST as usize,
                jobs.as_ptr().cast(),
                size_of_val(&jobs),
                ptr::null_mut(),
                ptr::null(),
            ))?;
            let mut startup: STARTUPINFOEXW = zeroed();
            startup.StartupInfo.cb = size_of::<STARTUPINFOEXW>() as u32;
            startup.StartupInfo.dwFlags = STARTF_USESTDHANDLES;
            startup.StartupInfo.hStdInput = handles[0];
            startup.StartupInfo.hStdOutput = handles[1];
            startup.StartupInfo.hStdError = handles[2];
            startup.lpAttributeList = attributes.ptr();
            let mut info: PROCESS_INFORMATION = zeroed();
            check(CreateProcessW(
                application.as_ptr(),
                line.as_mut_ptr(),
                ptr::null(),
                ptr::null(),
                1,
                CREATE_NO_WINDOW | CREATE_UNICODE_ENVIRONMENT | EXTENDED_STARTUPINFO_PRESENT,
                environment.as_ptr().cast(),
                cwd.as_ref().map(|v| v.as_ptr()).unwrap_or(ptr::null()),
                &startup.StartupInfo,
                &mut info,
            ))?;
            let process = OwnedHandle::from_raw_handle(info.hProcess);
            let thread = OwnedHandle::from_raw_handle(info.hThread);
            drop(thread);
            Ok(Self {
                stdin: Some(File::from(input_write)),
                stdout: Some(File::from(output_read)),
                process,
                job,
                pid: info.dwProcessId,
                scope,
                status: None,
            })
        }
    }
    pub fn id(&self) -> u32 {
        self.pid
    }
    pub fn scope_id(&self) -> &str {
        &self.scope
    }
    pub fn scope_info(&self) -> Result<serde_json::Value> {
        Ok(
            serde_json::json!({"supported":true,"scope_id":self.scope_id(),"active_processes":self.active_processes()?}),
        )
    }
    pub fn tree_empty(&self) -> io::Result<bool> {
        Ok(self.active_processes()? == 0)
    }
    pub fn active_processes(&self) -> io::Result<u32> {
        unsafe {
            let mut row: JOBOBJECT_BASIC_ACCOUNTING_INFORMATION = zeroed();
            check(QueryInformationJobObject(
                self.job.as_raw_handle(),
                JobObjectBasicAccountingInformation,
                (&mut row as *mut JOBOBJECT_BASIC_ACCOUNTING_INFORMATION).cast(),
                size_of::<JOBOBJECT_BASIC_ACCOUNTING_INFORMATION>() as u32,
                ptr::null_mut(),
            ))?;
            Ok(row.ActiveProcesses)
        }
    }
    pub fn try_wait(&mut self) -> io::Result<Option<ExitStatus>> {
        if self.status.is_none() {
            unsafe {
                match WaitForSingleObject(self.process.as_raw_handle(), 0) {
                    WAIT_OBJECT_0 => {
                        let mut code = 0;
                        check(GetExitCodeProcess(self.process.as_raw_handle(), &mut code))?;
                        self.status = Some(ExitStatus::from_raw(code));
                    }
                    WAIT_TIMEOUT => {}
                    _ => return Err(io::Error::last_os_error()),
                }
            }
        }
        Ok(self.status)
    }
    pub fn kill(&mut self) -> io::Result<()> {
        unsafe { check(TerminateJobObject(self.job.as_raw_handle(), 1)) }
    }
    pub fn wait(&mut self) -> io::Result<ExitStatus> {
        unsafe {
            if WaitForSingleObject(self.process.as_raw_handle(), INFINITE) != WAIT_OBJECT_0 {
                return Err(io::Error::last_os_error());
            }
        }
        self.try_wait()?
            .ok_or_else(|| io::Error::other("owned process wait returned without exit"))
    }
}
impl Drop for OwnedChild {
    fn drop(&mut self) {
        let _ = self.kill();
        let _ = self.wait();
    }
}
