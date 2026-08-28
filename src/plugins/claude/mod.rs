use anyhow::{Context, Result};
#[cfg(not(windows))]
use claude_code_stats::source::{SourceError, UsageSource, oauth::OauthSource};
#[cfg(not(windows))]
use claude_code_stats::source::{fetch_with_failover, web::WebSource};
#[cfg(not(windows))]
use claude_code_stats::types::to_usage_window;
use image::{ImageFormat, RgbaImage};
use serde::Deserialize;

use crate::plugin::{Plugin, PluginKind, UiPlugin};
use crate::plugins::agents_usage_ui::{self, UsageWindowData};

const ICON_BYTES: &[u8] = include_bytes!("icon.png");

fn icon() -> &'static RgbaImage {
    static ICON: std::sync::LazyLock<RgbaImage> = std::sync::LazyLock::new(|| {
        image::load_from_memory_with_format(ICON_BYTES, ImageFormat::Png)
            .expect("embedded Claude icon must be valid PNG")
            .into_rgba8()
    });
    &ICON
}

const MAX_CLI_REFRESH_RETRIES: u32 = 2;

#[derive(Debug, Deserialize)]
pub struct ActiveData {
    pub five_hour: Option<UsageWindow>,
    pub seven_day: Option<UsageWindow>,
    pub updated_at: Option<String>,
}

#[derive(Debug, Deserialize, Clone)]
pub struct UsageWindow {
    pub utilization: f64,
    pub resets_in_minutes: Option<f64>,
    pub usage_level: String,
    pub pace: Option<PaceInfo>,
}

#[derive(Debug, Deserialize, Clone)]
pub struct PaceInfo {
    pub delta_percent: f64,
    pub expected_percent: f64,
    pub will_last_to_reset: bool,
    pub eta_minutes: Option<f64>,
}

/// Fill in pace data for windows the API left blank.
#[cfg(not(windows))]
fn ensure_pace(window: &mut UsageWindow, window_minutes: f64) {
    if window.pace.is_some() {
        return;
    }
    let Some(resets_in) = window.resets_in_minutes else {
        return;
    };
    window.pace =
        agents_usage_ui::pace(window.utilization, resets_in, window_minutes).map(|pace| PaceInfo {
            delta_percent: pace.delta_percent,
            expected_percent: pace.expected_percent,
            will_last_to_reset: pace.will_last_to_reset,
            eta_minutes: pace.eta_minutes,
        });
}

#[cfg(not(windows))]
pub fn fetch_stats() -> Result<ActiveData> {
    // Bypass the crate's collect_widget_payload_json: its private cache layer
    // hardcodes CliProbeSource, whose PTY probe double-closes the master fd and
    // aborts the whole process (observed: "IO Safety violation: owned file
    // descriptor already closed"). Fetch via the public failover with the OAuth
    // and Web sources only; the skipped 4-minute cache never hits at our >=300s
    // daemon intervals anyway.
    let now = chrono::Utc::now();
    let sources: Vec<&dyn UsageSource> = vec![&OauthSource, &WebSource];
    let (usage, _source) = fetch_with_failover(&sources)?;
    let mut data: ActiveData = serde_json::from_value(serde_json::json!({
        "five_hour": usage.five_hour.as_ref().map(|w| to_usage_window(w, now, Some(300.0))),
        "seven_day": usage.seven_day.as_ref().map(|w| to_usage_window(w, now, Some(10080.0))),
        "updated_at": now.to_rfc3339(),
    }))
    .context("failed to map claude-code-stats usage")?;

    // Compute pace locally if not provided.
    if let Some(window) = &mut data.five_hour {
        ensure_pace(window, 300.0); // 5 hours
    }
    if let Some(window) = &mut data.seven_day {
        ensure_pace(window, 10080.0); // 7 days
    }
    Ok(data)
}

#[cfg(windows)]
pub fn fetch_stats() -> Result<ActiveData> {
    anyhow::bail!(
        "the Claude usage plugin is unavailable on Windows because claude-code-stats does not support Windows"
    )
}

/// Query only the OAuth source so a later web/CLI fallback cannot overwrite an
/// OAuth 401 in claude-code-stats' aggregate error payload.
#[cfg(not(windows))]
fn oauth_is_unauthorized() -> bool {
    matches!(
        OauthSource.try_fetch(),
        Err(SourceError::Failed(error)) if error.to_string().contains("status 401")
    )
}

#[cfg(windows)]
fn oauth_is_unauthorized() -> bool {
    false
}

/// Ask Claude Code to attempt delegated OAuth renewal. The upstream crate uses
/// the same invocation but describes renewal as potential, not guaranteed.
fn cli_refresh() -> Result<()> {
    let output = std::process::Command::new("claude")
        .arg("--version")
        .output()
        .context("failed to run Claude Code CLI for OAuth renewal")?;
    if !output.status.success() {
        anyhow::bail!(
            "Claude Code CLI OAuth renewal exited with {}",
            output.status
        );
    }
    Ok(())
}

fn fetch_with_cli_refresh<F, U, R>(
    mut fetch: F,
    mut unauthorized: U,
    mut refresh: R,
) -> Result<ActiveData>
where
    F: FnMut() -> Result<ActiveData>,
    U: FnMut() -> bool,
    R: FnMut() -> Result<()>,
{
    let mut retries = 0;
    loop {
        match fetch() {
            Ok(data) => return Ok(data),
            Err(error) => {
                if retries == MAX_CLI_REFRESH_RETRIES || !unauthorized() {
                    return Err(error);
                }
                refresh()?;
                retries += 1;
            }
        }
    }
}

pub struct Claude {
    data: Option<ActiveData>,
}

impl Claude {
    pub fn new() -> Self {
        Self { data: None }
    }
}

impl Plugin for Claude {
    fn name(&self) -> &'static str {
        "claude"
    }

    fn get_plugin_kind(&self) -> PluginKind {
        PluginKind::Ui
    }
}

impl UiPlugin for Claude {

    fn collect(&mut self) -> Result<()> {
        self.data = Some(fetch_with_cli_refresh(
            fetch_stats,
            oauth_is_unauthorized,
            cli_refresh,
        )?);
        Ok(())
    }

    fn render(&self) -> Result<RgbaImage> {
        let data = self.data.as_ref().context("collect() has not run")?;

        let mut windows = Vec::new();
        if let Some(window) = &data.five_hour {
            windows.push(window_data("Session", window));
        }
        if let Some(window) = &data.seven_day {
            windows.push(window_data("Weekly", window));
        }

        agents_usage_ui::render_usage_bars(
            "Claude Code",
            icon(),
            &windows,
            data.updated_at.as_deref(),
        )
    }
}

fn window_data(label: &str, window: &UsageWindow) -> UsageWindowData {
    UsageWindowData {
        label: label.to_string(),
        utilization: window.utilization,
        usage_level: window.usage_level.clone(),
        expected_percent: window.pace.as_ref().map(|pace| pace.expected_percent),
        delta_percent: window.pace.as_ref().map(|pace| pace.delta_percent),
        resets_in_minutes: window.resets_in_minutes,
        will_last_to_reset: window.pace.as_ref().map(|pace| pace.will_last_to_reset),
        eta_minutes: window.pace.as_ref().and_then(|pace| pace.eta_minutes),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::anyhow;

    fn active_data() -> ActiveData {
        ActiveData {
            five_hour: None,
            seven_day: None,
            updated_at: None,
        }
    }

    #[test]
    fn embedded_icon_is_valid() {
        let icon = icon();
        assert_eq!(icon.dimensions(), (18, 18));
        assert!(icon.pixels().any(|pixel| pixel[3] != 0));
    }

    #[test]
    fn cli_retry_refreshes_after_oauth_401_and_succeeds() {
        let mut fetches = 0;
        let mut probes = 0;
        let mut refreshes = 0;
        let result = fetch_with_cli_refresh(
            || {
                fetches += 1;
                if fetches == 1 {
                    Err(anyhow!("aggregate failure"))
                } else {
                    Ok(active_data())
                }
            },
            || {
                probes += 1;
                true
            },
            || {
                refreshes += 1;
                Ok(())
            },
        );
        assert!(result.is_ok());
        assert_eq!((fetches, probes, refreshes), (2, 1, 1));
    }

    #[test]
    fn cli_retry_stops_after_two_refreshes() {
        let mut fetches = 0;
        let mut probes = 0;
        let mut refreshes = 0;
        let error = fetch_with_cli_refresh(
            || {
                fetches += 1;
                Err(anyhow!("aggregate failure"))
            },
            || {
                probes += 1;
                true
            },
            || {
                refreshes += 1;
                Ok(())
            },
        )
        .unwrap_err();
        assert_eq!(error.to_string(), "aggregate failure");
        assert_eq!((fetches, probes, refreshes), (3, 2, 2));
    }

    #[test]
    fn non_401_does_not_invoke_cli_refresh() {
        let mut probes = 0;
        let mut refreshes = 0;
        let error = fetch_with_cli_refresh(
            || Err(anyhow!("network unavailable")),
            || {
                probes += 1;
                false
            },
            || {
                refreshes += 1;
                Ok(())
            },
        )
        .unwrap_err();
        assert_eq!(error.to_string(), "network unavailable");
        assert_eq!((probes, refreshes), (1, 0));
    }
}
