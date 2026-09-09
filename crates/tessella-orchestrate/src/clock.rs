//! A clock, where there is one.
//!
//! # Why not `web-time`
//!
//! `std::time::Instant::now` compiles for wasm32 and panics when it runs, which is what made a
//! clock the third item of WS-0. The answer taken then was `web-time`, a re-export of `std` off
//! wasm and `performance.now` on it. Measuring the artifact showed what that cost: `web-time`
//! reaches `performance` through `js-sys`, `js-sys` is `wasm-bindgen`, and the built module
//! carried 1,380 `__wbindgen_describe_*` exports. §19.2 rules `wasm-bindgen` out of the producer
//! in as many words, so the answer was wrong -- and wrong in the way this section keeps warning
//! about, since every lane was green and only the module said so.
//!
//! # What replaces it
//!
//! Nothing, on wasm. The only clock that changes what is *drawn* is the fade clock, and the ABI
//! already has the host supply that: `tessella_advance` is how a consumer says time has passed,
//! precisely because a producer that read its own clock would disagree with the compositor
//! driving it. What is left is the cold-start trace -- `style_parsed`, `sources_resolved` and the
//! rest -- which is a diagnostic that no behaviour reads. It reports zero on wasm, and the
//! orderings the tests assert over it still hold.
//!
//! A browser that wants those numbers has `performance.now` in its own hands and a tick to
//! record it around, which is the better place for it anyway: it measures what the consumer
//! actually waited for rather than what the producer thinks it spent.

/// A monotonic instant, where the target has one.
#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
pub use std::time::Instant;

/// A monotonic instant, on a target with no clock the producer may read.
///
/// Zero-sized and always the same moment, so every `elapsed` is zero. Deliberately not a panic
/// and not an error: the trace is optional, and a producer that refused to start because it could
/// not time itself would be trading the map for the measurement.
#[cfg(all(target_family = "wasm", target_os = "unknown"))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct Instant;

#[cfg(all(target_family = "wasm", target_os = "unknown"))]
impl Instant {
    /// The current moment, which here is the only moment.
    #[must_use]
    pub const fn now() -> Self {
        Self
    }

    /// How long since, which here is always nothing.
    #[must_use]
    pub const fn elapsed(&self) -> core::time::Duration {
        core::time::Duration::ZERO
    }
}
