//! Image previews, with no dependency on the views: decoding a file into a small thumbnail ready
//! for the GPU, and a small least-recently-used cache.
//!
//! gpui's own `img()` decodes at full resolution and keeps every image forever; a 24 MP photo
//! would take about 96 MB. Thumbnails keep the preview cheap no matter how big the file is.

use std::collections::VecDeque;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock, mpsc};
use std::thread;
use std::time::SystemTime;

use futures::channel::oneshot;
use gpui::RenderImage;
use image::metadata::Orientation;
use image::{DynamicImage, Frame, ImageDecoder, ImageReader, RgbaImage};
use resvg::{tiny_skia, usvg};

/// Longest side of a thumbnail, in pixels.
pub const MAX_SIDE: u32 = 1024;
/// Larger files are not previewed.
pub const MAX_FILE_SIZE: u64 = 128 << 20;

/// Formats the `image` crate decodes with the features gpui enables (AVIF needs a decoder that is
/// not included).
const RASTER_EXTENSIONS: &[&str] = &[
    "bmp", "dds", "exr", "farbfeld", "ff", "gif", "hdr", "ico", "jpeg", "jpg", "pam", "pbm", "pgm",
    "png", "pnm", "ppm", "qoi", "tga", "tif", "tiff", "webp",
];

fn extension(name: &str) -> Option<String> {
    Path::new(name)
        .extension()
        .map(|extension| extension.to_string_lossy().to_lowercase())
}

/// Whether a file with this name can be previewed, judging by its extension.
pub fn can_preview(name: &str) -> bool {
    extension(name).is_some_and(|extension| {
        extension == "svg" || RASTER_EXTENSIONS.contains(&extension.as_str())
    })
}

/// What identifies a preview: a file that changes gets a new key.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Key {
    pub path: PathBuf,
    pub len: u64,
    pub modified: Option<SystemTime>,
}

pub struct Thumbnail {
    pub image: Arc<RenderImage>,
    /// Size of the original image in pixels; for an SVG, its declared size.
    pub width: u32,
    pub height: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Failure {
    /// Larger than `MAX_FILE_SIZE`.
    TooLarge,
    /// Not readable or not a valid image.
    Unreadable,
}

/// Decodes previews one at a time on a thread of its own.
///
/// A single thread reuses the same memory for every image. With a pool of threads, each one
/// keeps its own allocator arena and the process keeps growing as different threads decode.
#[derive(Clone)]
pub struct Loader {
    requests: mpsc::Sender<Request>,
}

struct Request {
    path: PathBuf,
    reply: oneshot::Sender<Result<Thumbnail, Failure>>,
}

impl Loader {
    pub fn new() -> io::Result<Self> {
        let (requests, received) = mpsc::channel::<Request>();
        thread::Builder::new()
            .name("exporer-preview".to_owned())
            .spawn(move || {
                for request in received {
                    // Nobody waits for it anymore: the cursor moved on.
                    if request.reply.is_canceled() {
                        continue;
                    }
                    request.reply.send(load(&request.path)).ok();
                }
            })?;
        Ok(Self { requests })
    }

    /// Queues `path`. Dropping the receiver before its turn skips the work. If the thread is
    /// gone, the receiver is cancelled.
    pub fn load(&self, path: PathBuf) -> oneshot::Receiver<Result<Thumbnail, Failure>> {
        let (reply, receiver) = oneshot::channel();
        self.requests.send(Request { path, reply }).ok();
        receiver
    }
}

/// Reads and decodes `path` into a thumbnail. Blocks: meant for a thread of its own.
pub fn load(path: &Path) -> Result<Thumbnail, Failure> {
    let metadata = fs::metadata(path).map_err(|_| Failure::Unreadable)?;
    if metadata.len() > MAX_FILE_SIZE {
        return Err(Failure::TooLarge);
    }
    let name = path.file_name().unwrap_or_default().to_string_lossy();
    let (pixels, width, height) = if extension(&name).as_deref() == Some("svg") {
        render_svg(path)?
    } else {
        decode_raster(path)?
    };
    Ok(Thumbnail {
        image: Arc::new(render_image(pixels)),
        width,
        height,
    })
}

/// Decodes the first frame, turned upright according to its EXIF orientation, and shrinks it to
/// fit `MAX_SIDE`. Returns the thumbnail and the original size.
fn decode_raster(path: &Path) -> Result<(RgbaImage, u32, u32), Failure> {
    // The format comes from the content; the extension is the fallback (TGA has no signature).
    let reader = ImageReader::open(path)
        .and_then(ImageReader::with_guessed_format)
        .map_err(|_| Failure::Unreadable)?;
    // The reader's default limits refuse images that would take too much memory.
    let mut decoder = reader.into_decoder().map_err(|_| Failure::Unreadable)?;
    let orientation = decoder.orientation().unwrap_or(Orientation::NoTransforms);
    let mut image = DynamicImage::from_decoder(decoder).map_err(|_| Failure::Unreadable)?;
    image.apply_orientation(orientation);
    let (width, height) = (image.width(), image.height());
    if width > MAX_SIDE || height > MAX_SIDE {
        image = image.thumbnail(MAX_SIDE, MAX_SIDE);
    }
    Ok((image.into_rgba8(), width, height))
}

/// Renders an SVG so its longest side is `MAX_SIDE`. Returns the pixels and the declared size.
fn render_svg(path: &Path) -> Result<(RgbaImage, u32, u32), Failure> {
    let data = fs::read(path).map_err(|_| Failure::Unreadable)?;
    let options = usvg::Options {
        fontdb: system_fonts(),
        ..usvg::Options::default()
    };
    let tree = usvg::Tree::from_data(&data, &options).map_err(|_| Failure::Unreadable)?;
    let size = tree.size();
    let (width, height) = (size.width(), size.height());
    if !(width > 0. && height > 0.) {
        return Err(Failure::Unreadable);
    }
    let scale = MAX_SIDE as f32 / width.max(height);
    let pixel_width = ((width * scale).round() as u32).max(1);
    let pixel_height = ((height * scale).round() as u32).max(1);
    let mut pixmap =
        tiny_skia::Pixmap::new(pixel_width, pixel_height).ok_or(Failure::Unreadable)?;
    resvg::render(
        &tree,
        tiny_skia::Transform::from_scale(scale, scale),
        &mut pixmap.as_mut(),
    );
    let mut pixels = pixmap.take();
    unpremultiply(&mut pixels);
    let image =
        RgbaImage::from_raw(pixel_width, pixel_height, pixels).ok_or(Failure::Unreadable)?;
    Ok((
        image,
        width.round().max(1.) as u32,
        height.round().max(1.) as u32,
    ))
}

/// System fonts for text inside SVGs, loaded once: loading them takes a while.
fn system_fonts() -> Arc<usvg::fontdb::Database> {
    static FONTS: OnceLock<Arc<usvg::fontdb::Database>> = OnceLock::new();
    FONTS
        .get_or_init(|| {
            let mut fonts = usvg::fontdb::Database::new();
            fonts.load_system_fonts();
            Arc::new(fonts)
        })
        .clone()
}

/// `tiny-skia` gives premultiplied alpha; gpui takes straight alpha.
fn unpremultiply(rgba: &mut [u8]) {
    for pixel in rgba.chunks_exact_mut(4) {
        let alpha = u16::from(pixel[3]);
        if alpha == 0 || alpha == 255 {
            continue;
        }
        for channel in &mut pixel[..3] {
            *channel = ((u16::from(*channel) * 255 + alpha / 2) / alpha).min(255) as u8;
        }
    }
}

/// gpui takes images in BGRA order.
fn render_image(mut pixels: RgbaImage) -> RenderImage {
    for pixel in pixels.chunks_exact_mut(4) {
        pixel.swap(0, 2);
    }
    RenderImage::new(vec![Frame::new(pixels)])
}

/// The last `capacity` values used, by key. Small enough for a linear search.
pub struct Cache<K, V> {
    capacity: usize,
    /// From least to most recently used.
    items: VecDeque<(K, V)>,
}

impl<K: PartialEq, V> Cache<K, V> {
    pub fn new(capacity: usize) -> Self {
        Self {
            capacity,
            items: VecDeque::with_capacity(capacity),
        }
    }

    /// The value for `key`, without marking it as used.
    pub fn peek(&self, key: &K) -> Option<&V> {
        self.items
            .iter()
            .find(|(item, _)| item == key)
            .map(|(_, value)| value)
    }

    /// Marks `key` as the most recently used. Returns whether it is in the cache.
    pub fn touch(&mut self, key: &K) -> bool {
        let Some(index) = self.items.iter().position(|(item, _)| item == key) else {
            return false;
        };
        if let Some(item) = self.items.remove(index) {
            self.items.push_back(item);
        }
        true
    }

    /// Adds or replaces `key` as the most recently used. Returns what had to leave the cache:
    /// the previous value for `key`, or the least recently used item when it was full.
    pub fn insert(&mut self, key: K, value: V) -> Option<V> {
        let removed = match self.items.iter().position(|(item, _)| *item == key) {
            Some(index) => self.items.remove(index),
            None if self.items.len() >= self.capacity => self.items.pop_front(),
            None => None,
        };
        self.items.push_back((key, value));
        removed.map(|(_, value)| value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::{Rgba, RgbaImage};

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("exporer-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn pixel(thumbnail: &Thumbnail, x: u32, y: u32) -> [u8; 4] {
        let size = thumbnail.image.size(0);
        let width = u32::try_from(i32::from(size.width)).unwrap();
        let bytes = thumbnail.image.as_bytes(0).unwrap();
        let start = ((y * width + x) * 4) as usize;
        bytes[start..start + 4].try_into().unwrap()
    }

    fn thumbnail_size(thumbnail: &Thumbnail) -> (i32, i32) {
        let size = thumbnail.image.size(0);
        (size.width.into(), size.height.into())
    }

    #[test]
    fn knows_what_it_can_preview() {
        for name in [
            "a.png", "b.JPG", "c.jpeg", "d.gif", "e.webp", "f.svg", "g.tiff", "h.ico",
        ] {
            assert!(can_preview(name), "{name}");
        }
        for name in ["a.avif", "b.heic", "c.txt", "png", ".png", "d"] {
            assert!(!can_preview(name), "{name}");
        }
    }

    #[test]
    fn small_images_keep_their_size_in_bgra() {
        let dir = temp_dir("preview-small");
        let path = dir.join("red.png");
        RgbaImage::from_pixel(3, 2, Rgba([255, 0, 0, 255]))
            .save(&path)
            .unwrap();
        let thumbnail = load(&path).unwrap();
        fs::remove_dir_all(&dir).unwrap();
        assert_eq!((thumbnail.width, thumbnail.height), (3, 2));
        assert_eq!(thumbnail_size(&thumbnail), (3, 2));
        assert_eq!(pixel(&thumbnail, 0, 0), [0, 0, 255, 255], "red in BGRA");
    }

    #[test]
    fn large_images_shrink_and_keep_their_proportion() {
        let dir = temp_dir("preview-large");
        let path = dir.join("wide.png");
        RgbaImage::from_pixel(3000, 1000, Rgba([0, 128, 0, 255]))
            .save(&path)
            .unwrap();
        let thumbnail = load(&path).unwrap();
        fs::remove_dir_all(&dir).unwrap();
        assert_eq!((thumbnail.width, thumbnail.height), (3000, 1000));
        let (width, height) = thumbnail_size(&thumbnail);
        assert_eq!(width, MAX_SIDE as i32);
        assert!((340..=342).contains(&height), "{height}");
    }

    #[test]
    fn the_format_comes_from_the_content() {
        let dir = temp_dir("preview-content");
        // A PNG saved with a JPEG extension still decodes.
        let path = dir.join("misnamed.jpg");
        RgbaImage::from_pixel(4, 4, Rgba([0, 0, 255, 255]))
            .save_with_format(&path, image::ImageFormat::Png)
            .unwrap();
        let thumbnail = load(&path).unwrap();
        fs::write(dir.join("fake.png"), "not an image").unwrap();
        let fake = load(&dir.join("fake.png"));
        let missing = load(&dir.join("missing.png"));
        fs::remove_dir_all(&dir).unwrap();
        assert_eq!((thumbnail.width, thumbnail.height), (4, 4));
        assert_eq!(fake.err(), Some(Failure::Unreadable));
        assert_eq!(missing.err(), Some(Failure::Unreadable));
    }

    #[test]
    fn renders_svg_at_the_thumbnail_size() {
        let dir = temp_dir("preview-svg");
        let path = dir.join("shape.svg");
        fs::write(
            &path,
            r#"<svg xmlns="http://www.w3.org/2000/svg" width="100" height="50">
                 <rect width="50" height="50" fill="red"/>
                 <rect x="50" width="50" height="50" fill="blue" fill-opacity="0.5"/>
               </svg>"#,
        )
        .unwrap();
        let thumbnail = load(&path).unwrap();
        fs::write(dir.join("broken.svg"), "<svg").unwrap();
        let broken = load(&dir.join("broken.svg"));
        fs::remove_dir_all(&dir).unwrap();
        assert_eq!((thumbnail.width, thumbnail.height), (100, 50));
        assert_eq!(
            thumbnail_size(&thumbnail),
            (MAX_SIDE as i32, MAX_SIDE as i32 / 2)
        );
        assert_eq!(pixel(&thumbnail, 10, 10), [0, 0, 255, 255], "red in BGRA");
        // Half-transparent blue keeps its full color with straight alpha.
        let [blue, green, red, alpha] = pixel(&thumbnail, 900, 10);
        assert_eq!((blue, green, red), (255, 0, 0));
        assert!((126..=129).contains(&alpha), "{alpha}");
        assert_eq!(broken.err(), Some(Failure::Unreadable));
    }

    #[test]
    fn the_loader_answers_and_skips_requests_nobody_waits_for() {
        let dir = temp_dir("preview-loader");
        let path = dir.join("one.png");
        RgbaImage::from_pixel(2, 2, Rgba([1, 2, 3, 255]))
            .save(&path)
            .unwrap();
        let loader = Loader::new().unwrap();
        drop(loader.load(path.clone()));
        let thumbnail = futures::executor::block_on(loader.load(path)).unwrap();
        fs::remove_dir_all(&dir).unwrap();
        assert_eq!(thumbnail.unwrap().width, 2);
    }

    #[test]
    fn unpremultiplies_alpha() {
        let mut pixels = [64, 32, 0, 128, 10, 20, 30, 0, 1, 2, 3, 255];
        unpremultiply(&mut pixels);
        assert_eq!(pixels, [128, 64, 0, 128, 10, 20, 30, 0, 1, 2, 3, 255]);
    }

    #[test]
    fn cache_drops_the_least_recently_used() {
        let mut cache = Cache::new(2);
        assert_eq!(cache.insert("a", 1), None);
        assert_eq!(cache.insert("b", 2), None);
        assert!(cache.touch(&"a"));
        assert_eq!(
            cache.insert("c", 3),
            Some(2),
            "b was the least recently used"
        );
        assert_eq!(cache.peek(&"b"), None);
        assert_eq!(cache.peek(&"a"), Some(&1));
        assert_eq!(
            cache.insert("a", 10),
            Some(1),
            "replacing returns the old value"
        );
        assert_eq!(cache.peek(&"a"), Some(&10));
        assert!(!cache.touch(&"z"));
    }
}
