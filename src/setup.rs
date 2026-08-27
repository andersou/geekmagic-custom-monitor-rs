//! Interactive `setup` interview: prompts for every config field, Enter accepts
//! the bracketed default. Defaults come from the existing config file when it
//! exists, else built-ins — re-running `setup` edits rather than resets.

use std::io::{self, Write};

use anyhow::{Context, Result};

use crate::config::{self, AppConfig, PluginCfg};
use crate::plugins;
use crate::upload;

fn prompt(label: &str, default: Option<&str>) -> Result<String> {
    match default {
        Some(d) => print!("{label} [{d}]: "),
        None => print!("{label}: "),
    }
    io::stdout().flush()?;
    let mut line = String::new();
    io::stdin().read_line(&mut line)?;
    let v = line.trim();
    Ok(match (v.is_empty(), default) {
        (true, Some(d)) => d.to_string(),
        (true, None) => String::new(),
        (false, _) => v.to_string(),
    })
}

fn prompt_bool(label: &str, default: bool) -> Result<bool> {
    let hint = if default { "Y/n" } else { "y/N" };
    let answer = prompt(label, Some(hint))?;
    if answer.eq_ignore_ascii_case(hint) {
        return Ok(default); // bare Enter
    }
    Ok(matches!(answer.to_ascii_lowercase().as_str(), "y" | "yes"))
}

pub fn run(config_path: Option<&str>) -> Result<()> {
    let path = config::resolve_path(config_path);
    let existing: AppConfig = if path.exists() {
        config::load_from_path(&path)?
    } else {
        AppConfig::default()
    };

    let host = prompt("device host/IP (empty = unset)", existing.host.as_deref())?;

    let model = prompt("model (auto | ultra | pro)", Some(existing.model.as_deref().unwrap_or("auto")))?;

    let interval_default = existing.interval.map(|i| i.to_string());
    let interval = prompt(
        "daemon interval seconds (empty/0 = one-shot)",
        Some(interval_default.as_deref().unwrap_or("300")),
    )?;
    let interval: Option<u64> = match interval.parse::<u64>() {
        Ok(0) => None,
        Ok(n) => Some(n),
        Err(_) if interval.is_empty() => None,
        Err(_) => anyhow::bail!("invalid interval '{interval}'"),
    };

    let parallel = prompt_bool("parallel render?", existing.parallel_render.unwrap_or(false))?;

    let autoplay_default = existing.autoplay_interval.unwrap_or(10).to_string();
    let autoplay = prompt("device slideshow seconds", Some(&autoplay_default))?;
    let autoplay: u64 = autoplay
        .parse()
        .with_context(|| format!("invalid autoplay interval '{autoplay}'"))?;
    let image_mode = prompt(
        "image_mode (append | only-stats)",
        Some(existing.image_mode.as_deref().unwrap_or("append")),
    )?;
    if upload::ImageMode::parse(&image_mode).is_none() {
        anyhow::bail!("invalid image_mode '{image_mode}'");
    }
    let retention_default = existing.backup_retention.unwrap_or(5).to_string();
    let backup_retention = prompt("backup directories to keep", Some(&retention_default))?;
    let backup_retention: usize = backup_retention
        .parse()
        .with_context(|| format!("invalid backup retention '{backup_retention}'"))?;
    if backup_retention == 0 {
        anyhow::bail!("backup retention must be at least 1");
    }

    let mut plugin_configs = std::collections::BTreeMap::new();
    for name in plugins::known_plugin_names() {
        let default = existing
            .plugins
            .as_ref()
            .and_then(|plugins| plugins.get(*name))
            .and_then(|plugin| plugin.enabled)
            .unwrap_or(true);
        let enabled = prompt_bool(&format!("enable plugin '{name}'?"), default)?;
        plugin_configs.insert((*name).to_string(), PluginCfg { enabled: Some(enabled) });
    }

    let output = AppConfig {
        host: (!host.is_empty()).then_some(host),
        model: Some(model),
        interval,
        parallel_render: Some(parallel),
        autoplay_interval: Some(autoplay),
        image_mode: Some(image_mode),
        backup_retention: Some(backup_retention),
        plugins: Some(plugin_configs),
    };
    let out = toml::to_string_pretty(&output).context("failed to serialize config")?;

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("failed to create config dir {}", parent.display()))?;
    }
    std::fs::write(&path, &out)
        .with_context(|| format!("failed to write config at {}", path.display()))?;
    println!("wrote {}", path.display());
    Ok(())
}
