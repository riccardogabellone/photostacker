//! Turns a panic in the work on one file into an ordinary failure of that file.
//!
//! Decoders and parsers can panic on broken input: rawler, the `image`
//! codecs, the libheif wrappers, and our own code. A panic inside the
//! analysis pool would unwind through `rayon` and end the whole analysis, so
//! every per-file step runs inside [`catch`].
//!
//! That relies on `panic = "unwind"`, which the workspace profiles pin.
//!
//! The panic hook runs before the unwinding, so by default it prints the
//! message of every caught panic to stderr. [`install_panic_hook`] keeps
//! those quiet: a panic on a thread inside [`catch`] is logged at `debug`
//! instead, and the code that catches it logs a `warn` that names the file
//! and the step. A panic outside still reaches the previous hook.
//!
//! The hook runs on the thread that panicked, and rawler's own `par_iter`
//! (its decompressors and demosaic) runs pieces of its work on other rayon
//! workers. Such a panic is quiet only if that worker is itself inside a
//! [`catch`]. Either way rayon hands the panic back to the caller's
//! [`catch`], and handing it back runs no hook, so the file's failure is the
//! same; only the message may still be printed.
//! - In the analysis pool a worker that runs a piece while it waits within
//!   its own file's work is inside a [`catch`]; an idle one is not.
//! - At write time it is the common case: the writers decode on the
//!   caller's thread, outside any pool, so rawler's pieces run on the
//!   global rayon pool, whose workers are never inside a [`catch`]. Every
//!   panic in rawler's parallel code while a stack is written reaches the
//!   previous hook, and that is when most RAWs are first fully
//!   decompressed. The same holds for any caller thread, such as an
//!   engine's.
//!
//! A panic inside an `extern "C"` callback cannot be caught: it would have
//! to unwind through C frames, so Rust aborts the whole process instead. The
//! `StreamReader` that libheif calls to read a HEIF file is such a callback,
//! so nothing it runs may panic.
//!
//! Spec §4.3 states the rule this module serves: a panic in one file's work
//! never ends the job. Spec §5.1 says that unreadable EXIF is never a file
//! error, except for a panic caught while an active file's EXIF is read,
//! which fails that file like a panic in any other step.

use std::any::Any;
use std::cell::Cell;
use std::panic::{self, AssertUnwindSafe, PanicHookInfo, catch_unwind};
use std::sync::Once;

thread_local! {
    /// How many [`catch`] calls the current thread is inside.
    static CATCH_DEPTH: Cell<usize> = const { Cell::new(0) };
}

/// Runs `work` and returns its result, or the panic's message if it panicked.
///
/// Unwind safety: the caller must not use anything `work` mutated after it
/// panicked. The per-file steps only read shared state (file maps, decoder
/// tables, the partners' EXIF) and build their results locally.
pub fn catch<T>(work: impl FnOnce() -> T) -> Result<T, String> {
    // `catch_unwind` returns on a panic too, so the depth always comes back.
    CATCH_DEPTH.with(|depth| depth.set(depth.get() + 1));
    let caught = catch_unwind(AssertUnwindSafe(work));
    CATCH_DEPTH.with(|depth| depth.set(depth.get() - 1));
    caught.map_err(|payload| message(&*payload))
}

/// Installs a panic hook that wraps the current one: a panic on a thread
/// inside [`catch`] is logged with `tracing::debug!` (its message and
/// location) instead of reaching the previous hook, and any other panic
/// reaches it. Only the first call installs anything.
pub fn install_panic_hook() {
    static INSTALLED: Once = Once::new();
    INSTALLED.call_once(|| {
        let previous = panic::take_hook();
        panic::set_hook(Box::new(move |info| {
            if inside_catch() {
                log_caught(info);
            } else {
                previous(info);
            }
        }));
    });
}

/// Whether the current thread is inside [`catch`]; not while its
/// thread-locals are being destroyed.
fn inside_catch() -> bool {
    CATCH_DEPTH
        .try_with(|depth| depth.get() > 0)
        .unwrap_or(false)
}

fn log_caught(info: &PanicHookInfo) {
    let message = message(info.payload());
    let location = info
        .location()
        .map_or_else(|| "unknown".to_owned(), ToString::to_string);
    tracing::debug!(%location, "caught a panic: {message}");
}

/// The message of a panic payload: `panic!` gives a `&str` or a `String`.
pub fn message(payload: &(dyn Any + Send)) -> String {
    payload
        .downcast_ref::<&str>()
        .copied()
        .or_else(|| payload.downcast_ref::<String>().map(String::as_str))
        .unwrap_or("no message")
        .to_owned()
}

#[cfg(test)]
mod tests {
    use std::panic::{self, panic_any};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::thread;

    use super::*;

    #[test]
    fn caught_panic_skips_the_previous_hook() {
        // The counting hook sees only this test's panics; any other goes on
        // to the default hook, so a test sharing the process still reports.
        const MARK: &str = "guard test panic";
        static MARKED: AtomicUsize = AtomicUsize::new(0);
        let default = panic::take_hook();
        panic::set_hook(Box::new(move |info| {
            if message(info.payload()) == MARK {
                MARKED.fetch_add(1, Ordering::SeqCst);
            } else {
                default(info);
            }
        }));
        install_panic_hook();
        // A second call installs nothing more.
        install_panic_hook();

        assert_eq!(catch(|| -> u8 { panic!("{MARK}") }), Err(MARK.into()));
        assert_eq!(MARKED.load(Ordering::SeqCst), 0);
        assert!(thread::spawn(|| panic!("{MARK}")).join().is_err());
        assert_eq!(MARKED.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn a_result_passes_through() {
        assert_eq!(catch(|| 7), Ok(7));
    }

    #[test]
    fn a_str_panic_becomes_its_message() {
        assert_eq!(
            catch(|| -> u8 { panic!("broken tile") }),
            Err("broken tile".into())
        );
    }

    #[test]
    fn a_string_panic_becomes_its_message() {
        let offset = 12;
        assert_eq!(
            catch(|| -> u8 { panic!("offset {offset} out of range") }),
            Err("offset 12 out of range".into())
        );
    }

    #[test]
    fn any_other_payload_has_no_message() {
        assert_eq!(
            catch(|| -> u8 { panic_any(42_u32) }),
            Err("no message".into())
        );
    }
}
