//! The glyphs a style has, and the atlases they are packed into.
//!
//! [`GlyphManager`] knows which ranges are held and which are owed; [`Atlas`] knows where a glyph
//! sits in a texture. Neither is a thing layout can shape against on its own, and pairing them is
//! what turns "the ranges arrived" into a [`Glyphs`] a bucket builder can use.
//!
//! One atlas per font stack, which is what §5 says and what mbgl does. Not one per style: two
//! stacks that share a codepoint need it at two rectangles, since a rectangle is a position in a
//! *texture* and the two fonts draw it differently.
//!
//! # Only what was asked for is packed
//!
//! A range file is 256 codepoints and a label uses a handful. Packing a whole range on arrival
//! would fill the atlas with glyphs nothing draws and evict the ones that are drawn — so packing
//! is driven by the dependencies the layouts declared, not by what the response happened to
//! contain. It is also why the atlas fills in the order labels ask rather than in codepoint
//! order, which is the same order mbgl's fills in and the reason its packing is not
//! reproducible.
//!
//! # A missing glyph is not a missing label
//!
//! [`Fonts::stack`] answers for whatever is packed *now*. A label whose glyphs have not all
//! arrived shapes the ones that have, because a map that waited for a font before drawing
//! anything would show nothing during a pan into new text. What makes that safe is that the
//! label is measured whole either way — the advance comes from the metrics, which arrive with
//! the range, and not from the atlas.

use std::collections::{BTreeMap, BTreeSet};

use tessella_storage::FileSource;

use crate::Glyphs;
use crate::atlas::{Atlas, Rect};
use crate::manager::{FontStack, GlyphManager, LoadError};
use crate::pbf::Metrics;

/// The width and height of a glyph atlas.
///
/// Five hundred and twelve, because that is what the oracle emits: `symbol_style.dump` lists a
/// `512x512 fmt=1` texture beside mbgl's two placeholders. A fixed page rather than one that
/// grows — growing a texture the consumer has already uploaded would invalidate every rectangle
/// handed out for it, so mbgl opens another texture when one fills instead, which is the note
/// `ShelfPack` carries.
///
/// It is not a number to change on a hunch: it is on the wire, and a consumer sizing its
/// allocation from the first upload gets a different texture from the one the oracle describes.
pub const ATLAS_SIZE: u32 = 512;

/// What a set of layouts needs, per font stack.
///
/// The same shape `SymbolLayout::dependencies` produces, and the reason it is spelled here as
/// well is that a tile's worth of them get merged before anything is fetched: one request per
/// stack per range for the whole tile, not one per layer.
pub type Dependencies = BTreeMap<Vec<String>, BTreeSet<u32>>;

/// Every font stack a style uses, with its glyphs and its atlas.
#[derive(Debug)]
pub struct Fonts {
    manager: GlyphManager,
    atlases: BTreeMap<FontStack, Atlas>,
}

impl Fonts {
    /// A store fetching from `url`, which carries `{fontstack}` and `{range}`.
    #[must_use]
    pub fn new(url: impl Into<String>) -> Self {
        Self {
            manager: GlyphManager::new(url),
            atlases: BTreeMap::new(),
        }
    }

    /// Fetches whatever `dependencies` needs and is not held, then packs what they asked for.
    ///
    /// Returns how many ranges were fetched, which is what a counter watches: a number that does
    /// not fall to zero as a view settles means absence is not being remembered, and the same
    /// range is being asked for on every tile.
    ///
    /// # Errors
    ///
    /// The first [`LoadError`] a range produced. Ranges before it are held and packed; a caller
    /// that retries gets only the ones still owed, since a settled range is never re-fetched.
    /// Failing loudly rather than silently is deliberate — a font stack that never loads draws a
    /// map with no labels on it, and nothing else in the frame would say so.
    pub fn fetch(
        &mut self,
        dependencies: &Dependencies,
        files: &dyn FileSource,
    ) -> Result<usize, LoadError> {
        let mut fetched = 0usize;
        for (fonts, codepoints) in dependencies {
            let stack = FontStack(fonts.clone());
            for range in self.manager.owed(&stack, codepoints.iter().copied()) {
                self.manager.load_range(&stack, range, files)?;
                fetched += 1;
            }
            self.pack(&stack, codepoints);
        }
        Ok(fetched)
    }

    /// Packs the codepoints this stack was asked for and does not already have a rectangle for.
    fn pack(&mut self, stack: &FontStack, codepoints: &BTreeSet<u32>) {
        let atlas = self
            .atlases
            .entry(stack.clone())
            .or_insert_with(|| Atlas::new(ATLAS_SIZE, ATLAS_SIZE));

        for codepoint in codepoints {
            if atlas.get(*codepoint).is_some() {
                continue;
            }
            let Some(glyph) = self.manager.glyph(stack, *codepoint) else {
                continue;
            };
            // A glyph with no pixels — a space — has nothing to pack and still has an advance.
            // Packing a zero-area rectangle would take a shelf slot and give the shaper a
            // rectangle to draw, which is a blank quad per space on every label.
            if glyph.bitmap_size().is_none() {
                continue;
            }
            atlas.add(*codepoint, glyph);
        }
    }

    /// A [`Glyphs`] answering for one font stack.
    #[must_use]
    pub fn stack<'a>(&'a self, fonts: &[String]) -> StackGlyphs<'a> {
        let stack = FontStack(fonts.to_vec());
        StackGlyphs {
            atlas: self.atlases.get(&stack),
            manager: &self.manager,
            stack,
        }
    }

    /// This stack's atlas, for the texture upload.
    #[must_use]
    pub fn atlas(&self, fonts: &[String]) -> Option<&Atlas> {
        self.atlases.get(&FontStack(fonts.to_vec()))
    }

    /// The rectangles this stack's atlas has changed since the last call — §6.4's damage.
    pub fn take_dirty(&mut self, fonts: &[String]) -> Vec<Rect> {
        self.atlases
            .get_mut(&FontStack(fonts.to_vec()))
            .map(Atlas::take_dirty)
            .unwrap_or_default()
    }

    /// Whether every codepoint of `dependencies` has an answer, one way or the other.
    ///
    /// "Resolved" includes *known absent*: a font that does not contain a codepoint has answered
    /// about it, and waiting for it would be waiting forever. This is what a caller asks before
    /// deciding a tile's symbols are final rather than provisional.
    #[must_use]
    pub fn is_resolved(&self, dependencies: &Dependencies) -> bool {
        dependencies.iter().all(|(fonts, codepoints)| {
            let stack = FontStack(fonts.clone());
            codepoints
                .iter()
                .all(|codepoint| self.manager.is_resolved(&stack, *codepoint))
        })
    }

    /// The manager underneath, for counters and eviction.
    #[must_use]
    pub const fn manager(&self) -> &GlyphManager {
        &self.manager
    }

    /// Drops every stack but these, atlas included — §5.5's ownership, on a style change.
    pub fn evict(&mut self, keep: &BTreeSet<FontStack>) {
        self.manager.evict(keep);
        self.atlases.retain(|stack, _| keep.contains(stack));
    }
}

/// One font stack's view of the store.
#[derive(Debug, Clone)]
pub struct StackGlyphs<'a> {
    manager: &'a GlyphManager,
    atlas: Option<&'a Atlas>,
    stack: FontStack,
}

impl Glyphs for StackGlyphs<'_> {
    fn metrics(&self, codepoint: u32) -> Option<(Metrics, bool)> {
        let glyph = self.manager.glyph(&self.stack, codepoint)?;
        Some((glyph.metrics, glyph.bitmap_size().is_some()))
    }

    fn rect(&self, codepoint: u32) -> Option<Rect> {
        self.atlas?.get(codepoint)
    }
}
