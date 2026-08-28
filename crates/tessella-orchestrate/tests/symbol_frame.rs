//! A view's symbol frame: placement, fades, and the bytes that carry them.
//!
//! The join between R2's halves. Layout runs once per tile and is shared; this runs per view per
//! frame, and what it has to get right is that the two agree about *which* vertices belong to
//! which label — a shared buffer addressed by the wrong range writes one label's opacity over
//! another's, which draws as a label that will not fade.

use tessella_glyph::atlas::{Atlas, Rect};
use tessella_glyph::pbf::{self, Glyph, Metrics, Range};
use tessella_layout::symbol_bucket::{
    Glyphs, Label, SymbolBuffers, SymbolOptions, build_symbols, opacity_vertex,
};
use tessella_orchestrate::symbols::{FrameLabel, FrameOptions, ViewSymbols};
use tessella_place::feature::Padding;
use tessella_place::placement::Rules;

const GLYPHS: &[u8] = include_bytes!("../../../tests/glyph-fixtures/TestFont/0-255.pbf");

struct Font {
    glyphs: Vec<Glyph>,
    atlas: Atlas,
}

impl Font {
    fn new(pack: &str) -> Self {
        let glyphs = pbf::parse(
            Range {
                first: 0,
                last: 255,
            },
            GLYPHS,
        )
        .expect("the range parses");
        let mut atlas = Atlas::new(512, 512);
        for glyph in &glyphs {
            if pack.chars().any(|character| character as u32 == glyph.id) {
                atlas.add(glyph.id, glyph);
            }
        }
        Self { glyphs, atlas }
    }
}

impl Glyphs for Font {
    fn metrics(&self, codepoint: u32) -> Option<(Metrics, bool)> {
        let glyph = self.glyphs.iter().find(|glyph| glyph.id == codepoint)?;
        Some((glyph.metrics, glyph.bitmap_size().is_some()))
    }
    fn rect(&self, codepoint: u32) -> Option<Rect> {
        self.atlas.get(codepoint)
    }
}

/// Lays out labels at the tile anchors given, and pairs them with identities.
fn lay_out(entries: &[(&str, (f32, f32))]) -> (SymbolBuffers, Vec<FrameLabel<'static>>) {
    let packed: String = entries.iter().map(|(text, _)| *text).collect();
    let font = Font::new(&packed);
    let labels: Vec<Label> = entries
        .iter()
        .map(|(text, anchor)| Label {
        pending: 0,
            text: (*text).to_string(),
            anchor: *anchor,
        })
        .collect();

    let (buffers, laid) = build_symbols(&labels, &font, &SymbolOptions::default());
    let frame = laid
        .into_iter()
        .enumerate()
        .map(|(index, laid_out)| FrameLabel {
            cross_tile_id: index as u32 + 1,
            laid_out,
            icon: None,
            line: &[],
        })
        .collect();
    (buffers, frame)
}

/// Tile units to screen pixels, for a tile drawn 512 wide over an extent of 8192.
fn to_screen(anchor: (f32, f32)) -> (f32, f32) {
    (anchor.0 * 512.0 / 8192.0, anchor.1 * 512.0 / 8192.0)
}

/// Labels far apart are all placed; the frame reports what it drew.
#[test]
fn a_frame_places_what_fits() {
    let (_, labels) = lay_out(&[("Alpha", (1000.0, 1000.0)), ("Bravo", (5000.0, 5000.0))]);
    let mut view = ViewSymbols::new();

    let result = view.frame(&labels, to_screen, &FrameOptions::default());

    assert_eq!(result.placed.len(), 2);
    assert_eq!(result.drawn, 2);
    assert!(result.placed.iter().all(|symbol| symbol.text));
}

/// Labels on top of each other compete, and the first offered wins.
#[test]
fn a_frame_rejects_what_collides() {
    let (_, labels) = lay_out(&[("Alpha", (1000.0, 1000.0)), ("Bravo", (1010.0, 1000.0))]);
    let mut view = ViewSymbols::new();

    let result = view.frame(&labels, to_screen, &FrameOptions::default());

    assert_eq!(result.drawn, 1);
    assert!(result.placed[0].text);
    assert!(!result.placed[1].text);
}

/// The same labels collide at one zoom and not at another.
///
/// The reason placement is in screen space, stated as behaviour. The tile anchors do not move;
/// only the projection does, and the outcome changes — which is exactly right, because two towns
/// a kilometre apart are crowded on a small map and not on a large one.
#[test]
fn zoom_decides_whether_two_labels_collide() {
    let entries = [("Alpha", (1000.0, 1000.0)), ("Bravo", (1400.0, 1000.0))];
    let (_, labels) = lay_out(&entries);

    // Zoomed out: the tile is 512 pixels, so the two are 25 pixels apart and collide.
    let mut far = ViewSymbols::new();
    let out = far.frame(&labels, to_screen, &FrameOptions::default());
    assert_eq!(out.drawn, 1, "crowded when the tile is small");

    // Zoomed in: the same tile drawn eight times larger, so they are 200 apart and both fit.
    let mut near = ViewSymbols::new();
    let close = |anchor: (f32, f32)| (anchor.0 * 4096.0 / 8192.0, anchor.1 * 4096.0 / 8192.0);
    let out = near.frame(
        &labels,
        close,
        &FrameOptions {
            viewport: (4096.0, 4096.0),
            ..FrameOptions::default()
        },
    );
    assert_eq!(out.drawn, 2, "and not when it is large");
}

/// Opacity is written into each label's own vertices, and nowhere else.
///
/// Labels share one buffer per layer per tile, so a range that is off by one writes a label's
/// opacity over its neighbour's. That draws as a label which will not fade, and nothing errors.
#[test]
fn each_label_gets_its_own_opacity_slots() {
    let (mut buffers, labels) =
        lay_out(&[("Alpha", (1000.0, 1000.0)), ("Bravo", (5000.0, 5000.0))]);
    let mut view = ViewSymbols::new();

    // One frame at a quarter: both placed, both a quarter of the way in.
    view.frame(
        &labels,
        to_screen,
        &FrameOptions {
            increment: 0.25,
            ..FrameOptions::default()
        },
    );
    view.write_opacity(&labels, &mut buffers);

    let first = labels[0].laid_out.vertices.clone();
    let second = labels[1].laid_out.vertices.clone();
    assert!(first.end <= second.start, "the ranges do not overlap");
    assert_eq!(first.len(), labels[0].laid_out.glyphs * 4);

    // Every vertex of a label carries the same value, since opacity is the label's.
    let value = buffers.opacity[first.start];
    assert!(
        buffers.opacity[first.clone()]
            .iter()
            .all(|slot| *slot == value)
    );
    assert_eq!(
        value,
        opacity_vertex(true, 0.0),
        "the first frame starts transparent"
    );
}

/// A label that loses its collision fades out where a placed one fades in.
#[test]
fn a_rejected_label_fades_the_other_way() {
    let (mut buffers, labels) =
        lay_out(&[("Alpha", (1000.0, 1000.0)), ("Bravo", (1010.0, 1000.0))]);
    let mut view = ViewSymbols::new();
    let options = FrameOptions {
        increment: 0.5,
        ..FrameOptions::default()
    };

    for _ in 0..4 {
        view.frame(&labels, to_screen, &options);
    }
    view.write_opacity(&labels, &mut buffers);

    let winner = view.opacity(1).expect("tracked");
    assert_eq!(winner.text.opacity, 1.0, "the placed label is opaque");

    // The loser never rose: its first frame recorded unplaced, so it had nothing to fall from.
    assert!(
        view.opacity(2)
            .is_none_or(|state| state.text.opacity == 0.0)
    );

    // Both ranges, and they must differ. Checking only the rejected one passes for a writer
    // that fills the whole buffer with whatever label it saw last — which, iterating in order,
    // is the rejected one.
    let first = labels[0].laid_out.vertices.clone();
    let second = labels[1].laid_out.vertices.clone();
    assert!(
        buffers.opacity[first.clone()]
            .iter()
            .all(|slot| *slot == opacity_vertex(true, 1.0)),
        "the placed label is opaque"
    );
    assert!(
        buffers.opacity[second]
            .iter()
            .all(|slot| *slot == opacity_vertex(false, 0.0)),
        "the rejected label draws nothing"
    );
    assert_ne!(
        buffers.opacity[first.start], buffers.opacity[first.end],
        "the two labels hold different values, in their own slots"
    );
}

/// A settled frame stops moving, which is what lets the producer go quiet.
#[test]
fn a_frame_settles_and_stays_settled() {
    let (_, labels) = lay_out(&[("Alpha", (1000.0, 1000.0))]);
    let mut view = ViewSymbols::new();
    let options = FrameOptions {
        increment: 0.25,
        ..FrameOptions::default()
    };

    let first = view.frame(&labels, to_screen, &options);
    assert_eq!(first.fading, 1, "on its way in");
    assert!(!view.settled());

    for _ in 0..4 {
        view.frame(&labels, to_screen, &options);
    }
    assert!(view.settled());

    for _ in 0..10 {
        let result = view.frame(&labels, to_screen, &options);
        assert_eq!(result.fading, 0, "a settled frame started moving again");
    }
}

/// Positions are written per label, and follow the camera.
#[test]
fn positions_follow_the_camera() {
    let (mut buffers, labels) = lay_out(&[("Alpha", (1000.0, 2000.0))]);
    let view = ViewSymbols::new();

    view.write_positions(&labels, to_screen, &mut buffers);
    let near = buffers.dynamic[0];
    assert_eq!(
        near,
        [1000.0 * 512.0 / 8192.0, 2000.0 * 512.0 / 8192.0, 0.0]
    );

    // A different camera moves them, and the tile-local geometry does not change.
    let vertices = buffers.vertices.clone();
    view.write_positions(&labels, |anchor| (anchor.0, anchor.1), &mut buffers);
    assert_eq!(buffers.dynamic[0], [1000.0, 2000.0, 0.0]);
    assert_eq!(buffers.vertices, vertices, "the geometry is camera-free");
}

/// Two views over the same buffer place independently.
///
/// §9.2's invariant, at the symbol layer: the geometry is shared and the placement is not. A
/// view that inherited its neighbour's decisions would draw the other's map.
#[test]
fn two_views_place_the_same_labels_independently() {
    let (_, labels) = lay_out(&[("Alpha", (1000.0, 1000.0)), ("Bravo", (1400.0, 1000.0))]);

    let mut small = ViewSymbols::new();
    let crowded = small.frame(&labels, to_screen, &FrameOptions::default());

    let mut large = ViewSymbols::new();
    let roomy = large.frame(
        &labels,
        |anchor| (anchor.0 * 4096.0 / 8192.0, anchor.1 * 4096.0 / 8192.0),
        &FrameOptions {
            viewport: (4096.0, 4096.0),
            ..FrameOptions::default()
        },
    );

    assert_eq!(crowded.drawn, 1);
    assert_eq!(roomy.drawn, 2);
    assert_ne!(
        crowded.placed[1].text, roomy.placed[1].text,
        "the same label, two views, two answers"
    );
}

/// Allowing overlap draws everything, which the rules reach from here.
#[test]
fn the_layers_rules_reach_placement() {
    let (_, labels) = lay_out(&[("Alpha", (1000.0, 1000.0)), ("Bravo", (1010.0, 1000.0))]);
    let mut view = ViewSymbols::new();

    let result = view.frame(
        &labels,
        to_screen,
        &FrameOptions {
            rules: Rules {
                text_allow_overlap: true,
                ..Rules::default()
            },
            ..FrameOptions::default()
        },
    );
    assert_eq!(result.drawn, 2);
}

/// Padding reaches placement too.
#[test]
fn the_layers_padding_reaches_placement() {
    // "Alpha" measures 60 screen pixels wide, and one screen pixel is 16 tile units at this
    // projection. So the two anchors are put 80 pixels apart: clear of each other unpadded,
    // and inside the 100 pixels that twenty a side would need.
    let entries = [("Alpha", (1000.0, 1000.0)), ("Bravo", (2280.0, 1000.0))];
    let (_, labels) = lay_out(&entries);
    let mut tight = ViewSymbols::new();
    let mut loose = ViewSymbols::new();

    let apart = tight.frame(
        &labels,
        to_screen,
        &FrameOptions {
            padding: Padding::default(),
            ..FrameOptions::default()
        },
    );
    let crowded = loose.frame(
        &labels,
        to_screen,
        &FrameOptions {
            padding: Padding::uniform(20.0),
            ..FrameOptions::default()
        },
    );

    assert_eq!(apart.drawn, 2, "they clear each other unpadded");
    assert_eq!(crowded.drawn, 1, "and crowd each other padded");
}

/// A line label reserves the road it follows, not the box around it.
///
/// The reason line labels collide as circles at all. A name along a diagonal has a bounding box
/// close to a square — mbgl says as much about rotated labels — and reserving that square blanks
/// everything in the quadrants either side of the road, which no one is standing on. Placed as
/// circles it reserves a band along the road and nothing else.
///
/// Asserted as the difference between the two shapes rather than against a fixed answer: what
/// matters is that the reservation follows the line.
#[test]
fn a_line_label_reserves_its_road_and_not_its_box() {
    use tessella_layout::symbol_bucket::{LineLabel, LineOptions, build_line_symbols};

    // A road running diagonally across the tile.
    let road: Vec<(f32, f32)> = (0..=18i16)
        .map(|index| (f32::from(index) * 200.0, f32::from(index) * 200.0))
        .collect();

    let font = Font::new("Diagonal Road Beside");
    let (_, laid) = build_line_symbols(
        &[LineLabel {
        pending: 0,
            text: "Diagonal Road".to_string(),
            line: road.clone(),
        }],
        &font,
        &LineOptions {
            centred: true,
            ..LineOptions::default()
        },
    );
    assert_eq!(laid.len(), 1, "one centred label");

    // And a point label beside the road, well clear of it, but inside the square its upright
    // bounding box would cover.
    let (_, beside) = build_symbols(
        &[Label {
        pending: 0,
            text: "Beside".to_string(),
            anchor: (2880.0, 1600.0),
        }],
        &font,
        &SymbolOptions::default(),
    );

    let placed = |line: &[(f32, f32)]| {
        let labels = vec![
            FrameLabel {
                cross_tile_id: 1,
                laid_out: laid[0].clone(),
                icon: None,
                line,
            },
            FrameLabel {
                cross_tile_id: 2,
                laid_out: beside[0].clone(),
                icon: None,
                line: &[],
            },
        ];
        let mut view = ViewSymbols::new();
        let result = view.frame(&labels, to_screen, &FrameOptions::default());
        result
            .placed
            .iter()
            .find(|symbol| symbol.cross_tile_id == 2)
            .expect("the label beside the road was offered")
            .text
    };

    // An empty line means the label is point-placed, so it reserves its box — and the box
    // reaches the label beside the road.
    assert!(
        !placed(&[]),
        "the box did not block anything, so the comparison says nothing"
    );
    assert!(
        placed(&road),
        "the road's circles blocked a label the road does not pass"
    );
}

/// A symbol's two halves are placed together, and the optionality rules decide how.
///
/// `place` has modelled these since R2 and nothing exercised them with a real icon, because
/// until R3 there were none. The four combinations are genuinely four different maps: a shield
/// that vanishes with its label, a label that vanishes with its shield, either alone, or both
/// or nothing.
mod two_halves {
    use super::{lay_out, to_screen};

    use tessella_layout::symbol_bucket::{IconLabel, IconOptions, build_icons};
    use tessella_orchestrate::symbols::{FrameLabel, FrameOptions, ViewSymbols};
    use tessella_place::feature::Padding;
    use tessella_place::placement::Rules;

    /// An atlas with one big icon in it, padded the way the packer pads.
    fn sprites() -> tessella_glyph::sprite::Positions {
        [(
            "marker".to_string(),
            tessella_glyph::sprite::IconPosition {
                padded_rect: tessella_glyph::atlas::Rect {
                    x: 1,
                    y: 1,
                    width: 66,
                    height: 66,
                },
                pixel_ratio: 1.0,
                sdf: false,
                content: None,
                text_fit_width: None,
                text_fit_height: None,
            },
        )]
        .into_iter()
        .collect()
    }

    /// Two symbols at the same tile anchor, so their icons certainly collide.
    ///
    /// The text boxes collide too — the point is that both halves of the second symbol are in
    /// the way, and what is drawn is decided by the rules rather than by geometry.
    fn overlapping() -> (
        Vec<tessella_layout::symbol_bucket::LaidOut>,
        Vec<tessella_layout::symbol_bucket::LaidOut>,
    ) {
        let (_, text) = lay_out(&[("Alpha", (1000.0, 1000.0)), ("Bravo", (1010.0, 1000.0))]);
        let icons: Vec<IconLabel> = [(1000.0f32, 1000.0f32), (1010.0, 1000.0)]
            .into_iter()
            .map(|anchor| IconLabel {
        pending: 0,
                image: "marker".to_string(),
                anchor,
                options: IconOptions::default(),
                text: None,
            })
            .collect();
        let (_, laid_icons) = build_icons(&icons, &sprites());
        (
            text.into_iter().map(|label| label.laid_out).collect(),
            laid_icons,
        )
    }

    /// Runs a frame with both halves offered under `rules`, returning what each symbol drew.
    fn placed(rules: Rules) -> Vec<(bool, bool)> {
        let (text, icons) = overlapping();
        let labels: Vec<FrameLabel> = text
            .into_iter()
            .zip(icons)
            .enumerate()
            .map(|(index, (laid_out, icon))| FrameLabel {
                #[allow(clippy::cast_possible_truncation)]
                cross_tile_id: index as u32 + 1,
                laid_out,
                icon: Some(icon),
                line: &[],
            })
            .collect();

        let mut view = ViewSymbols::new();
        let result = view.frame(
            &labels,
            to_screen,
            &FrameOptions {
                rules,
                padding: Padding::uniform(2.0),
                icon_padding: Padding::uniform(1.0),
                ..FrameOptions::default()
            },
        );
        result
            .placed
            .iter()
            .map(|symbol| (symbol.text, symbol.icon))
            .collect()
    }

    /// The icon half is offered at all.
    ///
    /// The control: before this, `Candidate::icon` was always `None`, so every rule below would
    /// have agreed with every other for the wrong reason.
    #[test]
    fn the_icon_half_is_placed() {
        let drawn = placed(Rules::default());
        assert_eq!(drawn[0], (true, true), "the first symbol drew both halves");
    }

    /// With neither half optional, a symbol is both or nothing.
    ///
    /// The spec's default. A shield with a number in it is one thing, and drawing the number
    /// without the shield — or the shield without its number — is worse than drawing neither.
    #[test]
    fn neither_optional_is_both_or_nothing() {
        let drawn = placed(Rules::default());
        assert_eq!(drawn[0], (true, true));
        assert_eq!(drawn[1], (false, false), "a half survived alone");
    }

    /// `text-optional` lets the icon stand without its label.
    #[test]
    fn text_optional_keeps_the_icon() {
        let drawn = placed(Rules {
            text_optional: true,
            // The second symbol's icon has to fit for there to be anything to keep, so the
            // icons are allowed to overlap and only the text competes.
            icon_allow_overlap: true,
            ..Rules::default()
        });
        assert_eq!(drawn[1], (false, true), "the icon went with the text");
    }

    /// `icon-optional` lets the label stand without its icon.
    #[test]
    fn icon_optional_keeps_the_text() {
        let drawn = placed(Rules {
            icon_optional: true,
            text_allow_overlap: true,
            ..Rules::default()
        });
        assert_eq!(drawn[1], (true, false), "the text went with the icon");
    }

    /// A symbol with no icon is unaffected by the icon rules.
    ///
    /// Most symbols are text-only, and `icon-optional` defaulting to false must not make every
    /// one of them depend on an icon it does not have.
    #[test]
    fn a_symbol_with_no_icon_still_draws_its_text() {
        let (_, labels) = lay_out(&[("Alpha", (1000.0, 1000.0))]);
        let mut view = ViewSymbols::new();
        let result = view.frame(&labels, to_screen, &FrameOptions::default());
        assert_eq!(
            (result.placed[0].text, result.placed[0].icon),
            (true, false),
            "a text-only symbol was held back by an icon it does not have"
        );
    }

    /// The icon's padding is its own, and the spec's defaults differ.
    ///
    /// Two pixels around text and one around an icon. Sharing one value crowds icons or spaces
    /// them, depending which way it is shared — and either reads as a collision bug rather than
    /// as a padding one.
    #[test]
    fn the_icon_carries_its_own_padding() {
        let options = FrameOptions::default();
        assert_eq!(options.padding, Padding::uniform(2.0));
        assert_eq!(options.icon_padding, Padding::uniform(1.0));

        // And the value is used: a padding wide enough to make two separated icons collide does.
        let (text, icons) = overlapping();
        let apart: Vec<tessella_layout::symbol_bucket::LaidOut> = icons
            .into_iter()
            .enumerate()
            .map(|(index, mut laid)| {
                #[allow(clippy::cast_precision_loss)]
                {
                    laid.anchor = (1000.0 + index as f32 * 2000.0, 1000.0);
                }
                laid
            })
            .collect();

        let run = |icon_padding| {
            let labels: Vec<FrameLabel> = text
                .clone()
                .into_iter()
                .zip(apart.clone())
                .enumerate()
                .map(|(index, (laid_out, icon))| FrameLabel {
                    #[allow(clippy::cast_possible_truncation)]
                    cross_tile_id: index as u32 + 1,
                    laid_out,
                    icon: Some(icon),
                    line: &[],
                })
                .collect();
            let mut view = ViewSymbols::new();
            view.frame(
                &labels,
                to_screen,
                &FrameOptions {
                    rules: Rules {
                        text_allow_overlap: true,
                        icon_optional: true,
                        ..Rules::default()
                    },
                    icon_padding,
                    ..FrameOptions::default()
                },
            )
            .placed
            .iter()
            .filter(|symbol| symbol.icon)
            .count()
        };

        assert_eq!(
            run(Padding::uniform(1.0)),
            2,
            "two separated icons collided"
        );
        assert_eq!(
            run(Padding::uniform(200.0)),
            1,
            "a padding wide enough to reach the neighbour did not"
        );
    }
}
