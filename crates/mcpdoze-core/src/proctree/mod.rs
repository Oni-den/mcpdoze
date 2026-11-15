// SPDX-License-Identifier: GPL-3.0-or-later

//! Process trees: spawn a backend so that it and every descendant can be
//! measured and killed as one unit, and so that nothing outlives the broker.
//!
//! * Windows: the root is created suspended (`CREATE_SUSPENDED |
//!   CREATE_NO_WINDOW`), assigned to a Job Object with
//!   `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`, then resumed, so every descendant is
//!   in the job from its first instruction. Killing uses `TerminateJobObject`;
//!   dropping the tree (or the broker dying) closes the job handle, which kills
//!   the whole tree.
//! * Unix: the root calls `setsid` before `exec`, so the tree is one session
//!   and process group. Killing uses `killpg` (SIGTERM, then SIGKILL after the
//!   grace period). On Linux the root also gets `PR_SET_PDEATHSIG(SIGKILL)`.
//!
//! `unsafe` is permitted only in this module (OS process APIs); every block
//! carries a `SAFETY` comment.

#![allow(unsafe_code)]

use std::ffi::{OsStr, OsString};
use std::io;
use std::process::{ExitStatus, Stdio};
use std::time::Duration;

use tokio::process::{ChildStderr, ChildStdin, ChildStdout, Command};
use tokio::sync::watch;

#[cfg(unix)]
mod unix;
#[cfg(unix)]
use unix as imp;
#[cfg(windows)]
mod windows;
#[cfg(windows)]
use windows as imp;

/// How long `terminate` waits for the OS to finish tearing the tree down
/// after the hard kill before reporting an error.
const KILL_SETTLE: Duration = Duration::from_secs(5);

/// Memory use summed over every live process of a tree.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TreeMemory {
    /// Resident set / working set, in bytes.
    pub rss_bytes: u64,
    /// Private (non-shared) memory, in bytes. Windows: commit charge
    /// (`PrivateUsage`). Linux: `Private_Clean + Private_Dirty` from
    /// `smaps_rollup`. Other Unix systems: equal to `rss_bytes`.
    pub private_bytes: u64,
    /// Number of live processes in the tree.
    pub process_count: usize,
}

/// The piped standard streams of the root process.
#[derive(Debug)]
pub struct TreeStdio {
    /// Root's stdin. Dropping it closes the pipe (the polite stop signal).
    pub stdin: ChildStdin,
    /// Root's stdout.
    pub stdout: ChildStdout,
    /// Root's stderr. The caller must drain it, or a chatty backend blocks.
    pub stderr: ChildStderr,
}

/// A spawned root process plus all of its descendants.
///
/// Dropping a `ProcessTree` kills the entire tree.
///
/// The root process is reaped by a background task as soon as it exits, so
/// its exit can be observed ([`exited`](ProcessTree::exited)) while the
/// tree is still being measured or killed through `&self`.
#[derive(Debug)]
pub struct ProcessTree {
    exit: watch::Receiver<Option<Result<ExitStatus, String>>>,
    pid: u32,
    os: imp::Tree,
    stdio: Option<TreeStdio>,
}

impl ProcessTree {
    /// Spawns `cmd` as the root of a new process tree.
    ///
    /// stdin, stdout and stderr are always piped (see [`take_stdio`]). On
    /// Windows any creation flags already set on `cmd` are replaced; on Unix
    /// a `pre_exec` hook is added.
    ///
    /// [`take_stdio`]: ProcessTree::take_stdio
    pub fn spawn(mut cmd: Command) -> io::Result<Self> {
        cmd.stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let os = imp::Tree::new()?;
        imp::prepare(&mut cmd);
        let mut child = cmd.spawn()?;
        let Some(pid) = child.id() else {
            return Err(io::Error::other("spawned process has no pid"));
        };
        if let Err(e) = os.adopt(&child, pid) {
            // The root is still suspended (Windows) or alone (Unix); kill it.
            let _ = child.start_kill();
            return Err(e);
        }
        let stdio = match (child.stdin.take(), child.stdout.take(), child.stderr.take()) {
            (Some(stdin), Some(stdout), Some(stderr)) => Some(TreeStdio {
                stdin,
                stdout,
                stderr,
            }),
            _ => None,
        };
        let (exit_tx, exit) = watch::channel(None);
        tokio::spawn(async move {
            let res = child.wait().await.map_err(|e| e.to_string());
            let _ = exit_tx.send(Some(res));
        });
        Ok(Self {
            exit,
            pid,
            os,
            stdio,
        })
    }

    /// Takes the root's piped stdio. Returns `None` after the first call.
    pub fn take_stdio(&mut self) -> Option<TreeStdio> {
        self.stdio.take()
    }

    /// OS process id of the root.
    pub fn pid(&self) -> u32 {
        self.pid
    }

    /// Stops the tree: waits up to `grace` for the root to exit on its own,
    /// then kills every remaining process of the tree and waits (bounded)
    /// until the OS reports the tree empty.
    ///
    /// Closing stdin first is the caller's job. On Unix the process group is
    /// sent SIGTERM before the grace period starts.
    pub async fn terminate(&mut self, grace: Duration) -> io::Result<()> {
        self.os.soft_stop();
        let _ = tokio::time::timeout(grace, self.wait()).await;
        self.os.kill();
        // Reap the root so it is not left as a zombie (Unix) and its handle
        // state is final (Windows).
        let _ = tokio::time::timeout(KILL_SETTLE, self.wait()).await;
        let deadline = tokio::time::Instant::now() + KILL_SETTLE;
        while self.os.is_alive() {
            if tokio::time::Instant::now() >= deadline {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "process tree still alive after kill",
                ));
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        Ok(())
    }

    /// Kills the whole tree immediately without waiting.
    pub fn kill_now(&self) {
        self.os.kill();
    }

    /// Memory summed over every live process in the tree.
    ///
    /// On Unix this scans the process table and is comparatively slow; call
    /// it from a blocking context when polling frequently.
    pub fn memory(&self) -> TreeMemory {
        self.os.memory()
    }

    /// Pids of the live processes of the tree (root first when alive on
    /// Unix; job order on Windows). Same cost notes as [`memory`](ProcessTree::memory).
    pub fn pids(&self) -> Vec<u32> {
        self.os.pids()
    }

    /// True while any process of the tree is alive. On Unix an exited but
    /// not yet reaped root still counts until [`wait`](ProcessTree::wait).
    pub fn is_alive(&self) -> bool {
        self.os.is_alive()
    }

    /// Waits for the root process to exit. Descendants may still be running;
    /// use [`terminate`](ProcessTree::terminate) or drop the tree to end them.
    pub async fn wait(&mut self) -> io::Result<ExitStatus> {
        let mut rx = self.exit.clone();
        let res = rx
            .wait_for(Option::is_some)
            .await
            .map_err(|_| io::Error::other("process reaper ended"))?
            .clone();
        match res {
            Some(Ok(status)) => Ok(status),
            Some(Err(e)) => Err(io::Error::other(e)),
            None => Err(io::Error::other("process reaper ended")),
        }
    }

    /// A future that resolves once the root process has exited (or its
    /// reaper task ended with the runtime). Independent of `self`, so it can
    /// be awaited in a task while the tree stays usable for status and kill.
    pub fn exited(&self) -> impl std::future::Future<Output = ()> + Send + 'static {
        let mut rx = self.exit.clone();
        async move {
            let _ = rx.wait_for(Option::is_some).await;
        }
    }
}

impl Drop for ProcessTree {
    fn drop(&mut self) {
        self.os.kill();
    }
}

/// Why [`resolve_command`] refused to build a command.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ResolveError {
    /// The program was not found on `PATH`.
    #[error("program not found: {0}")]
    NotFound(String),
    /// The program exists but is not an executable regular file (Unix).
    #[error("program is not an executable file: {0}")]
    NotExecutable(String),
    /// The program path cannot be passed safely through `cmd.exe`.
    #[error("program path contains a character that cannot be safely quoted for cmd.exe: {0:?}")]
    UnsafeProgram(char),
    /// An argument cannot be passed safely through `cmd.exe`.
    #[error(
        "argument {index} contains a character that cannot be safely quoted for cmd.exe: {ch:?}"
    )]
    UnsafeArgument {
        /// Zero-based argument index.
        index: usize,
        /// The offending character.
        ch: char,
    },
    /// A batch path or argument is not valid Unicode (required for cmd.exe
    /// quoting).
    #[error("batch path or argument is not valid Unicode")]
    NotUnicode,
}

impl From<ResolveError> for io::Error {
    fn from(e: ResolveError) -> Self {
        let kind = match e {
            ResolveError::NotFound(_) => io::ErrorKind::NotFound,
            ResolveError::NotExecutable(_) => io::ErrorKind::PermissionDenied,
            _ => io::ErrorKind::InvalidInput,
        };
        io::Error::new(kind, e)
    }
}

/// Characters refused inside a `cmd.exe` command line. `"` cannot be
/// escaped inside a quoted cmd argument, `%` and `!` trigger variable
/// expansion even inside quotes, and line breaks / NUL end the command.
const CMD_UNSAFE: &[char] = &['"', '%', '!', '\r', '\n', '\0'];

fn check_cmd_safe(s: &str) -> Result<(), char> {
    match s.chars().find(|c| CMD_UNSAFE.contains(c)) {
        Some(c) => Err(c),
        None => Ok(()),
    }
}

/// Quotes one token for a `cmd.exe` line whose target program parses its
/// command line with the usual MSVC CRT rules. The caller has already
/// rejected `CMD_UNSAFE` characters, so only trailing backslashes need care:
/// they are doubled so they do not escape the closing quote.
fn quote_cmd_token(s: &str, out: &mut String) {
    out.push('"');
    out.push_str(s);
    let trailing = s.len() - s.trim_end_matches('\\').len();
    for _ in 0..trailing {
        out.push('\\');
    }
    out.push('"');
}

/// Builds the text that follows `cmd.exe /d /s /c` to run a batch file
/// `script` with `args`: the whole line is wrapped in one extra pair of
/// quotes, which `/s` strips, and every token is individually quoted.
///
/// Refuses any token containing a character that cannot be quoted safely.
pub fn batch_command_line<S: AsRef<str>>(script: &str, args: &[S]) -> Result<String, ResolveError> {
    check_cmd_safe(script).map_err(ResolveError::UnsafeProgram)?;
    for (index, a) in args.iter().enumerate() {
        check_cmd_safe(a.as_ref()).map_err(|ch| ResolveError::UnsafeArgument { index, ch })?;
    }
    let mut line = String::from("\"");
    quote_cmd_token(script, &mut line);
    for a in args {
        line.push(' ');
        quote_cmd_token(a.as_ref(), &mut line);
    }
    line.push('"');
    Ok(line)
}

/// Builds a [`Command`] for `program` with `args`, ready for
/// [`ProcessTree::spawn`].
///
/// On Windows the program is looked up on `PATH` with `PATHEXT` (the current
/// directory is not searched). `.cmd` / `.bat` shims such as `npx` are run
/// through `%SystemRoot%\System32\cmd.exe /d /s /c` with every token quoted;
/// arguments that cannot be quoted safely (`"`, `%`, `!`, line breaks) are
/// refused rather than risk command injection. On Unix a name without `/` is
/// searched in the absolute `PATH` entries and anything else is taken as a
/// path; either way it must be an executable regular file, and the command
/// runs the resolved absolute path with the arguments unchanged.
pub fn resolve_command<P, I, A>(program: P, args: I) -> Result<Command, ResolveError>
where
    P: AsRef<OsStr>,
    I: IntoIterator<Item = A>,
    A: AsRef<OsStr>,
{
    let args: Vec<OsString> = args.into_iter().map(|a| a.as_ref().to_owned()).collect();
    imp::resolve_command(program.as_ref(), &args)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn batch_line_quotes_every_token() {
        let line =
            batch_command_line(r"C:\tools\npx.cmd", &["-y", "@scope/pkg", "a b&c|d"]).unwrap();
        assert_eq!(line, r#"""C:\tools\npx.cmd" "-y" "@scope/pkg" "a b&c|d"""#);
    }

    #[test]
    fn batch_line_doubles_trailing_backslashes() {
        let line = batch_command_line(r"x.cmd", &[r"C:\dir\", ""]).unwrap();
        assert_eq!(line, r#"""x.cmd" "C:\dir\\" """"#);
    }

    #[test]
    fn batch_line_allows_shell_metacharacters_inside_quotes() {
        let line = batch_command_line("x.bat", &["^&<>()|"]).unwrap();
        assert_eq!(line, r#"""x.bat" "^&<>()|"""#);
    }

    #[test]
    fn batch_line_rejects_unquotable_arguments() {
        for (bad, ch) in [
            ("a\"b", '"'),
            ("%PATH%", '%'),
            ("hi!", '!'),
            ("a\nb", '\n'),
            ("a\rb", '\r'),
            ("a\0b", '\0'),
        ] {
            assert_eq!(
                batch_command_line("x.cmd", &["ok", bad]),
                Err(ResolveError::UnsafeArgument { index: 1, ch }),
                "{bad:?} must be rejected"
            );
        }
    }

    #[test]
    fn batch_line_rejects_unquotable_program() {
        assert_eq!(
            batch_command_line::<&str>("C:\\100%\\x.cmd", &[]),
            Err(ResolveError::UnsafeProgram('%'))
        );
    }

    #[test]
    fn resolve_error_maps_to_io_kind() {
        let e: io::Error = ResolveError::UnsafeProgram('"').into();
        assert_eq!(e.kind(), io::ErrorKind::InvalidInput);
        let e: io::Error = ResolveError::NotFound("x".into()).into();
        assert_eq!(e.kind(), io::ErrorKind::NotFound);
        let e: io::Error = ResolveError::NotExecutable("x".into()).into();
        assert_eq!(e.kind(), io::ErrorKind::PermissionDenied);
    }

    #[test]
    fn resolve_missing_program_is_not_found() {
        let r = resolve_command("mcpdoze-test-no-such-program-xyz", ["a"]);
        assert!(matches!(r, Err(ResolveError::NotFound(_))));
        let e: io::Error = r.unwrap_err().into();
        assert_eq!(e.kind(), io::ErrorKind::NotFound);
    }
}

/// Spawning tests. Each has a hard timeout; on timeout or panic the
/// `ProcessTree` is dropped, which kills the whole tree.
#[cfg(test)]
mod spawn_tests {
    use super::*;
    use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System};
    use tokio::io::AsyncReadExt;
    use tokio::time::{sleep, timeout, Instant};

    const TEST_TIMEOUT: Duration = Duration::from_secs(60);

    /// Root -> shell -> long sleeper, built from OS built-ins.
    fn chain_cmd() -> Command {
        #[cfg(windows)]
        {
            let mut c = Command::new("cmd.exe");
            c.raw_arg(r#"/d /c "cmd /d /c ping -n 300 127.0.0.1""#);
            c
        }
        #[cfg(unix)]
        {
            let mut c = Command::new("sh");
            // The trailing `; true` stops the inner shell from exec-ing `sleep` itself
            // (macOS's sh does that for a lone last command), which would shorten the
            // chain to two processes and defeat the three-deep assertion.
            c.arg("-c").arg(r#"sh -c "sleep 300; true" & wait"#);
            c
        }
    }

    const SLEEPER: &str = if cfg!(windows) { "ping" } else { "sleep" };

    fn refresh(sys: &mut System) {
        sys.refresh_processes_specifics(
            ProcessesToUpdate::All,
            true,
            ProcessRefreshKind::nothing(),
        );
    }

    /// (pid, start_time, lowercase name) of `root` and all its descendants.
    fn tree_of(sys: &System, root: u32) -> Vec<(Pid, u64, String)> {
        let mut out = Vec::new();
        let mut frontier = vec![Pid::from_u32(root)];
        while let Some(pid) = frontier.pop() {
            let Some(p) = sys.process(pid) else { continue };
            out.push((
                pid,
                p.start_time(),
                p.name().to_string_lossy().to_lowercase(),
            ));
            for (cpid, c) in sys.processes() {
                if c.parent() == Some(pid)
                    && c.thread_kind().is_none()
                    && !out.iter().any(|o| o.0 == *cpid)
                {
                    frontier.push(*cpid);
                }
            }
        }
        out
    }

    /// Waits until the chain (>= 3 processes including the sleeper) is up.
    async fn wait_for_chain(sys: &mut System, root: u32) -> Vec<(Pid, u64, String)> {
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            refresh(sys);
            let t = tree_of(sys, root);
            if t.len() >= 3 && t.iter().any(|p| p.2.contains(SLEEPER)) {
                return t;
            }
            assert!(Instant::now() < deadline, "chain never came up: {t:?}");
            sleep(Duration::from_millis(100)).await;
        }
    }

    /// Waits (bounded) until none of `procs` exists any more.
    async fn assert_all_gone(sys: &mut System, procs: &[(Pid, u64, String)]) {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            refresh(sys);
            let alive: Vec<_> = procs
                .iter()
                .filter(|(pid, start, _)| {
                    sys.process(*pid).is_some_and(|p| p.start_time() == *start)
                })
                .collect();
            if alive.is_empty() {
                return;
            }
            assert!(Instant::now() < deadline, "processes survived: {alive:?}");
            sleep(Duration::from_millis(100)).await;
        }
    }

    #[tokio::test]
    async fn terminate_kills_whole_tree() {
        timeout(TEST_TIMEOUT, async {
            let mut tree = ProcessTree::spawn(chain_cmd()).expect("spawn");
            let mut sys = System::new().expect("sysinfo");
            let procs = wait_for_chain(&mut sys, tree.pid()).await;
            assert!(tree.is_alive());
            let mem = tree.memory();
            assert!(mem.process_count >= 3, "{mem:?}");
            assert!(mem.rss_bytes > 0, "{mem:?}");
            drop(tree.take_stdio());
            tree.terminate(Duration::from_millis(500))
                .await
                .expect("terminate");
            assert!(!tree.is_alive());
            assert_all_gone(&mut sys, &procs).await;
        })
        .await
        .expect("test timed out");
    }

    #[tokio::test]
    async fn drop_kills_whole_tree() {
        timeout(TEST_TIMEOUT, async {
            let tree = ProcessTree::spawn(chain_cmd()).expect("spawn");
            let mut sys = System::new().expect("sysinfo");
            let procs = wait_for_chain(&mut sys, tree.pid()).await;
            drop(tree);
            assert_all_gone(&mut sys, &procs).await;
        })
        .await
        .expect("test timed out");
    }

    #[tokio::test]
    async fn kill_now_kills_whole_tree() {
        timeout(TEST_TIMEOUT, async {
            let mut tree = ProcessTree::spawn(chain_cmd()).expect("spawn");
            let mut sys = System::new().expect("sysinfo");
            let procs = wait_for_chain(&mut sys, tree.pid()).await;
            tree.kill_now();
            let _ = tree.wait().await.expect("wait");
            assert_all_gone(&mut sys, &procs).await;
        })
        .await
        .expect("test timed out");
    }

    #[tokio::test]
    async fn quick_exit_reports_status_and_output() {
        timeout(TEST_TIMEOUT, async {
            #[cfg(windows)]
            let cmd = {
                let mut c = Command::new("cmd.exe");
                c.raw_arg("/d /c echo hello&& exit 3");
                c
            };
            #[cfg(unix)]
            let cmd = {
                let mut c = Command::new("sh");
                c.arg("-c").arg("echo hello; exit 3");
                c
            };
            let mut tree = ProcessTree::spawn(cmd).expect("spawn");
            let stdio = tree.take_stdio().expect("stdio");
            assert!(tree.take_stdio().is_none());
            let mut out = Vec::new();
            stdio
                .stdout
                .take(4096)
                .read_to_end(&mut out)
                .await
                .expect("read");
            let status = tree.wait().await.expect("wait");
            assert_eq!(status.code(), Some(3));
            assert_eq!(String::from_utf8_lossy(&out).trim(), "hello");
            tree.terminate(Duration::from_millis(100))
                .await
                .expect("terminate");
            assert!(!tree.is_alive());
            assert_eq!(tree.memory().process_count, 0);
        })
        .await
        .expect("test timed out");
    }

    /// Removes a temporary file even if the test panics.
    #[cfg(windows)]
    struct TempFile(std::path::PathBuf);

    #[cfg(windows)]
    impl Drop for TempFile {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn batch_shim_receives_quoted_arguments() {
        timeout(TEST_TIMEOUT, async {
            let path =
                std::env::temp_dir().join(format!("mcpdoze-proctree-{}.cmd", std::process::id()));
            std::fs::write(&path, "@echo off\r\necho [%1] [%2]\r\n").expect("write shim");
            let _guard = TempFile(path.clone());
            let cmd = resolve_command(&path, ["a b&echo injected", r"C:\dir\"]).expect("resolve");
            let mut tree = ProcessTree::spawn(cmd).expect("spawn");
            let stdio = tree.take_stdio().expect("stdio");
            let mut out = Vec::new();
            stdio
                .stdout
                .take(4096)
                .read_to_end(&mut out)
                .await
                .expect("read");
            tree.wait().await.expect("wait");
            let text = String::from_utf8_lossy(&out);
            assert_eq!(text.trim(), r#"["a b&echo injected"] ["C:\dir\\"]"#);
        })
        .await
        .expect("test timed out");
    }

    #[cfg(windows)]
    #[test]
    fn resolve_runs_exe_directly() {
        // An .exe is run directly (std quoting), not through cmd.exe.
        let cmd = resolve_command("cmd", ["/c", "exit"]).expect("cmd.exe on PATH");
        let prog = std::path::PathBuf::from(cmd.as_std().get_program());
        assert!(prog.is_absolute());
        assert!(prog
            .extension()
            .is_some_and(|e| e.eq_ignore_ascii_case("exe")));
    }
}
