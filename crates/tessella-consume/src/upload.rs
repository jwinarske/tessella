//! The bytes a frame has to get onto the device, and where they are kept until it does.
//!
//! # Why these are copied when geometry is not
//!
//! Geometry lives in slabs. A slab reference stays resolvable until the consumer acknowledges past
//! the point the geometry was announced, so a backend resolves it when it is ready to upload and
//! nothing is copied.
//!
//! Uniform blocks and texture pixels do not. They are spans into the *ring*, and the ring's bytes
//! are the producer's again as soon as the tail moves past them — which it must, because reading
//! the next record is what moves it. So a consumer that wants them after the drain has to copy
//! them during it. There is no arrangement of the API that avoids this; the only choice is where
//! the copy goes.
//!
//! It goes into one buffer the host keeps and reuses. The work list carries ranges into it rather
//! than owning bytes, so a frame costs one `extend_from_slice` per record and no allocation once
//! the buffer has grown to a frame's worth.
//!
//! # Who clears it
//!
//! The backend, by saying it is done. The same shape as acknowledging geometry and for the same
//! reason: only the backend knows when its driver has finished reading what it was handed.

use alloc::vec::Vec;
use core::ops::Range;

use tessella_capture_abi::envelope::{Extent, Rect16, TextureId, ViewId};
use tessella_capture_abi::{TextureChannelDataType, TexturePixelType};

/// One piece of work: bytes to put somewhere on the device.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Upload {
    /// Uniform bytes for a layer's consolidated buffer.
    ///
    /// Latest wins at a slot, which is the producer's contract rather than this crate's: a second
    /// update for the same (view, layer, slot) within one read supersedes the first, and both
    /// appear here in order so a backend writing them in order lands on the later one.
    Uniforms {
        /// View whose buffer this belongs to.
        view: ViewId,
        /// Layer group within that view.
        layer_index: i32,
        /// Slot within the layer's buffer.
        slot: u32,
        /// Where the bytes are, in the host's buffer.
        bytes: Range<usize>,
    },
    /// Pixels for a texture, in whole or in rects.
    Texture {
        /// Texture to write into.
        texture: TextureId,
        /// The texture's full size, which a backend needs to create it.
        size: Extent,
        /// What a texel is and how the payload is laid out.
        shape: Shape,
        /// The regions these pixels cover, in order.
        ///
        /// A backend uploads per rect and never the whole texture: §6.4's dirty-rect list is what
        /// stops the opposite-corners pathology from rewriting an atlas every frame.
        rects: Vec<Rect16>,
        /// Where the bytes are, in the host's buffer.
        bytes: Range<usize>,
    },
}

/// What a texture's texels are, and how its payload is laid out.
///
/// The two format halves travel together because mbgl's `setFormat` takes both and neither is
/// enough alone: a color relief's elevation stops are `RGBA` *and* `Float`, because a stop is
/// meters above the sea and eight bits over that range is a forty-meter step. The packed flag
/// joins them because it is the third thing needed to find a pixel, and a backend that has two of
/// the three reads the right addresses' worth of the wrong rows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Shape {
    /// Which channels a texel carries.
    pub format: TexturePixelType,
    /// What one channel holds.
    pub channel: TextureChannelDataType,
    /// Whether the payload holds only the rects' pixels, packed.
    ///
    /// False means the payload is the whole texture and the rects name which parts of it moved;
    /// true means each rect's pixels are packed tight at its own width, in the order the rects
    /// name them. Read [`rows`] rather than branching on this.
    pub packed: bool,
}

impl Shape {
    /// How many bytes one texel of this shape occupies.
    ///
    /// Both of mbgl's factors: `getStorageSize` is `channelCount() * channelStorageSize()`, and a
    /// consumer multiplying by the first alone sizes a `Float` texture at a quarter.
    #[must_use]
    pub fn texel(&self) -> usize {
        self.format.channels() as usize * self.channel.storage_size() as usize
    }
}

/// Where one region's pixels are, relative to the start of an upload's bytes.
///
/// A backend copies `rect.h` rows of `rect.w * texel` bytes, starting at [`Self::at`] and stepping
/// [`Self::stride`] between them. Both forms of payload reduce to this, which is the point: the
/// packed and whole cases differ only in these two numbers, so nothing downstream has to know
/// which arrived.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rows {
    /// Byte offset of the region's first row.
    pub at: usize,
    /// Bytes from the start of one row to the start of the next.
    pub stride: usize,
}

/// Why an upload's rects and its bytes do not describe the same thing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mismatch {
    /// A rect reaches past the texture it names.
    ///
    /// The producer refuses to pack in this case and sends the texture whole instead; a rect out
    /// of bounds either way is a caller describing one texture and handing over another.
    OutOfBounds {
        /// Which rect, by its position in the list.
        rect: usize,
    },
    /// The payload is shorter than the rects and the size claim.
    ///
    /// `TextureUpdate::packed`'s documentation calls this check "the second half of the same
    /// guard" — the producer's own `fits` test is the first half, and a consumer that trusts it
    /// reads past the bytes it was given.
    ///
    /// Counted in `u64` rather than `usize` because the layout can need more than a `usize` can
    /// address: this crate is `no_std` and builds for `wasm32`, where a `usize` is thirty-two bits
    /// and a texture claiming a four-billion-pixel width overflows one. A wrapped total compares
    /// small, passes this check and sends a region to an offset nothing computed.
    Short {
        /// Bytes the layout needs.
        wanted: u64,
        /// Bytes there are.
        got: u64,
    },
}

impl Upload {
    /// Where each region's pixels are inside this upload's bytes.
    ///
    /// `None` for an `Uniforms`. One entry per rect, in the rects' own order. An empty rect list
    /// is one region covering the whole texture, which is how the producer says it has no damage
    /// worth describing.
    ///
    /// # Errors
    ///
    /// [`Mismatch`] when a rect reaches past the texture or the bytes are shorter than the layout
    /// needs. Nothing partial is returned: a backend that uploaded the regions it could would put
    /// some of them at the right addresses and leave the rest stale.
    pub fn rows(&self) -> Option<Result<Vec<Rows>, Mismatch>> {
        let Self::Texture {
            size,
            shape,
            rects,
            bytes,
            ..
        } = self
        else {
            return None;
        };
        Some(rows(*size, *shape, rects, bytes.len()))
    }
}

/// Where each region's pixels are, for an upload described piece by piece.
///
/// Split from [`Upload::rows`] so the arithmetic can be exercised without building an `Upload`,
/// and because a backend decoding a record itself needs it before it has one.
///
/// # Errors
///
/// [`Mismatch`], as [`Upload::rows`].
pub fn rows(
    size: Extent,
    shape: Shape,
    rects: &[Rect16],
    payload: usize,
) -> Result<Vec<Rows>, Mismatch> {
    let texel = shape.texel();
    let whole = Rect16 {
        x: 0,
        y: 0,
        // An extent is `u32` and a rect is `u16`, so a texture larger than a rect can address is
        // clamped rather than wrapped -- the same reading `textures::updated` takes, and the
        // producer could not have described damage on such a texture either.
        w: u16::try_from(size.width).unwrap_or(u16::MAX),
        h: u16::try_from(size.height).unwrap_or(u16::MAX),
    };
    // An empty list is one region covering everything, and `packed` is meaningless then: the
    // payload is the whole texture either way, which is what `TextureUpdate::packed` says.
    let (regions, packed) = if rects.is_empty() {
        (core::slice::from_ref(&whole), false)
    } else {
        (rects, shape.packed)
    };

    for (index, rect) in regions.iter().enumerate() {
        if u32::from(rect.x) + u32::from(rect.w) > size.width
            || u32::from(rect.y) + u32::from(rect.h) > size.height
        {
            return Err(Mismatch::OutOfBounds { rect: index });
        }
    }

    // Every product below is taken in `u64` and saturated, and the total is checked against the
    // payload before anything is narrowed.
    //
    // Two sizes of overflow to get past. A rect is two `u16`s and a texel is up to sixteen bytes,
    // so one region's bytes reach about 7e10 -- past a thirty-two bit `usize`, which is what this
    // crate gets on `wasm32`, where a wrapped total compares small, passes the check below and
    // leaves a region reading from an offset nothing computed. And an extent is two `u32`s, so a
    // whole-texture claim reaches about 3e20, past a `u64` as well.
    //
    // Saturated rather than checked, because a saturated total is still a correct answer to the
    // only question asked of it: it cannot be smaller than the real one, so it cannot be within a
    // payload the real one is outside, and the record is refused either way.
    let texel = texel as u64;
    let payload = payload as u64;
    let mut spans: Vec<(u64, u64)> = Vec::with_capacity(regions.len());
    let wanted = if packed {
        // Each region tight at its own width, laid down in the order the rects name them.
        let mut at = 0u64;
        for rect in regions {
            let stride = u64::from(rect.w).saturating_mul(texel);
            spans.push((at, stride));
            at = at.saturating_add(stride.saturating_mul(u64::from(rect.h)));
        }
        at
    } else {
        // The whole texture, so a region is a window into it at the texture's own stride.
        let stride = u64::from(size.width).saturating_mul(texel);
        for rect in regions {
            spans.push((
                u64::from(rect.y)
                    .saturating_mul(stride)
                    .saturating_add(u64::from(rect.x).saturating_mul(texel)),
                stride,
            ));
        }
        stride.saturating_mul(u64::from(size.height))
    };
    if wanted > payload {
        return Err(Mismatch::Short {
            wanted,
            got: payload,
        });
    }

    // Narrowed only now. Each offset is inside a layout that fits the payload, and the payload is
    // a slice length, so every one of these fits -- and it is written as a conversion that can
    // fail rather than one that cannot, so there is no panic to document.
    let mut out = Vec::with_capacity(spans.len());
    for (at, stride) in spans {
        let (Ok(at), Ok(stride)) = (usize::try_from(at), usize::try_from(stride)) else {
            return Err(Mismatch::Short {
                wanted,
                got: payload,
            });
        };
        out.push(Rows { at, stride });
    }
    Ok(out)
}

/// Work accumulated across reads, with the bytes behind it.
#[derive(Debug, Clone, Default)]
pub struct Uploads {
    work: Vec<Upload>,
    bytes: Vec<u8>,
}

impl Uploads {
    /// Nothing to upload.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The work outstanding, in the order it arrived.
    #[must_use]
    pub fn work(&self) -> &[Upload] {
        &self.work
    }

    /// The bytes a piece of work names.
    ///
    /// `None` for a range the buffer does not hold, which cannot happen for a range this crate
    /// produced and is checked anyway — the alternative is a panic reachable from a stream.
    #[must_use]
    pub fn bytes(&self, at: &Range<usize>) -> Option<&[u8]> {
        self.bytes.get(at.clone())
    }

    /// How many bytes are held, for a consumer watching its own footprint.
    #[must_use]
    pub fn held(&self) -> usize {
        self.bytes.len()
    }

    /// Drops the work and its bytes, keeping the buffer's capacity.
    ///
    /// The backend's call, once its driver has finished reading what it was handed. Nothing here
    /// knows when that is, and guessing would hand the producer's bytes back while a transfer was
    /// still running.
    pub fn clear(&mut self) {
        self.work.clear();
        self.bytes.clear();
    }

    /// Copies a payload in and records where it went.
    pub(crate) fn push_uniforms(&mut self, view: ViewId, layer_index: i32, slot: u32, data: &[u8]) {
        let bytes = self.stash(data);
        self.work.push(Upload::Uniforms {
            view,
            layer_index,
            slot,
            bytes,
        });
    }

    /// Copies pixels in and records where they went.
    pub(crate) fn push_texture(
        &mut self,
        texture: TextureId,
        size: Extent,
        shape: Shape,
        rects: Vec<Rect16>,
        pixels: &[u8],
    ) {
        let bytes = self.stash(pixels);
        self.work.push(Upload::Texture {
            texture,
            size,
            shape,
            rects,
            bytes,
        });
    }

    fn stash(&mut self, data: &[u8]) -> Range<usize> {
        let start = self.bytes.len();
        self.bytes.extend_from_slice(data);
        start..self.bytes.len()
    }
}
