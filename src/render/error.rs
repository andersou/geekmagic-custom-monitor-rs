//! App-owned screen shown after a UI plugin repeatedly fails to collect or render.

use ab_glyph::PxScale;
use image::{Rgba, RgbaImage};
use imageproc::drawing::draw_text_mut;

use crate::render::common::{
    self, BG, H, PANEL_BG, SEPARATOR, TEXT_MUTED, TEXT_PRIMARY, W,
};

const DANGER: Rgba<u8> = Rgba([239, 68, 68, 255]);

/// App-native screen replacing a plugin's image after `failure_threshold`
/// consecutive failed cycles: names the plugin and the failed attempts.
pub fn render_plugin_error(plugin_name: &str, failed_cycles: u32) -> RgbaImage {
    let mut image = RgbaImage::from_pixel(W, H, BG);
    let bold = common::font_bold();
    let regular = common::font_regular();

    draw_text_mut(
        &mut image,
        TEXT_PRIMARY,
        12,
        18,
        PxScale::from(24.0),
        bold,
        plugin_name,
    );
    common::draw_rounded_rect(&mut image, 12, 51, 216, 1, 0, SEPARATOR);
    common::draw_rounded_rect(&mut image, 12, 64, 216, 132, 12, PANEL_BG);
    common::draw_text_centered(
        &mut image,
        DANGER,
        120,
        88,
        20.0,
        bold,
        "data collection failing",
    );
    common::draw_text_centered(
        &mut image,
        TEXT_PRIMARY,
        120,
        126,
        18.0,
        regular,
        &format!("{failed_cycles} failed attempts"),
    );
    common::draw_text_centered(
        &mut image,
        TEXT_MUTED,
        120,
        158,
        16.0,
        regular,
        "check credentials / network",
    );
    image
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn error_screen_fits_and_marks_failure() {
        let image = render_plugin_error("kimi", 2);
        assert_eq!(image.dimensions(), (W, H));
        let changed = image.pixels().filter(|pixel| **pixel != BG).count();
        assert!(changed >= 500, "screen is unexpectedly sparse: {changed} changed pixels");
        assert!(
            (12..228).flat_map(|x| (64..196).map(move |y| (x, y))).any(|(x, y)| image.get_pixel(x, y) == &DANGER),
            "panel contains no danger-colored text"
        );
    }
}
