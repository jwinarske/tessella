//! Damage gating: emitting nothing when nothing changed (§6, DR-8).
//!
//! # The guarantee is a number, not an intention
//!
//! §6 states three things that §9.3 turns into CI assertions: a parked view emits **zero** ring
//! bytes, pure camera motion emits camera-block bytes only, and churn emits churn-proportional
//! bytes. DR-8 makes the first a protocol guarantee rather than an optimization.
//!
//! Zero is what makes it testable. "Few bytes" would be satisfied by a producer that re-emitted
//! a byte-identical camera every frame, which is precisely the failure the guarantee exists to
//! forbid — it keeps the consumer's tick awake, keeps the ring hot, and on a DVFS-governed part
//! keeps the whole SoC out of its idle state (§12.8).
//!
//! # Exact comparison, and why the projection had to be bit-exact
//!
//! A camera is unchanged when its fields compare **exactly** equal as f64 (§6.3). Not
//! approximately: the values are deterministic functions of the transform, so equality is
//! meaningful, and a tolerance would let a camera drift indefinitely while reporting itself
//! unchanged.
//!
//! The cost of that decision lands upstream. A projection that disagreed with the oracle by one
//! ULP would recompute `centerZoom0` slightly differently each frame and report the camera
//! changed forever, so a parked map would emit a camera block per frame and the guarantee would
//! fail — not visibly, just as traffic that never stops. That is why the projection is written
//! in mbgl's operation order rather than the textbook's, and why its test compares bit patterns.
//!
//! # NaN is not equal to itself
//!
//! A camera field that is NaN never compares equal, so a view whose transform went bad would
//! emit forever. Comparison is on the bit patterns rather than the values, which makes NaN equal
//! to itself and keeps the gate closed. A camera that is NaN is a bug worth finding, but it is
//! not a reason to abandon the traffic guarantee while looking for it.

use alloc::collections::BTreeMap;

use tessella_capture_abi::envelope::ViewId;

/// The camera fields the gate compares.
///
/// Only the fields a `CameraUpdate` carries: anything else could change without the stream
/// noticing, and anything omitted here would let a real change slip past.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CameraKey {
    /// Map center at zoom zero, scale-free (§2.2).
    pub center_zoom0: [f64; 2],
    /// Fractional zoom.
    pub zoom: f64,
    /// Bearing in degrees.
    pub bearing: f64,
    /// Pitch in degrees.
    pub pitch: f64,
    /// World pixels per meter.
    pub pixels_per_meter: f64,
}

impl CameraKey {
    /// True when every field is bit-identical.
    ///
    /// Bit patterns rather than values, so that NaN equals itself. A NaN camera is a bug, but a
    /// gate that reopened forever because of one would turn that bug into unbounded traffic.
    #[must_use]
    pub fn same_as(&self, other: &Self) -> bool {
        let fields = |key: &Self| {
            [
                key.center_zoom0[0].to_bits(),
                key.center_zoom0[1].to_bits(),
                key.zoom.to_bits(),
                key.bearing.to_bits(),
                key.pitch.to_bits(),
                key.pixels_per_meter.to_bits(),
            ]
        };
        fields(self) == fields(other)
    }
}

/// What a view wants done this frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Work {
    /// Emit a camera block: the transform changed.
    pub camera: bool,
    /// Emit geometry: a source reported churn.
    pub geometry: bool,
}

impl Work {
    /// True when there is nothing to do at all — the parked case.
    #[must_use]
    pub const fn is_idle(&self) -> bool {
        !self.camera && !self.geometry
    }
}

/// Per-view damage state.
#[derive(Debug, Default)]
struct ViewState {
    camera: Option<CameraKey>,
    /// Set when a source reported churn and cleared when the geometry is emitted. Kept rather
    /// than passed through, because churn is reported when a tile lands and consumed when the
    /// next frame runs, and those are not the same moment.
    dirty: bool,
    /// True once anything at all has been emitted for this view.
    started: bool,
}

/// Tracks what has changed per view.
#[derive(Debug, Default)]
pub struct DamageTracker {
    views: BTreeMap<u32, ViewState>,
}

impl DamageTracker {
    /// An empty tracker.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Records that a source contributed churn to a view.
    pub fn mark_dirty(&mut self, view: ViewId) {
        self.views.entry(view.0).or_default().dirty = true;
    }

    /// Decides what a view needs to emit, and records the decision.
    ///
    /// A view's first frame always emits both: nothing has been sent, so nothing can be
    /// unchanged. After that, the camera emits only when it differs and geometry only when a
    /// source has reported churn.
    pub fn begin_frame(&mut self, view: ViewId, camera: CameraKey) -> Work {
        let state = self.views.entry(view.0).or_default();

        let camera_changed = match &state.camera {
            None => true,
            Some(previous) => !previous.same_as(&camera),
        };
        let geometry = state.dirty || !state.started;

        state.camera = Some(camera);
        state.dirty = false;
        state.started = true;

        Work {
            camera: camera_changed,
            geometry,
        }
    }

    /// Forgets a view, for one that has been undeclared.
    pub fn forget(&mut self, view: ViewId) {
        self.views.remove(&view.0);
    }

    /// How many views are being tracked.
    #[must_use]
    pub fn len(&self) -> usize {
        self.views.len()
    }

    /// True when no view is tracked.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.views.is_empty()
    }
}

/// Ring traffic over a span of frames, for the §9.3 assertions.
///
/// Bytes are measured from the ring's own head counter rather than by adding up what a producer
/// believes it wrote. The two differ exactly when something emits without meaning to, which is
/// the thing being measured.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Traffic {
    /// Ring bytes written.
    pub bytes: u64,
    /// Frames counted.
    pub frames: u64,
}

impl Traffic {
    /// True when nothing at all was written.
    ///
    /// This is DR-8's guarantee in one call: a parked view over any number of frames.
    #[must_use]
    pub const fn is_silent(&self) -> bool {
        self.bytes == 0
    }
}

/// Measures ring traffic across frames.
#[derive(Debug)]
pub struct TrafficMeter {
    start: u64,
    frames: u64,
}

impl TrafficMeter {
    /// Starts measuring from the producer's current position.
    #[must_use]
    pub fn new(head: u64) -> Self {
        Self {
            start: head,
            frames: 0,
        }
    }

    /// Counts a frame.
    pub fn frame(&mut self) {
        self.frames += 1;
    }

    /// Traffic since the meter started.
    #[must_use]
    pub fn traffic(&self, head: u64) -> Traffic {
        Traffic {
            bytes: head.saturating_sub(self.start),
            frames: self.frames,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn camera() -> CameraKey {
        CameraKey {
            center_zoom0: [255.843_555_555_555_55, 170.258_551_462_728_78],
            zoom: 13.0,
            bearing: 0.0,
            pitch: 0.0,
            pixels_per_meter: 1.0,
        }
    }

    const VIEW: ViewId = ViewId(0);

    /// DR-8's guarantee: a view that has emitted once and then parks emits nothing more, for
    /// any number of frames.
    #[test]
    fn a_parked_view_goes_silent() {
        let mut tracker = DamageTracker::new();

        let first = tracker.begin_frame(VIEW, camera());
        assert!(first.camera && first.geometry, "the first frame emits both");

        for frame in 0..1000 {
            let work = tracker.begin_frame(VIEW, camera());
            assert!(work.is_idle(), "frame {frame} should be silent: {work:?}");
        }
    }

    /// Pure camera motion emits the camera and nothing else — no geometry, however far it
    /// moves, because moving the camera does not change what a tile contains.
    #[test]
    fn camera_motion_emits_the_camera_only() {
        let mut tracker = DamageTracker::new();
        tracker.begin_frame(VIEW, camera());

        let mut moved = camera();
        moved.zoom = 13.5;
        let work = tracker.begin_frame(VIEW, moved);
        assert!(work.camera);
        assert!(!work.geometry);

        // And it settles again once the camera stops.
        assert!(tracker.begin_frame(VIEW, moved).is_idle());
    }

    #[test]
    fn churn_emits_geometry_once() {
        let mut tracker = DamageTracker::new();
        tracker.begin_frame(VIEW, camera());
        assert!(tracker.begin_frame(VIEW, camera()).is_idle());

        tracker.mark_dirty(VIEW);
        let work = tracker.begin_frame(VIEW, camera());
        assert!(work.geometry, "the tile that landed");
        assert!(!work.camera, "which did not move the camera");

        // Consumed, not sticky. A dirty flag that stayed set would emit the same geometry
        // every frame, which is the AttributesModified storm §6.1 calls a visible bug.
        assert!(tracker.begin_frame(VIEW, camera()).is_idle());
    }

    /// The comparison is exact. A change too small to see is still a change, and letting a
    /// tolerance swallow it would let the camera drift indefinitely while reporting itself
    /// unchanged.
    #[test]
    fn the_smallest_possible_change_reopens_the_gate() {
        let mut tracker = DamageTracker::new();
        tracker.begin_frame(VIEW, camera());

        let mut nudged = camera();
        nudged.zoom = f64::from_bits(nudged.zoom.to_bits() + 1);
        assert_ne!(nudged.zoom, camera().zoom);

        let work = tracker.begin_frame(VIEW, nudged);
        assert!(work.camera, "one ULP is a change");
    }

    /// This is what a projection disagreeing with the oracle by one ULP would cost: a parked
    /// map emitting a camera block every frame, forever, and the guarantee failing as traffic
    /// that never stops rather than as anything visible.
    #[test]
    fn a_camera_that_drifts_by_one_ulp_never_goes_silent() {
        let mut tracker = DamageTracker::new();
        let mut drifting = camera();
        tracker.begin_frame(VIEW, drifting);

        for _ in 0..10 {
            drifting.center_zoom0[1] = f64::from_bits(drifting.center_zoom0[1].to_bits() + 1);
            assert!(
                tracker.begin_frame(VIEW, drifting).camera,
                "drift keeps the gate open"
            );
        }
    }

    /// NaN is not equal to itself, so a value comparison would reopen the gate forever. A NaN
    /// camera is a bug, but it must not also become unbounded traffic.
    #[test]
    fn a_nan_camera_still_goes_silent() {
        let mut tracker = DamageTracker::new();
        let mut broken = camera();
        broken.pitch = f64::NAN;

        tracker.begin_frame(VIEW, broken);
        let work = tracker.begin_frame(VIEW, broken);
        assert!(work.is_idle(), "NaN must compare equal to itself here");
    }

    /// Zero and negative zero compare equal as values but differ in bits. Treating them as a
    /// change costs one camera block on the frame a value crosses zero, which is a real change
    /// in sign that a consumer may care about; treating them as equal would need a value
    /// comparison, which breaks NaN. This pins which way it goes.
    #[test]
    fn negative_zero_counts_as_a_change() {
        let mut tracker = DamageTracker::new();
        let mut positive = camera();
        positive.bearing = 0.0;
        tracker.begin_frame(VIEW, positive);

        let mut negative = positive;
        negative.bearing = -0.0;
        assert!(
            tracker.begin_frame(VIEW, negative).camera,
            "bitwise comparison distinguishes them"
        );
    }

    #[test]
    fn views_are_tracked_independently() {
        let mut tracker = DamageTracker::new();
        let other = ViewId(1);

        tracker.begin_frame(VIEW, camera());
        tracker.begin_frame(other, camera());
        assert!(tracker.begin_frame(VIEW, camera()).is_idle());

        // Churn in one view does not wake the other.
        tracker.mark_dirty(other);
        assert!(tracker.begin_frame(VIEW, camera()).is_idle());
        assert!(tracker.begin_frame(other, camera()).geometry);
        assert_eq!(tracker.len(), 2);

        tracker.forget(other);
        assert_eq!(tracker.len(), 1);
    }

    /// A forgotten view starts over rather than resuming, because a consumer that undeclared it
    /// has dropped everything scoped to it.
    #[test]
    fn a_forgotten_view_starts_over() {
        let mut tracker = DamageTracker::new();
        tracker.begin_frame(VIEW, camera());
        assert!(tracker.begin_frame(VIEW, camera()).is_idle());

        tracker.forget(VIEW);
        let work = tracker.begin_frame(VIEW, camera());
        assert!(work.camera && work.geometry, "nothing has been sent to it");
    }

    #[test]
    fn traffic_measures_from_the_rings_own_counter() {
        let mut meter = TrafficMeter::new(1000);
        meter.frame();
        meter.frame();

        assert!(meter.traffic(1000).is_silent());
        assert_eq!(meter.traffic(1000).frames, 2);
        assert_eq!(meter.traffic(1064).bytes, 64);
    }
}
