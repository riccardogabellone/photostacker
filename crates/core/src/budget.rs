//! A byte budget shared by concurrent decodes (spec §4.3): a counting
//! semaphore over bytes instead of slots.
//!
//! Each decode first takes a [`Permit`] for the memory it may peak at, and
//! gives it back when the permit drops. Requests are served in the order they
//! arrive, so a large one is never overtaken forever by smaller ones that keep
//! fitting; a request larger than the whole budget is clamped to it, so it
//! waits for every other permit to drop and then runs alone.
//!
//! A thread must never wait for a permit while it holds one, or while work
//! that holds one is below it on its stack: that work could not go on, and
//! the permits it holds would never come back. See "Concurrency and memory"
//! in `analyze.rs` for how the analysis keeps to that.

use std::fs::File;
use std::io::{self, Read};
use std::path::Path;
use std::sync::{Condvar, Mutex, MutexGuard, PoisonError};

/// The memory the concurrent decodes of one analysis may use together. It
/// is also the longest file read whole into memory ([`read_whole_file`]).
pub const DECODE_MEMORY_BUDGET: u64 = 2 << 30;

/// Why [`read_whole_file`] gave no bytes.
#[derive(Debug)]
pub(crate) enum WholeFileError {
    /// The file is longer than [`DECODE_MEMORY_BUDGET`]; nothing was read.
    TooLarge { len: u64 },
    /// The file could not be opened, sized or read, or its buffer could not
    /// be allocated (`OutOfMemory`).
    Io {
        what: &'static str,
        source: io::Error,
    },
}

/// The whole file at `path`, read into memory, for the readers that need
/// all of it (the EXIF of a TIFF-family file, the RAW preview fallback,
/// rawler's source). No file is ever memory-mapped instead: one truncated or
/// removed while mapped kills the process (spec §4.3).
///
/// A file longer than [`DECODE_MEMORY_BUDGET`] is refused before anything
/// is allocated, and the buffer is reserved fallibly, as in `fs::read`. The
/// bytes are a copy, so a file truncated afterwards cannot fault their
/// reader. A file that shrinks while it is read gives fewer bytes; one that
/// grows gives the length it had when it was opened.
pub(crate) fn read_whole_file(path: &Path) -> Result<Vec<u8>, WholeFileError> {
    fn io_error(what: &'static str) -> impl FnOnce(io::Error) -> WholeFileError {
        move |source| WholeFileError::Io { what, source }
    }
    let file = File::open(path).map_err(io_error("opening the file"))?;
    let len = file
        .metadata()
        .map_err(io_error("reading the file size"))?
        .len();
    if len > DECODE_MEMORY_BUDGET {
        return Err(WholeFileError::TooLarge { len });
    }
    let mut data = Vec::new();
    // At most the budget, 2 GiB, which fits a `usize` even on 32 bits.
    data.try_reserve_exact(len as usize)
        .map_err(|e| io::Error::new(io::ErrorKind::OutOfMemory, e))
        .map_err(io_error("allocating memory for the file"))?;
    file.take(len)
        .read_to_end(&mut data)
        .map_err(io_error("reading the file"))?;
    Ok(data)
}

/// A pool of `total` bytes that decodes take [`Permit`]s from.
#[derive(Debug)]
pub struct MemoryBudget {
    total: u64,
    state: Mutex<State>,
    /// Signalled when a permit drops or a request is served, so the next
    /// request in line can look again.
    changed: Condvar,
}

#[derive(Debug, Default)]
struct State {
    /// The bytes of the permits held now.
    in_use: u64,
    /// The ticket the next request gets.
    next_ticket: u64,
    /// The ticket of the request at the head of the line.
    serving: u64,
}

/// A share of a [`MemoryBudget`], given back when it drops.
#[derive(Debug)]
#[must_use = "the bytes are given back as soon as the permit drops"]
pub struct Permit<'a> {
    budget: &'a MemoryBudget,
    bytes: u64,
}

impl MemoryBudget {
    pub fn new(total: u64) -> Self {
        Self {
            total,
            state: Mutex::new(State::default()),
            changed: Condvar::new(),
        }
    }

    /// The bytes the budget holds in all.
    pub fn total(&self) -> u64 {
        self.total
    }

    /// Blocks until `bytes`, clamped to [`total`](Self::total), fit beside
    /// the permits already held, and every earlier request has been served;
    /// then takes them.
    pub fn acquire(&self, bytes: u64) -> Permit<'_> {
        let bytes = bytes.min(self.total);
        let mut state = self.lock();
        let ticket = state.next_ticket;
        state.next_ticket += 1;
        while state.serving != ticket || state.in_use + bytes > self.total {
            state = self
                .changed
                .wait(state)
                .unwrap_or_else(PoisonError::into_inner);
        }
        state.in_use += bytes;
        state.serving += 1;
        drop(state);
        // The next request in line may fit too.
        self.changed.notify_all();
        Permit {
            budget: self,
            bytes,
        }
    }

    /// The state holds plain counters that every update leaves consistent,
    /// so a poisoned lock is still good; and a permit dropped while a panic
    /// unwinds must not panic again.
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    #[cfg(test)]
    fn waiting(&self) -> u64 {
        let state = self.lock();
        state.next_ticket - state.serving
    }
}

impl Permit<'_> {
    /// The bytes this permit holds: those asked for, clamped to the total.
    pub fn bytes(&self) -> u64 {
        self.bytes
    }
}

impl Drop for Permit<'_> {
    fn drop(&mut self) {
        self.budget.lock().in_use -= self.bytes;
        self.budget.changed.notify_all();
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
    use std::thread;
    use std::time::Duration;

    use super::*;

    /// The longest any test waits for a permit that should come.
    const TIMEOUT: Duration = Duration::from_secs(2);
    /// How long a test waits to see that a permit does not come.
    const STILL_BLOCKED: Duration = Duration::from_millis(100);

    /// Asks `budget` for `bytes` on a new thread, which sends the bytes of
    /// its permit once it has it, then holds it until `release` is sent to.
    fn request(budget: &Arc<MemoryBudget>, bytes: u64) -> (Receiver<u64>, mpsc::Sender<()>) {
        let (granted_tx, granted) = mpsc::channel();
        let (release, release_rx) = mpsc::channel::<()>();
        let budget = Arc::clone(budget);
        thread::spawn(move || {
            let permit = budget.acquire(bytes);
            granted_tx.send(permit.bytes()).unwrap();
            // An error means the test dropped its sender: release too.
            let _ = release_rx.recv();
        });
        (granted, release)
    }

    fn assert_blocked(granted: &Receiver<u64>) {
        assert_eq!(
            granted.recv_timeout(STILL_BLOCKED),
            Err(RecvTimeoutError::Timeout),
            "the permit came while it should not fit"
        );
    }

    /// Waits until `count` requests are in line behind the one being served.
    fn wait_for_waiting(budget: &MemoryBudget, count: u64) {
        let start = std::time::Instant::now();
        while budget.waiting() < count {
            assert!(start.elapsed() < TIMEOUT, "the requests never queued");
            thread::yield_now();
        }
    }

    #[test]
    fn acquire_waits_for_release() {
        let budget = Arc::new(MemoryBudget::new(100));
        let first = budget.acquire(60);
        let (granted, _release) = request(&budget, 60);
        assert_blocked(&granted);
        drop(first);
        assert_eq!(granted.recv_timeout(TIMEOUT), Ok(60));
    }

    #[test]
    fn requests_that_fit_together_run_together() {
        let budget = Arc::new(MemoryBudget::new(100));
        let _first = budget.acquire(60);
        let (granted, _release) = request(&budget, 40);
        assert_eq!(granted.recv_timeout(TIMEOUT), Ok(40));
    }

    #[test]
    fn oversize_request_runs_alone() {
        let budget = Arc::new(MemoryBudget::new(100));
        assert_eq!(budget.total(), 100);
        // It waits for a small permit already held...
        let small = budget.acquire(10);
        let (oversize, release_oversize) = request(&budget, 500);
        assert_blocked(&oversize);
        drop(small);
        // ...holds the whole budget, clamped...
        assert_eq!(oversize.recv_timeout(TIMEOUT), Ok(100));
        // ...and nothing runs beside it.
        let (tiny, _release) = request(&budget, 1);
        assert_blocked(&tiny);
        release_oversize.send(()).unwrap();
        assert_eq!(tiny.recv_timeout(TIMEOUT), Ok(1));
    }

    #[test]
    fn permits_release_on_drop() {
        let budget = Arc::new(MemoryBudget::new(100));
        for bytes in [70, 100, 30] {
            drop(budget.acquire(bytes));
        }
        let (whole, _release) = request(&budget, 100);
        assert_eq!(whole.recv_timeout(TIMEOUT), Ok(100));
    }

    #[test]
    fn requests_are_served_in_arrival_order() {
        // A large request in line is not overtaken by a small one that would fit.
        let budget = Arc::new(MemoryBudget::new(100));
        let held = budget.acquire(50);
        let (large, release_large) = request(&budget, 80);
        wait_for_waiting(&budget, 1);
        let (small, release_small) = request(&budget, 10);
        wait_for_waiting(&budget, 2);
        assert_blocked(&small);
        drop(held);
        assert_eq!(large.recv_timeout(TIMEOUT), Ok(80));
        assert_eq!(small.recv_timeout(TIMEOUT), Ok(10));
        drop((release_large, release_small));
    }
}
