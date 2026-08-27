//! Cross-platform auto-start at login: macOS launchd, Linux systemd user,
//! Windows Run key. The entry runs `<current_exe> run`, so the daemon interval
//! comes from the config file.

use anyhow::{Context, Result};

pub fn boot(enable: bool) -> Result<()> {
    boot_impl(enable)
}

#[cfg(target_os = "macos")]
fn boot_impl(enable: bool) -> Result<()> {
    use std::process::Command;

    const LABEL: &str = "apps.andersou.geekmagic-custom-monitors";
    let home = std::env::var("HOME").context("HOME not set")?;
    let plist = format!("{home}/Library/LaunchAgents/{LABEL}.plist");
    let exe = std::env::current_exe().context("failed to resolve current exe")?;
    let uid = String::from_utf8(Command::new("id").arg("-u").output()?.stdout)
        .context("id -u output not utf8")?
        .trim()
        .to_string();

    // Idempotent: bootout first, ignore failure (not loaded yet).
    let _ = Command::new("launchctl")
        .args(["bootout", &format!("gui/{uid}/{LABEL}")])
        .status();

    if enable {
        let contents = format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>{LABEL}</string>
    <key>ProgramArguments</key>
    <array>
        <string>{}</string>
        <string>run</string>
    </array>
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
            exe.display()
        );
        std::fs::write(&plist, contents).with_context(|| format!("failed to write {plist}"))?;
        println!("wrote {plist}");

        let status = Command::new("launchctl")
            .args(["bootstrap", &format!("gui/{uid}"), &plist])
            .status()
            .context("failed to run launchctl bootstrap")?;
        anyhow::ensure!(status.success(), "launchctl bootstrap failed");
        println!("launchctl bootstrap gui/{uid} {plist}");
    } else {
        if std::path::Path::new(&plist).exists() {
            std::fs::remove_file(&plist).with_context(|| format!("failed to remove {plist}"))?;
        }
        println!("launchctl bootout gui/{uid}/{LABEL}; removed {plist}");
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn boot_impl(enable: bool) -> Result<()> {
    use std::process::Command;

    const NAME: &str = "geekmagic-custom-monitors.service";
    let home = std::env::var("HOME").context("HOME not set")?;
    let unit = format!("{home}/.config/systemd/user/{NAME}");
    let exe = std::env::current_exe().context("failed to resolve current exe")?;

    if enable {
        let contents = format!(
            "[Unit]\nDescription=geekmagic-custom-monitors\n\n[Service]\nExecStart={} run\nRestart=always\n\n[Install]\nWantedBy=default.target\n",
            exe.display()
        );
        if let Some(parent) = std::path::Path::new(&unit).parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&unit, contents).with_context(|| format!("failed to write {unit}"))?;
        println!("wrote {unit}");

        for args in [
            vec!["--user", "daemon-reload"],
            vec!["--user", "enable", "--now", NAME],
        ] {
            let status = Command::new("systemctl")
                .args(&args)
                .status()
                .context("failed to run systemctl")?;
            anyhow::ensure!(status.success(), "systemctl {:?} failed", args);
            println!("systemctl {}", args.join(" "));
        }
    } else {
        let status = Command::new("systemctl")
            .args(["--user", "disable", "--now", NAME])
            .status();
        println!("systemctl --user disable --now {NAME} (status: {:?})", status.map(|s| s.code()));
        if std::path::Path::new(&unit).exists() {
            std::fs::remove_file(&unit).with_context(|| format!("failed to remove {unit}"))?;
        }
        println!("removed {unit}");
    }
    Ok(())
}

#[cfg(target_os = "windows")]
fn boot_impl(enable: bool) -> Result<()> {
    use winreg::enums::HKEY_CURRENT_USER;
    use winreg::RegKey;

    const VALUE: &str = "geekmagic-custom-monitors";
    let exe = std::env::current_exe().context("failed to resolve current exe")?;
    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    let (key, _) = hkcu
        .create_subkey(r"Software\Microsoft\Windows\CurrentVersion\Run")
        .context("failed to open Run key")?;

    if enable {
        let data = format!("\"{}\" run", exe.display());
        key.set_value(VALUE, &data).context("failed to set Run value")?;
        println!(r"set HKCU\Software\Microsoft\Windows\CurrentVersion\Run\{VALUE} = {data}");
    } else {
        let _ = key.delete_value(VALUE);
        println!(r"deleted HKCU\Software\Microsoft\Windows\CurrentVersion\Run\{VALUE}");
    }
    Ok(())
}

#[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
fn boot_impl(_enable: bool) -> Result<()> {
    anyhow::bail!("boot is not supported on this platform")
}
