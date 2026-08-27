pub mod claude;
pub mod disk;

use std::collections::HashSet;

use crate::config::AppConfig;
use crate::plugin::Plugin;

/// Every plugin name the binary knows about, enabled or not (used by `setup`).
pub fn known_plugin_names() -> &'static [&'static str] {
    &["claude", "disk"]
}

/// Build the enabled plugin list. Runs once at init; the returned Vec is kept
/// for the daemon's whole lifetime — plugins are never re-instantiated per
/// cycle.
pub fn registry(cfg: &AppConfig) -> Vec<Box<dyn Plugin>> {
    let all: Vec<Box<dyn Plugin>> = vec![Box::new(claude::Claude::new()), Box::new(disk::Disk::new())];
    let mut enabled: Vec<Box<dyn Plugin>> = all
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
            if let Some(dep) = enabled[i].depends_on().iter().find(|d| !names.contains(**d)) {
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
