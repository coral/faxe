use std::{
    fs::{self, File},
    io::{BufWriter, Write},
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Instant,
};

use fast_image_resize::{FilterType, ResizeAlg, ResizeOptions, Resizer};
use image::{DynamicImage, GrayImage, ImageReader, Luma, imageops};
use pdfium_render::prelude::*;
use tiff::{
    encoder::{Rational, TiffEncoder},
    tags::Tag,
};
use uuid::Uuid;

use crate::{Binarization, DocumentInput, DocumentOptions, Error, PreparedDocument, Result};

const WIDTH: u32 = 1728;
const MAX_PAGES: u32 = 250;
const MAX_SOURCE_BYTES: u64 = 128 * 1024 * 1024;

type WakeCallback = Arc<dyn Fn() + Send + Sync>;

#[derive(Clone, Default)]
pub struct Cancellation {
    notify: Arc<tokio::sync::Notify>,
    flag: Arc<AtomicBool>,
    wake: Arc<std::sync::Mutex<Option<WakeCallback>>>,
}
impl std::fmt::Debug for Cancellation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Cancellation")
            .field("cancelled", &self.is_cancelled())
            .finish()
    }
}

impl Cancellation {
    pub fn cancel(&self) {
        self.flag.store(true, Ordering::Release);
        self.notify.notify_waiters();
        let wake = self.wake.lock().ok().and_then(|w| w.clone());
        if let Some(wake) = wake {
            wake();
        }
    }

    pub fn is_cancelled(&self) -> bool {
        self.flag.load(Ordering::Acquire)
    }
    pub async fn cancelled(&self) {
        let notified = self.notify.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();
        if !self.is_cancelled() {
            notified.await;
        }
    }

    pub(crate) fn flag(&self) -> Arc<AtomicBool> {
        self.flag.clone()
    }
    pub(crate) fn set_wake(&self, wake: Option<WakeCallback>) {
        if let Ok(mut slot) = self.wake.lock() {
            *slot = wake.clone();
        }
        if self.is_cancelled()
            && let Some(wake) = wake
        {
            wake();
        }
    }
    pub(crate) fn check(&self) -> Result<()> {
        match self.is_cancelled() {
            true => Err(Error::Cancelled),
            false => Ok(()),
        }
    }
}

#[derive(Debug, Clone)]
pub struct Documents {
    root: PathBuf,
    cache: PathBuf,
}

/// One full-resolution grayscale page, retained in memory while adjusting its preview.
pub struct PreviewSource {
    gray: GrayImage,
    options: DocumentOptions,
}

impl std::fmt::Debug for PreviewSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PreviewSource")
            .field("dimensions", &self.gray.dimensions())
            .finish()
    }
}

impl PreviewSource {
    pub fn render(&self, binarization: Binarization, contrast: i16) -> Result<crate::Preview> {
        let mut page = self.gray.clone();
        let options = DocumentOptions {
            binarization,
            contrast,
            ..self.options
        };
        binarize(&mut page, options);
        let preview = DynamicImage::ImageLuma8(preview_gray(&mut Resizer::new(), &page, options)?)
            .into_rgba8();
        Ok(crate::Preview {
            width: preview.width(),
            height: preview.height(),
            pixels: preview.into_raw(),
        })
    }
}

impl Documents {
    pub fn new(root: impl Into<PathBuf>) -> Result<Self> {
        let root = root.into();
        fs::create_dir_all(&root)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&root, fs::Permissions::from_mode(0o700))?;
        }
        Ok(Self {
            cache: root.join("cache"),
            root,
        })
    }

    pub fn with_cache(mut self, cache: PathBuf) -> Self {
        self.cache = cache;
        self
    }

    pub fn fax_path(&self, id: Uuid) -> PathBuf {
        self.root.join(id.to_string()).join("fax.tiff")
    }

    pub fn preview_path(&self, id: Uuid, page: u32) -> PathBuf {
        self.root
            .join(id.to_string())
            .join(format!("page-{page:04}.png"))
    }

    fn grayscale_path(&self, id: Uuid, page: u32) -> PathBuf {
        self.root
            .join(id.to_string())
            .join(format!("gray-{page:04}.png"))
    }

    pub fn preview_source(&self, id: Uuid, page: u32) -> Result<PreviewSource> {
        let document = self.load(id)?;
        if page == 0 || page > document.pages {
            return Err(Error::Invalid("Preview page is out of range".into()));
        }
        Ok(PreviewSource {
            gray: image::open(self.grayscale_path(id, page))?.into_luma8(),
            options: document.options,
        })
    }

    pub fn load(&self, id: Uuid) -> Result<PreparedDocument> {
        let document: PreparedDocument = serde_json::from_slice(&fs::read(
            self.root.join(id.to_string()).join("document.json"),
        )?)?;
        if document.id != id || document.pages == 0 || !self.fax_path(id).is_file() {
            return Err(Error::Invalid("Prepared document is incomplete".into()));
        }
        Ok(document)
    }

    /// Publishes the spool directory only after every page and its metadata are complete.
    pub fn prepare(
        &self,
        input: DocumentInput,
        cancellation: &Cancellation,
        progress: impl FnMut(u32),
    ) -> Result<PreparedDocument> {
        self.prepare_impl(input, None, cancellation, progress)
    }

    /// Reuses grayscale pages without decoding originals or resampling again.
    /// Publishes a new document so already queued faxes remain immutable.
    pub fn adjust(
        &self,
        id: Uuid,
        options: DocumentOptions,
        cancellation: &Cancellation,
        progress: impl FnMut(u32),
    ) -> Result<PreparedDocument> {
        let document = self.load(id)?;
        if options.paper != document.options.paper
            || options.resolution != document.options.resolution
        {
            return Err(Error::Invalid(
                "Prepare the document again after changing paper or resolution".into(),
            ));
        }
        let input = DocumentInput {
            paths: (1..=document.pages)
                .map(|page| self.grayscale_path(id, page))
                .collect(),
            options,
        };
        self.prepare_impl(input, Some(&document), cancellation, progress)
    }

    fn prepare_impl(
        &self,
        input: DocumentInput,
        original: Option<&PreparedDocument>,
        cancellation: &Cancellation,
        mut progress: impl FnMut(u32),
    ) -> Result<PreparedDocument> {
        let started = Instant::now();
        if input.paths.is_empty() {
            return Err(Error::Invalid("Add at least one PDF, JPEG, or PNG".into()));
        }
        let staging = tempfile::Builder::new()
            .prefix("preparing-")
            .tempdir_in(&self.root)?;
        let file = File::create(staging.path().join("fax.tiff"))?;
        let mut writer = BufWriter::new(file);
        let mut encoder = TiffEncoder::new(&mut writer)?;
        let mut pages = 0;
        let mut resizer = Resizer::new();
        let mut append = |image: DynamicImage| -> Result<()> {
            cancellation.check()?;
            if pages >= MAX_PAGES {
                return Err(Error::Invalid(format!(
                    "A fax may contain at most {MAX_PAGES} pages"
                )));
            }
            let raster_started = Instant::now();
            let mut page = match original {
                Some(_) => image.into_luma8(),
                None => rasterize(image, input.options, &mut resizer)?,
            };
            let raster_ms = raster_started.elapsed().as_millis();
            cancellation.check()?;
            let cache_started = Instant::now();
            let gray_path = staging.path().join(format!("gray-{:04}.png", pages + 1));
            match original {
                Some(document) => {
                    let source = self.grayscale_path(document.id, pages + 1);
                    if fs::hard_link(&source, &gray_path).is_err() {
                        fs::copy(source, gray_path)?;
                    }
                }
                None => page.save(gray_path)?,
            }
            let cache_ms = cache_started.elapsed().as_millis();
            let tone_started = Instant::now();
            binarize(&mut page, input.options);
            let tone_ms = tone_started.elapsed().as_millis();
            let tiff_started = Instant::now();
            write_page(&mut encoder, &page, input.options.resolution.vertical_dpi())?;
            let tiff_ms = tiff_started.elapsed().as_millis();
            pages += 1;
            let preview_started = Instant::now();
            preview_gray(&mut resizer, &page, input.options)?
                .save(staging.path().join(format!("page-{pages:04}.png")))?;
            tracing::debug!(
                page = pages,
                raster_ms,
                cache_ms,
                tone_ms,
                tiff_ms,
                preview_ms = preview_started.elapsed().as_millis(),
                "Prepared page"
            );
            progress(pages);
            Ok(())
        };
        for path in &input.paths {
            cancellation.check()?;
            let metadata = fs::metadata(path)?;
            if !metadata.is_file() {
                return Err(Error::Invalid(format!(
                    "Not a regular file: {}",
                    path.display()
                )));
            }
            if metadata.len() > MAX_SOURCE_BYTES {
                return Err(Error::Invalid(format!(
                    "{} exceeds the 128 MiB source limit",
                    path.display()
                )));
            }
            match path
                .extension()
                .and_then(|s| s.to_str())
                .unwrap_or_default()
                .to_ascii_lowercase()
                .as_str()
            {
                "pdf" => {
                    let renderer = self.pdfium()?;
                    let document = renderer.load_pdf_from_file(path, None).map_err(pdf_error)?;
                    if document.pages().is_empty() {
                        return Err(Error::Invalid(format!("{} has no pages", path.display())));
                    }
                    for page in document.pages().iter() {
                        cancellation.check()?;
                        let image = page
                            .render_with_config(
                                &PdfRenderConfig::new()
                                    .set_target_width(WIDTH as i32)
                                    .set_maximum_height(4096),
                            )
                            .map_err(pdf_error)?
                            .as_image()
                            .map_err(pdf_error)?;
                        append(image)?;
                    }
                }
                "jpg" | "jpeg" | "png" => {
                    let decode_started = Instant::now();
                    let mut reader = ImageReader::open(path)?.with_guessed_format()?;
                    let mut limits = image::Limits::default();
                    limits.max_image_width = Some(16384);
                    limits.max_image_height = Some(16384);
                    limits.max_alloc = Some(256 * 1024 * 1024);
                    reader.limits(limits);
                    let mut decoder = reader.into_decoder()?;
                    use image::ImageDecoder;
                    let orientation = decoder.orientation()?;
                    let mut image = DynamicImage::from_decoder(decoder)?;
                    image.apply_orientation(orientation);
                    tracing::debug!(
                        width = image.width(),
                        height = image.height(),
                        decode_ms = decode_started.elapsed().as_millis(),
                        "Decoded source image"
                    );
                    append(image)?;
                }
                _ => {
                    return Err(Error::Invalid(format!(
                        "Unsupported document: {}",
                        path.display()
                    )));
                }
            }
        }
        let publish_started = Instant::now();
        writer.flush()?;
        writer.get_ref().sync_all()?;
        drop(writer);
        cancellation.check()?;
        let document = PreparedDocument {
            id: Uuid::new_v4(),
            pages,
            source_names: original
                .map(|document| document.source_names.clone())
                .unwrap_or_else(|| {
                    input
                        .paths
                        .iter()
                        .map(|path| {
                            path.file_name()
                                .unwrap_or_default()
                                .to_string_lossy()
                                .into_owned()
                        })
                        .collect()
                }),
            options: input.options,
        };
        let mut metadata = File::create(staging.path().join("document.json"))?;
        serde_json::to_writer_pretty(&mut metadata, &document)?;
        metadata.sync_all()?;
        // Windows cannot rename the staging directory with files still open in it.
        drop(metadata);
        fs::rename(staging.path(), self.root.join(document.id.to_string()))?;
        tracing::debug!(
            pages,
            publish_ms = publish_started.elapsed().as_millis(),
            total_ms = started.elapsed().as_millis(),
            "Published prepared document"
        );
        Ok(document)
    }

    pub(crate) fn pdfium(&self) -> Result<&'static Pdfium> {
        static PDFIUM: std::sync::OnceLock<std::result::Result<Pdfium, String>> =
            std::sync::OnceLock::new();
        match PDFIUM.get_or_init(|| self.load_pdfium().map_err(|error| error.to_string())) {
            Ok(renderer) => Ok(renderer),
            Err(error) => Err(Error::Pdf(error.clone())),
        }
    }

    fn load_pdfium(&self) -> Result<Pdfium> {
        #[cfg(target_os = "macos")]
        const LIBRARY: (&str, &[u8]) = (
            "libpdfium.dylib",
            include_bytes!(concat!(env!("FAXE_PDFIUM_DIR"), "/libpdfium.dylib")),
        );
        #[cfg(target_os = "linux")]
        const LIBRARY: (&str, &[u8]) = (
            "libpdfium.so",
            include_bytes!(concat!(env!("FAXE_PDFIUM_DIR"), "/libpdfium.so")),
        );
        #[cfg(target_os = "windows")]
        const LIBRARY: (&str, &[u8]) = (
            "pdfium.dll",
            include_bytes!(concat!(env!("FAXE_PDFIUM_DIR"), "/pdfium.dll")),
        );

        // Packaged applications load their bundled runtime. Development builds
        // retain the embedded fallback so `cargo run` needs no staging step.
        let executable = std::env::current_exe()?;
        if let Some(directory) = executable.parent() {
            #[cfg(target_os = "macos")]
            let packaged = directory.join("../Frameworks").join(LIBRARY.0);
            #[cfg(target_os = "linux")]
            let packaged = directory.join("../lib").join(LIBRARY.0);
            #[cfg(target_os = "windows")]
            let packaged = directory.join(LIBRARY.0);
            if packaged.is_file() {
                return Ok(Pdfium::new(
                    Pdfium::bind_to_library(packaged).map_err(pdf_error)?,
                ));
            }
        }

        let runtime = self.cache.join("runtime-pdfium-8044");
        fs::create_dir_all(&runtime)?;
        let path = runtime.join(LIBRARY.0);
        if !path.is_file() {
            let mut staging = tempfile::NamedTempFile::new_in(&runtime)?;
            staging.write_all(LIBRARY.1)?;
            staging.as_file().sync_all()?;
            match staging.persist_noclobber(&path) {
                Ok(_) => (),
                Err(error) if error.error.kind() == std::io::ErrorKind::AlreadyExists => (),
                Err(error) => return Err(error.error.into()),
            }
        }
        Ok(Pdfium::new(
            Pdfium::bind_to_library(path).map_err(pdf_error)?,
        ))
    }
}

fn pdf_error(error: PdfiumError) -> Error {
    Error::Pdf(error.to_string())
}

// Keep the existing fax luminance and white-background alpha compositing, but
// avoid expanding grayscale/RGB sources to RGBA or copying an owned RGBA image.
fn grayscale_on_white(source: DynamicImage) -> GrayImage {
    let (width, height) = (source.width(), source.height());
    let luma = |r: u8, g: u8, b: u8| {
        (u32::from(r) * 299 + u32::from(g) * 587 + u32::from(b) * 114 + 500) / 1000
    };
    let composite = |gray: u32, alpha: u8| {
        let alpha = u32::from(alpha);
        ((gray * alpha + 255 * (255 - alpha) + 127) / 255) as u8
    };
    let pixels = match source {
        DynamicImage::ImageLuma8(gray) => return gray,
        DynamicImage::ImageLumaA8(gray) => gray
            .as_raw()
            .as_chunks::<2>()
            .0
            .iter()
            .map(|p| composite(u32::from(p[0]), p[1]))
            .collect(),
        DynamicImage::ImageRgb8(rgb) => rgb
            .as_raw()
            .as_chunks::<3>()
            .0
            .iter()
            .map(|p| luma(p[0], p[1], p[2]) as u8)
            .collect(),
        DynamicImage::ImageRgba8(rgba) => rgba
            .as_raw()
            .as_chunks::<4>()
            .0
            .iter()
            .map(|p| composite(luma(p[0], p[1], p[2]), p[3]))
            .collect(),
        source => return grayscale_on_white(DynamicImage::ImageRgba8(source.into_rgba8())),
    };
    GrayImage::from_raw(width, height, pixels).expect("one grayscale sample per source pixel")
}

fn resize_gray(
    resizer: &mut Resizer,
    source: &GrayImage,
    width: u32,
    height: u32,
    filter: FilterType,
) -> Result<GrayImage> {
    let mut resized = GrayImage::new(width, height);
    // Operate directly on one-byte grayscale buffers with runtime SIMD dispatch.
    // imageops::resize uses a four-channel f32 intermediate even for grayscale.
    resizer
        .resize(
            source,
            &mut resized,
            &ResizeOptions::new().resize_alg(ResizeAlg::Convolution(filter)),
        )
        .map_err(|error| Error::Invalid(format!("Could not resize page: {error}")))?;
    Ok(resized)
}

fn preview_gray(
    resizer: &mut Resizer,
    page: &GrayImage,
    options: DocumentOptions,
) -> Result<GrayImage> {
    let height = (500.0 * options.paper.height_inches() / (WIDTH as f64 / 204.0)).round() as u32;
    resize_gray(resizer, page, 500, height, FilterType::Bilinear)
}

fn rasterize(
    source: DynamicImage,
    options: DocumentOptions,
    resizer: &mut Resizer,
) -> Result<GrayImage> {
    let ydpi = options.resolution.vertical_dpi();
    let height = (options.paper.height_inches() * ydpi as f64).round() as u32;
    let gray = grayscale_on_white(source);
    let margin_x = 41;
    let margin_y = ydpi / 5;
    let max_width = WIDTH - 2 * margin_x;
    let max_height = height - 2 * margin_y;
    let scale = (max_width as f64 / gray.width() as f64)
        .min(max_height as f64 * 204.0 / ydpi as f64 / gray.height() as f64);
    let width = (gray.width() as f64 * scale).round().max(1.0) as u32;
    let image_height = (gray.height() as f64 * scale * ydpi as f64 / 204.0)
        .round()
        .max(1.0) as u32;
    let resized = resize_gray(resizer, &gray, width, image_height, FilterType::Lanczos3)?;
    let mut page = GrayImage::from_pixel(WIDTH, height, Luma([255]));
    imageops::replace(
        &mut page,
        &resized,
        ((WIDTH - width) / 2) as i64,
        ((height - image_height) / 2) as i64,
    );
    Ok(page)
}

fn binarize(page: &mut GrayImage, options: DocumentOptions) {
    let exponent = 2.0_f64.powf(f64::from(options.contrast.clamp(-100, 100)) / 50.0);
    let tones: [u8; 256] = std::array::from_fn(|value| {
        // A symmetric contrast curve keeps pure white (including margins and
        // transparent backgrounds) white, and pure black black at every setting.
        let adjusted = if options.contrast == 0 {
            value as u8
        } else {
            let level = value as f64 / 255.0;
            let mapped = if level < 0.5 {
                0.5 * (2.0 * level).powf(exponent)
            } else {
                1.0 - 0.5 * (2.0 * (1.0 - level)).powf(exponent)
            };
            (mapped * 255.0).round() as u8
        };
        match options.binarization {
            Binarization::Text => {
                if adjusted < 180 {
                    0
                } else {
                    255
                }
            }
            Binarization::Photo => adjusted,
        }
    });
    if options.binarization == Binarization::Text || options.contrast != 0 {
        page.as_mut()
            .iter_mut()
            .for_each(|value| *value = tones[usize::from(*value)]);
    }
    if options.binarization == Binarization::Photo {
        imageops::dither(page, &imageops::BiLevel);
    }
}

fn write_page<W: Write + std::io::Seek>(
    encoder: &mut TiffEncoder<W>,
    image: &GrayImage,
    ydpi: u32,
) -> Result<()> {
    let mut packed = vec![0_u8; image.width().div_ceil(8) as usize * image.height() as usize];
    let stride = image.width().div_ceil(8) as usize;
    for (x, y, pixel) in image.enumerate_pixels() {
        if pixel.0[0] == 0 {
            packed[y as usize * stride + x as usize / 8] |= 0x80 >> (x % 8);
        }
    }
    let mut directory = encoder.image_directory()?;
    let offset = u32::try_from(directory.write_data(packed.as_slice())?)
        .map_err(|_| Error::Invalid("Prepared TIFF exceeds 4 GiB".into()))?;
    directory.write_tag(Tag::ImageWidth, image.width())?;
    directory.write_tag(Tag::ImageLength, image.height())?;
    directory.write_tag(Tag::BitsPerSample, 1_u16)?;
    directory.write_tag(Tag::Compression, 1_u16)?;
    directory.write_tag(Tag::PhotometricInterpretation, 0_u16)?;
    directory.write_tag(Tag::FillOrder, 1_u16)?;
    directory.write_tag(Tag::SamplesPerPixel, 1_u16)?;
    directory.write_tag(Tag::RowsPerStrip, image.height())?;
    directory.write_tag(Tag::StripOffsets, offset)?;
    directory.write_tag(Tag::StripByteCounts, packed.len() as u32)?;
    directory.write_tag(Tag::XResolution, Rational { n: 204, d: 1 })?;
    directory.write_tag(Tag::YResolution, Rational { n: ydpi, d: 1 })?;
    directory.write_tag(Tag::ResolutionUnit, 2_u16)?;
    directory.finish()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tiff::decoder::{Decoder, DecodingResult};

    #[test]
    fn contrast_preview_matches_fax_and_edits_preserve_the_original() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let source = directory.path().join("gradient.png");
        GrayImage::from_fn(256, 180, |x, _| Luma([x as u8])).save(&source)?;
        let documents = Documents::new(directory.path().join("spool"))?;
        let original = documents.prepare(
            DocumentInput {
                paths: vec![source.clone()],
                options: DocumentOptions::default(),
            },
            &Cancellation::default(),
            |_| {},
        )?;
        let original_tiff = fs::read(documents.fax_path(original.id))?;
        let preview = documents.preview_source(original.id, 1)?;
        fs::remove_file(source)?;
        let mut text_previews = Vec::new();
        for binarization in Binarization::ALL {
            for contrast in [-100, 0, 100] {
                let options = DocumentOptions {
                    binarization,
                    contrast,
                    ..original.options
                };
                let live = preview.render(binarization, contrast)?;
                let adjusted =
                    documents.adjust(original.id, options, &Cancellation::default(), |_| {})?;
                assert_ne!(original.id, adjusted.id);
                assert_eq!(adjusted.source_names, original.source_names);
                assert_eq!(documents.load(adjusted.id)?.options, options);
                let stored_preview =
                    image::open(documents.preview_path(adjusted.id, 1))?.into_rgba8();
                assert_eq!((live.width, live.height), stored_preview.dimensions());
                assert_eq!(live.pixels, stored_preview.into_raw());
                if binarization == Binarization::Text {
                    text_previews.push(live.pixels);
                }
                let mut expected = preview.gray.clone();
                binarize(&mut expected, options);
                assert!(expected.rows().next().unwrap().all(|p| p[0] == 255));
                let mut decoder = Decoder::new(File::open(documents.fax_path(adjusted.id))?)?;
                let DecodingResult::U8(pixels) = decoder.read_image()? else {
                    panic!("bilevel TIFF")
                };
                assert_eq!(
                    pixels.len(),
                    WIDTH as usize / 8 * expected.height() as usize
                );
                for (index, &value) in expected.as_raw().iter().enumerate() {
                    let decoded = if pixels[index / 8] & (0x80 >> (index % 8)) == 0 {
                        0
                    } else {
                        255
                    };
                    assert_eq!(
                        decoded, value,
                        "{binarization:?}, contrast {contrast}, pixel {index}"
                    );
                }
            }
        }
        assert_ne!(text_previews[0], text_previews[1]);
        assert_ne!(text_previews[1], text_previews[2]);
        assert_eq!(fs::read(documents.fax_path(original.id))?, original_tiff);
        assert_eq!(documents.load(original.id)?.options.contrast, 0);
        let cancellation = Cancellation::default();
        cancellation.cancel();
        assert!(matches!(
            documents.adjust(original.id, original.options, &cancellation, |_| {}),
            Err(Error::Cancelled)
        ));
        assert!(documents.preview_source(original.id, 0).is_err());
        assert!(documents.preview_source(original.id, 2).is_err());
        Ok(())
    }

    #[test]
    fn grayscale_preserves_luminance_and_transparency_for_source_formats() {
        let rgba = DynamicImage::ImageRgba8(image::RgbaImage::from_fn(256, 16, |x, y| {
            image::Rgba([x as u8, (x * 7 + y) as u8, (x + y * 13) as u8, x as u8])
        }));
        let sources = [
            rgba.clone(),
            DynamicImage::ImageRgb8(rgba.to_rgb8()),
            DynamicImage::ImageLuma8(rgba.to_luma8()),
            DynamicImage::ImageLumaA8(rgba.to_luma_alpha8()),
            DynamicImage::ImageRgb16(rgba.to_rgb16()),
            DynamicImage::ImageRgba16(rgba.to_rgba16()),
            DynamicImage::ImageLuma16(rgba.to_luma16()),
            DynamicImage::ImageLumaA16(rgba.to_luma_alpha16()),
        ];
        for source in sources {
            let color = source.color();
            let rgba = source.to_rgba8();
            let expected = GrayImage::from_fn(rgba.width(), rgba.height(), |x, y| {
                let [r, g, b, a] = rgba.get_pixel(x, y).0.map(u32::from);
                let luma = (r * 299 + g * 587 + b * 114 + 500) / 1000;
                Luma([((luma * a + 255 * (255 - a) + 127) / 255) as u8])
            });
            assert_eq!(grayscale_on_white(source), expected, "{color:?}");
        }
    }

    #[test]
    fn rasterized_pages_preserve_geometry_margins_and_bilevel_pixels() -> Result<()> {
        let mut resizer = Resizer::new();
        for paper in crate::PaperSize::ALL {
            for resolution in crate::Resolution::ALL {
                for binarization in Binarization::ALL {
                    let options = DocumentOptions {
                        paper,
                        resolution,
                        binarization,
                        ..DocumentOptions::default()
                    };
                    let mut page = rasterize(
                        DynamicImage::ImageLuma8(GrayImage::from_pixel(60, 100, Luma([0]))),
                        options,
                        &mut resizer,
                    )?;
                    binarize(&mut page, options);
                    let ydpi = resolution.vertical_dpi();
                    let height = (paper.height_inches() * f64::from(ydpi)).round() as u32;
                    assert_eq!(page.dimensions(), (WIDTH, height));
                    assert!(page.as_raw().iter().all(|&p| p == 0 || p == 255));
                    assert_eq!(page.get_pixel(WIDTH / 2, height / 2)[0], 0);
                    let (left, right, top, bottom) =
                        page.enumerate_pixels().filter(|(_, _, p)| p[0] == 0).fold(
                            (WIDTH, 0, height, 0),
                            |(left, right, top, bottom), (x, y, _)| {
                                (left.min(x), right.max(x), top.min(y), bottom.max(y))
                            },
                        );
                    assert!(left >= 41 && WIDTH - right > 41);
                    assert!(top >= ydpi / 5 && height - bottom > ydpi / 5);
                    assert!((left as i32 - (WIDTH - 1 - right) as i32).abs() <= 1);
                    assert!((top as i32 - (height - 1 - bottom) as i32).abs() <= 1);
                    let physical_aspect = f64::from(right - left + 1)
                        / 204.0
                        / (f64::from(bottom - top + 1) / f64::from(ydpi));
                    assert!((physical_aspect - 0.6).abs() < 0.002);
                }
            }
        }
        let transparent = DynamicImage::ImageRgba8(image::RgbaImage::from_pixel(
            32,
            32,
            image::Rgba([0, 0, 0, 0]),
        ));
        let page = rasterize(transparent, DocumentOptions::default(), &mut resizer)?;
        assert!(page.as_raw().iter().all(|&p| p == 255));
        Ok(())
    }

    #[test]
    fn grayscale_resizing_keeps_detail_and_smooth_tones() -> Result<()> {
        let mut resizer = Resizer::new();
        let source = GrayImage::from_fn(320, 240, |x, y| {
            Luma([if (100..104).contains(&x) || (100..104).contains(&y) {
                0
            } else {
                (x * 255 / 319) as u8
            }])
        });
        for (width, height) in [(80, 60), (640, 480), (320, 240), (1, 1)] {
            for (filter, reference_filter) in [
                (FilterType::Lanczos3, imageops::FilterType::Lanczos3),
                (FilterType::Bilinear, imageops::FilterType::Triangle),
            ] {
                let actual = resize_gray(&mut resizer, &source, width, height, filter)?;
                let expected = imageops::resize(&source, width, height, reference_filter);
                // SIMD integer convolution rounds between passes; compare image
                // quality, rather than requiring identical floating-point rounding.
                let squared_error: f64 = actual
                    .as_raw()
                    .iter()
                    .zip(expected.as_raw())
                    .map(|(&a, &b)| (f64::from(a) - f64::from(b)).powi(2))
                    .sum();
                let rmse = (squared_error / f64::from(width * height)).sqrt();
                assert!(rmse < 2.0, "{width}x{height} {filter:?}: RMSE {rmse}");
            }
        }
        Ok(())
    }

    /// Run with `cargo test -p faxe-engine preparation_benchmark -- --ignored --nocapture`.
    #[test]
    #[ignore = "manual page preparation benchmark"]
    fn preparation_benchmark() -> Result<()> {
        let _ = tracing_subscriber::fmt()
            .with_max_level(tracing::Level::DEBUG)
            .with_test_writer()
            .try_init();
        let directory = tempfile::tempdir()?;
        let documents = Documents::new(directory.path().join("spool"))?;
        for (width, height) in [(100, 200), (1920, 1080), (4000, 3000)] {
            let source = directory.path().join(format!("{width}x{height}.png"));
            image::RgbaImage::from_fn(width, height, |x, y| {
                image::Rgba([x as u8, y as u8, (x ^ y) as u8, 255])
            })
            .save(&source)?;
            for binarization in [Binarization::Text, Binarization::Photo] {
                let started = Instant::now();
                let document = documents.prepare(
                    DocumentInput {
                        paths: vec![source.clone()],
                        options: DocumentOptions {
                            binarization,
                            ..DocumentOptions::default()
                        },
                    },
                    &Cancellation::default(),
                    |_| {},
                )?;
                eprintln!("{width}x{height} {binarization:?}: {:?}", started.elapsed());
                assert_eq!(document.pages, 1);
                let preview = documents.preview_source(document.id, 1)?;
                let started = Instant::now();
                for contrast in [-80, -40, 0, 40, 80] {
                    std::hint::black_box(preview.render(binarization, contrast)?);
                }
                eprintln!(
                    "{width}x{height} {binarization:?} live preview average: {:?}",
                    started.elapsed() / 5
                );
            }
        }
        Ok(())
    }

    #[test]
    fn embedded_pdfium_loads_and_rasterizes_text() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let documents = Documents::new(directory.path().join("spool"))?;
        let source = directory.path().join("source.pdf");
        {
            let renderer = documents.pdfium()?;
            let mut document = renderer.create_new_pdf().map_err(pdf_error)?;
            let font = document.fonts_mut().helvetica();
            let mut page = document
                .pages_mut()
                .create_page_at_end(PdfPagePaperSize::a4())
                .map_err(pdf_error)?;
            page.objects_mut()
                .create_text_object(
                    PdfPoints::new(72.0),
                    PdfPoints::new(720.0),
                    "Faxe PDF runtime check",
                    font,
                    PdfPoints::new(24.0),
                )
                .map_err(pdf_error)?;
            drop(page);
            document.save_to_file(&source).map_err(pdf_error)?;
        }
        let document = documents.prepare(
            DocumentInput {
                paths: vec![source],
                options: DocumentOptions::default(),
            },
            &Cancellation::default(),
            |_| {},
        )?;
        let mut decoder = Decoder::new(File::open(documents.fax_path(document.id))?)?;
        let pixels = match decoder.read_image()? {
            DecodingResult::U8(pixels) => pixels,
            _ => panic!("expected bilevel samples"),
        };
        assert!(pixels.contains(&0) && pixels.contains(&255));
        assert_eq!(document.pages, 1);
        Ok(())
    }

    #[test]
    fn spool_is_multipage_bilevel_and_survives_source_removal() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let source = directory.path().join("source.png");
        DynamicImage::ImageLuma8(GrayImage::from_pixel(100, 200, Luma([0]))).save(&source)?;
        let documents = Documents::new(directory.path().join("spool"))?;
        let document = documents.prepare(
            DocumentInput {
                paths: vec![source.clone(), source.clone()],
                options: DocumentOptions::default(),
            },
            &Cancellation::default(),
            |_| {},
        )?;
        fs::remove_file(source)?;
        let mut decoder = Decoder::new(File::open(documents.fax_path(document.id))?)?;
        for index in 0..2 {
            assert_eq!(decoder.dimensions()?, (1728, 2292));
            assert_eq!(decoder.get_tag_u32(Tag::BitsPerSample)?, 1);
            let pixels = match decoder.read_image()? {
                DecodingResult::U8(pixels) => pixels,
                _ => panic!("expected bilevel samples"),
            };
            assert!(pixels.contains(&0) && pixels.contains(&255));
            assert_eq!(decoder.more_images(), index == 0);
            if decoder.more_images() {
                decoder.next_image()?;
            }
        }
        assert_eq!(documents.load(document.id)?.pages, 2);
        for page in 1..=2 {
            let preview = image::open(documents.preview_path(document.id, page))?;
            assert_eq!(preview.width(), 500);
        }
        assert_eq!(fs::read_dir(directory.path().join("spool"))?.count(), 1);
        Ok(())
    }
}
