/// UI plugins: one screen each, toggleable under `[plugins.<name>]`.
pub mod claude;
pub mod codex;
pub mod disk;
pub mod kimi;

/// Renderer plugins: shared drawing code imported by UI plugins.
pub mod agents_usage_ui;

use std::collections::HashSet;

use crate::config::AppConfig;
use crate::plugin::{Plugin, PluginKind, UiPlugin};

/// Every UI plugin, before the enabled filter. `cfg` supplies credentials.
fn ui_plugins(cfg: &AppConfig) -> Vec<Box<dyn UiPlugin>> {
    vec![
        Box::new(claude::Claude::new()),
        Box::new(codex::Codex::new()),
        Box::new(disk::Disk::new()),
        Box::new(kimi::Kimi::new(cfg.plugin_api_key("kimi"))),
    ]
}

/// Every plugin of every kind, for `setup` and config generation. Built from
/// default config: metadata only, no I/O and no credentials.
pub fn catalog() -> Vec<Box<dyn Plugin>> {
    let mut all: Vec<Box<dyn Plugin>> = ui_plugins(&AppConfig::default())
        .into_iter()
        .map(|p| p as Box<dyn Plugin>)
        .collect();
    all.push(Box::new(agents_usage_ui::AgentsUsageUi));
    all
}

pub fn ui_plugin_names() -> Vec<&'static str> {
    catalog()
        .iter()
        .filter(|p| p.get_plugin_kind() == PluginKind::Ui)
        .map(|p| p.name())
        .collect()
}

pub fn renderer_plugin_names() -> Vec<&'static str> {
    catalog()
        .iter()
        .filter(|p| p.get_plugin_kind() == PluginKind::Renderer)
        .map(|p| p.name())
        .collect()
}

/// Build the enabled plugin list. Runs once at init; the returned Vec is kept
/// for the daemon's whole lifetime — plugins are never re-instantiated per
/// cycle.
pub fn registry(cfg: &AppConfig) -> Vec<Box<dyn UiPlugin>> {
    let mut enabled: Vec<Box<dyn UiPlugin>> = ui_plugins(cfg)
        .into_iter()
        .filter(|p| {
            cfg.plugins
                .as_ref()
                .and_then(|m| m.get(p.name()))
                .and_then(|c| c.enabled)
                .unwrap_or(true)
        })
        .collect();

    // Dependency validation, fixpoint: drop plugins whose depends_on names are
    // not all enabled, warning each time. Covers plugin-instance dependencies
    // only — renderer imports are compile-time guaranteed.
    loop {
        let names: HashSet<&'static str> = enabled.iter().map(|p| p.name()).collect();
        let mut dropped = false;
        let mut i = 0;
        while i < enabled.len() {
            if let Some(dep) = enabled[i]
                .depends_on()
                .iter()
                .find(|d| !names.contains(**d))
            {
                eprintln!(
                    "plugin '{}' disabled: depends on '{dep}', which is disabled or unknown",
                    enabled[i].name()
                );
                enabled.remove(i);
                dropped = true;
            } else {
                i += 1;
            }
        }
        if !dropped {
            break;
        }
    }

    enabled
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config_enables_every_ui_plugin_and_no_renderer() {
        let built: Vec<&str> = registry(&AppConfig::default())
            .iter()
            .map(|p| p.name())
            .collect();
        assert_eq!(built, ui_plugin_names());
        assert!(built.contains(&"codex"));
        assert!(!built.contains(&"agents-usage-ui"));
    }
}
