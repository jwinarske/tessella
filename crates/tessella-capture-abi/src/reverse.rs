//! Reverse channel: consumer to producer (DR-10, §11.4).
//!
//! The ring is one-way. This is the small strip going the other way — no queue, no history,
//! just current state the producer polls when it wants it.
//!
//! Three things travel here, and each exists because the producer cannot get it any other way:
//!
//! - **The camera of a consumer-camera view.** Under DR-9 the consumer's camera is
//!   authoritative for interactive views, and the ring drops out of the interactive path
//!   entirely. But the producer still needs the camera for cover, placement, and screen-space
//!   uniforms, so it reads back a one-frame-stale copy (§11.1). Cover has padding, placement is
//!   throttled, and a screen-space width lagging one frame is imperceptible — R-8 tracks what
//!   that staleness costs.
//! - **Viewport and visibility per view.** A view whose slot reports hidden gets cover
//!   maintenance and nothing else: no placement, no emission. Gating at the source beats
//!   producing work the consumer discards (§11.4).
//! - **Acknowledged geometry.** §13.2 retains ancestors until every covering descendant is
//!   consumer-*acknowledged*, not merely built. mbgl retains until built, and the gap between
//!   build and GPU upload is exactly where its single-frame holes come from.
//!
//! Pacing (§11.4) needs no field here: the ring's own tail already tells the producer how far
//! the consumer has got, and duplicating it would be a second source of truth for one fact.
//!
//! # Why a seqlock
//!
//! A camera is five doubles and a viewport, which no single atomic covers. A reader that
//! copied the fields one at a time while the consumer wrote them would get a camera that never
//! existed — half of one frame's and half of the next's. At high zoom that is the §6.3
//! flicker bug wearing a different hat.
//!
//! A seqlock fits the shape exactly: one writer, readers that can retry, and a payload small
//! enough that retrying is cheap. The writer makes the sequence odd, writes, then makes it even
//! again; a reader that sees an odd sequence, or a different one after reading, tries again.
//!
//! The payload fields are atomics rather than plain `f64`s even though the sequence number
//! already orders them. This is not belt-and-braces: a seqlock whose payload is read
//! non-atomically while the writer writes it is a data race, and a data race is undefined
//! behavior in Rust's model regardless of whether the value is later discarded. Relaxed atomic
//! loads make the same access well-defined, compile to the same instructions on every target
//! here, and let Miri check the whole thing.

use core::sync::atomic::{AtomicU32, AtomicU64, Ordering, fence};

use crate::envelope::{Extent, ViewId};

/// Number of view slots the strip carries.
///
/// §13 budgets against four simultaneous views. Eight leaves room without making the strip
/// large — the whole structure is well under a page.
pub const MAX_VIEWS: usize = 8;

/// A camera as the consumer holds it, for a view running in consumer-camera mode.
///
/// `center_zoom0` keeps the same convention the forward direction uses: the map center at zoom
/// zero, so 0..512 regardless of the map's zoom, scale-free on purpose (§6.3). Reading it back
/// pre-multiplied by the consumer's zoom would reintroduce precisely the coupling that made
/// whole frames come back empty while zooming.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct ConsumerCamera {
    /// Map center at zoom zero. Scale-free.
    pub center_zoom0: [f64; 2],
    /// Fractional zoom.
    pub zoom: f64,
    /// Bearing in degrees.
    pub bearing: f64,
    /// Pitch in degrees.
    pub pitch: f64,
    /// Viewport in pixels.
    pub viewport: Extent,
}

/// Per-view state the consumer publishes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ViewStatus {
    /// False when the view is not on screen. A hidden view gets cover maintenance only.
    pub visible: bool,
    /// Non-zero when the slot has ever been published to. An unpublished slot is not a view
    /// that is hidden; it is a view the consumer has never mentioned, and the producer must
    /// not read a camera out of it.
    pub published: bool,
}

/// Set once the consumer has published anything to a slot.
///
/// An unpublished slot is not a hidden view; it is a view the consumer has never mentioned, and
/// the producer must not read a camera out of it.
pub const FLAG_PUBLISHED: u32 = 1 << 0;

/// Set while the view is on screen. A hidden view gets cover maintenance only.
pub const FLAG_VISIBLE: u32 = 1 << 1;

/// One view's slot.
///
/// Public, and its fields with it, for the reason [`crate::ring::RingControl`]'s are: a mirror
/// on the other side of the ABI writes this region, and the orderings that make the seqlock work
/// are a protocol obligation the type cannot express across a language boundary. Rust callers
/// should go through [`ReverseChannel`]'s methods, which implement that discipline; the fields
/// are exposed so the C header can be generated from them and so a consumer can be written
/// against the same definition rather than a transcription of it.
#[derive(Debug, Default)]
#[repr(C)]
pub struct ViewSlot {
    /// Seqlock counter. Even is stable, odd means a write is in progress.
    ///
    /// The writer increments to odd, fences, writes the payload, then stores even with release.
    /// A reader that sees an odd value, or a different value before and after, must retry.
    pub seq: AtomicU32,
    /// [`FLAG_PUBLISHED`] and [`FLAG_VISIBLE`].
    pub flags: AtomicU32,
    /// Map center at zoom zero, x, as `f64` bits.
    pub center_x: AtomicU64,
    /// Map center at zoom zero, y, as `f64` bits.
    pub center_y: AtomicU64,
    /// Fractional zoom, as `f64` bits.
    pub zoom: AtomicU64,
    /// Bearing in degrees, as `f64` bits.
    pub bearing: AtomicU64,
    /// Pitch in degrees, as `f64` bits.
    pub pitch: AtomicU64,
    /// Viewport width in pixels.
    pub viewport_width: AtomicU32,
    /// Viewport height in pixels.
    pub viewport_height: AtomicU32,
}

/// The consumer-to-producer strip.
///
/// Lives in the shared region alongside the ring. Both halves borrow it immutably: every field
/// is an atomic, and which side writes which field is a protocol rule rather than a type-system
/// one.
#[derive(Debug, Default)]
#[repr(C)]
pub struct ReverseChannel {
    /// Ring position whose geometry the consumer has uploaded to the GPU.
    ///
    /// Distinct from the ring's `tail`, which only says the bytes were read. §13.2's
    /// never-blank rule needs the stronger fact: an ancestor tile may be released once its
    /// descendants are on the GPU, not once their envelopes were parsed.
    pub acked_geometry: AtomicU64,
    /// One slot per view, indexed by view id. Slots beyond the declared views stay unpublished.
    pub views: [ViewSlot; MAX_VIEWS],
}

const _: () = {
    assert!(align_of::<ReverseChannel>() == 8);
    // Slots are read by the producer while the consumer writes them, so their layout is
    // protocol; a size change means the two sides disagree about where slot N starts.
    assert!(size_of::<ViewSlot>() == 56);
};

impl ReverseChannel {
    /// A strip with nothing published.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    fn slot(&self, view: ViewId) -> Option<&ViewSlot> {
        self.views.get(view.0 as usize)
    }

    // --- consumer side ---

    /// Publishes a view's camera and viewport.
    ///
    /// Only the consumer calls this, and only one thread of it. Two writers would both make the
    /// sequence odd and a reader could see a stable even count over a half-written payload.
    ///
    /// Ignores a view id past [`MAX_VIEWS`] rather than failing: the consumer cannot invent
    /// views the producer never declared, so an out-of-range id is a producer bug that will
    /// surface as a missing camera, not something the consumer can act on.
    pub fn publish_camera(&self, view: ViewId, camera: &ConsumerCamera) {
        let Some(slot) = self.slot(view) else {
            return;
        };

        let seq = slot.seq.load(Ordering::Relaxed);
        // Odd: a reader arriving now knows the payload is in flux.
        slot.seq.store(seq.wrapping_add(1), Ordering::Relaxed);
        // The fence is what makes the odd marker visible before the payload stores it guards.
        // A release store on the line above would not: releasing orders earlier accesses
        // before the store, not later stores after it, so the payload could be observed while
        // the sequence still looked even. Miri caught exactly that.
        fence(Ordering::Release);

        slot.center_x
            .store(camera.center_zoom0[0].to_bits(), Ordering::Relaxed);
        slot.center_y
            .store(camera.center_zoom0[1].to_bits(), Ordering::Relaxed);
        slot.zoom.store(camera.zoom.to_bits(), Ordering::Relaxed);
        slot.bearing
            .store(camera.bearing.to_bits(), Ordering::Relaxed);
        slot.pitch.store(camera.pitch.to_bits(), Ordering::Relaxed);
        slot.viewport_width
            .store(camera.viewport.width, Ordering::Relaxed);
        slot.viewport_height
            .store(camera.viewport.height, Ordering::Relaxed);
        slot.flags.fetch_or(FLAG_PUBLISHED, Ordering::Relaxed);

        // Even again, and release so a reader that sees this count also sees the payload.
        slot.seq.store(seq.wrapping_add(2), Ordering::Release);
    }

    /// Sets whether a view is on screen.
    ///
    /// Outside the seqlock: it is one word, and a producer that reads visibility a frame late
    /// wastes at most one frame of work on a view that just went away.
    pub fn set_visible(&self, view: ViewId, visible: bool) {
        let Some(slot) = self.slot(view) else {
            return;
        };
        if visible {
            slot.flags
                .fetch_or(FLAG_VISIBLE | FLAG_PUBLISHED, Ordering::Relaxed);
        } else {
            slot.flags.fetch_and(!FLAG_VISIBLE, Ordering::Relaxed);
            slot.flags.fetch_or(FLAG_PUBLISHED, Ordering::Relaxed);
        }
    }

    /// Reports that geometry up to `position` is on the GPU.
    ///
    /// `position` is a ring head value. Monotonic: this takes the maximum rather than the
    /// argument, so an out-of-order or stale acknowledgement cannot walk the value backwards
    /// and hand the producer permission to release a tile whose replacement is not up yet.
    pub fn ack_geometry(&self, position: u64) {
        self.acked_geometry.fetch_max(position, Ordering::Release);
    }

    // --- producer side ---

    /// The ring position whose geometry the consumer has uploaded.
    #[must_use]
    pub fn acked_geometry(&self) -> u64 {
        self.acked_geometry.load(Ordering::Acquire)
    }

    /// Reads back a view's camera, or `None` if the consumer has never published one.
    ///
    /// Retries while the consumer is mid-write. The value is one frame stale by construction
    /// (§11.1); that is the trade DR-9 makes to take the ring out of the interactive path.
    #[must_use]
    pub fn camera(&self, view: ViewId) -> Option<ConsumerCamera> {
        let slot = self.slot(view)?;
        loop {
            let before = slot.seq.load(Ordering::Acquire);
            if before & 1 != 0 {
                // A write is in progress; nothing read now would be coherent.
                core::hint::spin_loop();
                continue;
            }
            if slot.flags.load(Ordering::Relaxed) & FLAG_PUBLISHED == 0 {
                return None;
            }

            let camera = ConsumerCamera {
                center_zoom0: [
                    f64::from_bits(slot.center_x.load(Ordering::Relaxed)),
                    f64::from_bits(slot.center_y.load(Ordering::Relaxed)),
                ],
                zoom: f64::from_bits(slot.zoom.load(Ordering::Relaxed)),
                bearing: f64::from_bits(slot.bearing.load(Ordering::Relaxed)),
                pitch: f64::from_bits(slot.pitch.load(Ordering::Relaxed)),
                viewport: Extent {
                    width: slot.viewport_width.load(Ordering::Relaxed),
                    height: slot.viewport_height.load(Ordering::Relaxed),
                },
            };

            // Pairs with the writer's release fence: the payload loads above cannot be
            // reordered after the sequence load below, so an unchanged sequence really does
            // mean nothing was written while we copied.
            fence(Ordering::Acquire);
            if slot.seq.load(Ordering::Relaxed) == before {
                return Some(camera);
            }
        }
    }

    /// Reads a view's visibility.
    #[must_use]
    pub fn status(&self, view: ViewId) -> ViewStatus {
        let Some(slot) = self.slot(view) else {
            return ViewStatus::default();
        };
        let flags = slot.flags.load(Ordering::Relaxed);
        ViewStatus {
            visible: flags & FLAG_VISIBLE != 0,
            published: flags & FLAG_PUBLISHED != 0,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Builds a camera whose every field is derived from one counter, so a reader can tell
    /// whether the fields it got belong together.
    fn generation(n: u64) -> ConsumerCamera {
        let n = n as f64;
        ConsumerCamera {
            center_zoom0: [n, n + 1.0],
            zoom: n + 2.0,
            bearing: n + 3.0,
            pitch: n + 4.0,
            viewport: Extent {
                width: n as u32,
                height: n as u32 + 1,
            },
        }
    }

    /// True when every field belongs to the same generation.
    fn is_coherent(camera: &ConsumerCamera) -> bool {
        let n = camera.center_zoom0[0];
        camera.center_zoom0[1] == n + 1.0
            && camera.zoom == n + 2.0
            && camera.bearing == n + 3.0
            && camera.pitch == n + 4.0
            && f64::from(camera.viewport.width) == n
            && f64::from(camera.viewport.height) == n + 1.0
    }

    #[test]
    fn an_unpublished_slot_has_no_camera() {
        let channel = ReverseChannel::new();
        assert_eq!(channel.camera(ViewId(0)), None);
        assert_eq!(
            channel.status(ViewId(0)),
            ViewStatus {
                visible: false,
                published: false
            }
        );
    }

    /// An unpublished slot and a hidden view are different facts. Conflating them would let a
    /// producer read a zeroed camera out of a view the consumer has never mentioned and place
    /// tiles at null island.
    #[test]
    fn hidden_is_not_the_same_as_unpublished() {
        let channel = ReverseChannel::new();
        channel.set_visible(ViewId(0), false);
        let status = channel.status(ViewId(0));
        assert!(status.published, "the consumer has now mentioned this view");
        assert!(!status.visible);
    }

    #[test]
    fn publishes_and_reads_back_a_camera() {
        let channel = ReverseChannel::new();
        let camera = generation(7);
        channel.publish_camera(ViewId(2), &camera);

        assert_eq!(channel.camera(ViewId(2)), Some(camera));
        assert_eq!(channel.camera(ViewId(1)), None, "slots are independent");
    }

    #[test]
    fn view_ids_past_the_end_are_ignored_rather_than_panicking() {
        let channel = ReverseChannel::new();
        let out_of_range = ViewId(MAX_VIEWS as u32);

        channel.publish_camera(out_of_range, &generation(1));
        channel.set_visible(out_of_range, true);

        assert_eq!(channel.camera(out_of_range), None);
        assert_eq!(channel.status(out_of_range), ViewStatus::default());
    }

    #[test]
    fn visibility_toggles_without_disturbing_the_camera() {
        let channel = ReverseChannel::new();
        let camera = generation(3);
        channel.publish_camera(ViewId(0), &camera);

        channel.set_visible(ViewId(0), true);
        assert!(channel.status(ViewId(0)).visible);
        assert_eq!(channel.camera(ViewId(0)), Some(camera));

        channel.set_visible(ViewId(0), false);
        assert!(!channel.status(ViewId(0)).visible);
        assert_eq!(
            channel.camera(ViewId(0)),
            Some(camera),
            "going hidden does not discard the camera"
        );
    }

    /// The acknowledgement must never walk backwards. §13.2 releases an ancestor tile on the
    /// strength of this value, so a stale acknowledgement lowering it would hand the producer
    /// permission to drop a tile whose replacement is not on the GPU yet — a hole in the map.
    #[test]
    fn geometry_acknowledgement_only_advances() {
        let channel = ReverseChannel::new();
        assert_eq!(channel.acked_geometry(), 0);

        channel.ack_geometry(100);
        assert_eq!(channel.acked_geometry(), 100);

        channel.ack_geometry(50);
        assert_eq!(channel.acked_geometry(), 100, "a stale ack changes nothing");

        channel.ack_geometry(150);
        assert_eq!(channel.acked_geometry(), 150);
    }

    /// The seqlock's whole purpose: a reader must never assemble a camera from two generations.
    /// Without it the reader would get, say, one frame's center with the next frame's zoom —
    /// which at high zoom is the §6.3 flicker bug in a different place.
    #[test]
    fn concurrent_reads_never_see_a_torn_camera() {
        use std::sync::Arc;
        use std::sync::atomic::AtomicBool;

        // Miri interprets rather than executes; the tear would show in the first handful of
        // interleavings or not at all.
        const WRITES: u64 = if cfg!(miri) { 300 } else { 200_000 };

        let channel = Arc::new(ReverseChannel::new());
        let done = Arc::new(AtomicBool::new(false));

        let writer = {
            let channel = Arc::clone(&channel);
            let done = Arc::clone(&done);
            std::thread::spawn(move || {
                for n in 1..=WRITES {
                    channel.publish_camera(ViewId(0), &generation(n));
                }
                done.store(true, Ordering::Release);
            })
        };

        let reader = {
            let channel = Arc::clone(&channel);
            let done = Arc::clone(&done);
            std::thread::spawn(move || {
                let mut seen = 0u64;
                let mut highest = 0f64;
                // Until the writer is finished *and* something has been observed. The second
                // clause is what makes this not depend on scheduling: a reader that starts
                // after the writer has already set `done` would otherwise exit having read
                // nothing, and fail an assertion about its own liveness rather than about the
                // channel. It cannot spin: by the time `done` is set the channel holds a
                // camera, so the next read succeeds.
                while !done.load(Ordering::Acquire) || seen == 0 {
                    if let Some(camera) = channel.camera(ViewId(0)) {
                        assert!(is_coherent(&camera), "torn camera: {camera:?}");
                        assert!(
                            camera.center_zoom0[0] >= highest,
                            "a camera went backwards, so the sequence check let a stale \
                             payload through"
                        );
                        highest = camera.center_zoom0[0];
                        seen += 1;
                    }
                }
                seen
            })
        };

        writer.join().unwrap();
        let seen = reader.join().unwrap();

        // Liveness of the reader, not evidence that the two overlapped — a single read after
        // the writer finished would satisfy it too, and always could. What the test is *for* is
        // the coherence and monotonicity checks above, which run on every read the reader
        // manages to take while the writer is going.
        assert!(seen > 0, "the reader never observed a camera");
        assert_eq!(channel.camera(ViewId(0)), Some(generation(WRITES)));
    }
}
