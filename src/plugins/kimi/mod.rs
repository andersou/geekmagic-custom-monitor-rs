//! Kimi Code plan quota. Same two-window shape as the Claude screen, so it
//! renders through the shared `agents-usage-ui` renderer plugin.
//!
//! `GET /coding/v1/usages` reports the weekly membership pool in `usage` and
//! every rate-limit window (5h at the time of writing) in `limits`. Counts are
//! JSON strings, not numbers.

use std::env;
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;
use std::time::{Duration, SystemTime};

use anyhow::{Context, Result, anyhow, bail};
use chrono::{DateTime, Utc};
use image::{ImageFormat, RgbaImage};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::plugin::{Plugin, PluginKind, UiPlugin};
use crate::plugins::agents_usage_ui::{self, UsageWindowData};

const ICON_BYTES: &[u8] = include_bytes!("icon.png");

fn icon() -> &'static RgbaImage {
    static ICON: LazyLock<RgbaImage> = LazyLock::new(|| {
        image::load_from_memory_with_format(ICON_BYTES, ImageFormat::Png)
            .expect("embedded Kimi icon must be valid PNG")
            .into_rgba8()
    });
    &ICON
}

const USAGES_URL: &str = "https://api.kimi.com/coding/v1/usages";
const OAUTH_CLIENT_ID: &str = "17e5f671-d194-4dfb-9706-5516cb48c098";
const MAX_REQUEST_RETRIES: u32 = 2;
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

/// Counts arrive as strings ("2048"). Kimi currently reports `used` for the
/// weekly pool but only `remaining` for rate-limit windows.
#[derive(Debug, Deserialize)]
pub struct QuotaDetail {
    pub limit: String,
    pub used: Option<String>,
    pub remaining: Option<String>,
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
        if limit <= 0.0 {
            return Ok(0.0);
        }
        let used: f64 = match &self.used {
            Some(used) => used
                .parse()
                .with_context(|| format!("unparseable quota usage '{used}'"))?,
            None => {
                let remaining: f64 = self
                    .remaining
                    .as_deref()
                    .context("Kimi quota has neither used nor remaining")?
                    .parse()
                    .with_context(|| "unparseable Kimi quota remaining")?;
                limit - remaining
            }
        };
        Ok((used / limit * 100.0).clamp(0.0, 100.0))
    }

    fn resets_in_minutes(&self, now: DateTime<Utc>) -> Option<f64> {
        let reset = DateTime::parse_from_rfc3339(self.reset_time.as_deref()?).ok()?;
        let minutes = (reset.with_timezone(&Utc) - now).num_seconds() as f64 / 60.0;
        (minutes > 0.0).then_some(minutes)
    }
}

struct OAuthHosts {
    primary: &'static str,
    alternate: Option<&'static str>,
}

#[derive(Debug)]
struct CliCredentials {
    access_token: String,
    expires_at: Option<i64>,
}

/// Bearer token, in the order CodexBar and the official tooling use: explicit
/// config, then environment, then the signed-in CLI's access token.
fn resolve_token(configured: Option<&str>) -> Result<(String, Option<PathBuf>)> {
    if let Some(key) = configured.map(str::trim).filter(|key| !key.is_empty()) {
        return Ok((key.to_string(), None));
    }
    for var in ["KIMI_CODE_API_KEY", "KIMI_API_KEY"] {
        if let Ok(key) = env::var(var) {
            let key = key.trim();
            if !key.is_empty() {
                return Ok((key.to_string(), None));
            }
        }
    }
    let home = kimi_home();
    if let Some(credentials) = home.as_deref().and_then(cli_credentials) {
        return Ok((credentials.access_token, home));
    }
    Err(anyhow!(
        "no Kimi Code credential: set api_key under [plugins.kimi], export KIMI_CODE_API_KEY, or sign in with the Kimi Code CLI"
    ))
}

fn kimi_home() -> Option<PathBuf> {
    env::var("KIMI_CODE_HOME")
        .ok()
        .map(PathBuf::from)
        .or_else(|| {
            env::var("HOME")
                .ok()
                .map(|home| PathBuf::from(home).join(".kimi-code"))
        })
}

fn oauth_hosts(home: &Path) -> OAuthHosts {
    match fs::read_to_string(home.join("region")) {
        Ok(region) if region.trim() == "mainland-cn" => OAuthHosts {
            primary: "https://auth.kimi.com",
            alternate: Some("https://auth.kimi.ai"),
        },
        Ok(_) => OAuthHosts {
            primary: "https://auth.kimi.ai",
            alternate: Some("https://auth.kimi.com"),
        },
        Err(_) => OAuthHosts {
            primary: "https://auth.kimi.com",
            alternate: None,
        },
    }
}

/// Reuse the official CLI credential. Its refresh token is used only to renew
/// an expired or rejected access token, and a successful refresh replaces the
/// credential file before the new access token is used.
fn cli_credentials(home: &Path) -> Option<CliCredentials> {
    #[derive(Deserialize)]
    struct RawCredentials {
        access_token: Option<String>,
        expires_at: Option<i64>,
    }

    let raw = fs::read_to_string(home.join("credentials/kimi-code.json")).ok()?;
    let credentials: RawCredentials = serde_json::from_str(&raw).ok()?;
    let access_token = credentials.access_token?.trim().to_string();
    if access_token.is_empty() {
        return None;
    }
    let expires_at = credentials.expires_at.map(|expires_at| {
        if expires_at > 10_000_000_000 {
            expires_at / 1000
        } else {
            expires_at
        }
    });
    Some(CliCredentials {
        access_token,
        expires_at,
    })
}

fn credentials_path(home: &Path) -> PathBuf {
    home.join("credentials/kimi-code.json")
}

fn post_refresh(
    client: &reqwest::blocking::Client,
    host: &str,
    refresh_token: &str,
) -> Result<(reqwest::StatusCode, String)> {
    let response = client
        .post(format!("{host}/api/oauth/token"))
        .form(&[
            ("grant_type", "refresh_token"),
            ("refresh_token", refresh_token),
            ("client_id", OAUTH_CLIENT_ID),
        ])
        .send()
        .with_context(|| format!("failed to reach {host} for Kimi Code credential refresh"))?;
    let status = response.status();
    let body = response
        .text()
        .context("failed to read Kimi Code credential refresh response")?;
    Ok((status, body))
}

/// Marker for an OAuth `invalid_grant` refresh rejection: the stored refresh
/// token is dead until the user signs in again, so retrying is pointless.
#[derive(Debug)]
struct InvalidGrant;

impl fmt::Display for InvalidGrant {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("run `kimi login` to sign in again")
    }
}

impl std::error::Error for InvalidGrant {}

fn is_invalid_grant(error: &anyhow::Error) -> bool {
    error
        .chain()
        .any(|cause| cause.downcast_ref::<InvalidGrant>().is_some())
}

fn refresh_response_error(status: reqwest::StatusCode, body: &str) -> anyhow::Error {
    let message = format!(
        "kimi credential refresh failed ({status}): {}",
        body.trim().chars().take(200).collect::<String>()
    );
    if body.contains("invalid_grant") {
        anyhow::Error::new(InvalidGrant).context(message)
    } else {
        anyhow!(message)
    }
}

fn apply_refresh(credentials: &mut Value, body: &Value, now_epoch: i64) -> Result<String> {
    let object = credentials
        .as_object_mut()
        .context("Kimi CLI credential must be a JSON object")?;
    let access_token = body
        .get("access_token")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|token| !token.is_empty())
        .context("Kimi credential refresh response has no access_token")?
        .to_string();
    object.insert(
        "access_token".to_string(),
        Value::String(access_token.clone()),
    );

    if let Some(refresh_token) = body
        .get("refresh_token")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|token| !token.is_empty())
    {
        object.insert(
            "refresh_token".to_string(),
            Value::String(refresh_token.to_string()),
        );
    }

    if let Some(expires_in) = body.get("expires_in").and_then(Value::as_i64)
        && expires_in >= 0
    {
        let expires_at = now_epoch
            .checked_add(expires_in)
            .context("Kimi credential refresh expiry overflows epoch seconds")?;
        object.insert("expires_in".to_string(), json!(expires_in));
        object.insert("expires_at".to_string(), json!(expires_at));
    }
    Ok(access_token)
}

fn create_temp_file(path: &Path) -> Result<(PathBuf, File)> {
    let parent = path
        .parent()
        .context("Kimi CLI credential has no parent directory")?;
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .context("Kimi CLI credential filename is not valid UTF-8")?;
    for attempt in 0..128 {
        let temporary = parent.join(format!(".{name}.{}.{}.tmp", std::process::id(), attempt));
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        match options.open(&temporary) {
            Ok(file) => return Ok((temporary, file)),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => {
                return Err(error).with_context(|| {
                    format!(
                        "failed to create Kimi credential temporary file {}",
                        temporary.display()
                    )
                });
            }
        }
    }
    Err(anyhow!(
        "failed to create a unique Kimi credential temporary file beside {}",
        path.display()
    ))
}

#[cfg(unix)]
fn replace_credentials(temporary: &Path, path: &Path) -> Result<()> {
    fs::rename(temporary, path).with_context(|| {
        format!(
            "failed to replace Kimi CLI credential {}; recovery file remains at {}",
            path.display(),
            temporary.display()
        )
    })?;
    let parent = path
        .parent()
        .context("Kimi CLI credential has no parent directory")?;
    File::open(parent)
        .and_then(|directory| directory.sync_all())
        .with_context(|| {
            format!(
                "failed to sync Kimi CLI credential directory {}",
                parent.display()
            )
        })
}

#[cfg(windows)]
fn replace_credentials(temporary: &Path, path: &Path) -> Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::{
        MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH, MoveFileExW,
    };

    let temporary_wide: Vec<u16> = temporary.as_os_str().encode_wide().chain(Some(0)).collect();
    let path_wide: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
    let moved = unsafe {
        MoveFileExW(
            temporary_wide.as_ptr(),
            path_wide.as_ptr(),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    };
    if moved == 0 {
        return Err(std::io::Error::last_os_error()).with_context(|| {
            format!(
                "failed to replace Kimi CLI credential {}; recovery file remains at {}",
                path.display(),
                temporary.display()
            )
        });
    }
    Ok(())
}

#[cfg(not(any(unix, windows)))]
fn replace_credentials(temporary: &Path, path: &Path) -> Result<()> {
    fs::rename(temporary, path).with_context(|| {
        format!(
            "failed to replace Kimi CLI credential {}; recovery file remains at {}",
            path.display(),
            temporary.display()
        )
    })
}

fn persist_credentials_atomic(path: &Path, credentials: &Value) -> Result<()> {
    let bytes = serde_json::to_vec_pretty(credentials)
        .context("failed to serialize Kimi CLI credential")?;
    let (temporary, mut file) = create_temp_file(path)?;
    if let Err(error) = file.write_all(&bytes).and_then(|_| file.sync_all()) {
        let _ = fs::remove_file(&temporary);
        return Err(error).with_context(|| {
            format!(
                "failed to write Kimi credential temporary file {}",
                temporary.display()
            )
        });
    }
    drop(file);
    replace_credentials(&temporary, path)
}

fn refresh_cli_token(home: &Path) -> Result<String> {
    let path = credentials_path(home);
    let raw = fs::read_to_string(&path)
        .with_context(|| format!("failed to read Kimi CLI credential {}", path.display()))?;
    let mut credentials: Value =
        serde_json::from_str(&raw).context("failed to parse Kimi CLI credential JSON")?;
    let refresh_token = credentials
        .get("refresh_token")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|token| !token.is_empty())
        .context("kimi CLI credential has no refresh_token; sign in again with the Kimi Code CLI")?
        .to_string();

    // A generous timeout: Kimi refresh tokens are single-use, so a request the
    // server completes but the client abandons loses the rotated token and
    // kills the grant (observed with a 10s timeout).
    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(30))
        .build()
        .context("failed to build Kimi credential refresh HTTP client")?;
    let hosts = oauth_hosts(home);
    let (mut status, mut body) = post_refresh(&client, hosts.primary, &refresh_token)?;
    if !status.is_success()
        && body.contains("invalid_grant")
        && let Some(alternate) = hosts.alternate
    {
        (status, body) = post_refresh(&client, alternate, &refresh_token)?;
    }
    if !status.is_success() {
        return Err(refresh_response_error(status, &body));
    }

    let response: Value =
        serde_json::from_str(&body).context("failed to parse Kimi credential refresh response")?;
    let access_token = apply_refresh(&mut credentials, &response, Utc::now().timestamp())?;
    persist_credentials_atomic(&path, &credentials)?;
    Ok(access_token)
}

/// Skip the network refresh when the stored refresh token already came back
/// `invalid_grant` and the credential file is unchanged since: the grant is
/// dead until the user signs in again, and re-posting it every cycle only
/// hammers the OAuth endpoints. A re-login rewrites the file, which changes
/// the mtime and re-arms the refresh.
fn guarded_refresh(
    home: &Path,
    rejected: &mut Option<SystemTime>,
    refresh: impl FnOnce(&Path) -> Result<String>,
) -> Result<String> {
    let mtime = fs::metadata(credentials_path(home))
        .and_then(|meta| meta.modified())
        .ok();
    if rejected.is_some() && *rejected == mtime {
        bail!(
            "kimi refresh token was rejected (invalid_grant) and the credential file is unchanged; run `kimi login` to sign in again"
        );
    }
    match refresh(home) {
        Ok(token) => {
            *rejected = None;
            Ok(token)
        }
        Err(error) => {
            if is_invalid_grant(&error) {
                *rejected = mtime;
            }
            Err(error)
        }
    }
}

#[derive(Debug)]
pub enum FetchError {
    Status(reqwest::StatusCode, String),
    Other(anyhow::Error),
}

impl FetchError {
    fn is_unauthorized(&self) -> bool {
        matches!(self, Self::Status(status, _) if *status == reqwest::StatusCode::UNAUTHORIZED)
    }
}

impl fmt::Display for FetchError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Status(status, body) => {
                write!(formatter, "kimi usage request failed ({status}): {body}")
            }
            Self::Other(error) => error.fmt(formatter),
        }
    }
}

impl std::error::Error for FetchError {}

const RESPONSE_BODY_LIMIT: usize = 1_000;

fn redact_json_secrets(value: &mut Value) {
    match value {
        Value::Object(object) => {
            for (key, value) in object {
                let key = key.to_ascii_lowercase();
                if key.contains("token")
                    || key.contains("authorization")
                    || key.contains("api_key")
                    || key.contains("secret")
                    || key.contains("password")
                    || key.contains("cookie")
                {
                    *value = Value::String("[REDACTED]".to_string());
                } else {
                    redact_json_secrets(value);
                }
            }
        }
        Value::Array(values) => {
            for value in values {
                redact_json_secrets(value);
            }
        }
        _ => {}
    }
}

fn response_body_excerpt(body: &str) -> String {
    let excerpt: String = body.trim().chars().take(RESPONSE_BODY_LIMIT).collect();
    let mut value = match serde_json::from_str::<Value>(&excerpt) {
        Ok(value) => value,
        Err(_) => return excerpt,
    };
    redact_json_secrets(&mut value);
    serde_json::to_string(&value).unwrap_or(excerpt)
}

fn decode_usages(body: &str) -> std::result::Result<UsagesResponse, FetchError> {
    serde_json::from_str(body).map_err(|error| {
        FetchError::Other(anyhow!(error).context(format!(
            "failed to parse kimi usage payload; response body: {}",
            response_body_excerpt(body)
        )))
    })
}

pub fn fetch_usages(token: &str) -> std::result::Result<UsagesResponse, FetchError> {
    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
        .map_err(|error| {
            FetchError::Other(anyhow!(error).context("failed to build HTTP client"))
        })?;
    let response = client
        .get(USAGES_URL)
        .bearer_auth(token)
        .send()
        .map_err(|error| {
            FetchError::Other(anyhow!(error).context("failed to reach api.kimi.com"))
        })?;
    let status = response.status();
    let body = response.text().map_err(|error| {
        FetchError::Other(anyhow!(error).context("failed to read kimi usage response"))
    })?;
    if !status.is_success() {
        return Err(FetchError::Status(status, response_body_excerpt(&body)));
    }
    decode_usages(&body)
}

fn fetch_with_refresh<F, R>(
    token: String,
    cli_home: Option<&Path>,
    mut fetch: F,
    mut refresh: R,
) -> Result<UsagesResponse>
where
    F: FnMut(&str) -> std::result::Result<UsagesResponse, FetchError>,
    R: FnMut(&Path) -> Result<String>,
{
    let mut token = token;
    let mut retries = 0;
    loop {
        match fetch(&token) {
            Ok(usages) => return Ok(usages),
            Err(error) if error.is_unauthorized() && retries < MAX_REQUEST_RETRIES => {
                let Some(home) = cli_home else {
                    return Err(error.into());
                };
                token = refresh(home)?;
                retries += 1;
            }
            Err(error) => return Err(error.into()),
        }
    }
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
        out.push(window_data(
            "Weekly".to_string(),
            usage,
            Some(WEEK_MINUTES),
            now,
        )?);
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
        expected_percent: pace.as_ref().map(|pace| pace.expected_percent),
        delta_percent: pace.as_ref().map(|pace| pace.delta_percent),
        resets_in_minutes,
        will_last_to_reset: pace.as_ref().map(|pace| pace.will_last_to_reset),
        eta_minutes: pace.as_ref().and_then(|pace| pace.eta_minutes),
    })
}

pub struct Kimi {
    api_key: Option<String>,
    windows: Vec<UsageWindowData>,
    fetched_at: Option<String>,
    /// Credential-file mtime whose refresh token was rejected with
    /// `invalid_grant`; suppresses further refresh attempts until it changes.
    rejected_credentials: Option<SystemTime>,
}

impl Kimi {
    pub fn new(api_key: Option<String>) -> Self {
        Self {
            api_key,
            windows: Vec::new(),
            fetched_at: None,
            rejected_credentials: None,
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
    fn collect(&mut self) -> Result<()> {
        let (mut token, cli_home) = resolve_token(self.api_key.as_deref())?;
        let rejected = &mut self.rejected_credentials;
        let mut refresh = |home: &Path| guarded_refresh(home, rejected, refresh_cli_token);
        if let Some(home) = cli_home.as_deref()
            && let Some(credentials) = cli_credentials(home)
        {
            let expired = credentials
                .expires_at
                .is_some_and(|expires_at| expires_at <= Utc::now().timestamp());
            if expired {
                token = refresh(home)?;
            }
        }
        let usages = fetch_with_refresh(token, cli_home.as_deref(), fetch_usages, refresh)?;
        let now = Utc::now();
        self.windows = windows(&usages, now)?;
        // The payload carries no "generated at", so the header shows when this
        // cycle fetched it.
        self.fetched_at = Some(now.to_rfc3339());
        Ok(())
    }

    fn render(&self) -> Result<RgbaImage> {
        agents_usage_ui::render_usage_bars(
            "Kimi Code",
            icon(),
            &self.windows,
            self.fetched_at.as_deref(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

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
        let now = DateTime::parse_from_rfc3339(iso)
            .unwrap()
            .with_timezone(&Utc);
        windows(&usages, now).unwrap()
    }

    #[test]
    fn parse_failure_includes_a_redacted_response_body() {
        let error = decode_usages(
            r#"{"limits":"unexpected","authorization":"Bearer request-token","nested":{"api_key":"api-secret"}}"#,
        )
        .unwrap_err()
        .to_string();

        assert!(error.contains("response body"));
        assert!(error.contains("[REDACTED]"));
        assert!(!error.contains("request-token"));
        assert!(!error.contains("api-secret"));
    }

    #[test]
    fn parses_remaining_only_rate_limit_detail() {
        let usages = decode_usages(
            r#"{
                "usage": {"limit": "100", "used": "49", "remaining": "51"},
                "limits": [{
                    "window": {"duration": 300, "timeUnit": "TIME_UNIT_MINUTE"},
                    "detail": {"limit": "100", "remaining": "100"}
                }]
            }"#,
        )
        .unwrap();
        let windows = windows(&usages, Utc::now()).unwrap();

        assert_eq!(windows.len(), 2);
        assert_eq!(windows[0].label, "5h");
        assert_eq!(windows[0].utilization, 0.0);
        assert_eq!(windows[1].utilization, 49.0);
    }

    fn empty_usages() -> UsagesResponse {
        UsagesResponse {
            usage: None,
            limits: Vec::new(),
        }
    }

    fn temporary_dir(label: &str) -> PathBuf {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let root = env::temp_dir().join(format!(
            "geekmagic-kimi-{label}-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        root
    }

    #[test]
    fn embedded_icon_is_valid() {
        let icon = icon();
        assert_eq!(icon.dimensions(), (18, 18));
        assert!(icon.pixels().any(|pixel| pixel[3] != 0));
    }

    #[test]
    fn keeps_only_the_tightest_rate_limit_window_and_the_weekly_pool() {
        let windows = sample_at("2026-01-06T11:33:02Z");
        let labels: Vec<&str> = windows.iter().map(|window| window.label.as_str()).collect();
        assert_eq!(labels, ["5h", "Weekly"], "wider 24h window must be dropped");
        assert!(
            (windows[0].utilization - 69.5).abs() < 0.01,
            "139/200 => 69.5%"
        );
        assert!((windows[0].resets_in_minutes.unwrap() - 120.0).abs() < 0.05);
        assert!(
            (windows[1].utilization - 10.449).abs() < 0.01,
            "214/2048 => 10.45%"
        );
    }

    #[test]
    fn derives_pace_and_usage_level_from_raw_counts() {
        let windows = sample_at("2026-01-06T11:33:02Z");
        assert_eq!(windows[0].usage_level, "moderate");
        assert!(windows[0].delta_percent.unwrap() > 0.0, "used above pace");
        assert_eq!(windows[0].will_last_to_reset, Some(false));
        assert!(windows[0].eta_minutes.is_some());
        assert_eq!(windows[1].will_last_to_reset, Some(true));
        assert!(windows[1].delta_percent.unwrap() < 0.0);
    }

    #[test]
    fn expired_reset_times_drop_pace_instead_of_faking_it() {
        let windows = sample_at("2026-02-01T00:00:00Z");
        assert!(
            windows
                .iter()
                .all(|window| window.resets_in_minutes.is_none())
        );
        assert!(windows.iter().all(|window| window.delta_percent.is_none()));
    }

    #[test]
    fn reports_missing_credentials_instead_of_calling_the_api() {
        temp_env_clear();
        let error = resolve_token(None).unwrap_err().to_string();
        assert!(
            error.contains("KIMI_CODE_API_KEY"),
            "unhelpful error: {error}"
        );
        assert_eq!(
            resolve_token(Some(" sk-kimi-x ")).unwrap(),
            ("sk-kimi-x".to_string(), None)
        );
    }

    #[test]
    fn refresh_response_rotates_tokens_and_computes_expiry() {
        let mut credentials = json!({
            "access_token": "old-access",
            "refresh_token": "old-refresh",
            "expires_at": 1,
            "expires_in": 1,
            "scope": "coding",
            "token_type": "Bearer"
        });
        let token = apply_refresh(
            &mut credentials,
            &json!({"access_token": "new-access", "refresh_token": "new-refresh", "expires_in": 900}),
            1_000,
        )
        .unwrap();
        assert_eq!(token, "new-access");
        assert_eq!(credentials["access_token"], "new-access");
        assert_eq!(credentials["refresh_token"], "new-refresh");
        assert_eq!(credentials["expires_in"], 900);
        assert_eq!(credentials["expires_at"], 1_900);
    }

    #[test]
    fn refresh_response_without_rotation_keeps_refresh_token() {
        let mut credentials = json!({"access_token": "old", "refresh_token": "keep"});
        apply_refresh(&mut credentials, &json!({"access_token": "new"}), 1_000).unwrap();
        assert_eq!(credentials["refresh_token"], "keep");
    }

    #[test]
    fn cli_credentials_normalizes_millisecond_expiry() {
        let root = temporary_dir("milliseconds");
        fs::create_dir_all(root.join("credentials")).unwrap();
        fs::write(
            root.join("credentials/kimi-code.json"),
            r#"{"access_token":"access","refresh_token":"refresh","expires_at":1787869243000}"#,
        )
        .unwrap();
        let credentials = cli_credentials(&root).unwrap();
        assert_eq!(credentials.expires_at, Some(1_787_869_243));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn oauth_hosts_follow_region_file() {
        let root = temporary_dir("region");
        fs::write(root.join("region"), "mainland-cn\n").unwrap();
        let hosts = oauth_hosts(&root);
        assert_eq!(hosts.primary, "https://auth.kimi.com");
        assert_eq!(hosts.alternate, Some("https://auth.kimi.ai"));
        fs::write(root.join("region"), "overseas").unwrap();
        let hosts = oauth_hosts(&root);
        assert_eq!(hosts.primary, "https://auth.kimi.ai");
        assert_eq!(hosts.alternate, Some("https://auth.kimi.com"));
        fs::remove_file(root.join("region")).unwrap();
        let hosts = oauth_hosts(&root);
        assert_eq!(hosts.primary, "https://auth.kimi.com");
        assert_eq!(hosts.alternate, None);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn unauthorized_only_for_401() {
        assert!(
            FetchError::Status(reqwest::StatusCode::UNAUTHORIZED, String::new()).is_unauthorized()
        );
        assert!(
            !FetchError::Status(reqwest::StatusCode::INTERNAL_SERVER_ERROR, String::new())
                .is_unauthorized()
        );
        assert!(!FetchError::Other(anyhow!("network")).is_unauthorized());
    }

    #[test]
    fn cli_retry_succeeds_after_401_refresh() {
        let mut fetches = 0;
        let mut refreshes = 0;
        let result = fetch_with_refresh(
            "old".to_string(),
            Some(Path::new("/test-kimi-home")),
            |_| {
                fetches += 1;
                if fetches == 1 {
                    Err(FetchError::Status(
                        reqwest::StatusCode::UNAUTHORIZED,
                        String::new(),
                    ))
                } else {
                    Ok(empty_usages())
                }
            },
            |_| {
                refreshes += 1;
                Ok("new".to_string())
            },
        );
        assert!(result.is_ok());
        assert_eq!((fetches, refreshes), (2, 1));
    }

    #[test]
    fn refresh_error_tags_invalid_grant() {
        let error = refresh_response_error(
            reqwest::StatusCode::BAD_REQUEST,
            r#"{"error":"invalid_grant","error_description":"The provided authorization grant is invalid"}"#,
        );
        assert!(is_invalid_grant(&error));
        assert!(format!("{error:#}").contains("kimi login"));

        let other = refresh_response_error(reqwest::StatusCode::BAD_GATEWAY, "upstream error");
        assert!(!is_invalid_grant(&other));
    }

    #[test]
    fn invalid_grant_suppresses_refresh_until_credentials_change() {
        let root = temporary_dir("invalid-grant");
        fs::create_dir_all(root.join("credentials")).unwrap();
        let path = credentials_path(&root);
        fs::write(&path, "{}").unwrap();
        let set_mtime = |seconds: u64| {
            OpenOptions::new()
                .write(true)
                .open(&path)
                .unwrap()
                .set_modified(SystemTime::UNIX_EPOCH + Duration::from_secs(seconds))
                .unwrap();
        };
        set_mtime(1_000);

        let mut rejected = None;
        let error = guarded_refresh(&root, &mut rejected, |_| {
            Err(refresh_response_error(
                reqwest::StatusCode::BAD_REQUEST,
                r#"{"error":"invalid_grant"}"#,
            ))
        })
        .unwrap_err();
        assert!(is_invalid_grant(&error));
        assert!(rejected.is_some());

        // Unchanged credential file: fail fast, no refresh attempt.
        let error = guarded_refresh(&root, &mut rejected, |_| {
            panic!("refresh must not run while the credential file is unchanged")
        })
        .unwrap_err();
        assert!(error.to_string().contains("kimi login"));

        // A re-login rewrites the file; the guard re-arms and success clears it.
        set_mtime(2_000);
        let token = guarded_refresh(&root, &mut rejected, |_| Ok("fresh".to_string())).unwrap();
        assert_eq!(token, "fresh");
        assert!(rejected.is_none());
    }

    #[test]
    fn transient_refresh_failure_does_not_arm_the_guard() {
        let root = temporary_dir("transient");
        fs::create_dir_all(root.join("credentials")).unwrap();
        fs::write(credentials_path(&root), "{}").unwrap();

        let mut rejected = None;
        let _ = guarded_refresh(&root, &mut rejected, |_| {
            Err(anyhow!("operation timed out"))
        })
        .unwrap_err();
        assert!(rejected.is_none());

        // Next cycle still attempts the refresh.
        let token = guarded_refresh(&root, &mut rejected, |_| Ok("token".to_string())).unwrap();
        assert_eq!(token, "token");
    }

    #[test]
    fn cli_retry_stops_after_two_refreshes() {
        let mut fetches = 0;
        let mut refreshes = 0;
        let error = fetch_with_refresh(
            "old".to_string(),
            Some(Path::new("/test-kimi-home")),
            |_| {
                fetches += 1;
                Err(FetchError::Status(
                    reqwest::StatusCode::UNAUTHORIZED,
                    String::new(),
                ))
            },
            |_| {
                refreshes += 1;
                Ok(format!("new-{refreshes}"))
            },
        )
        .unwrap_err();
        assert!(error.to_string().contains("401"));
        assert_eq!((fetches, refreshes), (3, 2));
    }

    #[test]
    fn static_token_does_not_refresh() {
        let mut fetches = 0;
        let mut refreshes = 0;
        let _ = fetch_with_refresh(
            "static".to_string(),
            None,
            |_| {
                fetches += 1;
                Err(FetchError::Status(
                    reqwest::StatusCode::UNAUTHORIZED,
                    String::new(),
                ))
            },
            |_| {
                refreshes += 1;
                Ok("never".to_string())
            },
        );
        assert_eq!((fetches, refreshes), (1, 0));
    }

    #[test]
    fn atomic_write_replaces_credentials_and_preserves_unknown_keys() {
        let root = temporary_dir("atomic");
        let path = root.join("kimi-code.json");
        fs::write(&path, r#"{"access_token":"old","unknown":{"keep":true}}"#).unwrap();
        let credentials = json!({"access_token": "new", "unknown": {"keep": true}});
        persist_credentials_atomic(&path, &credentials).unwrap();
        let persisted: Value = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(persisted["access_token"], "new");
        assert_eq!(persisted["unknown"]["keep"], true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
        fs::remove_dir_all(root).unwrap();
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
