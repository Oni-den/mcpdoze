// SPDX-License-Identifier: GPL-3.0-or-later

//! Unix backend: one session / process group per tree.

use std::ffi::{OsStr, OsString};
use std::io;
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

pub(super) fn resolve_command(program: &OsStr, args: &[OsString]) -> Result<Command, ResolveError> {
    let mut cmd = Command::new(program);
    cmd.args(args);
    Ok(cmd)
}
