//! The four-view zoom sweep (§13.3), and the part of it that runs without the board.
//!
//! # What §13.3 asks for and what is here
//!
//! The R1.5 exit criterion is a four-view synchronized zoom sweep, z8→z16→z8 continuous, on
//! RK3566, holding five properties at once: frame budget on every tick, coverage completeness,
//! zero symbol pops, bounded ring occupancy, and §9.3 flatness.
//!
//! Two of those five are pure computation and need no hardware at all. Coverage completeness is
//! a walk over the cover of each frame; flatness is a count of what the shared store built. Both
//! can run in CI on a developer's machine today, and both are *correctness* properties rather
//! than performance ones — a sweep that leaves holes or duplicates work is wrong on any machine,
//! and it is cheaper to find that out here than on a board.
//!
//! The other three genuinely need the target. Frame budget and ring occupancy are measurements
//! of a running producer against a real consumer, and symbol pops need symbols, which are R2.
//! This module is the sweep those measurements will be taken over, so the board brings a
//! stopwatch to a harness that already exists rather than a harness that has to be written under
//! time pressure at R1.5.
//!
//! # Why the views are not in the same place
//!
//! Four views at one location share everything, which makes flatness trivially true and proves
//! nothing about the case that matters. Four views at *partially* overlapping locations is the
//! real cluster-display arrangement, and it is the one where a store keyed slightly wrong
//! duplicates the overlap while looking correct. [`four_views`] places them so that neighbours
//! share and opposite corners do not.
//!
//! # Flatness is stated against the union, not against a view count
//!
//! "Four views do what one view does" is only true when all four look at the same place. The
//! general statement — the one that holds for any arrangement — is that the work equals the size
//! of the *union* of the covers. Nothing is built twice, and nothing that is needed goes
//! unbuilt. That reduces to the view-count form when the covers coincide, and unlike the
//! view-count form it still says something when they do not.

use alloc::collections::BTreeSet;
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use tessella_tile::cover::{self, Gap, TileCoord, ViewTransform};

/// The zoom range §13.3 sweeps.
pub const SWEEP_LOW: f64 = 8.0;
/// The top of the sweep.
pub const SWEEP_HIGH: f64 = 16.0;

/// The four viewports of the benchmark.
///
/// Centred near the probe so the hermetic style's features are in range, and offset so that the
/// covers overlap partially rather than coinciding. The offsets are in degrees and deliberately
/// unequal on the two axes: a symmetric arrangement can hide an error that transposes x and y,
/// because a transposed cover of a square arrangement is the same set.
#[must_use]
pub fn four_views() -> [ViewTransform; 4] {
    let base = ViewTransform {
        longitude: -0.11,
        latitude: 51.505,
        zoom: SWEEP_LOW,
        width: 1024.0,
        height: 768.0,
        bearing: 0.0,
        pitch: 0.0,
    };
    // Roughly a third of a z13 tile apart, so neighbours overlap at high zoom and the whole
    // group collapses into shared tiles as the sweep descends.
    [
        base,
        ViewTransform {
            longitude: base.longitude + 0.010,
            ..base
        },
        ViewTransform {
            latitude: base.latitude + 0.006,
            ..base
        },
        ViewTransform {
            longitude: base.longitude + 0.010,
            latitude: base.latitude + 0.006,
            ..base
        },
    ]
}

/// The zoom of each frame of a z8→z16→z8 sweep.
///
/// `steps` is the number of frames in one direction. The turn at the top is not repeated, so a
/// sweep of `steps` up is `2 * steps - 1` frames total and z16 is visited once. Repeating the
/// turn would double-count the frame where the cover is smallest and flatter the flatness
/// numbers slightly.
#[must_use]
pub fn sweep_zooms(steps: usize) -> Vec<f64> {
    let steps = steps.max(2);
    let span = SWEEP_HIGH - SWEEP_LOW;
    let up: Vec<f64> = (0..steps)
        .map(|i| SWEEP_LOW + span * (i as f64) / ((steps - 1) as f64))
        .collect();
    let mut all = up.clone();
    all.extend(up.into_iter().rev().skip(1));
    all
}

/// One frame of the sweep: what the four views wanted, and whether they got it.
#[derive(Debug, Clone, PartialEq)]
pub struct Frame {
    /// Frame index in the sweep.
    pub index: usize,
    /// The zoom every view is at. Synchronized, per §13.3.
    pub zoom: f64,
    /// The union of the four covers — the tiles the frame needs, counted once.
    pub union: usize,
    /// The sum of the four covers. Exceeds `union` exactly where the views overlap, which is
    /// the work a shared store saves and a per-view store repeats.
    pub total: usize,
    /// Points of some viewport that no tile covered. Empty is the §13.3 requirement.
    pub gaps: Vec<Gap>,
}

impl Frame {
    /// True when every view was fully covered.
    #[must_use]
    pub fn is_covered(&self) -> bool {
        self.gaps.is_empty()
    }

    /// Tiles the overlap saved this frame: what a per-view store would have done twice.
    #[must_use]
    pub fn shared(&self) -> usize {
        self.total - self.union
    }
}

/// What a sweep did.
#[derive(Debug, Clone, PartialEq)]
pub struct SweepReport {
    /// Every frame.
    pub frames: Vec<Frame>,
    /// Distinct tiles the whole sweep touched.
    pub distinct_tiles: usize,
    /// Tiles summed over every view of every frame, ignoring sharing.
    pub tile_requests: usize,
}

impl SweepReport {
    /// Frames where some view had a hole. §13.3 requires this to be empty.
    #[must_use]
    pub fn uncovered(&self) -> Vec<&Frame> {
        self.frames.iter().filter(|f| !f.is_covered()).collect()
    }

    /// The largest union any single frame needed, which is the floor on store capacity: a store
    /// smaller than this evicts a tile that the same frame still wants and rebuilds it.
    #[must_use]
    pub fn peak_union(&self) -> usize {
        self.frames.iter().map(|f| f.union).max().unwrap_or(0)
    }

    /// How much of the requested work the overlap makes redundant, as a ratio. One means the
    /// views share nothing; four means all four wanted identical covers throughout.
    #[must_use]
    pub fn sharing_ratio(&self) -> f64 {
        if self.distinct_tiles == 0 {
            return 1.0;
        }
        self.tile_requests as f64 / self.distinct_tiles as f64
    }

    /// Tile keys the sweep touched, in a form the store can be driven with.
    #[must_use]
    pub fn describe(&self) -> String {
        format!(
            "{} frames, {} distinct tiles, {} requests, peak union {}",
            self.frames.len(),
            self.distinct_tiles,
            self.tile_requests,
            self.peak_union()
        )
    }
}

/// Runs the sweep's cover computation and coverage walk.
///
/// This is the half of §13.3 that needs no renderer: it computes what each view wants at each
/// frame and checks the result has no holes. Building the tiles is the caller's business,
/// because that is where the shared store — and the flatness the store exists for — comes in.
///
/// # Errors
///
/// [`cover::CoverError`] if a view is pitched.
pub fn run(
    views: &[ViewTransform],
    zooms: &[f64],
    samples: usize,
) -> Result<SweepReport, cover::CoverError> {
    let mut frames = Vec::with_capacity(zooms.len());
    let mut seen: BTreeSet<TileCoord> = BTreeSet::new();
    let mut tile_requests = 0;

    for (index, &zoom) in zooms.iter().enumerate() {
        let mut union: BTreeSet<TileCoord> = BTreeSet::new();
        let mut total = 0;
        let mut gaps = Vec::new();

        for view in views {
            let at_zoom = ViewTransform { zoom, ..*view };
            let tiles = cover::cover(&at_zoom)?;
            gaps.extend(cover::coverage_gaps(&at_zoom, &tiles, samples)?);
            total += tiles.len();
            union.extend(tiles.iter().copied());
        }

        tile_requests += total;
        seen.extend(union.iter().copied());
        frames.push(Frame {
            index,
            zoom,
            union: union.len(),
            total,
            gaps,
        });
    }

    Ok(SweepReport {
        frames,
        distinct_tiles: seen.len(),
        tile_requests,
    })
}
