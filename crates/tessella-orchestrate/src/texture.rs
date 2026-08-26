//! Texture uploads: `TextureUpdate` (§6.4).
//!
//! # Dirty rects, and why the cap is not a limit on correctness
//!
//! A texture upload carries the pixels for up to [`TEXTURE_RECT_CAP`] dirty regions rather than
//! the whole image, because the atlases R2 fills are large and change in small places. §4 gives
//! the envelope a `RectListMerge` coalescing policy: a consumer that stalls sees the rects merge
//! rather than accumulate, and past the cap they collapse to their union. That is lossy in
//! bandwidth and not in pixels — the union is a superset of what changed, so the consumer
//! re-reads more than it had to and never reads less.
//!
//! Zero rects is a whole-texture upload, which is what R0 has: two placeholder textures that are
//! written once and never touched again.
//!
//! # R0's textures are empty, and that is worth emitting anyway
//!
//! mbgl creates two of them before any style content exists — a `0x0` pattern atlas and a `1x1`
//! fully transparent image. Neither draws anything. They are emitted because the shaders bind
//! them unconditionally: a fill shader samples its pattern texture whether or not the layer has
//! `fill-pattern`, and a consumer with nothing bound there reads undefined memory or fails to
//! draw at all, depending on the backend's mood. The golden dump lists both.

use alloc::vec::Vec;

use tessella_capture_abi::EnvelopeKind;
use tessella_capture_abi::envelope::{
    Extent, Rect16, Span, TEXTURE_RECT_CAP, TextureId, TextureUpdate, WireRecord,
};
use tessella_capture_abi::generated::mbgl_enums::TexturePixelType;
use tessella_capture_abi::ring::{Full, Producer};

/// A texture upload, before it goes on the wire.
#[derive(Debug, Clone, PartialEq)]
pub struct Upload {
    /// The record as it will be written.
    pub record: TextureUpdate,
    /// The pixel bytes the record's span addresses.
    pub pixels: Vec<u8>,
}

/// An upload that replaces the whole texture.
///
/// `rect_count` of zero says so. A single rect covering the whole image would describe the same
/// pixels and would make a consumer walk a rect list to discover it had one entry that covered
/// everything, so the zero is the clearer statement.
#[must_use]
pub fn whole(texture: TextureId, size: Extent, format: TexturePixelType, pixels: &[u8]) -> Upload {
    #[allow(clippy::cast_possible_truncation)]
    let record = TextureUpdate {
        texture,
        size,
        rects: [Rect16 {
            x: 0,
            y: 0,
            w: 0,
            h: 0,
        }; TEXTURE_RECT_CAP],
        pixels: Span {
            offset: 0,
            // A byte count, not a pixel count.
            count: pixels.len() as u32,
        },
        format: format as u8,
        rect_count: 0,
        _pad: [0; 2],
    };
    Upload {
        record,
        pixels: pixels.to_vec(),
    }
}

/// An upload of one or more dirty regions.
///
/// # Errors
///
/// [`TooManyRects`] when more regions are given than the envelope carries. Reported rather than
/// merged here: merging is the ring's job under stall (§4), and doing it at the producer would
/// hide from the caller that it is describing more regions than the protocol carries.
pub fn regions(
    texture: TextureId,
    size: Extent,
    format: TexturePixelType,
    dirty: &[Rect16],
    pixels: &[u8],
) -> Result<Upload, TooManyRects> {
    if dirty.is_empty() || dirty.len() > TEXTURE_RECT_CAP {
        return Err(TooManyRects { given: dirty.len() });
    }

    let mut rects = [Rect16 {
        x: 0,
        y: 0,
        w: 0,
        h: 0,
    }; TEXTURE_RECT_CAP];
    rects[..dirty.len()].copy_from_slice(dirty);

    #[allow(clippy::cast_possible_truncation)]
    let record = TextureUpdate {
        texture,
        size,
        rects,
        pixels: Span {
            offset: 0,
            count: pixels.len() as u32,
        },
        format: format as u8,
        rect_count: dirty.len() as u8,
        _pad: [0; 2],
    };
    Ok(Upload {
        record,
        pixels: pixels.to_vec(),
    })
}

/// More dirty regions than the envelope carries, or none at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("{given} dirty regions; the envelope carries 1 to {TEXTURE_RECT_CAP}")]
pub struct TooManyRects {
    /// How many were given.
    pub given: usize,
}

/// The two textures mbgl has before any style content exists.
///
/// A `0x0` pattern atlas with no bytes, and a `1x1` fully transparent image. Both are in the
/// golden dump, and a producer that skipped them would leave a consumer's shaders sampling
/// nothing.
#[must_use]
pub fn placeholders() -> [Upload; 2] {
    [
        whole(
            TextureId(0),
            Extent {
                width: 0,
                height: 0,
            },
            TexturePixelType::RGBA,
            &[],
        ),
        whole(
            TextureId(1),
            Extent {
                width: 1,
                height: 1,
            },
            TexturePixelType::RGBA,
            &[0, 0, 0, 0],
        ),
    ]
}

/// Writes an upload to the ring.
///
/// # Errors
///
/// [`Full`] when the ring cannot take it.
pub fn write(producer: &mut Producer, upload: &Upload) -> Result<(), Full> {
    producer.write(
        EnvelopeKind::TextureUpdate,
        upload.record.as_bytes(),
        &upload.pixels,
    )
}

/// The pixel format a glyph atlas is uploaded in.
///
/// Alpha, not RGBA. §12.4's point: this is the largest texture the process keeps, and three of
/// four channels would hold copies of the one that matters. The oracle agrees — the symbol
/// capture's atlas is `fmt=1`.
pub const GLYPH_ATLAS_FORMAT: TexturePixelType = TexturePixelType::Alpha;

/// The upload for a glyph atlas, carrying only what changed since the last one.
///
/// `dirty` is what [`Fonts::take_dirty`] returned. Empty means nothing moved and there is
/// nothing to send — which is the common case once a view settles, and is why this answers
/// `None` rather than a whole-texture upload: §6.5's still frame is a frame with no envelopes in
/// it, and re-uploading a megabyte of unchanged glyphs every frame would make a settled map the
/// most expensive one.
///
/// Past [`TEXTURE_RECT_CAP`] the rects collapse to their union, which costs bandwidth and never
/// pixels.
///
/// [`Fonts::take_dirty`]: tessella_glyph::fonts::Fonts::take_dirty
#[must_use]
pub fn glyph_atlas(
    texture: TextureId,
    atlas: &tessella_glyph::atlas::Atlas,
    dirty: &[tessella_glyph::atlas::Rect],
) -> Option<Upload> {
    if dirty.is_empty() {
        return None;
    }

    let (width, height) = atlas.size();
    let size = Extent { width, height };

    // The atlas measures in `u32` and the envelope in `u16`. The atlas is 512 square, so the
    // narrowing cannot lose a coordinate — and it is bounded rather than cast, because an atlas
    // grown past a `u16` would otherwise wrap a rectangle onto the wrong pixels silently.
    let narrow = |value: u32| -> u16 { u16::try_from(value).unwrap_or(u16::MAX) };
    let rects: Vec<Rect16> = dirty
        .iter()
        .map(|rect| Rect16 {
            x: narrow(rect.x),
            y: narrow(rect.y),
            w: narrow(rect.width),
            h: narrow(rect.height),
        })
        .collect();

    // The whole image goes with it: the rects say which parts the consumer must re-read, and
    // the pixels behind them are read out of the atlas at those coordinates. Sending only the
    // rect pixels would need a packing the envelope does not describe.
    Some(
        regions(texture, size, GLYPH_ATLAS_FORMAT, &rects, atlas.pixels()).unwrap_or_else(|_| {
            // Past the cap: one rect covering everything that changed. Lossy in bandwidth and
            // not in pixels — the union is a superset of what moved.
            let union = union_of(&rects);
            regions(texture, size, GLYPH_ATLAS_FORMAT, &[union], atlas.pixels())
                .expect("one rect is inside any cap")
        }),
    )
}

/// The pixel format the icon atlas is uploaded in.
///
/// RGBA, unlike the glyph atlas. §12.4's single-channel argument does not reach here: a sprite
/// is a picture and three of its four channels carry something. An SDF sprite is the exception
/// and is still RGBA, because a sheet holds both kinds and the format is the sheet's.
pub const SPRITE_SHEET_FORMAT: TexturePixelType = TexturePixelType::RGBA;

/// The upload for a sprite sheet.
///
/// A whole-texture upload rather than a rect list, and the difference from the glyph atlas is
/// real: a glyph atlas fills in as labels arrive and changes in small places, while a sheet
/// arrives once, complete, and never changes again. Zero rects is what the envelope spells
/// "all of it", which is exactly what this is.
///
/// `None` when the sheet has not arrived or has not changed — a style's icons are uploaded once.
#[cfg(feature = "png")]
#[must_use]
pub fn sprite_sheet(texture: TextureId, sheet: &tessella_glyph::sprite::Sheet) -> Option<Upload> {
    let size = Extent {
        width: sheet.width,
        height: sheet.height,
    };
    Some(whole(texture, size, SPRITE_SHEET_FORMAT, &sheet.pixels))
}

/// The smallest rectangle containing all of these.
fn union_of(rects: &[Rect16]) -> Rect16 {
    let min_x = rects.iter().map(|rect| rect.x).min().unwrap_or(0);
    let min_y = rects.iter().map(|rect| rect.y).min().unwrap_or(0);
    let max_x = rects.iter().map(|rect| rect.x + rect.w).max().unwrap_or(0);
    let max_y = rects.iter().map(|rect| rect.y + rect.h).max().unwrap_or(0);
    Rect16 {
        x: min_x,
        y: min_y,
        w: max_x - min_x,
        h: max_y - min_y,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// FNV-1a, as the probe hashes texture bytes.
    fn fnv1a(bytes: &[u8]) -> u64 {
        let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
        for byte in bytes {
            hash = (hash ^ u64::from(*byte)).wrapping_mul(0x0100_0000_01b3);
        }
        hash
    }

    /// The placeholders reproduce the oracle's two textures, contents and all.
    ///
    /// The dump records `0x0 fmt=0 hash=cbf29ce484222325` and `1x1 fmt=0
    /// hash=4d25767f9dce13f5`. The first hash is the FNV offset basis, which is to say no bytes
    /// were uploaded at all; the second is four zero bytes, a fully transparent pixel. Neither
    /// draws anything, and both are emitted because the shaders sample them unconditionally.
    #[test]
    fn the_placeholders_match_the_oracle() {
        let [atlas, image] = placeholders();

        assert_eq!(
            atlas.record.size,
            Extent {
                width: 0,
                height: 0
            }
        );
        assert!(atlas.pixels.is_empty());
        assert_eq!(fnv1a(&atlas.pixels), 0xcbf2_9ce4_8422_2325);

        assert_eq!(
            image.record.size,
            Extent {
                width: 1,
                height: 1
            }
        );
        assert_eq!(image.pixels, [0, 0, 0, 0], "transparent, not white");
        assert_eq!(fnv1a(&image.pixels), 0x4d25_767f_9dce_13f5);

        for upload in [&atlas, &image] {
            assert_eq!(upload.record.format, TexturePixelType::RGBA as u8);
            assert_eq!(upload.record.rect_count, 0, "a whole-texture upload");
        }
    }

    /// A whole-texture upload says so with zero rects rather than one covering rect.
    ///
    /// The two describe the same pixels, but the second makes a consumer walk a rect list to
    /// discover it has one entry covering everything.
    #[test]
    fn a_whole_upload_carries_no_rects() {
        let upload = whole(
            TextureId(7),
            Extent {
                width: 4,
                height: 4,
            },
            TexturePixelType::RGBA,
            &[1; 64],
        );
        assert_eq!(upload.record.rect_count, 0);
        assert_eq!(
            upload.record.pixels.count, 64,
            "a byte count, not a pixel count"
        );
    }

    /// A partial upload carries its regions, and only as many as it was given.
    #[test]
    fn a_partial_upload_carries_its_regions() {
        let dirty = [
            Rect16 {
                x: 0,
                y: 0,
                w: 2,
                h: 2,
            },
            Rect16 {
                x: 2,
                y: 2,
                w: 1,
                h: 1,
            },
        ];
        let upload = regions(
            TextureId(7),
            Extent {
                width: 4,
                height: 4,
            },
            TexturePixelType::RGBA,
            &dirty,
            &[9; 20],
        )
        .expect("two regions fit");

        assert_eq!(upload.record.rect_count, 2);
        assert_eq!(&upload.record.rects[..2], &dirty[..]);
        assert_eq!(
            upload.record.rects[2],
            Rect16 {
                x: 0,
                y: 0,
                w: 0,
                h: 0
            },
            "the unused entries stay zero"
        );
    }

    /// Too many regions is reported, not silently merged.
    ///
    /// Merging is the ring's job under stall (§4). Doing it here would hide from the caller that
    /// it is describing more regions than the protocol carries, and the caller is the only one
    /// that knows whether that is a bug or a texture that genuinely changed everywhere.
    #[test]
    fn too_many_regions_is_an_error() {
        let many = [Rect16 {
            x: 0,
            y: 0,
            w: 1,
            h: 1,
        }; TEXTURE_RECT_CAP + 1];
        let size = Extent {
            width: 8,
            height: 8,
        };
        assert_eq!(
            regions(TextureId(0), size, TexturePixelType::RGBA, &many, &[]),
            Err(TooManyRects {
                given: TEXTURE_RECT_CAP + 1
            })
        );

        // And none at all, which would be a whole-texture upload spelled wrongly.
        assert!(regions(TextureId(0), size, TexturePixelType::RGBA, &[], &[]).is_err());
    }
}
