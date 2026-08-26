//! One view's symbol frame: what is placed, what is fading, and the bytes that say so.
//!
//! The join between R2's two halves. Layout produced a tile's glyphs and the box each label
//! occupies, once, shared by every view (§5.1). This runs per view per frame: project the
//! anchors, compete for space, advance the fades, and write the result back into the two
//! per-frame buffers a symbol shader reads.
//!
//! # Why the projection is the caller's
//!
//! Placement happens in screen space — labels compete for screen, not for ground, so two towns
//! a kilometre apart collide at z5 and not at z14, and the same two collide on a phone and not
//! on a wall display. The projection from tile units to screen is a function of the camera,
//! which is per view, so it is passed in rather than assumed. Getting this wrong does not fail:
//! it produces a map where nothing ever collides, which is what a tile-unit anchor against a
//! pixel-sized box gives.
//!
//! # Identity outlives geometry
//!
//! Fades are keyed by cross-tile id, not by position in a buffer. A tile rebuilt at a zoom
//! crossing has all new vertices for labels that did not move, and re-fading those is exactly
//! the symbol pop §13.3 asks for zero of. So the id comes from the cross-tile index and the
//! buffer is addressed by the range layout recorded.

use alloc::vec::Vec;

use tessella_layout::symbol_bucket::{LaidOut, SymbolBuffers, opacity_vertex};
use tessella_place::fade::{Fades, Joint};
use tessella_place::feature::{Extent, Padding, collision_box, collision_circles};
use tessella_place::grid::GridIndex;
use tessella_place::placement::{Candidate, Placed, Rules, Shape, place};

/// A label offered to placement this frame.
#[derive(Debug, Clone)]
pub struct FrameLabel<'a> {
    /// Its identity, from the cross-tile index.
    pub cross_tile_id: u32,
    /// Where layout put it, and which vertices are its own.
    pub laid_out: LaidOut,
    /// Where its icon was laid out, when it has one.
    ///
    /// A symbol is a label, an icon, or both, and placement treats the two halves together —
    /// `text-optional` and `icon-optional` are about exactly this pair. They are separate fields
    /// rather than one because they are separate *drawables*: the two go through different
    /// shaders and cannot share a vertex buffer.
    pub icon: Option<LaidOut>,
    /// The line it follows, in tile units, or empty when it is point-placed.
    ///
    /// A borrow rather than a copy: a street tile has thousands of these and the geometry is
    /// already in the tile's buffers, so cloning each road per label per *frame* would be the
    /// most expensive thing placement does.
    pub line: &'a [(f32, f32)],
}

/// What one view's symbols did this frame.
#[derive(Debug, Clone, Default)]
pub struct FrameResult {
    /// Placement's decision per label, in the order offered.
    pub placed: Vec<Placed>,
    /// How many labels are drawn.
    pub drawn: usize,
    /// How many are still mid-fade — §9.3's counter, and zero is what lets a frame go quiet.
    pub fading: usize,
}

/// How a view competes for space and how fast its symbols fade.
#[derive(Debug, Clone, Copy)]
pub struct FrameOptions {
    /// The layer's overlap and optionality rules.
    pub rules: Rules,
    /// `text-padding`, in screen pixels.
    pub padding: Padding,
    /// How far a fade moves this frame.
    pub increment: f32,
    /// The viewport, in pixels, which is the extent the collision grid covers.
    pub viewport: (f32, f32),
    /// The tile's overscaling, which widens a line label's padding circles.
    pub overscaling: f32,
    /// `icon-padding`, in screen pixels.
    ///
    /// Separate from the text's, and the spec's defaults differ: two pixels around text and
    /// *one* around an icon. Sharing one value crowds icons or spaces them, depending which way
    /// it is shared.
    pub icon_padding: Padding,
}

impl Default for FrameOptions {
    fn default() -> Self {
        Self {
            rules: Rules::default(),
            padding: Padding::uniform(2.0),
            increment: 1.0,
            viewport: (1024.0, 768.0),
            overscaling: 1.0,
            icon_padding: Padding::uniform(1.0),
        }
    }
}

/// One view's symbol state, carried between frames.
///
/// Per view and not shared (§5.2, §5.5): two views over the same tile place differently and
/// must, because each has its own camera and its own screen.
#[derive(Debug, Default)]
pub struct ViewSymbols {
    fades: Fades,
}

impl ViewSymbols {
    /// A view with nothing placed yet.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Runs one frame: place, fade, and report.
    ///
    /// `project` takes an anchor in tile units to the screen pixel it draws at. Labels are
    /// offered in the order given, which is the style's, and the first to fit wins.
    pub fn frame<P>(
        &mut self,
        labels: &[FrameLabel<'_>],
        project: P,
        options: &FrameOptions,
    ) -> FrameResult
    where
        P: Fn((f32, f32)) -> (f32, f32),
    {
        // A grid the size of the viewport. Anything off it is clamped to the nearest cell rather
        // than dropped: a label hanging off the edge still collides with one that does not.
        let mut grid: GridIndex<u32> =
            GridIndex::new(options.viewport.0.max(1.0), options.viewport.1.max(1.0), 32);

        let candidates: Vec<Candidate> = labels
            .iter()
            .map(|label| {
                let (top, bottom, left, right) = label.laid_out.extent;
                let extent = Extent {
                    top,
                    bottom,
                    left,
                    right,
                };
                let anchor = project(label.laid_out.anchor);

                // A line-placed label reserves a run of circles following the road; a point
                // label reserves one box. Both in screen space, because that is where labels
                // compete for room.
                let text = if label.line.is_empty() {
                    collision_box(extent, anchor, 1.0, options.padding, 0.0).map(Shape::Box)
                } else {
                    let line: Vec<(f32, f32)> =
                        label.line.iter().map(|point| project(*point)).collect();
                    collision_circles(
                        extent,
                        &line,
                        anchor,
                        label.laid_out.segment,
                        1.0,
                        options.padding,
                        options.overscaling,
                    )
                    .map(Shape::Circles)
                };

                // The icon's own box, at its own padding. Point-placed only: a line-placed
                // icon needs the anchors `get_anchors` produces, which layout does not build.
                let icon = label.icon.as_ref().and_then(|laid| {
                    let (top, bottom, left, right) = laid.extent;
                    collision_box(
                        Extent {
                            top,
                            bottom,
                            left,
                            right,
                        },
                        project(laid.anchor),
                        1.0,
                        options.icon_padding,
                        0.0,
                    )
                    .map(Shape::Box)
                });

                Candidate {
                    cross_tile_id: label.cross_tile_id,
                    text,
                    icon,
                }
            })
            .collect();

        let placed = place(&candidates, &options.rules, &mut grid);
        self.fades.step(
            options.increment,
            placed
                .iter()
                .map(|symbol| (symbol.cross_tile_id, symbol.text, symbol.icon)),
            false,
        );

        FrameResult {
            drawn: placed.iter().filter(|symbol| symbol.text).count(),
            fading: self.fades.fading(),
            placed,
        }
    }

    /// The opacity a label is drawing at, if it has one.
    #[must_use]
    pub fn opacity(&self, cross_tile_id: u32) -> Option<Joint> {
        self.fades.get(cross_tile_id)
    }

    /// Whether every fade has finished — §6.5's still-frame question.
    #[must_use]
    pub fn settled(&self) -> bool {
        self.fades.settled()
    }

    /// Writes this frame's opacities into the buffer's per-vertex slots.
    ///
    /// Every vertex of a label carries the same value, because opacity is a property of the
    /// label and the shader reads it per vertex. A label with no fade state is one that has
    /// faded away entirely; its vertices are set transparent rather than left holding whatever
    /// they last drew at, which would be a ghost that never clears.
    pub fn write_opacity(&self, labels: &[FrameLabel<'_>], buffers: &mut SymbolBuffers) {
        for label in labels {
            let (placed, opacity) = match self.fades.get(label.cross_tile_id) {
                Some(state) => (state.text.placed, state.text.opacity),
                None => (false, 0.0),
            };
            let packed = opacity_vertex(placed, opacity);
            let range = label.laid_out.vertices.clone();
            // Layout recorded the range against this buffer; a caller pairing labels with a
            // buffer they did not come from is the one way this goes wrong, so it is bounded
            // rather than trusted.
            if range.end > buffers.opacity.len() {
                continue;
            }
            for slot in &mut buffers.opacity[range] {
                *slot = packed;
            }
        }
    }

    /// Writes each label's placed anchor into the per-frame position buffer.
    ///
    /// The position the shader projects against, which is why it is per frame at all: the
    /// geometry is tile-local and shared, and this is where the camera enters.
    pub fn write_positions<P>(
        &self,
        labels: &[FrameLabel<'_>],
        project: P,
        buffers: &mut SymbolBuffers,
    ) where
        P: Fn((f32, f32)) -> (f32, f32),
    {
        for label in labels {
            let (x, y) = project(label.laid_out.anchor);
            let range = label.laid_out.vertices.clone();
            if range.end > buffers.dynamic.len() {
                continue;
            }
            for slot in &mut buffers.dynamic[range] {
                *slot = [x, y, 0.0];
            }
        }
    }
}
