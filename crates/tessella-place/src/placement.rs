//! Deciding which labels get drawn.
//!
//! A transcription of mbgl's placement decision for point symbols. Candidates are offered in
//! priority order; each is tested against everything already placed, and what wins is inserted so
//! that it blocks whatever comes after.
//!
//! # First come, first served, and the order is the whole design
//!
//! There is no global optimisation here — no attempt to fit the most labels, or the most
//! important ones. Placement walks a list and takes what fits. That is deliberate: the order is
//! the style's, by `symbol-sort-key` and then by feature order, so a cartographer decides what
//! matters rather than an algorithm. It is also what makes placement stable frame to frame,
//! which matters more than density: a set that re-optimises as the camera moves is a map where
//! labels swap places while you watch.
//!
//! # A symbol's two halves place together, apart, or not at all
//!
//! `text-optional` and `icon-optional` say whether each half can stand alone. Neither optional
//! means both or neither — a shield with no number is not a label. One optional means the other
//! is required, and the optional half follows it. Both optional means they are independent.
//! mbgl states this as three assignments over two derived booleans, and it is transcribed rather
//! than rewritten as a match, because the derived form is what the style spec's wording maps
//! onto.
//!
//! # Overlap and ignore-placement are different permissions
//!
//! `allow-overlap` says this label may be drawn over others: it skips the test. `ignore-placement`
//! says it does not block others: it skips the insert. A label with both is drawn always and
//! blocks nothing, which is how a style pins a label that must never move.

use crate::feature::{CollisionBox, LineCircle};
use crate::grid::GridIndex;

/// What a layer's style says about how its symbols compete.
#[derive(Debug, Clone, Copy, Default)]
pub struct Rules {
    /// `text-allow-overlap`: draw the text even where it collides.
    pub text_allow_overlap: bool,
    /// `icon-allow-overlap`.
    pub icon_allow_overlap: bool,
    /// `text-optional`: the icon may be drawn without the text.
    pub text_optional: bool,
    /// `icon-optional`: the text may be drawn without the icon.
    pub icon_optional: bool,
    /// `text-ignore-placement`: the text does not block anything else.
    pub text_ignore_placement: bool,
    /// `icon-ignore-placement`.
    pub icon_ignore_placement: bool,
}

/// One symbol offered for placement.
#[derive(Debug, Clone)]
pub struct Candidate {
    /// Its identity, from the cross-tile index — what the fade will be keyed by.
    pub cross_tile_id: u32,
    /// The shape the text reserves, if it has text.
    pub text: Option<Shape>,
    /// The shape it would reserve set *vertically*, if it can be.
    ///
    /// mbgl's second collision feature. A label that permits vertical writing is shaped both
    /// ways and both boxes are kept, because the two are different shapes — a column is tall
    /// and narrow where the row is wide and short — and a label that will not fit across may
    /// still fit down. Which one is drawn is decided here and nowhere earlier.
    pub vertical_text: Option<Shape>,
    /// The shape the icon reserves, if it has one.
    pub icon: Option<Shape>,
}

/// What a symbol reserves: one box, or a run of circles along a line.
///
/// mbgl's `CollisionFeature` carries both under one `alongLine` flag rather than two types, but
/// the two are never mixed and never both present, which is what an enum says and a flag does
/// not. The distinction is not cosmetic: a road name's upright bounding box is most of a square
/// once the road bends, and reserving that square blanks every street inside it.
#[derive(Debug, Clone, PartialEq)]
pub enum Shape {
    /// A point-placed label: one axis-aligned rectangle.
    Box(CollisionBox),
    /// A line-placed label: the circles `line_circles` produced.
    Circles(Vec<LineCircle>),
}

/// Which circles of a line label's run are worth testing.
///
/// mbgl's thinning pass in `CollisionIndex::placeLineFeature`, and it is a *performance* decision
/// with a stated shape rather than an approximation. The circles overlap by construction — they
/// step by half a box so the run is a covering rather than a dotted line — and on screen that
/// means adjacent circles are often nearly on top of each other, most of all where a pitched map
/// squeezes the far end of a road into a few pixels.
///
/// mbgl's rule: circles touch when their centres are two radii apart and are doubled up at one,
/// and it starts dropping at √2 — "thinning the number of circles as much as possible is a major
/// performance win, and the small gaps introduced don't make a very noticeable difference".
///
/// Two circles are never dropped in a row, and the last one is always kept: a run that thinned
/// itself away would reserve nothing, and the end of a label is where it collides with the next.
#[must_use]
pub fn thin(circles: &[LineCircle]) -> Vec<usize> {
    let mut kept: Vec<usize> = Vec::with_capacity(circles.len());
    let mut previous_placed = false;

    for (index, entry) in circles.iter().enumerate() {
        // Outside what the label reaches, so not a circle at all as far as this run is
        // concerned. mbgl makes this the *first* test in the loop and clears
        // `previousCirclePlaced` on the way out, which is why it belongs here rather than in a
        // filter afterwards: thinning compares each circle against the last one kept, and running
        // it over circles the label never covers compares against neighbours mbgl never sees.
        if !entry.covered_by_label {
            previous_placed = false;
            continue;
        }

        if previous_placed {
            let previous = circles[*kept.last().expect("a placed circle")].circle;
            let dx = entry.circle.center.0 - previous.center.0;
            let dy = entry.circle.center.1 - previous.center.1;
            let radius = entry.circle.radius;
            // Squared throughout: the comparison is against √2 radii and squaring both sides
            // removes the root from the inner loop of placement.
            let too_dense = radius * radius * 2.0 > dx * dx + dy * dy;

            // Unless it is the last one the *label* can use, in which case it is kept however
            // tightly it sits against its neighbour. mbgl asks whether the next circle is one it
            // would test -- `atLeastOneMoreCircle` and then the same reach test again -- not
            // merely whether the array continues.
            let next_is_usable = circles
                .get(index + 1)
                .is_some_and(|next| next.covered_by_label);
            if too_dense && next_is_usable {
                previous_placed = false;
                continue;
            }
        }
        kept.push(index);
        previous_placed = true;
    }

    kept
}

impl Shape {
    /// Whether anything already placed is in the way.
    #[must_use]
    pub fn collides(&self, grid: &GridIndex<u32>) -> bool {
        match self {
            Self::Box(box_) => grid.hit_test_box(box_.bounds()),
            // Any circle hitting is the whole label refused. A partial run would draw part of a
            // road name, and the thinning below drops circles that add nothing rather than
            // circles that are in the way.
            // Only what the label covers. The run reaches about twice the label's length --
            // mbgl's pitch padding, for a label a pitched camera stretches into the distance --
            // and mbgl does not test that padding either: `placeLineFeature` skips every circle
            // outside the placed first and last glyph. Testing it made a road name collide with
            // the next copy of itself 250 pixels away, so a road carried its name once where the
            // oracle carried it four times.
            Self::Circles(circles) => thin(circles)
                .into_iter()
                .any(|index| grid.hit_test_circle(circles[index].circle)),
        }
    }

    /// Whether it can be placed at all.
    ///
    /// An empty run is a label with glyphs whose road ran out before a run could be built for it.
    /// It is offered as a shape rather than as `None` so it still counts as *having* text -- that
    /// is what `text_optional` and `icon_optional` read -- but it cannot be placed, which is
    /// mbgl returning unplaced when `placeFirstAndLastGlyph` gives nothing. Colliding with
    /// nothing, an empty run otherwise places unconditionally, and every road too short for its
    /// own name kept its name.
    #[must_use]
    pub fn placeable(&self) -> bool {
        !matches!(self, Self::Circles(circles) if circles.is_empty())
    }

    /// Whether any part of it lands inside the grid.
    ///
    /// mbgl's `isInsideGrid`, and a label with nothing inside is not placed: the point path tests
    /// it beside the hit test, and the line path accumulates `inGrid |= isInsideGrid(...)` over the
    /// circles it walks and refuses the label when none was. Missing here, and a pitched frame is
    /// where it shows: sixteen labels at Seattle z14 pitch 45 were drawn that mbgl rejected for
    /// this and nothing else, all of them in the far field where a pitched projection throws a
    /// label past the padded viewport.
    #[must_use]
    pub fn in_grid(&self, grid: &GridIndex<u32>) -> bool {
        let (right, bottom) = grid.extent();
        let inside = |min: (f32, f32), max: (f32, f32)| {
            max.0 >= 0.0 && min.0 < right && max.1 >= 0.0 && min.1 < bottom
        };
        match self {
            Self::Box(box_) => {
                let bounds = box_.bounds();
                inside(bounds.min, bounds.max)
            }
            // Only the circles actually tested. mbgl accumulates `inGrid |= isInsideGrid(...)`
            // inside the same loop that skips a circle outside the label's reach and one thinned
            // for sitting too close to the last, so both `continue` past it -- a circle that is
            // never tested never reports itself in the grid either. Scanning the whole run instead
            // placed six labels at Seattle z14 pitch 45 that mbgl refused for this and nothing
            // else.
            Self::Circles(circles) => thin(circles).into_iter().any(|index| {
                let entry = &circles[index];
                let c = entry.circle;
                inside(
                    (c.center.0 - c.radius, c.center.1 - c.radius),
                    (c.center.0 + c.radius, c.center.1 + c.radius),
                )
            }),
        }
    }

    /// Reserves it, so later labels collide with it.
    pub fn insert(&self, grid: &mut GridIndex<u32>, id: u32) {
        match self {
            Self::Box(box_) => grid.insert_box(id, box_.bounds()),
            Self::Circles(circles) => {
                // The same run that was tested. Reserving every circle while testing a thinned
                // set would make a label block more than it checked against, which reads as a
                // map that thins out as it fills.
                for index in thin(circles) {
                    grid.insert_circle(id, circles[index].circle);
                }
            }
        }
    }
}

/// What placement decided about one symbol.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Placed {
    /// Its identity.
    pub cross_tile_id: u32,
    /// Whether the text is drawn.
    pub text: bool,
    /// Whether the icon is drawn.
    pub icon: bool,
    /// Whether the text that is drawn is the vertical shaping rather than the horizontal one.
    ///
    /// mbgl's `placedOrientation`. Always false when there is only one shaping, which is every
    /// label the style did not ask to set vertically.
    pub vertical: bool,
}

impl Placed {
    /// Whether anything at all is drawn.
    #[must_use]
    pub const fn any(&self) -> bool {
        self.text || self.icon
    }
}

/// Places a layer's symbols in the order given.
///
/// `grid` carries what is already placed — earlier layers, and earlier tiles of this one — and
/// is added to as symbols win. Passing a fresh grid places a layer against itself alone.
///
/// The returned list is one entry per candidate, in the same order, so a caller can hand it
/// straight to [`crate::fade::Fades::step`].
pub fn place(candidates: &[Candidate], rules: &Rules, grid: &mut GridIndex<u32>) -> Vec<Placed> {
    let mut out = Vec::with_capacity(candidates.len());

    for candidate in candidates {
        // A half with no box is nothing to draw, and nothing to test.
        let mut place_text = match &candidate.text {
            None => false,
            Some(shape) => {
                shape.placeable()
                    && shape.in_grid(grid)
                    && (rules.text_allow_overlap || !shape.collides(grid))
            }
        };
        // Horizontal first, and vertical only if it did not fit. mbgl's order, and it is a
        // preference rather than a tie-break: a label that fits both ways is drawn across.
        let mut vertical = false;
        if !place_text && let Some(shape) = &candidate.vertical_text {
            place_text = shape.placeable()
                && shape.in_grid(grid)
                && (rules.text_allow_overlap || !shape.collides(grid));
            vertical = place_text;
        }
        let mut place_icon = match &candidate.icon {
            None => false,
            Some(shape) => {
                shape.placeable()
                    && shape.in_grid(grid)
                    && (rules.icon_allow_overlap || !shape.collides(grid))
            }
        };

        // Whether each half can stand without the other, in mbgl's spelling.
        let icon_without_text = candidate.text.is_none() || rules.text_optional;
        let text_without_icon = candidate.icon.is_none() || rules.icon_optional;

        if !icon_without_text && !text_without_icon {
            // Neither stands alone: both or nothing.
            place_text = place_text && place_icon;
            place_icon = place_text;
        } else if !text_without_icon {
            // The text needs the icon.
            place_text = place_text && place_icon;
        } else if !icon_without_text {
            // The icon needs the text.
            place_icon = place_text && place_icon;
        }

        // Only what was placed goes in, and only if it is meant to block.
        // Whichever orientation won is the one that blocks, since it is the one drawn.
        let drawn = if vertical {
            candidate.vertical_text.as_ref()
        } else {
            candidate.text.as_ref()
        };
        if place_text
            && !rules.text_ignore_placement
            && let Some(shape) = drawn
        {
            shape.insert(grid, candidate.cross_tile_id);
        }
        if place_icon
            && !rules.icon_ignore_placement
            && let Some(shape) = &candidate.icon
        {
            shape.insert(grid, candidate.cross_tile_id);
        }

        out.push(Placed {
            cross_tile_id: candidate.cross_tile_id,
            text: place_text,
            icon: place_icon,
            vertical: place_text && vertical,
        });
    }

    out
}
