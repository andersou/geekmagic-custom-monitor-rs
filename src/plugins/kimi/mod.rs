//! Kimi Code plan quota. Same two-window shape as the Claude screen, so it
//! renders through the shared `agents-usage-ui` renderer plugin.
//!
//! `GET /coding/v1/usages` reports the weekly membership pool in `usage` and
//! every rate-limit window (5h at the time of writing) in `limits`. Counts are
//! JSON strings, not numbers.

use std::env;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use chrono::{DateTime, Utc};
use image::RgbaImage;
use serde::Deserialize;

use crate::plugin::{Plugin, PluginKind, UiPlugin};
use crate::plugins::agents_usage_ui::{self, UsageWindowData};

const USAGES_URL: &str = "https://api.kimi.com/coding/v1/usages";
const WEEK_MINUTES: f64 = 7.0 * 24.0 * 60.0;

#[derive(Debug, Deserialize)]
pub struct UsagesResponse {
    pub usage: Option<QuotaDetail>,
    #[serde(default)]
    pub limits: Vec<RateLimit>,
}

#[derive(Debug, Deserialize)]
pub struct RateLimit {
    pub window: Window,
    pub detail: QuotaDetail,
}

#[derive(Debug, Deserialize)]
pub struct Window {
    pub duration: f64,
    #[serde(rename = "timeUnit")]
    pub time_unit: String,
}

/// Counts arrive as strings ("2048"), so they are parsed, not deserialized as
/// numbers.
#[derive(Debug, Deserialize)]
pub struct QuotaDetail {
    pub limit: String,
    pub used: String,
    #[serde(rename = "resetTime")]
    pub reset_time: Option<String>,
}

impl Window {
    fn minutes(&self) -> Option<f64> {
        let factor = match self.time_unit.as_str() {
            "TIME_UNIT_SECOND" => 1.0 / 60.0,
            "TIME_UNIT_MINUTE" => 1.0,
            "TIME_UNIT_HOUR" => 60.0,
            "TIME_UNIT_DAY" => 1440.0,
            _ => return None,
        };
        Some(self.duration * factor)
    }

    /// Compact panel label: "5h", "30m", "7d".
    fn label(&self) -> String {
        match self.minutes() {
            Some(m) if m >= 1440.0 && m % 1440.0 == 0.0 => format!("{}d", (m / 1440.0) as u64),
            Some(m) if m >= 60.0 && m % 60.0 == 0.0 => format!("{}h", (m / 60.0) as u64),
            Some(m) => format!("{}m", m.round() as u64),
            None => "window".to_string(),
        }
    }
}

impl QuotaDetail {
    fn utilization(&self) -> Result<f64> {
        let limit: f64 = self
            .limit
            .parse()
            .with_context(|| format!("unparseable quota limit '{}'", self.limit))?;
        let used: f64 = self
            .used
            .parse()
            .with_context(|| format!("unparseable quota usage '{}'", self.used))?;
        if limit <= 0.0 {
            return Ok(0.0);
        }
        Ok((used / limit * 100.0).clamp(0.0, 100.0))
    }

    fn resets_in_minutes(&self, now: DateTime<Utc>) -> Option<f64> {
        let reset = DateTime::parse_from_rfc3339(self.reset_time.as_deref()?).ok()?;
        let minutes = (reset.with_timezone(&Utc) - now).num_seconds() as f64 / 60.0;
        (minutes > 0.0).then_some(minutes)
    }
}

/// Bearer token, in the order CodexBar and the official tooling use: explicit
/// config, then environment, then the signed-in CLI's access token.
fn resolve_token(configured: Option<&str>) -> Result<String> {
    if let Some(key) = configured.map(str::trim).filter(|k| !k.is_empty()) {
        return Ok(key.to_string());
    }
    for var in ["KIMI_CODE_API_KEY", "KIMI_API_KEY"] {
        if let Ok(key) = env::var(var) {
            let key = key.trim();
            if !key.is_empty() {
                return Ok(key.to_string());
            }
        }
    }
    if let Some(token) = cli_access_token() {
        return Ok(token);
    }
    Err(anyhow!(
        "no Kimi Code credential: set api_key under [plugins.kimi], export KIMI_CODE_API_KEY, or sign in with the Kimi Code CLI"
    ))
}

/// Read-only reuse of the official CLI credential. The refresh token is never
/// touched and the file is never rewritten; an expired access token surfaces
/// as an API error telling the user to sign in again.
fn cli_access_token() -> Option<String> {
    #[derive(Deserialize)]
    struct Credentials {
        access_token: Option<String>,
    }

    let home = env::var("KIMI_CODE_HOME")
        .map(PathBuf::from)
        .ok()
        .or_else(|| env::var("HOME").ok().map(|h| PathBuf::from(h).join(".kimi-code")))?;
    let raw = std::fs::read_to_string(home.join("credentials/kimi-code.json")).ok()?;
    let creds: Credentials = serde_json::from_str(&raw).ok()?;
    creds.access_token.filter(|t| !t.trim().is_empty())
}

pub fn fetch_usages(token: &str) -> Result<UsagesResponse> {
    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
        .context("failed to build HTTP client")?;

    let resp = client
        .get(USAGES_URL)
        .bearer_auth(token)
        .send()
        .context("failed to reach api.kimi.com")?;

    let status = resp.status();
    let body = resp.text().unwrap_or_default();
    if !status.is_success() {
        return Err(anyhow!(
            "kimi usage request failed ({status}): {}",
            body.trim().chars().take(200).collect::<String>()
        ));
    }

    serde_json::from_str(&body).context("failed to parse kimi usage payload")
}

/// Two panels at most, matching the Claude screen: the tightest rate-limit
/// window (the one that throttles first) on top, membership pool below. A third
/// tall panel does not fit in the 197px below the header.
fn windows(usages: &UsagesResponse, now: DateTime<Utc>) -> Result<Vec<UsageWindowData>> {
    let mut limits: Vec<&RateLimit> = usages.limits.iter().collect();
    limits.sort_by(|a, b| {
        a.window
            .minutes()
            .unwrap_or(f64::MAX)
            .total_cmp(&b.window.minutes().unwrap_or(f64::MAX))
    });

    let mut out = Vec::with_capacity(2);
    if let Some(limit) = limits.first() {
        out.push(window_data(
            limit.window.label(),
            &limit.detail,
            limit.window.minutes(),
            now,
        )?);
    }
    if let Some(usage) = &usages.usage {
        out.push(window_data("Weekly".to_string(), usage, Some(WEEK_MINUTES), now)?);
    }
    Ok(out)
}

fn window_data(
    label: String,
    detail: &QuotaDetail,
    window_minutes: Option<f64>,
    now: DateTime<Utc>,
) -> Result<UsageWindowData> {
    let utilization = detail.utilization()?;
    let resets_in_minutes = detail.resets_in_minutes(now);
    let pace = match (resets_in_minutes, window_minutes) {
        (Some(resets_in), Some(total)) => agents_usage_ui::pace(utilization, resets_in, total),
        _ => None,
    };

    Ok(UsageWindowData {
        label,
        utilization,
        usage_level: agents_usage_ui::usage_level(utilization).to_string(),
        expected_percent: pace.as_ref().map(|p| p.expected_percent),
        delta_percent: pace.as_ref().map(|p| p.delta_percent),
        resets_in_minutes,
        will_last_to_reset: pace.as_ref().map(|p| p.will_last_to_reset),
        eta_minutes: pace.as_ref().and_then(|p| p.eta_minutes),
    })
}

pub struct Kimi {
    api_key: Option<String>,
    windows: Vec<UsageWindowData>,
    fetched_at: Option<String>,
}

impl Kimi {
    pub fn new(api_key: Option<String>) -> Self {
        Self {
            api_key,
            windows: Vec::new(),
            fetched_at: None,
        }
    }
}

impl Plugin for Kimi {
    fn name(&self) -> &'static str {
        "kimi"
    }

    fn get_plugin_kind(&self) -> PluginKind {
        PluginKind::Ui
    }

    fn needs_api_key(&self) -> bool {
        true
    }
}

impl UiPlugin for Kimi {
    fn filename(&self) -> &'static str {
        "kimi.jpg"
    }

    fn collect(&mut self) -> Result<()> {
        let token = resolve_token(self.api_key.as_deref())?;
        let usages = fetch_usages(&token)?;
        let now = Utc::now();
        self.windows = windows(&usages, now)?;
        // The payload carries no "generated at", so the header shows when this
        // cycle fetched it.
        self.fetched_at = Some(now.to_rfc3339());
        Ok(())
    }

    fn render(&self) -> Result<RgbaImage> {
        agents_usage_ui::render_usage_bars("Kimi Code", &self.windows, self.fetched_at.as_deref())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Payload shape documented for GET /coding/v1/usages.
    const SAMPLE: &str = r#"{
        "usage": {"limit": "2048", "used": "214", "remaining": "1834",
                  "resetTime": "2026-01-09T15:23:13.716839300Z"},
        "limits": [{
            "window": {"duration": 300, "timeUnit": "TIME_UNIT_MINUTE"},
            "detail": {"limit": "200", "used": "139", "remaining": "61",
                       "resetTime": "2026-01-06T13:33:02.717479433Z"}
        }, {
            "window": {"duration": 24, "timeUnit": "TIME_UNIT_HOUR"},
            "detail": {"limit": "500", "used": "50",
                       "resetTime": "2026-01-07T00:00:00Z"}
        }]
    }"#;

    fn sample_at(iso: &str) -> Vec<UsageWindowData> {
        let usages: UsagesResponse = serde_json::from_str(SAMPLE).unwrap();
        let now = DateTime::parse_from_rfc3339(iso).unwrap().with_timezone(&Utc);
        windows(&usages, now).unwrap()
    }

    #[test]
    fn keeps_only_the_tightest_rate_limit_window_and_the_weekly_pool() {
        let w = sample_at("2026-01-06T11:33:02Z");
        let labels: Vec<&str> = w.iter().map(|w| w.label.as_str()).collect();
        assert_eq!(labels, ["5h", "Weekly"], "wider 24h window must be dropped");

        assert!((w[0].utilization - 69.5).abs() < 0.01, "139/200 => 69.5%");
        assert!((w[0].resets_in_minutes.unwrap() - 120.0).abs() < 0.05);

        assert!((w[1].utilization - 10.449).abs() < 0.01, "214/2048 => 10.45%");
    }

    #[test]
    fn derives_pace_and_usage_level_from_raw_counts() {
        let w = sample_at("2026-01-06T11:33:02Z");

        // 5h window: 2h left of 5h => 60% elapsed, 69.5% used => burning fast.
        assert_eq!(w[0].usage_level, "moderate");
        assert!(w[0].delta_percent.unwrap() > 0.0, "used above pace");
        assert_eq!(w[0].will_last_to_reset, Some(false));
        assert!(w[0].eta_minutes.is_some());

        // Weekly pool is far ahead of pace, so it lasts to reset.
        assert_eq!(w[1].will_last_to_reset, Some(true));
        assert!(w[1].delta_percent.unwrap() < 0.0);
    }

    #[test]
    fn expired_reset_times_drop_pace_instead_of_faking_it() {
        let w = sample_at("2026-02-01T00:00:00Z");
        assert!(w.iter().all(|w| w.resets_in_minutes.is_none()));
        assert!(w.iter().all(|w| w.delta_percent.is_none()));
    }

    #[test]
    fn reports_missing_credentials_instead_of_calling_the_api() {
        temp_env_clear();
        let err = resolve_token(None).unwrap_err().to_string();
        assert!(err.contains("KIMI_CODE_API_KEY"), "unhelpful error: {err}");
        assert_eq!(resolve_token(Some(" sk-kimi-x ")).unwrap(), "sk-kimi-x");
    }

    /// Keep the credential probe away from a real developer environment.
    fn temp_env_clear() {
        unsafe {
            env::remove_var("KIMI_CODE_API_KEY");
            env::remove_var("KIMI_API_KEY");
            env::set_var("KIMI_CODE_HOME", "/nonexistent-kimi-home");
        }
    }
}
