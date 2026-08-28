//! §13.3's last unmet criterion: zero symbol pops across the four-view sweep.
//!
//! `sweep_never_blank` proves the sweep is covered at every frame and `sweep_budget` measures
//! what it costs. Neither could say anything about labels, because until R2 there were none —
//! which is what "needs R2 to have symbols that could pop" meant.
//!
//! # What a pop is
//!
//! A label that is on the ground continuously and *stops being drawn*, or restarts its fade, on
//! a frame where nothing about it changed. The case §13.2 is built around is a zoom crossing: a
//! tile is replaced by its four children, and the label that was in the parent is a different
//! symbol instance in the child — different tile, different buffer, nothing in the geometry
//! saying it is the same label. If its fade state does not follow it across, it fades from
//! nothing while sitting in the same place on screen, and that is the pop.
//!
//! So the assertion is about *continuity*, not about any particular opacity: across consecutive
//! frames, a label's opacity may move by at most one fade increment. A pop is a jump.
//!
//! # Why the sweep and not a single crossing
//!
//! A single crossing can be got right by accident — one tile, one parent, one child. The sweep
//! crosses eight levels in each direction with four views at different centres, so the same
//! label is in a different tile of a different zoom in different views at the same instant, and
//! its identity has to hold across all of it.

use std::collections::{BTreeMap, BTreeSet};

use tessella_glyph::fonts::{Dependencies, Fonts};
use tessella_layout::symbol_layout::SymbolLayout;
use tessella_orchestrate::symbols::{FrameLabel, FrameOptions, ViewSymbols};
use tessella_orchestrate::tile::{Content, TileId, build_tile};
use tessella_orchestrate::{sweep, viewcover::ViewCover};
use tessella_place::cross_tile::{CrossTileIndex, Symbol};
use tessella_place::feature::Padding;
use tessella_source::geojson;
use tessella_source::tiling::TilingOptions;
use tessella_storage::source::{FetchError, FileSource, Response};
use tessella_style::{Source, Style};
use tessella_tile::cover::ViewTransform;
use tessella_tile::renderables::DataTileId;

const GLYPHS: &[u8] = include_bytes!("../../../tests/glyph-fixtures/TestFont/0-255.pbf");

/// How far a fade moves per frame. The whole assertion is stated against this.
const INCREMENT: f32 = 0.25;

/// Serves the one range the fixture font has.
struct Disk;

impl FileSource for Disk {
    fn fetch(&self, url: &str) -> Result<Response, FetchError> {
        let body = if url.contains("0-255") {
            GLYPHS.to_vec()
        } else {
            Vec::new()
        };
        Ok(Response {
            status: 200,
            body,
            ..Response::default()
        })
    }
}

/// A style with a grid of named points around the sweep's centre.
///
/// Spread wide enough that the low end of the sweep sees all of them and the high end sees a few,
/// so labels enter and leave the cover as it climbs — which is the traffic a pop hides in.
fn style() -> Style {
    let mut features = Vec::new();
    for row in 0..7i32 {
        for column in 0..7i32 {
            let longitude = -0.11 + f64::from(column - 3) * 0.006;
            let latitude = 51.505 + f64::from(row - 3) * 0.004;
            features.push(format!(
                r#"{{"type": "Feature",
                    "properties": {{"name": "P{row}{column}"}},
                    "geometry": {{"type": "Point", "coordinates": [{longitude}, {latitude}]}}}}"#
            ));
        }
    }

    Style::parse(&format!(
        r#"{{"version": 8, "name": "sweep-symbols",
            "glyphs": "https://example.com/fonts/{{fontstack}}/{{range}}.pbf",
            "sources": {{"probe": {{"type": "geojson",
                "data": {{"type": "FeatureCollection", "features": [{}]}}}}}},
            "layers": [{{"id": "labels", "type": "symbol", "source": "probe",
                "layout": {{"text-field": "{{name}}", "text-font": ["TestFont"],
                            "text-size": 16}}}}]}}"#,
        features.join(",")
    ))
    .expect("the style parses")
}

/// What one frame drew: per label *text*, the identity it was given and the opacity it drew at.
///
/// Keyed by text and not by identity, which is the whole difficulty. A pop is a label that keeps
/// existing on the ground and loses its history — and an implementation that hands it a fresh
/// identity every frame has no history to lose, so an assertion keyed by identity sees a stream
/// of brand-new labels and finds nothing wrong. Text is what the ground says; the identity is
/// what the implementation claims, and comparing the two is the point.
type Opacities = BTreeMap<String, Drawn>;

/// What became of one label this frame.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Drawn {
    /// The identity the implementation gave it.
    id: u32,
    /// What it drew at.
    opacity: f32,
    /// Whether placement kept it. A label losing its collision is *meant* to be at zero, so a
    /// fade only has to arrive for a label that is winning its space.
    placed: bool,
}

/// Runs the sweep and returns each view's per-frame opacities.
fn run(zooms: &[f64]) -> (Vec<Vec<Opacities>>, usize) {
    let style = style();
    let Some(Source::Geojson(source)) = style.source("probe") else {
        panic!("a geojson source");
    };
    let features = geojson::read(&source.data).expect("features read");

    // The tile store: built once per tile, shared by every view (§5.1). Symbol layouts are what
    // a symbol layer's bucket *is*, so this is that store for labels.
    let mut built: BTreeMap<(u8, u32, u32), SymbolLayout> = BTreeMap::new();
    let mut fonts = Fonts::new(style.glyphs.clone().expect("a glyph URL"));

    // The cross-tile index is process-scoped and not per view: identity is about where a label
    // is on the ground, and the ground does not move when a camera does. Four views looking at
    // the same label must agree it is one label, or its fade forks.
    let mut index = CrossTileIndex::new();
    let mut bucket_id = 0u32;

    let base = sweep::four_views();
    let mut covers: Vec<ViewCover> = base
        .iter()
        .map(|view| {
            ViewCover::new(&ViewTransform {
                zoom: zooms[0],
                ..*view
            })
            .expect("covers")
        })
        .collect();
    let mut views: Vec<ViewSymbols> = base.iter().map(|_| ViewSymbols::new()).collect();

    let mut out: Vec<Vec<Opacities>> = base.iter().map(|_| Vec::new()).collect();
    let mut labels_seen = 0usize;

    for &zoom in zooms {
        for (which, (view, cover)) in base.iter().zip(&mut covers).enumerate() {
            let at = ViewTransform { zoom, ..*view };
            cover.update(&at).expect("covers");

            let mut frame_labels: Vec<FrameLabel> = Vec::new();
            let mut texts: Vec<String> = Vec::new();
            let mut buffers = Vec::new();

            for tile in cover.tiles() {
                let key = (tile.z, tile.x, tile.y);
                let layout = built.entry(key).or_insert_with(|| {
                    let buckets = build_tile(
                        &style,
                        "probe",
                        TileId::new(tile.z, tile.x, tile.y),
                        &features,
                        TilingOptions::default(),
                    )
                    .expect("the tile builds");
                    buckets
                        .iter()
                        .find_map(|bucket| match &bucket.content {
                            Content::Symbol(layout) => Some(layout.clone()),
                            _ => None,
                        })
                        .expect("a symbol layer")
                });

                if layout.is_empty() {
                    continue;
                }
                fonts.fetch(&merged(layout), &Disk).expect("the font reads");
                let (_, laid) = layout.lay_out(&fonts, None);
                if laid.is_empty() {
                    continue;
                }

                // Identities, from the shared index. Keyed by text and by anchor rounded onto a
                // grid, so the same place name in a parent and in a child is one label.
                let mut symbols: Vec<Symbol> = layout
                    .pending
                    .iter()
                    .zip(&laid)
                    .map(|(pending, entry)| Symbol::new(pending.text.clone(), entry.anchor))
                    .collect();
                bucket_id += 1;
                index.add_bucket(
                    DataTileId::new(tile.z, tile.x, tile.y),
                    bucket_id,
                    &mut symbols,
                );

                buffers.push((laid, symbols));
            }

            for (laid, symbols) in &buffers {
                for (entry, symbol) in laid.iter().zip(symbols) {
                    frame_labels.push(FrameLabel {
                        cross_tile_id: symbol.cross_tile_id,
                        laid_out: entry.clone(),
                        icon: None,
                        line: &[],
                    });
                    texts.push(symbol.key.clone());
                }
            }
            labels_seen += frame_labels.len();

            // Tile units to this view's screen pixels, which is where labels compete.
            let scale = f64::powf(
                2.0,
                zoom - f64::from(cover.tiles().first().map_or(0, |t| t.z)),
            );
            #[allow(clippy::cast_possible_truncation)]
            let project = move |anchor: (f32, f32)| {
                let factor = (scale * 512.0 / 8192.0) as f32;
                (anchor.0 * factor, anchor.1 * factor)
            };

            let result = views[which].frame(
                &frame_labels,
                project,
                &FrameOptions {
                    padding: Padding::uniform(2.0),
                    increment: INCREMENT,
                    viewport: (view.width as f32, view.height as f32),
                    ..FrameOptions::default()
                },
            );

            let mut opacities = Opacities::new();
            for (placed, text) in result.placed.iter().zip(&texts) {
                if let Some(joint) = views[which].opacity(placed.cross_tile_id) {
                    opacities.insert(
                        text.clone(),
                        Drawn {
                            id: placed.cross_tile_id,
                            opacity: joint.text.opacity,
                            placed: placed.text,
                        },
                    );
                }
            }
            out[which].push(opacities);
        }
    }

    (out, labels_seen)
}

/// Every codepoint one layout needs, in the shape the store takes.
fn merged(layout: &SymbolLayout) -> Dependencies {
    let mut out = Dependencies::new();
    for (stack, codepoints) in layout.dependencies() {
        out.entry(stack).or_default().extend(codepoints);
    }
    out
}

/// The sweep draws labels, at both ends and through every crossing.
///
/// The control. An assertion about label continuity over a sweep with no labels in it passes for
/// the wrong reason, and this is what stops the rest of this file being vacuous.
#[test]
fn the_sweep_has_labels_to_pop() {
    let (frames, labels_seen) = run(&sweep::sweep_zooms(33));
    assert!(
        labels_seen > 500,
        "only {labels_seen} labels over the sweep"
    );

    for (which, view) in frames.iter().enumerate() {
        assert!(
            view.iter().all(|frame| !frame.is_empty()),
            "view {which} had a frame with no labels at all"
        );
    }
}

/// No label's opacity moves by more than one fade increment between frames.
///
/// §13.3's "zero symbol pops (fade-only transitions)", stated as the property rather than as a
/// count. A label that vanishes and returns, or that restarts its fade at a crossing, moves by
/// more than an increment in one frame; one that is genuinely fading moves by exactly one.
#[test]
fn no_label_jumps_its_fade() {
    let (frames, _) = run(&sweep::sweep_zooms(33));
    // Floating-point slack only. An increment of 0.25 against a pop of a whole step leaves no
    // room for this to mask anything.
    const SLACK: f32 = 1e-4;

    for (which, view) in frames.iter().enumerate() {
        for (index, pair) in view.windows(2).enumerate() {
            let (before, after) = (&pair[0], &pair[1]);
            for (text, drawn) in after {
                let opacity = &drawn.opacity;
                let Some(previous) = before.get(text).map(|drawn| drawn.opacity) else {
                    // A label that was not on screen at all in the previous frame is new to this
                    // view, and a new label starts its fade rather than jumping into one.
                    assert!(
                        *opacity <= INCREMENT + SLACK,
                        "view {which} frame {index}: {text} appeared at {opacity}"
                    );
                    continue;
                };
                let moved = (opacity - previous).abs();
                assert!(
                    moved <= INCREMENT + SLACK,
                    "view {which} frame {index}: {text} went {previous} -> {opacity}"
                );
            }
        }
    }
}

/// A label that stays on screen reaches full opacity and stops changing.
///
/// The assertion the continuity check above cannot make, and the one that catches the failure
/// that matters. An implementation that hands a label a fresh identity every frame never jumps —
/// every frame it is a new label starting a new fade, so every step is one increment and the
/// continuity check is satisfied. What it never does is *arrive*: the label sits at one
/// increment forever, flickering at a quarter opacity, and only asking whether a label that has
/// been there long enough is finished will say so.
///
/// A pop is the absence of history. This is what tests for history.
#[test]
fn a_label_that_stays_becomes_opaque() {
    let (frames, _) = run(&sweep::sweep_zooms(33));

    // Long enough to fade in from nothing, plus a frame.
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let needed = (1.0 / INCREMENT).ceil() as usize + 1;

    let mut checked = 0usize;
    for (which, view) in frames.iter().enumerate() {
        // How many consecutive frames each label has been on screen for.
        let mut streak: BTreeMap<String, usize> = BTreeMap::new();
        for (index, frame) in view.iter().enumerate() {
            let present: BTreeSet<&String> = frame.keys().collect();
            streak.retain(|text, _| present.contains(text));

            for (text, drawn) in frame {
                if !drawn.placed {
                    streak.remove(text);
                    continue;
                }
                let run = streak.entry(text.clone()).or_insert(0);
                *run += 1;
                if *run < needed {
                    continue;
                }
                let opacity = drawn.opacity;
                checked += 1;
                assert!(
                    (opacity - 1.0).abs() < 1e-4,
                    "view {which} frame {index}: {text} has been on screen for {run} frames \
                     and is still at {opacity} -- its fade is restarting"
                );
            }
        }
    }

    assert!(
        checked > 100,
        "only {checked} labels stayed long enough to settle, so this proved little"
    );
}

/// A label that crosses a zoom level keeps the fade it had.
///
/// The specific case §13.2 exists for, asserted where it is provable: at the frame a view's
/// deepest zoom changes, every label still in view has the opacity it had before. Without the
/// cross-tile index it would be a different symbol in a different tile with no history, and it
/// would start at zero — which the previous test would catch as a jump, and this one names.
#[test]
fn a_crossing_carries_the_fade_across() {
    let zooms = sweep::sweep_zooms(33);
    let (frames, _) = run(&zooms);

    let mut crossings = 0usize;
    for view in &frames {
        for (index, pair) in view.windows(2).enumerate() {
            // A crossing shows up as the frame's label set changing substantially: tiles were
            // replaced by their children, so identities that survive are the interesting ones.
            let (before, after) = (&pair[0], &pair[1]);
            let survived: Vec<String> = before
                .keys()
                .filter(|text| after.contains_key(*text))
                .cloned()
                .collect();
            let replaced = before.len().abs_diff(after.len());
            if replaced == 0 || survived.is_empty() {
                continue;
            }
            crossings += 1;

            for text in survived {
                let (was_id, was) = (before[&text].id, before[&text].opacity);
                let (now_id, now) = (after[&text].id, after[&text].opacity);
                // The identity has to survive the crossing, which is what carries the fade.
                assert_eq!(
                    was_id, now_id,
                    "frame {index}: {text} changed identity across a crossing"
                );
                assert!(
                    (now - was).abs() <= INCREMENT + 1e-4,
                    "frame {index}: {text} was re-faded from {was} to {now} at a crossing"
                );
            }
        }
    }

    assert!(
        crossings > 4,
        "only {crossings} frames changed their label set, so this proved little"
    );
}

/// Four views agree on what a label is.
///
/// The index is process-scoped, so a label two views can both see has one identity. If it had
/// one per view, the same label would fade independently in each — which is not a pop in any one
/// view and is a flicker across a group, and §9.2's whole point is that views share.
#[test]
fn the_views_share_one_identity_per_label() {
    let (frames, _) = run(&sweep::sweep_zooms(9));

    // At the low end of the sweep all four views look at nearly the same ground, so the sets
    // they draw overlap heavily. Disjoint sets would mean identity forked per view.
    let first: BTreeSet<u32> = frames[0][0].values().map(|drawn| drawn.id).collect();
    let mut shared = 0usize;
    for view in frames.iter().skip(1) {
        let theirs: BTreeSet<u32> = view[0].values().map(|drawn| drawn.id).collect();
        shared += first.intersection(&theirs).count();
    }
    assert!(
        shared > 0,
        "no label identity was shared between views: {} ids in the first",
        first.len()
    );
}
