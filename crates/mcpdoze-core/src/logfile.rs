// SPDX-License-Identifier: GPL-3.0-or-later

//! Size-capped rotating log file: used by `mcpdozed`, which has no usable
//! stdout/stderr (Windows GUI subsystem, or a service manager that discards
//! them), and by the failure diagnostics writer (`diag`). The `mcpdoze`
//! crate re-exports this module as `mcpdoze::logfile`.
//!
//! `mcpdozed.log` is written until the next write would take it past the
//! cap; then `mcpdozed.log.(N-1)` becomes `.N` (the oldest is deleted), ...,
//! `mcpdozed.log` becomes `.1`, and a fresh file is started. At most
//! `1 + keep` files exist.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, TryLockError};

/// Default size cap of one file.
pub const DEFAULT_MAX_BYTES: u64 = 5 * 1024 * 1024;
/// Default number of rotated files kept.
pub const DEFAULT_KEEP: usize = 5;

struct State {
    private: bool,
    path: PathBuf,
    max_bytes: u64,
    keep: usize,
    file: Option<File>,
    size: u64,
}

/// A cloneable handle; every clone writes to the same file.
#[derive(Clone)]
pub struct RotatingLog {
    state: Arc<Mutex<State>>,
}

impl std::fmt::Debug for RotatingLog {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RotatingLog").finish_non_exhaustive()
    }
}

/// `<path>.<n>`.
pub fn rotated_path(path: &Path, n: usize) -> PathBuf {
    let mut s = path.as_os_str().to_owned();
    s.push(format!(".{n}"));
    PathBuf::from(s)
}

fn open_append(path: &Path, private: bool) -> io::Result<File> {
    let mut o = if private {
        crate::auth::private_options()
    } else {
        OpenOptions::new()
    };
    o.create(true).append(true).open(path)
}

impl State {
    fn rotate(&mut self) -> io::Result<()> {
        self.file = None;
        if self.keep == 0 {
            let _ = fs::remove_file(&self.path);
        } else {
            let oldest = rotated_path(&self.path, self.keep);
            let _ = fs::remove_file(&oldest);
            for n in (1..self.keep).rev() {
                let from = rotated_path(&self.path, n);
                if from.exists() {
                    fs::rename(&from, rotated_path(&self.path, n + 1))?;
                }
            }
            if self.path.exists() {
                fs::rename(&self.path, rotated_path(&self.path, 1))?;
            }
        }
        self.file = Some(open_append(&self.path, self.private)?);
        self.size = 0;
        Ok(())
    }

    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if self.size > 0 && self.size + buf.len() as u64 > self.max_bytes {
            self.rotate()?;
        }
        let f = match &mut self.file {
            Some(f) => f,
            None => self.file.insert(open_append(&self.path, self.private)?),
        };
        f.write_all(buf)?;
        self.size += buf.len() as u64;
        Ok(buf.len())
    }
}

impl RotatingLog {
    /// Opens (appends to) `path`, creating its directory. `max_bytes` is
    /// the cap of one file, `keep` the number of rotated files kept.
    pub fn open(path: &Path, max_bytes: u64, keep: usize) -> io::Result<Self> {
        Self::open_with(path, max_bytes, keep, false)
    }

    /// Like [`open`](RotatingLog::open), but every file is created
    /// owner-only (mode 0600 on Unix) and the directory 0700.
    pub fn open_private(path: &Path, max_bytes: u64, keep: usize) -> io::Result<Self> {
        Self::open_with(path, max_bytes, keep, true)
    }

    fn open_with(path: &Path, max_bytes: u64, keep: usize, private: bool) -> io::Result<Self> {
        if let Some(dir) = path.parent() {
            if private {
                crate::auth::create_private_dir(dir)?;
            } else {
                fs::create_dir_all(dir)?;
            }
        }
        let file = open_append(path, private)?;
        let size = file.metadata()?.len();
        let mut st = State {
            private,
            path: path.to_path_buf(),
            max_bytes: max_bytes.max(1),
            keep,
            file: Some(file),
            size,
        };
        if st.size >= st.max_bytes {
            st.rotate()?;
        }
        Ok(Self {
            state: Arc::new(Mutex::new(st)),
        })
    }

    /// Writes `line` without waiting for the lock (for the panic hook,
    /// which may run while a log write holds it). False if it was busy.
    pub fn try_write(&self, line: &[u8]) -> bool {
        match self.state.try_lock() {
            Ok(mut st) => st.write(line).is_ok(),
            Err(TryLockError::Poisoned(p)) => p.into_inner().write(line).is_ok(),
            Err(TryLockError::WouldBlock) => false,
        }
    }
}

impl Write for RotatingLog {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
        match &mut st.file {
            Some(f) => f.flush(),
            None => Ok(()),
        }
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for RotatingLog {
    type Writer = RotatingLog;

    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rotates_at_the_cap_and_keeps_n_files() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("logs").join("mcpdozed.log");
        let mut log = RotatingLog::open(&path, 100, 2).unwrap();
        let line = [b'x'; 39];
        for i in 0..10u8 {
            let mut l = line.to_vec();
            l[0] = b'0' + i;
            l.push(b'\n');
            log.write_all(&l).unwrap();
        }
        log.flush().unwrap();
        // 40-byte lines, two per 100-byte file: 0-1, 2-3, 4-5, 6-7, 8-9.
        let cur = fs::read_to_string(&path).unwrap();
        assert_eq!(cur.len(), 80);
        assert!(cur.starts_with('8'), "{cur}");
        let one = fs::read_to_string(rotated_path(&path, 1)).unwrap();
        assert!(one.starts_with('6'), "{one}");
        let two = fs::read_to_string(rotated_path(&path, 2)).unwrap();
        assert!(two.starts_with('4'), "{two}");
        assert!(!rotated_path(&path, 3).exists());
    }

    #[test]
    fn reopening_a_full_file_rotates_first() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mcpdozed.log");
        fs::write(&path, vec![b'a'; 200]).unwrap();
        let log = RotatingLog::open(&path, 100, 3).unwrap();
        assert!(log.try_write(b"fresh\n"));
        assert_eq!(fs::read_to_string(&path).unwrap(), "fresh\n");
        assert_eq!(fs::metadata(rotated_path(&path, 1)).unwrap().len(), 200);
    }

    #[test]
    fn appends_below_the_cap() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mcpdozed.log");
        fs::write(&path, "old\n").unwrap();
        let mut log = RotatingLog::open(&path, 1000, 1).unwrap();
        log.write_all(b"new\n").unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "old\nnew\n");
        assert!(!rotated_path(&path, 1).exists());
    }
}
