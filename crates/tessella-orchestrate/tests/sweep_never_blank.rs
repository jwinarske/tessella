//! §13.3's sweep, driven through the real per-view state, with tiles that take time to arrive.
//!
//! `four_view_sweep.rs` checks the sweep's *covers*: what four views want at each frame, and
//! that those tiles leave no gap. It computes each frame from scratch and assumes every tile it
//! names is available. Both simplifications hide the thing §13.3 is actually about.
//!
//! A crossing is a burst. The cover changes, and for some number of frames afterwards the tiles
//! it now names do not exist yet — that gap is the whole reason never-blank and pre-warm are in
//! §13.2. A sweep where tiles appear the instant they are wanted never enters that state, so it
//! cannot tell an implementation that substitutes from one that draws holes.
//!
//! So this runs the same sweep through [`ViewCover`], against a pyramid where a tile becomes
//! drawable a fixed number of frames after something asks for it, and asserts across every frame
//! that each view is completely drawn. What it reports: sixty-five frames, complete from frame
//! six — which is the fetch latency, and the earliest any frame *could* be complete — and
//! seventy tiles fetched in seventy calls across four views, which is §9.3 flatness stated over
//! the shared pyramid rather than over a cover count.
//!
//! # Necessity is modelled, because it is load-bearing
//!
//! Only a `Required` retain starts a fetch here. An `Optional` one is a cache lookup and starts
//! nothing, which is what the distinction means. That is not decoration: if optional retains
//! also fetched, every substitute a crossing considered would become a request, and the burst
//! this is measuring would be several times larger than the cover that caused it.

use std::collections::{BTreeMap, BTreeSet};

use tessella_orchestrate::sweep;
use tessella_orchestrate::viewcover::ViewCover;
use tessella_tile::cover::ViewTransform;
use tessella_tile::renderables::{DataTileId, Necessity, Pyramid, RenderTileId, TileState};

/// How many frames a fetch takes. Long enough that a crossing is visibly mid-flight for several
/// frames, which is where a hole would appear.
const LATENCY: u64 = 6;

/// A pyramid where a required tile arrives `LATENCY` frames after it is first asked for.
#[derive(Default)]
struct Fleet {
    now: u64,
    /// When each tile becomes drawable. Absent means nothing has asked for it.
    ready_at: BTreeMap<DataTileId, u64>,
    drawn: Vec<DataTileId>,
    /// Every tile a fetch was started for, ever — the flatness counter.
    fetched: BTreeSet<DataTileId>,
    /// How many fetches were started, counting repeats. Must equal `fetched.len()`.
    fetch_calls: u64,
}

impl Fleet {
    fn state(&self, id: DataTileId) -> Option<TileState> {
        self.ready_at.get(&id).map(|&at| TileState {
            renderable: at <= self.now,
            loaded: at <= self.now,
            tried_cache: true,
        })
    }
}

impl Pyramid for Fleet {
    fn get(&mut self, id: DataTileId) -> Option<TileState> {
        self.state(id)
    }

    fn create(&mut self, id: DataTileId) -> Option<TileState> {
        // Creating an entry does not fetch it; the retain that follows decides that.
        self.ready_at.entry(id).or_insert(u64::MAX);
        self.state(id)
    }

    fn retain(&mut self, id: DataTileId, necessity: Necessity) {
        if necessity == Necessity::Optional {
            // A cache lookup. If it is not already on its way, it stays absent.
            return;
        }
        let arrives = self.now + LATENCY;
        let slot = self.ready_at.entry(id).or_insert(u64::MAX);
        if *slot == u64::MAX {
            *slot = arrives;
            self.fetch_calls += 1;
            self.fetched.insert(id);
        }
    }

    fn render(&mut self, _render: RenderTileId, data: DataTileId) {
        self.drawn.push(data);
    }
}

/// Whether `drawn` covers all of `tile`, exactly, on the quadtree.
fn covers(drawn: &BTreeSet<RenderTileId>, tile: RenderTileId, depth: u8) -> bool {
    let mut ancestor = tile;
    loop {
        if drawn.contains(&ancestor) {
            return true;
        }
        if ancestor.z == 0 {
            break;
        }
        ancestor = RenderTileId {
            wrap: ancestor.wrap,
            z: ancestor.z - 1,
            x: ancestor.x / 2,
            y: ancestor.y / 2,
        };
    }
    if tile.z >= depth {
        return false;
    }
    let (z, x, y) = (tile.z + 1, tile.x * 2, tile.y * 2);
    [(x, y), (x, y + 1), (x + 1, y), (x + 1, y + 1)]
        .into_iter()
        .all(|(x, y)| {
            covers(
                drawn,
                RenderTileId {
                    wrap: tile.wrap,
                    z,
                    x,
                    y,
                },
                depth,
            )
        })
}

/// What one run of the sweep produced.
struct Run {
    /// Per frame, per view: whether that view was completely drawn.
    complete: Vec<Vec<bool>>,
    /// The first frame at which every view was complete.
    settled: Option<usize>,
    /// Cover recomputations per view, and frames.
    churn: Vec<(u64, u64)>,
    fleet: Fleet,
}

/// Runs the §13.3 sweep through per-view state against a pyramid with latency.
fn run(zooms: &[f64]) -> Run {
    let base = sweep::four_views();
    let mut covers_state: Vec<ViewCover> = base
        .iter()
        .map(|view| {
            ViewCover::new(&ViewTransform {
                zoom: zooms[0],
                ..*view
            })
            .expect("covers")
        })
        .collect();

    let mut fleet = Fleet::default();
    let mut complete = Vec::with_capacity(zooms.len());
    let mut settled = None;

    for (index, &zoom) in zooms.iter().enumerate() {
        fleet.now = index as u64;
        let mut per_view = Vec::with_capacity(base.len());

        for (view, state) in base.iter().zip(&mut covers_state) {
            let at = ViewTransform { zoom, ..*view };
            state.update(&at).expect("covers");

            fleet.drawn.clear();
            state.draw(&mut fleet, 0..=16);

            let set: BTreeSet<RenderTileId> = fleet.drawn.iter().map(|id| id.render_id()).collect();
            let depth = set.iter().map(|id| id.z).max().unwrap_or(0);
            per_view.push(state.tiles().iter().all(|tile| {
                covers(
                    &set,
                    RenderTileId {
                        #[allow(clippy::cast_possible_truncation)]
                        wrap: tile.wrap as i16,
                        z: tile.z,
                        x: tile.x,
                        y: tile.y,
                    },
                    depth,
                )
            }));
        }

        if settled.is_none() && per_view.iter().all(|ok| *ok) {
            settled = Some(index);
        }
        complete.push(per_view);
    }

    Run {
        complete,
        settled,
        churn: covers_state.iter().map(ViewCover::churn).collect(),
        fleet,
    }
}

/// Once the map is covered, it stays covered for the rest of the sweep.
///
/// The assertion §13.3 words as "zero uncovered frames", made about what is *drawn* rather than
/// about what the cover names. Every crossing in the sweep happens after the map has settled, so
/// there is no frame after that point where a hole is excusable.
#[test]
fn once_covered_the_sweep_never_blanks() {
    let run = run(&sweep::sweep_zooms(33));
    let settled = run.settled.expect("the sweep must cover at some point");
    assert!(
        settled < LATENCY as usize + 2,
        "took {settled} frames to draw a complete frame at all"
    );

    for (index, views) in run.complete.iter().enumerate().skip(settled) {
        for (view, ok) in views.iter().enumerate() {
            assert!(*ok, "frame {index}, view {view} had a hole");
        }
    }
}

/// The sweep crosses levels, and the crossings are what the test is for.
///
/// Without this the test above passes for a sweep that never left one zoom level — which is
/// exactly the shape a mistake in the zoom list would produce, and it would be invisible.
#[test]
fn the_sweep_actually_crosses_levels() {
    let zooms = sweep::sweep_zooms(33);
    let run = run(&zooms);
    for (view, (changes, frames)) in run.churn.iter().enumerate() {
        // Construction counts as the first frame: it computes a cover like any other.
        assert_eq!(
            *frames,
            zooms.len() as u64 + 1,
            "view {view} skipped frames"
        );
        assert!(
            *changes >= 8,
            "view {view} recomputed its cover {changes} times over a z8 to z16 sweep and back"
        );
    }
}

/// Every tile is fetched once, however many views wanted it.
///
/// §9.3 flatness, over the shared pyramid rather than over a cover count: four views whose
/// covers overlap must not each start their own fetch for the tile they share, and a tile
/// revisited on the way back down must not be fetched again.
#[test]
fn the_sweep_fetches_each_tile_once() {
    let run = run(&sweep::sweep_zooms(33));
    assert_eq!(
        run.fleet.fetch_calls,
        run.fleet.fetched.len() as u64,
        "a tile was fetched more than once"
    );
    assert!(
        run.fleet.fetched.len() > 50,
        "the sweep should touch real work"
    );
}

/// A substitute does not start a fetch.
///
/// The necessity distinction, asserted where it costs something. During a crossing the algorithm
/// looks at children and ancestors it will not draw; if considering one were enough to request
/// it, the burst would be a multiple of the cover that caused it. Compared against the union of
/// every cover the sweep ever held, which is the set that legitimately needs fetching.
#[test]
fn only_the_ideal_tiles_are_fetched() {
    let zooms = sweep::sweep_zooms(33);
    let base = sweep::four_views();

    let mut wanted: BTreeSet<(u8, u32, u32, i32)> = BTreeSet::new();
    let mut states: Vec<ViewCover> = base
        .iter()
        .map(|view| {
            ViewCover::new(&ViewTransform {
                zoom: zooms[0],
                ..*view
            })
            .expect("covers")
        })
        .collect();
    for &zoom in &zooms {
        for (view, state) in base.iter().zip(&mut states) {
            state
                .update(&ViewTransform { zoom, ..*view })
                .expect("covers");
            wanted.extend(state.tiles().iter().map(|t| (t.z, t.x, t.y, t.wrap)));
        }
    }

    let run = run(&zooms);
    for id in &run.fleet.fetched {
        assert!(
            wanted.contains(&(id.z, id.x, id.y, i32::from(id.wrap))),
            "{id:?} was fetched but no view ever covered it"
        );
    }
}
/// The sweep's numbers, for reading rather than asserting.
///
/// `cargo test -p tessella-orchestrate --test sweep_never_blank report -- --ignored --nocapture`
#[test]
#[ignore]
fn report() {
    let zooms = sweep::sweep_zooms(33);
    let run = run(&zooms);
    println!("frames {}  settled at {:?}", zooms.len(), run.settled);
    println!(
        "fetched {} tiles in {} calls",
        run.fleet.fetched.len(),
        run.fleet.fetch_calls
    );
    println!("churn per view: {:?}", run.churn);
    let blank: Vec<usize> = run
        .complete
        .iter()
        .enumerate()
        .filter(|(_, v)| !v.iter().all(|ok| *ok))
        .map(|(i, _)| i)
        .collect();
    println!("incomplete frames: {blank:?}");
}
