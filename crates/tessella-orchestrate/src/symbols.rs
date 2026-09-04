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
use tessella_place::feature::{
    Extent, Padding, collision_box, collision_box_with, collision_circles,
};
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
    /// The along-line offsets of its first and last glyph, unscaled, when it has glyphs.
    ///
    /// mbgl's `glyphOffsets.front()` and `.back()`, which `placeLineFeature` walks along the
    /// line to find how far the label really reaches in each direction. It is not the collision
    /// box's half-width: the offsets locate the outermost *glyphs*, so the reach stops short of
    /// the box, and it is asymmetric whenever the shaping is.
    pub glyph_reach: Option<(f32, f32)>,
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
    /// Takes the shaped extent into the screen pixels the label competes in.
    ///
    /// `text-size / ONE_EM`, which is mbgl's `fontScale` and the same number the shader scales a
    /// glyph's corners by. Shaping works at one em -- 24 units to the em, whatever the label is
    /// finally set at -- so a shaped extent is in ems and a collision box built from it directly
    /// is the label's size only when the label happens to be set at 24 pixels. At the 12 most
    /// styles ask for it is twice as wide and twice as tall, which is four times the area
    /// reserved against every other label on the screen.
    ///
    /// mbgl carries the same factor into `CollisionFeature` as `textBoxScale`; its boxes are in
    /// tile units and projected later, and these are in screen pixels already, so the tile
    /// half of its `tilePixelRatio * fontScale` does not belong here.
    pub font_scale: f32,
    /// How far a fade moves this frame.
    pub increment: f32,
    /// The viewport, in pixels. The collision grid covers it plus [`VIEWPORT_PADDING`] on each
    /// side.
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

/// How far outside the viewport a label still collides, in pixels.
///
/// mbgl's `viewportPaddingDefault`. It doubles when the camera is pitched, and is 1024 for a
/// single static tile; neither case is produced here yet, so the one value stands.
pub const VIEWPORT_PADDING: f32 = 100.0;

impl Default for FrameOptions {
    fn default() -> Self {
        Self {
            rules: Rules::default(),
            padding: Padding::uniform(2.0),
            // The spec's default `text-size` against a 24-unit em.
            font_scale: 16.0 / 24.0,
            increment: 1.0,
            viewport: (1024.0, 768.0),
            overscaling: 1.0,
            // `icon-padding`'s spec default, which this read as one. Every other default here is
            // the spec's and this one was not.
            icon_padding: Padding::uniform(2.0),
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
    /// What placement decided this frame, across every bucket offered to it.
    ///
    /// Kept so the fades can be advanced without placing again. A fade's direction comes from the
    /// *previous* frame's decision, so a label placed for the first time spends one step still
    /// hidden and reaches full opacity on the second -- which is the fade working, and which
    /// means a caller that places once and draws once sees nothing at all.
    decided: alloc::vec::Vec<Placed>,
    /// Which orientation each label last drew in, by cross-tile id.
    ///
    /// mbgl's `placedOrientations`, and it is remembered rather than recomputed because the
    /// alternative flickers: a label that fails to place this frame is fading out in the
    /// orientation it was drawn in, and re-deciding while it fades would turn it on its side on
    /// the way. So a frame that places nothing for a label leaves its entry alone.
    orientations: alloc::collections::BTreeMap<u32, bool>,
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
        // The viewport with a margin around it, which is what mbgl collides in.
        //
        // `CollisionIndex` builds its grid over `width + 2 * viewportPadding` by
        // `height + 2 * viewportPadding` and offsets every projected point into it, so a label
        // that hangs off the screen keeps its real position and still reserves the ground it
        // covers. A grid the size of the viewport cannot: the index clamps whatever falls outside
        // to the nearest cell, so every off-screen label piles into the boundary cells and
        // collides there with labels it is nowhere near.
        //
        // Cell size 25, which is mbgl's. It is a broad phase and the exact tests follow it, so it
        // decides how much work a query does rather than what it answers.
        let mut grid: GridIndex<u32> = GridIndex::new(
            options.viewport.0.max(1.0) + 2.0 * VIEWPORT_PADDING,
            options.viewport.1.max(1.0) + 2.0 * VIEWPORT_PADDING,
            25,
        );
        self.frame_in(labels, project, options, &mut grid)
    }

    /// As [`Self::frame`], competing in a grid the caller owns.
    ///
    /// What this is for: a frame's labels compete across layers and tiles, not within one bucket.
    /// A road name and a shop name are laid out separately -- different layers, and often
    /// different tiles -- and the whole point of placement is that only one of them gets the
    /// space. [`Self::frame`] builds a grid per call, so it can only ever decide a bucket against
    /// itself; passing one in is what lets a caller walk a frame's buckets in painter order and
    /// have each compete against everything already placed.
    ///
    /// The projection stays per call because it is per *tile*: a label's anchor is in tile units
    /// and the matrix that takes it to the screen belongs to the tile it came from, while the
    /// grid it lands in belongs to the frame.
    pub fn frame_in<P>(
        &mut self,
        labels: &[FrameLabel<'_>],
        project: P,
        options: &FrameOptions,
        grid: &mut GridIndex<u32>,
    ) -> FrameResult
    where
        P: Fn((f32, f32)) -> (f32, f32),
    {
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

                // How far the outermost glyphs sit from the anchor, scaled as mbgl scales them:
                // `fontScale * glyphOffsets.front()` and `.back()`, with `fontScale` the same
                // `text-size / 24` this calls `font_scale`. Magnitudes, because
                // `placeLineFeature` compares against `-firstTileDistance` and `lastTileDistance`
                // and both are distances walked out from the anchor.
                let reach = label.glyph_reach.map(|(first, last)| {
                    (
                        (first * options.font_scale).abs(),
                        (last * options.font_scale).abs(),
                    )
                });

                // A line-placed label reserves a run of circles following the road; a point
                // label reserves one box. Both in screen space, because that is where labels
                // compete for room.
                let text = if label.line.is_empty() {
                    collision_box(extent, anchor, options.font_scale, options.padding, 0.0)
                        .map(Shape::Box)
                } else {
                    let line: Vec<(f32, f32)> =
                        label.line.iter().map(|point| project(*point)).collect();
                    collision_circles(
                        extent,
                        &line,
                        anchor,
                        label.laid_out.segment,
                        options.font_scale,
                        options.padding,
                        options.overscaling,
                        reach,
                    )
                    .map(Shape::Circles)
                    // A label with glyphs always offers a shape, even where no run could be
                    // built for it.
                    //
                    // `None` here means "this symbol has no text", and `place` reads it that way
                    // -- as nothing to draw. But `line_circles` also answers empty when the
                    // projected road runs out before the run does, and a label with text that
                    // got that answer was then dropped as though it had no text at all. On a
                    // road 600 pixels long that is both of its labels: two shaped, none drawn,
                    // against an empty grid.
                    //
                    // An empty run is what mbgl has here -- a `CollisionFeature` with no boxes
                    // tests nothing and reserves nothing, so the label places. It is not a label
                    // that escapes checking: `write_line_positions` walks the same road and hides
                    // it if the name genuinely will not fit, which is the check that belongs to
                    // that question.
                    .or_else(|| {
                        (label.laid_out.glyphs > 0).then(|| Shape::Circles(Vec::new()))
                    })
                };

                // The icon's own box, at its own padding. Point-placed only: a line-placed
                // icon needs the anchors `get_anchors` produces, which layout does not build.
                let icon = label.icon.as_ref().and_then(|laid| {
                    let (top, bottom, left, right) = laid.extent;
                    collision_box_with(
                        Extent {
                            top,
                            bottom,
                            left,
                            right,
                        },
                        project(laid.anchor),
                        1.0,
                        options.icon_padding,
                        // After `icon-text-fit` the extent is the shield's *content* area and
                        // the picture reaches further out; collision reserves the picture.
                        laid.content_margins,
                        0.0,
                    )
                    .map(Shape::Box)
                });

                // The same construction against the other shaping's box, offered only when
                // there is one. mbgl builds a second collision feature for exactly this.
                let vertical_text = label.laid_out.vertical.and_then(|vertical| {
                    let (top, bottom, left, right) = vertical.extent;
                    let extent = Extent {
                        top,
                        bottom,
                        left,
                        right,
                    };
                    if label.line.is_empty() {
                        collision_box(extent, anchor, options.font_scale, options.padding, 0.0)
                            .map(Shape::Box)
                    } else {
                        let line: Vec<(f32, f32)> =
                            label.line.iter().map(|point| project(*point)).collect();
                        collision_circles(
                            extent,
                            &line,
                            anchor,
                            label.laid_out.segment,
                            options.font_scale,
                            options.padding,
                            options.overscaling,
                            reach,
                        )
                        .map(Shape::Circles)
                    }
                });

                Candidate {
                    cross_tile_id: label.cross_tile_id,
                    text,
                    vertical_text,
                    icon,
                }
            })
            .collect();

        let placed = place(&candidates, &options.rules, grid);
        self.decided.extend_from_slice(&placed);
        for entry in &placed {
            if entry.text {
                self.orientations
                    .insert(entry.cross_tile_id, entry.vertical);
            }
        }
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

    /// Forgets what the last frame decided, before deciding a new one.
    pub fn begin(&mut self) {
        self.decided.clear();
    }

    /// Advances every fade to its resting value without placing again.
    ///
    /// For a caller drawing a settled frame rather than an animation: placement has decided, and
    /// this takes the labels it placed to opaque and the rest to transparent. Placing again to
    /// achieve the same would enter every label into the collision grid a second time, where it
    /// would then collide with itself.
    ///
    /// Bounded, because a fade that will not converge must not hang a frame.
    pub fn settle(&mut self, increment: f32) {
        let decided = core::mem::take(&mut self.decided);
        for _ in 0..8 {
            if self.fades.settled() {
                break;
            }
            self.fades.step(
                increment,
                decided
                    .iter()
                    .map(|symbol| (symbol.cross_tile_id, symbol.text, symbol.icon)),
                false,
            );
        }
        self.decided = decided;
    }

    /// The same, for the icon half, which has a decision of its own.
    ///
    /// A symbol's two channels fade together but not in step: `icon-optional` and `text-optional`
    /// exist precisely so one half can be dropped and the other kept. Writing the *text's*
    /// opacity into the icon's buffer throws the icon's own decision away, and what that looks
    /// like on a map is a shield drawn at every anchor along a road, because the collision that
    /// rejected all but one of them was never consulted.
    pub fn write_icon_opacity(&self, labels: &[FrameLabel<'_>], buffers: &mut SymbolBuffers) {
        for label in labels {
            let (placed, opacity) = match self.fades.get(label.cross_tile_id) {
                Some(state) => (state.icon.placed, state.icon.opacity),
                None => (false, 0.0),
            };
            let range = label.laid_out.vertices.clone();
            if range.end > buffers.opacity.len() {
                continue;
            }
            let packed = opacity_vertex(placed, opacity);
            for slot in &mut buffers.opacity[range] {
                *slot = packed;
            }
        }
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

            // A label shaped both ways has both in this range, and only one of them is drawn.
            // The other is set transparent rather than left out of the buffer, because the
            // choice is per view and the buffer is shared: two views of one tile can be drawing
            // different orientations of the same label in the same frame.
            let split = label.laid_out.vertical.map(|vertical| {
                let vertical_wins = self
                    .orientations
                    .get(&label.cross_tile_id)
                    .copied()
                    .unwrap_or(false);
                (vertical.at.clamp(range.start, range.end), vertical_wins)
            });
            let hidden = opacity_vertex(false, 0.0);
            for (at, slot) in buffers.opacity[range.clone()].iter_mut().enumerate() {
                let at = range.start + at;
                *slot = match split {
                    Some((boundary, vertical_wins)) if (at >= boundary) != vertical_wins => hidden,
                    _ => packed,
                };
            }
        }
    }

    /// Walks a line-placed label's glyphs along its road and writes where each one landed.
    ///
    /// The counterpart to [`Self::write_positions`] for the labels that follow a line rather than
    /// sitting at a point. A road name is not one quad rotated: each glyph is stepped along the
    /// projected road and takes the angle of the segment it lands on, which is what makes the text
    /// bend with the street. It is also why the drawable's label-plane matrix is the identity --
    /// the walk *is* the projection, and a plane matrix would bend the label a second time.
    ///
    /// `project` takes tile units into the label plane, which for a label lying flat on the map is
    /// pixels relative to the tile: walking a line in tile units would misplace every glyph by
    /// the scale factor.
    ///
    /// `font_size` is the layer's `text-size` at this zoom, and it is what turns the glyph
    /// distances into that same space. Shaping works at one em -- [`tessella_glyph::text::ONE_EM`]
    /// units to the em, whatever the label is finally set at -- so the distances layout recorded
    /// are ems, not pixels, and the shader scales them by `size / ONE_EM` when it draws. The walk
    /// has to scale them by the same factor or it steps a 12px label as though it were 24px, and
    /// mbgl's `fontScale` in `reprojectLineLabels` is this number.
    ///
    /// A label with no room on its line keeps whatever it last held rather than being written
    /// somewhere arbitrary; placement has already decided whether it draws at all.
    /// Returns the labels that found no room, by cross-tile id, so a caller can hide the other
    /// half of the same symbol. A shield whose number will not fit must not keep its shield: an
    /// empty box strung along a road is worse than nothing there, and it is what happens if only
    /// the text is hidden.
    pub fn write_line_positions<P>(
        &self,
        labels: &[FrameLabel<'_>],
        project: P,
        font_size: f32,
        buffers: &mut SymbolBuffers,
    ) -> alloc::vec::Vec<u32>
    where
        P: Fn((f32, f32)) -> (f32, f32),
    {
        let mut without_room = alloc::vec::Vec::new();
        let offsets = crate::project::LineOffsets {
            font_scale: font_size / tessella_glyph::text::ONE_EM,
            ..crate::project::LineOffsets::default()
        };
        for label in labels {
            if label.line.is_empty() {
                continue;
            }
            let range = label.laid_out.vertices.clone();
            // Four vertices to a glyph, and the distances are one per glyph.
            let quads = range.start / 4..range.end / 4;
            if range.end > buffers.dynamic.len() || quads.end > buffers.glyph_offsets.len() {
                continue;
            }
            let line: alloc::vec::Vec<(f32, f32)> =
                label.line.iter().map(|point| project(*point)).collect();
            let (placement, _flipped) = crate::project::place_upright(
                &line,
                project(label.laid_out.anchor),
                label.laid_out.segment,
                &buffers.glyph_offsets[quads],
                &offsets,
            );
            let crate::project::Placement::Placed(glyphs) = placement else {
                // No room on the line for this label at this anchor -- the road runs out before
                // the last glyph does. Hidden rather than left alone: what `lay_out` put in the
                // dynamic buffer is the anchor in *tile* units, and the shader reads that buffer
                // as label-plane coordinates, so leaving it draws the label some thousands of
                // pixels from where it belongs. mbgl drops such a label too; the difference is
                // that it never had a wrong position to leave behind.
                if range.end <= buffers.opacity.len() {
                    let hidden = tessella_layout::symbol_bucket::opacity_vertex(false, 0.0);
                    for slot in &mut buffers.opacity[range] {
                        *slot = hidden;
                    }
                }
                without_room.push(label.cross_tile_id);
                continue;
            };
            for (index, glyph) in glyphs.iter().enumerate() {
                let base = range.start + index * 4;
                if base + 4 > buffers.dynamic.len() {
                    break;
                }
                for slot in &mut buffers.dynamic[base..base + 4] {
                    *slot = [glyph.point.0, glyph.point.1, glyph.angle];
                }
            }
        }
            without_room
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
