//! Codex ChatGPT plan quota. The official Codex CLI exposes subscription rate
//! limits through its app-server JSON-RPC protocol, so this plugin starts a
//! short-lived server for each collection cycle. Credentials remain entirely
//! under the CLI's control: this module never reads or rewrites auth files.

use std::collections::BTreeMap;
use std::env;
use std::ffi::{OsStr, OsString};
use std::io::{BufRead, BufReader, ErrorKind, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::{
    LazyLock,
    mpsc::{self, Receiver, RecvTimeoutError},
};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use chrono::{DateTime, Utc};
use image::{ImageFormat, RgbaImage};
use serde::{Deserialize, de::DeserializeOwned};
use serde_json::{Value, json};

use crate::plugin::{Plugin, PluginKind, UiPlugin};
use crate::plugins::agents_usage_ui::{self, UsageWindowData};

const ICON_BYTES: &[u8] = include_bytes!("icon.png");

fn icon() -> &'static RgbaImage {
    static ICON: LazyLock<RgbaImage> = LazyLock::new(|| {
        image::load_from_memory_with_format(ICON_BYTES, ImageFormat::Png)
            .expect("embedded Codex icon must be valid PNG")
            .into_rgba8()
    });
    &ICON
}

const INITIALIZE_TIMEOUT: Duration = Duration::from_secs(8);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

/// Stable `account/rateLimits/read` result documented by the Codex App Server.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RateLimitsResult {
    #[serde(alias = "rate_limits")]
    pub rate_limits: RateLimitSnapshot,
    #[serde(default, alias = "rate_limits_by_limit_id")]
    pub rate_limits_by_limit_id: BTreeMap<String, RateLimitSnapshot>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RateLimitSnapshot {
    // Accepted from the stable protocol for forward-compatible decoding; the
    // shared two-panel UI intentionally renders only the rate-limit windows.
    #[allow(dead_code)]
    #[serde(alias = "limit_id")]
    pub limit_id: Option<String>,
    #[allow(dead_code)]
    #[serde(alias = "limit_name")]
    pub limit_name: Option<String>,
    pub primary: Option<RateLimitWindow>,
    pub secondary: Option<RateLimitWindow>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RateLimitWindow {
    #[serde(alias = "used_percent")]
    pub used_percent: f64,
    #[serde(alias = "window_duration_mins")]
    pub window_duration_mins: Option<u64>,
    #[serde(alias = "resets_at")]
    pub resets_at: Option<i64>,
}

/// Synchronous JSON-RPC client for one short-lived Codex app-server process.
/// Authentication refresh and active-profile selection stay with the official
/// CLI, while Drop guarantees that no server survives a collection cycle.
struct CodexRpc {
    child: Child,
    stdin: Option<ChildStdin>,
    messages: Receiver<std::result::Result<Value, String>>,
    reader: Option<JoinHandle<()>>,
    next_id: u64,
}

impl CodexRpc {
    fn start() -> Result<Self> {
        let codex_binary = env::var_os("CODEX_BINARY");
        let home = env::var_os("HOME").map(PathBuf::from);
        let mut not_found = None;

        for candidate in codex_program_candidates(codex_binary.as_deref(), home.as_deref()) {
            let mut child = match Command::new(&candidate)
                .args(["-s", "read-only", "-a", "never", "app-server"])
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .spawn()
            {
                Ok(child) => child,
                Err(error) if error.kind() == ErrorKind::NotFound => {
                    not_found = Some(error);
                    continue;
                }
                Err(error) => {
                    return Err(error).context(format!(
                        "failed to launch Codex CLI; install it and run 'codex login' (tried {})",
                        Path::new(&candidate).display()
                    ));
                }
            };

            let (Some(stdin), Some(stdout)) = (child.stdin.take(), child.stdout.take()) else {
                let _ = child.kill();
                let _ = child.wait();
                bail!("failed to open pipes to the Codex CLI");
            };
            let (sender, messages) = mpsc::channel();
            let reader = thread::spawn(move || {
                for line in BufReader::new(stdout).lines() {
                    let parsed = match line {
                        Ok(line) => serde_json::from_str::<Value>(&line)
                            .map_err(|error| format!("invalid JSON from the Codex CLI: {error}")),
                        Err(error) => Err(format!("failed to read Codex CLI output: {error}")),
                    };
                    let failed = parsed.is_err();
                    if sender.send(parsed).is_err() || failed {
                        break;
                    }
                }
            });

            return Ok(Self {
                child,
                stdin: Some(stdin),
                messages,
                reader: Some(reader),
                next_id: 1,
            });
        }

        let detail = not_found
            .map(|error| format!(" (last error: {error})"))
            .unwrap_or_default();
        bail!("failed to launch Codex CLI; install it and run 'codex login'{detail}");
    }

    fn initialize(&mut self) -> Result<()> {
        self.request::<Value>(
            "initialize",
            json!({
                "clientInfo": {
                    "name": "geekmagic_monitors",
                    "title": "GeekMagic Monitors",
                    "version": env!("CARGO_PKG_VERSION"),
                }
            }),
            INITIALIZE_TIMEOUT,
        )
        .context("Codex CLI initialize handshake failed")?;
        self.notify("initialized", json!({}))
            .context("failed to notify the Codex CLI")?;
        Ok(())
    }

    fn request<T: DeserializeOwned>(
        &mut self,
        method: &str,
        params: Value,
        timeout: Duration,
    ) -> Result<T> {
        let id = self.next_id;
        self.next_id += 1;
        self.send(&json!({"id": id, "method": method, "params": params}))?;
        let result = await_result(&self.messages, id, method, timeout)?;
        serde_json::from_value(result)
            .with_context(|| format!("failed to decode Codex RPC '{method}' result"))
    }

    fn notify(&mut self, method: &str, params: Value) -> Result<()> {
        self.send(&json!({"method": method, "params": params}))
    }

    fn send(&mut self, envelope: &Value) -> Result<()> {
        let stdin = self
            .stdin
            .as_mut()
            .ok_or_else(|| anyhow!("Codex CLI stdin is closed"))?;
        serde_json::to_writer(&mut *stdin, envelope).context("failed to write to the Codex CLI")?;
        stdin
            .write_all(b"\n")
            .context("failed to write to the Codex CLI")?;
        stdin.flush().context("failed to flush Codex CLI stdin")?;
        Ok(())
    }

    fn fetch_rate_limits(&mut self) -> Result<RateLimitSnapshot> {
        let result: RateLimitsResult =
            match self.request("account/rateLimits/read", json!({}), REQUEST_TIMEOUT) {
                Ok(result) => result,
                Err(error) if error.to_string().contains("-32601") => {
                    return Err(error.context(
                        "this Codex CLI does not expose account/rateLimits/read; update Codex",
                    ));
                }
                Err(error) => return Err(error),
            };
        Ok(select_snapshot(result))
    }
}

impl Drop for CodexRpc {
    fn drop(&mut self) {
        drop(self.stdin.take());
        if matches!(self.child.try_wait(), Ok(None)) {
            let _ = self.child.kill();
        }
        let _ = self.child.wait();
        if let Some(reader) = self.reader.take() {
            let _ = reader.join();
        }
    }
}

/// Read responses until the requested JSON-RPC ID arrives. Notifications and
/// unrelated responses can occur at any point and do not consume the deadline.
fn await_result(
    messages: &Receiver<std::result::Result<Value, String>>,
    id: u64,
    method: &str,
    timeout: Duration,
) -> Result<Value> {
    let deadline = Instant::now() + timeout;
    loop {
        let remaining = deadline
            .checked_duration_since(Instant::now())
            .ok_or_else(|| anyhow!("timed out waiting for Codex RPC '{method}'"))?;
        match messages.recv_timeout(remaining) {
            Ok(Ok(message)) => {
                if message.get("id").and_then(Value::as_u64) != Some(id) {
                    continue;
                }
                if let Some(error) = message.get("error") {
                    let code = error
                        .get("code")
                        .map(ToString::to_string)
                        .unwrap_or_else(|| "?".to_string());
                    let detail = error
                        .get("message")
                        .and_then(Value::as_str)
                        .unwrap_or("unknown error");
                    bail!("Codex RPC '{method}' failed ({code}): {detail}");
                }
                return message
                    .get("result")
                    .cloned()
                    .ok_or_else(|| anyhow!("Codex RPC '{method}' response carries no result"));
            }
            Ok(Err(error)) => bail!("{error}"),
            Err(RecvTimeoutError::Timeout) => {
                bail!("timed out waiting for Codex RPC '{method}'")
            }
            Err(RecvTimeoutError::Disconnected) => {
                bail!("Codex CLI closed its output while waiting for '{method}'")
            }
        }
    }
}

/// Return only `CODEX_BINARY` when explicitly configured; otherwise include
/// the interactive PATH lookup and service-safe installer locations.
fn codex_program_candidates(codex_binary: Option<&OsStr>, home: Option<&Path>) -> Vec<OsString> {
    if let Some(binary) = codex_binary.filter(|binary| !binary.is_empty()) {
        return vec![binary.to_os_string()];
    }

    let mut candidates = vec![OsString::from("codex")];
    if let Some(home) = home {
        candidates.push(home.join(".local/bin/codex").into_os_string());
    }
    candidates.push(OsString::from("/opt/homebrew/bin/codex"));
    candidates.push(OsString::from("/usr/local/bin/codex"));
    candidates
}

/// The `codex` bucket is the ChatGPT plan quota. Older CLIs expose only the
/// top-level snapshot, so use it only when no dedicated bucket is present.
fn select_snapshot(mut result: RateLimitsResult) -> RateLimitSnapshot {
    result
        .rate_limits_by_limit_id
        .remove("codex")
        .unwrap_or(result.rate_limits)
}

pub fn windows_from_snapshot(
    snapshot: &RateLimitSnapshot,
    now: DateTime<Utc>,
) -> Result<Vec<UsageWindowData>> {
    let mut windows = Vec::with_capacity(2);
    if let Some(primary) = &snapshot.primary {
        windows.push(window_data(primary, "Session", now));
    }
    if let Some(secondary) = &snapshot.secondary {
        windows.push(window_data(secondary, "Weekly", now));
    }
    if windows.is_empty() {
        bail!(
            "Codex CLI returned no ChatGPT rate-limit windows; run 'codex login' with ChatGPT authentication"
        );
    }
    Ok(windows)
}

fn window_data(
    window: &RateLimitWindow,
    fallback_label: &str,
    now: DateTime<Utc>,
) -> UsageWindowData {
    let utilization = window.used_percent.clamp(0.0, 100.0);
    let resets_in_minutes = window
        .resets_at
        .map(|resets_at| resets_at - now.timestamp())
        .filter(|seconds| *seconds > 0)
        .map(|seconds| seconds as f64 / 60.0);
    let pace = match (resets_in_minutes, window.window_duration_mins) {
        (Some(resets_in_minutes), Some(window_duration_mins)) if window_duration_mins > 0 => {
            agents_usage_ui::pace(utilization, resets_in_minutes, window_duration_mins as f64)
        }
        _ => None,
    };

    UsageWindowData {
        label: window_label(window.window_duration_mins, fallback_label),
        utilization,
        usage_level: agents_usage_ui::usage_level(utilization).to_string(),
        expected_percent: pace.as_ref().map(|pace| pace.expected_percent),
        delta_percent: pace.as_ref().map(|pace| pace.delta_percent),
        resets_in_minutes,
        will_last_to_reset: pace.as_ref().map(|pace| pace.will_last_to_reset),
        eta_minutes: pace.as_ref().and_then(|pace| pace.eta_minutes),
    }
}

fn window_label(window_duration_mins: Option<u64>, fallback: &str) -> String {
    match window_duration_mins {
        Some(10_080) => "Weekly".to_string(),
        Some(minutes) if minutes > 0 && minutes % 1_440 == 0 => {
            format!("{}d", minutes / 1_440)
        }
        Some(minutes) if minutes > 0 && minutes % 60 == 0 => format!("{}h", minutes / 60),
        Some(minutes) if minutes > 0 => format!("{minutes}m"),
        _ => fallback.to_string(),
    }
}

pub struct Codex {
    windows: Vec<UsageWindowData>,
    fetched_at: Option<String>,
}

impl Codex {
    pub fn new() -> Self {
        Self {
            windows: Vec::new(),
            fetched_at: None,
        }
    }
}

impl Plugin for Codex {
    fn name(&self) -> &'static str {
        "codex"
    }

    fn get_plugin_kind(&self) -> PluginKind {
        PluginKind::Ui
    }
}

impl UiPlugin for Codex {
    fn filename(&self) -> &'static str {
        "codex.jpg"
    }

    fn collect(&mut self) -> Result<()> {
        let mut rpc = CodexRpc::start()?;
        rpc.initialize()?;
        let snapshot = rpc.fetch_rate_limits()?;
        let now = Utc::now();
        let windows = windows_from_snapshot(&snapshot, now)?;
        let fetched_at = now.to_rfc3339();

        // Commit only after every protocol and conversion step succeeds.
        self.windows = windows;
        self.fetched_at = Some(fetched_at);
        Ok(())
    }

    fn render(&self) -> Result<RgbaImage> {
        agents_usage_ui::render_usage_bars(
            "Codex",
            icon(),
            &self.windows,
            self.fetched_at.as_deref(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixed_now() -> DateTime<Utc> {
        DateTime::parse_from_rfc3339("2026-02-01T12:00:00Z")
            .unwrap()
            .with_timezone(&Utc)
    }

    fn loaded_channel(
        messages: Vec<std::result::Result<Value, String>>,
    ) -> Receiver<std::result::Result<Value, String>> {
        let (sender, receiver) = mpsc::channel();
        for message in messages {
            sender.send(message).unwrap();
        }
        receiver
    }

    #[test]
    fn embedded_icon_is_valid() {
        let icon = icon();
        assert_eq!(icon.dimensions(), (18, 18));
        assert!(icon.pixels().any(|pixel| pixel[3] != 0));
    }

    #[test]
    fn maps_primary_and_secondary_windows() {
        let now = fixed_now();
        let result: RateLimitsResult = serde_json::from_value(json!({
            "rateLimits": {"primary": null, "secondary": null},
            "rateLimitsByLimitId": {
                "codex": {
                    "limitId": "codex",
                    "limitName": "Codex",
                    "primary": {
                        "usedPercent": 42.0,
                        "windowDurationMins": 300,
                        "resetsAt": now.timestamp() + 7_200
                    },
                    "secondary": {
                        "usedPercent": 18.0,
                        "windowDurationMins": 10_080,
                        "resetsAt": now.timestamp() + 345_600
                    }
                }
            }
        }))
        .unwrap();

        let windows = windows_from_snapshot(&select_snapshot(result), now).unwrap();
        assert_eq!(windows.len(), 2);
        assert_eq!(windows[0].label, "5h");
        assert_eq!(windows[0].utilization, 42.0);
        assert!((windows[0].resets_in_minutes.unwrap() - 120.0).abs() < 0.01);
        assert!(windows[0].expected_percent.is_some());
        assert!(windows[0].delta_percent.is_some());
        assert_eq!(windows[1].label, "Weekly");
        assert_eq!(windows[1].utilization, 18.0);
        assert!((windows[1].resets_in_minutes.unwrap() - 5_760.0).abs() < 0.01);
        assert!(windows[1].expected_percent.is_some());
        assert!(windows[1].delta_percent.is_some());
    }

    #[test]
    fn prefers_codex_bucket_from_multi_bucket_response() {
        let result: RateLimitsResult = serde_json::from_value(json!({
            "rateLimits": {
                "primary": {"usedPercent": 99.0, "windowDurationMins": 300},
                "secondary": null
            },
            "rateLimitsByLimitId": {
                "codex": {
                    "primary": {"usedPercent": 7.0, "windowDurationMins": 300},
                    "secondary": null
                },
                "other": {
                    "primary": {"usedPercent": 55.0, "windowDurationMins": 300},
                    "secondary": null
                }
            }
        }))
        .unwrap();

        let windows = windows_from_snapshot(&select_snapshot(result), fixed_now()).unwrap();
        assert_eq!(windows.len(), 1);
        assert_eq!(windows[0].utilization, 7.0);
    }

    #[test]
    fn single_window_does_not_synthesize_secondary() {
        let snapshot = RateLimitSnapshot {
            limit_id: None,
            limit_name: None,
            primary: Some(RateLimitWindow {
                used_percent: 30.0,
                window_duration_mins: Some(300),
                resets_at: None,
            }),
            secondary: None,
        };

        let windows = windows_from_snapshot(&snapshot, fixed_now()).unwrap();
        assert_eq!(windows.len(), 1);
        assert_eq!(windows[0].label, "5h");
    }

    #[test]
    fn clamps_usage_and_drops_expired_reset_pace() {
        let now = fixed_now();
        let snapshot = RateLimitSnapshot {
            limit_id: None,
            limit_name: None,
            primary: Some(RateLimitWindow {
                used_percent: 130.0,
                window_duration_mins: Some(300),
                resets_at: Some(now.timestamp() - 60),
            }),
            secondary: None,
        };

        let windows = windows_from_snapshot(&snapshot, now).unwrap();
        assert_eq!(windows[0].utilization, 100.0);
        assert_eq!(windows[0].resets_in_minutes, None);
        assert_eq!(windows[0].expected_percent, None);
        assert_eq!(windows[0].delta_percent, None);
        assert_eq!(windows[0].usage_level, "danger");
    }

    #[test]
    fn empty_snapshot_is_an_actionable_error() {
        let snapshot = RateLimitSnapshot {
            limit_id: None,
            limit_name: None,
            primary: None,
            secondary: None,
        };

        let error = windows_from_snapshot(&snapshot, fixed_now())
            .err()
            .expect("empty snapshots must fail")
            .to_string();
        assert!(
            error.contains("run 'codex login' with ChatGPT authentication"),
            "unhelpful error: {error}"
        );
    }

    #[test]
    fn decodes_camel_and_snake_case_rpc_fields() {
        let camel: RateLimitsResult = serde_json::from_value(json!({
            "rateLimits": {
                "limitId": "codex",
                "limitName": "Codex",
                "primary": {
                    "usedPercent": 42.0,
                    "windowDurationMins": 300,
                    "resetsAt": 123
                },
                "secondary": null
            },
            "rateLimitsByLimitId": {}
        }))
        .unwrap();
        let snake: RateLimitsResult = serde_json::from_value(json!({
            "rate_limits": {
                "limit_id": "codex",
                "limit_name": "Codex",
                "primary": {
                    "used_percent": 42.0,
                    "window_duration_mins": 300,
                    "resets_at": 123
                },
                "secondary": null
            }
        }))
        .unwrap();

        assert_eq!(camel.rate_limits.limit_id, snake.rate_limits.limit_id);
        assert_eq!(camel.rate_limits.limit_name, snake.rate_limits.limit_name);
        let camel_primary = camel.rate_limits.primary.unwrap();
        let snake_primary = snake.rate_limits.primary.unwrap();
        assert_eq!(camel_primary.used_percent, snake_primary.used_percent);
        assert_eq!(
            camel_primary.window_duration_mins,
            snake_primary.window_duration_mins
        );
        assert_eq!(camel_primary.resets_at, snake_primary.resets_at);
    }

    #[test]
    fn metadata_matches_catalog_contract() {
        let codex = Codex::new();
        assert_eq!(codex.name(), "codex");
        assert_eq!(codex.get_plugin_kind(), PluginKind::Ui);
        assert_eq!(codex.filename(), "codex.jpg");
        assert!(!codex.needs_api_key());
    }

    #[test]
    fn binary_candidates_honor_override_and_service_fallbacks() {
        let home = Path::new("/home/geekmagic");
        assert_eq!(
            codex_program_candidates(Some(OsStr::new("/custom/codex")), Some(home)),
            vec![OsString::from("/custom/codex")]
        );

        let candidates = codex_program_candidates(None, Some(home));
        assert_eq!(
            candidates,
            vec![
                OsString::from("codex"),
                OsString::from("/home/geekmagic/.local/bin/codex"),
                OsString::from("/opt/homebrew/bin/codex"),
                OsString::from("/usr/local/bin/codex"),
            ]
        );
        assert_eq!(
            codex_program_candidates(Some(OsStr::new("")), Some(home)),
            candidates
        );
    }

    #[test]
    fn window_labels_follow_duration() {
        assert_eq!(window_label(Some(300), "Session"), "5h");
        assert_eq!(window_label(Some(10_080), "Session"), "Weekly");
        assert_eq!(window_label(Some(2_880), "Session"), "2d");
        assert_eq!(window_label(Some(60), "Session"), "1h");
        assert_eq!(window_label(Some(45), "Session"), "45m");
        assert_eq!(window_label(Some(0), "Session"), "Session");
        assert_eq!(window_label(None, "Weekly"), "Weekly");
    }

    #[test]
    fn rpc_response_matching_ignores_notifications_and_other_ids() {
        let messages = loaded_channel(vec![
            Ok(json!({"method": "rateLimitsUpdated", "params": {}})),
            Ok(json!({"id": 99, "result": {"usedPercent": 1.0}})),
            Ok(json!({"id": 1, "result": {"usedPercent": 42.0}})),
        ]);

        let result = await_result(
            &messages,
            1,
            "account/rateLimits/read",
            Duration::from_millis(50),
        )
        .unwrap();
        assert_eq!(result, json!({"usedPercent": 42.0}));
    }

    #[test]
    fn rpc_error_envelope_preserves_code_and_message() {
        let messages = loaded_channel(vec![Ok(json!({
            "id": 2,
            "error": {"code": -32600, "message": "invalid request"}
        }))]);

        let error = await_result(&messages, 2, "initialize", Duration::from_millis(50))
            .unwrap_err()
            .to_string();
        assert_eq!(
            error,
            "Codex RPC 'initialize' failed (-32600): invalid request"
        );
    }

    #[test]
    fn rpc_reports_closed_channel() {
        let messages = loaded_channel(Vec::new());
        let error = await_result(&messages, 1, "initialize", Duration::from_millis(50))
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("closed its output"),
            "unhelpful error: {error}"
        );
    }
}
