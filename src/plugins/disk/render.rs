//! Reusable donut renderer. Takes plain values, never plugin types — importable
//! by any plugin regardless of TOML enablement.

use std::f64::consts::PI;

use ab_glyph::PxScale;
use anyhow::Result;
use image::{Rgba, RgbaImage};
use imageproc::drawing::draw_text_mut;

use crate::render::common::{
    self, BG, H, SEPARATOR, TEXT_DIM, TEXT_MUTED, TEXT_PRIMARY, W,
};

const PIE_USED: Rgba<u8> = Rgba([99, 102, 241, 255]);
const PIE_USED_2: Rgba<u8> = Rgba([139, 92, 246, 255]);
const PIE_FREE: Rgba<u8> = Rgba([34, 197, 94, 255]);
const PIE_FREE_2: Rgba<u8> = Rgba([16, 185, 129, 255]);
const PIE_BG: Rgba<u8> = Rgba([30, 30, 40, 255]);

pub fn render_donut(
    free_percent: f64,
    used_label: &str,
    free_label: &str,
) -> Result<RgbaImage> {
    let font = common::font_regular();
    let font_bold = common::font_bold();
    let mut img = RgbaImage::from_pixel(W, H, BG);

    let mx = 16i32;
    let right_edge = W as i32 - mx;
    let content_w = (right_edge - mx) as u32;

    // Header
    let header_y = 10;
    draw_text_mut(
        &mut img,
        TEXT_PRIMARY,
        mx,
        header_y,
        PxScale::from(17.0),
        font_bold,
        "Disk",
    );
    common::draw_text_right(
        &mut img,
        TEXT_DIM,
        right_edge,
        header_y + 1,
        15.0,
        font,
        free_label,
    );

    common::draw_rounded_rect(&mut img, mx, 33, content_w, 1, 0, SEPARATOR);

    // Pie chart
    let pie_cx = 120.0f64;
    let pie_cy = 118.0f64;
    let pie_r_outer = 68.0f64;
    let pie_r_inner = 42.0f64;

    let free_frac = (free_percent / 100.0).clamp(0.0, 1.0);
    let used_frac = 1.0 - free_frac;
    let used_angle = used_frac * 2.0 * PI;

    for py in 0..H {
        for px in 0..W {
            let dx = px as f64 - pie_cx;
            let dy = py as f64 - pie_cy;
            let dist = (dx * dx + dy * dy).sqrt();

            if dist >= pie_r_inner && dist <= pie_r_outer {
                let angle = (dx.atan2(-dy) + 2.0 * PI) % (2.0 * PI);

                let edge_outer = (pie_r_outer - dist).clamp(0.0, 1.0) as f32;
                let edge_inner = (dist - pie_r_inner).clamp(0.0, 1.0) as f32;
                let aa = edge_outer.min(edge_inner);

                let base_color = if angle < used_angle {
                    let t = (angle / used_angle) as f32;
                    common::lerp_color(PIE_USED, PIE_USED_2, t)
                } else {
                    let t = ((angle - used_angle) / (2.0 * PI - used_angle)) as f32;
                    common::lerp_color(PIE_FREE, PIE_FREE_2, t)
                };

                let depth = ((dist - pie_r_inner) / (pie_r_outer - pie_r_inner)) as f32;
                let lit = common::lerp_color(
                    Rgba([
                        (base_color[0] as f32 * 0.8) as u8,
                        (base_color[1] as f32 * 0.8) as u8,
                        (base_color[2] as f32 * 0.8) as u8,
                        255,
                    ]),
                    base_color,
                    depth,
                );

                let blended = common::lerp_color(BG, lit, aa);
                img.put_pixel(px, py, blended);
            } else if dist < pie_r_inner && dist >= pie_r_inner - 1.0 {
                let aa = (pie_r_inner - dist).clamp(0.0, 1.0) as f32;
                let blended = common::lerp_color(BG, PIE_BG, aa * 0.3);
                img.put_pixel(px, py, blended);
            }
        }
    }

    // Center text: free percentage
    let free_pct = (free_frac * 100.0).round() as i32;
    let pct_text = format!("{free_pct}%");
    common::draw_text_centered(
        &mut img,
        TEXT_PRIMARY,
        pie_cx as i32,
        pie_cy as i32 - 16,
        30.0,
        font_bold,
        &pct_text,
    );
    common::draw_text_centered(
        &mut img,
        TEXT_MUTED,
        pie_cx as i32,
        pie_cy as i32 + 12,
        13.0,
        font,
        "free",
    );

    // Bottom area: legend with prominent GB values
    let legend_y = 192;
    let col1_x = mx + 10;
    let col2_x = 132;

    // Used
    common::draw_rounded_rect(&mut img, col1_x, legend_y + 4, 10, 10, 3, PIE_USED);
    draw_text_mut(
        &mut img,
        TEXT_MUTED,
        col1_x + 14,
        legend_y,
        PxScale::from(13.0),
        font,
        "Used",
    );
    draw_text_mut(
        &mut img,
        TEXT_PRIMARY,
        col1_x + 14,
        legend_y + 16,
        PxScale::from(22.0),
        font_bold,
        used_label,
    );

    // Free
    common::draw_rounded_rect(&mut img, col2_x, legend_y + 4, 10, 10, 3, PIE_FREE);
    draw_text_mut(
        &mut img,
        TEXT_MUTED,
        col2_x + 14,
        legend_y,
        PxScale::from(13.0),
        font,
        "Free",
    );
    draw_text_mut(
        &mut img,
        TEXT_PRIMARY,
        col2_x + 14,
        legend_y + 16,
        PxScale::from(22.0),
        font_bold,
        free_label,
    );

    Ok(img)
}
