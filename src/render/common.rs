//! Primitives shared by every plugin renderer. No screen-level renderer here.

use std::sync::LazyLock;

use ab_glyph::{FontArc, PxScale};
use image::{Rgba, RgbaImage};
use imageproc::drawing::draw_text_mut;

pub const W: u32 = 240;
pub const H: u32 = 240;

pub const BG: Rgba<u8> = Rgba([12, 12, 16, 255]);
pub const PANEL_BG: Rgba<u8> = Rgba([22, 22, 30, 255]);
pub const TEXT_PRIMARY: Rgba<u8> = Rgba([240, 240, 245, 255]);
pub const TEXT_DIM: Rgba<u8> = Rgba([113, 113, 122, 255]);
pub const TEXT_MUTED: Rgba<u8> = Rgba([161, 161, 170, 255]);
pub const SEPARATOR: Rgba<u8> = Rgba([35, 35, 45, 255]);

const FONT_BYTES: &[u8] = include_bytes!("../../fonts/Inter-Regular.ttf");
const FONT_BOLD_BYTES: &[u8] = include_bytes!("../../fonts/Inter-Bold.ttf");

pub fn font_regular() -> &'static FontArc {
    static FONT: LazyLock<FontArc> =
        LazyLock::new(|| FontArc::try_from_slice(FONT_BYTES).expect("embedded Inter-Regular"));
    &FONT
}

pub fn font_bold() -> &'static FontArc {
    static FONT: LazyLock<FontArc> =
        LazyLock::new(|| FontArc::try_from_slice(FONT_BOLD_BYTES).expect("embedded Inter-Bold"));
    &FONT
}

pub fn lerp_color(a: Rgba<u8>, b: Rgba<u8>, t: f32) -> Rgba<u8> {
    let t = t.clamp(0.0, 1.0);
    Rgba([
        (a[0] as f32 + (b[0] as f32 - a[0] as f32) * t) as u8,
        (a[1] as f32 + (b[1] as f32 - a[1] as f32) * t) as u8,
        (a[2] as f32 + (b[2] as f32 - a[2] as f32) * t) as u8,
        255,
    ])
}

/// True when the pixel is inside a `w`x`h` rect with corner radius `r`.
/// Each pixel belongs to at most one corner: testing it against every corner
/// center kept only the intersection of the four circles, which flattened the
/// corners into a straight `r`-wide inset (cards rendered as stepped "T"s).
pub fn is_inside_rounded(px: u32, py: u32, w: u32, h: u32, r: u32) -> bool {
    if r == 0 || w == 0 || h == 0 {
        return true;
    }
    let r = r.min(w / 2).min(h / 2);
    if r == 0 {
        return true;
    }

    let cx = if px < r {
        r
    } else if px >= w - r {
        w - 1 - r
    } else {
        return true; // between the corner bands: only the edges matter
    };
    let cy = if py < r {
        r
    } else if py >= h - r {
        h - 1 - r
    } else {
        return true;
    };

    let dx = cx.abs_diff(px);
    let dy = cy.abs_diff(py);
    dx * dx + dy * dy <= r * r
}

pub fn draw_rounded_rect(
    img: &mut RgbaImage,
    x: i32,
    y: i32,
    w: u32,
    h: u32,
    r: u32,
    color: Rgba<u8>,
) {
    for px in 0..w {
        for py in 0..h {
            if is_inside_rounded(px, py, w, h, r) {
                let abs_x = x as u32 + px;
                let abs_y = y as u32 + py;
                if abs_x < W && abs_y < H {
                    img.put_pixel(abs_x, abs_y, color);
                }
            }
        }
    }
}

pub fn approx_text_width(text: &str, scale: f32) -> i32 {
    let char_w = scale * 0.55;
    let mut w = 0.0f32;
    for ch in text.chars() {
        w += match ch {
            '.' | ':' | '!' | '|' | 'i' | 'l' | '1' => char_w * 0.55,
            'm' | 'w' | 'M' | 'W' => char_w * 1.25,
            ' ' => char_w * 0.6,
            '%' => char_w * 1.1,
            _ => char_w,
        };
    }
    w.ceil() as i32
}

pub fn draw_text_right(
    img: &mut RgbaImage,
    color: Rgba<u8>,
    right_x: i32,
    y: i32,
    scale: f32,
    font: &FontArc,
    text: &str,
) {
    let w = approx_text_width(text, scale);
    draw_text_mut(img, color, right_x - w, y, PxScale::from(scale), font, text);
}

pub fn draw_text_centered(
    img: &mut RgbaImage,
    color: Rgba<u8>,
    center_x: i32,
    y: i32,
    scale: f32,
    font: &FontArc,
    text: &str,
) {
    let w = approx_text_width(text, scale);
    draw_text_mut(
        img,
        color,
        center_x - w / 2,
        y,
        PxScale::from(scale),
        font,
        text,
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Corners must be an arc: the inset shrinks row by row and vanishes at
    /// `y == r`. A constant inset means the corners collapsed into a step.
    #[test]
    fn rounded_corners_form_an_arc() {
        let (w, h, r) = (216u32, 60u32, 10u32);
        let inset = |y: u32| {
            (0..w)
                .position(|x| is_inside_rounded(x, y, w, h, r))
                .unwrap() as u32
        };

        assert_eq!(inset(0), r, "top row spans only between the corner centers");
        assert!(inset(1) < inset(0), "corner did not start curving");
        for y in 1..r {
            assert!(
                inset(y) <= inset(y - 1),
                "row {y} inset {} grew past {}",
                inset(y),
                inset(y - 1)
            );
        }
        assert_eq!(inset(r), 0, "rows past the radius must reach the edge");
        assert_eq!(inset(h - 1), r, "bottom row mirrors the top");

        // Symmetry: right side matches the left.
        for y in 0..r {
            let right = (0..w)
                .rev()
                .position(|x| is_inside_rounded(x, y, w, h, r))
                .unwrap() as u32;
            assert_eq!(right, inset(y), "asymmetric corner at row {y}");
        }
    }

    #[test]
    fn rounded_rect_keeps_its_interior_and_drops_corner_pixels() {
        let (w, h, r) = (40u32, 40u32, 10u32);
        assert!(!is_inside_rounded(0, 0, w, h, r));
        assert!(!is_inside_rounded(w - 1, h - 1, w, h, r));
        assert!(is_inside_rounded(w / 2, 0, w, h, r));
        assert!(is_inside_rounded(0, h / 2, w, h, r));
        assert!(is_inside_rounded(w / 2, h / 2, w, h, r));
    }
}
