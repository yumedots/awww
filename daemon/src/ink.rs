use common::ipc::PixelFormat;

pub fn is_black(y: f64) -> bool {
    (y + 0.05) / 0.05 >= 1.05 / (y + 0.05)
}

fn lin(c: u8) -> f64 {
    let c = c as f64 / 255.0;
    if c <= 0.04045 {
        c / 12.92
    } else {
        ((c + 0.055) / 1.055).powf(2.4)
    }
}

fn luma(r: u8, g: u8, b: u8) -> f64 {
    0.2126 * lin(r) + 0.7152 * lin(g) + 0.0722 * lin(b)
}

pub fn ink_code(y: f64) -> u8 {
    if is_black(y) { 1 } else { 2 }
}

pub fn unchanged(a: [u8; 3], b: [u8; 3]) -> bool {
    a.iter()
        .zip(b)
        .all(|(a, b)| a.abs_diff(b) <= 8)
}

/// Mean color and mean relative luminance of the top `rows` rows of a drawn canvas.
pub fn sample(
    canvas: &[u8],
    width: usize,
    format: PixelFormat,
    rows: usize,
) -> Option<([u8; 3], f64)> {
    let channels = format.channels() as usize;
    if width == 0 || rows == 0 {
        return None;
    }
    let count = rows * width;
    if canvas.len() < count * channels {
        return None;
    }

    // wayland memory layout: rgb888/abgr8888 start with red, bgr888/argb8888 start with blue
    let swap = matches!(format, PixelFormat::Rgb | PixelFormat::Abgr);
    let mut sum = [0f64; 3];
    let mut y_sum = 0f64;
    for px in canvas[..count * channels].chunks_exact(channels) {
        let (r, g, b) = if swap {
            (px[0], px[1], px[2])
        } else {
            (px[2], px[1], px[0])
        };
        sum[0] += r as f64;
        sum[1] += g as f64;
        sum[2] += b as f64;
        y_sum += luma(r, g, b);
    }

    let n = count as f64;
    let mean = [
        (sum[0] / n).round() as u8,
        (sum[1] / n).round() as u8,
        (sum[2] / n).round() as u8,
    ];
    Some((mean, y_sum / n))
}

/// Keeps the band's hue but pushes its lightness away from the band's own luminance, so the
/// result always contrasts with whatever color the band happens to be.
pub fn tint(mean: [u8; 3], y: f64) -> [u8; 4] {
    let mut c = [mean[0] as f64, mean[1] as f64, mean[2] as f64];
    // is_black means black text wins over this band, so the tint must go the other way
    let darken = is_black(y);
    for _ in 0..6 {
        let cur = luma(c[0].round() as u8, c[1].round() as u8, c[2].round() as u8);
        if if darken { cur <= 0.12 } else { cur >= 0.55 } {
            break;
        }
        for ch in &mut c {
            *ch = if darken {
                *ch * 0.6
            } else {
                *ch + (255.0 - *ch) * 0.4
            };
        }
    }
    [
        c[0].round() as u8,
        c[1].round() as u8,
        c[2].round() as u8,
        255,
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn samples_the_top_rows_only() {
        // 4x2, Bgr (bytes are b, g, r)
        let canvas = [
            0u8, 0, 255, 0, 0, 255, 0, 0, 255, 0, 0, 255, // top row: pure red
            255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, // bottom row: white
        ];
        let (mean, y) = sample(&canvas, 4, PixelFormat::Bgr, 1).unwrap();
        assert_eq!(mean, [255, 0, 0], "the mean is the top row's red");
        assert!(is_black(y), "pure red reads as a black-ish band");
        assert_eq!(sample(&canvas, 4, PixelFormat::Bgr, 0), None, "no rows, no sample");
        assert_eq!(sample(&canvas[..12], 4, PixelFormat::Bgr, 2), None, "short canvas, no sample");
    }

    #[test]
    fn wayland_canvas_byte_order() {
        // argb8888 canvases store pixels as b, g, r, a
        let canvas = [
            0u8, 0, 255, 255, 0, 0, 255, 255, 0, 0, 255, 255, 0, 0, 255, 255, // red pixels
            255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255,
        ];
        assert_eq!(
            sample(&canvas, 4, PixelFormat::Argb, 1).unwrap().0,
            [255, 0, 0],
            "argb8888 memory starts with blue"
        );
        assert_eq!(
            sample(&canvas, 4, PixelFormat::Abgr, 1).unwrap().0,
            [0, 0, 255],
            "abgr8888 memory starts with red"
        );
    }

    #[test]
    fn swapped_formats_read_the_same_color() {
        // same picture as above but saved as Rgb (bytes are r, g, b)
        let canvas = [
            255u8, 0, 0, 255, 0, 0, 255, 0, 0, 255, 0, 0, //
            255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255,
        ];
        let (mean, y) = sample(&canvas, 4, PixelFormat::Rgb, 1).unwrap();
        assert_eq!(mean, [255, 0, 0]);
        assert!(is_black(y));
    }

    #[test]
    fn tint_always_contrasts_the_band() {
        let dark = tint([40, 40, 60], 0.03);
        assert_eq!(dark[3], 255, "a picked color is opaque");
        assert!(
            luma(dark[0], dark[1], dark[2]) >= 0.5,
            "over a dark band the tint lightens"
        );

        let bright = tint([230, 220, 200], 0.8);
        assert!(
            luma(bright[0], bright[1], bright[2]) <= 0.2,
            "over a bright band the tint darkens"
        );

        let red = tint([255, 0, 0], luma(255, 0, 0));
        assert!(
            luma(red[0], red[1], red[2]) <= 0.2,
            "even saturated red gets a dark readable tint"
        );

        let blue = tint([0, 0, 255], luma(0, 0, 255));
        assert!(
            luma(blue[0], blue[1], blue[2]) >= 0.5,
            "deep blue reads as a dark band, so its tint lightens"
        );
    }

    #[test]
    fn ink_codes_both_sides() {
        assert_eq!(ink_code(0.0), 2, "a black band wants white ink");
        assert_eq!(ink_code(1.0), 1, "a white band wants black ink");
    }
}
