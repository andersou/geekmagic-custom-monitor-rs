pub mod render;

use anyhow::{Context, Result};
use image::RgbaImage;
use serde::Deserialize;

use crate::plugin::Plugin;

#[derive(Debug, Deserialize)]
pub struct StatsPayload {
    #[allow(dead_code)]
    pub status: String,
    pub data: Option<ActiveData>,
}

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

/// Compute pace locally when the API doesn't provide it.
/// Mirrors the logic in claude-code-stats/src/types.rs.
fn compute_pace(utilization: f64, resets_in_minutes: f64, window_minutes: f64) -> Option<PaceInfo> {
    if window_minutes <= 0.0 || resets_in_minutes <= 0.0 || resets_in_minutes > window_minutes {
        return None;
    }

    let elapsed = (window_minutes - resets_in_minutes) * 60.0;
    let duration = window_minutes * 60.0;
    let time_left = resets_in_minutes * 60.0;

    let actual = utilization.clamp(0.0, 100.0);
    let expected = ((elapsed / duration) * 100.0).clamp(0.0, 100.0);

    if (elapsed == 0.0 && actual > 0.0) || expected < 3.0 {
        return None;
    }

    let delta = actual - expected;

    let (will_last_to_reset, eta_minutes) = if elapsed > 0.0 && actual > 0.0 {
        let rate = actual / elapsed;
        if rate > 0.0 {
            let remaining = (100.0 - actual).max(0.0);
            let candidate = remaining / rate;
            if candidate >= time_left {
                (true, None)
            } else {
                (false, Some(candidate / 60.0))
            }
        } else {
            (true, None)
        }
    } else if elapsed > 0.0 {
        (true, None)
    } else {
        return None;
    };

    Some(PaceInfo {
        delta_percent: delta,
        expected_percent: expected,
        will_last_to_reset,
        eta_minutes,
    })
}

/// Fill in pace data for windows that don't have it.
fn ensure_pace(window: &mut UsageWindow, window_minutes: f64) {
    if window.pace.is_some() {
        return;
    }
    if let Some(resets_in) = window.resets_in_minutes {
        window.pace = compute_pace(window.utilization, resets_in, window_minutes);
    }
}

pub fn fetch_stats() -> Result<ActiveData> {
    let payload_json = claude_code_stats::collect_widget_payload_json();
    let payload: StatsPayload =
        serde_json::from_str(&payload_json).context("failed to parse claude-code-stats payload")?;

    let mut data = payload
        .data
        .context("claude-code-stats returned non-active status")?;

    // Compute pace locally if not provided
    if let Some(w) = &mut data.five_hour {
        ensure_pace(w, 300.0); // 5 hours
    }
    if let Some(w) = &mut data.seven_day {
        ensure_pace(w, 10080.0); // 7 days
    }

    Ok(data)
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

    fn filename(&self) -> &'static str {
        "claude.jpg"
    }

    fn collect(&mut self) -> Result<()> {
        self.data = Some(fetch_stats()?);
        Ok(())
    }

    fn render(&self) -> Result<RgbaImage> {
        let data = self.data.as_ref().context("collect() has not run")?;

        let mut windows = Vec::new();
        if let Some(w) = &data.five_hour {
            windows.push(window_data("Session", w));
        }
        if let Some(w) = &data.seven_day {
            windows.push(window_data("Weekly", w));
        }

        render::render_usage_bars(&windows, data.updated_at.as_deref())
    }
}

fn window_data(label: &str, w: &UsageWindow) -> render::UsageWindowData {
    render::UsageWindowData {
        label: label.to_string(),
        utilization: w.utilization,
        usage_level: w.usage_level.clone(),
        expected_percent: w.pace.as_ref().map(|p| p.expected_percent),
        delta_percent: w.pace.as_ref().map(|p| p.delta_percent),
        resets_in_minutes: w.resets_in_minutes,
        will_last_to_reset: w.pace.as_ref().map(|p| p.will_last_to_reset),
        eta_minutes: w.pace.as_ref().and_then(|p| p.eta_minutes),
    }
}
