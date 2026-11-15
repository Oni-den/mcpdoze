// SPDX-License-Identifier: GPL-3.0-or-later

//! `mcpdoze autostart install|remove`: a per-user, no-admin login service.
//!
//! * Windows: Task Scheduler logon task `MCPDoze` (XML imported with
//!   `schtasks`, hidden, restart on failure, no time limit).
//! * Linux: systemd user unit `mcpdoze.service`.
//! * macOS: LaunchAgent `dev.mcpdoze.broker`.
//!
//! The generators are pure functions (unit-tested). The install/remove
//! functions only run external commands and report exactly what they did.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{bail, Context};

/// Windows scheduled task name.
pub const TASK_NAME: &str = "MCPDoze";
/// systemd unit file name.
pub const UNIT_NAME: &str = "mcpdoze.service";
/// launchd label.
pub const LAUNCHD_LABEL: &str = "dev.mcpdoze.broker";

/// Options of `autostart install`.
#[derive(Debug, Clone, Default)]
pub struct AutostartOpts {
    pub config: Option<PathBuf>,
    pub bind: Option<String>,
}

/// Registers the broker to start at login. Returns what was done.
pub fn install(opts: AutostartOpts) -> anyhow::Result<String> {
    let exe = std::env::current_exe().context("cannot determine the mcpdoze executable")?;
    let args = serve_args(&opts)?;
    if cfg!(windows) {
        install_windows(&exe, &args)
    } else if cfg!(target_os = "macos") {
        install_macos(&exe, &args)
    } else {
        install_linux(&exe, &args)
    }
}

/// Removes the registration. Idempotent.
pub fn remove() -> anyhow::Result<String> {
    if cfg!(windows) {
        remove_windows()
    } else if cfg!(target_os = "macos") {
        remove_macos()
    } else {
        remove_linux()
    }
}

/// Arguments after the executable: `serve [--config P] [--bind A]`.
fn serve_args(opts: &AutostartOpts) -> anyhow::Result<Vec<String>> {
    let mut a = vec!["serve".to_string()];
    if let Some(c) = &opts.config {
        let abs = std::path::absolute(c).context("cannot resolve --config")?;
        a.push("--config".into());
        a.push(abs.to_string_lossy().into_owned());
    }
    if let Some(b) = &opts.bind {
        a.push("--bind".into());
        a.push(b.clone());
    }
    Ok(a)
}

// ------------------------------------------------------------ generators

/// Escape text for XML element content / attribute values.
pub fn xml_escape(s: &str) -> String {
    let mut o = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => o.push_str("&amp;"),
            '<' => o.push_str("&lt;"),
            '>' => o.push_str("&gt;"),
            '"' => o.push_str("&quot;"),
            '\'' => o.push_str("&apos;"),
            c => o.push(c),
        }
    }
    o
}

/// Quote one argument for a Windows command line.
fn win_quote(a: &str) -> String {
    if !a.is_empty() && !a.contains([' ', '\t', '"']) {
        return a.to_string();
    }
    format!("\"{}\"", a.replace('"', "\\\""))
}

/// Task Scheduler XML. `user` (e.g. `DOMAIN\name`) restricts the logon
/// trigger and principal to the current user, which is what lets a standard
/// user register it without elevation.
pub fn windows_task_xml(exe: &Path, args: &[String], user: Option<&str>) -> String {
    let command = xml_escape(&exe.to_string_lossy());
    let arguments = xml_escape(
        &args
            .iter()
            .map(|a| win_quote(a))
            .collect::<Vec<_>>()
            .join(" "),
    );
    let user_el = user
        .map(|u| format!("      <UserId>{}</UserId>\n", xml_escape(u)))
        .unwrap_or_default();
    let principal_user = user
        .map(|u| format!("      <UserId>{}</UserId>\n", xml_escape(u)))
        .unwrap_or_default();
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-16\"?>
<Task version=\"1.4\" xmlns=\"http://schemas.microsoft.com/windows/2004/02/mit/task\">
  <RegistrationInfo>
    <Description>MCPDoze MCP broker (starts at logon)</Description>
  </RegistrationInfo>
  <Triggers>
    <LogonTrigger>
      <Enabled>true</Enabled>
{user_el}    </LogonTrigger>
  </Triggers>
  <Principals>
    <Principal id=\"Author\">
{principal_user}      <LogonType>InteractiveToken</LogonType>
      <RunLevel>LeastPrivilege</RunLevel>
    </Principal>
  </Principals>
  <Settings>
    <MultipleInstancesPolicy>IgnoreNew</MultipleInstancesPolicy>
    <DisallowStartIfOnBatteries>false</DisallowStartIfOnBatteries>
    <StopIfGoingOnBatteries>false</StopIfGoingOnBatteries>
    <AllowHardTerminate>true</AllowHardTerminate>
    <StartWhenAvailable>true</StartWhenAvailable>
    <RunOnlyIfNetworkAvailable>false</RunOnlyIfNetworkAvailable>
    <AllowStartOnDemand>true</AllowStartOnDemand>
    <Enabled>true</Enabled>
    <Hidden>true</Hidden>
    <ExecutionTimeLimit>PT0S</ExecutionTimeLimit>
    <Priority>7</Priority>
    <RestartOnFailure>
      <Interval>PT1M</Interval>
      <Count>999</Count>
    </RestartOnFailure>
  </Settings>
  <Actions Context=\"Author\">
    <Exec>
      <Command>{command}</Command>
      <Arguments>{arguments}</Arguments>
    </Exec>
  </Actions>
</Task>\n"
    )
}

/// Quote one word for a systemd `ExecStart=` line.
fn systemd_quote(a: &str) -> String {
    let mut o = String::from("\"");
    for c in a.chars() {
        match c {
            '\\' => o.push_str("\\\\"),
            '"' => o.push_str("\\\""),
            '%' => o.push_str("%%"),
            '$' => o.push_str("$$"),
            '\n' => o.push_str("\\n"),
            c => o.push(c),
        }
    }
    o.push('"');
    o
}

/// systemd user unit.
pub fn systemd_unit(exe: &Path, args: &[String]) -> String {
    let mut words = vec![systemd_quote(&exe.to_string_lossy())];
    words.extend(args.iter().map(|a| systemd_quote(a)));
    format!(
        "[Unit]\n\
Description=MCPDoze MCP broker\n\
After=network.target\n\
\n\
[Service]\n\
Type=simple\n\
ExecStart={}\n\
Restart=on-failure\n\
RestartSec=5\n\
\n\
[Install]\n\
WantedBy=default.target\n",
        words.join(" ")
    )
}

/// macOS LaunchAgent plist. `log` receives stdout and stderr.
pub fn launchd_plist(exe: &Path, args: &[String], log: &Path) -> String {
    let mut items = format!(
        "    <string>{}</string>\n",
        xml_escape(&exe.to_string_lossy())
    );
    for a in args {
        items.push_str(&format!("    <string>{}</string>\n", xml_escape(a)));
    }
    let log = xml_escape(&log.to_string_lossy());
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>
<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">
<plist version=\"1.0\">
<dict>
  <key>Label</key>
  <string>{LAUNCHD_LABEL}</string>
  <key>ProgramArguments</key>
  <array>
{items}  </array>
  <key>RunAtLoad</key>
  <true/>
  <key>KeepAlive</key>
  <dict>
    <key>SuccessfulExit</key>
    <false/>
  </dict>
  <key>ProcessType</key>
  <string>Background</string>
  <key>StandardOutPath</key>
  <string>{log}</string>
  <key>StandardErrorPath</key>
  <string>{log}</string>
</dict>
</plist>\n"
    )
}

/// UTF-16LE with BOM, as `schtasks /XML` expects.
fn utf16le_bom(s: &str) -> Vec<u8> {
    let mut out = vec![0xFF, 0xFE];
    for u in s.encode_utf16() {
        out.extend_from_slice(&u.to_le_bytes());
    }
    out
}

// ------------------------------------------------------------ run helper

struct Cmd {
    code: Option<i32>,
    stderr: String,
}

/// Run an external command with a hard timeout; stdout is discarded and at
/// most 4 KiB of stderr is kept.
fn run_cmd(program: &str, args: &[&str]) -> anyhow::Result<Cmd> {
    use std::io::Read;
    let mut child = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("cannot run `{program}`"))?;
    let deadline = Instant::now() + Duration::from_secs(30);
    let status = loop {
        if let Some(s) = child.try_wait()? {
            break s;
        }
        if Instant::now() > deadline {
            let _ = child.kill();
            let _ = child.wait();
            bail!("`{program}` timed out");
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    let mut buf = Vec::new();
    if let Some(e) = child.stderr.take() {
        let _ = e.take(4096).read_to_end(&mut buf);
    }
    Ok(Cmd {
        code: status.code(),
        stderr: String::from_utf8_lossy(&buf).trim().to_string(),
    })
}

fn must(c: Cmd, what: &str) -> anyhow::Result<()> {
    if c.code == Some(0) {
        Ok(())
    } else {
        bail!("{what} failed (exit {:?}): {}", c.code, c.stderr)
    }
}

// --------------------------------------------------------------- Windows

fn current_windows_user() -> Option<String> {
    let name = std::env::var("USERNAME").ok().filter(|s| !s.is_empty())?;
    match std::env::var("USERDOMAIN").ok().filter(|s| !s.is_empty()) {
        Some(d) => Some(format!("{d}\\{name}")),
        None => Some(name),
    }
}

fn install_windows(exe: &Path, args: &[String]) -> anyhow::Result<String> {
    let xml = windows_task_xml(exe, args, current_windows_user().as_deref());
    let tmp = std::env::temp_dir().join(format!("mcpdoze-task-{}.xml", std::process::id()));
    std::fs::write(&tmp, utf16le_bom(&xml)).context("cannot write the task XML")?;
    let tmp_s = tmp.to_string_lossy().into_owned();
    let created = run_cmd(
        "schtasks",
        &["/Create", "/TN", TASK_NAME, "/XML", &tmp_s, "/F"],
    );
    let _ = std::fs::remove_file(&tmp);
    must(created?, "schtasks /Create")?;
    let started = run_cmd("schtasks", &["/Run", "/TN", TASK_NAME])?;
    let mut out = format!(
        "Ran: schtasks /Create /TN {TASK_NAME} /XML <generated file> /F\nRegistered logon task `{TASK_NAME}` running: {} {}\n",
        exe.display(),
        args.join(" ")
    );
    if started.code == Some(0) {
        out.push_str(&format!(
            "Ran: schtasks /Run /TN {TASK_NAME} (started now)\n"
        ));
    } else {
        out.push_str("Task registered; it will start at next logon.\n");
    }
    Ok(out)
}

fn remove_windows() -> anyhow::Result<String> {
    if run_cmd("schtasks", &["/Query", "/TN", TASK_NAME])?.code != Some(0) {
        return Ok(format!(
            "Task `{TASK_NAME}` is not installed; nothing to do.\n"
        ));
    }
    let _ = run_cmd("schtasks", &["/End", "/TN", TASK_NAME]);
    must(
        run_cmd("schtasks", &["/Delete", "/TN", TASK_NAME, "/F"])?,
        "schtasks /Delete",
    )?;
    Ok(format!(
        "Ran: schtasks /End /TN {TASK_NAME}\nRan: schtasks /Delete /TN {TASK_NAME} /F\nRemoved task `{TASK_NAME}`.\n"
    ))
}

// ----------------------------------------------------------------- Linux

fn home_dir() -> anyhow::Result<PathBuf> {
    std::env::var_os("HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .context("HOME is not set")
}

fn linux_unit_path() -> anyhow::Result<PathBuf> {
    let base = match std::env::var_os("XDG_CONFIG_HOME").filter(|v| !v.is_empty()) {
        Some(v) => PathBuf::from(v),
        None => home_dir()?.join(".config"),
    };
    Ok(base.join("systemd").join("user").join(UNIT_NAME))
}

fn install_linux(exe: &Path, args: &[String]) -> anyhow::Result<String> {
    let path = linux_unit_path()?;
    if let Some(p) = path.parent() {
        std::fs::create_dir_all(p).context("cannot create the systemd user directory")?;
    }
    std::fs::write(&path, systemd_unit(exe, args)).context("cannot write the unit file")?;
    must(
        run_cmd("systemctl", &["--user", "daemon-reload"])?,
        "systemctl --user daemon-reload",
    )?;
    must(
        run_cmd("systemctl", &["--user", "enable", "--now", UNIT_NAME])?,
        "systemctl --user enable --now",
    )?;
    Ok(format!(
        "Wrote {}\nRan: systemctl --user daemon-reload\nRan: systemctl --user enable --now {UNIT_NAME}\n",
        path.display()
    ))
}

fn remove_linux() -> anyhow::Result<String> {
    let path = linux_unit_path()?;
    if !path.exists() {
        return Ok(format!(
            "{} does not exist; nothing to do.\n",
            path.display()
        ));
    }
    let _ = run_cmd("systemctl", &["--user", "disable", "--now", UNIT_NAME]);
    std::fs::remove_file(&path).context("cannot remove the unit file")?;
    let _ = run_cmd("systemctl", &["--user", "daemon-reload"]);
    Ok(format!(
        "Ran: systemctl --user disable --now {UNIT_NAME}\nRemoved {}\nRan: systemctl --user daemon-reload\n",
        path.display()
    ))
}

// ----------------------------------------------------------------- macOS

fn macos_paths() -> anyhow::Result<(PathBuf, PathBuf)> {
    let home = home_dir()?;
    Ok((
        home.join("Library")
            .join("LaunchAgents")
            .join(format!("{LAUNCHD_LABEL}.plist")),
        home.join("Library").join("Logs").join("mcpdoze.log"),
    ))
}

fn uid() -> anyhow::Result<String> {
    let out = Command::new("id")
        .arg("-u")
        .stdin(Stdio::null())
        .output()
        .context("cannot run `id -u`")?;
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

fn install_macos(exe: &Path, args: &[String]) -> anyhow::Result<String> {
    let (plist, log) = macos_paths()?;
    if let Some(p) = plist.parent() {
        std::fs::create_dir_all(p).context("cannot create the LaunchAgents directory")?;
    }
    std::fs::write(&plist, launchd_plist(exe, args, &log)).context("cannot write the plist")?;
    let domain = format!("gui/{}", uid()?);
    // Re-installing: unload any previous copy first (ignore "not loaded").
    let _ = run_cmd(
        "launchctl",
        &["bootout", &format!("{domain}/{LAUNCHD_LABEL}")],
    );
    let p = plist.to_string_lossy().into_owned();
    must(
        run_cmd("launchctl", &["bootstrap", &domain, &p])?,
        "launchctl bootstrap",
    )?;
    Ok(format!(
        "Wrote {}\nRan: launchctl bootstrap {domain} {}\n",
        plist.display(),
        plist.display()
    ))
}

fn remove_macos() -> anyhow::Result<String> {
    let (plist, _) = macos_paths()?;
    if !plist.exists() {
        return Ok(format!(
            "{} does not exist; nothing to do.\n",
            plist.display()
        ));
    }
    let domain = format!("gui/{}", uid()?);
    let _ = run_cmd(
        "launchctl",
        &["bootout", &format!("{domain}/{LAUNCHD_LABEL}")],
    );
    std::fs::remove_file(&plist).context("cannot remove the plist")?;
    Ok(format!(
        "Ran: launchctl bootout {domain}/{LAUNCHD_LABEL}\nRemoved {}\n",
        plist.display()
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn xml_escaping() {
        assert_eq!(xml_escape("a&b<c>\"'"), "a&amp;b&lt;c&gt;&quot;&apos;");
    }

    #[test]
    fn windows_xml_golden() {
        let xml = windows_task_xml(
            Path::new("C:\\Program Files\\Tom & Co\\mcpdoze.exe"),
            &args(&["serve", "--config", "C:\\My Dir\\a&b.toml"]),
            Some("EXAMPLE\\user"),
        );
        assert!(
            xml.starts_with("<?xml version=\"1.0\" encoding=\"UTF-16\"?>\n<Task version=\"1.4\"")
        );
        assert!(xml.contains("<Command>C:\\Program Files\\Tom &amp; Co\\mcpdoze.exe</Command>"));
        assert!(xml.contains(
            "<Arguments>serve --config &quot;C:\\My Dir\\a&amp;b.toml&quot;</Arguments>"
        ));
        assert!(xml.contains(
            "<LogonTrigger>\n      <Enabled>true</Enabled>\n      <UserId>EXAMPLE\\user</UserId>\n    </LogonTrigger>"
        ));
        assert!(xml.contains("<RunLevel>LeastPrivilege</RunLevel>"));
        assert!(xml.contains("<Hidden>true</Hidden>"));
        assert!(xml.contains("<ExecutionTimeLimit>PT0S</ExecutionTimeLimit>"));
        assert!(xml.contains("<RestartOnFailure>"));
        assert!(xml.ends_with("</Task>\n"));
        let no_user = windows_task_xml(Path::new("m.exe"), &args(&["serve"]), None);
        assert!(!no_user.contains("<UserId>"));
        assert!(no_user.contains("<Arguments>serve</Arguments>"));
    }

    #[test]
    fn systemd_golden() {
        let u = systemd_unit(
            Path::new("/opt/my tools/mcpdoze"),
            &args(&["serve", "--config", "/home/user/a b/c%d$.toml"]),
        );
        assert_eq!(
            u,
            "[Unit]\nDescription=MCPDoze MCP broker\nAfter=network.target\n\n[Service]\nType=simple\nExecStart=\"/opt/my tools/mcpdoze\" \"serve\" \"--config\" \"/home/user/a b/c%%d$$.toml\"\nRestart=on-failure\nRestartSec=5\n\n[Install]\nWantedBy=default.target\n"
        );
    }

    #[test]
    fn plist_golden() {
        let p = launchd_plist(
            Path::new("/Applications/A & B/mcpdoze"),
            &args(&["serve", "--bind", "127.0.0.1:7878"]),
            Path::new("/home/user/Library/Logs/mcpdoze.log"),
        );
        assert!(p.contains("<string>dev.mcpdoze.broker</string>"));
        assert!(p.contains("    <string>/Applications/A &amp; B/mcpdoze</string>\n    <string>serve</string>\n    <string>--bind</string>\n    <string>127.0.0.1:7878</string>\n"));
        assert!(p.contains("<key>RunAtLoad</key>\n  <true/>"));
        assert!(p.contains("<key>SuccessfulExit</key>\n    <false/>"));
        assert!(p.contains(
            "<key>StandardErrorPath</key>\n  <string>/home/user/Library/Logs/mcpdoze.log</string>"
        ));
        assert!(p.ends_with("</plist>\n"));
    }

    #[test]
    fn utf16_has_bom() {
        assert_eq!(utf16le_bom("a"), vec![0xFF, 0xFE, b'a', 0]);
    }

    #[test]
    fn serve_args_shape() {
        let a = serve_args(&AutostartOpts {
            config: None,
            bind: Some("127.0.0.1:9".into()),
        })
        .unwrap();
        assert_eq!(a, ["serve", "--bind", "127.0.0.1:9"]);
    }
}
