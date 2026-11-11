//! The verified, resumable download of a model and its import from a file
//! (spec §8.2). Only the `ml` feature builds it.
//!
//! A download writes `<models_dir>/<id>/model.onnx.part`, which a later
//! download resumes; an import copies to `model.onnx.import`, so it never
//! touches a download's part. Either file is checked against the registry's
//! SHA-256, synced and renamed to `model.onnx`: a file whose hash differs is
//! never installed.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use sha2::{Digest, Sha256};

use super::lock::{ModelLock, try_lock};
use super::{ModelError, ModelSpec, UrlFailure, model_file};
use crate::fsio::{sync_dir, sync_dir_or_warn, with_suffix};

/// The bytes read and written at a time; a cancel is seen between chunks.
const CHUNK: usize = 64 * 1024;

/// The longest wait for a connection to a download URL.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);

/// The longest wait for a response's headers. The body has no time limit:
/// a slow link may take many minutes over a large model.
const RESPONSE_TIMEOUT: Duration = Duration::from_secs(60);

/// The longest wait for any one read of a response, its headers or its
/// body: a connection that sends nothing for this long counts as dropped
/// ([`fetch`]), so a stalled download ends, and a cancel is seen, within it.
const READ_IDLE_TIMEOUT: Duration = Duration::from_secs(15);

/// The `User-Agent` of every download request.
const USER_AGENT: &str = concat!("photostacker/", env!("CARGO_PKG_VERSION"));

/// Downloads the model `spec` into `models_dir` and returns the installed
/// [`model_file`].
///
/// - A `.part` left by an earlier download is resumed. One that is already
///   complete is verified without a request; one that is complete but
///   wrong, or longer than the model, is deleted first.
/// - Each URL is tried in order until one gives the registry's file
///   ([`fetch`]). A part whose SHA-256 differs is deleted, so the next URL
///   starts from zero.
/// - `progress(done, total)` follows every chunk written, in bytes of the
///   whole file. `cancel` is checked before each request and between
///   chunks: a cancel returns [`ModelError::Cancelled`] and keeps the part.
///   No read waits longer than [`READ_IDLE_TIMEOUT`]: a server that stops
///   sending holds a cancel back for that long at most.
/// - When every URL fails, [`ModelError::AllUrlsFailed`] gives each one's
///   reason and nothing is installed (a part kept from a dropped connection
///   is resumed next time). A local file error ends the download at once.
/// - The model's lock is held throughout: while another download, import or
///   removal of the model runs, this one returns [`ModelError::Busy`] and
///   touches nothing.
///
/// The requests go through the proxy the environment names (`ALL_PROXY`,
/// `HTTPS_PROXY`, `HTTP_PROXY`, minus `NO_PROXY`), as ureq reads them.
pub fn download(
    spec: &ModelSpec,
    models_dir: &Path,
    progress: &dyn Fn(u64, u64),
    cancel: &AtomicBool,
) -> Result<PathBuf, ModelError> {
    let agent: ureq::Agent = agent_config().build().into();
    download_with(&agent, spec, models_dir, progress, cancel)
}

/// [`download`] through `agent`: the tests' agent ignores the environment's
/// proxy, so they reach their local server whatever the machine sets.
fn download_with(
    agent: &ureq::Agent,
    spec: &ModelSpec,
    models_dir: &Path,
    progress: &dyn Fn(u64, u64),
    cancel: &AtomicBool,
) -> Result<PathBuf, ModelError> {
    let target = model_file(models_dir, spec);
    let part = with_suffix(&target, ".part");
    let _lock = lock(spec, folder_of(&target))?;
    if earlier_part_is_complete(spec, &part)? {
        return install(&part, &target);
    }
    let mut failures = Vec::new();
    for url in spec.urls {
        match fetch(agent, url, spec, &part, progress, cancel) {
            Ok(()) => {}
            Err(Fetch::Failed(reason)) => {
                failures.push(failure(url, reason));
                continue;
            }
            Err(Fetch::Cancelled) => return Err(ModelError::Cancelled),
            Err(Fetch::Local(error)) => return Err(error.into()),
        }
        match mismatch(spec, &part)? {
            None => return install(&part, &target),
            Some(reason) => {
                fs::remove_file(&part)?;
                failures.push(failure(url, reason));
            }
        }
    }
    Err(ModelError::AllUrlsFailed(failures))
}

/// The address `url`, failed for `reason`.
fn failure(url: &str, reason: String) -> UrlFailure {
    UrlFailure {
        url: url.to_owned(),
        reason,
    }
}

/// Imports the model `spec` from `file`, for a machine with no network, and
/// returns the installed [`model_file`]. `file` itself is only read.
///
/// - A file whose size is not the registry's is refused at once with
///   [`ModelError::WrongSize`], before anything is locked or copied.
/// - Otherwise it is copied to `<id>/model.onnx.import`, a name of the
///   import's own, hashing what is written: a download's `.part` is never
///   touched, so a cancelled download can still be resumed. A copy whose
///   SHA-256 is not the registry's is refused with
///   [`ModelError::HashMismatch`]. The copy is removed on every exit but the
///   install, which renames it.
/// - The model's lock is held as by [`download`].
pub fn import(spec: &ModelSpec, file: &Path, models_dir: &Path) -> Result<PathBuf, ModelError> {
    let target = model_file(models_dir, spec);
    let copy = with_suffix(&target, ".import");
    let source = File::open(file)?;
    let size = source.metadata()?.len();
    if size != spec.size {
        return Err(ModelError::WrongSize {
            path: file.to_path_buf(),
            name: spec.name,
            size,
            expected: spec.size,
        });
    }
    let _lock = lock(spec, folder_of(&target))?;
    let _removed = RemoveOnDrop(&copy);
    let actual = hash_copy(source, File::create(&copy)?)?;
    if actual != spec.sha256 {
        return Err(ModelError::HashMismatch {
            path: file.to_path_buf(),
            id: spec.id,
            expected: spec.sha256,
            actual,
        });
    }
    install(&copy, &target)
}

/// Removes the file at its path when dropped, unless it is gone already (an
/// import's copy, once installed, has been renamed).
struct RemoveOnDrop<'a>(&'a Path);

impl Drop for RemoveOnDrop<'_> {
    fn drop(&mut self) {
        match fs::remove_file(self.0) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => {
                tracing::warn!(path = %self.0.display(), %error, "cannot remove an import's copy");
            }
        }
    }
}

/// The settings of the downloads' HTTP client. The proxy is ureq's
/// default: the environment's.
fn agent_config() -> ureq::config::ConfigBuilder<ureq::typestate::AgentScope> {
    ureq::Agent::config_builder()
        .timeout_connect(Some(CONNECT_TIMEOUT))
        .timeout_recv_response(Some(RESPONSE_TIMEOUT))
        .timeout_per_read(Some(READ_IDLE_TIMEOUT))
}

/// Takes the lock of the model folder `model_dir`, creating the folder.
fn lock(spec: &ModelSpec, model_dir: &Path) -> Result<ModelLock, ModelError> {
    try_lock(model_dir)?.ok_or(ModelError::Busy(spec.id))
}

/// Why one URL gave no complete body.
enum Fetch {
    /// This URL failed; the next one may not.
    Failed(String),
    Cancelled,
    /// The part could not be written: no URL would do better.
    Local(io::Error),
}

/// Downloads the rest of `part` from `url`: from the part's length with
/// `Range: bytes=<len>-`, appended on a 206, or from zero on a 200 (a
/// server that ignores the range). Any other status (416 included) or a
/// failed request fails the URL. A connection that drops while the body is
/// read, or sends nothing for [`READ_IDLE_TIMEOUT`], is retried once, from
/// the new length.
///
/// `Ok` means the body was read to its end or to the registry's size,
/// whichever comes first; the hash is checked next.
fn fetch(
    agent: &ureq::Agent,
    url: &str,
    spec: &ModelSpec,
    part: &Path,
    progress: &dyn Fn(u64, u64),
    cancel: &AtomicBool,
) -> Result<(), Fetch> {
    let mut retried = false;
    loop {
        let offset = part_len(part).map_err(Fetch::Local)?;
        if offset >= spec.size {
            // The dropped connection had sent every byte.
            return Ok(());
        }
        if cancel.load(Ordering::Relaxed) {
            return Err(Fetch::Cancelled);
        }
        let mut request = agent
            .get(url)
            .header("User-Agent", USER_AGENT)
            // A compressed body would not be the bytes the range counts.
            .header("Accept-Encoding", "identity");
        if offset > 0 {
            request = request.header("Range", format!("bytes={offset}-"));
        }
        let response = request.call().map_err(|error| {
            Fetch::Failed(match error {
                ureq::Error::StatusCode(status) => format!("HTTP status {status}"),
                other => other.to_string(),
            })
        })?;
        let (file, done) = match response.status().as_u16() {
            206 => (
                OpenOptions::new().create(true).append(true).open(part),
                offset,
            ),
            200 => (File::create(part), 0),
            status => {
                return Err(Fetch::Failed(format!("unexpected HTTP status {status}")));
            }
        };
        let file = file.map_err(Fetch::Local)?;
        let body = response.into_body().into_reader();
        match copy_body(body, file, done, spec.size, progress, cancel) {
            Ok(()) => return Ok(()),
            Err(Copy::Dropped(error)) if !retried => {
                tracing::warn!(url, %error, "the download's connection dropped; resuming it");
                retried = true;
            }
            Err(Copy::Dropped(error)) => {
                return Err(Fetch::Failed(format!(
                    "the connection dropped again: {error}"
                )));
            }
            Err(Copy::Cancelled) => return Err(Fetch::Cancelled),
            Err(Copy::Local(error)) => return Err(Fetch::Local(error)),
        }
    }
}

/// Why a body was not copied to its end.
enum Copy {
    /// Reading the body failed: the connection dropped.
    Dropped(io::Error),
    Cancelled,
    /// Writing the part failed.
    Local(io::Error),
}

/// Copies `body` into `file`, which holds `done` bytes of the model, until
/// the body ends or the file reaches `size` bytes: a longer body is not the
/// model, and its first `size` bytes fail the hash.
fn copy_body(
    mut body: impl Read,
    mut file: File,
    mut done: u64,
    size: u64,
    progress: &dyn Fn(u64, u64),
    cancel: &AtomicBool,
) -> Result<(), Copy> {
    let mut buffer = vec![0; CHUNK];
    while done < size {
        if cancel.load(Ordering::Relaxed) {
            return Err(Copy::Cancelled);
        }
        let read = match body.read(&mut buffer) {
            Ok(0) => break,
            Ok(read) => read,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(Copy::Dropped(error)),
        };
        // `done < size`, so at least one byte is kept.
        let kept = (read as u64).min(size - done) as usize;
        file.write_all(&buffer[..kept]).map_err(Copy::Local)?;
        done += kept as u64;
        progress(done, size);
    }
    Ok(())
}

/// Settles the part an earlier download left: `true` when it is complete
/// and is the registry's file. A complete part that is not, or one longer
/// than the model, is deleted; a shorter one is kept to be resumed.
fn earlier_part_is_complete(spec: &ModelSpec, part: &Path) -> io::Result<bool> {
    if part_len(part)? < spec.size {
        return Ok(false);
    }
    if mismatch(spec, part)?.is_none() {
        return Ok(true);
    }
    fs::remove_file(part)?;
    Ok(false)
}

/// Why the file at `path` is not the model `spec`, or `None` when its size
/// and SHA-256 are the registry's.
fn mismatch(spec: &ModelSpec, path: &Path) -> io::Result<Option<String>> {
    let len = fs::metadata(path)?.len();
    if len != spec.size {
        return Ok(Some(format!(
            "got {len} bytes, the registry's size is {}",
            spec.size
        )));
    }
    let actual = hash_copy(File::open(path)?, io::sink())?;
    Ok((actual != spec.sha256).then(|| {
        format!(
            "SHA-256 {actual} differs from the registry's {}",
            spec.sha256
        )
    }))
}

/// The length of `part`, 0 when there is none.
fn part_len(part: &Path) -> io::Result<u64> {
    match fs::metadata(part) {
        Ok(metadata) => Ok(metadata.len()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(0),
        Err(error) => Err(error),
    }
}

/// Reads `reader` to its end, writing every byte to `sink`, and returns the
/// SHA-256 of those bytes in lowercase hex.
pub(super) fn hash_copy(mut reader: impl Read, mut sink: impl Write) -> io::Result<String> {
    let mut hasher = Sha256::new();
    let mut buffer = vec![0; CHUNK];
    loop {
        let read = match reader.read(&mut buffer) {
            Ok(0) => break,
            Ok(read) => read,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error),
        };
        hasher.update(&buffer[..read]);
        sink.write_all(&buffer[..read])?;
    }
    Ok(hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect())
}

/// Syncs the verified `part` and renames it to `target`, replacing an
/// earlier install, then syncs the folder (a failed folder sync is logged).
fn install(part: &Path, target: &Path) -> Result<PathBuf, ModelError> {
    // Opened for writing: Windows flushes only a handle that may write.
    OpenOptions::new().write(true).open(part)?.sync_all()?;
    fs::rename(part, target)?;
    sync_dir_or_warn(folder_of(target), sync_dir);
    Ok(target.to_path_buf())
}

/// The model's folder, `<models_dir>/<id>`.
fn folder_of(target: &Path) -> &Path {
    target
        .parent()
        .expect("a model file is inside its model's folder")
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::sync::atomic::Ordering;

    use photostacker_testkit::http::{Behaviour, Server, serve};
    use sha2::{Digest, Sha256};

    use super::*;
    use crate::models::{installed, model_file};

    /// A body of several download chunks whose bytes differ, so a byte
    /// out of place changes the hash.
    fn body() -> Vec<u8> {
        (0..300_000u32).map(|i| (i * 31 % 251) as u8).collect()
    }

    /// A registry entry for `body`, downloaded from `urls` in order. The
    /// registry's fields are `'static`, so its strings are leaked: tests only.
    fn spec_for(body: &[u8], urls: &[&Server]) -> &'static ModelSpec {
        let sha256 = Sha256::digest(body)
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        let urls: Vec<&'static str> = urls
            .iter()
            .map(|server| &*Box::leak(server.url().to_string().into_boxed_str()))
            .collect();
        Box::leak(Box::new(ModelSpec {
            id: "test-model",
            name: "Test model",
            licence: "MIT",
            size: body.len() as u64,
            sha256: Box::leak(sha256.into_boxed_str()),
            urls: Box::leak(urls.into_boxed_slice()),
            dims: 3,
            default_cosine: 0.9,
        }))
    }

    fn part_file(models_dir: &Path, spec: &ModelSpec) -> PathBuf {
        models_dir.join(spec.id).join("model.onnx.part")
    }

    fn never_cancel() -> AtomicBool {
        AtomicBool::new(false)
    }

    /// The production client's settings without the environment's proxy, so
    /// a machine's `HTTP_PROXY` cannot send the requests away from the
    /// local server.
    fn local_agent() -> ureq::Agent {
        agent_config().proxy(None).build().into()
    }

    fn download_local(
        spec: &ModelSpec,
        models_dir: &Path,
        progress: &dyn Fn(u64, u64),
        cancel: &AtomicBool,
    ) -> Result<PathBuf, ModelError> {
        download_with(&local_agent(), spec, models_dir, progress, cancel)
    }

    #[test]
    fn the_tests_client_ignores_the_environment_proxy() {
        let agent = local_agent();
        assert!(agent.config().proxy().is_none());
        // Otherwise it is the production client.
        let production = agent_config().build();
        let timeouts = |config: &ureq::config::Config| {
            (
                config.timeouts().connect,
                config.timeouts().recv_response,
                config.timeout_per_read(),
            )
        };
        assert_eq!(timeouts(agent.config()), timeouts(&production));
        assert_eq!(
            timeouts(&production),
            (
                Some(CONNECT_TIMEOUT),
                Some(RESPONSE_TIMEOUT),
                Some(READ_IDLE_TIMEOUT)
            )
        );
    }

    #[test]
    fn a_stalled_download_fails_within_the_idle_timeout() {
        let body = body();
        let half = body.len() / 2;
        let server = serve(body.clone(), Behaviour::StallAt(half));
        let spec = spec_for(&body, &[&server]);
        let models_dir = tempfile::tempdir().unwrap();
        // The production client with an idle timeout short enough for a test.
        let agent: ureq::Agent = agent_config()
            .proxy(None)
            .timeout_per_read(Some(Duration::from_millis(300)))
            .build()
            .into();

        let started = std::time::Instant::now();
        let error = download_with(&agent, spec, models_dir.path(), &|_, _| {}, &never_cancel())
            .unwrap_err();

        assert!(started.elapsed() < Duration::from_secs(5), "{error:?}");
        // A stalled read is a dropped connection: resumed once, then failed.
        let ModelError::AllUrlsFailed(reasons) = error else {
            panic!("expected AllUrlsFailed, got {error:?}");
        };
        assert_eq!(reasons.len(), 1);
        assert!(reasons[0].reason.contains("dropped again"), "{reasons:?}");
        let ranges: Vec<_> = server
            .requests()
            .iter()
            .map(|request| request.header("range").map(str::to_string))
            .collect();
        assert_eq!(ranges, [None, Some(format!("bytes={half}-"))]);
        // The part is kept for the next download.
        assert_eq!(
            fs::read(part_file(models_dir.path(), spec)).unwrap(),
            &body[..half]
        );
        assert!(!installed(models_dir.path(), spec));
    }

    #[test]
    fn a_busy_model_is_left_alone() {
        let body = body();
        let server = serve(body.clone(), Behaviour::Normal);
        let spec = spec_for(&body, &[&server]);
        let models_dir = tempfile::tempdir().unwrap();
        let part = part_file(models_dir.path(), spec);
        fs::create_dir_all(part.parent().unwrap()).unwrap();
        fs::write(&part, &body[..1000]).unwrap();
        let source_dir = tempfile::tempdir().unwrap();
        let source = source_dir.path().join("model.onnx");
        fs::write(&source, &body).unwrap();
        // Another download or import holds the model.
        let held = try_lock(part.parent().unwrap()).unwrap().unwrap();

        let downloaded = download_local(spec, models_dir.path(), &|_, _| {}, &never_cancel());
        let imported = import(spec, &source, models_dir.path());

        assert!(
            matches!(downloaded, Err(ModelError::Busy("test-model"))),
            "{downloaded:?}"
        );
        assert!(
            matches!(imported, Err(ModelError::Busy("test-model"))),
            "{imported:?}"
        );
        assert_eq!(fs::read(&part).unwrap(), &body[..1000]);
        assert!(server.requests().is_empty());
        assert!(!installed(models_dir.path(), spec));
        // Released, the model installs.
        drop(held);
        download_local(spec, models_dir.path(), &|_, _| {}, &never_cancel()).unwrap();
        assert!(installed(models_dir.path(), spec));
        assert_eq!(server.requests()[0].header("range"), Some("bytes=1000-"));
    }

    #[test]
    fn download_installs_a_verified_file() {
        let body = body();
        let server = serve(body.clone(), Behaviour::Normal);
        let spec = spec_for(&body, &[&server]);
        let models_dir = tempfile::tempdir().unwrap();
        let reports = std::cell::RefCell::new(Vec::new());

        let path = download_local(
            spec,
            models_dir.path(),
            &|done, total| reports.borrow_mut().push((done, total)),
            &never_cancel(),
        )
        .unwrap();

        assert_eq!(path, model_file(models_dir.path(), spec));
        assert_eq!(fs::read(&path).unwrap(), body);
        assert!(installed(models_dir.path(), spec));
        assert!(!part_file(models_dir.path(), spec).exists());
        let reports = reports.into_inner();
        assert_eq!(reports.last(), Some(&(spec.size, spec.size)));
        assert!(reports.windows(2).all(|pair| pair[0].0 < pair[1].0));
        let requests = server.requests();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].header("range"), None);
        let user_agent = format!("photostacker/{}", env!("CARGO_PKG_VERSION"));
        assert_eq!(requests[0].header("user-agent"), Some(user_agent.as_str()));
    }

    #[test]
    fn interrupted_download_resumes_with_range() {
        let body = body();
        let server = serve(body.clone(), Behaviour::DropAfter(100_000));
        let spec = spec_for(&body, &[&server]);
        let models_dir = tempfile::tempdir().unwrap();

        let path = download_local(spec, models_dir.path(), &|_, _| {}, &never_cancel()).unwrap();

        assert_eq!(fs::read(path).unwrap(), body);
        let ranges: Vec<_> = server
            .requests()
            .iter()
            .map(|request| request.header("range").map(str::to_string))
            .collect();
        assert_eq!(ranges, [None, Some("bytes=100000-".to_string())]);
    }

    #[test]
    fn server_ignoring_range_restarts_from_zero() {
        let body = body();
        let server = serve(body.clone(), Behaviour::IgnoreRange);
        let spec = spec_for(&body, &[&server]);
        let models_dir = tempfile::tempdir().unwrap();
        // An unfinished part whose bytes are not the body's.
        let part = part_file(models_dir.path(), spec);
        fs::create_dir_all(part.parent().unwrap()).unwrap();
        fs::write(&part, vec![0xAA; 1000]).unwrap();

        let path = download_local(spec, models_dir.path(), &|_, _| {}, &never_cancel()).unwrap();

        assert_eq!(fs::read(path).unwrap(), body);
        assert_eq!(server.requests()[0].header("range"), Some("bytes=1000-"));
    }

    #[test]
    fn a_complete_part_is_verified_without_a_request() {
        let body = body();
        let server = serve(body.clone(), Behaviour::Normal);
        let spec = spec_for(&body, &[&server]);
        let models_dir = tempfile::tempdir().unwrap();
        let part = part_file(models_dir.path(), spec);
        fs::create_dir_all(part.parent().unwrap()).unwrap();
        fs::write(&part, &body).unwrap();

        let path = download_local(spec, models_dir.path(), &|_, _| {}, &never_cancel()).unwrap();

        assert_eq!(fs::read(path).unwrap(), body);
        assert!(!part.exists());
        assert!(server.requests().is_empty());
    }

    #[test]
    fn hash_mismatch_tries_the_next_url() {
        let body = body();
        let wrong = serve(body.clone(), Behaviour::WrongBytes);
        let good = serve(body.clone(), Behaviour::Normal);
        let spec = spec_for(&body, &[&wrong, &good]);
        let models_dir = tempfile::tempdir().unwrap();

        let path = download_local(spec, models_dir.path(), &|_, _| {}, &never_cancel()).unwrap();

        assert_eq!(fs::read(path).unwrap(), body);
        assert_eq!(wrong.requests().len(), 1);
        // The mismatched part was deleted: the next URL starts from zero.
        let requests = good.requests();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].header("range"), None);
    }

    #[test]
    fn no_url_matches_leaves_nothing_installed() {
        let body = body();
        let wrong = serve(body.clone(), Behaviour::WrongBytes);
        let missing = serve(body.clone(), Behaviour::NotFound);
        let spec = spec_for(&body, &[&wrong, &missing]);
        let models_dir = tempfile::tempdir().unwrap();

        let error =
            download_local(spec, models_dir.path(), &|_, _| {}, &never_cancel()).unwrap_err();

        let ModelError::AllUrlsFailed(reasons) = error else {
            panic!("expected AllUrlsFailed, got {error:?}");
        };
        assert_eq!(reasons.len(), 2);
        assert_eq!(reasons[0].url, wrong.url(), "{reasons:?}");
        assert!(reasons[0].reason.contains("SHA-256"), "{reasons:?}");
        assert_eq!(reasons[1].url, missing.url(), "{reasons:?}");
        assert!(reasons[1].reason.contains("404"), "{reasons:?}");
        assert!(!model_file(models_dir.path(), spec).exists());
        assert!(!part_file(models_dir.path(), spec).exists());
        assert!(!installed(models_dir.path(), spec));
    }

    #[test]
    fn cancel_keeps_the_part_for_later() {
        let body = body();
        let server = serve(body.clone(), Behaviour::Normal);
        let spec = spec_for(&body, &[&server]);
        let models_dir = tempfile::tempdir().unwrap();
        let cancel = never_cancel();

        // Cancelled once the first chunk is written.
        let error = download_local(
            spec,
            models_dir.path(),
            &|done, _| {
                if done > 0 {
                    cancel.store(true, Ordering::Relaxed);
                }
            },
            &cancel,
        )
        .unwrap_err();

        assert!(matches!(error, ModelError::Cancelled), "{error:?}");
        assert!(!model_file(models_dir.path(), spec).exists());
        let kept = fs::metadata(part_file(models_dir.path(), spec))
            .unwrap()
            .len();
        assert!(kept > 0 && kept < spec.size, "{kept}");

        // The next download goes on from the part.
        let path = download_local(spec, models_dir.path(), &|_, _| {}, &never_cancel()).unwrap();
        assert_eq!(fs::read(path).unwrap(), body);
        let requests = server.requests();
        assert_eq!(requests.len(), 2);
        let range = format!("bytes={kept}-");
        assert_eq!(requests[1].header("range"), Some(range.as_str()));
    }

    /// The files in the model's folder, by name, sorted.
    fn folder_files(models_dir: &Path, spec: &ModelSpec) -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(models_dir.join(spec.id))
            .map(|entries| {
                entries
                    .map(|entry| entry.unwrap().file_name().into_string().unwrap())
                    .collect()
            })
            .unwrap_or_default();
        names.sort();
        names
    }

    /// The part an earlier, cancelled download of `spec` left in `models_dir`.
    fn earlier_part(models_dir: &Path, spec: &ModelSpec) -> Vec<u8> {
        let part = part_file(models_dir, spec);
        fs::create_dir_all(part.parent().unwrap()).unwrap();
        let kept: Vec<u8> = body()[..CHUNK + 17].to_vec();
        fs::write(&part, &kept).unwrap();
        kept
    }

    #[test]
    fn import_refuses_a_wrong_file() {
        let body = body();
        let spec = spec_for(&body, &[]);
        let source_dir = tempfile::tempdir().unwrap();
        let source = source_dir.path().join("downloaded.onnx");
        let mut wrong = body.clone();
        wrong[1234] ^= 1;
        fs::write(&source, &wrong).unwrap();
        let models_dir = tempfile::tempdir().unwrap();
        let part = earlier_part(models_dir.path(), spec);

        let error = import(spec, &source, models_dir.path()).unwrap_err();

        assert!(
            matches!(&error, ModelError::HashMismatch { path, .. } if path == &source),
            "{error:?}"
        );
        assert!(error.to_string().contains("does not match test-model"));
        assert!(!installed(models_dir.path(), spec));
        assert!(!model_file(models_dir.path(), spec).exists());
        // Its copy is gone, and the download's part is as it was.
        assert_eq!(
            folder_files(models_dir.path(), spec),
            [".lock", "model.onnx.part"]
        );
        assert_eq!(fs::read(part_file(models_dir.path(), spec)).unwrap(), part);
        assert_eq!(fs::read(&source).unwrap(), wrong);
    }

    #[test]
    fn import_refuses_a_file_of_another_size_without_copying_it() {
        let body = body();
        let spec = spec_for(&body, &[]);
        let source_dir = tempfile::tempdir().unwrap();
        let source = source_dir.path().join("photo.jpg");
        fs::write(&source, &body[..1000]).unwrap();
        let models_dir = tempfile::tempdir().unwrap();
        let part = earlier_part(models_dir.path(), spec);
        let before = folder_files(models_dir.path(), spec);

        let error = import(spec, &source, models_dir.path()).unwrap_err();

        assert!(
            matches!(
                &error,
                ModelError::WrongSize { path, name: "Test model", size: 1000, expected }
                    if path == &source && *expected == body.len() as u64
            ),
            "{error:?}"
        );
        // Nothing was copied, nor locked: the folder holds what it held.
        assert_eq!(folder_files(models_dir.path(), spec), before);
        assert_eq!(fs::read(part_file(models_dir.path(), spec)).unwrap(), part);
    }

    #[test]
    fn a_wrong_size_reads_in_megabytes() {
        let error = ModelError::WrongSize {
            path: PathBuf::from("photo.jpg"),
            name: "DINOv2 small",
            size: 3 << 20,
            expected: 88_459_888,
        };
        assert_eq!(
            error.to_string(),
            "this file is 3 MB, DINOv2 small is 84 MB"
        );
    }

    #[test]
    fn import_copies_and_keeps_the_source() {
        let body = body();
        let spec = spec_for(&body, &[]);
        let source_dir = tempfile::tempdir().unwrap();
        let source = source_dir.path().join("model.onnx");
        fs::write(&source, &body).unwrap();
        let models_dir = tempfile::tempdir().unwrap();

        let path = import(spec, &source, models_dir.path()).unwrap();

        assert_eq!(path, model_file(models_dir.path(), spec));
        assert_eq!(fs::read(&path).unwrap(), body);
        assert!(installed(models_dir.path(), spec));
        assert!(!part_file(models_dir.path(), spec).exists());
        assert_eq!(
            folder_files(models_dir.path(), spec),
            [".lock", "model.onnx"]
        );
        assert_eq!(fs::read(&source).unwrap(), body);
    }
}
