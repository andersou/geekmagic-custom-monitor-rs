//! Device model + firmware detection. Detection order per the base's
//! device-protocol.md; generalizes the old `album_theme()` helper, which is
//! deleted.

use log::warn;
use serde::Deserialize;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Model {
    Ultra,
    Pro,
    Unknown, // => album_theme 3, re-probe each cycle
}

#[derive(Debug, Clone)]
pub struct DeviceInfo {
    pub model: Model,
    pub album_theme: u8,
    pub firmware: Option<String>,
}

#[derive(Deserialize)]
struct VersionInfo {
    m: Option<String>,
    v: Option<String>,
}

pub fn detect(
    client: &reqwest::blocking::Client,
    base: &str,
    cfg_model: Option<&str>,
) -> DeviceInfo {
    let (model, album_theme) = match cfg_model {
        Some("ultra") => (Model::Ultra, 3),
        Some("pro") => (Model::Pro, 4),
        Some(other) if other != "auto" => {
            warn!("unknown model '{other}', falling back to auto-detect");
            probe(client, base)
        }
        _ => probe(client, base),
    };

    // Firmware log line; tolerate failure.
    let firmware = client
        .get(format!("{base}/v.json"))
        .send()
        .ok()
        .and_then(|r| r.text().ok())
        .and_then(|body| serde_json::from_str::<VersionInfo>(&body).ok())
        .map(|vi| match (vi.m, vi.v) {
            (Some(m), Some(v)) => format!("{m} {v}"),
            (Some(m), None) => m,
            (None, Some(v)) => v,
            (None, None) => "unknown".to_string(),
        });

    DeviceInfo {
        model,
        album_theme,
        firmware,
    }
}

fn probe(client: &reqwest::blocking::Client, base: &str) -> (Model, u8) {
    let ok = |path: &str| {
        client
            .get(format!("{base}{path}"))
            .send()
            .map(|r| r.status().is_success())
            .unwrap_or(false)
    };
    if ok("/.sys/app.json") {
        (Model::Pro, 4)
    } else if ok("/app.json") {
        (Model::Ultra, 3)
    } else {
        (Model::Unknown, 3)
    }
}
