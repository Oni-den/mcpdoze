// SPDX-License-Identifier: GPL-3.0-or-later

//! Unix backend: one session / process group per tree.

use std::ffi::{OsStr, OsString};
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};

use nix::errno::Errno;
use nix::sys::signal::{killpg, Signal};
use tokio::process::{Child, Command};

use super::{ResolveError, TreeMemory};

/// Process group of the tree (equal to the root pid after `setsid`).
#[derive(Debug)]
pub(super) struct Tree {
    pgid: AtomicI32,
    /// Set once `killpg` reports the group empty, so a later kill can never
    /// hit a recycled process group.
    gone: AtomicBool,
}

/// Makes the child a session and group leader; on Linux it also dies with
/// the broker.
pub(super) fn prepare(cmd: &mut Command) {
    #[cfg(target_os = "linux")]
    let parent = nix::unistd::getpid();
    // SAFETY: the closure runs in the forked child before exec. It only makes
    // async-signal-safe system calls (setsid, prctl, getppid), does not
    // allocate, and builds errors with `from_raw_os_error` (no allocation).
    unsafe {
        cmd.pre_exec(move || {
            nix::unistd::setsid().map_err(|e| io::Error::from_raw_os_error(e as i32))?;
            #[cfg(target_os = "linux")]
            {
                nix::sys::prctl::set_pdeathsig(Signal::SIGKILL)
                    .map_err(|e| io::Error::from_raw_os_error(e as i32))?;
                // The broker may have died between fork and prctl.
                if nix::unistd::getppid() != parent {
                    return Err(io::Error::from_raw_os_error(Errno::ESRCH as i32));
                }
            }
            Ok(())
        });
    }
}

impl Tree {
    pub(super) fn new() -> io::Result<Self> {
        Ok(Self {
            pgid: AtomicI32::new(0),
            gone: AtomicBool::new(false),
        })
    }

    pub(super) fn adopt(&self, _child: &Child, pid: u32) -> io::Result<()> {
        let pgid = i32::try_from(pid).map_err(|_| io::Error::other("pid out of range"))?;
        self.pgid.store(pgid, Ordering::SeqCst);
        Ok(())
    }

    /// Sends `sig` (or probes with `None`) to the group. Returns whether the
    /// group still exists.
    fn signal(&self, sig: Option<Signal>) -> bool {
        if self.gone.load(Ordering::SeqCst) {
            return false;
        }
        let pg = self.pgid.load(Ordering::SeqCst);
        if pg <= 1 {
            return false;
        }
        match killpg(nix::unistd::Pid::from_raw(pg), sig) {
            Ok(()) => true,
            Err(Errno::ESRCH) => {
                self.gone.store(true, Ordering::SeqCst);
                false
            }
            // EPERM and similar: the group exists but we may not signal it.
            Err(_) => true,
        }
    }

    pub(super) fn soft_stop(&self) {
        self.signal(Some(Signal::SIGTERM));
    }

    pub(super) fn kill(&self) {
        self.signal(Some(Signal::SIGKILL));
    }

    pub(super) fn is_alive(&self) -> bool {
        self.signal(None)
    }

    /// Pids of the live, non-zombie members of the session.
    pub(super) fn pids(&self) -> Vec<u32> {
        use sysinfo::{ProcessRefreshKind, ProcessStatus, ProcessesToUpdate, System};
        let pg = self.pgid.load(Ordering::SeqCst);
        if pg <= 1 || self.gone.load(Ordering::SeqCst) {
            return Vec::new();
        }
        let Ok(mut sys) = System::new() else {
            return Vec::new();
        };
        sys.refresh_processes_specifics(
            ProcessesToUpdate::All,
            true,
            ProcessRefreshKind::nothing(),
        );
        let sid = sysinfo::Pid::from_u32(pg as u32);
        let mut out: Vec<u32> = sys
            .processes()
            .iter()
            .filter(|(_, p)| {
                p.session_id() == Some(sid)
                    && p.status() != ProcessStatus::Zombie
                    && p.thread_kind().is_none()
            })
            .map(|(pid, _)| pid.as_u32())
            .collect();
        out.sort_unstable_by_key(|p| (*p != pg as u32, *p));
        out
    }

    /// Sums memory over the live, non-zombie members of the session.
    pub(super) fn memory(&self) -> TreeMemory {
        use sysinfo::{ProcessRefreshKind, ProcessStatus, ProcessesToUpdate, System};
        let pg = self.pgid.load(Ordering::SeqCst);
        let mut mem = TreeMemory::default();
        if pg <= 1 || self.gone.load(Ordering::SeqCst) {
            return mem;
        }
        let Ok(mut sys) = System::new() else {
            return mem;
        };
        sys.refresh_processes_specifics(
            ProcessesToUpdate::All,
            true,
            ProcessRefreshKind::nothing().with_memory(),
        );
        let sid = sysinfo::Pid::from_u32(pg as u32);
        for (pid, p) in sys.processes() {
            if p.session_id() != Some(sid) || p.status() == ProcessStatus::Zombie {
                continue;
            }
            if p.thread_kind().is_some() {
                // Linux lists threads as tasks; count processes only.
                continue;
            }
            let rss = p.memory();
            mem.process_count += 1;
            mem.rss_bytes += rss;
            mem.private_bytes += private_bytes(pid.as_u32()).unwrap_or(rss);
        }
        mem
    }
}

/// Memory of the calling process alone.
pub(super) fn self_memory() -> TreeMemory {
    use sysinfo::{ProcessRefreshKind, ProcessesToUpdate, System};
    let pid = std::process::id();
    let Ok(mut sys) = System::new() else {
        return TreeMemory::default();
    };
    let p = sysinfo::Pid::from_u32(pid);
    sys.refresh_processes_specifics(
        ProcessesToUpdate::Some(&[p]),
        true,
        ProcessRefreshKind::nothing().with_memory(),
    );
    match sys.process(p) {
        Some(proc_) => {
            let rss = proc_.memory();
            TreeMemory {
                rss_bytes: rss,
                private_bytes: private_bytes(pid).unwrap_or(rss),
                process_count: 1,
            }
        }
        None => TreeMemory::default(),
    }
}

#[cfg(target_os = "linux")]
fn private_bytes(pid: u32) -> Option<u64> {
    let text = std::fs::read_to_string(format!("/proc/{pid}/smaps_rollup")).ok()?;
    let mut kb = 0u64;
    let mut found = false;
    for line in text.lines() {
        if let Some(rest) = line
            .strip_prefix("Private_Clean:")
            .or_else(|| line.strip_prefix("Private_Dirty:"))
        {
            let v = rest.trim().trim_end_matches("kB").trim();
            kb += v.parse::<u64>().ok()?;
            found = true;
        }
    }
    found.then_some(kb * 1024)
}

#[cfg(not(target_os = "linux"))]
fn private_bytes(_pid: u32) -> Option<u64> {
    None
}

/// `PATH` used when the broker has none, as `execvp` does.
const DEFAULT_PATH: &str = "/usr/bin:/bin";

/// Outcome of checking one candidate file.
enum Candidate {
    Executable,
    NotExecutable,
    Missing,
}

fn check_candidate(path: &Path) -> Candidate {
    use std::os::unix::fs::PermissionsExt;
    // `metadata` follows symlinks, like exec does.
    match std::fs::metadata(path) {
        Ok(m) if m.is_file() && m.permissions().mode() & 0o111 != 0 => Candidate::Executable,
        Ok(_) => Candidate::NotExecutable,
        Err(_) => Candidate::Missing,
    }
}

/// Finds `program` like a shell would: a name containing `/` is taken as a
/// path (relative to the broker's working directory), anything else is
/// searched in the absolute entries of `path`; empty and relative entries,
/// which would mean the current directory, are skipped. Errors name the
/// program only, never its arguments or environment.
fn find_program(program: &OsStr, path: Option<&OsStr>) -> Result<PathBuf, ResolveError> {
    let name = || program.to_string_lossy().into_owned();
    if program.is_empty() {
        return Err(ResolveError::NotFound(name()));
    }
    let p = Path::new(program);
    if program.as_bytes().contains(&b'/') {
        let abs = std::path::absolute(p).map_err(|_| ResolveError::NotFound(name()))?;
        return match check_candidate(&abs) {
            Candidate::Executable => Ok(abs),
            Candidate::NotExecutable => Err(ResolveError::NotExecutable(name())),
            Candidate::Missing => Err(ResolveError::NotFound(name())),
        };
    }
    let path = path.unwrap_or_else(|| OsStr::new(DEFAULT_PATH));
    let mut saw_non_exec = false;
    for dir in std::env::split_paths(path).filter(|d| d.is_absolute()) {
        let cand = dir.join(p);
        match check_candidate(&cand) {
            Candidate::Executable => return Ok(cand),
            Candidate::NotExecutable => saw_non_exec = true,
            Candidate::Missing => {}
        }
    }
    Err(if saw_non_exec {
        ResolveError::NotExecutable(name())
    } else {
        ResolveError::NotFound(name())
    })
}

pub(super) fn resolve_command(program: &OsStr, args: &[OsString]) -> Result<Command, ResolveError> {
    let path = std::env::var_os("PATH");
    let resolved = find_program(program, path.as_deref())?;
    let mut cmd = Command::new(resolved);
    cmd.args(args);
    Ok(cmd)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn make_file(dir: &Path, name: &str, mode: u32) -> PathBuf {
        let p = dir.join(name);
        std::fs::write(&p, b"#!/bin/sh\nexit 0\n").unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(mode)).unwrap();
        p
    }

    #[test]
    fn finds_executable_on_path_as_absolute() {
        let a = tempfile::tempdir().unwrap();
        let b = tempfile::tempdir().unwrap();
        let exe = make_file(b.path(), "tool", 0o755);
        let path = std::env::join_paths([a.path(), b.path()]).unwrap();
        let got = find_program(OsStr::new("tool"), Some(&path)).unwrap();
        assert_eq!(got, exe);
        assert!(got.is_absolute());
    }

    #[test]
    fn skips_non_executable_and_keeps_searching() {
        let a = tempfile::tempdir().unwrap();
        let b = tempfile::tempdir().unwrap();
        make_file(a.path(), "tool", 0o644);
        let exe = make_file(b.path(), "tool", 0o700);
        let path = std::env::join_paths([a.path(), b.path()]).unwrap();
        assert_eq!(find_program(OsStr::new("tool"), Some(&path)).unwrap(), exe);
    }

    #[test]
    fn only_non_executable_match_is_not_executable() {
        let a = tempfile::tempdir().unwrap();
        make_file(a.path(), "tool", 0o644);
        let path = std::env::join_paths([a.path()]).unwrap();
        assert_eq!(
            find_program(OsStr::new("tool"), Some(&path)),
            Err(ResolveError::NotExecutable("tool".into()))
        );
    }

    #[test]
    fn directory_on_path_is_not_a_program() {
        let a = tempfile::tempdir().unwrap();
        let sub = a.path().join("tool");
        std::fs::create_dir(&sub).unwrap();
        let path = std::env::join_paths([a.path()]).unwrap();
        assert!(find_program(OsStr::new("tool"), Some(&path)).is_err());
    }

    #[test]
    fn empty_and_relative_path_entries_are_ignored() {
        let a = tempfile::tempdir().unwrap();
        make_file(a.path(), "tool", 0o755);
        // A relative entry naming the same directory must not be searched.
        let rel = a.path().strip_prefix("/").unwrap().as_os_str().to_owned();
        let mut path = OsString::from(":");
        path.push(&rel);
        path.push(":.");
        assert_eq!(
            find_program(OsStr::new("tool"), Some(&path)),
            Err(ResolveError::NotFound("tool".into()))
        );
    }

    #[test]
    fn path_with_separator_is_checked_directly() {
        let a = tempfile::tempdir().unwrap();
        let exe = make_file(a.path(), "tool", 0o755);
        let plain = make_file(a.path(), "data", 0o644);
        let empty = OsStr::new("");
        assert_eq!(find_program(exe.as_os_str(), Some(empty)).unwrap(), exe);
        assert_eq!(
            find_program(plain.as_os_str(), Some(empty)),
            Err(ResolveError::NotExecutable(
                plain.to_string_lossy().into_owned()
            ))
        );
        let missing = a.path().join("nope");
        assert!(matches!(
            find_program(missing.as_os_str(), Some(empty)),
            Err(ResolveError::NotFound(_))
        ));
    }

    #[test]
    fn errors_name_the_program_but_not_arguments() {
        let err = resolve_command(
            OsStr::new("mcpdoze-test-no-such-program-xyz"),
            &[OsString::from("--token=secret-arg-value")],
        )
        .unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("mcpdoze-test-no-such-program-xyz"), "{msg}");
        assert!(!msg.contains("secret-arg-value"), "{msg}");
    }

    #[test]
    fn resolves_sh_to_an_absolute_path() {
        let cmd = resolve_command(OsStr::new("sh"), &[]).unwrap();
        assert!(Path::new(cmd.as_std().get_program()).is_absolute());
    }
}
