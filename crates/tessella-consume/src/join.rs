//! Putting a drawable back together.
//!
//! mbgl's one `DrawableAdd` is two records here: a `GeometryAdd` carrying what every view shares,
//! and a `ViewUse` per view carrying what that view alone knows — its layer, its sub-layer, its
//! tile, its render pass and its draw flags. Four views over one tile send one add and four uses.
//! A consumer wants the pair back, and getting the pairing wrong is not a crash: it is one view's
//! geometry drawn with another view's layer index.
//!
//! # Why the announcement is copied
//!
//! A `GeometryAdd`'s attribute, segment and texture runs are spans into the *ring*, which the
//! producer reuses as soon as the tail advances past them. A view may use that geometry many
//! frames later. So the runs are read out when the announcement arrives, not when the use does —
//! the alternative reads whatever the producer has since written there, which is geometry made of
//! noise rather than a missing draw.
//!
//! # Why the uses are kept
//!
//! A use is *durable*. The producer sends one when a drawable enters a view's cover and not again
//! while it stays, so a geometry re-announced afterwards — `AttributesModified`, say — has nothing
//! arriving to join it to. Keeping the uses is what lets the second announcement reach the
//! consumer at all.
//!
//! Every view's use is kept, not the last one. The C++ this is ported from holds one use per
//! geometry, so a re-announcement re-joins whichever view was seen most recently and the others
//! silently keep the old geometry. That is invisible with one view and is the quad's ordinary
//! case with four.

use alloc::collections::BTreeMap;
use alloc::vec::Vec;

use tessella_capture_abi::envelope::{
    AttributeDesc, GeometryAdd, GeometryId, Segment, TextureRef, ViewId, ViewUse,
};

/// The shared half of a drawable, held until a view uses it.
#[derive(Debug, Clone, PartialEq)]
pub struct Announcement {
    /// The record as it arrived.
    pub add: GeometryAdd,
    /// Ring position just past this announcement.
    ///
    /// Carried onto every drawable joined from it, so a consumer can acknowledge its upload
    /// against the position the bytes were announced at rather than against the frame it drew
    /// them in.
    pub announced_at: u64,
    /// The attribute run, read out of the payload rather than left as a span.
    pub attrs: Vec<AttributeDesc>,
    /// The instanced attribute run.
    pub instance_attrs: Vec<AttributeDesc>,
    /// The segment run.
    pub segments: Vec<Segment>,
    /// The texture bindings.
    pub texture_refs: Vec<TextureRef>,
}

/// A drawable: what every view shares, and what one view adds.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Drawable<'a> {
    /// The shared half.
    pub geometry: &'a Announcement,
    /// The per-view half.
    pub use_: &'a ViewUse,
}

/// Holds announcements and uses, and pairs them.
///
/// Both halves outlive a frame: geometry is announced once and used by any number of views over
/// any number of frames, which is the whole point of the split.
#[derive(Debug, Clone, Default)]
pub struct Joiner {
    geometry: BTreeMap<GeometryId, Announcement>,
    /// Uses per geometry, one entry per view, latest winning within a view.
    uses: BTreeMap<GeometryId, Vec<ViewUse>>,
}

impl Joiner {
    /// An empty joiner.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Takes a geometry announcement, and reports every view that already uses it.
    ///
    /// The returned views are the ones whose drawables have changed: on a first announcement that
    /// is usually none, and on a re-announcement it is every view holding the old geometry.
    pub fn announce(&mut self, announcement: Announcement) -> impl Iterator<Item = ViewId> + '_ {
        let id = announcement.add.geometry;
        self.geometry.insert(id, announcement);
        self.uses
            .get(&id)
            .into_iter()
            .flatten()
            .map(|use_| use_.view)
    }

    /// Takes a view's use of a geometry.
    ///
    /// Returns whether it can be drawn now — that is, whether its announcement has arrived. A use
    /// whose geometry has not been announced is kept rather than dropped: the producer may
    /// announce it later, and the use is durable.
    pub fn used(&mut self, use_: ViewUse) -> bool {
        let held = self.uses.entry(use_.geometry).or_default();
        if let Some(prior) = held.iter_mut().find(|prior| prior.view == use_.view) {
            *prior = use_;
        } else {
            held.push(use_);
        }
        self.geometry.contains_key(&use_.geometry)
    }

    /// The drawable a view draws for this geometry, if both halves are in.
    #[must_use]
    pub fn drawable(&self, geometry: GeometryId, view: ViewId) -> Option<Drawable<'_>> {
        let announcement = self.geometry.get(&geometry)?;
        let use_ = self
            .uses
            .get(&geometry)?
            .iter()
            .find(|use_| use_.view == view)?;
        Some(Drawable {
            geometry: announcement,
            use_,
        })
    }

    /// Every drawable of one geometry, across the views using it.
    pub fn drawables(&self, geometry: GeometryId) -> impl Iterator<Item = Drawable<'_>> + '_ {
        self.geometry
            .get(&geometry)
            .into_iter()
            .flat_map(move |announcement| {
                self.uses
                    .get(&geometry)
                    .into_iter()
                    .flatten()
                    .map(move |use_| Drawable {
                        geometry: announcement,
                        use_,
                    })
            })
    }

    /// Every view holding a claim on a geometry.
    ///
    /// Read before retiring it, so a caller can mark those views' work stale: a geometry going
    /// away changes what they draw, and nothing else tells them.
    pub fn views(&self, geometry: GeometryId) -> impl Iterator<Item = ViewId> + '_ {
        self.uses
            .get(&geometry)
            .into_iter()
            .flatten()
            .map(|use_| use_.view)
    }

    /// Drops one view's claim on a geometry, leaving the geometry for the views that remain.
    ///
    /// Returns whether anything was holding it. `ViewRelease` and `GeometryRemove` are distinct in
    /// this ABI where mbgl had one: a view dropping its hold is not the geometry retiring, and
    /// treating them alike retires geometry three other views are still drawing.
    pub fn release(&mut self, geometry: GeometryId, view: ViewId) -> bool {
        let Some(held) = self.uses.get_mut(&geometry) else {
            return false;
        };
        let before = held.len();
        held.retain(|use_| use_.view != view);
        let dropped = held.len() != before;
        if held.is_empty() {
            self.uses.remove(&geometry);
        }
        dropped
    }

    /// Retires a geometry and every view's use of it.
    ///
    /// Returns whether it was held. A retire for something never announced is not an error: the
    /// producer may retire a geometry this consumer joined the stream too late to see.
    pub fn retire(&mut self, geometry: GeometryId) -> bool {
        self.uses.remove(&geometry);
        self.geometry.remove(&geometry).is_some()
    }

    /// How many geometries are held.
    #[must_use]
    pub fn geometries(&self) -> usize {
        self.geometry.len()
    }

    /// How many uses are held, across every geometry.
    ///
    /// Counted because a consumer that leaks uses leaks them quietly: the map grows with the
    /// cover and nothing draws differently until the machine runs out.
    #[must_use]
    pub fn uses(&self) -> usize {
        self.uses.values().map(Vec::len).sum()
    }
}
