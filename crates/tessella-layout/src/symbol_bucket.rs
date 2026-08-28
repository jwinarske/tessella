//! Quads into the vertex buffers a symbol shader reads.
//!
//! A transcription of mbgl's `SymbolBucket::layoutVertex` and the four-vertex emission around
//! it. This is the last step before the capture stream: a placed label becomes two triangles per
//! glyph, in the exact byte layout `SymbolIconShader` and `SymbolSDFShader` declare.
//!
//! # Three attributes, because eight is the limit
//!
//! mbgl packs the anchor and the corner offset into one `Short4` with the comment "combining pos
//! and offset to reduce number of vertex attributes passed to shader (8 max for some devices)".
//! That is a hardware constraint, not a preference, and it is why the layout looks arbitrary:
//! the anchor is whole tile units, the offset is *thirty-seconds* of a pixel, and they share
//! four shorts.
//!
//! # Everything is fixed point, at three different scales
//!
//! The corner offset is in 1/32 pixel, the pixel offset in 1/16, and the minimum font scale in
//! 1/256. Each is the precision that attribute needs against the range it has to cover, and
//! each is a number the shader divides back out. Getting one wrong scales that term by a power
//! of two — a label at the right place with the wrong size, or the right size in the wrong
//! place.
//!
//! # The size is packed with a flag in its low bit
//!
//! `aSizeMin` is the minimum size times 128, shifted up one, with `isSDF` in the bit that
//! vacates. So the shader reads the size *and* which of the two symbol shaders drew it from one
//! unsigned short. It is why sizes are capped at 255: `255 * 128 << 1` is the largest value that
//! still fits.

use alloc::vec::Vec;

/// The largest glyph or icon size the packing can carry.
pub const MAX_GLYPH_ICON_SIZE: u16 = 255;

/// What a size is multiplied by before packing.
pub const SIZE_PACK_FACTOR: u16 = 128;

/// The largest packed size, before the flag bit is shifted in.
pub const MAX_PACKED_SIZE: u16 = MAX_GLYPH_ICON_SIZE * SIZE_PACK_FACTOR;

/// One symbol vertex, in the three attributes the shader declares.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SymbolVertex {
    /// `idSymbolPosOffsetVertexAttribute`: the anchor in tile units, then the corner offset in
    /// thirty-seconds of a pixel.
    pub pos_offset: [i16; 4],
    /// `idSymbolDataVertexAttribute`: texture x and y, then the packed size range.
    pub data: [u16; 4],
    /// `idSymbolPixelOffsetVertexAttribute`: the pixel offset in sixteenths, then the minimum
    /// font scale in two-hundred-and-fifty-sixths.
    pub pixel_offset: [i16; 4],
}

/// The size range a symbol is drawn across, before packing.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SizeRange {
    /// Size at the low end of the zoom range.
    pub min: f32,
    /// Size at the high end.
    pub max: f32,
}

impl SizeRange {
    /// A size that does not vary with zoom.
    #[must_use]
    pub const fn constant(size: f32) -> Self {
        Self {
            min: size,
            max: size,
        }
    }
}

/// Packs a size range and the SDF flag into the two unsigned shorts the shader reads.
///
/// The flag rides in the low bit of the minimum, which is what caps sizes at 255: shifting a
/// larger one up would carry it past what a `u16` holds.
#[must_use]
pub fn pack_size(size: SizeRange, is_sdf: bool) -> (u16, u16) {
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let scale = |value: f32| (value * f32::from(SIZE_PACK_FACTOR)) as u16;
    let min = (scale(size.min).min(MAX_PACKED_SIZE) << 1) + u16::from(is_sdf);
    let max = scale(size.max).min(MAX_PACKED_SIZE);
    (min, max)
}

/// One vertex of a symbol quad.
///
/// `anchor` is the label's anchor in tile units; `corner` is this vertex's offset from it in
/// pixels; `glyph_offset_y` is the glyph's vertical position along a line-following label, which
/// is folded into the offset rather than carried separately.
#[must_use]
#[allow(clippy::too_many_arguments)]
pub fn layout_vertex(
    anchor: (f32, f32),
    corner: (f32, f32),
    glyph_offset_y: f32,
    tex: (u16, u16),
    size: SizeRange,
    is_sdf: bool,
    pixel_offset: (f32, f32),
    min_font_scale: (f32, f32),
) -> SymbolVertex {
    let (size_min, size_max) = pack_size(size, is_sdf);
    #[allow(clippy::cast_possible_truncation)]
    SymbolVertex {
        pos_offset: [
            anchor.0 as i16,
            anchor.1 as i16,
            // A thirty-second of a pixel: fine enough that rounding is invisible, coarse enough
            // that a label's corners stay inside a short.
            (corner.0 * 32.0).round() as i16,
            ((corner.1 + glyph_offset_y) * 32.0).round() as i16,
        ],
        data: [tex.0, tex.1, size_min, size_max],
        pixel_offset: [
            (pixel_offset.0 * 16.0) as i16,
            (pixel_offset.1 * 16.0) as i16,
            (min_font_scale.0 * 256.0) as i16,
            (min_font_scale.1 * 256.0) as i16,
        ],
    }
}

/// The per-frame vertex: where placement put the label, and which way it faces.
///
/// Separate from the layout vertex because it changes every frame while the layout does not —
/// which is the whole reason a symbol has two vertex buffers.
#[must_use]
pub const fn dynamic_vertex(anchor: (f32, f32), label_angle: f32) -> [f32; 3] {
    [anchor.0, anchor.1, label_angle]
}

/// The opacity vertex: a fade and a placement flag in one float.
///
/// Opacity is quantised to seven bits and shifted up one, with `placed` in the bit that
/// vacates. The shader wants both and a float carries both, which saves an attribute against
/// the eight-attribute limit the layout above is already fighting.
#[must_use]
pub fn opacity_vertex(placed: bool, opacity: f32) -> f32 {
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let quantised = (opacity * 127.0) as u8;
    f32::from((quantised << 1) | u8::from(placed))
}

/// A symbol layer's geometry for one tile.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct SymbolBuffers {
    /// Four vertices per glyph.
    pub vertices: Vec<SymbolVertex>,
    /// The per-frame positions, one per vertex.
    pub dynamic: Vec<[f32; 3]>,
    /// The per-frame opacities, one per vertex.
    pub opacity: Vec<f32>,
    /// Two triangles per glyph.
    pub indices: Vec<u16>,
    /// How far along its line each glyph sits, one per quad.
    ///
    /// mbgl's `PlacedSymbol::glyphOffsets`, and it is deliberately *not* in the vertex: the
    /// shader projects the line first and then walks this distance along the projected result,
    /// so a value baked into the geometry would be bent twice. Zero throughout for a point
    /// label, which is what makes the two placements share one buffer format.
    pub glyph_offsets: Vec<f32>,
}

impl SymbolBuffers {
    /// How many glyphs are in here.
    #[must_use]
    pub fn glyphs(&self) -> usize {
        self.vertices.len() / 4
    }

    /// Whether anything has been added.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.vertices.is_empty()
    }

    /// Appends another buffer's contents, offsetting its indices onto these vertices.
    ///
    /// A layer whose `text-font` is data-driven resolves to more than one font stack, and each
    /// stack is shaped against its own glyphs — so the layer's buffers are built per stack and
    /// joined here. mbgl reaches the same place from the other direction, handing
    /// `prepareSymbols` the whole `GlyphMap` and looking up per feature.
    ///
    /// Returns how many vertices were already here, which is what a caller shifts the appended
    /// labels' vertex ranges by.
    ///
    /// # Panics
    ///
    /// When the join would put a vertex past what a `u16` index reaches. The same bound
    /// [`Self::add_quad`] asserts, and it has to be checked here too: two buffers each inside it
    /// can be outside it together.
    pub fn append(&mut self, other: &Self) -> usize {
        let base = self.vertices.len();
        assert!(
            base + other.vertices.len() <= usize::from(u16::MAX),
            "a symbol buffer past what a u16 index reaches needs a new segment"
        );

        self.vertices.extend_from_slice(&other.vertices);
        self.dynamic.extend_from_slice(&other.dynamic);
        self.opacity.extend_from_slice(&other.opacity);
        self.glyph_offsets.extend_from_slice(&other.glyph_offsets);

        #[allow(clippy::cast_possible_truncation)]
        let offset = base as u16;
        self.indices
            .extend(other.indices.iter().map(|index| index + offset));

        base
    }

    /// Adds one glyph's quad: four vertices and two triangles.
    ///
    /// The corners are given in mbgl's order — top-left, top-right, bottom-left, bottom-right —
    /// and the texture coordinates follow from the rectangle, so a caller cannot pair a corner
    /// with the wrong texel.
    ///
    /// # Panics
    ///
    /// When the buffer already holds more vertices than a `u16` index can reach. mbgl starts a
    /// new segment there; this refuses, because silently wrapping the index draws one label's
    /// glyphs from another's vertices.
    #[allow(clippy::too_many_arguments)]
    pub fn add_quad(
        &mut self,
        anchor: (f32, f32),
        corners: [(f32, f32); 4],
        glyph_offset: (f32, f32),
        tex: (u16, u16, u16, u16),
        size: SizeRange,
        is_sdf: bool,
        opacity: f32,
    ) {
        let glyph_offset_y = glyph_offset.1;
        let index = self.vertices.len();
        assert!(
            index + 4 <= usize::from(u16::MAX),
            "a symbol buffer past what a u16 index reaches needs a new segment"
        );
        #[allow(clippy::cast_possible_truncation)]
        let base = index as u16;

        let (x, y, width, height) = tex;
        // Each corner takes the texel of its own corner of the rectangle.
        let texels = [
            (x, y),
            (x + width, y),
            (x, y + height),
            (x + width, y + height),
        ];

        for (corner, texel) in corners.into_iter().zip(texels) {
            self.vertices.push(layout_vertex(
                anchor,
                corner,
                glyph_offset_y,
                texel,
                size,
                is_sdf,
                (0.0, 0.0),
                (0.0, 0.0),
            ));
            self.dynamic.push(dynamic_vertex(anchor, 0.0));
            self.opacity.push(opacity_vertex(true, opacity));
        }

        // Two triangles over the four corners, sharing the diagonal.
        self.indices
            .extend_from_slice(&[base, base + 1, base + 2, base + 1, base + 2, base + 3]);
        self.glyph_offsets.push(glyph_offset.0);
    }
}

/// What a symbol layer needs to know about a glyph.
///
/// Re-exported from `tessella-glyph`, which is where it lives: the crate that *answers* these
/// questions is the one that should declare them, and it could not implement a trait declared
/// here without depending on this crate, which depends on it.
pub use tessella_glyph::Glyphs;

/// A label to lay out: what it says and where it is anchored, in tile coordinates.
#[derive(Debug, Clone, PartialEq)]
pub struct Label {
    /// Which of the layout's pending symbols this is, stamped into every instance it produces.
    pub pending: usize,
    /// The text, already resolved from `text-field`.
    pub text: alloc::string::String,
    /// Its anchor, in tile units.
    pub anchor: (f32, f32),
}

/// How a symbol layer sets its text.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SymbolOptions {
    /// `text-size`, in pixels.
    pub size: f32,
    /// `text-max-width`, in ems. Zero never wraps.
    pub max_width_ems: f32,
    /// `text-letter-spacing`, in pixels.
    pub letter_spacing: f32,
    /// Where the label sits relative to its anchor.
    pub anchor: tessella_glyph::shaping::Anchor,
    /// How its lines align.
    pub justify: tessella_glyph::shaping::Justify,
}

impl Default for SymbolOptions {
    fn default() -> Self {
        Self {
            size: 16.0,
            max_width_ems: 10.0,
            letter_spacing: 0.0,
            anchor: tessella_glyph::shaping::Anchor::Center,
            justify: tessella_glyph::shaping::Justify::Center,
        }
    }
}

/// One laid-out label: its geometry, and the box placement will compete with.
#[derive(Debug, Clone, PartialEq)]
pub struct LaidOut {
    /// Which of the layout's pending symbols this instance came from.
    ///
    /// Not the index of this entry, and the two differ exactly where it matters: a line-placed
    /// label is one pending symbol and *one instance per anchor*, so a road named twice along
    /// its length is one feature and several entries. Anything pairing a symbol's halves — an
    /// icon with its label — has to pair on this rather than on position, which held only
    /// because point placement is the case where the two agree.
    pub pending: usize,
    /// Where it is anchored, in tile units.
    pub anchor: (f32, f32),
    /// The extent it occupies around that anchor, in pixels: top, bottom, left, right.
    pub extent: (f32, f32, f32, f32),
    /// How many glyphs it drew.
    pub glyphs: usize,
    /// An icon's margins between its content box and its own edges, in logical pixels.
    ///
    /// `None` for text and for an icon whose sprite names no content box. Collision adds them,
    /// because after `icon-text-fit` the extent above is the *content* area and the drawn picture
    /// reaches further out by its border.
    pub content_margins: Option<(f32, f32, f32, f32)>,
    /// Which segment of its line the anchor falls on.
    ///
    /// mbgl's `anchorSegment`, and the projection cannot do without it: walking a line from a
    /// point needs to know which pair of vertices that point lies between, and recovering it by
    /// searching the line is both slower and ambiguous where a line crosses itself. Zero for a
    /// point label, which has no line.
    pub segment: usize,
    /// Which vertices of the shared buffer are this label's.
    ///
    /// A layer's labels share one buffer, so per-frame state — the opacity a fade produced, the
    /// position placement chose — has to be written into a *slice* of it. Without the range a
    /// caller would have to re-derive it from glyph counts, which is the kind of arithmetic that
    /// is right until one label draws fewer glyphs than its text has characters.
    pub vertices: core::ops::Range<usize>,
}

/// Lays out a layer's labels into one tile's buffers.
///
/// Every label's quads go into the same buffers, in the order given, which is what makes the
/// index arithmetic a running total and what the golden's single drawable per tile reflects —
/// mbgl emits one buffer per layer per tile, not one per label.
///
/// A label whose glyphs are not all packed yet still lays out the ones that are. A map that
/// waited for a whole font before drawing anything would show nothing during a pan into new
/// text, and a partly-drawn label is what mbgl shows too.
pub fn build_symbols<G: Glyphs + ?Sized>(
    labels: &[Label],
    glyphs: &G,
    options: &SymbolOptions,
) -> (SymbolBuffers, Vec<LaidOut>) {
    use tessella_glyph::quads::{self, Placed};
    use tessella_glyph::shaping::{self, Char, Options as ShapeOptions};
    use tessella_glyph::text::ONE_EM;

    let mut buffers = SymbolBuffers::default();
    let mut out = Vec::with_capacity(labels.len());

    for label in labels {
        let chars: Vec<Char> = label
            .text
            .chars()
            .map(|character| {
                let codepoint = character as u32;
                match glyphs.metrics(codepoint) {
                    #[allow(clippy::cast_precision_loss)]
                    Some((metrics, true)) => {
                        Char::new(codepoint, metrics.advance as f32 + options.letter_spacing)
                    }
                    #[allow(clippy::cast_precision_loss)]
                    Some((metrics, false)) => {
                        Char::blank(codepoint, metrics.advance as f32 + options.letter_spacing)
                    }
                    // A codepoint the stack does not carry: no advance and nothing to draw, so
                    // the rest of the label still sets correctly around the gap.
                    None => Char::blank(codepoint, 0.0),
                }
            })
            .collect();

        let shaping = shaping::shape(
            &chars,
            &ShapeOptions {
                max_width: options.max_width_ems * ONE_EM,
                line_height: ONE_EM,
                anchor: options.anchor,
                justify: options.justify,
                spacing: options.letter_spacing,
            },
        );

        let before = buffers.glyphs();
        let quads = quads::glyph_quads(
            &shaping,
            |codepoint| {
                let (metrics, _) = glyphs.metrics(codepoint)?;
                Some(Placed {
                    rect: glyphs.rect(codepoint)?,
                    metrics,
                })
            },
            &quads::Options::default(),
        );

        for quad in quads {
            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
            buffers.add_quad(
                label.anchor,
                [quad.tl, quad.tr, quad.bl, quad.br],
                quad.glyph_offset,
                (
                    quad.tex.x as u16,
                    quad.tex.y as u16,
                    quad.tex.width as u16,
                    quad.tex.height as u16,
                ),
                SizeRange::constant(options.size),
                true,
                1.0,
            );
        }

        out.push(LaidOut {
            pending: label.pending,
            anchor: label.anchor,
            extent: (shaping.top, shaping.bottom, shaping.left, shaping.right),
            glyphs: buffers.glyphs() - before,
            content_margins: None,
            segment: 0,
            vertices: before * 4..buffers.vertices.len(),
        });
    }

    (buffers, out)
}

/// A label that follows a line rather than sitting at a point.
#[derive(Debug, Clone, PartialEq)]
pub struct LineLabel {
    /// Which of the layout's pending symbols this is, stamped into every instance it produces.
    pub pending: usize,
    /// The icon's horizontal extent around the anchor, as `(left, right)` in logical pixels.
    ///
    /// mbgl passes both the shaped text's and the shaped icon's to `getAnchors`, so a symbol
    /// with an icon and no text still gets anchors — from the icon. Zero where there is none,
    /// which is what mbgl passes for the same case.
    pub icon: (f32, f32),
    /// The text, already resolved from `text-field`.
    pub text: alloc::string::String,
    /// The line it follows, in tile units.
    pub line: Vec<(f32, f32)>,
}

/// How a line-placed label repeats.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LineOptions {
    /// How the text itself is set.
    pub symbol: SymbolOptions,
    /// `symbol-spacing`, in tile units.
    pub spacing: f32,
    /// `text-max-angle`, in radians.
    pub max_angle: f32,
    /// The tile's overscale factor, which keeps a child's anchors aligned with its parent's.
    pub overscaling: f32,
    /// One label at the line's midpoint rather than a repeating run.
    pub centred: bool,
}

impl Default for LineOptions {
    fn default() -> Self {
        Self {
            symbol: SymbolOptions::default(),
            spacing: 250.0,
            max_angle: core::f32::consts::PI / 4.0,
            overscaling: 1.0,
            centred: false,
        }
    }
}

/// Lays out labels that follow lines.
///
/// One shaping per label however many times it repeats: the glyphs, their corners and their
/// texture coordinates are identical at every anchor, and only the anchor differs. Shaping once
/// per anchor would redo the same work for every repetition of a long road's name — which at
/// street zoom is most of the labels on the tile.
///
/// The quads carry the along-line offset in `glyph_offset` rather than in their corners, because
/// the shader projects a line-following label before placing each glyph. A builder that baked
/// the offset into the corners would lay the label out flat and then bend it, putting every
/// glyph but the first in the wrong place.
pub fn build_line_symbols<G: Glyphs + ?Sized>(
    labels: &[LineLabel],
    glyphs: &G,
    options: &LineOptions,
) -> (SymbolBuffers, Vec<LaidOut>) {
    use crate::anchors::{get_anchors, get_center_anchor};
    use tessella_glyph::quads::{self, Placed};
    use tessella_glyph::shaping::{self, Char, Options as ShapeOptions};
    use tessella_glyph::text::ONE_EM;

    let mut buffers = SymbolBuffers::default();
    let mut out = Vec::new();

    for label in labels {
        let chars: Vec<Char> = label
            .text
            .chars()
            .map(|character| {
                let codepoint = character as u32;
                match glyphs.metrics(codepoint) {
                    #[allow(clippy::cast_precision_loss)]
                    Some((metrics, true)) => Char::new(
                        codepoint,
                        metrics.advance as f32 + options.symbol.letter_spacing,
                    ),
                    #[allow(clippy::cast_precision_loss)]
                    Some((metrics, false)) => Char::blank(
                        codepoint,
                        metrics.advance as f32 + options.symbol.letter_spacing,
                    ),
                    None => Char::blank(codepoint, 0.0),
                }
            })
            .collect();

        let shaping = shaping::shape(
            &chars,
            &ShapeOptions {
                // A line-placed label does not wrap: it follows the line, and a second line of
                // text would have to follow it too. mbgl sets the width to zero for the same
                // reason.
                max_width: 0.0,
                line_height: ONE_EM,
                anchor: options.symbol.anchor,
                justify: options.symbol.justify,
                spacing: options.symbol.letter_spacing,
            },
        );

        // The label's extent decides both where it fits and how far the bend check looks.
        let anchors = if options.centred {
            get_center_anchor(
                &label.line,
                options.max_angle,
                shaping.left,
                shaping.right,
                label.icon.0,
                label.icon.1,
                ONE_EM,
                1.0,
            )
            .into_iter()
            .collect()
        } else {
            get_anchors(
                &label.line,
                options.spacing,
                options.max_angle,
                shaping.left,
                shaping.right,
                label.icon.0,
                label.icon.1,
                ONE_EM,
                1.0,
                options.overscaling,
            )
        };

        // Shaped once; emitted once per anchor.
        let quads = quads::glyph_quads(
            &shaping,
            |codepoint| {
                let (metrics, _) = glyphs.metrics(codepoint)?;
                Some(Placed {
                    rect: glyphs.rect(codepoint)?,
                    metrics,
                })
            },
            &quads::Options {
                along_line: true,
                ..quads::Options::default()
            },
        );

        for anchor in anchors {
            let before = buffers.glyphs();
            for quad in &quads {
                #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
                buffers.add_quad(
                    anchor.point,
                    [quad.tl, quad.tr, quad.bl, quad.br],
                    quad.glyph_offset,
                    (
                        quad.tex.x as u16,
                        quad.tex.y as u16,
                        quad.tex.width as u16,
                        quad.tex.height as u16,
                    ),
                    SizeRange::constant(options.symbol.size),
                    true,
                    1.0,
                );
            }
            out.push(LaidOut {
                pending: label.pending,
                anchor: anchor.point,
                extent: (shaping.top, shaping.bottom, shaping.left, shaping.right),
                glyphs: buffers.glyphs() - before,
                content_margins: None,
                segment: anchor.segment,
                vertices: before * 4..buffers.vertices.len(),
            });
        }
    }

    (buffers, out)
}

/// One icon to place: which sprite, and where.
#[derive(Debug, Clone, PartialEq)]
pub struct IconLabel {
    /// Which of the layout's pending symbols this is, stamped into every instance it produces.
    pub pending: usize,
    /// The sprite name the layer's `icon-image` resolved to.
    pub image: alloc::string::String,
    /// Where it is anchored, in tile units.
    pub anchor: (f32, f32),
    /// How this feature's icon is set.
    pub options: IconOptions,
    /// The shaped label this icon is drawn around, as `(top, bottom, left, right)`.
    ///
    /// `None` for an icon with no text, which is most markers. `icon-text-fit` needs it and does
    /// nothing without it: an icon told to stretch to a label that is not there has no size to
    /// stretch to, so it keeps its own.
    pub text: Option<(f32, f32, f32, f32)>,
}

/// How a symbol layer draws its icons.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct IconOptions {
    /// `icon-size`, a *multiplier* rather than a pixel size — the sprite already has one.
    pub size: f32,
    /// `icon-offset`, in logical pixels.
    pub offset: [f32; 2],
    /// `icon-rotate`, in radians.
    pub rotate: f32,
    /// Which part of the icon touches the anchor.
    pub anchor: tessella_glyph::shaping::Anchor,
    /// `icon-text-fit`: which of the icon's axes stretch around the label.
    pub text_fit: tessella_glyph::quads::IconTextFit,
    /// `icon-text-fit-padding`, in logical pixels, ordered top, right, bottom, left as the spec
    /// writes it.
    pub text_fit_padding: [f32; 4],
}

impl Default for IconOptions {
    fn default() -> Self {
        Self {
            // One, not sixteen. `icon-size` scales a sprite that is already the size its author
            // drew it; `text-size` names a size outright. Treating icon-size like text-size draws
            // every marker sixteen times too large.
            size: 1.0,
            offset: [0.0, 0.0],
            rotate: 0.0,
            anchor: tessella_glyph::shaping::Anchor::Center,
            text_fit: tessella_glyph::quads::IconTextFit::None,
            text_fit_padding: [0.0; 4],
        }
    }
}

/// Lays out a layer's icons into one tile's buffers.
///
/// The icon counterpart of [`build_symbols`], and separate from it because the two halves are
/// separate *drawables*: text draws through `SymbolSDFShader` and an icon through
/// `SymbolIconShader`, so they cannot share a buffer even when they belong to the same symbol.
///
/// `positions` is where each icon sits in the *icon atlas* — not in the sprite sheet. mbgl cuts
/// every icon out of the sheet and repacks it with a pixel of padding around it, and that pixel
/// is what the quad's one-pixel border samples. Drawing straight from the sheet, where icons are
/// usually flush against each other, puts a hairline of the neighbouring picture around every
/// marker on the map.
///
/// An icon naming a sprite the sheet does not have is skipped — mbgl does the same, and it is
/// why the layout records the name it asked for rather than a resolved rectangle: the sheet may
/// not have arrived yet, and a style naming one missing icon still draws the rest.
#[must_use]
pub fn build_icons(
    labels: &[IconLabel],
    positions: &tessella_glyph::sprite::Positions,
) -> (SymbolBuffers, Vec<LaidOut>) {
    use tessella_glyph::quads::{icon_quad, shape_icon};

    let mut buffers = SymbolBuffers::default();
    let mut out = Vec::with_capacity(labels.len());

    for label in labels {
        let Some(position) = positions.get(&label.image) else {
            continue;
        };
        let (width, height) = position.display_size();
        #[allow(clippy::cast_possible_truncation)]
        let size = (width as f32, height as f32);
        if size.0 <= 0.0 || size.1 <= 0.0 {
            continue;
        }

        let mut placed = shape_icon(size, label.options.offset, label.options.anchor);

        // Stretch it around the label, if the layer says to and there is a label to stretch to.
        // The sprite then constrains how far that stretch may distort it.
        if label.options.text_fit != tessella_glyph::quads::IconTextFit::None
            && let Some(text) = label.text
        {
            let [top, right, bottom, left] = label.options.text_fit_padding;
            placed = tessella_glyph::quads::fit_icon_to_text(
                placed,
                size,
                text,
                label.options.text_fit,
                (top, bottom, left, right),
                label.options.offset,
                1.0,
            );
            if let Some(content) = position.content {
                #[allow(clippy::cast_possible_truncation)]
                let content = (
                    content.left as f32,
                    content.top as f32,
                    content.right as f32,
                    content.bottom as f32,
                );
                placed = tessella_glyph::quads::apply_text_fit(
                    placed,
                    content,
                    position.text_fit_width,
                    position.text_fit_height,
                );
            }
        }
        // The *padded* rectangle, which is a pixel larger than the icon on every side and is
        // exactly what the quad's border covers.
        let quad = icon_quad(placed, position.padded_rect, label.options.rotate);

        let before = buffers.glyphs();
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        buffers.add_quad(
            label.anchor,
            [quad.tl, quad.tr, quad.bl, quad.br],
            quad.glyph_offset,
            (
                quad.tex.x as u16,
                quad.tex.y as u16,
                quad.tex.width as u16,
                quad.tex.height as u16,
            ),
            SizeRange::constant(label.options.size),
            // The sprite decides, not the layer. A shield drawn as a distance field is
            // recolourable by `icon-color`; a photographic icon is not, and putting a plain
            // image through the SDF shader draws its alpha as a coverage ramp.
            position.sdf,
            1.0,
        );

        // The *box*, not the quad: collision measures what the icon occupies and the quad is a
        // pixel larger on every side for sampling.
        // The margins only mean something once fitting has made the extent a content area. An
        // unfitted icon's extent already *is* its picture, and adding them would reserve a
        // border twice.
        let margins = (label.options.text_fit != tessella_glyph::quads::IconTextFit::None
            && label.text.is_some())
        .then(|| position.content_margins());

        out.push(LaidOut {
            pending: label.pending,
            anchor: label.anchor,
            extent: (placed.top, placed.bottom, placed.left, placed.right),
            glyphs: buffers.glyphs() - before,
            content_margins: margins,
            segment: 0,
            vertices: before * 4..buffers.vertices.len(),
        });
    }

    (buffers, out)
}
