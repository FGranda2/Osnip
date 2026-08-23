//! Pure pixel transforms used by the keyboard-shortcut actions on a
//! pin window. All functions are total, allocate a fresh buffer, and
//! never panic.
//!
//! Implementations defer to `image::imageops`, which is well-tested
//! and SIMD-aware where applicable. Wrapping them gives the daemon a
//! stable internal API independent of the `image` crate's exact
//! symbol locations.

use image::imageops;
use image::RgbaImage;

/// Rotate 90° clockwise. The output's width is the input's height and
/// vice versa.
#[must_use]
pub fn rotate_right(src: &RgbaImage) -> RgbaImage {
    imageops::rotate90(src)
}

/// Rotate 90° counter-clockwise.
#[must_use]
pub fn rotate_left(src: &RgbaImage) -> RgbaImage {
    imageops::rotate270(src)
}

/// Mirror horizontally — pixel `(x, y)` ends up at `(w-1-x, y)`.
#[must_use]
pub fn flip_horizontal(src: &RgbaImage) -> RgbaImage {
    imageops::flip_horizontal(src)
}

/// Mirror vertically — pixel `(x, y)` ends up at `(x, h-1-y)`.
#[must_use]
pub fn flip_vertical(src: &RgbaImage) -> RgbaImage {
    imageops::flip_vertical(src)
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::Rgba;

    fn fixture() -> RgbaImage {
        // Asymmetric in both axes so any swap or flip is detectable.
        let mut img = RgbaImage::from_pixel(3, 2, Rgba([0, 0, 0, 255]));
        img.put_pixel(0, 0, Rgba([1, 0, 0, 255]));
        img.put_pixel(2, 0, Rgba([2, 0, 0, 255]));
        img.put_pixel(0, 1, Rgba([3, 0, 0, 255]));
        img.put_pixel(2, 1, Rgba([4, 0, 0, 255]));
        img
    }

    #[test]
    fn rotate_right_swaps_dimensions() {
        let img = fixture();
        let r = rotate_right(&img);
        assert_eq!(r.width(), img.height());
        assert_eq!(r.height(), img.width());
    }

    #[test]
    fn four_rotates_right_are_identity() {
        let img = fixture();
        let r = rotate_right(&rotate_right(&rotate_right(&rotate_right(&img))));
        assert_eq!(r.dimensions(), img.dimensions());
        assert_eq!(r.as_raw(), img.as_raw());
    }

    #[test]
    fn rotate_left_then_right_is_identity() {
        let img = fixture();
        let r = rotate_right(&rotate_left(&img));
        assert_eq!(r.as_raw(), img.as_raw());
    }

    #[test]
    fn double_flip_horizontal_is_identity() {
        let img = fixture();
        let r = flip_horizontal(&flip_horizontal(&img));
        assert_eq!(r.as_raw(), img.as_raw());
    }

    #[test]
    fn double_flip_vertical_is_identity() {
        let img = fixture();
        let r = flip_vertical(&flip_vertical(&img));
        assert_eq!(r.as_raw(), img.as_raw());
    }

    #[test]
    fn flip_horizontal_moves_top_left_to_top_right() {
        let img = fixture();
        let r = flip_horizontal(&img);
        assert_eq!(r.get_pixel(0, 0), &Rgba([2, 0, 0, 255]));
        assert_eq!(r.get_pixel(2, 0), &Rgba([1, 0, 0, 255]));
    }

    #[test]
    fn flip_vertical_moves_top_left_to_bottom_left() {
        let img = fixture();
        let r = flip_vertical(&img);
        assert_eq!(r.get_pixel(0, 1), &Rgba([1, 0, 0, 255]));
        assert_eq!(r.get_pixel(0, 0), &Rgba([3, 0, 0, 255]));
    }
}
