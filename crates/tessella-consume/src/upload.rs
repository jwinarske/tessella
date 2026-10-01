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
        /// Pixel format, as a `TexturePixelType` discriminant.
        format: u8,
        /// The regions these pixels cover, in order.
        ///
        /// A backend uploads per rect and never the whole texture: §6.4's dirty-rect list is what
        /// stops the opposite-corners pathology from rewriting an atlas every frame.
        rects: Vec<Rect16>,
        /// Where the bytes are, in the host's buffer.
        bytes: Range<usize>,
    },
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
        format: u8,
        rects: Vec<Rect16>,
        pixels: &[u8],
    ) {
        let bytes = self.stash(pixels);
        self.work.push(Upload::Texture {
            texture,
            size,
            format,
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
