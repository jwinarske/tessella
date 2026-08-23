//! Shared-work counters: proving the work does not scale with view count (§9.3, §5.5).
//!
//! # The assertion this exists to make
//!
//! §5.5 lists what is process-scoped: tile fetches, decodes, bucket builds, shaped labels, atlas
//! uploads, compiled materials. §9.3 turns that into a CI assertion — those counts must be
//! **flat in view count for overlapping covers**. Two views looking at the same place do one
//! fetch, one decode, one bucket build, not two.
//!
//! That is the whole architectural claim of §5. mbgl cannot make it: every `Map` owns its own
//! style, tile pyramid, file sources, atlases and workers, so N views are N of everything. §5
//! says the shared-store model is R0 architecture even while R0 runs one view, precisely so
//! that the sharing is not retrofitted later into a design that assumed duplication.
//!
//! # Why the counters come before the four-view benchmark
//!
//! §13.3's benchmark is an R1.5 exit criterion and needs four views, a tile store, cover
//! computation, zoom animation and RK3566 hardware. These counters need two views and a
//! counter. They catch the same failure — per-view duplication of shared work — at the moment
//! someone would introduce it, rather than at R1.5 when it is structural and expensive to undo.
//!
//! A timing benchmark tells you the port got slower. A flatness counter tells you which
//! invariant broke, and it does so on a developer's machine in milliseconds.
//!
//! # Flat means flat, not "grows slowly"
//!
//! The assertion is equality, not a ratio. A shared store that did 1.1 fetches per view would
//! satisfy any bound above one while still duplicating work, and would keep doing so at four
//! views and at forty. What is being asserted is that the work happened *once*, so the count
//! for N views over one cover equals the count for one view over that cover.

use alloc::collections::BTreeMap;
use alloc::string::String;

/// A kind of work §5.5 says happens once per process, not once per view.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum SharedWork {
    /// A tile fetched from a source. Request coalescing means two views wanting one tile
    /// produce one in-flight fetch with two waiters (§5.1).
    TileFetch,
    /// A tile's bytes decoded.
    TileDecode,
    /// A bucket built from a decoded tile. Buckets are functions of `(tile, layer, tile zoom)`
    /// and camera-free, which is what makes them shareable (§5.1).
    BucketBuild,
    /// A text run shaped. Keyed `(fontstack, text, params)` and cached process-wide (§12.3).
    LabelShape,
    /// An atlas region uploaded. One atlas per fontstack, emitted once (§5.1).
    AtlasUpload,
    /// A shader material compiled. Per shader-permutation family, never per view (§5.5).
    MaterialCompile,
}

impl SharedWork {
    /// Every kind, for asserting over all of them at once.
    pub const ALL: [Self; 6] = [
        Self::TileFetch,
        Self::TileDecode,
        Self::BucketBuild,
        Self::LabelShape,
        Self::AtlasUpload,
        Self::MaterialCompile,
    ];

    /// A short name, for failure messages.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::TileFetch => "tile fetch",
            Self::TileDecode => "tile decode",
            Self::BucketBuild => "bucket build",
            Self::LabelShape => "label shape",
            Self::AtlasUpload => "atlas upload",
            Self::MaterialCompile => "material compile",
        }
    }
}

/// Counts shared work, keyed so that duplicate work is visible rather than merely summed.
///
/// The key matters. A plain tally would say "four bucket builds" without saying whether that
/// was four different tiles or one tile built four times — and those are the healthy case and
/// the failure, respectively. Counting per key makes the failure a count greater than one on a
/// single key.
#[derive(Debug, Default)]
pub struct SharedCounters {
    counts: BTreeMap<(SharedWork, String), u64>,
}

impl SharedCounters {
    /// Empty counters.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Records one unit of work on a key.
    ///
    /// The key names *what* the work was done on — a tile address, a shaping key, a shader
    /// permutation — not which view asked for it. A key that includes the view would make every
    /// count trivially one and assert nothing.
    pub fn record(&mut self, work: SharedWork, key: impl Into<String>) {
        *self.counts.entry((work, key.into())).or_insert(0) += 1;
    }

    /// How many times work was done on one key.
    #[must_use]
    pub fn count(&self, work: SharedWork, key: &str) -> u64 {
        self.counts
            .get(&(work, String::from(key)))
            .copied()
            .unwrap_or(0)
    }

    /// Total units of one kind of work.
    #[must_use]
    pub fn total(&self, work: SharedWork) -> u64 {
        self.counts
            .iter()
            .filter(|((kind, _), _)| *kind == work)
            .map(|(_, count)| *count)
            .sum()
    }

    /// Distinct keys one kind of work touched.
    #[must_use]
    pub fn distinct(&self, work: SharedWork) -> usize {
        self.counts.keys().filter(|(kind, _)| *kind == work).count()
    }

    /// Keys that were worked on more than once, with their counts.
    ///
    /// Empty is the invariant. A non-empty result names exactly which shared thing was
    /// duplicated, which is the diagnostic a timing regression cannot give.
    #[must_use]
    pub fn duplicated(&self) -> alloc::vec::Vec<(SharedWork, &str, u64)> {
        self.counts
            .iter()
            .filter(|(_, count)| **count > 1)
            .map(|((work, key), count)| (*work, key.as_str(), *count))
            .collect()
    }

    /// True when nothing was done more than once.
    ///
    /// This is §9.3's flatness assertion: with the store shared, the count for N views over one
    /// cover equals the count for one view, so no key is touched twice.
    #[must_use]
    pub fn is_flat(&self) -> bool {
        self.duplicated().is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The shape of the assertion, on a two-view overlapping cover.
    ///
    /// Two views want the same six tiles. A shared store fetches each once; a per-view store
    /// fetches each twice. The counter distinguishes them, and it does so by naming the tile
    /// rather than by comparing a total against a threshold.
    #[test]
    fn overlapping_views_do_the_work_once() {
        let cover = ["13/4092/2723", "13/4093/2723", "13/4094/2723"];
        let mut shared = SharedCounters::new();

        // Two views, one shared store: the second view finds every tile already there.
        let mut fetched = alloc::collections::BTreeSet::new();
        for _view in 0..2 {
            for tile in cover {
                if fetched.insert(tile) {
                    shared.record(SharedWork::TileFetch, tile);
                    shared.record(SharedWork::TileDecode, tile);
                    shared.record(SharedWork::BucketBuild, tile);
                }
            }
        }

        assert!(shared.is_flat(), "{:?}", shared.duplicated());
        assert_eq!(shared.total(SharedWork::TileFetch), 3);
        assert_eq!(shared.distinct(SharedWork::TileFetch), 3);
    }

    /// And what the failure looks like: the same work per view, which is the mbgl model.
    ///
    /// This is the case the assertion has to catch, so it is worth having a test that the
    /// assertion does not pass it.
    #[test]
    fn per_view_duplication_is_caught() {
        let cover = ["13/4092/2723", "13/4093/2723"];
        let mut duplicated = SharedCounters::new();

        // Two views, each with its own store: every tile fetched twice.
        for _view in 0..2 {
            for tile in cover {
                duplicated.record(SharedWork::TileFetch, tile);
            }
        }

        assert!(!duplicated.is_flat());
        let offenders = duplicated.duplicated();
        assert_eq!(offenders.len(), 2);
        for (work, key, count) in offenders {
            assert_eq!(work, SharedWork::TileFetch);
            assert_eq!(count, 2, "{key} fetched twice");
        }
    }

    /// Flatness holds at any view count, which is what makes it an architectural assertion
    /// rather than a two-view coincidence. Four is the number §13 budgets against.
    #[test]
    fn flatness_holds_at_four_views() {
        let cover = [
            "13/4092/2723",
            "13/4092/2724",
            "13/4093/2723",
            "13/4093/2724",
        ];
        let mut shared = SharedCounters::new();
        let mut built = alloc::collections::BTreeSet::new();

        for _view in 0..4 {
            for tile in cover {
                if built.insert(tile) {
                    shared.record(SharedWork::BucketBuild, tile);
                }
            }
        }

        assert!(shared.is_flat());
        assert_eq!(
            shared.total(SharedWork::BucketBuild),
            4,
            "four tiles, not sixteen"
        );
    }

    /// Non-overlapping covers legitimately do more work. Flatness is about a *shared* cover;
    /// two views looking at different places genuinely need different tiles, and the counter
    /// must not call that a failure.
    #[test]
    fn disjoint_covers_are_not_duplication() {
        let mut shared = SharedCounters::new();
        shared.record(SharedWork::TileFetch, "13/4092/2723");
        shared.record(SharedWork::TileFetch, "13/9000/9000");

        assert!(shared.is_flat(), "different tiles are different work");
        assert_eq!(shared.total(SharedWork::TileFetch), 2);
        assert_eq!(shared.distinct(SharedWork::TileFetch), 2);
    }

    /// Keying by what the work was done on, not by who asked. A key including the view would
    /// make every count one and assert nothing at all — the counter would pass while the
    /// duplication it exists to catch went on underneath it.
    #[test]
    fn keying_by_view_would_assert_nothing() {
        let mut by_view = SharedCounters::new();
        by_view.record(SharedWork::TileFetch, "view0:13/4092/2723");
        by_view.record(SharedWork::TileFetch, "view1:13/4092/2723");

        assert!(
            by_view.is_flat(),
            "which is exactly why keys must not name the view"
        );
        assert_eq!(by_view.total(SharedWork::TileFetch), 2, "yet work doubled");
    }

    #[test]
    fn every_kind_of_shared_work_is_named() {
        for work in SharedWork::ALL {
            assert!(!work.name().is_empty());
        }
        let mut counters = SharedCounters::new();
        for work in SharedWork::ALL {
            counters.record(work, "k");
        }
        assert!(counters.is_flat());
        for work in SharedWork::ALL {
            assert_eq!(counters.total(work), 1, "{}", work.name());
        }
    }
}
