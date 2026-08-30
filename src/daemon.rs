//! Background daemon management: auto-start at login (macOS launchd, Linux
//! systemd user, Windows Run key), restart, and status. The integration runs
//! `<current_exe> run`, so the daemon interval comes from the config file.

use anyhow::{Context, Result};

pub fn enable() -> Result<()> {
    enable_impl()
}

pub fn disable() -> Result<()> {
    disable_impl()
}

pub fn restart() -> Result<()> {
    restart_impl()
}

pub fn status() -> Result<()> {
    status_impl()?;
    print_cycle_status();
    Ok(())
}

fn display_version(version: &str) -> &str {
    if version.is_empty() {
        "unknown (status written by an older binary)"
    } else {
        version
    }
}

fn print_cycle_status() {
    match crate::status::read() {
        None => println!("status: no cycle recorded yet (run the daemon or a one-shot cycle once)"),
        Some(s) => {
            println!("version: {}", display_version(&s.version));
            println!("process: pid {}, started {}", s.pid, s.started_at);
            match s.interval_secs {
                Some(secs) => println!("interval: every {secs}s"),
                None => println!("interval: one-shot"),
            }
            println!("plugins: {}", s.plugins.join(", "));
            println!("device: {}", s.device.as_deref().unwrap_or("unknown"));
            match &s.last_cycle_at {
                Some(at) => println!("last cycle: {at}"),
                None => println!("last cycle: none yet"),
            }
            if s.succeeded.is_empty() {
                println!("ok: none");
            } else {
                println!("ok: {}", s.succeeded.join(", "));
            }
            for failure in &s.failed {
                println!("failed: {}: {}", failure.plugin, failure.error);
            }
            match &s.upload {
                Some(upload) => println!("upload: {upload}"),
                None => println!("upload: not reached"),
            }
            if let Some(error) = &s.cycle_error {
                println!("error: {error}");
            }
        }
    }
}

#[cfg(target_os = "macos")]
const MACOS_LABEL: &str = "apps.andersou.geekmagic-custom-monitors";

#[cfg(target_os = "macos")]
fn macos_uid() -> Result<String> {
    Ok(
        String::from_utf8(std::process::Command::new("id").arg("-u").output()?.stdout)
            .context("id -u output not utf8")?
            .trim()
            .to_string(),
    )
}

#[cfg(target_os = "macos")]
fn macos_plist() -> Result<String> {
    let home = std::env::var("HOME").context("HOME not set")?;
    Ok(format!("{home}/Library/LaunchAgents/{MACOS_LABEL}.plist"))
}

/// launchd starts gui agents with PATH=/usr/bin:/bin:/usr/sbin:/sbin, hiding
/// Homebrew, ~/.local/bin and friends. Plugins shell out (the Claude Code CLI
/// renews the OAuth token), so the daemon is given the same environment as the
/// interactive shell that enabled it.
#[cfg(target_os = "macos")]
fn xml_escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// Idempotent: bootout first, ignore failure (not loaded yet). `launchctl
/// bootout` returns before the domain forgets the label, and bootstrapping a
/// label that is still registered fails with "Input/output error" (5), so wait
/// for the service to disappear.
#[cfg(target_os = "macos")]
fn macos_bootout(uid: &str) {
    let _ = std::process::Command::new("launchctl")
        .args(["bootout", &format!("gui/{uid}/{MACOS_LABEL}")])
        .stderr(std::process::Stdio::null())
        .status();
    for _ in 0..50 {
        let still_loaded = std::process::Command::new("launchctl")
            .args(["print", &format!("gui/{uid}/{MACOS_LABEL}")])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map(|status| status.success())
            .unwrap_or(false);
        if !still_loaded {
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
}

/// Parse `launchctl print` output into (running, pid).
#[cfg(target_os = "macos")]
fn parse_launchd(output: &str) -> (bool, Option<u32>) {
    // The service-level `state =`/`pid =` come first; sub-entries repeat
    // `state =` (e.g. "active"), so only the first occurrence counts.
    let mut running = None;
    let mut pid = None;
    let mut pid_seen = false;
    for line in output.lines() {
        let line = line.trim();
        if running.is_none()
            && let Some(value) = line.strip_prefix("state =")
        {
            running = Some(value.trim() == "running");
        }
        if !pid_seen && let Some(value) = line.strip_prefix("pid =") {
            pid_seen = true;
            pid = value.trim().parse().ok();
        }
    }
    (running.unwrap_or(false), pid)
}

#[cfg(target_os = "macos")]
fn enable_impl() -> Result<()> {
    use std::process::Command;

    let exe = std::env::current_exe().context("failed to resolve current exe")?;
    let uid = macos_uid()?;
    let plist = macos_plist()?;

    macos_bootout(&uid);

    let path = std::env::var("PATH").unwrap_or_default();
    let user = std::env::var("USER").unwrap_or_default();
    let home = std::env::var("HOME").context("HOME not set")?;

    let contents = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>{MACOS_LABEL}</string>
    <key>ProgramArguments</key>
    <array>
        <string>{}</string>
        <string>run</string>
    </array>
    <key>EnvironmentVariables</key>
    <dict>
        <key>PATH</key>
        <string>{}</string>
        <key>HOME</key>
        <string>{}</string>
        <key>USER</key>
        <string>{}</string>
    </dict>
    <key>KeepAlive</key>
    <true/>
    <key>RunAtLoad</key>
    <true/>
    <key>StandardOutPath</key>
    <string>/tmp/geekmagic-custom-monitors.log</string>
    <key>StandardErrorPath</key>
    <string>/tmp/geekmagic-custom-monitors.log</string>
</dict>
</plist>
"#,
        xml_escape(&exe.display().to_string()),
        xml_escape(&path),
        xml_escape(&home),
        xml_escape(&user)
    );
    std::fs::write(&plist, contents).with_context(|| format!("failed to write {plist}"))?;
    println!("wrote {plist}");

    let status = Command::new("launchctl")
        .args(["bootstrap", &format!("gui/{uid}"), &plist])
        .status()
        .context("failed to run launchctl bootstrap")?;
    anyhow::ensure!(status.success(), "launchctl bootstrap failed");
    println!("launchctl bootstrap gui/{uid} {plist}");
    Ok(())
}

#[cfg(target_os = "macos")]
fn disable_impl() -> Result<()> {
    let uid = macos_uid()?;
    let plist = macos_plist()?;

    macos_bootout(&uid);

    if std::path::Path::new(&plist).exists() {
        std::fs::remove_file(&plist).with_context(|| format!("failed to remove {plist}"))?;
    }
    println!("launchctl bootout gui/{uid}/{MACOS_LABEL}; removed {plist}");
    Ok(())
}

#[cfg(target_os = "macos")]
fn restart_impl() -> Result<()> {
    use std::process::Command;

    let uid = macos_uid()?;
    let status = Command::new("launchctl")
        .args(["kickstart", "-k", &format!("gui/{uid}/{MACOS_LABEL}")])
        .status()
        .context("failed to run launchctl kickstart")?;
    anyhow::ensure!(
        status.success(),
        "daemon restart failed (not loaded?); run 'daemon enable' first"
    );
    println!("launchctl kickstart -k gui/{uid}/{MACOS_LABEL}");
    Ok(())
}

#[cfg(target_os = "macos")]
fn status_impl() -> Result<()> {
    use std::process::Command;

    if !std::path::Path::new(&macos_plist()?).exists() {
        println!("service: launchd {MACOS_LABEL} — disabled");
        return Ok(());
    }
    let uid = macos_uid()?;
    let output = Command::new("launchctl")
        .args(["print", &format!("gui/{uid}/{MACOS_LABEL}")])
        .output()
        .context("failed to run launchctl print")?;
    if !output.status.success() {
        println!("service: launchd {MACOS_LABEL} — enabled, not running");
        return Ok(());
    }
    let (running, pid) = parse_launchd(&String::from_utf8_lossy(&output.stdout));
    match (running, pid) {
        (true, Some(pid)) => {
            println!("service: launchd {MACOS_LABEL} — enabled, running (pid {pid})")
        }
        (true, None) => println!("service: launchd {MACOS_LABEL} — enabled, running"),
        (false, _) => println!("service: launchd {MACOS_LABEL} — enabled, not running"),
    }
    Ok(())
}

#[cfg(target_os = "linux")]
const LINUX_NAME: &str = "geekmagic-custom-monitors.service";

#[cfg(target_os = "linux")]
fn enable_impl() -> Result<()> {
    use std::process::Command;

    let home = std::env::var("HOME").context("HOME not set")?;
    let unit = format!("{home}/.config/systemd/user/{LINUX_NAME}");
    let exe = std::env::current_exe().context("failed to resolve current exe")?;

    // systemd --user services get a minimal PATH; plugins shell out (the
    // Claude Code CLI renews the OAuth token), so inherit the enabling shell's.
    let path = std::env::var("PATH").unwrap_or_default();
    let contents = format!(
        "[Unit]\nDescription=geekmagic-custom-monitors\n\n[Service]\nExecStart={} run\nEnvironment=PATH={path}\nRestart=always\n\n[Install]\nWantedBy=default.target\n",
        exe.display()
    );
    if let Some(parent) = std::path::Path::new(&unit).parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&unit, contents).with_context(|| format!("failed to write {unit}"))?;
    println!("wrote {unit}");

    for args in [
        vec!["--user", "daemon-reload"],
        vec!["--user", "enable", "--now", LINUX_NAME],
    ] {
        let status = Command::new("systemctl")
            .args(&args)
            .status()
            .context("failed to run systemctl")?;
        anyhow::ensure!(status.success(), "systemctl {:?} failed", args);
        println!("systemctl {}", args.join(" "));
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn disable_impl() -> Result<()> {
    use std::process::Command;

    let home = std::env::var("HOME").context("HOME not set")?;
    let unit = format!("{home}/.config/systemd/user/{LINUX_NAME}");

    let status = Command::new("systemctl")
        .args(["--user", "disable", "--now", LINUX_NAME])
        .status();
    println!(
        "systemctl --user disable --now {LINUX_NAME} (status: {:?})",
        status.map(|s| s.code())
    );
    if std::path::Path::new(&unit).exists() {
        std::fs::remove_file(&unit).with_context(|| format!("failed to remove {unit}"))?;
    }
    println!("removed {unit}");
    Ok(())
}

#[cfg(target_os = "linux")]
fn restart_impl() -> Result<()> {
    use std::process::Command;

    let status = Command::new("systemctl")
        .args(["--user", "restart", LINUX_NAME])
        .status()
        .context("failed to run systemctl")?;
    anyhow::ensure!(
        status.success(),
        "daemon restart failed (not enabled?); run 'daemon enable' first"
    );
    println!("systemctl --user restart {LINUX_NAME}");
    Ok(())
}

#[cfg(target_os = "linux")]
fn status_impl() -> Result<()> {
    use std::process::Command;

    let probe = |args: &[&str]| -> String {
        let out = Command::new("systemctl")
            .args(args)
            .output()
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
            .unwrap_or_default();
        if out.is_empty() {
            "unknown".to_string()
        } else {
            out
        }
    };
    let enabled = probe(&["--user", "is-enabled", LINUX_NAME]);
    let active = probe(&["--user", "is-active", LINUX_NAME]);
    println!("service: systemd {LINUX_NAME} — {enabled}, {active}");
    Ok(())
}

#[cfg(target_os = "windows")]
const RUN_VALUE: &str = "geekmagic-monitors";

#[cfg(target_os = "windows")]
fn exe_name() -> Result<String> {
    let exe = std::env::current_exe().context("failed to resolve current exe")?;
    Ok(exe
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("geekmagic-monitors.exe")
        .to_string())
}

/// Pids of running processes whose image name matches, excluding our own
/// process (`daemon status`/`restart` run from the same binary).
#[cfg(target_os = "windows")]
fn running_pids(name: &str) -> Vec<u32> {
    use std::process::Command;

    let own = std::process::id();
    let output = Command::new("tasklist")
        .args(["/FI", &format!("IMAGENAME eq {name}"), "/FO", "CSV", "/NH"])
        .output();
    let output = match output {
        Ok(o) => o,
        Err(_) => return Vec::new(),
    };
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| {
            let mut fields = line.split(',');
            fields.next()?; // image name
            fields.next()?.trim_matches('"').parse().ok()
        })
        .filter(|pid| *pid != own)
        .collect()
}

#[cfg(target_os = "windows")]
fn enable_impl() -> Result<()> {
    use winreg::RegKey;
    use winreg::enums::HKEY_CURRENT_USER;

    let exe = std::env::current_exe().context("failed to resolve current exe")?;
    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    let (key, _) = hkcu
        .create_subkey(r"Software\Microsoft\Windows\CurrentVersion\Run")
        .context("failed to open Run key")?;

    let data = format!("\"{}\" run", exe.display());
    key.set_value(RUN_VALUE, &data)
        .context("failed to set Run value")?;
    println!(r"set HKCU\Software\Microsoft\Windows\CurrentVersion\Run\{RUN_VALUE} = {data}");
    Ok(())
}

#[cfg(target_os = "windows")]
fn disable_impl() -> Result<()> {
    use winreg::RegKey;
    use winreg::enums::HKEY_CURRENT_USER;

    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    let (key, _) = hkcu
        .create_subkey(r"Software\Microsoft\Windows\CurrentVersion\Run")
        .context("failed to open Run key")?;

    let _ = key.delete_value(RUN_VALUE);
    println!(r"deleted HKCU\Software\Microsoft\Windows\CurrentVersion\Run\{RUN_VALUE}");
    Ok(())
}

#[cfg(target_os = "windows")]
fn restart_impl() -> Result<()> {
    use std::process::Command;

    let name = exe_name()?;
    for pid in running_pids(&name) {
        let _ = Command::new("taskkill")
            .args(["/PID", &pid.to_string(), "/F"])
            .status();
    }
    let exe = std::env::current_exe().context("failed to resolve current exe")?;
    let child = Command::new(&exe)
        .arg("run")
        .spawn()
        .context("failed to start daemon process")?;
    println!("restarted daemon as pid {}", child.id());
    Ok(())
}

#[cfg(target_os = "windows")]
fn status_impl() -> Result<()> {
    use winreg::RegKey;
    use winreg::enums::HKEY_CURRENT_USER;

    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    let enabled = hkcu
        .open_subkey(r"Software\Microsoft\Windows\CurrentVersion\Run")
        .and_then(|key| key.get_value::<String, _>(RUN_VALUE))
        .is_ok();
    let running = !running_pids(&exe_name()?).is_empty();
    println!(
        "service: windows Run key — {}, process {}",
        if enabled { "enabled" } else { "disabled" },
        if running { "running" } else { "not running" }
    );
    Ok(())
}

#[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
fn enable_impl() -> Result<()> {
    anyhow::bail!("daemon is not supported on this platform")
}

#[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
fn disable_impl() -> Result<()> {
    anyhow::bail!("daemon is not supported on this platform")
}

#[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
fn restart_impl() -> Result<()> {
    anyhow::bail!("daemon restart is not supported on this platform")
}

#[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
fn status_impl() -> Result<()> {
    println!("service: unsupported on this platform");
    Ok(())
}

#[cfg(all(test, target_os = "macos"))]
mod tests {
    use super::parse_launchd;

    #[test]
    fn launchd_output_parses_running_state_and_pid() {
        let output = "gui/501/apps.andersou.geekmagic-custom-monitors = {\n\
                      \tstate = running\n\
                      \tpid = 46327\n\
                      \tlast exit code = 0\n\
                      }";
        assert_eq!(parse_launchd(output), (true, Some(46327)));

        // Sub-entries repeat `state =` (e.g. "active"); the first,
        // service-level value must win.
        let subentries = "gui/501/x = {\n\
                          \tstate = running\n\
                          \tpid = 46327\n\
                          \tsub-entries = {\n\
                          \t\tstate = active\n\
                          \t}\n\
                          }";
        assert_eq!(parse_launchd(subentries), (true, Some(46327)));
    }

    #[test]
    fn launchd_output_parses_waiting_state() {
        let output = "gui/501/apps.andersou.geekmagic-custom-monitors = {\n\
                      \tstate = waiting\n\
                      \tlast exit code = 0\n\
                      }";
        assert_eq!(parse_launchd(output), (false, None));
    }
}

#[cfg(test)]
mod version_tests {
    use super::display_version;

    #[test]
    fn identifies_status_without_a_recorded_version() {
        assert_eq!(
            display_version(""),
            "unknown (status written by an older binary)"
        );
        assert_eq!(display_version("1.2.3"), "1.2.3");
    }
}
