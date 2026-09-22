// Imports
use crate::document::background;
use crate::engine::import::XoppImportPrefs;
use crate::fileformats::{FileFormatLoader, rnoteformat, xoppformat};
use crate::store::{ChronoComponent, StrokeKey};
use crate::strokes::Stroke;
use crate::{Camera, Document, Engine};
use futures::channel::oneshot;
use p2d::math::Vector2;
use serde::{Deserialize, Serialize};
use slotmap::{SecondaryMap, SlotMap};
use std::sync::Arc;
use std::time::Instant;
use tracing::error;

/// Trait for types which hold configuration needed for engine snapshots
pub trait Snapshotable {
    fn extract_snapshot_data(&self) -> Self;
}

// An engine snapshot, used when loading/saving the current document from/into a file.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, rename = "engine_snapshot")]
pub struct EngineSnapshot {
    #[serde(rename = "document")]
    pub document: Document,
    #[serde(rename = "camera")]
    pub camera: Camera,
    #[serde(rename = "stroke_components")]
    pub stroke_components: Arc<SlotMap<StrokeKey, Arc<Stroke>>>,
    #[serde(rename = "chrono_components")]
    pub chrono_components: Arc<SecondaryMap<StrokeKey, Arc<ChronoComponent>>>,
    #[serde(rename = "chrono_counter")]
    pub chrono_counter: u32,
}

impl Default for EngineSnapshot {
    fn default() -> Self {
        Self {
            document: Document::default(),
            camera: Camera::default(),
            stroke_components: Arc::new(SlotMap::with_key()),
            chrono_components: Arc::new(SecondaryMap::new()),
            chrono_counter: 0,
        }
    }
}

impl EngineSnapshot {
    /// Compresses the pixels of all bitmap images of the snapshot.
    ///
    /// The decoded pixels of a bitmap are large - a single imported Pdf page is 7.1 MB at
    /// 1123x1589 - so a document holding many of them keeps multiple GB of memory resident. Images
    /// are stored with their pixels compressed, but files written before that was the case are read
    /// uncompressed, and those are compressed here. The pixels are then decoded again on demand
    /// when an image is rendered, which is bounded by the viewport.
    ///
    /// Runs in parallel, since a document can hold hundreds of large images.
    pub fn compact_bitmap_images(&mut self) {
        use rayon::prelude::*;

        // The strokes are held behind `Arc`s, but nothing else holds them at this point, so
        // `make_mut()` compacts them in place instead of cloning them.
        let stroke_components = Arc::make_mut(&mut self.stroke_components);

        stroke_components
            .values_mut()
            .collect::<Vec<_>>()
            .into_par_iter()
            .for_each(|stroke| {
                if !matches!(**stroke, Stroke::BitmapImage(_)) {
                    return;
                }

                if let Stroke::BitmapImage(bitmapimage) = Arc::make_mut(stroke)
                    && let Err(e) = bitmapimage.compact()
                {
                    // The pixels stay uncompressed, which only costs memory, so this is not fatal.
                    error!("Compacting the pixels of a bitmap image failed, Err: {e:?}");
                }
            });
    }

    /// Loads a snapshot from the bytes of a .rnote file.
    ///
    /// To import this snapshot into the current engine, use [`Engine::load_snapshot()`].
    pub async fn load_from_rnote_bytes(bytes: Vec<u8>) -> anyhow::Result<Self> {
        let (snapshot_sender, snapshot_receiver) = oneshot::channel::<anyhow::Result<Self>>();

        rayon::spawn(move || {
            #[rustfmt::skip]
            let result = || -> anyhow::Result<Self> {
                let start = Instant::now();
                rnoteformat::load_engine_snapshot_from_bytes(&bytes)
                    .inspect(|_| {tracing::debug!("Going from bytes to `EngineSnapshot` took {} ms", Instant::now().duration_since(start).as_millis())})
            };

            if let Err(_data) = snapshot_sender.send(result()) {
                error!(
                    "Sending bytes result to receiver failed while loading rnote bytes in. Receiver already dropped."
                );
            }
        });

        snapshot_receiver.await?
    }
    /// Loads from the bytes of a Xournal++ .xopp file.
    ///
    /// To import this snapshot into the current engine, use [`Engine::load_snapshot()`].
    pub async fn load_from_xopp_bytes(
        bytes: Vec<u8>,
        xopp_import_prefs: XoppImportPrefs,
    ) -> anyhow::Result<Self> {
        let (snapshot_sender, snapshot_receiver) = oneshot::channel::<anyhow::Result<Self>>();

        rayon::spawn(move || {
            let result = || -> anyhow::Result<Self> {
                let xopp_file = xoppformat::XoppFile::load_from_bytes(&bytes)?;

                // Extract the largest width of all pages, add together all heights
                let (doc_width, doc_height) = xopp_file
                    .xopp_root
                    .pages
                    .iter()
                    .map(|page| (page.width, page.height))
                    .fold(
                        (0_f64, 0_f64),
                        |(prev_width, prev_height), (next_width, next_height)| {
                            (prev_width.max(next_width), prev_height + next_height)
                        },
                    );
                let no_pages = xopp_file.xopp_root.pages.len() as u32;

                let mut engine = Engine::default();

                // We convert all values from the hardcoded 72 DPI of Xopp files to the preferred dpi
                engine.document.config.format.set_dpi(xopp_import_prefs.dpi);

                engine.document.x = 0.0;
                engine.document.y = 0.0;
                engine.document.width = crate::utils::convert_value_dpi(
                    doc_width,
                    xoppformat::XoppFile::DPI,
                    xopp_import_prefs.dpi,
                );
                engine.document.height = crate::utils::convert_value_dpi(
                    doc_height,
                    xoppformat::XoppFile::DPI,
                    xopp_import_prefs.dpi,
                );

                engine
                    .document
                    .config
                    .format
                    .set_width(crate::utils::convert_value_dpi(
                        doc_width,
                        xoppformat::XoppFile::DPI,
                        xopp_import_prefs.dpi,
                    ));
                engine
                    .document
                    .config
                    .format
                    .set_height(crate::utils::convert_value_dpi(
                        doc_height / (no_pages as f64),
                        xoppformat::XoppFile::DPI,
                        xopp_import_prefs.dpi,
                    ));

                if let Some(first_page) = xopp_file.xopp_root.pages.first()
                    && let xoppformat::XoppBackgroundType::Solid {
                        color: _color,
                        style: _style,
                    } = &first_page.background.bg_type
                {
                    // Xopp background styles are not compatible with Rnotes, so everything is plain for now
                    engine.document.config.background.pattern = background::PatternStyle::None;
                }

                // Offsetting as rnote has one global coordinate space
                let mut offset = Vector2::ZERO;

                for page in xopp_file.xopp_root.pages.into_iter() {
                    for layers in page.layers.into_iter() {
                        // import strokes
                        for new_xoppstroke in layers.strokes.into_iter() {
                            match Stroke::from_xoppstroke(
                                new_xoppstroke,
                                offset,
                                xopp_import_prefs.dpi,
                            ) {
                                Ok((new_stroke, layer)) => {
                                    engine.store.insert_stroke(new_stroke, Some(layer));
                                }
                                Err(e) => {
                                    error!(
                                        "Creating Stroke from XoppStroke failed while loading Xopp bytess, Err: {e:?}",
                                    );
                                }
                            }
                        }

                        // import images
                        for new_xoppimage in layers.images.into_iter() {
                            match Stroke::from_xoppimage(
                                new_xoppimage,
                                offset,
                                xopp_import_prefs.dpi,
                            ) {
                                Ok(new_image) => {
                                    engine.store.insert_stroke(new_image, None);
                                }
                                Err(e) => {
                                    error!(
                                        "Creating Stroke from XoppImage failed while loading Xopp bytes, Err: {e:?}",
                                    );
                                }
                            }
                        }

                        for new_xopptext in layers.texts.into_iter() {
                            match Stroke::from_xopptext(new_xopptext, offset, xopp_import_prefs.dpi)
                            {
                                Ok(new_text) => {
                                    engine.store.insert_stroke(new_text, None);
                                }
                                Err(e) => {
                                    error!(
                                        "Creating Stroke from XoppText failed while loading Xopp bytes, Err: {e:?}",
                                    );
                                }
                            }
                        }
                    }

                    // Only add to y offset, results in vertical pages
                    offset[1] += crate::utils::convert_value_dpi(
                        page.height,
                        xoppformat::XoppFile::DPI,
                        xopp_import_prefs.dpi,
                    );
                }

                Ok(engine.take_snapshot())
            };

            if snapshot_sender.send(result()).is_err() {
                error!(
                    "Sending result to receiver while loading Xopp bytes failed. Receiver already dropped"
                );
            }
        });

        snapshot_receiver.await?
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::image::{EncodedImage, ImageEncoding};
    use crate::strokes::BitmapImage;
    use crate::strokes::resize::ImageSizeOption;

    #[test]
    fn compacting_the_pixels_of_bitmap_images_is_lossless() {
        let (pixel_width, pixel_height) = (8u32, 8u32);
        let pixels: Vec<u8> = (0..pixel_width * pixel_height)
            .flat_map(|i| [(i * 3 % 256) as u8, (i * 5 % 256) as u8, 0, 255])
            .collect();
        // A document as an older version of Rnote wrote it: the pixels are stored uncompressed.
        let image = EncodedImage::from_premultiplied_rgba8(
            &pixels,
            pixel_width,
            pixel_height,
            ImageEncoding::Raw,
        )
        .expect("encoding the test image failed");
        let bitmapimage = BitmapImage::from_encoded_image(
            image,
            Vector2::ZERO,
            ImageSizeOption::RespectOriginalSize,
        )
        .expect("creating the test bitmap image failed");

        let mut snapshot = EngineSnapshot::default();
        Arc::make_mut(&mut snapshot.stroke_components)
            .insert(Arc::new(Stroke::BitmapImage(bitmapimage)));

        snapshot.compact_bitmap_images();

        let stroke = snapshot
            .stroke_components
            .values()
            .next()
            .expect("the stroke should still be there");
        let Stroke::BitmapImage(bitmapimage) = &**stroke else {
            panic!("expected a bitmap image stroke");
        };
        assert_eq!(bitmapimage.image.encoding, ImageEncoding::Zstd);
        assert!(!bitmapimage.image.needs_compaction());
        // Compacting must not change the pixels.
        assert_eq!(&bitmapimage.decoded_image().unwrap().data[..], &pixels[..]);
    }
}
