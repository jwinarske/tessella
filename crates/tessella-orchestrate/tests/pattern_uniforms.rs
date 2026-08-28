//! `FillPatternTilePropsUBO`, checked against the oracle's own bytes.
//!
//! # Where the expected values come from
//!
//! `tests/golden/pattern_style.dump`, captured from the probe on a style with four pattern
//! layers. The pattern buffers themselves are elided in the committed golden — mbgl packs its
//! shared image atlas in the order images arrive and that order is not deterministic, so the
//! *origin* of every rectangle moves between captures. What does not move is the block's shape:
//! two `vec4` rectangles, a `vec2` atlas size, two words of padding, forty-eight bytes.
//!
//! So this pins the layout against the capture's structure and the values against arithmetic
//! taken from it — the decoded blocks of a real run, which is where `[512, 512, 0, 0]` and the
//! fifty-by-fifty rectangle below came from.

use tessella_orchestrate::ubo::{PatternPlacement, pack_pattern_tile_props};

/// One drawable's block is forty-eight bytes in the order the layout declares.
#[test]
fn a_block_is_two_rectangles_a_size_and_padding() {
    // The decoded blocks of one capture: `sand_noise`, fifty by fifty, in a 512x512 atlas.
    let placement = PatternPlacement {
        from: [56, 9, 106, 59],
        to: [56, 9, 106, 59],
        texsize: [512, 512],
    };
    let packed = pack_pattern_tile_props(&[placement]);
    assert_eq!(packed.len(), 48, "the block is forty-eight bytes");

    let word = |at: usize| f32::from_le_bytes(packed[at..at + 4].try_into().expect("four bytes"));

    // pattern_from at 0, pattern_to at 16, texsize at 32.
    assert_eq!(
        [word(0), word(4), word(8), word(12)],
        [56.0, 9.0, 106.0, 59.0]
    );
    assert_eq!(
        [word(16), word(20), word(24), word(28)],
        [56.0, 9.0, 106.0, 59.0]
    );
    assert_eq!([word(32), word(36)], [512.0, 512.0]);
    assert_eq!([word(40), word(44)], [0.0, 0.0], "pad1 and pad2 are zero");
}

/// The bytes are floats, because the shader declares `vec4`.
///
/// A rectangle is whole pixels and writing it as `u32` would pack the same number of bytes —
/// and be read as a denormal, which is a pattern sampled from the corner of the atlas rather
/// than a failure anyone would see in a count.
#[test]
fn a_rectangle_is_written_as_floats() {
    let packed = pack_pattern_tile_props(&[PatternPlacement {
        from: [56, 0, 0, 0],
        to: [0; 4],
        texsize: [0; 2],
    }]);
    assert_eq!(
        &packed[0..4],
        &56f32.to_le_bytes(),
        "56.0 as f32, not 56u32"
    );
    assert_ne!(&packed[0..4], &56u32.to_le_bytes());
}

/// A constant pattern's two rectangles are identical, which the capture shows directly.
///
/// For the constant `fill-pattern` layer the oracle emitted 576 bytes over twelve drawables:
/// twelve blocks of the atlas size and twenty-four of the rectangle, two per drawable. Equal
/// `from` and `to` is what produces that ratio.
#[test]
fn a_constant_pattern_repeats_its_rectangle() {
    let rect = [76, 175, 124, 223];
    let packed = pack_pattern_tile_props(&[PatternPlacement {
        from: rect,
        to: rect,
        texsize: [512, 512],
    }]);
    assert_eq!(&packed[0..16], &packed[16..32], "from and to agree");
}

/// Twelve drawables give the 576 bytes the capture carries.
#[test]
fn the_buffer_is_one_block_per_drawable() {
    // Six tiles, each a fill and its outline: the capture's `ubo layer:1 slot=4 size=576`.
    let entries = [PatternPlacement {
        from: [1, 2, 3, 4],
        to: [5, 6, 7, 8],
        texsize: [512, 512],
    }; 12];
    assert_eq!(pack_pattern_tile_props(&entries).len(), 576);

    // And the line layer's, which the capture gives as 384 over eight.
    assert_eq!(pack_pattern_tile_props(&entries[..8]).len(), 384);
}

/// A stepped pattern's rectangles differ, and the sizes are the padded ones.
///
/// `hospital_striped` and `school_striped` are three by three in the sheet and the capture shows
/// them as `[2, 2, 5, 5]` — a five-by-five padded rectangle at origin one, inset by the padding
/// on every side. A `tlbr` computed from the unpadded size would give `[2, 2, 3, 3]`.
#[test]
fn a_stepped_pattern_carries_two_different_rectangles() {
    use tessella_style::crossfade::tlbr;

    // Padded rect five by five at (1, 1), padding one: the capture's numbers exactly.
    assert_eq!(tlbr(1, 1, 5, 5, 1), [2, 2, 5, 5]);

    let packed = pack_pattern_tile_props(&[PatternPlacement {
        from: tlbr(1, 1, 5, 5, 1),
        to: tlbr(7, 1, 5, 5, 1),
        texsize: [512, 512],
    }]);
    assert_ne!(&packed[0..16], &packed[16..32], "the two images differ");
}

/// The atlas rectangle, and the convention trap in getting there.
mod rects {
    use tessella_glyph::atlas::Rect;
    use tessella_glyph::sprite::IconPosition;
    use tessella_orchestrate::ubo::{atlas_rect, pattern_placement};

    fn position(x: u32, y: u32, width: u32, height: u32) -> IconPosition {
        IconPosition {
            padded_rect: Rect {
                x,
                y,
                width,
                height,
            },
            pixel_ratio: 1.0,
            sdf: false,
            content: None,
            text_fit_width: None,
            text_fit_height: None,
        }
    }

    /// The rectangle is the sprite's own bounds, reached by insetting the reported one.
    ///
    /// This test used to build an `IconPosition` whose rectangle *was* the sprite and assert the
    /// conversion passed it through — which proved only that the fixture and the conversion
    /// agreed, and they agreed on the wrong answer.
    ///
    /// The invariant, from `IconAtlas::add`: `atlas::PADDING` is two, so a sprite of width `W`
    /// occupies a slot of `W + 2 * PADDING` and the reported rectangle is the slot inset by one
    /// — `W + PADDING`, the sprite plus a pixel each side. The fixture is built from that
    /// constant here rather than from a number I chose, so a change to the padding fails the
    /// test rather than silently moving what it asserts.
    #[test]
    fn a_rectangle_is_the_sprite_not_its_border() {
        use tessella_glyph::atlas::PADDING;

        const SIDE: u32 = 50;
        // What the atlas reports for a fifty-pixel sprite packed at the origin.
        let placed = position(1, 1, SIDE + PADDING, SIDE + PADDING);

        let rect = atlas_rect(&placed);
        assert_eq!(
            [rect[2] - rect[0], rect[3] - rect[1]],
            [SIDE as u16, SIDE as u16],
            "the rectangle is the sprite's size, not the reported rectangle's"
        );
        assert_eq!(rect, [2, 2, 52, 52], "inset by one on every side");

        // The display size reaches the same number by the other route, subtracting the pixel of
        // padding the rectangle reports where this insets it. They have to agree, and a
        // convention error moves one and not the other.
        let (width, height) = placed.display_size();
        assert_eq!(
            [width as u16, height as u16],
            [rect[2] - rect[0], rect[3] - rect[1]],
            "display size and rectangle size disagree"
        );
    }

    /// A pattern that does not vary carries one rectangle twice.
    #[test]
    fn a_constant_pattern_places_one_image_twice() {
        let sand = position(56, 9, 52, 52);
        let placed = pattern_placement(Some(&sand), Some(&sand), [512, 512]).expect("placed");
        assert_eq!(placed.from, placed.to);
        assert_eq!(placed.texsize, [512, 512]);
    }

    /// A missing image places nothing.
    ///
    /// Drawing it against whatever rectangle was to hand would sample a different sprite at full
    /// opacity, with nothing in the stream saying the pattern was never found.
    #[test]
    fn a_missing_image_places_nothing() {
        let present = position(1, 1, 4, 4);
        assert!(pattern_placement(None, Some(&present), [512, 512]).is_none());
        assert!(pattern_placement(Some(&present), None, [512, 512]).is_none());
        assert!(pattern_placement(None, None, [512, 512]).is_none());
    }
}

/// A line pattern's block, against the capture's decoded values.
mod line {
    use tessella_orchestrate::ubo::{
        LinePatternPlacement, PatternPlacement, pack_line_pattern_tile_props,
    };
    use tessella_style::crossfade::Crossfade;

    /// The capture's `ubo layer:3 slot=3`, field for field.
    ///
    /// Six drawables at sixty-four bytes is the 384 the oracle wrote. Its blocks decoded to a
    /// rectangle of `[2, 9, 52, 59]`, a scale of `[1, 0.0625, 0.5, 1]` and a texsize-plus-fade
    /// of `[512, 512, 1, 0]`.
    ///
    /// `0.0625` is one sixteenth, and sixteen is `pixels_to_tile_units(1)` at a tile drawn at
    /// its own level: the extent, 8192, over the tile size, 512. Getting that term wrong scales
    /// every pattern by a power of two and still renders.
    ///
    /// `[0.5, 1.0]` is the crossfade zooming *out*, which is what an integer camera zoom gives:
    /// `z > last_integer_zoom` is false at exactly thirteen.
    #[test]
    fn a_line_block_matches_the_capture() {
        let entry = LinePatternPlacement {
            placement: PatternPlacement {
                from: [2, 9, 52, 59],
                to: [2, 9, 52, 59],
                texsize: [512, 512],
            },
            pixel_ratio: 1.0,
            units_per_pixel: 1.0 / 16.0,
            crossfade: Crossfade {
                from_scale: 0.5,
                to_scale: 1.0,
                t: 1.0,
            },
        };
        let packed = pack_line_pattern_tile_props(&[entry]);
        assert_eq!(packed.len(), 64, "a line's block is sixty-four bytes");

        let word =
            |at: usize| f32::from_le_bytes(packed[at..at + 4].try_into().expect("four bytes"));
        assert_eq!(
            [word(0), word(4), word(8), word(12)],
            [2.0, 9.0, 52.0, 59.0]
        );
        assert_eq!(
            [word(16), word(20), word(24), word(28)],
            [2.0, 9.0, 52.0, 59.0]
        );
        assert_eq!(
            [word(32), word(36), word(40), word(44)],
            [1.0, 0.0625, 0.5, 1.0],
            "scale is pixel ratio, units per pixel, then the crossfade's two"
        );
        assert_eq!([word(48), word(52)], [512.0, 512.0], "texsize");
        assert_eq!(word(56), 1.0, "fade is the crossfade's t");
        assert_eq!(word(60), 0.0, "pad1");
    }

    /// Six drawables give the 384 bytes the capture carries.
    #[test]
    fn the_buffer_is_one_block_per_drawable() {
        let entry = LinePatternPlacement {
            placement: PatternPlacement {
                from: [1, 2, 3, 4],
                to: [1, 2, 3, 4],
                texsize: [512, 512],
            },
            pixel_ratio: 1.0,
            units_per_pixel: 0.0625,
            crossfade: Crossfade {
                from_scale: 0.5,
                to_scale: 1.0,
                t: 1.0,
            },
        };
        assert_eq!(pack_line_pattern_tile_props(&[entry; 6]).len(), 384);
    }

    /// A line's block is wider than a fill's, and that is the whole difference.
    ///
    /// Sixty-four against forty-eight: a `scale` vector and a `fade`. Packing a line's placement
    /// with a fill's packer would write blocks a quarter short and every drawable after the
    /// first would read the previous one's tail.
    #[test]
    fn a_line_block_is_wider_than_a_fill_block() {
        use tessella_orchestrate::ubo::pack_pattern_tile_props;

        let placement = PatternPlacement {
            from: [1, 2, 3, 4],
            to: [1, 2, 3, 4],
            texsize: [512, 512],
        };
        assert_eq!(pack_pattern_tile_props(&[placement]).len(), 48);
        assert_eq!(
            pack_line_pattern_tile_props(&[LinePatternPlacement {
                placement,
                pixel_ratio: 1.0,
                units_per_pixel: 0.0625,
                crossfade: Crossfade {
                    from_scale: 0.5,
                    to_scale: 1.0,
                    t: 1.0
                },
            }])
            .len(),
            64
        );
    }
}

/// A background pattern's block, against the capture's decoded values.
mod background {
    use tessella_orchestrate::ubo::{
        BackgroundPatternPlacement, PatternPlacement, pack_background_pattern_props,
    };
    use tessella_style::crossfade::Crossfade;

    /// The capture's `ubo layer:0 slot=5`, field for field.
    ///
    /// Sixty-four bytes, one block for the layer rather than one per drawable — a background has
    /// no tiles to vary over. Its blocks decoded to `[1, 1, 51, 51]` twice, `[50, 50, 50, 50]`,
    /// and `[0.5, 1, 1, 1]`.
    ///
    /// `grass_pattern` is fifty by fifty, so its padded slot is fifty-two and its rectangle runs
    /// from one to fifty-one. The sizes are the *display* sizes, fifty, which agree with the
    /// rectangle's width only because the sheet is not retina.
    ///
    /// `[0.5, 1]` is the crossfade zooming out and `1` its mix, the same pair the line layer
    /// carries at the same camera.
    #[test]
    fn a_background_block_matches_the_capture() {
        let packed = pack_background_pattern_props(&BackgroundPatternPlacement {
            placement: PatternPlacement {
                from: [1, 1, 51, 51],
                to: [1, 1, 51, 51],
                texsize: [512, 512],
            },
            display: [[50.0, 50.0], [50.0, 50.0]],
            crossfade: Crossfade {
                from_scale: 0.5,
                to_scale: 1.0,
                t: 1.0,
            },
            opacity: 1.0,
        });
        assert_eq!(packed.len(), 64);

        let word =
            |at: usize| f32::from_le_bytes(packed[at..at + 4].try_into().expect("four bytes"));
        // tl_a, br_a as two vec2s rather than one vec4 — the same numbers, split.
        assert_eq!([word(0), word(4)], [1.0, 1.0], "pattern_tl_a");
        assert_eq!([word(8), word(12)], [51.0, 51.0], "pattern_br_a");
        assert_eq!([word(16), word(20)], [1.0, 1.0], "pattern_tl_b");
        assert_eq!([word(24), word(28)], [51.0, 51.0], "pattern_br_b");
        assert_eq!([word(32), word(36)], [50.0, 50.0], "pattern_size_a");
        assert_eq!([word(40), word(44)], [50.0, 50.0], "pattern_size_b");
        assert_eq!(word(48), 0.5, "scale_a is the crossfade's from");
        assert_eq!(word(52), 1.0, "scale_b is its to");
        assert_eq!(word(56), 1.0, "mix is its t");
        assert_eq!(word(60), 1.0, "opacity");
    }

    /// The three pattern blocks are three different shapes, and none is another's.
    ///
    /// A fill is forty-eight bytes of two rectangles and a size. A line is sixty-four, adding a
    /// scale vector and a fade. A background is sixty-four arranged differently again: corners
    /// rather than rectangles, display sizes, and the crossfade beside the opacity. Reaching for
    /// the last one's packer because the size matched is the mistake this guards.
    #[test]
    fn the_three_blocks_are_not_interchangeable() {
        use tessella_orchestrate::ubo::{
            LinePatternPlacement, pack_line_pattern_tile_props, pack_pattern_tile_props,
        };

        let placement = PatternPlacement {
            from: [1, 1, 51, 51],
            to: [1, 1, 51, 51],
            texsize: [512, 512],
        };
        let crossfade = Crossfade {
            from_scale: 0.5,
            to_scale: 1.0,
            t: 1.0,
        };

        let fill = pack_pattern_tile_props(&[placement]);
        let line = pack_line_pattern_tile_props(&[LinePatternPlacement {
            placement,
            pixel_ratio: 1.0,
            units_per_pixel: 0.0625,
            crossfade,
        }]);
        let background = pack_background_pattern_props(&BackgroundPatternPlacement {
            placement,
            display: [[50.0, 50.0], [50.0, 50.0]],
            crossfade,
            opacity: 1.0,
        });

        assert_eq!((fill.len(), line.len(), background.len()), (48, 64, 64));
        // The line and the background are the same length and different bytes, which is the
        // case a length check alone would miss.
        assert_ne!(line, background);
    }
}

/// A data-driven pattern's per-vertex rectangles.
mod data_driven {
    use tessella_capture_abi::envelope::{AttributeDesc, GeometryId, Span, WireRecord as _};
    use tessella_orchestrate::emit::{FillDraw, PatternVertices};
    use tessella_orchestrate::{SlabArena, encode_fill};
    use tessella_style::LayerKind;

    fn bucket() -> tessella_layout::fill::FillBucket {
        // A square: five vertices, the last repeating the first.
        tessella_layout::fill::build(&[vec![[0, 0], [0, 16], [16, 16], [16, 0], [0, 0]]])
    }

    fn descriptors(encoded: &tessella_orchestrate::Encoded) -> Vec<AttributeDesc> {
        let span: Span = encoded.record.attrs;
        (0..span.count as usize)
            .filter_map(|index| {
                encoded
                    .payload
                    .get(span.offset as usize + index * size_of::<AttributeDesc>()..)
                    .and_then(AttributeDesc::from_bytes)
            })
            .collect()
    }

    /// The capture's shape: ids 4 and 5, bindings 1 and 2, `UShort4` at a stride of eight.
    ///
    /// `L00004`'s drawables carry `id=4 bind=1 dt=15 ddt=15 off=0 stride=8` and the same at
    /// id 5 — and `dt=15` is `UShort4`, which is a `tlbr` exactly. The constant layer beside it
    /// carries neither, which is what says these are the composite binder's addition rather
    /// than something every pattern writes.
    #[test]
    fn a_data_driven_pattern_adds_two_attributes() {
        let bucket = bucket();
        let rect = [57u16, 10, 107, 60];
        let vertices = PatternVertices {
            from: vec![rect; bucket.vertices.len()],
            to: vec![rect; bucket.vertices.len()],
        };

        let mut arena = SlabArena::new();
        let layout = tessella_orchestrate::binder::VertexLayout::default();
        let (encoded, _) = encode_fill(
            &mut arena,
            GeometryId(1),
            &bucket,
            &FillDraw::new(&layout, &[], 0, None, None).with_pattern_vertices(&vertices),
        );

        let attrs = descriptors(&encoded);
        let pattern: Vec<&AttributeDesc> = attrs
            .iter()
            .filter(|attr| attr.attr_id == 4 || attr.attr_id == 5)
            .collect();
        assert_eq!(pattern.len(), 2, "two pattern attributes: {attrs:?}");

        for (attr, binding) in pattern.iter().zip([1, 2]) {
            assert_eq!(attr.binding, binding, "binding for id {}", attr.attr_id);
            assert_eq!(attr.stride, 8, "four u16 per vertex");
            assert_eq!(attr.offset, 0, "each in a buffer of its own");
            // UShort4 is 15, which is what the capture's `dt=15` is.
            assert_eq!(attr.data_type, 15);
            assert_eq!(attr.declared_data_type, 15);
            assert_eq!(
                attr.source.length as usize,
                bucket.vertices.len() * 8,
                "one rectangle per vertex"
            );
        }
        let _ = LayerKind::Fill;
    }

    /// A constant pattern adds neither, which is what the capture shows beside it.
    #[test]
    fn a_constant_pattern_adds_no_attributes() {
        let bucket = bucket();
        let mut arena = SlabArena::new();
        let layout = tessella_orchestrate::binder::VertexLayout::default();
        let (encoded, _) = encode_fill(
            &mut arena,
            GeometryId(1),
            &bucket,
            &FillDraw::new(&layout, &[], 0, None, None),
        );
        assert!(
            descriptors(&encoded)
                .iter()
                .all(|attr| attr.attr_id != 4 && attr.attr_id != 5),
            "a constant pattern writes no per-vertex rectangles"
        );
    }

    /// Rectangles that do not cover every vertex are refused rather than written short.
    ///
    /// mbgl fills a feature whose pattern did not resolve with `{0, 0, 0, 0}` rather than
    /// leaving the buffer short, and says why: it cannot know at draw time whether every feature
    /// resolved. A short buffer is a read past its end for every vertex after the gap, which is
    /// worse than a layer that draws without its pattern.
    #[test]
    fn a_short_buffer_is_refused() {
        let bucket = bucket();
        let vertices = PatternVertices {
            from: vec![[1, 2, 3, 4]; bucket.vertices.len() - 1],
            to: vec![[1, 2, 3, 4]; bucket.vertices.len()],
        };
        assert!(!vertices.covers(bucket.vertices.len()));

        let mut arena = SlabArena::new();
        let layout = tessella_orchestrate::binder::VertexLayout::default();
        let (encoded, _) = encode_fill(
            &mut arena,
            GeometryId(1),
            &bucket,
            &FillDraw::new(&layout, &[], 0, None, None).with_pattern_vertices(&vertices),
        );
        assert!(
            descriptors(&encoded)
                .iter()
                .all(|attr| attr.attr_id != 4 && attr.attr_id != 5),
            "a short buffer must not reach the wire"
        );
    }
}
