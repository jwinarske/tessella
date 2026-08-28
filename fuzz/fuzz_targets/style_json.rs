#![no_main]
//! A style document, from wherever a style URL points.
//!
//! Deeper than it looks: the expression parser inside it is recursive, and a document is free to
//! nest to whatever depth it likes. What this is looking for is the depth at which that stops
//! returning an error and starts overflowing the stack.

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let Ok(text) = core::str::from_utf8(data) else {
        return;
    };
    let _ = tessella_style::Style::parse(text);
});
