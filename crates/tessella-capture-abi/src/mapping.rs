//! A block of memory two processes can both reach.
//!
//! # Why this is here and not where it is used
//!
//! The ring already lives on one: [`ring::init`](crate::ring::init) takes a pointer and a
//! length and builds a producer over it, because a region that crosses a process boundary is
//! exactly what this crate is for. The slab region is the other half of the same story — a
//! producer that allocates geometry straight into a mapping has no pack step that can be late
//! (§11.3) — and the arena that does it lives in `tessella-orchestrate`, which is
//! `deny(unsafe_code)` and has not needed a single allowance.
//!
//! Keeping the raw pointer here rather than there is what preserves that. The obligation is
//! stated once, at the one constructor that can break it, and every user works in slices.

use core::marker::PhantomData;

/// Memory the caller has mapped, borrowed as slices.
///
/// Not `Send` or `Sync`, and deliberately: a mapping's lifetime is the caller's business, and
/// a type that carried one across threads on its own would be promising something this cannot
/// check. A caller that has established the mapping outlives every thread touching it can say
/// so itself.
#[derive(Debug)]
pub struct Mapping {
    base: *mut u8,
    len: usize,
    /// Holds the auto traits off.
    _not_send: PhantomData<*mut u8>,
}

impl Mapping {
    /// Borrows `len` bytes at `base`.
    ///
    /// # Safety
    ///
    /// `base` must point at `len` bytes that stay mapped and writable for as long as this value
    /// lives, and nothing else may write them. Reads by another process are expected — that is
    /// the point — and are what the caller synchronises through the ring's `head`: a store that
    /// publishes a record happens after the writes here, so a consumer that acquires `head`
    /// sees the bytes of every record it can see.
    #[must_use]
    pub const unsafe fn new(base: *mut u8, len: usize) -> Self {
        Self {
            base,
            len,
            _not_send: PhantomData,
        }
    }

    /// How many bytes it covers.
    #[must_use]
    pub const fn len(&self) -> usize {
        self.len
    }

    /// Whether it covers none.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// The bytes.
    #[must_use]
    pub fn bytes(&self) -> &[u8] {
        // SAFETY: the constructor's contract is that these stay mapped for this value's life.
        unsafe { core::slice::from_raw_parts(self.base, self.len) }
    }

    /// The bytes, to write.
    pub fn bytes_mut(&mut self) -> &mut [u8] {
        // SAFETY: as above, and `&mut self` is what excludes another borrow of the same bytes
        // *in this process*. The other process is the caller's obligation, above.
        unsafe { core::slice::from_raw_parts_mut(self.base, self.len) }
    }
}
