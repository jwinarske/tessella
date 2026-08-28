//! A label whose sections are set at different sizes (§R2's per-section scaling).
//!
//! # Why this has its own capture
//!
//! `symbol_style.dump` is the R2 symbol reference and the golden README's rule is that a new
//! question gets a new capture rather than an amended one. Adding a layer to it moved every
//! count nine tests assert, none of which is about scaling.
//!
//! # Why it can be checked at all
//!
//! It could not be, until the probe emitted a per-attribute hash. Three attributes share the
//! glyph vertex buffer and only one carries texture coordinates, but the whole buffer had to be
//! elided because the glyph atlas is packed in arrival order — so a capture of this label and a
//! capture of the same label at scale one were byte-identical in everything compared. Measured,
//! not assumed: that is why `fld=` exists.
//!
//! # What a scaled section changes
//!
//! Three things, and mbgl reads the scale at each of them. The glyph's *advance*, so the pen
//! moves further — `metrics.advance * scale + spacing`, the spacing unscaled, so a double-size
//! word is not also a loosely-set one. The *line's height*, which takes the largest scale on it,
//! with every glyph offset by `(lineMaxScale - scale) * ONE_EM` so the small text keeps its feet
//! on the same baseline instead of floating. And the *quad*, since a bigger glyph covers more of
//! the atlas rectangle it samples.
//!
//! Getting one of the three wrong draws text of the right size in the wrong place, or the wrong
//! size in the right one — which is exactly the failure a capture that could not see any of it
//! would have let through.

use tessella_glyph::shaping::{Char, Options as ShapeOptions};
use tessella_glyph::text::ONE_EM;

/// FNV-1a, as the probe hashes with.
fn fnv1a(bytes: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in bytes {
        hash = (hash ^ u64::from(*byte)).wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

const DUMP: &str = include_str!("../../../tests/golden/scaled_style.dump");

/// The symbol drawable's vertex count and its layout attribute's field hash.
fn golden() -> (usize, u64) {
    let mut found = None;
    for line in DUMP.lines() {
        if !line.contains("sh0033") {
            continue;
        }
        if let Some(rest) = line.trim().strip_prefix("attr ")
            && rest.contains(" id=0 ")
            && let Some(hash) = rest
                .split_whitespace()
                .find_map(|token| token.strip_prefix("fld="))
            && let Ok(hash) = u64::from_str_radix(hash, 16)
        {
            let vertices = rest
                .split('.')
                .find_map(|part| part.strip_prefix('v'))
                .and_then(|part| part.split('#').next())
                .and_then(|part| part.parse().ok())
                .expect("a vertex count");
            found = Some((vertices, hash));
        }
    }
    found.expect("the capture holds a symbol drawable")
}

/// The oracle shaped both sections.
#[test]
fn both_sections_are_laid_out() {
    let (vertices, _) = golden();
    assert_eq!(
        vertices, 32,
        "eight glyphs of four vertices: `Big` at twice the size and `small` at half, both drawn"
    );
}

/// A line's height is its largest section's, and the smaller text sits on the same baseline.
///
/// The arithmetic mbgl does, checked here rather than through the capture because the capture
/// hashes the result and this is the rule that produces it. `Big` is at two and `small` at a
/// half, so the line is two ems tall and every `small` glyph drops by one and a half of one.
#[test]
fn the_line_takes_its_largest_scale() {
    let chars: Vec<Char> = "Big"
        .chars()
        .map(|c| Char::new(c as u32, 10.0).at_scale(2.0))
        .chain(
            "small"
                .chars()
                .map(|c| Char::new(c as u32, 10.0).at_scale(0.5)),
        )
        .collect();

    let shaping = tessella_glyph::shaping::shape(
        &chars,
        &ShapeOptions {
            max_width: 0.0,
            line_height: ONE_EM,
            ..ShapeOptions::default()
        },
    );

    let line = shaping.lines.first().expect("one line");
    assert_eq!(line.len(), 8);

    // The big section defines the baseline, so it is not offset at all.
    for glyph in &line[..3] {
        assert!(
            (glyph.y - line[0].y).abs() < f32::EPSILON,
            "the largest section sets the baseline"
        );
    }
    // And the small section drops by the difference, rather than floating at the line's top.
    let drop = line[3].y - line[0].y;
    assert!(
        (drop - (2.0 - 0.5) * ONE_EM).abs() < 0.01,
        "small text should drop by `(lineMaxScale - scale) * ONE_EM`, dropped by {drop}"
    );

    // Two ems tall, not one: a line with a double-size word is a double-height line.
    assert!(
        (shaping.bottom - shaping.top - 2.0 * ONE_EM).abs() < 0.01,
        "height {} should be two ems",
        shaping.bottom - shaping.top
    );
}

/// An unscaled label is unchanged, which is every label a style has ever written.
#[test]
fn one_section_at_one_is_what_it_always_was() {
    let plain: Vec<Char> = "Big".chars().map(|c| Char::new(c as u32, 10.0)).collect();
    let marked: Vec<Char> = plain.iter().map(|c| c.at_scale(1.0)).collect();

    let options = ShapeOptions {
        max_width: 0.0,
        line_height: ONE_EM,
        ..ShapeOptions::default()
    };
    assert_eq!(
        tessella_glyph::shaping::shape(&plain, &options),
        tessella_glyph::shaping::shape(&marked, &options),
        "a scale of one is the identity, so nothing that never writes `format` moves"
    );
}

/// The hash the capture carries, for the test that will compare against it.
///
/// Not compared yet. Reaching it needs the tile builder to run this style end to end, which
/// wants the sprite-less symbol path the other parity tests take; what is asserted here is that
/// the capture *has* a value to compare with, so the next step has a target rather than a hole.
#[test]
fn the_capture_carries_a_layout_hash() {
    let (_, hash) = golden();
    assert_ne!(hash, 0, "the layout attribute was not elided");
    assert_ne!(
        fnv1a(&[]),
        hash,
        "and it is a hash of something rather than of nothing"
    );
}
