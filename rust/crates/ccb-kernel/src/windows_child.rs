//! Atomic per-provider Windows Job membership with an explicit stdio handle list.
//! Provider drivers use null stderr; bounded CLI transport can capture it.
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
        Environment::*, JobObjects::*, Pipes::CreatePipe, RemoteDesktop::ProcessIdToSessionId,
        SystemInformation::*, SystemServices::JOB_OBJECT_QUERY, Threading::*,
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
    pub stderr: Option<File>,
    process: OwnedHandle,
    job: OwnedHandle,
    pid: u32,
    scope: String,
    recovery: serde_json::Value,
    status: Option<ExitStatus>,
}
impl OwnedChild {
    pub fn spawn(command: &mut Command) -> Result<Self> {
        Self::spawn_with_stderr(command, false)
    }
    pub fn capture(command: &mut Command) -> Result<Self> {
        Self::spawn_with_stderr(command, true)
    }
    fn spawn_with_stderr(command: &mut Command, capture_stderr: bool) -> Result<Self> {
        let scope = uuid::Uuid::new_v4().to_string();
        let owner_created = creation_time(unsafe { GetCurrentProcess() })?;
        let namespace = current_session()?;
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
        let (error_read, error_write) = if capture_stderr {
            let (read, write) = pipe()?;
            (Some(read), Some(write))
        } else {
            (None, None)
        };
        unsafe {
            if let Some(read) = &error_read {
                check(SetHandleInformation(
                    read.as_raw_handle(),
                    HANDLE_FLAG_INHERIT,
                    0,
                ))?;
            }
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
            let job_name = scope_name(&scope);
            let job_name_wide = wide(OsStr::new(&job_name))?;
            SetLastError(0);
            let raw_job = CreateJobObjectW(ptr::null(), job_name_wide.as_ptr());
            let job_error = GetLastError();
            ensure!(!raw_job.is_null(), "native provider Job unavailable");
            let job = OwnedHandle::from_raw_handle(raw_job);
            ensure!(
                job_error != ERROR_ALREADY_EXISTS,
                "native provider Job identity collision"
            );
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
                error_write
                    .as_ref()
                    .map(|h| h.as_raw_handle())
                    .unwrap_or_else(|| null.as_raw_handle()),
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
            let recovery = serde_json::json!({"kind":"windows_job","scope_id":scope,"scope_name":job_name,
                "owner_pid":std::process::id(),"owner_created":owner_created,"windows_session":namespace,
                "provider_pid":info.dwProcessId,"provider_created":creation_time(process.as_raw_handle())?,
                "kill_on_owner_exit":true});
            Ok(Self {
                stdin: Some(File::from(input_write)),
                stdout: Some(File::from(output_read)),
                stderr: error_read.map(File::from),
                process,
                job,
                pid: info.dwProcessId,
                scope,
                recovery,
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
            serde_json::json!({"supported":true,"scope_id":self.scope_id(),"active_processes":self.active_processes()?,"recovery":self.recovery}),
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

fn creation_time(handle: HANDLE) -> io::Result<String> {
    unsafe {
        let (mut created, mut ended, mut kernel, mut user): (
            FILETIME,
            FILETIME,
            FILETIME,
            FILETIME,
        ) = zeroed();
        check(GetProcessTimes(
            handle,
            &mut created,
            &mut ended,
            &mut kernel,
            &mut user,
        ))?;
        Ok((((created.dwHighDateTime as u64) << 32) | created.dwLowDateTime as u64).to_string())
    }
}
fn current_session() -> io::Result<u32> {
    let mut id = 0;
    unsafe {
        check(ProcessIdToSessionId(std::process::id(), &mut id))?;
    }
    Ok(id)
}
/// Observe an external process without adopting it or acquiring termination rights.
pub(crate) fn observe_external_process(pid: u32) -> anyhow::Result<serde_json::Value> {
    anyhow::ensure!(pid > 0, "invalid external process identity");
    unsafe {
        let raw = OpenProcess(
            PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SYNCHRONIZE,
            0,
            pid,
        );
        if raw.is_null() {
            return Err(io::Error::last_os_error().into());
        }
        let handle = OwnedHandle::from_raw_handle(raw);
        anyhow::ensure!(
            WaitForSingleObject(handle.as_raw_handle(), 0) == WAIT_TIMEOUT,
            "external process is no longer running"
        );
        let mut session = 0;
        check(ProcessIdToSessionId(pid, &mut session))?;
        Ok(
            serde_json::json!({"pid":pid,"created":creation_time(handle.as_raw_handle())?,"windows_session":session}),
        )
    }
}
fn scope_name(scope: &str) -> String {
    format!("Local\\CCB.Native.Provider.{scope}")
}
fn process_gone(pid: u32, created: &str) -> io::Result<bool> {
    unsafe {
        let raw = OpenProcess(
            PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SYNCHRONIZE,
            0,
            pid,
        );
        if raw.is_null() {
            let error = io::Error::last_os_error();
            return if error.raw_os_error() == Some(ERROR_INVALID_PARAMETER as i32) {
                Ok(true)
            } else {
                Err(error)
            };
        }
        let handle = OwnedHandle::from_raw_handle(raw);
        if creation_time(handle.as_raw_handle())? != created {
            return Ok(true);
        }
        match WaitForSingleObject(handle.as_raw_handle(), 0) {
            WAIT_OBJECT_0 => Ok(true),
            WAIT_TIMEOUT => Ok(false),
            _ => Err(io::Error::last_os_error()),
        }
    }
}
pub(crate) fn probe_recovery(proof: &serde_json::Value) -> serde_json::Value {
    use serde_json::json;
    let pending = |reason| json!({"status":"pending","reason":reason});
    if current_session().ok().map(u64::from) != proof["windows_session"].as_u64() {
        return pending("windows_namespace_unverified");
    }
    for (prefix, alive, unknown) in [
        ("owner", "owner_alive", "owner_unverified"),
        ("provider", "provider_alive", "provider_unverified"),
    ] {
        let pid = proof[format!("{prefix}_pid")].as_u64().unwrap_or(0) as u32;
        let key = format!("{prefix}_created");
        match process_gone(pid, proof[&key].as_str().unwrap_or("")) {
            Ok(true) => {}
            Ok(false) => return pending(alive),
            Err(_) => return pending(unknown),
        }
    }
    let name = scope_name(proof["scope_id"].as_str().unwrap_or(""));
    let Ok(name) = wide(OsStr::new(&name)) else {
        return pending("scope_unverified");
    };
    unsafe {
        let raw = OpenJobObjectW(JOB_OBJECT_QUERY, 0, name.as_ptr());
        if raw.is_null() {
            return if GetLastError() == ERROR_FILE_NOT_FOUND {
                json!({"status":"stopped","reason":"owned_scope_destroyed"})
            } else {
                pending("scope_unverified")
            };
        }
        let handle = OwnedHandle::from_raw_handle(raw);
        let mut info: JOBOBJECT_BASIC_ACCOUNTING_INFORMATION = zeroed();
        if QueryInformationJobObject(
            handle.as_raw_handle(),
            JobObjectBasicAccountingInformation,
            (&mut info as *mut JOBOBJECT_BASIC_ACCOUNTING_INFORMATION).cast(),
            size_of::<JOBOBJECT_BASIC_ACCOUNTING_INFORMATION>() as u32,
            ptr::null_mut(),
        ) == 0
        {
            return pending("scope_unverified");
        }
        if info.ActiveProcesses == 0 {
            json!({"status":"stopped","reason":"owned_scope_empty"})
        } else {
            pending("scope_has_live_processes")
        }
    }
}
