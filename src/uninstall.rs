use std::path::Path;

use anyhow::{Context, Result};

pub fn run() -> Result<()> {
    let executable = std::env::current_exe().context("failed to resolve current executable")?;
    let config_root = crate::config::config_root()?;

    if let Err(error) = crate::daemon::disable() {
        eprintln!("warning: failed to disable automatic startup: {error:#}");
    }

    remove_config_root(&config_root)?;
    remove_executable(&executable)?;

    println!("removed {}", config_root.display());
    #[cfg(target_os = "windows")]
    println!("scheduled removal of {}", executable.display());
    #[cfg(not(target_os = "windows"))]
    println!("removed {}", executable.display());
    Ok(())
}

fn remove_config_root(path: &Path) -> Result<()> {
    if path.exists() {
        std::fs::remove_dir_all(path)
            .with_context(|| format!("failed to remove config root {}", path.display()))?;
    }
    Ok(())
}

fn remove_file_now(path: &Path) -> Result<()> {
    if path.exists() {
        std::fs::remove_file(path)
            .with_context(|| format!("failed to remove executable {}", path.display()))?;
    }
    Ok(())
}

#[cfg(not(target_os = "windows"))]
fn remove_executable(path: &Path) -> Result<()> {
    remove_file_now(path)
}

#[cfg(target_os = "windows")]
fn remove_executable(path: &Path) -> Result<()> {
    use std::process::Command;

    let escaped_path = path.display().to_string().replace('\'', "''");
    let script = format!(
        "Wait-Process -Id {}; Remove-Item -LiteralPath '{}' -Force",
        std::process::id(),
        escaped_path
    );
    Command::new("powershell.exe")
        .args([
            "-NoProfile",
            "-NonInteractive",
            "-WindowStyle",
            "Hidden",
            "-Command",
            &script,
        ])
        .spawn()
        .context("failed to schedule executable removal")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{remove_config_root, remove_file_now};

    #[test]
    fn removes_config_tree_and_executable() {
        let root =
            std::env::temp_dir().join(format!("geekmagic-uninstall-test-{}", std::process::id()));
        let config_root = root.join(".config/geekmagic-custom-monitors");
        let executable = root.join("bin/geekmagic-monitors");
        let _ = std::fs::remove_dir_all(&root);

        std::fs::create_dir_all(config_root.join("backups/20260827-120000")).unwrap();
        std::fs::write(config_root.join("config.toml"), b"host = 'test'").unwrap();
        std::fs::write(
            config_root.join("backups/20260827-120000/image.jpg"),
            b"backup",
        )
        .unwrap();
        std::fs::create_dir_all(executable.parent().unwrap()).unwrap();
        std::fs::write(&executable, b"binary").unwrap();

        remove_config_root(&config_root).unwrap();
        remove_file_now(&executable).unwrap();

        assert!(!config_root.exists());
        assert!(!executable.exists());
        std::fs::remove_dir_all(root).unwrap();
    }
}
