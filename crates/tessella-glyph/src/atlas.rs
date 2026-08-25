//! Packing glyphs into one shared texture.
//!
//! A port of `mapbox::ShelfPack`, which is what mbgl's dynamic texture atlas uses, plus the R8
//! atlas built on it. One atlas serves every view (§5.1): a glyph is packed once however many
//! maps are drawing it, and the §9.3 flatness counters exist to catch it not being.
//!
//! # Shelves, and why not a general rectangle packer
//!
//! A shelf packer puts each rectangle on a row of uniform height, opening a new row when none
//! fits. It wastes the space above short glyphs on a tall row, and a general packer would waste
//! less — but glyphs from one font are nearly all the same height, so the waste is small in
//! practice, and the property that matters more is that insertions stay *clustered*. §6.4's
//! damage is a list of dirty rectangles, and a packer that scattered new glyphs across the
//! texture would make every frame's upload a union covering most of it.
//!
//! # Padding is two pixels, and one of them comes back
//!
//! A glyph reserves its bitmap plus two pixels on every side, and the rectangle it reports back
//! covers the bitmap plus *one*. The difference matters: the outer pixel keeps two glyphs from
//! bleeding into each other when the texture is sampled with linear filtering, and the inner
//! one is deliberately included so that the shader sampling the glyph's edge has real distance
//! field to read rather than whatever its neighbour left there.

use std::collections::BTreeMap;

use crate::pbf::{BORDER, Glyph};

/// A rectangle in the atlas.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Rect {
    /// Left edge.
    pub x: u32,
    /// Top edge.
    pub y: u32,
    /// Width.
    pub width: u32,
    /// Height.
    pub height: u32,
}

/// A packed rectangle, with the slot it occupies.
#[derive(Debug, Clone, Copy)]
struct Bin {
    rect: Rect,
    /// The slot's original size, which stays put when a smaller bin is placed in it.
    slot: (u32, u32),
    refcount: u32,
}

/// A row of uniform height.
#[derive(Debug, Clone, Copy)]
struct Shelf {
    x: u32,
    y: u32,
    height: u32,
    free: u32,
}

/// Shelf packing, as `mapbox::ShelfPack` does it.
///
/// Fixed size: mbgl constructs its textures with `autoResize` off and opens another texture when
/// one fills, and growing a texture the consumer has already uploaded would invalidate every
/// rectangle handed out for it.
#[derive(Debug)]
pub struct ShelfPack {
    width: u32,
    height: u32,
    shelves: Vec<Shelf>,
    bins: BTreeMap<u32, Bin>,
    /// Bins whose refcount reached zero, available to be reused.
    free: Vec<u32>,
}

impl ShelfPack {
    /// An empty packer of this size.
    #[must_use]
    pub fn new(width: u32, height: u32) -> Self {
        Self {
            width,
            height,
            shelves: Vec::new(),
            bins: BTreeMap::new(),
            free: Vec::new(),
        }
    }

    /// The slot for `id`, if it is packed.
    #[must_use]
    pub fn get(&self, id: u32) -> Option<Rect> {
        self.bins.get(&id).map(|bin| bin.rect)
    }

    /// How many rectangles are packed, including freed ones still holding their slot.
    #[must_use]
    pub fn len(&self) -> usize {
        self.bins.len()
    }

    /// Whether anything is packed.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.bins.is_empty()
    }

    /// Packs one rectangle, or returns `None` when there is no room.
    ///
    /// Packing an id that is already packed takes a reference to it and returns the slot it
    /// already has — which is the whole point of the id: the same glyph asked for by two tiles
    /// is one rectangle, and the second ask is free.
    pub fn pack(&mut self, id: u32, width: u32, height: u32) -> Option<Rect> {
        if let Some(bin) = self.bins.get_mut(&id) {
            bin.refcount += 1;
            return Some(bin.rect);
        }

        // A freed slot of exactly the right size is taken at once; otherwise the one that wastes
        // least is remembered while the shelves are searched.
        let mut best_free: Option<(usize, u32)> = None;
        for (position, freed) in self.free.iter().enumerate() {
            let Some(bin) = self.bins.get(freed) else {
                continue;
            };
            let (slot_width, slot_height) = bin.slot;
            if slot_width == width && slot_height == height {
                return Some(self.take_free(position, id, width, height));
            }
            if slot_width < width || slot_height < height {
                continue;
            }
            let waste = slot_width * slot_height - width * height;
            if best_free.is_none_or(|(_, best)| waste < best) {
                best_free = Some((position, waste));
            }
        }

        // Then the shelves, by the same rule: an exact height wins immediately, otherwise the
        // least wasteful is kept.
        let mut best_shelf: Option<(usize, u32)> = None;
        let mut next_y = 0;
        for (position, shelf) in self.shelves.iter().enumerate() {
            next_y += shelf.height;
            if width > shelf.free {
                continue;
            }
            if height == shelf.height {
                return Some(self.take_shelf(position, id, width, height));
            }
            if height > shelf.height {
                continue;
            }
            let waste = (shelf.height - height) * width;
            if best_shelf.is_none_or(|(_, best)| waste < best) {
                best_shelf = Some((position, waste));
            }
        }

        if let Some((position, _)) = best_free {
            return Some(self.take_free(position, id, width, height));
        }
        if let Some((position, _)) = best_shelf {
            return Some(self.take_shelf(position, id, width, height));
        }

        // Nothing fits: open a new shelf if there is room below the last one.
        if height <= self.height.saturating_sub(next_y) && width <= self.width {
            self.shelves.push(Shelf {
                x: 0,
                y: next_y,
                height,
                free: self.width,
            });
            let position = self.shelves.len() - 1;
            return Some(self.take_shelf(position, id, width, height));
        }

        None
    }

    /// Reuses a freed slot, keeping its original size so a later bin can use the whole of it.
    fn take_free(&mut self, position: usize, id: u32, width: u32, height: u32) -> Rect {
        let freed = self.free.remove(position);
        let old = self.bins.remove(&freed).expect("a freed bin");
        let rect = Rect {
            x: old.rect.x,
            y: old.rect.y,
            width,
            height,
        };
        self.bins.insert(
            id,
            Bin {
                rect,
                slot: old.slot,
                refcount: 1,
            },
        );
        rect
    }

    /// Places a bin at the open end of a shelf.
    fn take_shelf(&mut self, position: usize, id: u32, width: u32, height: u32) -> Rect {
        let shelf = &mut self.shelves[position];
        let rect = Rect {
            x: shelf.x,
            y: shelf.y,
            width,
            height,
        };
        shelf.x += width;
        shelf.free -= width;
        let slot = (width, shelf.height);
        self.bins.insert(
            id,
            Bin {
                rect,
                slot,
                refcount: 1,
            },
        );
        rect
    }

    /// Drops a reference, freeing the slot when the last one goes.
    ///
    /// The slot is kept rather than merged back into its shelf: merging would need neighbours to
    /// be adjacent and the same height, which after a few evictions they are not. Keeping it
    /// means a glyph of the same size lands exactly where the old one was, which is also the
    /// arrangement that keeps §6.4's dirty rectangles small.
    pub fn unref(&mut self, id: u32) {
        let Some(bin) = self.bins.get_mut(&id) else {
            return;
        };
        bin.refcount -= 1;
        if bin.refcount == 0 && !self.free.contains(&id) {
            self.free.push(id);
        }
    }
}

/// Padding around every glyph in the atlas.
///
/// Two: one so linear filtering cannot pull a neighbouring glyph's pixels in, and one more that
/// is handed back inside the reported rectangle so the shader has distance field to read at the
/// glyph's own edge.
pub const PADDING: u32 = 2;

/// The padding that stays outside the reported rectangle.
const OUTER: u32 = 1;

/// A single-channel atlas of glyph distance fields.
///
/// R8 rather than RGBA, per §12.4: this is the largest texture the process keeps, and three of
/// four channels would hold copies of the one that matters.
#[derive(Debug)]
pub struct Atlas {
    pack: ShelfPack,
    pixels: Vec<u8>,
    width: u32,
    height: u32,
    dirty: Vec<Rect>,
}

impl Atlas {
    /// An empty atlas.
    #[must_use]
    pub fn new(width: u32, height: u32) -> Self {
        Self {
            pack: ShelfPack::new(width, height),
            pixels: vec![0; (width * height) as usize],
            width,
            height,
            dirty: Vec::new(),
        }
    }

    /// The atlas dimensions.
    #[must_use]
    pub const fn size(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    /// The single-channel pixels.
    #[must_use]
    pub fn pixels(&self) -> &[u8] {
        &self.pixels
    }

    /// Adds a glyph, or returns `None` when the atlas is full.
    ///
    /// `key` identifies the glyph across font stacks — the same codepoint in two fonts is two
    /// entries, since they are different pictures.
    ///
    /// The returned rectangle covers the distance field plus one pixel on each side, which is
    /// what a quad samples.
    pub fn add(&mut self, key: u32, glyph: &Glyph) -> Option<Rect> {
        if let Some(rect) = self.pack.get(key) {
            // Already here: take a reference and hand back the same rectangle.
            self.pack.pack(key, rect.width, rect.height);
            return Some(reported(rect));
        }

        let Some((bitmap_width, bitmap_height)) = glyph.bitmap_size() else {
            // A glyph with no pixels — a space — occupies no atlas space. Its metrics are all
            // the shaper needs, and reserving a rectangle for nothing would fill the texture
            // with blanks.
            return None;
        };

        let slot = self
            .pack
            .pack(key, bitmap_width + 2 * PADDING, bitmap_height + 2 * PADDING)?;

        // Blit the distance field into the middle of its slot.
        for row in 0..bitmap_height {
            let from = (row * bitmap_width) as usize;
            let to = ((slot.y + PADDING + row) * self.width + slot.x + PADDING) as usize;
            self.pixels[to..to + bitmap_width as usize]
                .copy_from_slice(&glyph.bitmap[from..from + bitmap_width as usize]);
        }
        self.dirty.push(slot);
        Some(reported(slot))
    }

    /// The rectangle a packed glyph occupies, if it is here.
    #[must_use]
    pub fn get(&self, key: u32) -> Option<Rect> {
        self.pack.get(key).map(reported)
    }

    /// Drops a reference to a glyph.
    pub fn remove(&mut self, key: u32) {
        self.pack.unref(key);
    }

    /// How many glyphs are packed.
    #[must_use]
    pub fn len(&self) -> usize {
        self.pack.len()
    }

    /// Whether the atlas holds anything.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.pack.is_empty()
    }

    /// Takes the rectangles changed since this was last called.
    ///
    /// A list rather than a union, per §6.4: glyphs land clustered on a shelf, so a handful of
    /// small rectangles is usually a far smaller upload than the box that contains them.
    pub fn take_dirty(&mut self) -> Vec<Rect> {
        core::mem::take(&mut self.dirty)
    }

    /// The border every glyph's distance field carries, for a caller sizing a quad.
    #[must_use]
    pub const fn border() -> u32 {
        BORDER
    }
}

/// The rectangle handed to a caller: the slot, less the pixel that exists only to separate it
/// from its neighbour.
const fn reported(slot: Rect) -> Rect {
    Rect {
        x: slot.x + OUTER,
        y: slot.y + OUTER,
        width: slot.width - 2 * OUTER,
        height: slot.height - 2 * OUTER,
    }
}
