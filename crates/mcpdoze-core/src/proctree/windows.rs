// SPDX-License-Identifier: GPL-3.0-or-later

//! Windows backend: one Job Object per tree.

use std::ffi::{OsStr, OsString};
use std::io;
use std::mem::{offset_of, size_of};
use std::path::{Path, PathBuf};

use tokio::process::{Child, Command};
use windows_sys::Win32::Foundation::{
    CloseHandle, GetLastError, ERROR_MORE_DATA, HANDLE, INVALID_HANDLE_VALUE,
};
use windows_sys::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, Thread32First, Thread32Next, TH32CS_SNAPTHREAD, THREADENTRY32,
};
use windows_sys::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JobObjectBasicAccountingInformation,
    JobObjectBasicProcessIdList, JobObjectExtendedLimitInformation, QueryInformationJobObject,
    SetInformationJobObject, TerminateJobObject, JOBOBJECT_BASIC_ACCOUNTING_INFORMATION,
    JOBOBJECT_BASIC_PROCESS_ID_LIST, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
    JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
};
use windows_sys::Win32::System::ProcessStatus::{
    K32GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS, PROCESS_MEMORY_COUNTERS_EX,
};
use windows_sys::Win32::System::Threading::{
    OpenProcess, OpenThread, ResumeThread, CREATE_NO_WINDOW, CREATE_SUSPENDED,
    PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_VM_READ, THREAD_SUSPEND_RESUME,
};

use super::{batch_command_line, ResolveError, TreeMemory};

/// Upper bound on pids read from a job, so a fork bomb cannot make
/// `memory()` allocate without limit.
const MAX_PIDS: usize = 4096;

/// Exit code given to processes killed via the job.
const KILLED_EXIT_CODE: u32 = 1;

/// Owned handle that is closed on drop.
#[derive(Debug)]
struct OwnedHandle(HANDLE);

impl Drop for OwnedHandle {
    fn drop(&mut self) {
        // SAFETY: the handle was returned valid by a Win32 call, is owned
        // exclusively by this wrapper and is closed exactly once here.
        unsafe {
            CloseHandle(self.0);
        }
    }
}

/// A Job Object holding the whole tree.
#[derive(Debug)]
pub(super) struct Tree {
    job: OwnedHandle,
}

// SAFETY: a job handle is a process-wide kernel handle; every Win32 job API
// used here is thread-safe, and the handle is only closed in Drop.
unsafe impl Send for Tree {}
// SAFETY: see `Send`; all methods take `&self` and only pass the handle to
// thread-safe kernel calls.
unsafe impl Sync for Tree {}

/// Sets the creation flags that keep the root suspended until it is in the
/// job, and without a console window.
pub(super) fn prepare(cmd: &mut Command) {
    cmd.creation_flags(CREATE_SUSPENDED | CREATE_NO_WINDOW);
}

impl Tree {
    /// Creates an anonymous job that kills its processes when closed.
    pub(super) fn new() -> io::Result<Self> {
        // SAFETY: null security attributes and a null name are documented as
        // valid; the returned handle is checked before use.
        let raw = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
        if raw.is_null() {
            return Err(io::Error::last_os_error());
        }
        let job = OwnedHandle(raw);
        let mut info = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
        info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        // SAFETY: `info` is a properly initialised struct of the class's type
        // and the length passed is its exact size.
        let ok = unsafe {
            SetInformationJobObject(
                job.0,
                JobObjectExtendedLimitInformation,
                (&info as *const JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast(),
                size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            )
        };
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Self { job })
    }

    /// Puts the suspended root into the job, then resumes it.
    pub(super) fn adopt(&self, child: &Child, pid: u32) -> io::Result<()> {
        let Some(process) = child.raw_handle() else {
            return Err(io::Error::other("child has no process handle"));
        };
        // SAFETY: both handles are valid: the job is owned by `self`, the
        // process handle is owned by `child`, which outlives this call.
        let ok = unsafe { AssignProcessToJobObject(self.job.0, process as HANDLE) };
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }
        resume_threads(pid)
    }

    /// Windows has no polite tree-wide signal; stdin close is the request.
    pub(super) fn soft_stop(&self) {}

    /// Terminates every process in the job.
    pub(super) fn kill(&self) {
        // SAFETY: the job handle is valid for the lifetime of `self`.
        unsafe {
            TerminateJobObject(self.job.0, KILLED_EXIT_CODE);
        }
    }

    /// True while the job has any active process. Errs on the side of
    /// "alive" if the job cannot be queried.
    pub(super) fn is_alive(&self) -> bool {
        let mut info = JOBOBJECT_BASIC_ACCOUNTING_INFORMATION::default();
        // SAFETY: `info` is a writable struct of the class's type and the
        // length passed is its exact size; the return length may be null.
        let ok = unsafe {
            QueryInformationJobObject(
                self.job.0,
                JobObjectBasicAccountingInformation,
                (&mut info as *mut JOBOBJECT_BASIC_ACCOUNTING_INFORMATION).cast(),
                size_of::<JOBOBJECT_BASIC_ACCOUNTING_INFORMATION>() as u32,
                std::ptr::null_mut(),
            )
        };
        ok == 0 || info.ActiveProcesses > 0
    }

    /// Sums working set and private bytes over the job's processes.
    pub(super) fn memory(&self) -> TreeMemory {
        let pids = self.pids();
        let mut mem = TreeMemory {
            process_count: pids.len(),
            ..TreeMemory::default()
        };
        for pid in pids {
            if let Some((rss, private)) = process_memory(pid) {
                mem.rss_bytes += rss;
                mem.private_bytes += private;
            }
        }
        mem
    }

    /// Pids currently in the job (at most `MAX_PIDS`).
    pub(super) fn pids(&self) -> Vec<u32> {
        let header =
            offset_of!(JOBOBJECT_BASIC_PROCESS_ID_LIST, ProcessIdList) / size_of::<usize>();
        let mut cap = 64usize;
        loop {
            let mut buf = vec![0usize; header + cap];
            // SAFETY: `buf` is usize-aligned (the struct's alignment) and
            // large enough for the header plus `cap` ids; its exact byte
            // length is passed.
            let ok = unsafe {
                QueryInformationJobObject(
                    self.job.0,
                    JobObjectBasicProcessIdList,
                    buf.as_mut_ptr().cast(),
                    (buf.len() * size_of::<usize>()) as u32,
                    std::ptr::null_mut(),
                )
            };
            // SAFETY: called right after the failing API on the same thread.
            let more = ok == 0 && unsafe { GetLastError() } == ERROR_MORE_DATA;
            if ok == 0 && !more {
                return Vec::new();
            }
            // SAFETY: `buf` starts with a JOBOBJECT_BASIC_PROCESS_ID_LIST
            // header written by the kernel and is suitably aligned.
            let list = unsafe { &*buf.as_ptr().cast::<JOBOBJECT_BASIC_PROCESS_ID_LIST>() };
            let assigned = list.NumberOfAssignedProcesses as usize;
            let n = (list.NumberOfProcessIdsInList as usize).min(cap);
            if !more || cap >= MAX_PIDS {
                return buf[header..header + n].iter().map(|&p| p as u32).collect();
            }
            cap = (assigned + 16).max(cap * 2).min(MAX_PIDS);
        }
    }
}

/// Resumes every thread of a freshly created, suspended process.
fn resume_threads(pid: u32) -> io::Result<()> {
    // SAFETY: TH32CS_SNAPTHREAD with process id 0 is a documented call; the
    // handle is checked and owned by `OwnedHandle`.
    let snap = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0) };
    if snap == INVALID_HANDLE_VALUE {
        return Err(io::Error::last_os_error());
    }
    let snap = OwnedHandle(snap);
    let mut entry = THREADENTRY32 {
        dwSize: size_of::<THREADENTRY32>() as u32,
        ..THREADENTRY32::default()
    };
    let mut resumed = 0usize;
    // SAFETY: `entry` is writable and `dwSize` is initialised as required.
    let mut more = unsafe { Thread32First(snap.0, &mut entry) } != 0;
    while more {
        if entry.th32OwnerProcessID == pid {
            // SAFETY: plain open of a thread id from the snapshot; the
            // result is checked before use.
            let thread = unsafe { OpenThread(THREAD_SUSPEND_RESUME, 0, entry.th32ThreadID) };
            if !thread.is_null() {
                let thread = OwnedHandle(thread);
                // SAFETY: `thread` is a valid handle with SUSPEND_RESUME access.
                if unsafe { ResumeThread(thread.0) } != u32::MAX {
                    resumed += 1;
                }
            }
        }
        // SAFETY: same snapshot and entry as above.
        more = unsafe { Thread32Next(snap.0, &mut entry) } != 0;
    }
    if resumed == 0 {
        return Err(io::Error::other("could not resume the spawned process"));
    }
    Ok(())
}

/// Memory of the calling process alone.
pub(super) fn self_memory() -> TreeMemory {
    match process_memory(std::process::id()) {
        Some((rss, private)) => TreeMemory {
            rss_bytes: rss,
            private_bytes: private,
            process_count: 1,
        },
        None => TreeMemory::default(),
    }
}

/// Working set and private bytes of one process, if it can be opened.
fn process_memory(pid: u32) -> Option<(u64, u64)> {
    // SAFETY: plain open by pid; the result is checked before use.
    let h = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_VM_READ, 0, pid) };
    if h.is_null() {
        return None;
    }
    let h = OwnedHandle(h);
    let mut pmc = PROCESS_MEMORY_COUNTERS_EX {
        cb: size_of::<PROCESS_MEMORY_COUNTERS_EX>() as u32,
        ..PROCESS_MEMORY_COUNTERS_EX::default()
    };
    // SAFETY: the EX struct is a documented extension of
    // PROCESS_MEMORY_COUNTERS; `cb` tells the API which layout it gets.
    let ok = unsafe {
        K32GetProcessMemoryInfo(
            h.0,
            (&mut pmc as *mut PROCESS_MEMORY_COUNTERS_EX).cast::<PROCESS_MEMORY_COUNTERS>(),
            pmc.cb,
        )
    };
    (ok != 0).then_some((pmc.WorkingSetSize as u64, pmc.PrivateUsage as u64))
}

/// `%SystemRoot%\System32\cmd.exe`, avoiding a `PATH` lookup.
fn cmd_exe() -> PathBuf {
    if let Some(root) = std::env::var_os("SystemRoot") {
        let p = Path::new(&root).join("System32").join("cmd.exe");
        if p.is_file() {
            return p;
        }
    }
    PathBuf::from("cmd.exe")
}

/// Extensions from `PATHEXT`, e.g. `.EXE`.
fn path_exts() -> Vec<OsString> {
    let raw = std::env::var_os("PATHEXT").unwrap_or_else(|| ".COM;.EXE;.BAT;.CMD".into());
    raw.to_string_lossy()
        .split(';')
        .filter(|e| e.starts_with('.') && e.len() > 1)
        .map(OsString::from)
        .collect()
}

fn with_exts(base: &Path, exts: &[OsString]) -> Option<PathBuf> {
    if base.extension().is_some() && base.is_file() {
        return Some(base.to_path_buf());
    }
    exts.iter().find_map(|e| {
        let mut s = base.as_os_str().to_owned();
        s.push(e);
        let p = PathBuf::from(s);
        p.is_file().then_some(p)
    })
}

/// Finds `program` like the shell would, minus the current directory.
fn find_program(program: &OsStr) -> Option<PathBuf> {
    let exts = path_exts();
    let p = Path::new(program);
    let lossy = program.to_string_lossy();
    if p.is_absolute() || lossy.contains(['\\', '/']) {
        return with_exts(p, &exts);
    }
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .filter(|d| d.is_absolute())
        .find_map(|d| with_exts(&d.join(p), &exts))
}

pub(super) fn resolve_command(program: &OsStr, args: &[OsString]) -> Result<Command, ResolveError> {
    let path = find_program(program)
        .ok_or_else(|| ResolveError::NotFound(program.to_string_lossy().into_owned()))?;
    let is_batch = path
        .extension()
        .map(|e| e.eq_ignore_ascii_case("cmd") || e.eq_ignore_ascii_case("bat"))
        .unwrap_or(false);
    if !is_batch {
        let mut cmd = Command::new(path);
        cmd.args(args);
        return Ok(cmd);
    }
    let script = path.to_str().ok_or(ResolveError::NotUnicode)?;
    let args: Vec<&str> = args
        .iter()
        .map(|a| a.to_str().ok_or(ResolveError::NotUnicode))
        .collect::<Result<_, _>>()?;
    let line = batch_command_line(script, &args)?;
    let mut cmd = Command::new(cmd_exe());
    cmd.raw_arg("/d /s /c").raw_arg(line);
    Ok(cmd)
}
