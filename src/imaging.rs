//! Sizing pictures for the vision encoder. The encoder takes an image as a grid of 16 px patches pooled 3 × 3 into
//! soft tokens, and the number of tokens is the budget (70 to 1120). The reference (transformers' Gemma image
//! processor) scales every image to fill the budget, keeping its aspect ratio, with each side a multiple of 48 px, and
//! resamples it bicubically (Pillow's). Doing the same here keeps the vectors equal to the reference's.

use anyhow::{Result, bail};
use fast_image_resize::{
    FilterType, PixelType, ResizeAlg, ResizeOptions, Resizer, images::Image as Buffer, images::ImageRef,
};

/// Packed 8-bit RGB.
#[derive(Clone)]
pub struct Image {
    pub width: u32,
    pub height: u32,
    pub rgb: Vec<u8>,
}

const PATCH: u32 = 16;
const POOLING: u32 = 3;
const SIDE: u32 = PATCH * POOLING;

/// The size (width, height) the reference gives an image of this size for `budget` tokens, or None for an image too
/// thin to hold even one row of tokens.
pub fn target_size(width: u32, height: u32, budget: u32) -> Option<(u32, u32)> {
    if width == 0 || height == 0 {
        return None;
    }
    let max_patches = (budget * POOLING * POOLING) as f64;
    let factor = (max_patches * (PATCH * PATCH) as f64 / (width as f64 * height as f64)).sqrt();
    let mut target_height = (factor * height as f64 / SIDE as f64).floor() as u32 * SIDE;
    let mut target_width = (factor * width as f64 / SIDE as f64).floor() as u32 * SIDE;
    if target_height == 0 && target_width == 0 {
        return None;
    }
    let max_side = budget * SIDE;
    if target_height == 0 {
        target_height = SIDE;
        target_width = ((width / height) * SIDE).min(max_side);
    } else if target_width == 0 {
        target_width = SIDE;
        target_height = ((height / width) * SIDE).min(max_side);
    }
    Some((target_width, target_height))
}

/// The image resized (bicubic, a = −0.5 as Pillow's) to fill `budget` tokens.
pub fn fit(image: &Image, budget: u32) -> Result<Image> {
    let Some((width, height)) = target_size(image.width, image.height, budget) else {
        bail!("image too thin to embed ({}×{})", image.width, image.height);
    };
    resize(image, width, height)
}

pub fn resize(image: &Image, width: u32, height: u32) -> Result<Image> {
    if (width, height) == (image.width, image.height) {
        return Ok(image.clone());
    }
    let source = ImageRef::new(image.width, image.height, &image.rgb, PixelType::U8x3)?;
    let mut target = Buffer::new(width, height, PixelType::U8x3);
    let options = ResizeOptions::new().resize_alg(ResizeAlg::Convolution(FilterType::CatmullRom));
    Resizer::new().resize(&source, &mut target, &options)?;
    Ok(Image { width, height, rgb: target.into_vec() })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes_fill_the_budget_in_multiples_of_48() {
        // A 1440 × 900 screenshot at 280 tokens: 280 × 9 patches of 16 px.
        let (w, h) = target_size(1440, 900, 280).unwrap();
        assert_eq!((w % 48, h % 48), (0, 0));
        assert!((w / 16) * (h / 16) <= 280 * 9);
        assert_eq!((w, h), (1008, 624));
        assert_eq!(target_size(1, 4000, 70), Some((48, 3360)));
    }
}
