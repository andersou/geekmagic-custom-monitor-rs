#[cfg(not(windows))]
mod auth;

use anyhow::{Context, Result};
use image::{ImageFormat, RgbaImage};
use serde::Deserialize;
use std::time::{Duration, Instant};

use crate::plugin::{Plugin, PluginKind, UiPlugin};
use crate::plugins::agents_usage_ui::{self, UsageWindowData};

const ICON_BYTES: &[u8] = include_bytes!("icon.png");
const RATE_LIMIT_COOLDOWN: Duration = Duration::from_secs(15 * 60);

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

/// A failed usage fetch plus the typed HTTP status that caused it.
#[derive(Debug)]
pub struct FetchError {
    pub error: anyhow::Error,
    pub status: Option<reqwest::StatusCode>,
}

impl FetchError {
    fn other(error: anyhow::Error) -> Self {
        Self {
            error,
            status: None,
        }
    }

    #[cfg(not(windows))]
    fn status(status: reqwest::StatusCode, body: String) -> Self {
        Self {
            error: anyhow::anyhow!("Claude usage API returned {status}: {body}"),
            status: Some(status),
        }
    }
}

fn icon() -> &'static RgbaImage {
    static ICON: std::sync::LazyLock<RgbaImage> = std::sync::LazyLock::new(|| {
        image::load_from_memory_with_format(ICON_BYTES, ImageFormat::Png)
            .expect("embedded Claude icon must be valid PNG")
            .into_rgba8()
    });
    &ICON
}

/// Fill in pace data for windows the API left blank.
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
#[derive(Deserialize)]
struct UsageResponse {
    five_hour: Option<ApiUsageWindow>,
    seven_day: Option<ApiUsageWindow>,
}

#[cfg(not(windows))]
#[derive(Deserialize)]
struct ApiUsageWindow {
    utilization: f64,
    resets_at: Option<chrono::DateTime<chrono::Utc>>,
}

#[cfg(not(windows))]
fn usage_level(utilization: f64) -> &'static str {
    match utilization {
        value if value >= 100.0 => "over",
        value if value >= 80.0 => "danger",
        value if value >= 60.0 => "warn",
        _ => "normal",
    }
}

#[cfg(not(windows))]
fn map_window(window: ApiUsageWindow, now: chrono::DateTime<chrono::Utc>) -> UsageWindow {
    let resets_in_minutes = window
        .resets_at
        .map(|resets_at| (resets_at - now).num_seconds().max(0) as f64 / 60.0);
    UsageWindow {
        utilization: window.utilization,
        resets_in_minutes,
        usage_level: usage_level(window.utilization).to_owned(),
        pace: None,
    }
}

#[cfg(not(windows))]
fn fetch_usage(access_token: &str) -> std::result::Result<UsageResponse, FetchError> {
    const USAGE_URL: &str = "https://api.anthropic.com/api/oauth/usage";
    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(30))
        .build()
        .map_err(|error| {
            FetchError::other(
                anyhow::Error::new(error).context("failed to build Claude usage client"),
            )
        })?;
    let response = client
        .get(USAGE_URL)
        .header("Authorization", format!("Bearer {access_token}"))
        .header("anthropic-version", "2023-06-01")
        .header(
            "anthropic-beta",
            "oauth-2025-04-20,fine-grained-tool-streaming-2025-05-14",
        )
        .header("anthropic-dangerous-direct-browser-access", "true")
        .send()
        .map_err(|error| {
            FetchError::other(anyhow::Error::new(error).context("failed to reach Claude usage API"))
        })?;
    let status = response.status();
    let body = response.text().map_err(|error| {
        FetchError::other(anyhow::Error::new(error).context("failed to read Claude usage response"))
    })?;
    if !status.is_success() {
        return Err(FetchError::status(status, body));
    }
    serde_json::from_str(&body).map_err(|error| {
        FetchError::other(
            anyhow::Error::new(error).context("failed to parse Claude usage response"),
        )
    })
}

#[cfg(not(windows))]
fn fetch_stats_with<F, R>(
    mut credentials: auth::Credentials,
    mut fetch: F,
    mut refresh: R,
) -> Result<ActiveData, FetchError>
where
    F: FnMut(&str) -> std::result::Result<UsageResponse, FetchError>,
    R: FnMut(&mut auth::Credentials) -> Result<()>,
{
    let mut refreshed = false;
    if credentials.expires_within_refresh_margin(chrono::Utc::now()) {
        refresh(&mut credentials).map_err(|error| {
            FetchError::other(error.context(
                "Claude OAuth token is expired or expires within 60 seconds; refresh failed",
            ))
        })?;
        refreshed = true;
    }

    let usage = match fetch(&credentials.access_token) {
        Ok(usage) => usage,
        Err(failure) if failure.status == Some(reqwest::StatusCode::UNAUTHORIZED) && !refreshed => {
            refresh(&mut credentials).map_err(|error| FetchError {
                error: error.context("Claude OAuth token was rejected (401) and refresh failed"),
                status: failure.status,
            })?;
            fetch(&credentials.access_token)?
        }
        Err(failure) => return Err(failure),
    };

    let now = chrono::Utc::now();
    let mut data = ActiveData {
        five_hour: usage.five_hour.map(|window| map_window(window, now)),
        seven_day: usage.seven_day.map(|window| map_window(window, now)),
        updated_at: Some(now.to_rfc3339()),
    };
    if let Some(window) = &mut data.five_hour {
        ensure_pace(window, 300.0);
    }
    if let Some(window) = &mut data.seven_day {
        ensure_pace(window, 10_080.0);
    }
    Ok(data)
}

#[cfg(not(windows))]
pub fn fetch_stats() -> Result<ActiveData, FetchError> {
    let credentials = auth::load_credentials().map_err(|error| {
        FetchError::other(error.context("failed to load Claude OAuth credentials"))
    })?;
    fetch_stats_with(credentials, fetch_usage, auth::refresh)
}

#[cfg(windows)]
pub fn fetch_stats() -> Result<ActiveData, FetchError> {
    Err(FetchError::other(anyhow::anyhow!(
        "the Claude usage plugin is unavailable on Windows"
    )))
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

        match fetch_stats() {
            Ok(data) => {
                self.data = Some(data);
                Ok(())
            }
            Err(failure) => {
                if failure.status == Some(reqwest::StatusCode::TOO_MANY_REQUESTS) {
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

#[cfg(all(test, not(windows)))]
mod tests {
    use super::*;
    use anyhow::anyhow;
    use chrono::Duration as ChronoDuration;
    use std::cell::RefCell;

    fn usage() -> UsageResponse {
        UsageResponse {
            five_hour: None,
            seven_day: None,
        }
    }

    fn credentials(expired: bool) -> auth::Credentials {
        auth::Credentials::for_test(if expired {
            chrono::Utc::now() - ChronoDuration::seconds(1)
        } else {
            chrono::Utc::now() + ChronoDuration::hours(1)
        })
    }

    fn failure(status: reqwest::StatusCode) -> FetchError {
        FetchError::status(status, String::new())
    }

    #[test]
    fn embedded_icon_is_valid() {
        let icon = icon();
        assert_eq!(icon.dimensions(), (18, 18));
        assert!(icon.pixels().any(|pixel| pixel[3] != 0));
    }

    #[test]
    fn usage_level_uses_api_buckets() {
        assert_eq!(usage_level(0.0), "normal");
        assert_eq!(usage_level(59.9), "normal");
        assert_eq!(usage_level(60.0), "warn");
        assert_eq!(usage_level(80.0), "danger");
        assert_eq!(usage_level(100.0), "over");
    }

    #[test]
    fn map_window_calculates_countdown_and_clamps_it() {
        let now = chrono::Utc::now();
        let future = map_window(
            ApiUsageWindow {
                utilization: 61.0,
                resets_at: Some(now + ChronoDuration::minutes(5)),
            },
            now,
        );
        assert_eq!(future.resets_in_minutes, Some(5.0));
        assert_eq!(future.usage_level, "warn");
        let past = map_window(
            ApiUsageWindow {
                utilization: 10.0,
                resets_at: Some(now - ChronoDuration::minutes(1)),
            },
            now,
        );
        assert_eq!(past.resets_in_minutes, Some(0.0));
    }

    #[test]
    fn proactive_refresh_happens_before_the_first_fetch() {
        let calls = RefCell::new(Vec::new());
        fetch_stats_with(
            credentials(true),
            |token| {
                calls.borrow_mut().push(format!("fetch:{token}"));
                Ok(usage())
            },
            |credentials| {
                calls.borrow_mut().push("refresh".to_owned());
                credentials.set_access_token_for_test("fresh");
                Ok(())
            },
        )
        .unwrap();
        assert_eq!(calls.into_inner(), ["refresh", "fetch:fresh"]);
    }

    #[test]
    fn unauthorized_response_refreshes_once_with_a_fresh_token() {
        let mut fetches = 0;
        let mut refreshes = 0;
        fetch_stats_with(
            credentials(false),
            |token| {
                fetches += 1;
                if token == "stale" {
                    Err(failure(reqwest::StatusCode::UNAUTHORIZED))
                } else {
                    Ok(usage())
                }
            },
            |credentials| {
                refreshes += 1;
                credentials.set_access_token_for_test("fresh");
                Ok(())
            },
        )
        .unwrap();
        assert_eq!((fetches, refreshes), (2, 1));
    }

    #[test]
    fn proactive_refresh_does_not_retry_a_second_unauthorized_response() {
        let mut fetches = 0;
        let mut refreshes = 0;
        let error = fetch_stats_with(
            credentials(true),
            |_| {
                fetches += 1;
                Err(failure(reqwest::StatusCode::UNAUTHORIZED))
            },
            |_| {
                refreshes += 1;
                Ok(())
            },
        )
        .unwrap_err();
        assert_eq!(error.status, Some(reqwest::StatusCode::UNAUTHORIZED));
        assert_eq!((fetches, refreshes), (1, 1));
    }

    #[test]
    fn rate_limit_does_not_refresh() {
        let mut refreshes = 0;
        let error = fetch_stats_with(
            credentials(false),
            |_| Err(failure(reqwest::StatusCode::TOO_MANY_REQUESTS)),
            |_| {
                refreshes += 1;
                Ok(())
            },
        )
        .unwrap_err();
        assert_eq!(error.status, Some(reqwest::StatusCode::TOO_MANY_REQUESTS));
        assert_eq!(refreshes, 0);
    }

    #[test]
    fn proactive_refresh_failure_has_context() {
        let error = fetch_stats_with(
            credentials(true),
            |_| Ok(usage()),
            |_| Err(anyhow!("write-back failed")),
        )
        .unwrap_err();
        assert!(format!("{:#}", error.error).contains("expires within 60 seconds"));
    }

    #[test]
    fn rate_limit_cooldown_blocks_then_expires() {
        let now = Instant::now();
        let until = now + Duration::from_secs(60);
        assert_eq!(
            cooldown_remaining(Some(until), now).map(|duration| duration.as_secs()),
            Some(60)
        );
        assert!(cooldown_remaining(Some(until), until).is_none());
        assert!(cooldown_remaining(None, now).is_none());
    }
}
