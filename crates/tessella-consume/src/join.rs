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
    AttributeDesc, GeometryAdd, GeometryId, MeshAdd, Segment, TextureRef, ViewId, ViewUse,
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

/// The shared half of an authored mesh, held until a view uses it.
///
/// A `MeshAdd` rather than a `GeometryAdd`, and so not an [`Announcement`]: a model tile arrives
/// authored and there is no mbgl family for a PBR mesh, so there are no attributes to describe, no
/// segments and no shader to name. What it carries is where the bytes are and what they are.
///
/// Its id is in the same space as a geometry's, which is what lets a `ViewUse` bind it, a
/// `ViewRelease` release it and a `GeometryRemove` drop it with no record of their own.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Mesh {
    /// The record as it arrived.
    pub add: MeshAdd,
    /// Ring position just past this announcement, read the way [`Announcement::announced_at`] is.
    pub announced_at: u64,
}

/// A mesh a view draws: the asset and that view's placement of it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MeshDrawable<'a> {
    /// The shared half.
    pub mesh: &'a Mesh,
    /// The per-view half, which carries the layer, the tile and the draw flags.
    pub use_: &'a ViewUse,
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
    /// Authored meshes, in the same id space as `geometry` and never both.
    ///
    /// A separate map rather than an enum in one, because every reader of `geometry` wants the
    /// attributes and the shader a mesh does not have: folded together, each would have to ask
    /// which kind it got before it could use the answer. Kept in the same id space, which is the
    /// ABI's own choice, so `uses` serves both.
    meshes: BTreeMap<GeometryId, Mesh>,
    /// One entry per (geometry, view), latest winning within a view.
    ///
    /// Keyed by the pair rather than by the geometry with a `Vec` of uses behind it. That shape
    /// allocated a `Vec` for every geometry to hold a single forty-byte use: measured at 1.17
    /// allocations per geometry against 0.17 for the announcements, which are the same count of
    /// B-tree inserts without the inner buffer. Keyed by the pair it is 0.2, and `drawable` is a
    /// direct lookup rather than a lookup and a scan.
    ///
    /// Everything per geometry is a range: the views using it are the keys from `(g, 0)` through
    /// `(g, u32::MAX)`, which is why the id order matters and the pair is in this order.
    uses: BTreeMap<(GeometryId, ViewId), ViewUse>,
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
        self.range(id).map(|(_, use_)| use_.view)
    }

    /// Takes a mesh announcement, and reports every view that already uses it.
    ///
    /// The same shape as [`Self::announce`] and for the same reason: a use arrives once and is
    /// durable, so a view may be holding one before the asset it names has been announced.
    pub fn announce_mesh(&mut self, mesh: Mesh) -> impl Iterator<Item = ViewId> + '_ {
        let id = mesh.add.mesh;
        self.meshes.insert(id, mesh);
        self.range(id).map(|(_, use_)| use_.view)
    }

    /// The mesh an id names, if one was announced.
    #[must_use]
    pub fn mesh(&self, id: GeometryId) -> Option<&Mesh> {
        self.meshes.get(&id)
    }

    /// The mesh a view draws for this id, if both halves are in.
    #[must_use]
    pub fn mesh_drawable(&self, id: GeometryId, view: ViewId) -> Option<MeshDrawable<'_>> {
        let mesh = self.meshes.get(&id)?;
        let use_ = self.uses.get(&(id, view))?;
        Some(MeshDrawable { mesh, use_ })
    }

    /// How many meshes are held.
    #[must_use]
    pub fn meshes(&self) -> usize {
        self.meshes.len()
    }

    /// Takes a view's use of a geometry.
    ///
    /// Returns whether it can be drawn now — that is, whether its announcement has arrived. A use
    /// whose geometry has not been announced is kept rather than dropped: the producer may
    /// announce it later, and the use is durable.
    pub fn used(&mut self, use_: ViewUse) -> bool {
        self.uses.insert((use_.geometry, use_.view), use_);
        // Either kind: the id space is one, and a use that named a mesh is as drawable as a use
        // that named geometry -- see [`Mesh`].
        self.geometry.contains_key(&use_.geometry) || self.meshes.contains_key(&use_.geometry)
    }

    /// The drawable a view draws for this geometry, if both halves are in.
    #[must_use]
    pub fn drawable(&self, geometry: GeometryId, view: ViewId) -> Option<Drawable<'_>> {
        let announcement = self.geometry.get(&geometry)?;
        let use_ = self.uses.get(&(geometry, view))?;
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
                self.range(geometry).map(move |(_, use_)| Drawable {
                    geometry: announcement,
                    use_,
                })
            })
    }

    /// Every use of one geometry, as a range over the pair key.
    fn range(
        &self,
        geometry: GeometryId,
    ) -> impl Iterator<Item = (&(GeometryId, ViewId), &ViewUse)> {
        self.uses
            .range((geometry, ViewId(0))..=(geometry, ViewId(u32::MAX)))
    }

    /// Every view holding a claim on a geometry.
    ///
    /// Read before retiring it, so a caller can mark those views' work stale: a geometry going
    /// away changes what they draw, and nothing else tells them.
    pub fn views(&self, geometry: GeometryId) -> impl Iterator<Item = ViewId> + '_ {
        self.range(geometry).map(|(_, use_)| use_.view)
    }

    /// Drops one view's claim on a geometry, leaving the geometry for the views that remain.
    ///
    /// Returns whether anything was holding it. `ViewRelease` and `GeometryRemove` are distinct in
    /// this ABI where mbgl had one: a view dropping its hold is not the geometry retiring, and
    /// treating them alike retires geometry three other views are still drawing.
    pub fn release(&mut self, geometry: GeometryId, view: ViewId) -> bool {
        self.uses.remove(&(geometry, view)).is_some()
    }

    /// Drops every use belonging to one view, and answers how many there were.
    ///
    /// What a `ViewUndeclare` costs: the view is gone, so its uses are too. The *announcements* are
    /// not touched -- geometry is shared and another view may still hold it, which is the whole
    /// point of the split. One that nothing uses any more is retired by the producer in its own
    /// time.
    pub fn release_view(&mut self, view: ViewId) -> usize {
        let held: Vec<(GeometryId, ViewId)> = self
            .uses
            .keys()
            .filter(|(_, held)| *held == view)
            .copied()
            .collect();
        for key in &held {
            self.uses.remove(key);
        }
        held.len()
    }

    /// Retires a geometry and every view's use of it.
    ///
    /// Returns whether it was held. A retire for something never announced is not an error: the
    /// producer may retire a geometry this consumer joined the stream too late to see.
    pub fn retire(&mut self, geometry: GeometryId) -> bool {
        let views: Vec<ViewId> = self.views(geometry).collect();
        for view in views {
            self.uses.remove(&(geometry, view));
        }
        // One id space, one removal record: `GeometryRemove` drops a mesh exactly as it drops
        // geometry, which is what the ABI says of a mesh's id.
        let had_geometry = self.geometry.remove(&geometry).is_some();
        self.meshes.remove(&geometry).is_some() || had_geometry
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
        self.uses.len()
    }
}
