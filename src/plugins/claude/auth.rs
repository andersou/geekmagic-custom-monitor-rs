use anyhow::{Context, Result, bail};
use chrono::{DateTime, Duration, Utc};
use reqwest::blocking::Client;
use serde::Deserialize;
use serde_json::Value;
#[cfg(target_os = "linux")]
use std::fs;
#[cfg(target_os = "linux")]
use std::path::PathBuf;
#[cfg(target_os = "macos")]
use std::process::Command;
use std::time::Duration as StdDuration;

#[cfg(target_os = "macos")]
const KEYCHAIN_SERVICES: &[&str] = &[
    "Claude Code-credentials",
    "Claude Code-local-oauth-credentials",
    "Claude Code",
    "Claude Code-local-oauth",
];
const OAUTH_TOKEN_URL: &str = "https://console.anthropic.com/v1/oauth/token";
const OAUTH_CLIENT_ID: &str = "9d1c250a-e61b-44d9-88ed-5944d1962f5e";
const REFRESH_MARGIN: Duration = Duration::seconds(60);

#[derive(Clone, Debug)]
enum CredentialSource {
    #[cfg(target_os = "macos")]
    Keychain { service: String, account: String },
    #[cfg(target_os = "linux")]
    File(PathBuf),
    #[cfg(test)]
    Test,
}

#[derive(Debug)]
pub struct Credentials {
    source: CredentialSource,
    pub access_token: String,
    refresh_token: String,
    expires_at: DateTime<Utc>,
}

impl Credentials {
    pub fn expires_within_refresh_margin(&self, now: DateTime<Utc>) -> bool {
        self.expires_at <= now + REFRESH_MARGIN
    }

    fn from_payload(source: CredentialSource, payload: Value) -> Result<Self> {
        let oauth = payload
            .get("claudeAiOauth")
            .and_then(Value::as_object)
            .context("Claude credentials contain no claudeAiOauth object")?;
        let access_token = oauth
            .get("accessToken")
            .and_then(Value::as_str)
            .context("Claude credentials contain no access token")?
            .to_owned();
        let refresh_token = oauth
            .get("refreshToken")
            .and_then(Value::as_str)
            .context("Claude credentials contain no refresh token")?
            .to_owned();
        let expires_at_millis = oauth
            .get("expiresAt")
            .and_then(Value::as_i64)
            .context("Claude credentials contain no expiresAt timestamp")?;
        let expires_at = DateTime::from_timestamp_millis(expires_at_millis)
            .context("Claude credentials contain an invalid expiresAt timestamp")?;

        Ok(Self {
            source,
            access_token,
            refresh_token,
            expires_at,
        })
    }

    fn update_tokens(
        &mut self,
        access_token: String,
        refresh_token: String,
        expires_at: DateTime<Utc>,
    ) {
        self.access_token = access_token;
        self.refresh_token = refresh_token;
        self.expires_at = expires_at;
    }

    #[cfg(test)]
    pub fn for_test(expires_at: DateTime<Utc>) -> Self {
        Self {
            source: CredentialSource::Test,
            access_token: "stale".to_owned(),
            refresh_token: "refresh".to_owned(),
            expires_at,
        }
    }

    #[cfg(test)]
    pub fn set_access_token_for_test(&mut self, access_token: &str) {
        self.access_token = access_token.to_owned();
    }
}

#[derive(Deserialize)]
struct RefreshResponse {
    access_token: String,
    #[serde(default)]
    refresh_token: Option<String>,
    expires_in: i64,
}

pub fn load_credentials() -> Result<Credentials> {
    #[cfg(target_os = "macos")]
    {
        let account = std::env::var("USER").context("USER is not set for Keychain lookup")?;
        for service in KEYCHAIN_SERVICES {
            if let Ok(payload) = read_keychain(service, &account) {
                return Credentials::from_payload(
                    CredentialSource::Keychain {
                        service: (*service).to_owned(),
                        account,
                    },
                    parse_payload(&payload)?,
                );
            }
        }
        bail!("Claude OAuth credentials not found in the macOS Keychain")
    }

    #[cfg(target_os = "linux")]
    {
        let path = credentials_file()?;
        let payload = fs::read_to_string(&path).with_context(|| {
            format!("failed to read Claude credentials from {}", path.display())
        })?;
        Credentials::from_payload(CredentialSource::File(path), parse_payload(&payload)?)
    }

    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    bail!("Claude OAuth credentials are supported only on macOS and Linux")
}

pub fn refresh(credentials: &mut Credentials) -> Result<()> {
    let response = Client::builder()
        .timeout(StdDuration::from_secs(30))
        .build()
        .context("failed to build Claude OAuth refresh client")?
        .post(OAUTH_TOKEN_URL)
        .json(&serde_json::json!({
            "grant_type": "refresh_token",
            "refresh_token": credentials.refresh_token,
            "client_id": OAUTH_CLIENT_ID,
        }))
        .send()
        .context("failed to reach Claude OAuth token endpoint")?
        .error_for_status()
        .context("Claude OAuth token refresh was rejected")?
        .json::<RefreshResponse>()
        .context("failed to parse Claude OAuth refresh response")?;
    if response.expires_in <= 0 {
        bail!("Claude OAuth refresh response has a non-positive expires_in")
    }

    let refresh_token = response
        .refresh_token
        .unwrap_or_else(|| credentials.refresh_token.clone());
    let expires_at = Utc::now() + Duration::milliseconds(response.expires_in * 1_000);
    write_updated_payload(
        credentials,
        &response.access_token,
        &refresh_token,
        expires_at,
    )?;
    credentials.update_tokens(response.access_token, refresh_token, expires_at);
    Ok(())
}

fn write_updated_payload(
    credentials: &mut Credentials,
    access_token: &str,
    refresh_token: &str,
    expires_at: DateTime<Utc>,
) -> Result<()> {
    let payload = read_source(&credentials.source)?;
    let mut payload = parse_payload(&payload)?;
    let oauth = payload
        .get_mut("claudeAiOauth")
        .and_then(Value::as_object_mut)
        .context("Claude credentials contain no claudeAiOauth object during write-back")?;
    oauth.insert(
        "accessToken".to_owned(),
        Value::String(access_token.to_owned()),
    );
    oauth.insert(
        "refreshToken".to_owned(),
        Value::String(refresh_token.to_owned()),
    );
    oauth.insert(
        "expiresAt".to_owned(),
        Value::Number(expires_at.timestamp_millis().into()),
    );
    write_source(&credentials.source, &serde_json::to_string(&payload)?)
}

fn parse_payload(payload: &str) -> Result<Value> {
    serde_json::from_str(payload).context("failed to parse Claude credential JSON")
}

fn read_source(source: &CredentialSource) -> Result<String> {
    match source {
        #[cfg(target_os = "macos")]
        CredentialSource::Keychain { service, account } => read_keychain(service, account),
        #[cfg(target_os = "linux")]
        CredentialSource::File(path) => fs::read_to_string(path).with_context(|| {
            format!(
                "failed to re-read Claude credentials from {}",
                path.display()
            )
        }),
        #[cfg(test)]
        CredentialSource::Test => bail!("test credentials cannot be read"),
    }
}

fn write_source(source: &CredentialSource, payload: &str) -> Result<()> {
    match source {
        #[cfg(target_os = "macos")]
        CredentialSource::Keychain { service, account } => {
            let status = Command::new("security")
                .args([
                    "add-generic-password",
                    "-U",
                    "-a",
                    account,
                    "-s",
                    service,
                    "-w",
                    payload,
                ])
                .status()
                .context("failed to invoke security for Claude credential write-back")?;
            if !status.success() {
                bail!("security failed to update Claude OAuth credentials")
            }
            Ok(())
        }
        #[cfg(target_os = "linux")]
        CredentialSource::File(path) => {
            use std::os::unix::fs::PermissionsExt;
            fs::write(path, payload).with_context(|| {
                format!("failed to write Claude credentials to {}", path.display())
            })?;
            fs::set_permissions(path, fs::Permissions::from_mode(0o600)).with_context(|| {
                format!("failed to secure Claude credentials at {}", path.display())
            })
        }
        #[cfg(test)]
        CredentialSource::Test => bail!("test credentials cannot be written"),
    }
}

#[cfg(target_os = "macos")]
fn read_keychain(service: &str, account: &str) -> Result<String> {
    let output = Command::new("security")
        .args(["find-generic-password", "-a", account, "-s", service, "-w"])
        .output()
        .with_context(|| format!("failed to invoke security for Keychain service {service}"))?;
    if !output.status.success() {
        bail!("Keychain service {service} has no Claude credentials")
    }
    String::from_utf8(output.stdout)
        .context("Keychain Claude credentials are not UTF-8")
        .map(|payload| payload.trim_end().to_owned())
}

#[cfg(target_os = "linux")]
fn credentials_file() -> Result<PathBuf> {
    let home = std::env::var_os("HOME").context("HOME is not set for Claude credential lookup")?;
    Ok(PathBuf::from(home).join(".claude/.credentials.json"))
}
