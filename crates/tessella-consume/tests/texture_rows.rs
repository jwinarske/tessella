// SPDX-License-Identifier: BSD-2-Clause
//! Where a texture update's pixels are, in each of the two forms the wire carries.
//!
//! # What this is for
//!
//! `TextureUpdate::packed` has two payload shapes behind one rect list, and its own documentation
//! says what reading the wrong one costs:
//!
//! > the rects would land at the right addresses holding the wrong pixels, which is a map that
//! > draws rather than one that fails
//!
//! `upload::rows` is the one place that difference is resolved, so a backend never branches on the
//! flag. These are the cases that would make it resolve wrongly.
//!
//! # What would be caught
//!
//! A region sourced at the wrong offset or the wrong stride. Both upload the right number of bytes
//! to the right place in the image, taken from the wrong rows -- an atlas whose glyphs are each
//! other's, which draws. The bounds cases are the same failure one step earlier: a rect past the
//! image, or a payload shorter than the layout claims, is a read past what the producer sent.

use tessella_capture_abi::envelope::{Extent, Rect16};
use tessella_capture_abi::{TextureChannelDataType, TexturePixelType};
use tessella_consume::upload::{self, Mismatch, Rows, Shape};

const BYTE: TextureChannelDataType = TextureChannelDataType::UnsignedByte;
const RGBA: TexturePixelType = TexturePixelType::RGBA;

const fn rect(x: u16, y: u16, w: u16, h: u16) -> Rect16 {
    Rect16 { x, y, w, h }
}

const fn size(width: u32, height: u32) -> Extent {
    Extent { width, height }
}

/// A shape, named by its three parts in the order they are argued about.
const fn shape(format: TexturePixelType, channel: TextureChannelDataType, packed: bool) -> Shape {
    Shape {
        format,
        channel,
        packed,
    }
}

/// A texel is both of mbgl's factors, not just the channel count.
///
/// `getStorageSize` multiplies them, and a consumer using the channel count alone sizes a color
/// relief's elevation stops at a quarter -- which is the texture the channel type was added for.
#[test]
fn a_texel_is_channels_times_channel_size() {
    assert_eq!(shape(RGBA, BYTE, false).texel(), 4);
    assert_eq!(
        shape(RGBA, TextureChannelDataType::Float, false).texel(),
        16
    );
    assert_eq!(
        shape(RGBA, TextureChannelDataType::HalfFloat, false).texel(),
        8
    );
    assert_eq!(shape(TexturePixelType::Alpha, BYTE, false).texel(), 1);
    assert_eq!(
        shape(
            TexturePixelType::Alpha,
            TextureChannelDataType::Float,
            false
        )
        .texel(),
        4
    );
}

/// Packed regions sit end to end, each at its own width.
#[test]
fn packed_regions_are_tight_at_their_own_widths() {
    let rects = [rect(0, 0, 2, 2), rect(8, 8, 4, 4), rect(1, 1, 1, 3)];
    // (2*2 + 4*4 + 1*3) * 4 bytes.
    let found =
        upload::rows(size(64, 64), shape(RGBA, BYTE, true), &rects, 23 * 4).expect("a layout");
    assert_eq!(
        found,
        [
            Rows { at: 0, stride: 8 },
            Rows { at: 16, stride: 16 },
            Rows { at: 80, stride: 4 },
        ],
        "each region starts where the one before it ended, at its own row width"
    );
}

/// A whole payload's regions are windows into it, all at the texture's stride.
#[test]
fn whole_regions_are_windows_at_the_texture_stride() {
    let rects = [rect(0, 0, 2, 2), rect(8, 4, 4, 4)];
    let found = upload::rows(size(64, 64), shape(RGBA, BYTE, false), &rects, 64 * 64 * 4)
        .expect("a layout");
    assert_eq!(
        found,
        [
            Rows { at: 0, stride: 256 },
            // y 4 down at 256 bytes a row, then x 8 across at 4 bytes a texel.
            Rows {
                at: 4 * 256 + 8 * 4,
                stride: 256
            },
        ]
    );
}

/// The two forms disagree about the same rect list, which is why the flag has to be read.
///
/// The assertion behind the whole slice: if a backend took the packed payload for a whole one, it
/// would source region two at byte 1056 instead of byte 16.
#[test]
fn the_two_forms_place_the_same_rects_differently() {
    let rects = [rect(0, 0, 2, 2), rect(8, 4, 4, 4)];
    let whole =
        upload::rows(size(64, 64), shape(RGBA, BYTE, false), &rects, 64 * 64 * 4).expect("whole");
    let packed =
        upload::rows(size(64, 64), shape(RGBA, BYTE, true), &rects, 20 * 4).expect("packed");
    assert_ne!(
        whole[1], packed[1],
        "the flag must change where a region is read from"
    );
    assert_eq!(packed[1], Rows { at: 16, stride: 16 });
    assert_eq!(
        whole[1],
        Rows {
            at: 1056,
            stride: 256
        }
    );
}

/// An empty rect list is one region covering the whole texture.
///
/// How the producer says it has no damage worth describing. `packed` is meaningless then -- the
/// payload is the whole texture either way -- so a stray `true` must not change the answer.
#[test]
fn an_empty_rect_list_covers_the_whole_texture() {
    let whole = upload::rows(size(8, 4), shape(RGBA, BYTE, false), &[], 8 * 4 * 4).expect("whole");
    assert_eq!(whole, [Rows { at: 0, stride: 32 }]);

    let claimed_packed = upload::rows(size(8, 4), shape(RGBA, BYTE, true), &[], 8 * 4 * 4)
        .expect("packed is ignored");
    assert_eq!(
        claimed_packed, whole,
        "with no rects the payload is the whole texture whatever the flag says"
    );
}

/// A rect reaching past the texture is refused, in either form.
#[test]
fn a_rect_past_the_texture_is_refused() {
    let past = [rect(0, 0, 2, 2), rect(62, 0, 4, 4)];
    for packed in [false, true] {
        assert_eq!(
            upload::rows(size(64, 64), shape(RGBA, BYTE, packed), &past, 1 << 20),
            Err(Mismatch::OutOfBounds { rect: 1 }),
            "packed {packed}: the second rect runs two texels past the width"
        );
    }
    // And the vertical case, which a check on x alone would pass.
    let below = [rect(0, 62, 2, 4)];
    assert_eq!(
        upload::rows(size(64, 64), shape(RGBA, BYTE, false), &below, 1 << 20),
        Err(Mismatch::OutOfBounds { rect: 0 })
    );
}

/// A payload shorter than the layout needs is refused rather than read past.
///
/// `TextureUpdate::packed` calls this "the second half of the same guard"; the producer's `fits`
/// test is the first half, and a consumer that trusts it reads bytes it was not given.
#[test]
fn a_short_payload_is_refused() {
    let rects = [rect(0, 0, 2, 2), rect(8, 8, 4, 4)];
    // Packed wants (4 + 16) * 4 = 80.
    assert_eq!(
        upload::rows(size(64, 64), shape(RGBA, BYTE, true), &rects, 79),
        Err(Mismatch::Short {
            wanted: 80,
            got: 79
        })
    );
    assert!(upload::rows(size(64, 64), shape(RGBA, BYTE, true), &rects, 80).is_ok());

    // Whole wants the entire texture, whatever the rects name.
    assert_eq!(
        upload::rows(size(64, 64), shape(RGBA, BYTE, false), &rects, 80),
        Err(Mismatch::Short {
            wanted: 16384,
            got: 80
        }),
        "a rect list does not shrink a whole-texture payload"
    );
}

/// The channel type changes the stride, which is the defect behind carrying it.
///
/// A color relief's elevation stops are `RGBA` and `Float`. Sized by channels alone they would be
/// read at a quarter of their stride, so every row after the first comes from inside the one
/// before it.
#[test]
fn the_channel_type_changes_the_stride() {
    let rects = [rect(1, 1, 2, 2)];
    let bytes =
        upload::rows(size(8, 8), shape(RGBA, BYTE, false), &rects, 8 * 8 * 4).expect("bytes");
    let floats = upload::rows(
        size(8, 8),
        shape(RGBA, TextureChannelDataType::Float, false),
        &rects,
        8 * 8 * 16,
    )
    .expect("floats");

    assert_eq!(bytes[0], Rows { at: 36, stride: 32 });
    assert_eq!(
        floats[0],
        Rows {
            at: 144,
            stride: 128
        },
        "four bytes a channel, so four times the stride and four times the offset"
    );
}

/// A texture larger than a rect can address is clamped rather than wrapped.
///
/// An extent is `u32` and a rect is `u16`. The same reading `textures::updated` takes: the producer
/// could not have described damage on such a texture either, since its own rects are the same type.
#[test]
fn an_extent_past_a_rect_is_clamped() {
    let huge = size(70_000, 1);
    let found = upload::rows(
        huge,
        shape(TexturePixelType::Alpha, BYTE, false),
        &[],
        70_000,
    )
    .expect("a clamped whole region");
    assert_eq!(found.len(), 1);
    assert_eq!(
        found[0],
        Rows {
            at: 0,
            stride: 70_000
        }
    );
}

/// A size whose layout overflows is refused, not wrapped into a plausible one.
///
/// The case the saturating arithmetic exists for, and the numbers are chosen so that wrapping and
/// saturating give *different answers* rather than merely large ones. A width and height of 2^30
/// with a sixteen-byte texel is a layout of exactly 2^64 bytes: wrapped that is zero, which is
/// inside any payload, so the record is accepted and every region reads from an offset nothing
/// computed. Saturated it is `u64::MAX`, which is outside every payload.
///
/// An extent is two `u32`s, so these are values a producer can put on the wire.
#[test]
fn a_layout_that_wraps_to_nothing_is_refused() {
    let billion = 1u32 << 30;
    let found = upload::rows(
        size(billion, billion),
        shape(RGBA, TextureChannelDataType::Float, false),
        &[],
        1 << 20,
    );
    let Err(Mismatch::Short { wanted, .. }) = found else {
        panic!("a layout of exactly 2^64 bytes must be refused, got {found:?}");
    };
    assert_eq!(
        wanted,
        u64::MAX,
        "the total has to saturate; wrapped it is zero, which passes every length check"
    );
}

/// A region larger than a 32-bit `usize` reports its real size.
///
/// This crate is `no_std` and builds for `wasm32`, where a `usize` is thirty-two bits. One maximal
/// rect of RGBA floats is 65535 * 65535 * 16, about 7e10 -- so the count has to be carried in
/// something wider than the target's `usize` or the refusal reports a wrapped number, and the
/// narrowing to an offset has to be a conversion that can fail rather than a cast.
///
/// Only the reported count is observable on a sixty-four bit host: there the narrowing succeeds,
/// which is correct there. The assertion is that the number is the real one.
#[test]
fn a_region_past_a_32_bit_usize_reports_its_real_size() {
    let maximal = [rect(0, 0, u16::MAX, u16::MAX)];
    let found = upload::rows(
        size(u32::from(u16::MAX), u32::from(u16::MAX)),
        shape(RGBA, TextureChannelDataType::Float, true),
        &maximal,
        1 << 20,
    );
    let Err(Mismatch::Short { wanted, got: had }) = found else {
        panic!("a region far past the payload must be refused, got {found:?}");
    };
    assert_eq!(wanted, u64::from(u16::MAX) * u64::from(u16::MAX) * 16);
    assert_eq!(had, 1 << 20);
    assert!(
        wanted > u64::from(u32::MAX),
        "the point of the case is that this does not fit a 32-bit usize"
    );
}
