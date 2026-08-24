//! The process-scoped tile store (§5.1, §5.5).
//!
//! # What sharing means, concretely
//!
//! §5 puts ownership at the process rather than the view. A tile's decoded features and built
//! buckets are functions of `(source, tile, zoom used, style revision)` and nothing else — so
//! two views looking at the same place at the same zoom want the *same object*, not two equal
//! ones.
//!
//! "Camera-free" is the usual shorthand and it is not quite true. A paint property that varies
//! with zoom *as well as* per feature cannot be a uniform, and its per-feature part cannot be
//! deferred to one, so its value at each end of the tile's zoom range is stored in the vertices
//! and a `_t` uniform mixes between them per view per frame. The endpoints depend on the zoom
//! the tile is *used* at — `overscaled_z`, not `z` — which is why that is in the key. What
//! stays camera-free is everything continuous: fractional zoom, centre, bearing and pitch never
//! reach a bucket, which is what makes sharing between views at different fractional zooms work
//! at all.
//!
//! mbgl cannot do this: every `Map` owns its own style, tile pyramid, file sources, atlases and
//! workers, so N views are N fetches, N decodes, N bucket builds. §5 calls the shared store R0
//! architecture even while R0 runs one view, precisely so that sharing is not retrofitted into
//! a design that assumed duplication.
//!
//! # Retain is a count, not a flag
//!
//! A view's cover is a set of handles, and a tile is retained while any view holds it (§5.1).
//! Refcounting rather than a boolean is what makes §13.2's retain-chain unification work: one
//! view's active tiles are another's retained ancestors, so a tile can be simultaneously
//! current for one view and insurance for another, and releasing it from one must not drop it.
//!
//! R-11 is the risk this creates — one view's zoom behaviour extends another's tile lifetimes —
//! which is why eviction sheds unretained entries first and why the cap is on the store rather
//! than per view.
//!
//! # The style revision is part of the key
//!
//! A bucket built against one style is not valid against another: a changed filter admits
//! different features and a changed paint property changes what is data-driven. §5.1 keys on
//! the revision so a live restyle repoints rather than mutating, and stale buckets fall out of
//! the cache instead of being silently reused.

use std::collections::BTreeMap;
use std::sync::Arc;

/// Identifies a tile's contribution to a style.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct TileKey {
    /// Source id the tile came from.
    pub source: String,
    /// Zoom.
    pub z: u8,
    /// Column.
    pub x: u32,
    /// Row.
    pub y: u32,
    /// The zoom the entry's buckets were built for — mbgl's `overscaledZ`.
    ///
    /// # Why this is part of the key and not a display detail
    ///
    /// A bucket is only shareable between views that would build it identically. That held
    /// trivially while nothing in a bucket depended on zoom, and stops holding the moment a
    /// paint property varies with it: a zoom-varying property is stored as its value at
    /// `overscaled_z` and at `overscaled_z + 1`, so the same canonical tile standing in at two
    /// different zooms is two different buckets. Keying only on `(z, x, y)` would hand one
    /// view the other's endpoints — wrong colours and widths, and invisible at integer zoom,
    /// which is where one would look first.
    pub overscaled_z: u8,
    /// Style revision the entry was built against.
    pub style_rev: u64,
}

impl TileKey {
    /// A key for a tile drawn at its own zoom.
    #[must_use]
    pub fn new(source: impl Into<String>, z: u8, x: u32, y: u32, style_rev: u64) -> Self {
        Self {
            source: source.into(),
            z,
            x,
            y,
            overscaled_z: z,
            style_rev,
        }
    }

    /// A key for a tile standing in above its own zoom.
    ///
    /// # Panics
    ///
    /// When `overscaled_z` is below `z`, which is not overscaling but a different tile.
    #[must_use]
    pub fn overscaled(
        source: impl Into<String>,
        z: u8,
        x: u32,
        y: u32,
        overscaled_z: u8,
        style_rev: u64,
    ) -> Self {
        assert!(
            overscaled_z >= z,
            "overscaled_z is below the tile's own zoom"
        );
        Self {
            source: source.into(),
            z,
            x,
            y,
            overscaled_z,
            style_rev,
        }
    }
}

impl std::fmt::Display for TileKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{}@{}:{}/{}/{}",
            self.source, self.style_rev, self.z, self.x, self.y
        )?;
        if self.overscaled_z != self.z {
            write!(f, "@{}", self.overscaled_z)?;
        }
        Ok(())
    }
}

#[derive(Debug)]
struct Entry<T> {
    value: Arc<T>,
    /// How many views hold this tile. Zero means evictable, not absent.
    retained: usize,
    /// Monotonic tick of last use, for LRU ordering.
    used: u64,
}

/// What a lookup did, so a caller can count shared work honestly.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lookup {
    /// The value was already there. No work was done.
    Hit,
    /// The value was built. This is the only case that should increment a §9.3 counter.
    Miss,
}

impl Lookup {
    /// True when work was done.
    #[must_use]
    pub const fn did_work(self) -> bool {
        matches!(self, Self::Miss)
    }
}

/// A refcounted, LRU-bounded store of per-tile values.
///
/// Generic over what is stored because §5.1 shares several things on the same key — decoded
/// features, buckets, symbol layout — and they have the same lifetime rules and different
/// types.
#[derive(Debug)]
pub struct TileStore<T> {
    entries: BTreeMap<TileKey, Entry<T>>,
    capacity: usize,
    clock: u64,
    evictions: u64,
}

impl<T> TileStore<T> {
    /// A store holding at most `capacity` unretained entries beyond those in use.
    ///
    /// The cap bounds memory across every view at once rather than per view, which is what
    /// stops a pathological view from being charged to everyone else (R-11).
    #[must_use]
    pub fn new(capacity: usize) -> Self {
        Self {
            entries: BTreeMap::new(),
            capacity,
            clock: 0,
            evictions: 0,
        }
    }

    /// Fetches a tile's value, building it only if it is not already there.
    ///
    /// The returned [`Lookup`] says which happened, so a caller can record shared work on a
    /// miss and nothing on a hit. That distinction is the §9.3 flatness assertion: with a
    /// shared store, N views over one cover produce one miss and N-1 hits.
    pub fn get_or_build(&mut self, key: &TileKey, build: impl FnOnce() -> T) -> (Arc<T>, Lookup) {
        self.clock += 1;
        if let Some(entry) = self.entries.get_mut(key) {
            entry.used = self.clock;
            return (Arc::clone(&entry.value), Lookup::Hit);
        }

        let value = Arc::new(build());
        self.entries.insert(
            key.clone(),
            Entry {
                value: Arc::clone(&value),
                retained: 0,
                used: self.clock,
            },
        );
        self.evict_if_needed(key);
        (value, Lookup::Miss)
    }

    /// Looks a tile up without building it.
    #[must_use]
    pub fn get(&self, key: &TileKey) -> Option<Arc<T>> {
        self.entries.get(key).map(|entry| Arc::clone(&entry.value))
    }

    /// Marks a tile as held by one more view.
    ///
    /// Retaining a tile that is not present does nothing: a view cannot hold what was never
    /// built, and silently inserting an empty entry would make the store lie about what it has.
    pub fn retain(&mut self, key: &TileKey) {
        if let Some(entry) = self.entries.get_mut(key) {
            entry.retained += 1;
        }
    }

    /// Releases one view's hold.
    ///
    /// The entry stays: an unretained tile is a candidate for eviction, not a deletion. That is
    /// what makes a tile that briefly leaves a cover cheap to get back, and what §13.2's
    /// never-blank retention depends on.
    pub fn release(&mut self, key: &TileKey) {
        if let Some(entry) = self.entries.get_mut(key)
            && entry.retained > 0
        {
            entry.retained -= 1;
        }
    }

    /// How many views hold a tile.
    #[must_use]
    pub fn retain_count(&self, key: &TileKey) -> usize {
        self.entries.get(key).map_or(0, |entry| entry.retained)
    }

    /// Entries currently held.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// True when the store holds nothing.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Entries evicted over the store's life, for pressure diagnostics.
    #[must_use]
    pub fn evictions(&self) -> u64 {
        self.evictions
    }

    /// Drops unretained entries until the store is within its cap.
    ///
    /// Retained entries are never evicted, however old: a tile some view is drawing with must
    /// not vanish underneath it. A store whose retained set exceeds the cap therefore grows
    /// past it, which is correct — the alternative is a hole in the map — and is the pressure
    /// R-11 says to shed by per-view retain budgets rather than by breaking this rule.
    ///
    /// `just_built` is excluded, and that exclusion is load-bearing rather than an
    /// optimization. Eviction runs inside `get_or_build`, before the caller has had a chance to
    /// retain what it asked for, so when every other entry is held the new one is the only
    /// candidate — and evicting it means the work was done and thrown away, guaranteeing that
    /// the next view rebuilds it. Under exactly the pressure the cap exists to manage, the
    /// store would stop sharing.
    fn evict_if_needed(&mut self, just_built: &TileKey) {
        while self.entries.len() > self.capacity {
            let victim = self
                .entries
                .iter()
                .filter(|(key, entry)| entry.retained == 0 && *key != just_built)
                .min_by_key(|(_, entry)| entry.used)
                .map(|(key, _)| key.clone());

            match victim {
                Some(key) => {
                    self.entries.remove(&key);
                    self.evictions += 1;
                }
                // Everything is retained. Growing past the cap beats dropping a tile a view is
                // drawing with.
                None => break,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    fn key(x: u32) -> TileKey {
        TileKey::new("probe", 13, x, 2723, 1)
    }

    /// The flatness property, at the store level: N views over one cover produce one miss and
    /// N-1 hits, so the work happens once however many views want it.
    #[test]
    fn many_views_over_one_cover_build_once() {
        let mut store: TileStore<u32> = TileStore::new(64);
        let builds = Cell::new(0u32);

        let mut misses = 0;
        for _view in 0..4 {
            for x in 4092..4095 {
                let (_, lookup) = store.get_or_build(&key(x), || {
                    builds.set(builds.get() + 1);
                    x
                });
                if lookup.did_work() {
                    misses += 1;
                }
            }
        }

        assert_eq!(builds.get(), 3, "three tiles, not twelve");
        assert_eq!(misses, 3);
        assert_eq!(store.len(), 3);
    }

    /// And the views get the *same object*, not equal copies. Sharing a value is what makes one
    /// GPU buffer serve every view (§5.3); sharing a recipe would not.
    #[test]
    fn views_share_the_object_not_a_copy() {
        let mut store: TileStore<u32> = TileStore::new(64);
        let (first, _) = store.get_or_build(&key(4092), || 7);
        let (second, lookup) = store.get_or_build(&key(4092), || 9);

        assert_eq!(lookup, Lookup::Hit);
        assert!(Arc::ptr_eq(&first, &second));
        assert_eq!(*second, 7, "the second build never ran");
    }

    /// A bucket built against one style is not valid against another: a changed filter admits
    /// different features. The revision is in the key so a restyle repoints rather than reusing.
    #[test]
    fn a_style_revision_change_is_a_different_tile() {
        let mut store: TileStore<u32> = TileStore::new(64);
        let old = TileKey::new("probe", 13, 4092, 2723, 1);
        let new = TileKey::new("probe", 13, 4092, 2723, 2);

        store.get_or_build(&old, || 1);
        let (value, lookup) = store.get_or_build(&new, || 2);
        assert_eq!(lookup, Lookup::Miss, "a new revision rebuilds");
        assert_eq!(*value, 2);
        assert_eq!(store.len(), 2);
    }

    /// Sources are separate too: the same tile address in two sources is two tiles.
    #[test]
    fn sources_do_not_collide() {
        let mut store: TileStore<u32> = TileStore::new(64);
        store.get_or_build(&TileKey::new("a", 13, 1, 1, 1), || 1);
        let (_, lookup) = store.get_or_build(&TileKey::new("b", 13, 1, 1, 1), || 2);
        assert_eq!(lookup, Lookup::Miss);
    }

    /// Retain counts rather than flags. §13.2 unifies retain chains across views, so a tile can
    /// be current for one view and a retained ancestor for another; releasing it from one must
    /// not drop it.
    #[test]
    fn retain_survives_release_by_one_view() {
        let mut store: TileStore<u32> = TileStore::new(1);
        store.get_or_build(&key(4092), || 1);
        store.retain(&key(4092));
        store.retain(&key(4092));
        assert_eq!(store.retain_count(&key(4092)), 2);

        store.release(&key(4092));
        assert_eq!(store.retain_count(&key(4092)), 1, "one view still holds it");

        // And a tile held by anyone is not evicted, even past the cap.
        store.get_or_build(&key(4093), || 2);
        store.get_or_build(&key(4094), || 3);
        assert!(store.get(&key(4092)).is_some(), "retained, so kept");
    }

    /// An unretained entry is a candidate for eviction, not a deletion. A tile that briefly
    /// leaves a cover should be cheap to get back.
    #[test]
    fn releasing_does_not_delete() {
        let mut store: TileStore<u32> = TileStore::new(64);
        store.get_or_build(&key(4092), || 1);
        store.retain(&key(4092));
        store.release(&key(4092));

        assert_eq!(store.retain_count(&key(4092)), 0);
        assert!(store.get(&key(4092)).is_some(), "still cached");

        let (_, lookup) = store.get_or_build(&key(4092), || 99);
        assert_eq!(lookup, Lookup::Hit, "and cheap to get back");
    }

    #[test]
    fn eviction_takes_the_least_recently_used_unretained_entry() {
        let mut store: TileStore<u32> = TileStore::new(2);
        store.get_or_build(&key(1), || 1);
        store.get_or_build(&key(2), || 2);

        // Touch 1 so 2 is the oldest.
        store.get_or_build(&key(1), || 0);
        store.get_or_build(&key(3), || 3);

        assert!(store.get(&key(1)).is_some(), "recently used");
        assert!(store.get(&key(2)).is_none(), "evicted");
        assert!(store.get(&key(3)).is_some());
        assert_eq!(store.evictions(), 1);
    }

    /// A store whose retained set exceeds its cap grows past it. Dropping a tile a view is
    /// drawing with would be a hole in the map, which is worse than exceeding a soft bound —
    /// R-11 sheds this pressure with per-view retain budgets rather than by breaking the rule.
    #[test]
    fn retained_entries_are_never_evicted() {
        let mut store: TileStore<u32> = TileStore::new(2);
        for x in 0..5 {
            store.get_or_build(&key(x), || x);
            store.retain(&key(x));
        }

        assert_eq!(store.len(), 5, "over the cap, because all are held");
        assert_eq!(store.evictions(), 0);
        for x in 0..5 {
            assert!(store.get(&key(x)).is_some());
        }

        // Release them and the cap reasserts itself on the next insert.
        for x in 0..5 {
            store.release(&key(x));
        }
        store.get_or_build(&key(99), || 99);
        assert!(store.len() <= 2, "cap reasserted, got {}", store.len());
    }

    /// Retaining something absent does nothing rather than inserting a placeholder. A view
    /// cannot hold what was never built, and an empty entry would make the store lie.
    #[test]
    fn retaining_an_absent_tile_does_nothing() {
        let mut store: TileStore<u32> = TileStore::new(4);
        store.retain(&key(4092));
        assert!(store.is_empty());
        assert_eq!(store.retain_count(&key(4092)), 0);

        // And releasing below zero does not wrap.
        store.get_or_build(&key(4092), || 1);
        store.release(&key(4092));
        store.release(&key(4092));
        assert_eq!(store.retain_count(&key(4092)), 0);
    }
}

#[cfg(test)]
mod overscale_tests {
    use super::*;

    /// The overscale factor discriminates. Two views using the same canonical tile at different
    /// zooms must not be served each other's buckets, because a zoom-varying paint property's
    /// endpoints are evaluated at that zoom.
    #[test]
    fn the_overscaled_zoom_is_part_of_the_key() {
        let mut store: TileStore<u32> = TileStore::new(8);
        let own = TileKey::new("s", 13, 4093, 2723, 1);
        let stood_in = TileKey::overscaled("s", 13, 4093, 2723, 15, 1);

        assert_ne!(own, stood_in);

        let (first, a) = store.get_or_build(&own, || 1);
        let (second, b) = store.get_or_build(&stood_in, || 2);
        assert_eq!(a, Lookup::Miss);
        assert_eq!(b, Lookup::Miss, "not served the other's bucket");
        assert_eq!((*first, *second), (1, 2));
        assert_eq!(store.len(), 2);

        // And a second view wanting the same tile at the same zoom still shares.
        let (third, c) = store.get_or_build(&own, || 3);
        assert_eq!(c, Lookup::Hit);
        assert_eq!(*third, 1);
    }

    /// A tile at its own zoom carries an overscale equal to its zoom, so the plain constructor
    /// and the explicit one agree — sharing is not lost by spelling the same tile two ways.
    #[test]
    fn the_two_constructors_agree_at_a_tiles_own_zoom() {
        assert_eq!(
            TileKey::new("s", 13, 1, 2, 7),
            TileKey::overscaled("s", 13, 1, 2, 13, 7)
        );
    }

    #[test]
    #[should_panic(expected = "below the tile's own zoom")]
    fn an_overscale_below_the_tiles_zoom_is_refused() {
        let _ = TileKey::overscaled("s", 13, 1, 2, 12, 7);
    }
}
