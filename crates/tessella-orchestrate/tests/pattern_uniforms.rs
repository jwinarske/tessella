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
