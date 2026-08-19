//! Touch gesture recognition vocabulary.
//!
//! GPUI recognizes gestures from raw [`TouchEvent`](crate::TouchEvent)s in a
//! single, portable arena in gpui core: recognizers compete for in-flight
//! touches, winners claim them, and losers are cancelled. Recognized gestures
//! are surfaced through *existing* semantic events wherever possible, a tap
//! becomes [`ClickEvent::Touch`](crate::ClickEvent), a pan becomes
//! [`ScrollWheelEvent`](crate::ScrollWheelEvent)s carrying a
//! [`TouchPhase`](crate::TouchPhase), and a pinch becomes
//! [`PinchEvent`](crate::PinchEvent)s — so components written against
//! `on_click` and scroll containers work untouched on mobile.

use std::{
    collections::VecDeque,
    time::{Duration, Instant},
};

use crate::{App, Axis, IsZero, Pixels, Point, TouchPhase, Window, point, px};

const SCROLL_EVENT_SEPARATION: Duration = Duration::from_millis(28);

const VELOCITY_SAMPLE_WINDOW: Duration = Duration::from_millis(100);
const VELOCITY_RELEASE_MAX_AGE: Duration = Duration::from_millis(50);
const VELOCITY_MIN_SAMPLE_SPAN: Duration = Duration::from_millis(8);
const VELOCITY_MAX_SAMPLES: usize = 32;
const MOMENTUM_STOP_VELOCITY: f32 = 10.;
const MOMENTUM_MAX_VELOCITY: f32 = 8_000.;
const MOMENTUM_MAX_DURATION: Duration = Duration::from_secs(3);
const MOMENTUM_MAX_FRAMES: u16 = 480;
const MOMENTUM_MAX_FRAME_INTERVAL: Duration = Duration::from_millis(50);
const MOMENTUM_RESUME_TIMEOUT: Duration = Duration::from_millis(250);
const MOMENTUM_CONSUMPTION_EPSILON: f32 = 0.25;

#[derive(Clone, Copy, Debug)]
struct ScrollMotionSample {
    at: Instant,
    position: Point<f32>,
}

#[derive(Debug)]
struct ScrollVelocityRecorder {
    samples: VecDeque<ScrollMotionSample>,
    position: Point<f32>,
    last_movement_at: Option<Instant>,
}

impl ScrollVelocityRecorder {
    fn new(started_at: Instant) -> Self {
        Self {
            samples: VecDeque::from([ScrollMotionSample {
                at: started_at,
                position: Point::default(),
            }]),
            position: Point::default(),
            last_movement_at: None,
        }
    }

    fn record(&mut self, delta: Point<Pixels>, now: Instant) {
        if delta.is_zero() {
            return;
        }
        self.position.x += delta.x.as_f32();
        self.position.y += delta.y.as_f32();
        self.last_movement_at = Some(now);
        if self.samples.len() == VELOCITY_MAX_SAMPLES {
            self.samples.pop_front();
        }
        self.samples.push_back(ScrollMotionSample {
            at: now,
            position: self.position,
        });
        while self.samples.len() > 2
            && self
                .samples
                .front()
                .is_some_and(|sample| now.duration_since(sample.at) > VELOCITY_SAMPLE_WINDOW)
        {
            self.samples.pop_front();
        }
    }

    fn release_velocity(&self, now: Instant) -> Option<Point<f32>> {
        let last_movement_at = self.last_movement_at?;
        if now.duration_since(last_movement_at) > VELOCITY_RELEASE_MAX_AGE {
            return None;
        }
        let last = self.samples.back()?;
        let first = self
            .samples
            .iter()
            .find(|sample| last.at.duration_since(sample.at) <= VELOCITY_SAMPLE_WINDOW)?;
        let span = last.at.duration_since(first.at);
        if span < VELOCITY_MIN_SAMPLE_SPAN {
            return None;
        }
        let seconds = span.as_secs_f32();
        Some(point(
            (last.position.x - first.position.x) / seconds,
            (last.position.y - first.position.y) / seconds,
        ))
    }
}

#[derive(Clone, Copy, Debug)]
struct ScrollMomentum {
    velocity: Point<f32>,
    last_frame_at: Instant,
    elapsed: Duration,
    frames: u16,
}

/// One analytically integrated kinetic-scroll frame.
#[derive(Clone, Copy, Debug)]
pub struct KineticScrollStep {
    /// Requested content movement for this frame.
    pub delta: Point<Pixels>,
    /// Whether another frame is required after this one is consumed.
    pub continues: bool,
}

/// Records precise finger motion and integrates a bounded momentum tail.
///
/// The state is platform-neutral. Scroll containers opt in only when the
/// platform event explicitly requests synthesized momentum.
#[derive(Debug, Default)]
pub struct KineticScroll {
    generation: u64,
    recorder: Option<ScrollVelocityRecorder>,
    momentum: Option<ScrollMomentum>,
}

impl KineticScroll {
    /// Begin a new finger gesture, cancelling any pending momentum callback.
    pub fn begin_at(&mut self, now: Instant) {
        self.cancel();
        self.recorder = Some(ScrollVelocityRecorder::new(now));
    }

    /// Record content pixels that the scroll container actually consumed.
    pub fn record_movement_at(&mut self, delta: Point<Pixels>, now: Instant) {
        if let Some(recorder) = self.recorder.as_mut() {
            recorder.record(delta, now);
        }
    }

    /// Whether a paired finger gesture is currently recording movement.
    pub fn is_recording(&self) -> bool {
        self.recorder.is_some()
    }

    /// Finish a finger gesture and return the generation to schedule, if its
    /// measured release velocity is sufficient for a fling.
    pub fn finish_at(&mut self, now: Instant, tuning: GestureTuning) -> Option<u64> {
        let recorder = self.recorder.take()?;
        let mut velocity = recorder.release_velocity(now)?;
        let speed = velocity.x.hypot(velocity.y);
        if speed < tuning.min_fling_velocity {
            return None;
        }
        if speed > MOMENTUM_MAX_VELOCITY {
            let scale = MOMENTUM_MAX_VELOCITY / speed;
            velocity.x *= scale;
            velocity.y *= scale;
        }
        self.momentum = Some(ScrollMomentum {
            velocity,
            last_frame_at: now,
            elapsed: Duration::ZERO,
            frames: 0,
        });
        Some(self.generation)
    }

    /// Integrate the next momentum frame for a matching, active generation.
    pub fn frame_at(
        &mut self,
        generation: u64,
        now: Instant,
        tuning: GestureTuning,
    ) -> Option<KineticScrollStep> {
        if generation != self.generation {
            return None;
        }
        let momentum = self.momentum.as_mut()?;
        let actual_elapsed = now.saturating_duration_since(momentum.last_frame_at);
        if actual_elapsed > MOMENTUM_RESUME_TIMEOUT {
            self.momentum = None;
            return None;
        }
        let elapsed = actual_elapsed.min(MOMENTUM_MAX_FRAME_INTERVAL);
        momentum.last_frame_at = now;
        momentum.elapsed += actual_elapsed;
        momentum.frames = momentum.frames.saturating_add(1);

        let seconds = elapsed.as_secs_f32();
        let decay_per_second = tuning.momentum_decay_per_ms.ln() * 1_000.;
        let decay = (decay_per_second * seconds).exp();
        let distance_scale = if decay_per_second.abs() > f32::EPSILON {
            (decay - 1.) / decay_per_second
        } else {
            seconds
        };
        let delta = point(
            px(momentum.velocity.x * distance_scale),
            px(momentum.velocity.y * distance_scale),
        );
        momentum.velocity.x *= decay;
        momentum.velocity.y *= decay;

        let continues = momentum.velocity.x.hypot(momentum.velocity.y) >= MOMENTUM_STOP_VELOCITY
            && momentum.elapsed < MOMENTUM_MAX_DURATION
            && momentum.frames < MOMENTUM_MAX_FRAMES;
        if !continues {
            self.momentum = None;
        }
        Some(KineticScrollStep { delta, continues })
    }

    /// Consume a frame using the pixels the container actually moved. A
    /// clipped component stops independently, so diagonal momentum may keep
    /// moving on the axis that is not at a bound.
    pub fn consume(
        &mut self,
        generation: u64,
        requested: Point<Pixels>,
        consumed: Point<Pixels>,
    ) -> bool {
        if generation != self.generation {
            return false;
        }
        let Some(momentum) = self.momentum.as_mut() else {
            return false;
        };
        if (requested.x - consumed.x).abs().as_f32() > MOMENTUM_CONSUMPTION_EPSILON {
            momentum.velocity.x = 0.;
        }
        if (requested.y - consumed.y).abs().as_f32() > MOMENTUM_CONSUMPTION_EPSILON {
            momentum.velocity.y = 0.;
        }
        if momentum.velocity.x.hypot(momentum.velocity.y) < MOMENTUM_STOP_VELOCITY {
            self.momentum = None;
        }
        self.momentum.is_some()
    }

    /// Cancel recording and momentum. A queued callback becomes stale and
    /// drains without scheduling a successor.
    pub fn cancel(&mut self) {
        self.generation = self.generation.wrapping_add(1);
        self.recorder = None;
        self.momentum = None;
    }

    #[cfg(test)]
    fn is_animating(&self) -> bool {
        self.momentum.is_some()
    }
}

/// Schedule one kinetic-scroll callback on the next platform frame.
pub fn schedule_kinetic_scroll_frame(
    window: &mut Window,
    callback: impl FnOnce(Instant, &mut Window, &mut App) + 'static,
) {
    window.on_next_frame(move |window, cx| callback(cx.background_executor().now(), window, cx));
}

/// Return the platform's kinetic-scroll tuning, or GPUI's portable defaults.
pub fn platform_gesture_tuning(cx: &App) -> GestureTuning {
    cx.platform
        .gestures()
        .map(|gestures| gestures.tuning())
        .unwrap_or_default()
}

/// Tracks the dominant axis across the events in a scroll gesture.
#[derive(Clone, Copy, Debug, Default)]
pub struct OngoingScroll {
    last_event: Option<Instant>,
    axis: Option<Axis>,
}

impl OngoingScroll {
    /// Filters the given delta to the dominant axis of the current scroll gesture.
    ///
    /// Gestures are delimited by their touch phase when available, with a timeout
    /// fallback for platforms that only emit [`TouchPhase::Moved`].
    pub fn filter(&mut self, delta: &mut Point<Pixels>, touch_phase: TouchPhase) {
        self.filter_at(delta, touch_phase, Instant::now())
    }

    fn filter_at(&mut self, delta: &mut Point<Pixels>, touch_phase: TouchPhase, now: Instant) {
        const UNLOCK_PERCENT: f32 = 1.9;
        const UNLOCK_LOWER_BOUND: Pixels = px(6.);

        if matches!(touch_phase, TouchPhase::Ended | TouchPhase::Cancelled) {
            self.last_event = None;
            self.axis = None;
            return;
        }

        let x = delta.x.abs();
        let y = delta.y.abs();
        if x.is_zero() && y.is_zero() {
            if touch_phase == TouchPhase::Started {
                self.last_event = None;
                self.axis = None;
            }
            return;
        }

        let starts_new_gesture = touch_phase == TouchPhase::Started
            || self
                .last_event
                .is_none_or(|last_event| now.duration_since(last_event) >= SCROLL_EVENT_SEPARATION);
        let mut axis = self.axis;
        if starts_new_gesture {
            axis = if x <= y {
                Some(Axis::Vertical)
            } else {
                Some(Axis::Horizontal)
            };
        } else if x.max(y) >= UNLOCK_LOWER_BOUND {
            match axis {
                Some(Axis::Vertical) if x > y && x >= y * UNLOCK_PERCENT => {
                    axis = None;
                }
                Some(Axis::Horizontal) if y > x && y >= x * UNLOCK_PERCENT => {
                    axis = None;
                }
                _ => {}
            }
        }

        self.last_event = Some(now);
        self.axis = axis;
        match axis {
            Some(Axis::Vertical) => delta.x = Pixels::ZERO,
            Some(Axis::Horizontal) => delta.y = Pixels::ZERO,
            None => {}
        }
    }
}

/// Feel constants consumed by gesture recognizers. Provided on a best-effort
/// basis, depending on each platform's support, defaulting to GPUI's own
/// (iOS flavored) values
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct GestureTuning {
    /// Distance a touch may travel before it stops being a potential tap and
    /// becomes a pan/drag.
    pub touch_slop: Pixels,
    /// Maximum interval between taps for them to accumulate a tap count.
    pub multi_tap_interval: Duration,
    /// Maximum distance between taps for them to accumulate a tap count.
    pub multi_tap_slop: Pixels,
    /// How long a touch must remain within [`Self::touch_slop`] to be
    /// recognized as a long press.
    pub long_press_duration: Duration,
    /// Per-millisecond decay factor applied to scroll momentum after a fling.
    /// (`UIScrollView` uses `0.998` per millisecond for its normal
    /// deceleration rate.)
    pub momentum_decay_per_ms: f32,
    /// Minimum release velocity, in pixels per second, required to start
    /// scroll momentum.
    pub min_fling_velocity: f32,
}

impl Default for GestureTuning {
    fn default() -> Self {
        Self {
            touch_slop: px(8.),
            multi_tap_interval: Duration::from_millis(400),
            multi_tap_slop: px(16.),
            long_press_duration: Duration::from_millis(500),
            momentum_decay_per_ms: 0.998,
            min_fling_velocity: 50.,
        }
    }
}

/// The set of gesture kinds that participate in recognition.
///
/// Used by [`PlatformGestures::native_recognizers`] to declare which gestures
/// the platform recognizes natively rather than leaving to gpui core's
/// portable recognizers.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct GestureKinds {
    /// Tap (and multi-tap), surfaced as [`ClickEvent::Touch`](crate::ClickEvent).
    pub tap: bool,
    /// Long press, surfaced as [`LongPressEvent`].
    pub long_press: bool,
    /// Pan/scroll (including fling momentum), surfaced as
    /// [`ScrollWheelEvent`](crate::ScrollWheelEvent)s.
    pub pan: bool,
    /// Pinch to zoom, surfaced as [`PinchEvent`](crate::PinchEvent)s.
    pub pinch: bool,
}

impl GestureKinds {
    /// No gestures; gpui core's portable recognizers handle everything.
    pub const NONE: Self = Self {
        tap: false,
        long_press: false,
        pan: false,
        pinch: false,
    };

    /// All gesture kinds.
    pub const ALL: Self = Self {
        tap: true,
        long_press: true,
        pan: true,
        pinch: true,
    };
}

/// A long-press gesture, mobile's context-menu trigger.
///
/// A bare long press is surfaced as a [`ClickEvent`](crate::ClickEvent) with
/// `long_press: true`, delivered to aux-click listeners alongside right
/// clicks. This event is the raw hook for elements that need the gesture
/// itself (e.g. long-press to start a drag); the registration API ships
/// together with the gesture arena.
#[derive(Clone, Debug, Default)]
pub struct LongPressEvent {
    /// The position of the touch that was recognized as a long press.
    pub position: Point<Pixels>,
}

/// Platform gesture recognition services.
///
/// If your mobile platform supports native gesture recognition, use this
/// to share it with GPUI.
pub trait PlatformGestures {
    /// Feel constants for the portable recognizers on this platform.
    fn tuning(&self) -> GestureTuning {
        GestureTuning::default()
    }

    /// The gesture kinds this platform recognizes natively.
    fn native_recognizers(&self) -> GestureKinds {
        GestureKinds::NONE
    }
}

/// A no-op [`PlatformGestures`] implementation: no native recognizers and
/// default tuning. Suitable for desktop platforms and tests.
pub struct NullPlatformGestures;

impl PlatformGestures for NullPlatformGestures {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::point;

    #[test]
    fn ongoing_scroll_locks_to_dominant_axis() {
        let now = Instant::now();
        let mut ongoing_scroll = OngoingScroll::default();
        let mut horizontal_delta = point(px(10.), px(2.));
        ongoing_scroll.filter_at(&mut horizontal_delta, TouchPhase::Started, now);
        assert_eq!(ongoing_scroll.axis, Some(Axis::Horizontal));
        assert_eq!(horizontal_delta, point(px(10.), px(0.)));

        let mut continued_delta = point(px(3.), px(2.));
        ongoing_scroll.filter_at(
            &mut continued_delta,
            TouchPhase::Moved,
            now + Duration::from_millis(1),
        );
        assert_eq!(ongoing_scroll.axis, Some(Axis::Horizontal));
        assert_eq!(continued_delta, point(px(3.), px(0.)));
    }

    #[test]
    fn ongoing_scroll_unlocks_when_direction_changes() {
        let now = Instant::now();
        let mut ongoing_scroll = OngoingScroll::default();
        let mut horizontal_delta = point(px(10.), px(2.));
        ongoing_scroll.filter_at(&mut horizontal_delta, TouchPhase::Started, now);

        let mut vertical_delta = point(px(2.), px(10.));
        ongoing_scroll.filter_at(
            &mut vertical_delta,
            TouchPhase::Moved,
            now + Duration::from_millis(1),
        );
        assert_eq!(ongoing_scroll.axis, None);
        assert_eq!(vertical_delta, point(px(2.), px(10.)));
    }

    #[test]
    fn ongoing_scroll_starts_new_gesture_at_timeout_boundary() {
        let now = Instant::now();
        let mut ongoing_scroll = OngoingScroll::default();
        let mut horizontal_delta = point(px(10.), px(2.));
        ongoing_scroll.filter_at(&mut horizontal_delta, TouchPhase::Moved, now);

        let mut vertical_delta = point(px(2.), px(10.));
        ongoing_scroll.filter_at(
            &mut vertical_delta,
            TouchPhase::Moved,
            now + SCROLL_EVENT_SEPARATION,
        );
        assert_eq!(ongoing_scroll.axis, Some(Axis::Vertical));
        assert_eq!(vertical_delta, point(px(0.), px(10.)));
    }

    #[test]
    fn ongoing_scroll_ignores_zero_delta_and_resets_when_ended() {
        let now = Instant::now();
        let mut ongoing_scroll = OngoingScroll::default();
        let mut horizontal_delta = point(px(10.), px(2.));
        ongoing_scroll.filter_at(&mut horizontal_delta, TouchPhase::Started, now);

        let mut zero_delta = Point::default();
        ongoing_scroll.filter_at(
            &mut zero_delta,
            TouchPhase::Ended,
            now + Duration::from_millis(1),
        );
        assert_eq!(ongoing_scroll.axis, None);

        let mut vertical_delta = point(px(2.), px(3.));
        ongoing_scroll.filter_at(
            &mut vertical_delta,
            TouchPhase::Moved,
            now + Duration::from_millis(2),
        );
        assert_eq!(ongoing_scroll.axis, Some(Axis::Vertical));
        assert_eq!(vertical_delta, point(px(0.), px(3.)));
    }

    #[test]
    fn ongoing_scroll_ignores_zero_delta_movement() {
        let now = Instant::now();
        let mut ongoing_scroll = OngoingScroll::default();
        let mut horizontal_delta = point(px(10.), px(2.));
        ongoing_scroll.filter_at(&mut horizontal_delta, TouchPhase::Started, now);

        let mut zero_delta = Point::default();
        ongoing_scroll.filter_at(
            &mut zero_delta,
            TouchPhase::Moved,
            now + SCROLL_EVENT_SEPARATION,
        );

        let mut vertical_delta = point(px(2.), px(10.));
        ongoing_scroll.filter_at(
            &mut vertical_delta,
            TouchPhase::Moved,
            now + SCROLL_EVENT_SEPARATION,
        );
        assert_eq!(ongoing_scroll.axis, Some(Axis::Vertical));
        assert_eq!(vertical_delta, point(px(0.), px(10.)));
    }

    #[test]
    fn ongoing_scroll_supports_moved_only_platforms() {
        let now = Instant::now();
        let mut ongoing_scroll = OngoingScroll::default();
        let mut horizontal_delta = point(px(10.), px(2.));
        ongoing_scroll.filter_at(&mut horizontal_delta, TouchPhase::Moved, now);
        assert_eq!(ongoing_scroll.axis, Some(Axis::Horizontal));
        assert_eq!(horizontal_delta, point(px(10.), px(0.)));
    }

    #[test]
    fn kinetic_scroll_records_release_velocity_from_consumed_pixels() {
        let start = Instant::now();
        let mut kinetic = KineticScroll::default();
        kinetic.begin_at(start);
        kinetic.record_movement_at(point(px(0.), px(10.)), start);
        kinetic.record_movement_at(point(px(0.), px(10.)), start + Duration::from_millis(10));
        kinetic.record_movement_at(point(px(0.), px(10.)), start + Duration::from_millis(20));

        let generation = kinetic
            .finish_at(start + Duration::from_millis(20), GestureTuning::default())
            .expect("consumed motion should produce a fling");
        let step = kinetic
            .frame_at(
                generation,
                start + Duration::from_millis(30),
                GestureTuning::default(),
            )
            .expect("fling should produce a frame");

        assert!(step.delta.y > px(10.));
        assert!(step.continues);
    }

    #[test]
    fn unpaired_moved_input_never_opens_a_velocity_recorder() {
        let now = Instant::now();
        let mut kinetic = KineticScroll::default();

        kinetic.record_movement_at(point(px(0.), px(20.)), now);

        assert!(kinetic.recorder.is_none());
        assert!(kinetic.finish_at(now, GestureTuning::default()).is_none());
    }

    #[test]
    fn velocity_recorder_has_a_hard_sample_bound() {
        let start = Instant::now();
        let mut kinetic = KineticScroll::default();
        kinetic.begin_at(start);
        for sample in 0..100 {
            kinetic
                .record_movement_at(point(px(0.), px(1.)), start + Duration::from_millis(sample));
        }

        assert_eq!(
            kinetic
                .recorder
                .as_ref()
                .expect("started gesture should retain its recorder")
                .samples
                .len(),
            VELOCITY_MAX_SAMPLES
        );
    }

    #[test]
    fn kinetic_integration_is_equivalent_at_sixty_and_one_twenty_hz() {
        let distance_60 = integrated_distance(Duration::from_nanos(16_666_667), 60);
        let distance_120 = integrated_distance(Duration::from_nanos(8_333_333), 120);

        assert!((distance_60 - distance_120).abs() < 0.1);
    }

    #[test]
    fn kinetic_scroll_stops_at_bounds_and_stale_callback_drains_once() {
        let start = Instant::now();
        let mut kinetic = momentum_for_test(start, point(0., 2_000.));
        let generation = kinetic.generation;
        let step = kinetic
            .frame_at(
                generation,
                start + Duration::from_millis(8),
                GestureTuning::default(),
            )
            .expect("momentum should produce a frame");

        assert!(!kinetic.consume(generation, step.delta, Point::default()));
        assert!(!kinetic.is_animating());
        assert!(
            kinetic
                .frame_at(
                    generation,
                    start + Duration::from_millis(16),
                    GestureTuning::default(),
                )
                .is_none()
        );
    }

    #[test]
    fn diagonal_momentum_continues_on_the_axis_that_consumed_pixels() {
        let start = Instant::now();
        let mut kinetic = momentum_for_test(start, point(2_000., 2_000.));
        let generation = kinetic.generation;
        let first = kinetic
            .frame_at(
                generation,
                start + Duration::from_millis(8),
                GestureTuning::default(),
            )
            .expect("momentum should produce a frame");
        let consumed = point(first.delta.x, px(0.));

        assert!(kinetic.consume(generation, first.delta, consumed));
        let second = kinetic
            .frame_at(
                generation,
                start + Duration::from_millis(16),
                GestureTuning::default(),
            )
            .expect("unblocked horizontal momentum should continue");
        assert!(second.delta.x > px(0.));
        assert_eq!(second.delta.y, px(0.));
    }

    #[test]
    fn pathological_frame_intervals_are_clamped_or_cancelled() {
        let start = Instant::now();
        let mut kinetic = momentum_for_test(start, point(0., 2_000.));
        let generation = kinetic.generation;

        let clamped = kinetic
            .frame_at(
                generation,
                start + Duration::from_millis(100),
                GestureTuning::default(),
            )
            .expect("a delayed but active frame should be integrated");
        assert!(clamped.delta.y < px(100.));
        assert!(clamped.delta.y > px(0.));

        assert!(
            kinetic
                .frame_at(
                    generation,
                    start + Duration::from_millis(400),
                    GestureTuning::default(),
                )
                .is_none()
        );
        assert!(!kinetic.is_animating());
    }

    #[test]
    fn kinetic_scroll_has_a_hard_frame_bound() {
        let start = Instant::now();
        let mut kinetic = momentum_for_test(start, point(0., MOMENTUM_MAX_VELOCITY));
        let generation = kinetic.generation;
        let cadence = Duration::from_nanos(8_333_333);
        let mut frames = 0_u16;

        while kinetic
            .frame_at(
                generation,
                start + cadence * u32::from(frames + 1),
                GestureTuning::default(),
            )
            .is_some()
        {
            frames += 1;
            assert!(frames <= MOMENTUM_MAX_FRAMES);
        }
        assert!(!kinetic.is_animating());
    }

    fn integrated_distance(cadence: Duration, frames: u32) -> f32 {
        let start = Instant::now();
        let mut kinetic = momentum_for_test(start, point(0., 2_000.));
        let generation = kinetic.generation;
        (1..=frames)
            .filter_map(|frame| {
                kinetic.frame_at(
                    generation,
                    start + cadence * frame,
                    GestureTuning::default(),
                )
            })
            .map(|step| step.delta.y.as_f32())
            .sum()
    }

    fn momentum_for_test(start: Instant, velocity: Point<f32>) -> KineticScroll {
        KineticScroll {
            generation: 1,
            recorder: None,
            momentum: Some(ScrollMomentum {
                velocity,
                last_frame_at: start,
                elapsed: Duration::ZERO,
                frames: 0,
            }),
        }
    }
}
