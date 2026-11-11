//! The sizes the app remembers (spec §9.2): the side panels' widths, which are the user's
//! preferences and which a narrow window only lends less of, and the window's size, both
//! saved once they settle. `App` draws with them; the rules here are pure and tested
//! with explicit values and times.

use std::ops::RangeInclusive;

use eframe::egui;

/// How far a panel's width may come back from the width it was given and still be that
/// width: egui rounds a panel's edges to 1/32 point.
const SIZE_TOLERANCE: f32 = 0.5;
/// How long, in seconds, a changed window or panel size must stay the same to be saved.
const SETTLE_SECONDS: f64 = 1.0;

/// The widths the side panels were drawn at, once they have been.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct DrawnPanels {
    pub(crate) jobs: Option<f32>,
    pub(crate) inspector: Option<f32>,
}

/// A value saved once it has changed and then stayed the same for [`SETTLE_SECONDS`]: the
/// window's size and the panels', which change on every frame of a drag. It is fed with
/// egui's input time, never a wall clock, so it is tested with explicit times.
#[derive(Debug)]
pub(crate) struct Settle<T> {
    /// The value last saved, or the first one seen.
    saved: T,
    /// A value other than `saved`, with the time it was first seen.
    waiting: Option<(T, f64)>,
}

impl<T: Clone + PartialEq> Settle<T> {
    pub(crate) fn new(saved: T) -> Self {
        Self {
            saved,
            waiting: None,
        }
    }

    /// Takes `value` as seen at `time`: a change starts its second, the same value seen
    /// again keeps it running, and a value back to the saved one leaves nothing to save.
    pub(crate) fn see(&mut self, value: T, time: f64) {
        if value == self.saved {
            self.waiting = None;
        } else if self
            .waiting
            .as_ref()
            .is_none_or(|(waiting, _)| *waiting != value)
        {
            self.waiting = Some((value, time));
        }
    }

    /// When the waiting value is due, if one waits.
    pub(crate) fn due_at(&self) -> Option<f64> {
        self.waiting
            .as_ref()
            .map(|(_, since)| since + SETTLE_SECONDS)
    }

    /// The value to save at `time`: the waiting one, once its second is over.
    pub(crate) fn due(&mut self, time: f64) -> Option<T> {
        if self.due_at().is_some_and(|at| time >= at) {
            self.flush()
        } else {
            None
        }
    }

    /// The waiting value, due or not: what is saved when the window closes.
    pub(crate) fn flush(&mut self) -> Option<T> {
        let (value, _) = self.waiting.take()?;
        self.saved = value.clone();
        Some(value)
    }
}

/// The size range that makes a resizable panel `given` wide this frame, from `least` up
/// to `most`, while its edge can still be dragged anywhere in it. egui draws a panel at
/// the width it drew it at the frame before, clamped to its range: once the window has
/// narrowed it, that width is too small when the window grows again. So while the last
/// frame drew it narrower than `given`, the range starts at `given`. (Before its first
/// frame a panel takes its default size, `given` too.)
///
/// Its limit: it corrects a panel drawn narrower than it is given, never a preference
/// lowered by code while the panel is drawn wider. egui would keep the wider width, which
/// would then read as the user's resize and put the preference back. No caller lowers a
/// preference today; one that does must also give the panel a range that ends at it.
pub(crate) fn giving(
    given: f32,
    drawn_before: Option<f32>,
    least: f32,
    most: f32,
) -> RangeInclusive<f32> {
    let narrower = drawn_before.is_some_and(|drawn| drawn < given - SIZE_TOLERANCE);
    let low = if narrower { given } else { least };
    low..=most
}

/// Whether a panel given `given` and drawn at `drawn` was resized by the user: its edge
/// dragged, or a screen reader's step on it. Only then is the width drawn the preference;
/// the width a narrow window allows never is.
pub(crate) fn resized_by_the_user(given: f32, drawn: f32) -> bool {
    (drawn - given).abs() > SIZE_TOLERANCE
}

/// The window's size to save: its inner size in points at a zoom of 1, so a window drawn
/// zoomed in (Ctrl +, egui's `zoom`, which the next start does not keep) opens at its own
/// size again. `None` while it is maximised, minimised or in full screen, or when the
/// platform does not say.
pub(crate) fn ordinary_size(viewport: &egui::ViewportInfo, zoom: f32) -> Option<[f32; 2]> {
    let unusual =
        [viewport.maximized, viewport.minimized, viewport.fullscreen].contains(&Some(true));
    let size = viewport.inner_rect?.size() * zoom;
    (!unusual).then_some([size.x, size.y])
}

#[cfg(test)]
mod tests {
    use eframe::egui::{Pos2, Rect, Vec2};

    use super::*;

    #[test]
    fn of_several_values_within_a_second_only_the_last_is_saved() {
        let mut settle = Settle::new([1360.0, 820.0]);
        settle.see([1000.0, 700.0], 0.0);
        settle.see([1100.0, 700.0], 0.3);
        settle.see([1200.0, 800.0], 0.6);
        assert!(
            settle.due_at().is_some_and(|at| (at - 1.6).abs() < 1e-9),
            "{:?}",
            settle.due_at()
        );
        assert_eq!(settle.due(1.5), None);
        assert_eq!(settle.due(1.6), Some([1200.0, 800.0]));
        assert_eq!(settle.due(10.0), None);
        // An unchanged value saves nothing, and neither does one changed back.
        settle.see([1200.0, 800.0], 11.0);
        assert_eq!(settle.due_at(), None);
        settle.see([1300.0, 800.0], 12.0);
        settle.see([1200.0, 800.0], 12.5);
        assert_eq!(settle.due_at(), None);
        assert_eq!(settle.due(20.0), None);
        // Closing takes a waiting value at once.
        settle.see([1000.0, 600.0], 30.0);
        assert_eq!(settle.flush(), Some([1000.0, 600.0]));
        assert_eq!(settle.due(40.0), None);
    }

    #[test]
    fn the_size_saved_is_the_size_at_zoom_one() {
        let viewport = |maximized| egui::ViewportInfo {
            inner_rect: Some(Rect::from_min_size(Pos2::ZERO, Vec2::new(800.0, 500.0))),
            maximized: Some(maximized),
            ..egui::ViewportInfo::default()
        };
        assert_eq!(ordinary_size(&viewport(false), 1.0), Some([800.0, 500.0]));
        // Zoomed in by half, the window shows fewer points of a larger size.
        assert_eq!(ordinary_size(&viewport(false), 1.5), Some([1200.0, 750.0]));
        assert_eq!(ordinary_size(&viewport(true), 1.0), None);
        assert_eq!(ordinary_size(&egui::ViewportInfo::default(), 1.0), None);
    }

    #[test]
    fn a_panel_narrowed_by_the_window_is_given_its_width_back() {
        // Drawn as wide as given, or wider (a drag), or never: its edge moves freely.
        assert_eq!(giving(260.0, Some(260.0), 200.0, 400.0), 200.0..=400.0);
        assert_eq!(giving(260.0, Some(300.0), 200.0, 400.0), 200.0..=400.0);
        assert_eq!(giving(260.0, None, 200.0, 400.0), 200.0..=400.0);
        // Narrower than given: the window was widened again.
        assert_eq!(giving(260.0, Some(240.0), 200.0, 400.0), 260.0..=400.0);
        // A user's resize is a width other than the one given, past egui's rounding.
        assert!(!resized_by_the_user(260.0, 260.03125));
        assert!(resized_by_the_user(260.0, 300.0));
    }
}
