//! Wakes the window when the engine has news. egui draws only on input or on request, so
//! an engine event arriving while the window is idle would wait for the next mouse move;
//! the engine calls its [`Wake`] callback after each event instead, and the callback asks
//! egui for a frame.
//!
//! The engine starts before the window (the saved window size must be read first), so its
//! callback is made before there is a context to repaint: [`Waker`] hands it out early and
//! is given the context once the window exists.

use std::sync::{Arc, OnceLock};

use eframe::egui;
use photostacker_core::engine::Wake;

/// The window's context, once there is one, shared with the callbacks it handed out.
#[derive(Default)]
pub struct Waker {
    context: Arc<OnceLock<egui::Context>>,
}

impl Waker {
    /// A callback for the engine: it requests a repaint once a context is attached, and
    /// does nothing before (the first frame reads whatever the engine sent until then).
    pub fn wake(&self) -> Wake {
        let context = Arc::clone(&self.context);
        Arc::new(move || {
            if let Some(context) = context.get() {
                context.request_repaint();
            }
        })
    }

    /// Gives the callbacks the window's context. The app has one window, so only the first
    /// context attached is kept.
    pub fn attach(&self, context: &egui::Context) {
        let _ = self.context.set(context.clone());
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;

    #[test]
    fn a_wake_requests_a_repaint_only_once_a_context_is_attached() {
        let waker = Waker::default();
        let wake = waker.wake();
        // Before the window exists, a wake has nothing to repaint and must not fail.
        wake();

        let context = egui::Context::default();
        let requests = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&requests);
        context.set_repaint_observer(move |_| {
            counter.fetch_add(1, Ordering::SeqCst);
        });
        waker.attach(&context);
        assert_eq!(requests.load(Ordering::SeqCst), 0);
        wake();
        assert_eq!(requests.load(Ordering::SeqCst), 1);
    }
}
