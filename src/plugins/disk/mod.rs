pub mod render;

use std::path::Path;

use anyhow::{Context, Result};
use image::RgbaImage;

use crate::plugin::Plugin;

pub struct DiskInfo {
    pub total_bytes: u64,
    pub free_bytes: u64,
    pub used_bytes: u64,
}

/// Cross-platform disk collection via sysinfo — replaces the base's
/// macOS-only `diskutil` parser. Same code path on every OS.
pub fn get_disk_info() -> Result<DiskInfo> {
    let disks = sysinfo::Disks::new_with_refreshed_list();
    let root = if cfg!(windows) {
        Path::new("C:\\")
    } else {
        Path::new("/")
    };
    let disk = disks
        .iter()
        .find(|d| d.mount_point() == root)
        .or_else(|| disks.iter().next())
        .context("no disks found")?;

    let total = disk.total_space();
    let free = disk.available_space();
    Ok(DiskInfo {
        total_bytes: total,
        free_bytes: free,
        used_bytes: total.saturating_sub(free),
    })
}

pub fn format_size(bytes: u64) -> String {
    let gb = bytes as f64 / 1_000_000_000.0;
    if gb >= 1000.0 {
        format!("{:.1} TB", gb / 1000.0)
    } else if gb >= 100.0 {
        format!("{:.0} GB", gb)
    } else if gb >= 10.0 {
        format!("{:.1} GB", gb)
    } else {
        format!("{:.2} GB", gb)
    }
}

pub struct Disk {
    info: Option<DiskInfo>,
}

impl Disk {
    pub fn new() -> Self {
        Self { info: None }
    }
}

impl Plugin for Disk {
    fn name(&self) -> &'static str {
        "disk"
    }

    fn filename(&self) -> &'static str {
        "disk.jpg"
    }

    fn collect(&mut self) -> Result<()> {
        self.info = Some(get_disk_info()?);
        Ok(())
    }

    fn render(&self) -> Result<RgbaImage> {
        let info = self.info.as_ref().context("collect() has not run")?;
        let free_percent = info.free_bytes as f64 / info.total_bytes as f64 * 100.0;
        render::render_donut(
            free_percent,
            &format_size(info.used_bytes),
            &format_size(info.free_bytes),
        )
    }
}
