use std::{
    fs::{self, File},
    io::{BufWriter, Write},
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

use image::{
    DynamicImage, GrayImage, ImageReader, Luma,
    imageops::{self, FilterType},
};
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

#[derive(Clone, Default)]
pub struct Cancellation {
    flag: Arc<AtomicBool>,
    wake: Arc<std::sync::Mutex<Option<Arc<dyn Fn() + Send + Sync>>>>,
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
        let wake = self.wake.lock().ok().and_then(|w| w.clone());
        if let Some(wake) = wake {
            wake();
        }
    }

    pub fn is_cancelled(&self) -> bool {
        self.flag.load(Ordering::Acquire)
    }

    pub(crate) fn flag(&self) -> Arc<AtomicBool> {
        self.flag.clone()
    }
    pub(crate) fn set_wake(&self, wake: Option<Arc<dyn Fn() + Send + Sync>>) {
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
        mut progress: impl FnMut(u32),
    ) -> Result<PreparedDocument> {
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
        let mut append = |image: DynamicImage| -> Result<()> {
            cancellation.check()?;
            if pages >= MAX_PAGES {
                return Err(Error::Invalid(format!(
                    "A fax may contain at most {MAX_PAGES} pages"
                )));
            }
            let page = rasterize(image, input.options);
            write_page(&mut encoder, &page, input.options.resolution.vertical_dpi())?;
            pages += 1;
            let display_height = (500.0 * input.options.paper.height_inches()
                / (WIDTH as f64 / 204.0))
                .round() as u32;
            imageops::resize(&page, 500, display_height, FilterType::Triangle)
                .save(staging.path().join(format!("page-{pages:04}.png")))?;
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
        writer.flush()?;
        writer.get_ref().sync_all()?;
        cancellation.check()?;
        let document = PreparedDocument {
            id: Uuid::new_v4(),
            pages,
            source_names: input
                .paths
                .iter()
                .map(|path| {
                    path.file_name()
                        .unwrap_or_default()
                        .to_string_lossy()
                        .into_owned()
                })
                .collect(),
            options: input.options,
        };
        let mut metadata = File::create(staging.path().join("document.json"))?;
        serde_json::to_writer_pretty(&mut metadata, &document)?;
        metadata.sync_all()?;
        fs::rename(staging.path(), self.root.join(document.id.to_string()))?;
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

fn rasterize(source: DynamicImage, options: DocumentOptions) -> GrayImage {
    let ydpi = options.resolution.vertical_dpi();
    let height = (options.paper.height_inches() * ydpi as f64).round() as u32;
    let rgba = source.to_rgba8();
    let gray = GrayImage::from_fn(rgba.width(), rgba.height(), |x, y| {
        let [r, g, b, a] = rgba.get_pixel(x, y).0.map(u32::from);
        let luma = (r * 299 + g * 587 + b * 114 + 500) / 1000;
        Luma([((luma * a + 255 * (255 - a) + 127) / 255) as u8])
    });
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
    let resized = imageops::resize(&gray, width, image_height, FilterType::Lanczos3);
    let mut page = GrayImage::from_pixel(WIDTH, height, Luma([255]));
    imageops::replace(
        &mut page,
        &resized,
        ((WIDTH - width) / 2) as i64,
        ((height - image_height) / 2) as i64,
    );
    match options.binarization {
        Binarization::Text => page.pixels_mut().for_each(|pixel| {
            pixel.0[0] = match pixel.0[0] < 180 {
                true => 0,
                false => 255,
            }
        }),
        Binarization::Photo => imageops::dither(&mut page, &imageops::BiLevel),
    }
    page
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
        Ok(())
    }
}
