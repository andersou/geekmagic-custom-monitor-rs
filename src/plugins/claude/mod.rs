use anyhow::{Context, Result};
#[cfg(not(windows))]
use claude_code_stats::source::{SourceError, UsageSource, oauth::OauthSource, web::WebSource};
#[cfg(not(windows))]
use claude_code_stats::types::to_usage_window;
use image::{ImageFormat, RgbaImage};
use serde::Deserialize;
use std::path::PathBuf;
use std::time::{Duration, Instant};

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

/// The usage endpoint answers 429 for a long while once it is tripped, and each
/// daemon cycle would otherwise keep the limit alive. Stop calling it until the
/// cooldown expires.
const RATE_LIMIT_COOLDOWN: Duration = Duration::from_secs(15 * 60);

/// Absolute candidates for the Claude Code CLI, tried when PATH lookup fails.
/// Daemons started by launchd or systemd inherit a minimal PATH
/// (`/usr/bin:/bin:/usr/sbin:/sbin` under launchd), so the CLI that renews the
/// OAuth token is invisible to them even though an interactive run finds it.
const CLI_FALLBACK_HOME_RELATIVE: &[&str] = &[
    ".claude/local/claude",
    ".local/bin/claude",
    ".bun/bin/claude",
];
const CLI_FALLBACK_ABSOLUTE: &[&str] = &["/opt/homebrew/bin/claude", "/usr/local/bin/claude"];

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

/// A failed usage fetch plus the HTTP status that caused it, so the caller can
/// tell "token expired" (renew and retry) from "rate limited" (back off).
pub struct FetchError {
    pub error: anyhow::Error,
    pub status: Option<u16>,
}

impl FetchError {
    fn new(error: anyhow::Error) -> Self {
        let status = http_status(&error);
        Self { error, status }
    }
}

/// Pull the HTTP status out of claude-code-stats' error text
/// ("API request failed with status 401 Unauthorized: ...").
fn http_status(error: &anyhow::Error) -> Option<u16> {
    let text = error.to_string();
    let rest = text.split("status ").nth(1)?;
    rest.split(|c: char| !c.is_ascii_digit())
        .find(|part| !part.is_empty())?
        .parse()
        .ok()
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
fn source_error(name: &str, error: SourceError) -> Option<anyhow::Error> {
    match error {
        SourceError::NotAvailable(reason) => {
            eprintln!("claude: {name} skipped: {reason}");
            None
        }
        SourceError::Failed(error) => {
            eprintln!("claude: {name} failed: {error}");
            Some(error)
        }
    }
}

#[cfg(not(windows))]
pub fn fetch_stats() -> Result<ActiveData, FetchError> {
    // Bypass the crate's collect_widget_payload_json: its private cache layer
    // hardcodes CliProbeSource, whose PTY probe double-closes the master fd and
    // aborts the whole process (observed: "IO Safety violation: owned file
    // descriptor already closed"). Fetch via the public sources only; the
    // skipped 4-minute cache never hits at our >=300s daemon intervals anyway.
    //
    // The failover is open-coded instead of using fetch_with_failover because
    // that helper only returns the *last* failing source's error, which hides
    // an OAuth 401 behind a later web failure. Detecting the 401 from a second
    // probe request (the previous approach) doubled the request rate against an
    // endpoint that answers 429 when hammered.
    let now = chrono::Utc::now();
    let usage = match OauthSource.try_fetch() {
        Ok(usage) => usage,
        Err(oauth_error) => {
            let oauth_error = source_error("oauth_api", oauth_error);
            match WebSource.try_fetch() {
                Ok(usage) => usage,
                Err(web_error) => {
                    let web_error = source_error("web_api", web_error);
                    return Err(FetchError::new(oauth_error.or(web_error).unwrap_or_else(
                        || anyhow::anyhow!("no Claude usage source available"),
                    )));
                }
            }
        }
    };

    let mut data: ActiveData = serde_json::from_value(serde_json::json!({
        "five_hour": usage.five_hour.as_ref().map(|w| to_usage_window(w, now, Some(300.0))),
        "seven_day": usage.seven_day.as_ref().map(|w| to_usage_window(w, now, Some(10080.0))),
        "updated_at": now.to_rfc3339(),
    }))
    .context("failed to map claude-code-stats usage")
    .map_err(FetchError::new)?;

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
pub fn fetch_stats() -> Result<ActiveData, FetchError> {
    Err(FetchError::new(anyhow::anyhow!(
        "the Claude usage plugin is unavailable on Windows because claude-code-stats does not support Windows"
    )))
}

fn path_candidates() -> Vec<PathBuf> {
    let mut candidates = Vec::new();
    if let Some(path) = std::env::var_os("PATH") {
        candidates.extend(std::env::split_paths(&path).map(|dir| dir.join("claude")));
    }
    if let Some(home) = std::env::var_os("HOME").map(PathBuf::from) {
        candidates.extend(
            CLI_FALLBACK_HOME_RELATIVE
                .iter()
                .map(|relative| home.join(relative)),
        );
    }
    candidates.extend(CLI_FALLBACK_ABSOLUTE.iter().map(PathBuf::from));
    candidates
}

fn resolve_binary<I>(candidates: I) -> Option<PathBuf>
where
    I: IntoIterator<Item = PathBuf>,
{
    candidates.into_iter().find(|candidate| candidate.is_file())
}

/// Ask Claude Code to attempt delegated OAuth renewal. The upstream crate uses
/// the same invocation but describes renewal as potential, not guaranteed.
fn cli_refresh() -> Result<()> {
    let binary = resolve_binary(path_candidates()).ok_or_else(|| {
        anyhow::anyhow!(
            "Claude Code CLI not found for OAuth renewal (PATH={})",
            std::env::var("PATH").unwrap_or_default()
        )
    })?;
    let output = std::process::Command::new(&binary)
        .arg("--version")
        .output()
        .with_context(|| format!("failed to run {} for OAuth renewal", binary.display()))?;
    if !output.status.success() {
        anyhow::bail!(
            "Claude Code CLI OAuth renewal exited with {}",
            output.status
        );
    }
    Ok(())
}

fn fetch_with_cli_refresh<F, R>(mut fetch: F, mut refresh: R) -> Result<ActiveData, FetchError>
where
    F: FnMut() -> Result<ActiveData, FetchError>,
    R: FnMut() -> Result<()>,
{
    let mut retries = 0;
    loop {
        match fetch() {
            Ok(data) => return Ok(data),
            Err(failure) => {
                if retries == MAX_CLI_REFRESH_RETRIES || failure.status != Some(401) {
                    return Err(failure);
                }
                if let Err(error) = refresh() {
                    return Err(FetchError {
                        error: error
                            .context("Claude OAuth token rejected (401) and renewal failed"),
                        status: failure.status,
                    });
                }
                retries += 1;
            }
        }
    }
}

/// Remaining cooldown, or None once the deadline passed.
fn cooldown_remaining(until: Option<Instant>, now: Instant) -> Option<Duration> {
    until
        .and_then(|until| until.checked_duration_since(now))
        .filter(|remaining| !remaining.is_zero())
}

pub struct Claude {
    data: Option<ActiveData>,
    rate_limited_until: Option<Instant>,
}

impl Claude {
    pub fn new() -> Self {
        Self {
            data: None,
            rate_limited_until: None,
        }
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
        if let Some(remaining) = cooldown_remaining(self.rate_limited_until, Instant::now()) {
            anyhow::bail!(
                "Claude usage API rate limited (429); next attempt in {}s",
                remaining.as_secs()
            );
        }
        self.rate_limited_until = None;

        match fetch_with_cli_refresh(fetch_stats, cli_refresh) {
            Ok(data) => {
                self.data = Some(data);
                Ok(())
            }
            Err(failure) => {
                if failure.status == Some(429) {
                    self.rate_limited_until = Some(Instant::now() + RATE_LIMIT_COOLDOWN);
                }
                Err(failure.error)
            }
        }
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

    fn failure(message: &str) -> FetchError {
        FetchError::new(anyhow!("{message}"))
    }

    #[test]
    fn embedded_icon_is_valid() {
        let icon = icon();
        assert_eq!(icon.dimensions(), (18, 18));
        assert!(icon.pixels().any(|pixel| pixel[3] != 0));
    }

    #[test]
    fn http_status_is_parsed_from_upstream_error_text() {
        assert_eq!(
            http_status(&anyhow!(
                "API request failed with status 401 Unauthorized: {{\"type\":\"error\"}}"
            )),
            Some(401)
        );
        assert_eq!(
            http_status(&anyhow!(
                "API request failed with status 429 Too Many Requests"
            )),
            Some(429)
        );
        assert_eq!(http_status(&anyhow!("network unavailable")), None);
    }

    #[test]
    fn cli_retry_refreshes_after_oauth_401_and_succeeds() {
        let mut fetches = 0;
        let mut refreshes = 0;
        let result = fetch_with_cli_refresh(
            || {
                fetches += 1;
                if fetches == 1 {
                    Err(failure("API request failed with status 401 Unauthorized"))
                } else {
                    Ok(active_data())
                }
            },
            || {
                refreshes += 1;
                Ok(())
            },
        );
        assert!(result.is_ok());
        assert_eq!((fetches, refreshes), (2, 1));
    }

    #[test]
    fn cli_retry_stops_after_two_refreshes() {
        let mut fetches = 0;
        let mut refreshes = 0;
        let error = fetch_with_cli_refresh(
            || {
                fetches += 1;
                Err(failure("API request failed with status 401 Unauthorized"))
            },
            || {
                refreshes += 1;
                Ok(())
            },
        )
        .unwrap_err();
        assert_eq!(error.status, Some(401));
        assert_eq!((fetches, refreshes), (3, 2));
    }

    #[test]
    fn rate_limit_does_not_invoke_cli_refresh() {
        let mut fetches = 0;
        let mut refreshes = 0;
        let error = fetch_with_cli_refresh(
            || {
                fetches += 1;
                Err(failure(
                    "API request failed with status 429 Too Many Requests",
                ))
            },
            || {
                refreshes += 1;
                Ok(())
            },
        )
        .unwrap_err();
        assert_eq!(error.status, Some(429));
        assert_eq!((fetches, refreshes), (1, 0));
    }

    #[test]
    fn refresh_failure_is_reported_with_the_401_context() {
        let error = fetch_with_cli_refresh(
            || Err(failure("API request failed with status 401 Unauthorized")),
            || Err(anyhow!("Claude Code CLI not found for OAuth renewal")),
        )
        .unwrap_err();
        let text = format!("{:#}", error.error);
        assert!(text.contains("renewal failed"), "{text}");
        assert!(text.contains("CLI not found"), "{text}");
    }

    #[test]
    fn rate_limit_cooldown_blocks_then_expires() {
        let now = Instant::now();
        let until = now + Duration::from_secs(60);
        assert_eq!(
            cooldown_remaining(Some(until), now).map(|d| d.as_secs()),
            Some(60)
        );
        assert!(cooldown_remaining(Some(until), until).is_none());
        assert!(cooldown_remaining(None, now).is_none());
    }

    #[test]
    fn cli_is_resolved_from_absolute_fallbacks_when_path_misses() {
        let missing = PathBuf::from("/nonexistent/bin/claude");
        assert!(resolve_binary(vec![missing.clone()]).is_none());
        assert_eq!(
            resolve_binary(vec![missing, PathBuf::from("/bin/sh")]),
            Some(PathBuf::from("/bin/sh"))
        );
    }

    #[test]
    fn cli_candidates_cover_path_and_fallbacks() {
        let candidates = path_candidates();
        assert!(
            candidates
                .iter()
                .all(|candidate| candidate.ends_with("claude"))
        );
        for absolute in CLI_FALLBACK_ABSOLUTE {
            assert!(candidates.contains(&PathBuf::from(absolute)));
        }
    }
}
