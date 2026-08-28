//! Wakeups, and the shape of them (§12.8, R4).
//!
//! # Why bytes were not enough
//!
//! §9.3's counters are of what reaches the wire, and `parked_is_silent` asserts the strongest
//! form of that: a parked view costs exactly one camera block a frame and wants no geometry.
//! What neither says is whether the producer *woke up* to discover it had nothing to send.
//!
//! A DVFS governor reads utilisation, not usefulness. A producer busy a little of every tick
//! holds the part at a middling frequency for as long as it runs, which costs more than the same
//! work done in bursts with the part idle between them. §12.8 says so — "sustained-idle-then-
//! burst beats constant medium load" — and until now nothing measured it.

use tessella_orchestrate::pacing::{Demand, Idle, Pacer, Tick};

/// A tick with nothing to send.
fn quiet(now_ns: u64) -> Demand {
    Demand {
        changed: false,
        outstanding: 0,
        now_ns,
    }
}

/// A tick with something to send, and `outstanding` bytes the consumer has not drained.
fn busy(now_ns: u64, outstanding: u64) -> Demand {
    Demand {
        changed: true,
        outstanding,
        now_ns,
    }
}

/// A parked view builds nothing, not merely sends nothing.
#[test]
fn a_parked_view_does_not_wake_up_to_do_nothing() {
    let mut pacer = Pacer::default();
    for tick in 0..120 {
        assert_eq!(
            pacer.tick(&quiet(tick * 16_666_667)),
            Tick::Skip(Idle::Parked)
        );
    }

    let pacing = *pacer.pacing();
    assert_eq!(pacing.wakeups, 120);
    assert_eq!(
        pacing.emitted, 0,
        "two seconds of a still map built no frame"
    );
    assert_eq!(pacing.parked, 120);
    assert_eq!(
        pacing.longest_idle_run, 120,
        "and it was one idle run rather than a hundred and twenty short ones"
    );
    assert!(pacing.is_bursty(), "nothing to clump is not a dribble");
}

/// A producer does not get ahead of a consumer that is still draining.
#[test]
fn production_follows_consumption_rather_than_the_loop() {
    let mut pacer = Pacer::default();

    // The consumer is keeping up: every tick has something to send and nothing outstanding.
    for tick in 0..10 {
        assert_eq!(pacer.tick(&busy(tick * 1_000_000, 0)), Tick::Emit);
    }
    assert_eq!(pacer.pacing().emitted, 10);

    // Now it stalls, within the latency budget. The producer holds rather than piling frames
    // in behind the one being drawn — they would be drawn first, and the newest delayed by
    // exactly the ones that need not have been sent.
    let stalled_at = 10_000_000;
    for tick in 0..8 {
        assert_eq!(
            pacer.tick(&busy(stalled_at + tick * 1_000_000, 4096)),
            Tick::Skip(Idle::Draining)
        );
    }
    assert_eq!(
        pacer.pacing().emitted,
        10,
        "nothing was produced into the stall"
    );
}

/// A stalled consumer makes the map update less often, not stop.
///
/// The bound is what separates pacing from blocking. Past it the change goes anyway and the
/// ring's own backpressure takes over — which at least fails loudly, where a held change is
/// a map that is quietly wrong.
#[test]
fn a_held_change_goes_once_it_has_waited_long_enough() {
    let budget = 16_666_667;
    let mut pacer = Pacer::new(budget);

    assert_eq!(pacer.tick(&busy(0, 4096)), Tick::Skip(Idle::Draining));
    assert_eq!(
        pacer.tick(&busy(budget - 1, 4096)),
        Tick::Skip(Idle::Draining),
        "just inside the budget"
    );
    assert_eq!(
        pacer.tick(&busy(budget, 4096)),
        Tick::Emit,
        "and out the other side of it"
    );
}

/// A quiet tick does not forget a change that is already waiting.
///
/// The case is a consumer that stalled while something was pending and then a tick where
/// nothing further changed: the pending change is still pending, and restarting its clock would
/// let a slow consumer defer it indefinitely by being quiet at the right moments.
#[test]
fn a_quiet_tick_does_not_restart_the_clock() {
    let budget = 16_666_667;
    let mut pacer = Pacer::new(budget);

    assert_eq!(pacer.tick(&busy(0, 4096)), Tick::Skip(Idle::Draining));
    for tick in 1..8 {
        assert_eq!(
            pacer.tick(&quiet(tick * 1_000_000)),
            Tick::Skip(Idle::Parked)
        );
    }
    assert_eq!(
        pacer.tick(&busy(budget, 4096)),
        Tick::Emit,
        "the wait is measured from when the change appeared, not from the last busy tick"
    );
}

/// Sparse changes come out as bursts with the part idle between them.
#[test]
fn sparse_changes_clump() {
    let mut pacer = Pacer::new(0);

    // A tick every sixtieth of a second for two seconds, with a run of work every half second.
    for tick in 0..120u64 {
        let working = (tick / 30) % 2 == 1;
        pacer.tick(&busy(tick * 16_666_667, 0).changed_to(working));
    }

    let pacing = *pacer.pacing();
    assert_eq!(pacing.wakeups, 120);
    assert_eq!(pacing.emitted, 60);
    assert!(
        pacing.is_bursty(),
        "the emissions clumped rather than dribbling: {pacing:?}"
    );
    assert_eq!(
        pacing.longest_burst, 30,
        "a full run of work went out as one burst"
    );
    assert_eq!(
        pacing.longest_idle_run, 30,
        "and the part was left alone for the whole of the gap"
    );
    assert_eq!(pacing.bursts, 2);
}

/// A dribble is visible as one however many ticks it lasts.
#[test]
fn a_dribble_is_not_a_burst() {
    let mut pacer = Pacer::new(0);
    for tick in 0..120u64 {
        pacer.tick(&busy(tick * 16_666_667, 0).changed_to(tick % 2 == 0));
    }

    let pacing = *pacer.pacing();
    assert_eq!(pacing.emitted, 60, "the same work as the burst case");
    assert_eq!(pacing.longest_burst, 1);
    assert!(
        !pacing.is_bursty(),
        "busy a little of every other tick is exactly the load §12.8 says costs most"
    );
}

/// Test-local sugar: the same tick with a different answer to "did anything change".
trait ChangedTo {
    fn changed_to(self, changed: bool) -> Self;
}

impl ChangedTo for Demand {
    fn changed_to(mut self, changed: bool) -> Self {
        self.changed = changed;
        self
    }
}

/// Against a real ring and a consumer that drains slowly.
///
/// The state machine above is only worth having if it is reading the right numbers, and the
/// consumption rate is not a number anything reports directly: it is the ring's own `tail`,
/// which the consumer publishes and the producer reads. `Demand::from_ring` is that reading,
/// and this is it working — same scene, same lazy consumer, with the pacer and without.
mod against_a_ring {
    use super::*;
    use tessella_capture_abi::envelope::ViewId;
    use tessella_capture_abi::ring::Ring;
    use tessella_orchestrate::SlabArena;
    use tessella_orchestrate::frame::{self, Frame};
    use tessella_orchestrate::registry::Session;
    use tessella_orchestrate::tile::{LayerBucket, TileId, build_mvt_tile, build_sourceless};
    use tessella_source::mvt::Tile;
    use tessella_style::Style;
    use tessella_style::light::Light;
    use tessella_tile::camera;
    use tessella_tile::cover::{self, ViewTransform};

    const REAL_TILE: &[u8] = include_bytes!("../../../tests/mvt-fixtures/real-world-0-0-0.mvt");

    const STYLE: &str = r##"{
      "version": 8,
      "sources": {"src": {"type": "vector", "tiles": []}},
      "layers": [
        {"id": "bg", "type": "background", "paint": {"background-color": "#101418"}},
        {"id": "sea", "type": "fill", "source": "src", "source-layer": "water",
         "paint": {"fill-color": "#20344c"}},
        {"id": "banks", "type": "line", "source": "src", "source-layer": "water",
         "paint": {"line-color": "#88a", "line-width": 1.5}}
      ]
    }"##;

    /// Small enough that a producer running at loop speed fills it.
    const CAPACITY: usize = 1 << 16;
    /// How often the consumer gets round to draining, in ticks.
    ///
    /// The ring has to hold the largest single frame however it is paced — that is §4's
    /// high-water mark and not something pacing can help with — so the two runs are separated by
    /// how many frames pile up behind a lazy consumer, not by whether one fits.
    const DRAIN_EVERY: u64 = 16;
    /// Ticks at sixty hertz.
    const TICK_NS: u64 = 16_666_667;

    struct Scene {
        style: Style,
        view: ViewTransform,
        tiles: Vec<cover::TileCoord>,
        buckets: Vec<(TileId, Vec<LayerBucket>)>,
    }

    fn scene(longitude: f64) -> Scene {
        let style = Style::parse(STYLE).expect("the style parses");
        let view = camera::settled(&ViewTransform {
            longitude,
            latitude: 0.0,
            zoom: 3.0,
            width: 512.0,
            height: 512.0,
            bearing: 0.0,
            pitch: 0.0,
        });
        let tiles = cover::cover(&view).expect("covers");
        let decoded = Tile::decode(REAL_TILE).expect("the fixture decodes");
        let mut buckets = Vec::new();
        for tile in &tiles {
            let id = TileId::new(tile.z, tile.x, tile.y);
            let mut built = build_mvt_tile(&style, "src", id, &decoded).expect("the tile builds");
            built.extend(build_sourceless(&style, id).expect("the background builds"));
            built.sort_by_key(|bucket| bucket.layer_index);
            buckets.push((id, built));
        }
        Scene {
            style,
            view,
            tiles,
            buckets,
        }
    }

    /// Runs sixty ticks of a moving map against a consumer that drains every fourth one.
    ///
    /// Returns how many frames were refused by a full ring, and the peak occupancy.
    fn run(paced: bool) -> (u64, usize, u64) {
        let mut ring = Ring::new(CAPACITY);
        let (producer, consumer) = ring.split();
        let mut arena = SlabArena::new();
        let mut session = Session::new();
        // One drain interval. Set shorter, the budget would force a frame out before the
        // consumer had caught up and the ring would fill anyway — which is the design working,
        // and would be measuring the budget rather than the policy.
        let mut pacer = Pacer::new(TICK_NS * DRAIN_EVERY);
        let mut refused = 0;
        let mut peak = 0;
        let mut emitted = 0;

        for tick in 0..60u64 {
            let now = tick * TICK_NS;
            // A moving map: every tick has something to send.
            if paced {
                let demand = Demand::from_ring(producer, true, now);
                if pacer.tick(&demand) != Tick::Emit {
                    continue;
                }
            }
            // A camera that keeps moving rather than circling a handful of positions: a cover
            // it has already shown costs nothing to show again, and a run of those would be
            // measuring the registry rather than the pacing.
            #[allow(clippy::cast_precision_loss)]
            let scene = scene((tick * 29 % 360) as f64 - 180.0);
            let attempt = frame::emit_incremental(
                producer,
                &mut arena,
                &Frame {
                    style: &scene.style,
                    view: &scene.view,
                    view_id: ViewId(0),
                    tiles: &scene.tiles,
                    buckets: &scene.buckets,
                    light: &Light::default(),
                    fonts: None,
                    patterns: None,
                },
                &mut session,
            );
            match attempt {
                Ok(_) => emitted += 1,
                Err(_) => refused += 1,
            }
            peak = peak.max(consumer.occupancy());

            // The consumer gets round to it every fourth tick.
            if tick % DRAIN_EVERY == DRAIN_EVERY - 1 {
                while let Some(record) = consumer.peek() {
                    let consumed = record.consumed();
                    consumer.advance(consumed);
                }
            }
        }
        (refused, peak, emitted)
    }

    /// A producer at loop speed fills the ring; one at consumption rate does not.
    #[test]
    fn pacing_is_what_keeps_the_ring_off_its_bound() {
        let (unpaced_refusals, unpaced_peak, _) = run(false);
        let (paced_refusals, paced_peak, paced_frames) = run(true);

        assert!(
            unpaced_refusals > 0,
            "the unpaced run was supposed to hit the bound; a ring this size against a consumer \
             this lazy should fill, and if it does not the comparison below says nothing"
        );
        assert_eq!(
            paced_refusals, 0,
            "and the paced run never did: it produced when the consumer had caught up, or when \
             a change had waited longer than the budget, and never merely because a tick came"
        );
        assert!(
            paced_peak < unpaced_peak,
            "and never held more than the frame it had just sent: {paced_peak} against \
             {unpaced_peak}, of a {CAPACITY}-byte ring"
        );
        assert!(
            paced_frames >= 60 / DRAIN_EVERY,
            "with the map still updating about once per drain: {paced_frames} frames"
        );
    }
}
