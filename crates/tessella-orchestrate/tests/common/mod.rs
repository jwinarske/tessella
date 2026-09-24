// SPDX-License-Identifier: BSD-2-Clause
//! Helpers shared by the golden tests.
//!
//! Each integration test is its own crate, so this is reached with `mod common;` and compiles once
//! per test binary. `dead_code` is allowed because no single test uses all of it, and an unused
//! helper is not a defect here -- it is a helper some other binary wants.

#![allow(dead_code)]

/// FNV-1a's 64-bit offset basis.
///
/// Also what the probe reports as a texture's hash when it saw no bytes for it: its `hash()`
/// returns the seed unchanged rather than erroring, so this value in a `texture ... hash=` line
/// means "nothing was recorded", not "the bytes hashed to this".
pub(crate) const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;

/// FNV-1a's 64-bit prime.
const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;

/// FNV-1a, as `capture_probe.cpp` hashes buffers and texture bytes.
///
/// Shared rather than copied because the constants are a contract with the oracle -- the probe
/// seeds each texture with [`FNV_OFFSET`] and folds every upload into the running value -- and
/// thirteen transcriptions were thirteen chances to mistype a digit (tessella#291). They were
/// checked against each other before being collapsed: all thirteen agreed, differing only in how
/// the prime's underscores were grouped and whether the locals were named `hash`/`byte` or `h`/`b`.
#[must_use]
pub(crate) fn fnv1a(bytes: &[u8]) -> u64 {
    let mut hash = FNV_OFFSET;
    for byte in bytes {
        hash = (hash ^ u64::from(*byte)).wrapping_mul(FNV_PRIME);
    }
    hash
}
