//! Last-cycle outcome written by `run` every cycle and read by `daemon status`.
//! Cross-platform substitute for log scraping (the macOS log lives in
//! ~/Library/Logs; Linux logs land in journald; Windows has none).

use std::path::PathBuf;

use anyhow::{Context, Result};
use log::warn;
use serde::{Deserialize, Serialize};

#[derive(Debug, Serialize, Deserialize)]
pub struct PluginFailure {
    pub plugin: String,
    pub error: String,
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct DaemonStatus {
    #[serde(default)]
    pub version: String,
    pub pid: u32,
    pub started_at: String,         // RFC 3339, local time
    pub interval_secs: Option<u64>, // None = one-shot run
    pub plugins: Vec<String>,       // enabled plugin names
    pub device: Option<String>,     // firmware string or detection note
    pub last_cycle_at: Option<String>,
    pub succeeded: Vec<String>, // plugin names that produced a real screen
    pub failed: Vec<PluginFailure>, // plugin name + "{error:#}"
    pub upload: Option<String>, // "pushed N screen(s) to HOST" | "saved N screen(s) to DIR" | "skipped: all plugins failed" | "failed: ..."
    pub cycle_error: Option<String>, // top-level run_cycle error, "{e:#}"
}

pub fn path() -> Result<PathBuf> {
    Ok(crate::config::config_root()?.join("status.json"))
}

/// Best-effort persistence; a failed write must never fail the cycle.
pub fn write(status: &DaemonStatus) {
    let result = (|| -> Result<()> {
        let path = path()?;
        let body = serde_json::to_string_pretty(status).context("failed to serialize status")?;
        std::fs::write(&path, body)
            .with_context(|| format!("failed to write {}", path.display()))?;
        Ok(())
    })();
    if let Err(error) = result {
        warn!("could not persist daemon status: {error:#}");
    }
}

/// None when the file is missing or corrupt.
pub fn read() -> Option<DaemonStatus> {
    let body = std::fs::read_to_string(path().ok()?).ok()?;
    serde_json::from_str(&body).ok()
}

#[cfg(test)]
mod tests {
    use super::DaemonStatus;

    #[test]
    fn reads_status_written_before_version_was_tracked() {
        let status: DaemonStatus = serde_json::from_str(
            r#"{
                "pid": 1234,
                "started_at": "2026-08-28T12:00:00-03:00",
                "interval_secs": 300,
                "plugins": [],
                "device": null,
                "last_cycle_at": null,
                "succeeded": [],
                "failed": [],
                "upload": null,
                "cycle_error": null
            }"#,
        )
        .unwrap();

        assert!(status.version.is_empty());
    }
}
