use anyhow::Result;
use image::RgbaImage;

/// What a plugin is for. Only `Ui` plugins are toggleable in TOML.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PluginKind {
    /// Produces a 240x240 screen uploaded to the device, toggleable under
    /// `[plugins.<name>]`.
    Ui,
    /// Shared drawing code imported by UI plugins: no screen, never enabled,
    /// never uploaded.
    Renderer,
}

/// Identity carried by every plugin, whatever its kind.
pub trait Plugin: Send {
    fn name(&self) -> &'static str; // TOML section key, e.g. "claude"
    fn get_plugin_kind(&self) -> PluginKind;
    /// UI plugins that authenticate against a remote API: `setup` asks for a
    /// credential and `[plugins.<name>].api_key` is honoured.
    fn needs_api_key(&self) -> bool {
        false
    }
}

/// A screen: collected, rendered and uploaded once per cycle.
pub trait UiPlugin: Plugin {
    fn collect(&mut self) -> Result<()>; // fetch data into self
    fn render(&self) -> Result<RgbaImage>; // 240x240, uses crate::render primitives
    fn depends_on(&self) -> &'static [&'static str] {
        &[]
    } // plugins that must also be enabled
}
