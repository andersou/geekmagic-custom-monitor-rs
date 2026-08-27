//! Reusable usage-bars renderer. Takes plain data structs, never plugin types —
//! any plugin (e.g. a future Codex plugin) can build `UsageWindowData` and call
//! `render_usage_bars`, whether or not the claude plugin is enabled in TOML.

use ab_glyph::{FontArc, PxScale};
use anyhow::Result;
use image::{Rgba, RgbaImage};
use imageproc::drawing::draw_text_mut;

use crate::render::common::{
    self, BG, H, PANEL_BG, SEPARATOR, TEXT_DIM, TEXT_MUTED, TEXT_PRIMARY, W,
};

const BAR_TRACK: Rgba<u8> = Rgba([40, 40, 50, 255]);
const BAR_FILL_LEFT: Rgba<u8> = Rgba([59, 130, 246, 255]);
const BAR_FILL_RIGHT: Rgba<u8> = Rgba([6, 182, 212, 255]);
const PACE_OK: Rgba<u8> = Rgba([34, 197, 94, 255]);
const PACE_WARN: Rgba<u8> = Rgba([249, 115, 22, 255]);
const WARN_FILL_LEFT: Rgba<u8> = Rgba([234, 179, 8, 255]);
const WARN_FILL_RIGHT: Rgba<u8> = Rgba([249, 115, 22, 255]);
const DANGER_FILL: Rgba<u8> = Rgba([239, 68, 68, 255]);

pub struct UsageWindowData {
    pub label: String, // "Session" / "Weekly" (was hardcoded in the base)
    pub utilization: f64,
    pub usage_level: String, // drives bar_colors
    pub expected_percent: Option<f64>,
    pub delta_percent: Option<f64>,
    pub resets_in_minutes: Option<f64>,
    pub will_last_to_reset: Option<bool>,
    pub eta_minutes: Option<f64>,
}

fn bar_colors(usage_level: &str) -> (Rgba<u8>, Rgba<u8>) {
    match usage_level {
        "danger" | "over" => (DANGER_FILL, DANGER_FILL),
        "warn" => (WARN_FILL_LEFT, WARN_FILL_RIGHT),
        _ => (BAR_FILL_LEFT, BAR_FILL_RIGHT),
    }
}

fn draw_gradient_bar(
    img: &mut RgbaImage,
    x: i32,
    y: i32,
    total_w: u32,
    h: u32,
    fill_frac: f32,
    left_color: Rgba<u8>,
    right_color: Rgba<u8>,
    corner_r: u32,
) {
    common::draw_rounded_rect(img, x, y, total_w, h, corner_r, BAR_TRACK);
    let fill_w = ((total_w as f32) * fill_frac.clamp(0.0, 1.0)) as u32;
    if fill_w == 0 {
        return;
    }
    for px in 0..fill_w {
        let t = if total_w > 1 {
            px as f32 / (total_w - 1) as f32
        } else {
            0.0
        };
        let color = common::lerp_color(left_color, right_color, t);
        let abs_x = x as u32 + px;
        for py in 0..h {
            let abs_y = y as u32 + py;
            if common::is_inside_rounded(px, py, fill_w, h, corner_r) && abs_x < W && abs_y < H {
                img.put_pixel(abs_x, abs_y, color);
            }
        }
    }
}

fn blend_over(base: Rgba<u8>, over: Rgba<u8>) -> Rgba<u8> {
    let a = over[3] as f32 / 255.0;
    Rgba([
        (base[0] as f32 * (1.0 - a) + over[0] as f32 * a) as u8,
        (base[1] as f32 * (1.0 - a) + over[1] as f32 * a) as u8,
        (base[2] as f32 * (1.0 - a) + over[2] as f32 * a) as u8,
        255,
    ])
}

fn draw_pace_marker(
    img: &mut RgbaImage,
    bar_x: i32,
    bar_y: i32,
    bar_w: u32,
    bar_h: u32,
    expected_pct: f64,
    ok: bool,
) {
    let marker_x = bar_x + (bar_w as f64 * expected_pct.clamp(0.0, 100.0) / 100.0) as i32;
    let color = if ok { PACE_OK } else { PACE_WARN };
    let glow = if ok {
        Rgba([34, 197, 94, 80])
    } else {
        Rgba([249, 115, 22, 80])
    };

    for dx in 0..2i32 {
        for dy in -3..(bar_h as i32 + 3) {
            let px = marker_x + dx;
            let py = bar_y + dy;
            if px >= 0 && px < W as i32 && py >= 0 && py < H as i32 {
                img.put_pixel(px as u32, py as u32, color);
            }
        }
    }
    for dx in [-1i32, 2] {
        for dy in -2..(bar_h as i32 + 2) {
            let px = marker_x + dx;
            let py = bar_y + dy;
            if px >= 0 && px < W as i32 && py >= 0 && py < H as i32 {
                let existing = *img.get_pixel(px as u32, py as u32);
                img.put_pixel(px as u32, py as u32, blend_over(existing, glow));
            }
        }
    }
}

fn draw_circle(img: &mut RgbaImage, cx: i32, cy: i32, r: i32, color: Rgba<u8>) {
    for dx in -r..=r {
        for dy in -r..=r {
            if dx * dx + dy * dy <= r * r {
                let px = cx + dx;
                let py = cy + dy;
                if px >= 0 && px < W as i32 && py >= 0 && py < H as i32 {
                    img.put_pixel(px as u32, py as u32, color);
                }
            }
        }
    }
}

fn format_duration(minutes: f64) -> String {
    let total = minutes.max(0.0).round() as u64;
    let days = total / 1440;
    let hours = (total % 1440) / 60;
    let mins = total % 60;
    if days > 0 {
        if hours == 0 {
            return format!("{days}d");
        }
        return format!("{days}d {hours}h");
    }
    if hours == 0 {
        return format!("{mins}m");
    }
    if mins == 0 {
        return format!("{hours}h");
    }
    format!("{hours}h {mins}m")
}

fn format_updated_time(iso: &str) -> String {
    use chrono::{DateTime, Local};
    if let Ok(utc) = DateTime::parse_from_rfc3339(iso) {
        let local: DateTime<Local> = utc.with_timezone(&Local);
        return local.format("%H:%M").to_string();
    }
    // Fallback: try extracting time part
    if let Some(t_pos) = iso.find('T') {
        let time_part = &iso[t_pos + 1..];
        if time_part.len() >= 5 {
            return time_part[..5].to_string();
        }
    }
    "??:??".to_string()
}

pub fn render_usage_bars(
    windows: &[UsageWindowData],
    updated_at: Option<&str>,
) -> Result<RgbaImage> {
    let font: &FontArc = common::font_regular();
    let font_bold: &FontArc = common::font_bold();
    let mut img = RgbaImage::from_pixel(W, H, BG);

    if windows.is_empty() {
        draw_text_mut(
            &mut img,
            TEXT_DIM,
            60,
            110,
            PxScale::from(16.0),
            font,
            "No usage data",
        );
        return Ok(img);
    }

    let mx = 16i32;
    let right_edge = (W as i32) - mx;
    let content_w = (right_edge - mx) as u32;

    // ── Header: "Claude Code" + updated time ──
    let header_y = 10;
    draw_text_mut(
        &mut img,
        TEXT_PRIMARY,
        mx,
        header_y,
        PxScale::from(17.0),
        font_bold,
        "Claude Code",
    );

    // Updated timestamp (right-aligned, bigger)
    let updated_text = updated_at.map(format_updated_time).unwrap_or_else(|| "—".to_string());
    common::draw_text_right(
        &mut img,
        TEXT_DIM,
        right_edge,
        header_y + 1,
        15.0,
        font,
        &updated_text,
    );

    // Separator
    common::draw_rounded_rect(&mut img, mx, 33, content_w, 1, 0, SEPARATOR);

    // ── Bar sections ──
    // Sections are not uniform: the pace row exists only when the window has
    // pace data. A fixed section height left dead space in short sections and
    // pushed the last panel against the bottom edge (panels touching, rounded
    // corners notching into each other), so measure each section and spend the
    // leftover space on panel padding.
    const BAR_OFFSET: i32 = 40; // content top -> bar top (label + big percentage)
    const BAR_H: u32 = 14;
    const ROW3_OFFSET: i32 = 6; // bar bottom -> "x% left"
    const ROW3_H: i32 = 17;
    const PACE_OFFSET: i32 = 18; // "x% left" top -> pace row top
    const PACE_H: i32 = 14;
    const GAP_RANGE: (i32, i32) = (2, 10); // between panels
    const PAD_RANGE: (i32, i32) = (4, 20); // per panel, top + bottom combined
    const TOP_Y: i32 = 37; // below the header separator
    const BOTTOM_MARGIN: i32 = 6;

    let content_h: Vec<i32> = windows
        .iter()
        .map(|w| {
            let to_row3 = BAR_OFFSET + BAR_H as i32 + ROW3_OFFSET;
            if w.delta_percent.is_some() {
                to_row3 + PACE_OFFSET + PACE_H
            } else {
                to_row3 + ROW3_H
            }
        })
        .collect();

    let n = windows.len() as i32;
    let avail = H as i32 - TOP_Y - BOTTOM_MARGIN;
    // Free space goes first to a visible gap between panels (touching panels
    // notch each other's rounded corners), then to panel padding.
    let free = avail - content_h.iter().sum::<i32>();
    let gap = (free / (2 * n)).clamp(GAP_RANGE.0, GAP_RANGE.1);
    let pad = ((free - gap * (n - 1)) / n).clamp(PAD_RANGE.0, PAD_RANGE.1);

    let mut panel_y = TOP_Y;
    for (i, w) in windows.iter().enumerate() {
        let panel_h = content_h[i] + pad;
        let by = panel_y + pad / 2;
        let bar_x = mx + 8;
        let bar_w = content_w - 16;
        let inner_right = right_edge - 6;

        // Panel background
        common::draw_rounded_rect(
            &mut img,
            mx - 4,
            panel_y,
            content_w + 8,
            panel_h as u32,
            10,
            PANEL_BG,
        );

        // Row 1: Label left, big percentage right
        draw_text_mut(
            &mut img,
            TEXT_MUTED,
            bar_x,
            by + 12,
            PxScale::from(14.0),
            font_bold,
            &w.label,
        );

        let pct_val = w.utilization.round() as i32;
        let pct_text = format!("{pct_val}%");
        common::draw_text_right(
            &mut img,
            TEXT_PRIMARY,
            inner_right,
            by,
            36.0,
            font_bold,
            &pct_text,
        );

        // Row 2: Progress bar
        let bar_y = by + BAR_OFFSET;
        let bar_h = BAR_H;
        let fill_frac = (w.utilization / 100.0) as f32;
        let (fill_l, fill_r) = bar_colors(&w.usage_level);
        draw_gradient_bar(&mut img, bar_x, bar_y, bar_w, bar_h, fill_frac, fill_l, fill_r, 7);

        // Pace marker on bar
        if let (Some(expected), Some(will_last)) = (w.expected_percent, w.will_last_to_reset) {
            draw_pace_marker(&mut img, bar_x, bar_y, bar_w, bar_h, expected, will_last);
        }

        // Row 3: "X% left" bigger + "Resets in ..."
        let row3_y = bar_y + bar_h as i32 + ROW3_OFFSET;
        let remaining = (100.0 - w.utilization).max(0.0);
        let left_text = format!("{}% left", remaining.round() as i32);
        draw_text_mut(
            &mut img,
            TEXT_PRIMARY,
            bar_x,
            row3_y,
            PxScale::from(15.0),
            font_bold,
            &left_text,
        );

        if let Some(mins) = w.resets_in_minutes {
            let reset_text = format!("resets {}", format_duration(mins));
            common::draw_text_right(
                &mut img,
                TEXT_DIM,
                inner_right,
                row3_y + 1,
                15.0,
                font,
                &reset_text,
            );
        }

        // Row 4: Pace info
        if let Some(delta) = w.delta_percent {
            let pace_y = row3_y + PACE_OFFSET;
            let abs_delta = delta.abs().round() as i32;
            let (pace_text, pace_color) = if abs_delta <= 2 {
                ("On pace".to_string(), PACE_OK)
            } else if delta < 0.0 {
                (format!("{abs_delta}% reserve"), PACE_OK)
            } else {
                (format!("{abs_delta}% deficit"), PACE_WARN)
            };

            // Colored dot + text (bigger green/orange text)
            draw_circle(&mut img, bar_x + 4, pace_y + 6, 3, pace_color);
            draw_text_mut(
                &mut img,
                pace_color,
                bar_x + 12,
                pace_y,
                PxScale::from(13.0),
                font,
                &pace_text,
            );

            // Right side: ETA
            let right_text = if w.will_last_to_reset == Some(true) {
                "Lasts to reset".to_string()
            } else if let Some(eta) = w.eta_minutes {
                format!("Out in {}", format_duration(eta))
            } else {
                String::new()
            };
            if !right_text.is_empty() {
                common::draw_text_right(
                    &mut img,
                    pace_color,
                    inner_right,
                    pace_y,
                    12.0,
                    font,
                    &right_text,
                );
            }
        }

        panel_y += panel_h + gap;
    }

    Ok(img)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn window(label: &str, utilization: f64, pace: bool) -> UsageWindowData {
        UsageWindowData {
            label: label.to_string(),
            utilization,
            usage_level: "moderate".to_string(),
            expected_percent: pace.then_some(40.0),
            delta_percent: pace.then_some(-23.0),
            resets_in_minutes: Some(296.0),
            will_last_to_reset: pace.then_some(true),
            eta_minutes: None,
        }
    }

    /// Rows of `img` that contain no panel pixel, i.e. background-only bands.
    fn empty_rows(img: &RgbaImage) -> Vec<u32> {
        (0..H)
            .filter(|&y| (0..W).all(|x| *img.get_pixel(x, y) == BG))
            .collect()
    }

    fn last_drawn_row(img: &RgbaImage) -> u32 {
        (0..H)
            .filter(|&y| (0..W).any(|x| *img.get_pixel(x, y) != BG))
            .next_back()
            .expect("something was drawn")
    }

    #[test]
    fn panels_fit_and_stay_separated_with_mixed_row_counts() {
        // Session without pace row, Weekly with it: the case that used to leave
        // dead space up top and shove the last row against the bottom edge.
        let img = render_usage_bars(
            &[window("Session", 1.0, false), window("Weekly", 0.0, true)],
            Some("2026-08-27T18:43:00Z"),
        )
        .unwrap();

        assert!(last_drawn_row(&img) < H - 2, "content touches the bottom edge");
        let gap = empty_rows(&img);
        assert!(
            gap.iter().any(|&y| (100..160).contains(&y)),
            "panels are fused: no background band between them ({gap:?})"
        );
    }

    #[test]
    fn panels_fit_when_both_windows_have_a_pace_row() {
        // Tallest layout: 4 rows per section. Must still fit inside 240px.
        let img = render_usage_bars(
            &[window("Session", 62.0, true), window("Weekly", 41.0, true)],
            Some("2026-08-27T18:43:00Z"),
        )
        .unwrap();

        assert!(last_drawn_row(&img) < H - 2, "content touches the bottom edge");
        assert!(
            empty_rows(&img).iter().any(|&y| (100..160).contains(&y)),
            "panels are fused when both sections are tall"
        );
    }
}
