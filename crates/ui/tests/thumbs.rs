//! The review thumbnails as egui textures (spec §9.1): decoded off the UI thread, held in
//! a bounded cache, asked for newest first.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use eframe::egui::{self, RawInput};
use image::{Rgb, RgbImage};
use photostacker_ui::thumbs::{DECODED, TEXTURES, Thumb, ThumbCache};
use tempfile::TempDir;

/// Writes a `width` × `height` JPEG named `<name>.jpg` into `dir`, creating it if needed.
fn jpeg(dir: &Path, name: &str, width: u32, height: u32) -> PathBuf {
    std::fs::create_dir_all(dir).unwrap();
    let path = dir.join(format!("{name}.jpg"));
    RgbImage::from_pixel(width, height, Rgb([200, 120, 40]))
        .save(&path)
        .unwrap();
    path
}

/// `count` small JPEGs in `dir`, named by their index.
fn jpegs(dir: &Path, count: usize) -> Vec<PathBuf> {
    (0..count)
        .map(|n| jpeg(dir, &n.to_string(), 8, 8))
        .collect()
}

/// Draws one frame as the app does: the cache takes in what the loader finished, then
/// `views` ask for thumbnails.
fn frame(context: &egui::Context, cache: &mut ThumbCache, mut views: impl FnMut(&mut ThumbCache)) {
    context
        .run_ui(RawInput::default(), |ui| {
            cache.poll(ui.ctx());
            views(cache);
        })
        .drop_without_applying_deltas();
}

/// Draws frames until `done` holds after one; fails after a minute.
fn wait_until(
    context: &egui::Context,
    cache: &mut ThumbCache,
    what: &str,
    mut done: impl FnMut(&mut ThumbCache) -> bool,
) {
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        frame(context, cache, |_| {});
        if done(cache) {
            return;
        }
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(1));
    }
}

fn is_ready(cache: &mut ThumbCache, path: &Path) -> bool {
    matches!(cache.get(Some(path)), Thumb::Ready(_))
}

fn is_loading(cache: &mut ThumbCache, path: &Path) -> bool {
    matches!(cache.get(Some(path)), Thumb::Loading)
}

fn is_missing(cache: &mut ThumbCache, path: Option<&Path>) -> bool {
    matches!(cache.get(path), Thumb::Missing)
}

/// Asks for each of `paths`, then draws frames until every one is ready.
fn load_all(context: &egui::Context, cache: &mut ThumbCache, paths: &[PathBuf]) {
    for path in paths {
        cache.get(Some(path));
    }
    wait_until(context, cache, "the thumbnails", |cache| {
        paths.iter().all(|path| is_ready(cache, path))
    });
}

fn setup() -> (TempDir, egui::Context, ThumbCache) {
    let folder = tempfile::tempdir().unwrap();
    let context = egui::Context::default();
    let cache = ThumbCache::new(&context);
    (folder, context, cache)
}

#[test]
fn a_thumbnail_is_loading_then_ready() {
    let (folder, context, mut cache) = setup();
    let job_1 = folder.path().join("1");
    let job_10 = folder.path().join("10");
    let first = jpeg(&job_1, "first", 40, 30);
    let other = jpeg(&job_10, "other", 8, 8);

    assert!(is_loading(&mut cache, &first));
    assert_eq!((cache.requested(), cache.textures()), (1, 0));
    wait_until(&context, &mut cache, "the thumbnail", |cache| {
        is_ready(cache, &first)
    });
    match cache.get(Some(&first)) {
        Thumb::Ready(texture) => assert_eq!(texture.size(), [40, 30]),
        _ => panic!("the thumbnail is no longer ready"),
    }
    // Asking again for a texture held asks the loader for nothing.
    assert_eq!((cache.requested(), cache.textures()), (1, 1));

    load_all(&context, &mut cache, std::slice::from_ref(&other));
    assert_eq!((cache.requested(), cache.textures()), (2, 2));
    // Leaving job 1 drops its textures and keeps job 10's: "10" begins with "1" as text,
    // not as a folder.
    cache.forget_job(&job_1);
    assert_eq!(cache.textures(), 1);
    assert!(is_ready(&mut cache, &other));
    assert!(is_loading(&mut cache, &first));
    assert_eq!(cache.requested(), 3);
}

#[test]
fn a_missing_thumbnail_is_a_placeholder() {
    let (folder, context, mut cache) = setup();
    let job = folder.path().join("1");
    std::fs::create_dir_all(&job).unwrap();
    let gone = job.join("gone.jpg");
    let garbage = job.join("garbage.jpg");
    std::fs::write(&garbage, b"not a JPEG at all").unwrap();

    // A file with no thumbnail asks for nothing.
    assert!(is_missing(&mut cache, None));
    assert_eq!(cache.requested(), 0);

    assert!(is_loading(&mut cache, &gone));
    assert!(is_loading(&mut cache, &garbage));
    wait_until(&context, &mut cache, "the failures", |cache| {
        is_missing(cache, Some(&gone)) && is_missing(cache, Some(&garbage))
    });
    // Asked for once: a failure is remembered.
    for _ in 0..3 {
        frame(&context, &mut cache, |cache| {
            assert!(is_missing(cache, Some(&gone)));
            assert!(is_missing(cache, Some(&garbage)));
        });
    }
    assert_eq!((cache.requested(), cache.textures()), (2, 0));

    // Until the cache is cleared for its job.
    cache.forget_job(&job);
    assert!(is_loading(&mut cache, &gone));
    assert_eq!(cache.requested(), 3);
}

#[test]
fn the_least_recently_used_texture_goes_first() {
    let (folder, context, mut cache) = setup();
    let paths = jpegs(folder.path(), TEXTURES + 10);
    // Ten at a time, each ten uploaded after the ones before, until the cache is full.
    for chunk in paths[..TEXTURES].chunks(10) {
        load_all(&context, &mut cache, chunk);
    }
    assert_eq!(cache.textures(), TEXTURES);
    // Using the first ten again makes the next ten the least recently used.
    for path in &paths[..10] {
        assert!(is_ready(&mut cache, path));
    }
    load_all(&context, &mut cache, &paths[TEXTURES..]);
    assert_eq!(cache.textures(), TEXTURES);
    assert_eq!(cache.requested(), TEXTURES + 10);
    for path in &paths[..10] {
        assert!(is_ready(&mut cache, path), "{path:?} was dropped");
    }
    assert!(is_ready(&mut cache, &paths[20]));

    // The next ten were dropped instead: each is asked for again, and loads again.
    for path in &paths[10..20] {
        assert!(is_loading(&mut cache, path));
    }
    assert_eq!(cache.requested(), TEXTURES + 20);
    load_all(&context, &mut cache, &paths[10..20]);
    assert_eq!(cache.textures(), TEXTURES);
}

/// A cache whose loader has decoded all it may while no frame took its images: more
/// thumbnails than [`DECODED`] are asked for, and `poll` is never called. Also returns
/// the thumbnails, and how many images the loader finished (its repaint requests).
fn held_back() -> (
    TempDir,
    egui::Context,
    ThumbCache,
    Vec<PathBuf>,
    Arc<AtomicUsize>,
) {
    let (folder, context, mut cache) = setup();
    let finished = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&finished);
    context.set_repaint_observer(move |_| {
        counter.fetch_add(1, Ordering::SeqCst);
    });
    let paths = jpegs(folder.path(), DECODED + 40);
    for path in &paths {
        assert!(is_loading(&mut cache, path));
    }
    let deadline = Instant::now() + Duration::from_secs(60);
    while finished.load(Ordering::SeqCst) < DECODED {
        assert!(cache.decoded() <= DECODED);
        assert!(
            Instant::now() < deadline,
            "the loader never filled its share"
        );
        std::thread::sleep(Duration::from_millis(1));
    }
    (folder, context, cache, paths, finished)
}

#[test]
fn the_decoded_thumbnails_waiting_for_upload_are_bounded() {
    let (_folder, context, mut cache, paths, finished) = held_back();
    // The loader must now wait: give it time to go too far if it would.
    let quiet = Instant::now() + Duration::from_millis(200);
    while Instant::now() < quiet {
        assert_eq!(cache.decoded(), DECODED);
        std::thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(finished.load(Ordering::SeqCst), DECODED);

    // Frames take the images, and the loader goes on with the rest.
    wait_until(&context, &mut cache, "every thumbnail", |cache| {
        paths.iter().all(|path| is_ready(cache, path))
    });
    assert_eq!(cache.textures(), paths.len());
    assert_eq!(cache.decoded(), 0);
    assert_eq!(cache.requested(), paths.len());
}

#[test]
fn dropping_the_cache_stops_a_loader_held_back_by_its_decoded_thumbnails() {
    let (_folder, _context, cache, _paths, _finished) = held_back();
    let (dropped, done) = mpsc::channel();
    std::thread::spawn(move || {
        drop(cache);
        dropped.send(()).unwrap();
    });
    done.recv_timeout(Duration::from_secs(10))
        .expect("dropping the cache did not stop its loader");
}

#[test]
fn forgetting_a_job_lets_go_of_its_decoded_thumbnails() {
    let (folder, context, mut cache, paths, _finished) = held_back();
    cache.forget_job(folder.path());
    assert_eq!(
        cache.decoded(),
        0,
        "the forgotten images still hold the loader"
    );
    // The rest of the job's requests were forgotten too: nothing more is decoded.
    frame(&context, &mut cache, |_| {});
    assert_eq!(cache.textures(), 0);
    assert!(is_loading(&mut cache, &paths[0]));
}

/// The loader is held reading a named pipe while the 1,000 requests are made, so that
/// they all wait together whatever the threads' timing: opening the pipe's writing end
/// waits until the loader has opened it, and the loader reads until that end is closed.
#[cfg(unix)]
#[test]
fn a_full_queue_serves_the_newest_requests() {
    use std::fs::OpenOptions;
    use std::process::Command;

    use photostacker_ui::thumbs::{QUEUE, UPLOADS_PER_FRAME};

    const ASKED: usize = 1_000;
    let (folder, context, mut cache) = setup();
    let paths = jpegs(folder.path(), ASKED);
    let pipe = folder.path().join("pipe.jpg");
    let made = Command::new("mkfifo").arg(&pipe).status().unwrap();
    assert!(made.success(), "mkfifo failed");

    assert!(is_loading(&mut cache, &pipe));
    let writer = OpenOptions::new().write(true).open(&pipe).unwrap();
    frame(&context, &mut cache, |cache| {
        for path in &paths {
            assert!(is_loading(cache, path));
        }
    });
    assert_eq!(cache.requested(), ASKED + 1);
    // An empty file: the pipe is not a JPEG.
    drop(writer);

    // The newest QUEUE requests were kept. Each is ready no later than every one asked
    // before it, and no more than UPLOADS_PER_FRAME become textures in one frame.
    let kept = ASKED - QUEUE;
    let mut ready_at: Vec<Option<usize>> = vec![None; ASKED];
    let mut frames = 0;
    wait_until(&context, &mut cache, "the newest requests", |cache| {
        frames += 1;
        for (index, path) in paths.iter().enumerate().skip(kept) {
            if ready_at[index].is_none() && is_ready(cache, path) {
                ready_at[index] = Some(frames);
            }
        }
        ready_at[kept..].iter().all(Option::is_some)
    });
    let ready_at: Vec<usize> = ready_at[kept..].iter().map(|at| at.unwrap()).collect();
    assert!(
        ready_at.windows(2).all(|pair| pair[0] >= pair[1]),
        "the newest were not served first: {ready_at:?}"
    );
    for frame in 1..=frames {
        let uploaded = ready_at.iter().filter(|&&at| at == frame).count();
        assert!(
            uploaded <= UPLOADS_PER_FRAME,
            "{uploaded} uploads in frame {frame}"
        );
    }
    assert_eq!(cache.textures(), QUEUE);
    wait_until(&context, &mut cache, "the pipe's failure", |cache| {
        is_missing(cache, Some(&pipe))
    });
    assert_eq!(cache.requested(), ASKED + 1);

    // The older ones were pushed out and forgotten: asking again asks the loader again,
    // once for each.
    frame(&context, &mut cache, |cache| {
        for path in &paths[..kept] {
            assert!(is_loading(cache, path));
        }
    });
    assert_eq!(cache.requested(), ASKED + 1 + kept);
}
