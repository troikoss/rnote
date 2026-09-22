// Imports
use super::content::{GeneratedContentImages, gen_rasterized_images};
use super::resize::{ImageSizeOption, calculate_resize_ratio};
use super::{Content, Stroke};
use crate::Drawable;
use crate::document::Format;
use crate::engine::import::{PdfImportPageSpacing, PdfImportPrefs};
use crate::{EncodedImage, Image, ImageEncoding};
use anyhow::anyhow;
use hayro::{hayro_interpret, hayro_syntax, vello_cpu};
use kurbo::Shape;
use p2d::bounding_volume::Aabb;
use p2d::glamx::DAffine2;
use p2d::math::Vector2;
use rnote_compose::Transformable;
use rnote_compose::ext::{AabbExt, DAffine2Ext};
use rnote_compose::shapes::Rectangle;
use rnote_compose::shapes::Shapeable;
use serde::{Deserialize, Serialize};
use std::ops::Range;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, rename = "bitmapimage")]
pub struct BitmapImage {
    /// The bitmap image.
    ///
    /// The pixels are stored encoded and decoded on demand, so that a document holding many large
    /// bitmaps does not keep the pixels of all of them in memory.
    ///
    /// The bounds field of the image should not be used to determine the stroke bounds.
    /// Use rectangle.bounds() instead.
    #[serde(rename = "image")]
    pub image: EncodedImage,
    #[serde(rename = "rectangle")]
    pub rectangle: Rectangle,
}

impl Default for BitmapImage {
    fn default() -> Self {
        Self {
            image: EncodedImage::default(),
            rectangle: Rectangle::default(),
        }
    }
}

impl Content for BitmapImage {
    fn update_geometry(&mut self) {}

    /// Generate bitmap images for rendering in the app.
    ///
    /// The stored pixels are the finest detail a bitmap image has, so magnifying it never needs a
    /// new raster: the decoded pixels are handed to the renderer, which scales them when drawing.
    /// Rasterizing them at the image scale instead keeps a second copy of pixels that are already
    /// in memory, growing with the square of the zoom (16 MB for a single page at 3x zoom).
    ///
    /// The decoded pixels cover the entire stroke, hence [`GeneratedContentImages::Full`]. They are
    /// kept for as long as the stroke is in the viewport, and dropped with the render component when
    /// it leaves it.
    ///
    /// A minified image is the exception: rasterizing it once yields a raster *smaller* than the
    /// pixels themselves, so it is rasterized like other content instead.
    fn gen_images(
        &self,
        viewport: Aabb,
        image_scale: f64,
    ) -> Result<GeneratedContentImages, anyhow::Error> {
        if self.image.is_empty() {
            return Ok(GeneratedContentImages::Full(vec![]));
        }

        if !self.is_drawn_smaller_than_its_pixels(image_scale) {
            return Ok(GeneratedContentImages::Full(vec![self.decoded_image()?]));
        }

        gen_rasterized_images(self, viewport, image_scale)
    }
}

impl Drawable for BitmapImage {
    fn draw(&self, cx: &mut impl piet::RenderContext, _image_scale: f64) -> anyhow::Result<()> {
        // Drawing the decoded pixels through `Image` keeps this identical to how the cached image
        // is drawn when the stroke is rendered in the viewport.
        self.decoded_image()?.draw(cx, 1.0)
    }
}

impl Shapeable for BitmapImage {
    fn bounds(&self) -> Aabb {
        self.rectangle.bounds()
    }

    fn hitboxes(&self) -> Vec<Aabb> {
        vec![self.bounds()]
    }

    fn outline_path(&self) -> kurbo::BezPath {
        self.bounds().to_kurbo_rect().to_path(0.25)
    }
}

impl Transformable for BitmapImage {
    fn translate(&mut self, offset: Vector2) {
        self.rectangle.translate(offset);
    }

    fn rotate(&mut self, angle: f64, center: Vector2) {
        self.rectangle.rotate(angle, center);
    }

    fn scale(&mut self, scale: Vector2) {
        self.rectangle.scale(scale);
    }
}

impl BitmapImage {
    pub fn from_image_bytes(
        bytes: &[u8],
        pos: Vector2,
        size_option: ImageSizeOption,
    ) -> Result<Self, anyhow::Error> {
        Self::from_image(Image::try_from_encoded_bytes(bytes)?, pos, size_option)
    }

    /// Create a [BitmapImage] from an already decoded [Image].
    ///
    /// The pixels are encoded before they are stored, so that the decoded pixels are not held in
    /// memory for the lifetime of the document.
    pub fn from_image(
        image: Image,
        pos: Vector2,
        size_option: ImageSizeOption,
    ) -> Result<Self, anyhow::Error> {
        Self::from_encoded_image(
            EncodedImage::from_image(image, ImageEncoding::Zstd)?,
            pos,
            size_option,
        )
    }

    /// Create a [BitmapImage] from an [EncodedImage], e.g. one whose pixels are still encoded as
    /// they were read from a file.
    pub fn from_encoded_image(
        image: EncodedImage,
        pos: Vector2,
        size_option: ImageSizeOption,
    ) -> Result<Self, anyhow::Error> {
        let initial_size = Vector2::new(image.pixel_width as f64, image.pixel_height as f64);
        let (size, resize_ratio) = match size_option {
            ImageSizeOption::RespectOriginalSize => (initial_size, 1.0f64),
            ImageSizeOption::ImposeSize(given_size) => (given_size, 1.0f64),
            ImageSizeOption::ResizeImage(resize_struct) => (
                initial_size,
                calculate_resize_ratio(resize_struct, initial_size, pos),
            ),
        };
        let mut transform = DAffine2::IDENTITY;
        transform.append_scale_mut(Vector2::splat(resize_ratio));
        transform.append_translation_mut(pos + size * resize_ratio * 0.5);
        let rectangle = Rectangle {
            cuboid: p2d::shape::Cuboid::new(size * 0.5),
            affine: transform,
        };

        Ok(Self { image, rectangle })
    }

    /// Decodes the stored pixels, placed at the rectangle of this stroke.
    ///
    /// The rectangle of the image itself is only the extent of its pixels and does not determine
    /// where the stroke is drawn, so it is replaced by the rectangle of the stroke.
    pub fn decoded_image(&self) -> Result<Image, anyhow::Error> {
        let mut image = self.image.decode()?;
        image.rectangle = self.rectangle;

        Ok(image)
    }

    /// Whether the image is drawn smaller than its pixels at the given image scale.
    ///
    /// `image_scale` is the number of device pixels per document unit, so the width of the stroke
    /// on screen is its width in document units scaled by it.
    fn is_drawn_smaller_than_its_pixels(&self, image_scale: f64) -> bool {
        let width_on_screen = self.rectangle.bounds().extents()[0] * image_scale;

        self.image.pixel_width as f64 > width_on_screen
    }

    /// Compresses the stored pixels if they are not compressed yet.
    pub fn compact(&mut self) -> Result<(), anyhow::Error> {
        self.image.compact()
    }

    /// Generate bitmap image strokes from the pages of a Pdf.
    ///
    /// Takes ownership of the Pdf bytes: hayro parses Pdf objects lazily and keeps the file bytes
    /// alive for as long as the [hayro_syntax::Pdf] exists, so the buffer is handed over instead of
    /// being copied. Copying it kept a second, equally large copy of the whole file resident for the
    /// entire import.
    pub fn from_pdf_bytes(
        to_be_read: Vec<u8>,
        pdf_import_prefs: PdfImportPrefs,
        insert_pos: Vector2,
        page_range: Option<Range<usize>>,
        format: &Format,
        password: Option<String>,
    ) -> Result<Vec<Self>, anyhow::Error> {
        let pdf = if let Some(password) = password {
            hayro_syntax::Pdf::new_with_password(to_be_read, &password)
                .map_err(|err| anyhow!("Creating Pdf instance failed, Err: {err:?}"))?
        } else {
            hayro_syntax::Pdf::new(to_be_read)
                .map_err(|err| anyhow!("Creating Pdf instance failed, Err: {err:?}"))?
        };
        let interpreter_settings = hayro_interpret::InterpreterSettings::default();
        // hayro 0.7 takes a render cache, which upstream intends to be created once per PDF and
        // reused across the render invocations of that document.
        let render_cache = hayro::RenderCache::new();
        let pages = pdf.pages();
        let page_range = page_range.unwrap_or(0..pages.len());
        let page_width = if pdf_import_prefs.adjust_document {
            format.width()
        } else {
            format.width() * (pdf_import_prefs.page_width_perc / 100.0)
        };

        // calculate the page zoom based on the width of the first page.
        let page_zoom = if let Some(first_page) = pages.first() {
            page_width / first_page.render_dimensions().0 as f64
        } else {
            return Ok(vec![]);
        };
        let x = insert_pos[0];
        let mut y = insert_pos[1];

        // TODO: investigate if this can be parallelized with rayon's `par_iter()`
        let images = page_range
            .map(|page_i| {
                let page = pages
                    .get(page_i)
                    .ok_or_else(|| anyhow::anyhow!("no page at index '{page_i}"))?;
                let (intrinsic_width, intrinsic_height) = {
                    let dimensions = page.render_dimensions();
                    (dimensions.0 as f64, dimensions.1 as f64)
                };
                let width = intrinsic_width * page_zoom;
                let height = intrinsic_height * page_zoom;
                let render_settings = hayro::RenderSettings {
                    x_scale: (pdf_import_prefs.bitmap_scalefactor * page_zoom) as f32,
                    y_scale: (pdf_import_prefs.bitmap_scalefactor * page_zoom) as f32,
                    width: Some((pdf_import_prefs.bitmap_scalefactor * width).ceil() as u16),
                    height: Some((pdf_import_prefs.bitmap_scalefactor * height).ceil() as u16),
                    bg_color: vello_cpu::color::AlphaColor::WHITE,
                };

                // TODO: implement drawing page borders.
                // Possibly with vello-cpu, since it already is a dependency of hayro
                let pixmap =
                    hayro::render(page, &render_cache, &interpreter_settings, &render_settings);

                // vello-cpu renders to premultiplied RGBA8, which is exactly the format rnote
                // stores images in memory. The pixels are compressed right away instead of being
                // kept decoded: a document holds one of these per page (7.1 MB at 1123x1589 for a
                // rendered A4 page), and a compressed page is an order of magnitude smaller, so a
                // 204 page document keeps ~0.17 GB instead of 1.4 GB resident.
                let image = EncodedImage::from_premultiplied_rgba8(
                    pixmap.data_as_u8_slice(),
                    pixmap.width() as u32,
                    pixmap.height() as u32,
                    ImageEncoding::Zstd,
                )?;

                let image_pos = Vector2::new(x, y);
                let image_size = Vector2::new(width, height);

                if pdf_import_prefs.adjust_document {
                    y += height
                } else {
                    y += match pdf_import_prefs.page_spacing {
                        PdfImportPageSpacing::Continuous => {
                            height + Stroke::IMPORT_OFFSET_DEFAULT[1] * 0.5
                        }
                        PdfImportPageSpacing::OnePerDocumentPage => format.height(),
                    };
                }

                Self::from_encoded_image(image, image_pos, ImageSizeOption::ImposeSize(image_size))
            })
            .collect::<anyhow::Result<Vec<Self>>>()?;

        Ok(images)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Image;

    /// A bitmap whose pixels are larger than the rectangle it is drawn into, like a rendered Pdf
    /// page, with a pattern so that resampling differences are visible.
    fn test_bitmap_image() -> BitmapImage {
        let (pixel_width, pixel_height) = (64u32, 96u32);
        let data = (0..pixel_width * pixel_height)
            .flat_map(|i| {
                let x = i % pixel_width;
                let y = i / pixel_width;
                [
                    (x * 4 % 256) as u8,
                    (y * 3 % 256) as u8,
                    ((x + y) * 2 % 256) as u8,
                    255,
                ]
            })
            .collect::<Vec<u8>>();
        let image = EncodedImage::from_premultiplied_rgba8(
            &data,
            pixel_width,
            pixel_height,
            ImageEncoding::Zstd,
        )
        .expect("encoding the test image failed");

        BitmapImage::from_encoded_image(
            image,
            Vector2::ZERO,
            // Drawn into a rectangle half the size of the pixels.
            ImageSizeOption::ImposeSize(Vector2::new(32.0, 48.0)),
        )
        .expect("creating the test bitmap image failed")
    }

    /// The mean absolute difference of the pixel channels of two images of the same size.
    fn mean_absolute_difference(a: &Image, b: &Image) -> f64 {
        let (a, b) = (&a.data[..], &b.data[..]);

        a.iter()
            .zip(b)
            .map(|(a, b)| (f64::from(*a) - f64::from(*b)).abs())
            .sum::<f64>()
            / a.len() as f64
    }

    #[test]
    fn a_bitmap_image_is_only_rasterized_when_it_is_minified() {
        // 64x96 px of pixels drawn into a 32x48 doc unit rectangle.
        let bitmapimage = test_bitmap_image();
        let bounds = bitmapimage.bounds();

        // At an image scale of 2 the image is not minified, so the cached image holds the stored
        // pixels. Rasterizing them instead would keep a second copy of pixels that are already in
        // memory, and would grow with the square of the zoom.
        let GeneratedContentImages::Full(images) = bitmapimage
            .gen_images(bounds, 2.0)
            .expect("generating the stroke images failed")
        else {
            panic!("a bitmap image covers its entire stroke");
        };
        assert_eq!(images.len(), 1);
        assert_eq!(images[0].pixel_width, bitmapimage.image.pixel_width);
        assert_eq!(images[0].pixel_height, bitmapimage.image.pixel_height);

        // At an image scale of 0.5 it is minified, so it is rasterized like other content, which
        // yields a raster smaller than the pixels themselves.
        let GeneratedContentImages::Full(images) = bitmapimage
            .gen_images(bounds, 0.5)
            .expect("generating the stroke images failed")
        else {
            panic!("a bitmap image covers its entire stroke");
        };
        assert_eq!(images.len(), 1);
        assert!(images[0].pixel_width < bitmapimage.image.pixel_width);
        assert!(images[0].pixel_height < bitmapimage.image.pixel_height);
    }

    #[test]
    fn the_cached_image_renders_like_the_stroke() {
        // The render cache replaces the stroke itself while the stroke is in the viewport, so what
        // `gen_images()` returns has to draw the same way as the stroke does.
        for image_scale in [0.5, 1.0, 2.0] {
            let bitmapimage = test_bitmap_image();
            let bounds = bitmapimage.bounds();

            let directly =
                Image::gen_with_piet(|cx| bitmapimage.draw(cx, image_scale), bounds, image_scale)
                    .expect("rendering the stroke failed");

            let GeneratedContentImages::Full(images) = bitmapimage
                .gen_images(bounds, image_scale)
                .expect("generating the stroke images failed")
            else {
                panic!("a bitmap image covers its entire stroke");
            };
            let cached =
                Image::gen_with_piet(|cx| images[0].draw(cx, image_scale), bounds, image_scale)
                    .expect("rendering the cached image failed");

            assert_eq!(directly.pixel_width, cached.pixel_width);
            assert_eq!(directly.pixel_height, cached.pixel_height);

            let difference = mean_absolute_difference(&directly, &cached);
            assert!(
                difference < 1.0,
                "the cached image renders differently from the stroke at image scale \
                 {image_scale} (mean absolute difference {difference})"
            );
        }
    }
}
