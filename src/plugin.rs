use anyhow::Result;
use image::RgbaImage;

pub trait Plugin: Send {
    fn name(&self) -> &'static str; // TOML section key, e.g. "claude"
    fn filename(&self) -> &'static str; // device file, e.g. "claude.jpg"
    fn collect(&mut self) -> Result<()>; // fetch data into self
    fn render(&self) -> Result<RgbaImage>; // 240x240, uses crate::render renderers
    fn depends_on(&self) -> &'static [&'static str] {
        &[]
    } // plugins that must also be enabled
}
