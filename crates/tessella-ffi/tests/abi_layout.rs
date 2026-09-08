//! The layout `include/tessella.h` claims, asserted against the Rust types it describes.
//!
//! # Why this exists beside the header's own assertions
//!
//! `tessella.h` is hand-written -- unlike `tessella_capture_abi.h`, which is generated from the
//! Rust types and cannot disagree with them. Its `_Static_assert`s check the C struct against
//! itself: that `width` sits where the header says it sits. What they cannot check is whether
//! the *Rust* struct agrees, and that is the direction a mismatch actually breaks in -- a field
//! added on this side, the header left alone, and a consumer reading the style pointer out of
//! what is now the length.
//!
//! So the same numbers are asserted here from `offset_of!`. Between the two, a change to either
//! side without the other fails: the C compiler catches the header drifting from itself, and
//! this catches it drifting from Rust.

use std::mem::{align_of, offset_of, size_of};

use tessella_ffi::{Config, Regions};

/// What `sizeof(void*)` is on this target, which is what the header's assertions are written in.
const PTR: usize = size_of::<*const u8>();

#[test]
fn the_config_lays_out_the_way_the_header_says() {
    assert_eq!(offset_of!(Config, style_json), 0);
    assert_eq!(offset_of!(Config, style_json_len), PTR);
    assert_eq!(offset_of!(Config, width), 2 * PTR);
    // Not asserted by the header, and asserted here because it is the property the header's
    // arithmetic rests on: a pointer and a `size_t` are the same width, so `2 * sizeof(void*)`
    // describes where `width` lands only while that holds.
    assert_eq!(size_of::<usize>(), PTR);
    assert_eq!(align_of::<Config>(), PTR);
}

#[test]
fn the_regions_are_four_words_as_the_header_says() {
    assert_eq!(size_of::<Regions>(), 4 * PTR);
    assert_eq!(offset_of!(Regions, ring), 0);
    assert_eq!(offset_of!(Regions, ring_len), PTR);
    assert_eq!(offset_of!(Regions, slabs), 2 * PTR);
    assert_eq!(offset_of!(Regions, slabs_len), 3 * PTR);
}

/// What a `(pointer, length)` style means in each of the ways it can be wrong.
///
/// The three cases a NUL-terminated parameter did not have to distinguish, and the reason the
/// change is worth a test rather than only a compile: a null pointer is a caller fault, bytes
/// that are not UTF-8 are a document fault, and a zero length is neither -- it is an empty
/// document, which parses as far as "not a style" and no further.
mod supplied {
    use super::Config;
    use tessella_ffi::{MapHandle, Status, tessella_create, tessella_destroy};

    fn config(style: *const u8, len: usize) -> Config {
        Config {
            style_json: style,
            style_json_len: len,
            width: 256,
            height: 256,
            ring_capacity: 1 << 20,
            slab_capacity: 0,
        }
    }

    fn create(config: &Config) -> (Status, MapHandle) {
        let mut out: MapHandle = core::ptr::null_mut();
        // SAFETY: both pointers are valid, and the style is whatever the case under test says.
        let status = unsafe { tessella_create(config, 0.0, 0.0, 0.0, &raw mut out) };
        (status, out)
    }

    #[test]
    fn a_null_pointer_is_a_null_argument() {
        let (status, map) = create(&config(core::ptr::null(), 0));
        assert_eq!(status, Status::NullArgument);
        assert!(map.is_null());
    }

    #[test]
    fn bytes_that_are_not_utf8_are_a_bad_style() {
        // A lone continuation byte: never valid UTF-8, and exactly what a mis-decoded document
        // arrives as. Reported as content rather than as a null argument, because the pointer
        // was fine.
        let bytes = [0x80u8, 0x80];
        let (status, map) = create(&config(bytes.as_ptr(), bytes.len()));
        assert_eq!(status, Status::BadStyle);
        assert!(map.is_null());
    }

    #[test]
    fn a_zero_length_is_an_empty_document_not_an_absent_one() {
        // A real pointer and nothing behind it. Distinguishable from null only because the
        // length is carried, which is the whole point of carrying it.
        let anchor = [0u8; 1];
        let (status, map) = create(&config(anchor.as_ptr(), 0));
        assert_eq!(status, Status::BadStyle);
        assert!(map.is_null());
    }

    #[test]
    fn a_style_is_not_read_past_its_length() {
        // The bytes after the document are garbage that would fail to parse if they were read.
        // A NUL-terminated parameter could not express this at all: it would read to the first
        // zero byte, which is somewhere in the trailing junk.
        let style = concat!(
            r#"{"version": 8, "sources": {}, "layers": [{"id": "bg", "type": "background"}]}"#,
            "!!! not part of the document !!!"
        );
        let len = style.len() - "!!! not part of the document !!!".len();
        let (status, map) = create(&config(style.as_ptr(), len));
        assert_eq!(status, Status::Ok, "the style was read past its length");
        assert!(!map.is_null());
        // SAFETY: the handle came back non-null from a successful create.
        unsafe { tessella_destroy(map) };
    }
}
