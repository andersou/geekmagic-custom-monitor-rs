use std::collections::BTreeMap;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

pub const DEFAULT_FAILURE_THRESHOLD: u32 = 5;

#[derive(Debug, Default, Deserialize, Serialize, Clone)]
pub struct AppConfig {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub host: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>, // "auto" | "ultra" | "pro"; default auto
    #[serde(skip_serializing_if = "Option::is_none")]
    pub interval: Option<u64>, // daemon seconds; absent = one-shot
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parallel_render: Option<bool>, // default false
    #[serde(skip_serializing_if = "Option::is_none")]
    pub autoplay_interval: Option<u64>, // device slideshow seconds, default 10
    #[serde(skip_serializing_if = "Option::is_none")]
    pub image_mode: Option<String>, // "append" (default) | "only-stats"
    #[serde(skip_serializing_if = "Option::is_none")]
    pub backup_retention: Option<usize>, // backup directories kept, default 5
    #[serde(skip_serializing_if = "Option::is_none")]
    pub failure_threshold: Option<u32>, // consecutive failed cycles before a plugin error screen
    #[serde(skip_serializing_if = "Option::is_none")]
    pub plugins: Option<BTreeMap<String, PluginCfg>>, // [plugins.<name>] enabled = bool
}

#[derive(Debug, Default, Deserialize, Serialize, Clone)]
pub struct PluginCfg {
    pub enabled: Option<bool>, // absent section or absent key => enabled
    #[serde(skip_serializing_if = "Option::is_none")]
    pub api_key: Option<String>, // credential for plugins that need one
}

impl AppConfig {
    /// Credential configured for a plugin, if any. Plugins fall back to their
    /// own environment variables when this is absent.
    pub fn plugin_api_key(&self, plugin: &str) -> Option<String> {
        self.plugins
            .as_ref()?
            .get(plugin)?
            .api_key
            .as_ref()
            .map(|k| k.trim().to_string())
            .filter(|k| !k.is_empty())
    }
}

fn expand_home(path: &str) -> PathBuf {
    if path == "~" {
        return env::var("HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from(path));
    }

    if let Some(rest) = path.strip_prefix("~/") {
        if let Ok(home) = env::var("HOME") {
            return PathBuf::from(home).join(rest);
        }
    }

    PathBuf::from(path)
}

pub fn config_root() -> Result<PathBuf> {
    let home = env::var("HOME").context("HOME not set")?;
    Ok(PathBuf::from(home)
        .join(".config")
        .join("geekmagic-custom-monitors"))
}

pub fn default_config_path() -> PathBuf {
    config_root()
        .unwrap_or_else(|_| PathBuf::from(".config").join("geekmagic-custom-monitors"))
        .join("config.toml")
}

pub fn resolve_path(path_override: Option<&str>) -> PathBuf {
    path_override.map(expand_home).unwrap_or_else(default_config_path)
}

/// TOML written when the config file does not exist yet: built-in defaults,
/// optional keys commented out, every known plugin enabled.
fn default_config_toml() -> String {
    let mut out = String::from(concat!(
        "# geekmagic-custom-monitors configuration\n",
        "# host = \"192.168.1.201\"   # GeekMagic device IP\n",
        "# model = \"auto\"           # auto | ultra | pro\n",
        "# interval = 300            # daemon seconds; omit for one-shot\n",
        "# parallel_render = false\n",
        "# autoplay_interval = 10    # device slideshow seconds\n",
        "# image_mode = \"append\"  # append | only-stats\n",
        "backup_retention = 5      # backup directories kept\n",
        "# failure_threshold = 5  # failed cycles before a plugin shows an error screen\n",
    ));
    for name in crate::plugins::ui_plugin_names() {
        out.push_str(&format!("\n[plugins.{name}]\nenabled = true\n"));
    }
    out.push_str(&format!(
        "\n# renderer plugins (always available, not toggleable): {}\n",
        crate::plugins::renderer_plugin_names().join(", ")
    ));
    out
}

/// Create the config file (with parent dirs) containing built-in defaults.
pub fn create_default(path: &Path) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create config dir {}", parent.display()))?;
    }
    fs::write(path, default_config_toml())
        .with_context(|| format!("failed to write default config at {}", path.display()))
}

pub fn load(path_override: Option<&str>) -> Result<AppConfig> {
    let path = resolve_path(path_override);
    if !path.exists() {
        // Autostarted bare `run` always ends up with a file on disk; host
        // stays unset until `setup` (or a hand edit) fills it.
        create_default(&path)?;
        return Ok(AppConfig::default());
    }
    load_from_path(&path)
}

pub fn load_from_path(path: &Path) -> Result<AppConfig> {
    let raw = fs::read_to_string(path)
        .with_context(|| format!("failed to read config at {}", path.display()))?;
    toml::from_str(&raw).with_context(|| format!("failed to parse config at {}", path.display()))
}
