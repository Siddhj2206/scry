//! The virtual frame clock driving `wl_surface.frame` callbacks.
//!
//! Wayland clients request frame callbacks to pace their rendering; a
//! compositor answers them from its presentation loop. scry has no physical
//! scanout, so this module provides a controllable virtual clock instead:
//!
//! - [`FrameClockMode::Realtime`] (the default) ticks on a 60Hz `calloop`
//!   timer, so real applications neither stall after their first frame nor
//!   spin at unbounded FPS.
//! - [`FrameClockMode::Paused`] holds all pending callbacks until a caller
//!   explicitly steps the clock, letting deterministic tests advance
//!   animations frame by frame.
//!
//! The clock itself only tracks the mode and the virtual presentation time;
//! [`ScryState`](crate::state::ScryState) owns the timer source and turns
//! each produced timestamp into `wl_callback.done` events.

use std::time::Duration;

/// Nominal frame period in realtime mode: 60Hz.
///
/// Each realtime tick advances the virtual clock by exactly this interval,
/// keeping `wl_callback.done` timestamps evenly spaced at a deterministic
/// cadence regardless of event-loop jitter.
pub(crate) const FRAME_INTERVAL: Duration = Duration::from_nanos(1_000_000_000 / 60);

/// Operating mode of the virtual frame clock.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FrameClockMode {
    /// Frame callbacks fire on a steady 60Hz cadence.
    #[default]
    Realtime,
    /// Frame callbacks are held until [`CompositorHandle::step_frame_clock`]
    /// advances the clock.
    ///
    /// [`CompositorHandle::step_frame_clock`]: crate::CompositorHandle::step_frame_clock
    Paused,
}

/// The compositor's virtual presentation clock.
///
/// `now` is the monotonic virtual time reported to clients in
/// `wl_callback.done`. It only ever increases: realtime ticks advance it by
/// [`FRAME_INTERVAL`], [`FrameClock::step`] by the caller's duration.
pub(crate) struct FrameClock {
    mode: FrameClockMode,
    now: Duration,
}

impl FrameClock {
    /// A fresh clock in realtime mode at virtual time zero.
    pub(crate) fn new() -> Self {
        Self {
            mode: FrameClockMode::Realtime,
            now: Duration::ZERO,
        }
    }

    /// Switches between realtime and paused operation.
    ///
    /// The virtual time is never reset, so timestamps stay monotonic across
    /// mode switches. Switching to paused merely stops [`FrameClock::tick`]
    /// from advancing the clock.
    pub(crate) fn set_mode(&mut self, mode: FrameClockMode) {
        self.mode = mode;
    }

    /// One realtime tick.
    ///
    /// In realtime mode this advances the clock by [`FRAME_INTERVAL`] and
    /// returns the new presentation time. While paused it returns `None` and
    /// the clock holds still; the caller may fire it on a fixed timer
    /// regardless of mode so switching needs no timer juggling.
    pub(crate) fn tick(&mut self) -> Option<Duration> {
        if self.mode == FrameClockMode::Paused {
            return None;
        }
        self.now += FRAME_INTERVAL;
        Some(self.now)
    }

    /// Advances the clock by `duration` and returns the new presentation
    /// time, releasing frame callbacks held while paused.
    ///
    /// Stepping also works in realtime mode: it injects one extra
    /// advancement on top of the regular ticks, still monotonically.
    pub(crate) fn step(&mut self, duration: Duration) -> Duration {
        self.now += duration;
        self.now
    }
}
