//! The review thumbnails as egui textures (spec §9.1).
//!
//! The engine writes each file's thumbnail as a small JPEG (256 px on the long side) and
//! the review names its path. After start-up the app reads no file on the UI thread, so
//! [`ThumbCache::get`] only looks in memory: a thumbnail not there yet is asked of the
//! loader, one thread that reads and decodes the JPEGs into pixels, and
//! [`ThumbCache::poll`] turns the pixels into textures at the next frame. Views ask only
//! for the thumbnails they show.
//!
//! Everything is bounded, so a job of thousands of photos scrolled end to end cannot fill
//! the memory:
//! - at most [`TEXTURES`] textures are held, the least recently used going first;
//! - at most [`QUEUE`] requests wait for the loader, which serves the newest first: what
//!   was asked last is what is on screen now. When the queue is full the oldest request
//!   is forgotten, and asked again if its card is still visible;
//! - at most [`DECODED`] images are decoded and not yet textures (the one being decoded
//!   included): the loader waits for [`ThumbCache::poll`] to take some before it decodes
//!   more, so slow frames that each show many new cards cannot pile pixels up;
//! - a thumbnail that cannot be read is remembered as missing, for as many paths as
//!   there are textures, until its job is left ([`ThumbCache::forget_job`]);
//! - at most [`UPLOADS_PER_FRAME`] textures are created in one frame, so a burst of
//!   finished images cannot stall a frame; the rest wait for the next ones.

use std::collections::{HashMap, VecDeque};
use std::fs;
use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::thread::{self, JoinHandle};

use eframe::egui::{self, ColorImage, TextureHandle, TextureOptions};
use image::ImageFormat;
use lru::LruCache;
use photostacker_core::guard;

/// The name of the loader's thread.
pub const THUMBS_THREAD: &str = "photostacker-ui-thumbs";

/// The most textures held: more than a window shows at once, or the visible ones would be
/// evicted and loaded again for ever. A 4K window can show about 900 (narrow photos, the
/// tray at its largest; DESIGN.md, Thumbnails). At most about 180 MB of 256 px thumbnails
/// of 3:2 photos, and narrower ones take less.
pub const TEXTURES: usize = 1024;

/// The most requests waiting for the loader.
pub const QUEUE: usize = 256;

/// The most images decoded, or being decoded, and not yet textures: two frames' uploads,
/// about 16 MB at most.
pub const DECODED: usize = 64;

/// The most textures created in one frame.
pub const UPLOADS_PER_FRAME: usize = 32;

/// What a view draws for a thumbnail.
pub enum Thumb<'a> {
    /// The texture, valid for this frame.
    Ready(&'a TextureHandle),
    /// Asked for, not decoded yet.
    Loading,
    /// The file has no thumbnail, or it cannot be read: a placeholder.
    Missing,
}

/// A thumbnail asked of the loader. Its ticket tells a request from an earlier one of the
/// same path that was forgotten meanwhile.
struct Request {
    path: PathBuf,
    ticket: usize,
}

/// What the loader made of a [`Request`]: the pixels, or `None` when the file cannot be
/// read or decoded.
struct Loaded {
    path: PathBuf,
    ticket: usize,
    image: Option<ColorImage>,
}

/// The requests waiting for the loader, and how many it may still take.
#[derive(Default)]
struct Queue {
    /// Oldest first; the loader takes from the back.
    waiting: VecDeque<Request>,
    /// The requests the loader took whose images the cache has not let go of yet: being
    /// decoded, in [`Shared::loaded`], or in the cache's `to_upload`. The loader takes no
    /// more while there are [`DECODED`].
    taken: usize,
    /// The cache is being dropped.
    quit: bool,
}

impl Queue {
    /// Adds `request` as the newest, and returns the oldest when the queue was full.
    fn push(&mut self, request: Request) -> Option<Request> {
        let pushed_out = if self.waiting.len() == QUEUE {
            self.waiting.pop_front()
        } else {
            None
        };
        self.waiting.push_back(request);
        pushed_out
    }
}

/// What the cache and the loader share.
struct Shared {
    queue: Mutex<Queue>,
    /// Signalled when a request is queued, when the cache lets go of decoded images, and
    /// when it quits.
    work: Condvar,
    /// What the loader finished, in order, not taken yet.
    loaded: Mutex<Vec<Loaded>>,
}

/// Locks `mutex`, also when a thread panicked while holding it: every holder leaves the
/// queue and the finished images consistent.
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// The review thumbnails' textures, and the thread that loads them.
pub struct ThumbCache {
    textures: LruCache<PathBuf, TextureHandle>,
    /// The thumbnails that could not be read, most recently asked for last.
    failed: LruCache<PathBuf, ()>,
    /// Each path asked of the loader and not resolved yet, with its request's ticket:
    /// waiting, being decoded, or decoded and waiting for its upload.
    loading: HashMap<PathBuf, usize>,
    /// What the loader finished, oldest first, waiting for its upload.
    to_upload: VecDeque<Loaded>,
    /// The requests handed to the loader since the start; the next one's ticket.
    requested: usize,
    shared: Arc<Shared>,
    /// `None` when the thread could not be started: every thumbnail is then missing.
    loader: Option<JoinHandle<()>>,
}

impl ThumbCache {
    /// Starts the loader thread, [`THUMBS_THREAD`], which repaints `context` after each
    /// image.
    pub fn new(context: &egui::Context) -> ThumbCache {
        let shared = Arc::new(Shared {
            queue: Mutex::default(),
            work: Condvar::new(),
            loaded: Mutex::default(),
        });
        let spawned = thread::Builder::new().name(THUMBS_THREAD.into()).spawn({
            let shared = Arc::clone(&shared);
            let context = context.clone();
            move || load(&shared, &context)
        });
        let loader = spawned
            .inspect_err(|error| tracing::error!("cannot start the thumbnail loader: {error}"))
            .ok();
        let capacity = NonZeroUsize::new(TEXTURES).expect("TEXTURES is not zero");
        ThumbCache {
            textures: LruCache::new(capacity),
            failed: LruCache::new(capacity),
            loading: HashMap::new(),
            to_upload: VecDeque::new(),
            requested: 0,
            shared,
            loader,
        }
    }

    /// The texture of the thumbnail file `path`, asking the loader for it if needed.
    /// `None` is a file with no thumbnail. Looks only in memory.
    pub fn get(&mut self, path: Option<&Path>) -> Thumb<'_> {
        let Some(path) = path else {
            return Thumb::Missing;
        };
        // `contains` first: returning the borrow of a `get` that may miss would hold
        // `self` borrowed for the rest of the function.
        if self.textures.contains(path) {
            return self.textures.get(path).map_or(Thumb::Missing, Thumb::Ready);
        }
        if self.failed.get(path).is_some() || self.loader.is_none() {
            return Thumb::Missing;
        }
        if !self.loading.contains_key(path) {
            self.request(path);
        }
        Thumb::Loading
    }

    /// Hands `path` to the loader, forgetting the oldest request if the queue is full.
    fn request(&mut self, path: &Path) {
        let ticket = self.requested;
        self.requested += 1;
        self.loading.insert(path.to_path_buf(), ticket);
        let request = Request {
            path: path.to_path_buf(),
            ticket,
        };
        let pushed_out = lock(&self.shared.queue).push(request);
        self.shared.work.notify_one();
        if let Some(Request { path, ticket }) = pushed_out
            && self.loading.get(&path) == Some(&ticket)
        {
            self.loading.remove(&path);
        }
    }

    /// Turns what the loader finished into textures, at most [`UPLOADS_PER_FRAME`] of them,
    /// and asks for another frame when some are left. Called once per frame.
    pub fn poll(&mut self, context: &egui::Context) {
        self.take_loaded();
        let max_side = context.input(|input| input.max_texture_side);
        let mut uploads = 0;
        let mut released = 0;
        while uploads < UPLOADS_PER_FRAME
            && let Some(Loaded {
                path,
                ticket,
                image,
            }) = self.to_upload.pop_front()
        {
            released += 1;
            // Of a request forgotten since: its job was left.
            if self.loading.get(&path) != Some(&ticket) {
                continue;
            }
            self.loading.remove(&path);
            match image {
                Some(image) if image.width() <= max_side && image.height() <= max_side => {
                    let name = path.to_string_lossy().into_owned();
                    let texture = context.load_texture(name, image, TextureOptions::LINEAR);
                    self.textures.put(path, texture);
                    uploads += 1;
                }
                _ => {
                    self.failed.put(path, ());
                }
            }
        }
        self.release(released);
        if !self.to_upload.is_empty() {
            context.request_repaint();
        }
    }

    /// Moves what the loader finished to `to_upload`, keeping its order.
    fn take_loaded(&mut self) {
        self.to_upload.extend(lock(&self.shared.loaded).drain(..));
    }

    /// Lets the loader take `count` more requests: the cache let go of that many images.
    fn release(&self, count: usize) {
        if count > 0 {
            lock(&self.shared.queue).taken -= count;
            self.shared.work.notify_one();
        }
    }

    /// Drops every texture, failure, decoded image and waiting request of the thumbnails
    /// in `thumbs_dir`, a job's folder of them: called when the app leaves that job. A
    /// path comparison only; nothing is looked at.
    pub fn forget_job(&mut self, thumbs_dir: &Path) {
        let outside = |path: &PathBuf| !path.starts_with(thumbs_dir);
        self.textures.retain(|path, _| outside(path));
        self.failed.retain(|path, _| outside(path));
        // An image of the job still being decoded is dropped once it is done.
        self.loading.retain(|path, _| outside(path));
        // The waiting requests first: the loader, once released, must not take one.
        lock(&self.shared.queue)
            .waiting
            .retain(|request| outside(&request.path));
        self.take_loaded();
        let before = self.to_upload.len();
        self.to_upload.retain(|loaded| outside(&loaded.path));
        self.release(before - self.to_upload.len());
    }

    /// The requests handed to the loader since the start; a path asked for again after it
    /// was forgotten counts again. For tests.
    pub fn requested(&self) -> usize {
        self.requested
    }

    /// The textures held now.
    pub fn textures(&self) -> usize {
        self.textures.len()
    }

    /// The images decoded, or being decoded, and not yet textures nor let go of: at most
    /// [`DECODED`]. For tests.
    pub fn decoded(&self) -> usize {
        lock(&self.shared.queue).taken
    }
}

impl Drop for ThumbCache {
    /// Stops the loader after the image it is decoding, or while it waits for work or
    /// for decoded images to be taken, and joins it.
    fn drop(&mut self) {
        lock(&self.shared.queue).quit = true;
        self.shared.work.notify_all();
        if let Some(loader) = self.loader.take()
            && loader.join().is_err()
        {
            tracing::error!("the thumbnail loader panicked");
        }
    }
}

/// The loader's loop: decodes the newest request, hands the image over and repaints,
/// until the cache quits.
fn load(shared: &Shared, context: &egui::Context) {
    while let Some(Request { path, ticket }) = next_request(shared) {
        let image = decode(&path)
            .inspect_err(|error| tracing::debug!(?path, "no thumbnail: {error}"))
            .ok();
        lock(&shared.loaded).push(Loaded {
            path,
            ticket,
            image,
        });
        context.request_repaint();
    }
}

/// Waits for a request, and for fewer than [`DECODED`] images to be held, then takes the
/// newest; `None` once the cache quits.
fn next_request(shared: &Shared) -> Option<Request> {
    let mut queue = lock(&shared.queue);
    loop {
        if queue.quit {
            return None;
        }
        if queue.taken < DECODED
            && let Some(request) = queue.waiting.pop_back()
        {
            queue.taken += 1;
            return Some(request);
        }
        queue = shared
            .work
            .wait(queue)
            .unwrap_or_else(PoisonError::into_inner);
    }
}

/// Reads and decodes the JPEG at `path`. A decoder's panic is a failure of this file.
fn decode(path: &Path) -> Result<ColorImage, String> {
    let bytes = fs::read(path).map_err(|error| error.to_string())?;
    guard::catch(|| {
        let image = image::load_from_memory_with_format(&bytes, ImageFormat::Jpeg)
            .map_err(|error| error.to_string())?
            .to_rgb8();
        let size = [image.width() as usize, image.height() as usize];
        Ok(ColorImage::from_rgb(size, image.as_raw()))
    })?
}
