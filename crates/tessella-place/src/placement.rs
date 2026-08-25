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

use crate::feature::CollisionBox;
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
#[derive(Debug, Clone, Copy)]
pub struct Candidate {
    /// Its identity, from the cross-tile index — what the fade will be keyed by.
    pub cross_tile_id: u32,
    /// The text's collision box, if it has text.
    pub text: Option<CollisionBox>,
    /// The icon's collision box, if it has one.
    pub icon: Option<CollisionBox>,
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
        let mut place_text = match candidate.text {
            None => false,
            Some(box_) => rules.text_allow_overlap || !grid.hit_test_box(box_.bounds()),
        };
        let mut place_icon = match candidate.icon {
            None => false,
            Some(box_) => rules.icon_allow_overlap || !grid.hit_test_box(box_.bounds()),
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
        if place_text
            && !rules.text_ignore_placement
            && let Some(box_) = candidate.text
        {
            grid.insert_box(candidate.cross_tile_id, box_.bounds());
        }
        if place_icon
            && !rules.icon_ignore_placement
            && let Some(box_) = candidate.icon
        {
            grid.insert_box(candidate.cross_tile_id, box_.bounds());
        }

        out.push(Placed {
            cross_tile_id: candidate.cross_tile_id,
            text: place_text,
            icon: place_icon,
        });
    }

    out
}
