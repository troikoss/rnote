// Imports
use crate::Drawable;
use anyhow::Context;
use core::fmt::Debug;
use image::ImageReader;
use p2d::bounding_volume::{Aabb, BoundingVolume};
use p2d::math::Vector2;
use piet::RenderContext;
use rnote_compose::Transformable;
use rnote_compose::ext::{AabbExt, DAffine2Ext};
use rnote_compose::shapes::{Rectangle, Shapeable};
use serde::{Deserialize, Serialize};
use std::io::{self, Cursor};

/// Px unit (96 DPI ) to Point unit ( 72 DPI ) conversion factor.
pub const PX_TO_POINT_CONV_FACTOR: f64 = 96.0 / 72.0;
/// Point unit ( 72 DPI ) to Px unit (96 DPI ) conversion factor.
pub const POINT_TO_PX_CONV_FACTOR: f64 = 72.0 / 96.0;
/// The factor for which the rendering for the current viewport is extended by.
/// For example:: 1.0 means the viewport is extended by its own extents on all sides.
///
/// Used when checking rendering for new zooms or a moved viewport.
/// There is a trade off: a larger value will consume more memory, a smaller value will mean more stuttering on zooms and when moving the view.
pub const VIEWPORT_EXTENTS_MARGIN_FACTOR: f64 = 0.4;

#[non_exhaustive]
#[derive(Debug, Copy, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ImageMemoryFormat {
    R8g8b8a8Premultiplied,
}

impl Default for ImageMemoryFormat {
    fn default() -> Self {
        Self::R8g8b8a8Premultiplied
    }
}

#[cfg(feature = "ui")]
impl TryFrom<gtk4::gdk::MemoryFormat> for ImageMemoryFormat {
    type Error = anyhow::Error;
    fn try_from(value: gtk4::gdk::MemoryFormat) -> Result<Self, Self::Error> {
        match value {
            gtk4::gdk::MemoryFormat::R8g8b8a8Premultiplied => Ok(Self::R8g8b8a8Premultiplied),
            _ => Err(anyhow::anyhow!(
                "ImageMemoryFormat try_from() gdk::MemoryFormat failed, unsupported MemoryFormat `{:?}`",
                value
            )),
        }
    }
}

#[cfg(feature = "ui")]
impl From<ImageMemoryFormat> for gtk4::gdk::MemoryFormat {
    fn from(value: ImageMemoryFormat) -> Self {
        match value {
            ImageMemoryFormat::R8g8b8a8Premultiplied => {
                gtk4::gdk::MemoryFormat::R8g8b8a8Premultiplied
            }
        }
    }
}

impl From<ImageMemoryFormat> for piet::ImageFormat {
    fn from(value: ImageMemoryFormat) -> Self {
        match value {
            ImageMemoryFormat::R8g8b8a8Premultiplied => piet::ImageFormat::RgbaPremul,
        }
    }
}

/// A bitmap image.
#[derive(Clone, Serialize, Deserialize)]
#[serde(default, rename = "image")]
pub struct Image {
    /// The image data.
    ///
    /// Is (de)serialized with base64 encoding.
    #[serde(rename = "data", with = "crate::utils::glib_bytes_base64")]
    pub data: glib::Bytes,
    /// The target rect in the coordinate space of the document.
    #[serde(rename = "rectangle")]
    pub rectangle: Rectangle,
    /// Width of the image data.
    #[serde(rename = "pixel_width")]
    pub pixel_width: u32,
    /// Height of the image data.
    #[serde(rename = "pixel_height")]
    pub pixel_height: u32,
    /// Memory format.
    #[serde(rename = "memory_format")]
    pub memory_format: ImageMemoryFormat,
}

impl Debug for Image {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Image")
            .field("data", &String::from("{.. no debug impl ..}"))
            .field("rect", &self.rectangle)
            .field("pixel_width", &self.pixel_width)
            .field("pixel_height", &self.pixel_height)
            .field("memory_format", &self.memory_format)
            .finish()
    }
}

impl Default for Image {
    fn default() -> Self {
        Self {
            data: glib::Bytes::from_owned(Vec::new()),
            rectangle: Rectangle::default(),
            pixel_width: 0,
            pixel_height: 0,
            memory_format: ImageMemoryFormat::default(),
        }
    }
}

impl From<image::DynamicImage> for Image {
    fn from(dynamic_image: image::DynamicImage) -> Self {
        let pixel_width = dynamic_image.width();
        let pixel_height = dynamic_image.height();
        let memory_format = ImageMemoryFormat::R8g8b8a8Premultiplied;
        let data = glib::Bytes::from_owned(dynamic_image.into_rgba8().to_vec());
        let bounds = Aabb::new(
            Vector2::ZERO,
            Vector2::new(pixel_width as f64, pixel_height as f64),
        );

        Self {
            data,
            rectangle: Rectangle::from_p2d_aabb(bounds),
            pixel_width,
            pixel_height,
            memory_format,
        }
    }
}

impl Drawable for Image {
    /// Draw itself on a [piet::RenderContext].
    ///
    /// Expects image to be in rgba8-premultiplied format, else drawing will fail.
    ///
    /// `image_scale` has no meaning here, because the bitmap is already provided.
    fn draw(&self, cx: &mut impl piet::RenderContext, _image_scale: f64) -> anyhow::Result<()> {
        let piet_image_format = piet::ImageFormat::from(self.memory_format);

        cx.save().map_err(|e| anyhow::anyhow!("{e:?}"))?;
        let piet_image = cx
            .make_image(
                self.pixel_width as usize,
                self.pixel_height as usize,
                &self.data,
                piet_image_format,
            )
            .map_err(|e| anyhow::anyhow!("{e:?}"))?;
        cx.transform(self.rectangle.affine.to_kurbo());
        cx.draw_image(
            &piet_image,
            self.rectangle.cuboid.local_aabb().to_kurbo_rect(),
            piet::InterpolationMode::Bilinear,
        );
        cx.restore().map_err(|e| anyhow::anyhow!("{e:?}"))?;
        Ok(())
    }
}

impl Transformable for Image {
    fn translate(&mut self, offset: Vector2) {
        self.rectangle.translate(offset)
    }

    fn rotate(&mut self, angle: f64, center: Vector2) {
        self.rectangle.rotate(angle, center)
    }

    fn scale(&mut self, scale: Vector2) {
        self.rectangle.scale(scale)
    }
}

impl Image {
    /// Create a new image from a buffer of premultiplied RGBA8 bytes.
    ///
    /// This avoids an encode/decode round-trip for pixel data that is already in the
    /// format rnote keeps images in memory ([`ImageMemoryFormat::R8g8b8a8Premultiplied`]).
    pub fn from_premultiplied_rgba8(data: Vec<u8>, pixel_width: u32, pixel_height: u32) -> Self {
        let bounds = Aabb::new(
            Vector2::ZERO,
            Vector2::new(pixel_width as f64, pixel_height as f64),
        );

        Self {
            data: glib::Bytes::from_owned(data),
            rectangle: Rectangle::from_p2d_aabb(bounds),
            pixel_width,
            pixel_height,
            memory_format: ImageMemoryFormat::R8g8b8a8Premultiplied,
        }
    }

    pub fn assert_valid(&self) -> anyhow::Result<()> {
        self.rectangle.bounds().assert_valid()?;

        if self.pixel_width == 0
            || self.pixel_height == 0
            || self.data.len() as u32 != 4 * self.pixel_width * self.pixel_height
        {
            Err(anyhow::anyhow!(
                "Asserting image validity failed, invalid size or data."
            ))
        } else {
            Ok(())
        }
    }

    /// Applies an alpha multiplier to all pixels in the image.
    ///
    /// This is used for highlighter strokes where the stroke is drawn with full opacity
    /// and then the intended alpha is applied during compositing.
    ///
    /// The alpha parameter should be in the range [0.0, 1.0].
    pub fn apply_alpha(&mut self, alpha: f64) {
        if alpha >= 1.0 {
            return; // No modification needed for full opacity
        }

        let alpha_u8 = (alpha.clamp(0.0, 1.0) * 255.0).round() as u8;
        let mut data = self.data.to_vec();

        // The image is in RGBA8 premultiplied format.
        // For premultiplied alpha: R' = R * A, G' = G * A, B' = B * A, A' = A
        // To apply an additional alpha multiplier, we scale all components by the multiplier.
        for pixel in data.chunks_exact_mut(4) {
            pixel[0] = ((u16::from(pixel[0]) * u16::from(alpha_u8)) / 255) as u8;
            pixel[1] = ((u16::from(pixel[1]) * u16::from(alpha_u8)) / 255) as u8;
            pixel[2] = ((u16::from(pixel[2]) * u16::from(alpha_u8)) / 255) as u8;
            pixel[3] = ((u16::from(pixel[3]) * u16::from(alpha_u8)) / 255) as u8;
        }

        self.data = glib::Bytes::from_owned(data);
    }

    pub fn try_from_encoded_bytes(bytes: &[u8]) -> Result<Self, anyhow::Error> {
        let reader = ImageReader::new(io::Cursor::new(bytes)).with_guessed_format()?;
        Ok(Image::from(reader.decode()?))
    }

    pub fn try_from_cairo_surface(
        mut surface: cairo::ImageSurface,
        bounds: Aabb,
    ) -> anyhow::Result<Self> {
        let width = surface.width() as u32;
        let height = surface.height() as u32;
        let data = surface.data()?.to_vec();

        Ok(Image {
            data: glib::Bytes::from_owned(convert_image_bgra_to_rgba(width, height, data)),
            rectangle: Rectangle::from_p2d_aabb(bounds),
            pixel_width: width,
            pixel_height: height,
            // cairo renders to bgra8-premultiplied, but we convert it to rgba8-premultiplied
            memory_format: ImageMemoryFormat::R8g8b8a8Premultiplied,
        })
    }

    pub fn into_imgbuf(
        self,
    ) -> Result<image::ImageBuffer<image::Rgba<u8>, Vec<u8>>, anyhow::Error> {
        self.assert_valid()?;

        match self.memory_format {
            ImageMemoryFormat::R8g8b8a8Premultiplied => image::RgbaImage::from_vec(
                self.pixel_width,
                self.pixel_height,
                self.data.to_vec(),
            )
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "Creating RgbaImage from data failed for image with memory-format {:?}.",
                    self.memory_format
                )
            }),
        }
    }

    /// Encodes the image into the provided format.
    ///
    /// When the format is `Jpeg`, the quality should be provided, but falls back to 93 if it is None.
    pub fn into_encoded_bytes(
        self,
        format: image::ImageFormat,
        quality: Option<u8>,
    ) -> Result<Vec<u8>, anyhow::Error> {
        const QUALITY_FALLBACK: u8 = 93;

        self.assert_valid()?;
        let mut bytes_buf: Cursor<Vec<u8>> = Cursor::new(Vec::new());
        let dynamic_image = image::DynamicImage::ImageRgba8(
            self.into_imgbuf()
                .context("Converting image to image::ImageBuffer failed.")?,
        );
        match format {
            image::ImageFormat::Jpeg => {
                image::codecs::jpeg::JpegEncoder::new_with_quality(
                    &mut bytes_buf,
                    quality.map(|q| q.clamp(0, 100)).unwrap_or(QUALITY_FALLBACK),
                )
                .encode_image(&dynamic_image)
                .context("Encode dynamic image to jpeg failed.")?;
            }
            format => {
                dynamic_image
                    .write_to(&mut bytes_buf, format)
                    .context("Encode dynamic image to format '{format}' failed.")?;
            }
        }

        Ok(bytes_buf.into_inner())
    }

    #[cfg(feature = "ui")]
    pub fn to_memtexture(&self) -> Result<gtk4::gdk::MemoryTexture, anyhow::Error> {
        self.assert_valid()?;

        Ok(gtk4::gdk::MemoryTexture::new(
            self.pixel_width as i32,
            self.pixel_height as i32,
            self.memory_format.into(),
            &self.data,
            (self.pixel_width * 4) as usize,
        ))
    }

    #[cfg(feature = "ui")]
    pub fn to_rendernode(&self) -> Result<gtk4::gsk::RenderNode, anyhow::Error> {
        use crate::ext::GrapheneRectExt;
        use gtk4::{graphene, gsk, prelude::*};

        self.assert_valid()?;

        let memtexture = self.to_memtexture()?;
        let texture_node = gsk::TextureNode::new(
            &memtexture,
            &graphene::Rect::from_p2d_aabb(self.rectangle.cuboid.local_aabb()),
        )
        .upcast();
        let transform_node = gsk::TransformNode::new(
            &texture_node,
            &crate::utils::affine_to_gsk(&self.rectangle.affine),
        )
        .upcast();

        Ok(transform_node)
    }

    #[cfg(feature = "ui")]
    pub fn images_to_rendernodes<'a>(
        images: impl IntoIterator<Item = &'a Self>,
    ) -> Result<Vec<gtk4::gsk::RenderNode>, anyhow::Error> {
        images.into_iter().map(|img| img.to_rendernode()).collect()
    }

    /// Generates an image with a provided closure that draws onto a [cairo::Context].
    pub fn gen_with_cairo<F>(
        draw_func: F,
        mut bounds: Aabb,
        image_scale: f64,
    ) -> anyhow::Result<Self>
    where
        F: FnOnce(&cairo::Context) -> anyhow::Result<()>,
    {
        bounds.ensure_positive();
        bounds.loosen(1.0);
        bounds.assert_valid()?;

        let width_scaled = ((bounds.extents()[0]) * image_scale).round() as u32;
        let height_scaled = ((bounds.extents()[1]) * image_scale).round() as u32;

        let mut image_surface = cairo::ImageSurface::create(
            cairo::Format::ARgb32,
            width_scaled as i32,
            height_scaled as i32,
        )
        .map_err(|e| {
            anyhow::anyhow!(
                "creating image surface with dimensions ({}, {}) failed, Err: {e:?}",
                width_scaled,
                height_scaled,
            )
        })?;

        {
            let cairo_cx = cairo::Context::new(&image_surface)?;
            cairo_cx.scale(image_scale, image_scale);
            cairo_cx.translate(-bounds.mins[0], -bounds.mins[1]);
            // Apply the draw function
            draw_func(&cairo_cx)?;
        }

        let data = image_surface
            .data()
            .map_err(|e| anyhow::anyhow!("accessing image surface data failed, Err: {e:?}"))?
            .to_vec();

        Ok(Image {
            data: glib::Bytes::from_owned(convert_image_bgra_to_rgba(
                width_scaled,
                height_scaled,
                data,
            )),
            rectangle: Rectangle::from_p2d_aabb(bounds),
            pixel_width: width_scaled,
            pixel_height: height_scaled,
            // cairo renders to bgra8-premultiplied, but we convert it to rgba8-premultiplied
            memory_format: ImageMemoryFormat::R8g8b8a8Premultiplied,
        })
    }

    /// Generates an image with a provided closure that draws onto a [piet_cairo::CairoRenderContext].
    pub fn gen_with_piet<F>(draw_func: F, bounds: Aabb, image_scale: f64) -> anyhow::Result<Self>
    where
        F: FnOnce(&mut piet_cairo::CairoRenderContext) -> anyhow::Result<()>,
    {
        let cairo_draw_fn = move |cairo_cx: &cairo::Context| -> anyhow::Result<()> {
            let mut piet_cx = piet_cairo::CairoRenderContext::new(cairo_cx);
            // Apply the draw function
            draw_func(&mut piet_cx)?;
            piet_cx
                .finish()
                .map_err(|e| anyhow::anyhow!("finishing piet context failed, Err: {e:?}"))?;
            Ok(())
        };

        Self::gen_with_cairo(cairo_draw_fn, bounds, image_scale)
    }
}

pub(super) fn convert_image_bgra_to_rgba(_width: u32, _height: u32, mut bytes: Vec<u8>) -> Vec<u8> {
    for src in bytes.as_chunks_mut::<4>().0 {
        let (blue, green, red, alpha) = (src[0], src[1], src[2], src[3]);
        src[0] = red;
        src[1] = green;
        src[2] = blue;
        src[3] = alpha;
    }
    bytes
}

/// The compression level used when compressing the pixel data of an [`EncodedImage`].
///
/// Chosen for a balance of encoding speed and size: on a scanned textbook page (1123x1589
/// premultiplied RGBA8, 7.1 MB of pixels) level 1 encodes in 8 ms and level 3 in 12 ms, both
/// producing 0.18 MB, and decoding either takes ~5 ms - an order of magnitude faster than decoding
/// the equivalent Png, which encodes 10x slower and is no smaller.
const IMAGE_ZSTD_COMPRESSION_LEVEL: i32 = 3;

/// How the pixel data of an [`EncodedImage`] is encoded.
#[non_exhaustive]
#[derive(Debug, Copy, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ImageEncoding {
    /// Uncompressed pixel data, in [`ImageMemoryFormat`].
    ///
    /// This is how images were always stored, so it is what the images of files written by older
    /// versions of Rnote are read as.
    Raw,
    /// Zstd-compressed pixel data.
    ///
    /// The pixels of a bitmap are large: a single 1123x1589 page of an imported Pdf is 7.1 MB, so a
    /// 204 page document holds 1.4 GB of them. Keeping them compressed keeps the memory of a
    /// document proportional to its compressed size instead of to its pixel count; the pixels are
    /// then decoded on demand when the image is rendered, which is bounded by the viewport.
    Zstd,
}

impl Default for ImageEncoding {
    fn default() -> Self {
        Self::Raw
    }
}

/// A bitmap image whose pixel data is stored encoded, and decoded on demand.
///
/// Holding the decoded pixels of every image of a document is what makes documents with many large
/// bitmaps (e.g. imported Pdf pages) use multiple GB of memory. An `EncodedImage` instead keeps its
/// pixels encoded, and [`EncodedImage::decode`] materializes them only when the image is rendered.
///
/// The serde representation is a superset of the one of [`Image`], with the additional `encoding`
/// field, so that images written before the pixels were compressed are read as
/// [`ImageEncoding::Raw`].
#[derive(Clone, Serialize, Deserialize)]
#[serde(default, rename = "image")]
pub struct EncodedImage {
    /// The encoded image data.
    ///
    /// Is (de)serialized with base64 encoding.
    #[serde(rename = "data", with = "crate::utils::glib_bytes_base64")]
    pub data: glib::Bytes,
    /// The target rect in the coordinate space of the document.
    #[serde(rename = "rectangle")]
    pub rectangle: Rectangle,
    /// Width of the decoded image data.
    #[serde(rename = "pixel_width")]
    pub pixel_width: u32,
    /// Height of the decoded image data.
    #[serde(rename = "pixel_height")]
    pub pixel_height: u32,
    /// Memory format of the decoded pixels.
    #[serde(rename = "memory_format")]
    pub memory_format: ImageMemoryFormat,
    /// How [`EncodedImage::data`] is encoded.
    #[serde(rename = "encoding")]
    pub encoding: ImageEncoding,
}

impl Debug for EncodedImage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EncodedImage")
            .field("data", &String::from("{.. no debug impl ..}"))
            .field("encoding", &self.encoding)
            .field("rect", &self.rectangle)
            .field("pixel_width", &self.pixel_width)
            .field("pixel_height", &self.pixel_height)
            .field("memory_format", &self.memory_format)
            .finish()
    }
}

impl Default for EncodedImage {
    fn default() -> Self {
        Self {
            data: glib::Bytes::from_owned(Vec::new()),
            rectangle: Rectangle::default(),
            pixel_width: 0,
            pixel_height: 0,
            memory_format: ImageMemoryFormat::default(),
            encoding: ImageEncoding::default(),
        }
    }
}

impl EncodedImage {
    /// Encodes the pixels of the given image with the given encoding.
    pub fn from_image(image: Image, encoding: ImageEncoding) -> anyhow::Result<Self> {
        image.assert_valid()?;

        Ok(Self {
            data: Self::encode(&image.data, encoding)?,
            rectangle: image.rectangle,
            pixel_width: image.pixel_width,
            pixel_height: image.pixel_height,
            memory_format: image.memory_format,
            encoding,
        })
    }

    /// Encodes a buffer of premultiplied RGBA8 bytes with the given encoding.
    pub fn from_premultiplied_rgba8(
        data: &[u8],
        pixel_width: u32,
        pixel_height: u32,
        encoding: ImageEncoding,
    ) -> anyhow::Result<Self> {
        let bounds = Aabb::new(
            Vector2::ZERO,
            Vector2::new(pixel_width as f64, pixel_height as f64),
        );
        let rectangle = Rectangle::from_p2d_aabb(bounds);

        // Assert validity without going through `Image`, which would copy the pixels.
        if pixel_width == 0
            || pixel_height == 0
            || data.len() as u32 != 4 * pixel_width * pixel_height
        {
            return Err(anyhow::anyhow!(
                "Creating encoded image from premultiplied rgba8 failed, invalid size or data."
            ));
        }

        Ok(Self {
            data: Self::encode(data, encoding)?,
            rectangle,
            pixel_width,
            pixel_height,
            memory_format: ImageMemoryFormat::R8g8b8a8Premultiplied,
            encoding,
        })
    }

    /// Decodes the stored pixels into an [`Image`].
    ///
    /// This is the expensive part of an [`EncodedImage`], which is why the decoded pixels are not
    /// held by the image itself, but only by whatever renders it for as long as it is visible.
    pub fn decode(&self) -> anyhow::Result<Image> {
        let image = Image {
            data: match self.encoding {
                ImageEncoding::Raw => self.data.clone(),
                ImageEncoding::Zstd => glib::Bytes::from_owned(Self::decompress(
                    &self.data,
                    self.pixel_width,
                    self.pixel_height,
                )?),
            },
            rectangle: self.rectangle,
            pixel_width: self.pixel_width,
            pixel_height: self.pixel_height,
            memory_format: self.memory_format,
        };
        image.assert_valid()?;

        Ok(image)
    }

    /// Compresses the stored pixels, so that the uncompressed pixels are not held in memory.
    ///
    /// Is a no-op if the image holds no data, or already is compressed.
    pub fn compact(&mut self) -> anyhow::Result<()> {
        if !self.needs_compaction() || self.data.is_empty() {
            return Ok(());
        }

        self.data = Self::encode(&self.data, ImageEncoding::Zstd)?;
        self.encoding = ImageEncoding::Zstd;

        Ok(())
    }

    /// Whether the stored pixels are uncompressed, and hence can be [`EncodedImage::compact`]ed.
    pub fn needs_compaction(&self) -> bool {
        self.encoding == ImageEncoding::Raw && self.pixel_width > 0 && self.pixel_height > 0
    }

    /// Whether the image holds any pixels.
    pub fn is_empty(&self) -> bool {
        self.pixel_width == 0 || self.pixel_height == 0 || self.data.is_empty()
    }

    pub fn assert_valid(&self) -> anyhow::Result<()> {
        self.rectangle.bounds().assert_valid()?;

        if self.pixel_width == 0 || self.pixel_height == 0 {
            return Err(anyhow::anyhow!(
                "Asserting encoded image validity failed, invalid size."
            ));
        }

        // Encoded data has no length to check against, but uncompressed pixels must match the
        // memory format.
        if self.encoding == ImageEncoding::Raw
            && self.data.len() as u32 != 4 * self.pixel_width * self.pixel_height
        {
            return Err(anyhow::anyhow!(
                "Asserting encoded image validity failed, invalid size or data."
            ));
        }

        Ok(())
    }

    fn encode(data: &[u8], encoding: ImageEncoding) -> anyhow::Result<glib::Bytes> {
        match encoding {
            ImageEncoding::Raw => Ok(glib::Bytes::from_owned(data.to_vec())),
            ImageEncoding::Zstd => zstd::bulk::compress(data, IMAGE_ZSTD_COMPRESSION_LEVEL)
                .map(glib::Bytes::from_owned)
                .context("Compressing image data failed."),
        }
    }

    fn decompress(data: &[u8], pixel_width: u32, pixel_height: u32) -> anyhow::Result<Vec<u8>> {
        // Decompressing with the exact expected size also guards against decompression bombs.
        let expected_len = 4 * pixel_width as usize * pixel_height as usize;

        zstd::bulk::decompress(data, expected_len).context("Decompressing image data failed.")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An image with a pattern, so that compressing it has something to work with.
    fn test_image() -> Image {
        let (pixel_width, pixel_height) = (37u32, 23u32);
        let data = (0..pixel_width * pixel_height)
            .flat_map(|i| {
                let x = i % pixel_width;
                let y = i / pixel_width;
                [(x * 7 % 256) as u8, (y * 11 % 256) as u8, 0, 255]
            })
            .collect::<Vec<u8>>();

        Image::from_premultiplied_rgba8(data, pixel_width, pixel_height)
    }

    fn assert_same_pixels(decoded: &Image, expected: &Image) {
        assert_eq!(decoded.pixel_width, expected.pixel_width);
        assert_eq!(decoded.pixel_height, expected.pixel_height);
        assert_eq!(decoded.rectangle.affine, expected.rectangle.affine);
        assert_eq!(decoded.memory_format, expected.memory_format);
        assert_eq!(&decoded.data[..], &expected.data[..], "pixel data changed");
    }

    #[test]
    fn encoding_and_decoding_an_image_is_lossless() {
        let image = test_image();

        for encoding in [ImageEncoding::Raw, ImageEncoding::Zstd] {
            let encoded = EncodedImage::from_image(image.clone(), encoding)
                .expect("encoding the image failed");
            assert_eq!(encoded.encoding, encoding);

            assert_same_pixels(&encoded.decode().expect("decoding failed"), &image);
        }
    }

    #[test]
    fn compacting_an_encoded_image_is_lossless() {
        let image = test_image();
        let mut encoded =
            EncodedImage::from_image(image.clone(), ImageEncoding::Raw).expect("encoding failed");
        assert!(encoded.needs_compaction());
        let uncompressed_len = encoded.data.len();

        encoded.compact().expect("compacting failed");

        assert!(!encoded.needs_compaction());
        assert!(encoded.data.len() < uncompressed_len);
        assert_same_pixels(&encoded.decode().expect("decoding failed"), &image);

        // Compacting again must not change anything.
        let compacted_len = encoded.data.len();
        encoded.compact().expect("compacting again failed");
        assert_eq!(encoded.data.len(), compacted_len);
    }

    #[test]
    fn images_written_before_the_pixels_were_compressed_are_read_as_raw() {
        let image = test_image();

        // `Image` is how the pixels were stored before `EncodedImage` existed, so serializing it
        // produces exactly the representation an older Rnote version wrote.
        let legacy_json = serde_json::to_string(&image).expect("serializing the image failed");
        assert!(
            !legacy_json.contains("encoding"),
            "the representation written by older versions has no encoding field"
        );

        let encoded: EncodedImage =
            serde_json::from_str(&legacy_json).expect("reading the legacy representation failed");
        assert_eq!(encoded.encoding, ImageEncoding::Raw);
        assert!(encoded.needs_compaction());

        assert_same_pixels(&encoded.decode().expect("decoding failed"), &image);
    }

    #[test]
    fn encoded_images_are_read_back_as_they_were_written() {
        let image = test_image();
        let encoded =
            EncodedImage::from_image(image.clone(), ImageEncoding::Zstd).expect("encoding failed");

        let json = serde_json::to_string(&encoded).expect("serializing failed");
        let read_back: EncodedImage = serde_json::from_str(&json).expect("reading failed");

        assert_eq!(read_back.encoding, ImageEncoding::Zstd);
        assert!(!read_back.needs_compaction());
        assert_same_pixels(&read_back.decode().expect("decoding failed"), &image);
    }
}
