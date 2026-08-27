use anyhow::{Context, Result};
use image::RgbaImage;
use serde::Deserialize;

use crate::plugin::{Plugin, PluginKind, UiPlugin};
use crate::plugins::agents_usage_ui::{self, UsageWindowData};

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

/// Fill in pace data for windows the API left blank.
fn ensure_pace(window: &mut UsageWindow, window_minutes: f64) {
    if window.pace.is_some() {
        return;
    }
    let Some(resets_in) = window.resets_in_minutes else {
        return;
    };
    window.pace =
        agents_usage_ui::pace(window.utilization, resets_in, window_minutes).map(|p| PaceInfo {
            delta_percent: p.delta_percent,
            expected_percent: p.expected_percent,
            will_last_to_reset: p.will_last_to_reset,
            eta_minutes: p.eta_minutes,
        });
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

    fn get_plugin_kind(&self) -> PluginKind {
        PluginKind::Ui
    }
}

impl UiPlugin for Claude {
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

        agents_usage_ui::render_usage_bars("Claude Code", &windows, data.updated_at.as_deref())
    }
}

fn window_data(label: &str, w: &UsageWindow) -> UsageWindowData {
    UsageWindowData {
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
