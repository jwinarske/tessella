//! The bytes a frame has to get onto the device, copied off the ring before it moves.
//!
//! The thing under test is a timing property, not a data one: the ring's bytes belong to the
//! producer again as soon as the tail passes, so a consumer that holds a span instead of a copy
//! reads whatever was written there next. These drive a real ring and then keep reading, which is
//! what makes the span go stale.

use tessella_capture_abi::envelope::{
    Extent, Rect16, Span, TextureId, UboUpdate, ViewId, WireRecord,
};
use tessella_capture_abi::ring::Ring;
use tessella_capture_abi::{EnvelopeKind, TextureChannelDataType, TexturePixelType};
use tessella_consume::host::Host;
use tessella_consume::upload::Upload;

const CAPACITY: usize = 1 << 16;

fn uniforms(view: u32, layer: i32, slot: u32, len: u32) -> UboUpdate {
    UboUpdate {
        view: ViewId(view),
        layer_index: layer,
        slot,
        _pad: 0,
        data: Span {
            offset: 0,
            count: len,
        },
    }
}

/// A texture update over `rects`, whose pixels are `len` bytes of packed payload.
///
/// Packed, because that is what `texture::regions` sends whenever the rects fit -- which is the
/// ordinary case -- and because the alternative here would be a 16 KiB payload to describe four
/// texels of damage. `len` is passed rather than derived so a test can hand over a payload that
/// does not match the rects on purpose.
fn texture(id: u64, rects: &[Rect16], len: u32) -> Vec<u8> {
    let mut filled = [Rect16::default(); 4];
    filled[..rects.len()].copy_from_slice(rects);
    let update = tessella_capture_abi::envelope::TextureUpdate {
        texture: TextureId(id),
        size: Extent {
            width: 64,
            height: 64,
        },
        rects: filled,
        pixels: Span {
            offset: 0,
            count: len,
        },
        format: TexturePixelType::RGBA as u8,
        rect_count: u8::try_from(rects.len()).unwrap(),
        channel_type: TextureChannelDataType::UnsignedByte as u8,
        packed: 1,
        _pad: [0; 4],
    };
    update.as_bytes().to_vec()
}

fn rect(x: u16, y: u16, w: u16, h: u16) -> Rect16 {
    Rect16 { x, y, w, h }
}

/// Uniform bytes survive the read that consumed them.
#[test]
fn uniform_bytes_are_copied_off_the_ring() {
    let mut ring = Ring::new(CAPACITY);
    let mut host = Host::new();

    {
        let (producer, _) = ring.split();
        producer
            .write(
                EnvelopeKind::UboUpdate,
                uniforms(0, 3, 7, 8).as_bytes(),
                &[1, 2, 3, 4, 5, 6, 7, 8],
            )
            .expect("room");
    }
    host.read(ring.consumer());

    let work = host.uploads().work();
    assert_eq!(work.len(), 1);
    let Upload::Uniforms {
        view,
        layer_index,
        slot,
        bytes,
    } = &work[0]
    else {
        panic!("a uniform update");
    };
    assert_eq!((*view, *layer_index, *slot), (ViewId(0), 3, 7));
    assert_eq!(
        host.uploads().bytes(bytes),
        Some([1, 2, 3, 4, 5, 6, 7, 8].as_slice()),
        "the bytes are held, not borrowed"
    );
}

/// And they are still right after the ring has been written over.
///
/// This is the whole reason for the copy. The producer reuses the bytes as soon as the tail moves,
/// and the tail moves because reading the next record is what moves it — so a consumer holding a
/// span rather than a copy reads the next frame's records as though they were last frame's
/// uniforms.
#[test]
fn the_copy_survives_the_producer_reusing_the_ring() {
    let mut ring = Ring::new(CAPACITY);
    let mut host = Host::new();

    {
        let (producer, _) = ring.split();
        producer
            .write(
                EnvelopeKind::UboUpdate,
                uniforms(0, 0, 0, 4).as_bytes(),
                &[0xAA, 0xAA, 0xAA, 0xAA],
            )
            .expect("room");
    }
    host.read(ring.consumer());

    // Enough traffic to wrap well past where those bytes were.
    for _ in 0..64 {
        {
            let (producer, _) = ring.split();
            producer
                .write(
                    EnvelopeKind::UboUpdate,
                    uniforms(1, 0, 0, 64).as_bytes(),
                    &[0x55; 64],
                )
                .expect("room");
        }
        host.read(ring.consumer());
    }

    let Upload::Uniforms { bytes, .. } = &host.uploads().work()[0] else {
        panic!("the first update");
    };
    assert_eq!(
        host.uploads().bytes(bytes),
        Some([0xAA, 0xAA, 0xAA, 0xAA].as_slice()),
        "the first update's bytes are what they were"
    );
}

/// A texture update keeps its rects, and only the ones it claimed.
#[test]
fn a_texture_update_keeps_its_rects() {
    let mut ring = Ring::new(CAPACITY);
    let mut host = Host::new();
    let two = [rect(0, 0, 2, 2), rect(8, 8, 4, 4)];

    {
        let (producer, _) = ring.split();
        producer
            // 2x2 and 4x4 RGBA texels, packed: (4 + 16) * 4 bytes.
            .write(EnvelopeKind::TextureUpdate, &texture(5, &two, 80), &[7; 80])
            .expect("room");
    }
    host.read(ring.consumer());

    let Upload::Texture {
        texture: id,
        rects,
        bytes,
        shape,
        ..
    } = &host.uploads().work()[0]
    else {
        panic!("a texture update");
    };
    assert_eq!(*id, TextureId(5));
    assert_eq!(rects.as_slice(), two.as_slice(), "both rects, and no more");
    assert_eq!(shape.format, TexturePixelType::RGBA);
    assert_eq!(host.uploads().bytes(bytes).map(<[u8]>::len), Some(80));
}

/// A rect count past the array is refused rather than clamped.
///
/// The rects say which pixels these bytes are. Clamping to the array would upload them to the
/// wrong place, which is a picture; refusing counts the record as malformed, which is a number
/// somebody can look at.
#[test]
fn a_rect_count_past_the_array_is_malformed() {
    let mut ring = Ring::new(CAPACITY);
    let mut host = Host::new();

    // Written through the field rather than by finding a byte that holds its value. This test
    // used to search for the last `1` in the record, which its own comment worried about -- and it
    // was right: `packed` is a later field that is also 1 on the ordinary path, so the search
    // moved onto it and the test went on compiling while testing nothing.
    let mut one = tessella_capture_abi::envelope::TextureUpdate::from_bytes(&texture(
        1,
        &[rect(0, 0, 1, 1)],
        4,
    ))
    .expect("reads");
    assert_eq!(one.rect_count, 1, "the fixture wrote one rect");
    one.rect_count = 9;
    let bytes = one.as_bytes().to_vec();

    {
        let (producer, _) = ring.split();
        producer
            .write(EnvelopeKind::TextureUpdate, &bytes, &[0; 4])
            .expect("room");
    }
    let progress = host.read(ring.consumer());

    assert_eq!(progress.malformed, 1);
    assert!(
        host.uploads().work().is_empty(),
        "nothing was taken from it"
    );
}

/// A span claiming more bytes than the payload holds is refused.
#[test]
fn a_span_past_the_payload_is_malformed() {
    let mut ring = Ring::new(CAPACITY);
    let mut host = Host::new();

    {
        let (producer, _) = ring.split();
        producer
            .write(
                EnvelopeKind::UboUpdate,
                uniforms(0, 0, 0, 64).as_bytes(),
                &[0; 8],
            )
            .expect("room");
    }
    let progress = host.read(ring.consumer());

    assert_eq!(
        progress.malformed, 1,
        "sixty-four bytes claimed, eight sent"
    );
    assert!(host.uploads().work().is_empty());
}

/// Work accumulates until the backend says it is done, and the buffer is then reused.
#[test]
fn the_buffer_is_cleared_by_the_backend_and_reused() {
    let mut ring = Ring::new(CAPACITY);
    let mut host = Host::new();

    for _ in 0..3 {
        {
            let (producer, _) = ring.split();
            producer
                .write(
                    EnvelopeKind::UboUpdate,
                    uniforms(0, 0, 0, 16).as_bytes(),
                    &[1; 16],
                )
                .expect("room");
        }
        host.read(ring.consumer());
    }
    assert_eq!(host.uploads().work().len(), 3, "three reads, three pieces");
    assert_eq!(host.uploads().held(), 48);

    host.uploads_done();
    assert!(host.uploads().work().is_empty());
    assert_eq!(host.uploads().held(), 0, "and the bytes with them");

    // And it takes more afterwards, into the same allocation.
    {
        let (producer, _) = ring.split();
        producer
            .write(
                EnvelopeKind::UboUpdate,
                uniforms(0, 0, 0, 16).as_bytes(),
                &[2; 16],
            )
            .expect("room");
    }
    host.read(ring.consumer());
    assert_eq!(host.uploads().held(), 16);
}

/// Two updates to one slot both appear, in order.
///
/// Latest-wins is the producer's contract and a backend writing these in order lands on the later
/// one. Collapsing them here would be this crate deciding something the ABI already decided, and
/// would lose the count a consumer watching its own upload traffic wants.
#[test]
fn two_updates_to_one_slot_both_appear_in_order() {
    let mut ring = Ring::new(CAPACITY);
    let mut host = Host::new();

    {
        let (producer, _) = ring.split();
        for fill in [0xAA_u8, 0xBB] {
            producer
                .write(
                    EnvelopeKind::UboUpdate,
                    uniforms(0, 1, 2, 4).as_bytes(),
                    &[fill; 4],
                )
                .expect("room");
        }
    }
    host.read(ring.consumer());

    let work = host.uploads().work();
    assert_eq!(work.len(), 2);
    let fills: Vec<u8> = work
        .iter()
        .map(|piece| {
            let Upload::Uniforms { bytes, .. } = piece else {
                panic!("uniforms");
            };
            host.uploads().bytes(bytes).expect("held")[0]
        })
        .collect();
    assert_eq!(fills, [0xAA, 0xBB], "in arrival order, latest last");
}

/// An empty payload is legal and holds nothing.
#[test]
fn an_empty_update_is_legal() {
    let mut ring = Ring::new(CAPACITY);
    let mut host = Host::new();

    {
        let (producer, _) = ring.split();
        producer
            .write(
                EnvelopeKind::UboUpdate,
                uniforms(0, 0, 0, 0).as_bytes(),
                &[],
            )
            .expect("room");
    }
    let progress = host.read(ring.consumer());

    assert_eq!(progress.malformed, 0);
    assert_eq!(host.uploads().work().len(), 1);
    assert_eq!(host.uploads().held(), 0);
}

/// A pixel format this consumer does not know is malformed, not passed on.
///
/// The backend picks an image format from it. Passing an unknown discriminant through would have
/// it choose from a number it cannot interpret, on a texture it would then sample.
#[test]
fn an_unknown_pixel_format_is_malformed() {
    let mut one = tessella_capture_abi::envelope::TextureUpdate::from_bytes(&texture(
        1,
        &[rect(0, 0, 1, 1)],
        4,
    ))
    .expect("reads");
    one.format = 200;

    let mut ring = Ring::new(CAPACITY);
    let mut host = Host::new();
    {
        let (producer, _) = ring.split();
        producer
            .write(EnvelopeKind::TextureUpdate, one.as_bytes(), &[0; 4])
            .expect("room");
    }
    assert_eq!(host.read(ring.consumer()).malformed, 1);
    assert!(host.uploads().work().is_empty());
}

/// A channel type this consumer does not know is malformed too.
///
/// The other half of mbgl's two-part format, and the half that decides how many bytes a channel
/// is. A guess here is a stride.
#[test]
fn an_unknown_channel_type_is_malformed() {
    let mut one = tessella_capture_abi::envelope::TextureUpdate::from_bytes(&texture(
        1,
        &[rect(0, 0, 1, 1)],
        4,
    ))
    .expect("reads");
    one.channel_type = 7;

    let mut ring = Ring::new(CAPACITY);
    let mut host = Host::new();
    {
        let (producer, _) = ring.split();
        producer
            .write(EnvelopeKind::TextureUpdate, one.as_bytes(), &[0; 4])
            .expect("room");
    }
    assert_eq!(host.read(ring.consumer()).malformed, 1);
    assert!(host.uploads().work().is_empty());
}

/// `packed` is zero or one, and anything else is malformed.
///
/// It was taken from the record's padding, so a producer that has never heard of it writes zero.
/// A value that is neither is a producer this consumer does not understand, and reading it as
/// `!= 0` would guess at the payload's shape.
#[test]
fn a_packed_flag_that_is_neither_is_malformed() {
    let mut one = tessella_capture_abi::envelope::TextureUpdate::from_bytes(&texture(
        1,
        &[rect(0, 0, 1, 1)],
        4,
    ))
    .expect("reads");
    one.packed = 2;

    let mut ring = Ring::new(CAPACITY);
    let mut host = Host::new();
    {
        let (producer, _) = ring.split();
        producer
            .write(EnvelopeKind::TextureUpdate, one.as_bytes(), &[0; 4])
            .expect("room");
    }
    assert_eq!(host.read(ring.consumer()).malformed, 1);
    assert!(host.uploads().work().is_empty());
}

/// A payload shorter than the rects claim is malformed rather than uploaded in part.
///
/// The consumer's half of the guard the producer's `fits` test is the first half of. One texel of
/// RGBA is four bytes and the rect names one texel, so three bytes cannot describe it.
#[test]
fn a_payload_shorter_than_the_rects_is_malformed() {
    let mut ring = Ring::new(CAPACITY);
    let mut host = Host::new();
    {
        let (producer, _) = ring.split();
        producer
            .write(
                EnvelopeKind::TextureUpdate,
                &texture(1, &[rect(0, 0, 1, 1)], 3),
                &[0; 3],
            )
            .expect("room");
    }
    assert_eq!(host.read(ring.consumer()).malformed, 1);
    assert!(host.uploads().work().is_empty());
}

/// The carried flag and channel type reach the work list, and `rows` agrees with them.
///
/// The end-to-end form: a packed record decoded by `Host` must produce the packed layout, because
/// a backend reads the layout and never the flag.
#[test]
fn a_packed_record_yields_the_packed_layout() {
    let two = [rect(0, 0, 2, 2), rect(8, 8, 4, 4)];
    let mut ring = Ring::new(CAPACITY);
    let mut host = Host::new();
    {
        let (producer, _) = ring.split();
        producer
            .write(EnvelopeKind::TextureUpdate, &texture(5, &two, 80), &[7; 80])
            .expect("room");
    }
    host.read(ring.consumer());

    let work = &host.uploads().work()[0];
    let Upload::Texture { shape, .. } = work else {
        panic!("a texture update");
    };
    assert!(
        shape.packed,
        "the record said packed and the work list must say so"
    );
    assert_eq!(shape.channel, TextureChannelDataType::UnsignedByte);

    let rows = work.rows().expect("a texture").expect("a layout");
    assert_eq!(
        rows,
        [
            tessella_consume::upload::Rows { at: 0, stride: 8 },
            tessella_consume::upload::Rows { at: 16, stride: 16 },
        ],
        "the second region is read from byte 16, not from byte 2080"
    );
}

/// A `Float` record's channel type reaches the work list, and changes the stride.
///
/// The color relief's elevation stops: `RGBA` and `Float` together, which is the pair
/// `texture::whole_float` sends and the reason the channel type is carried at all. Every other
/// fixture here is `UnsignedByte`, so without this one a consumer that hardcoded the common case
/// would pass the whole file.
#[test]
fn a_float_record_carries_its_channel_type() {
    // Two by two RGBA floats, whole: 2 * 2 * 4 channels * 4 bytes.
    let update = tessella_capture_abi::envelope::TextureUpdate {
        texture: TextureId(9),
        size: Extent {
            width: 2,
            height: 2,
        },
        rects: [Rect16::default(); 4],
        pixels: Span {
            offset: 0,
            count: 64,
        },
        format: TexturePixelType::RGBA as u8,
        rect_count: 0,
        channel_type: TextureChannelDataType::Float as u8,
        packed: 0,
        _pad: [0; 4],
    };

    let mut ring = Ring::new(CAPACITY);
    let mut host = Host::new();
    {
        let (producer, _) = ring.split();
        producer
            .write(EnvelopeKind::TextureUpdate, update.as_bytes(), &[0; 64])
            .expect("room");
    }
    host.read(ring.consumer());

    let work = &host.uploads().work()[0];
    let Upload::Texture { shape, .. } = work else {
        panic!("a texture update");
    };
    assert_eq!(
        shape.channel,
        TextureChannelDataType::Float,
        "a byte channel type here is a ramp quantized to forty-meter steps"
    );

    let rows = work.rows().expect("a texture").expect("a layout");
    assert_eq!(
        rows,
        [tessella_consume::upload::Rows { at: 0, stride: 32 }],
        "two RGBA float texels a row is 32 bytes, not 8"
    );
}

/// An `Alpha` record's format reaches the work list, and changes the stride.
///
/// The glyph atlas, which is `Alpha` and packed -- the commonest texture of this shape on the
/// wire. Every other fixture here is `RGBA`, so without this one a consumer that hardcoded the
/// four-channel case would pass the whole file while reading every glyph at four times its width.
#[test]
fn an_alpha_record_carries_its_format() {
    let two = [rect(0, 0, 2, 2), rect(4, 4, 3, 3)];
    let mut filled = [Rect16::default(); 4];
    filled[..two.len()].copy_from_slice(&two);
    let update = tessella_capture_abi::envelope::TextureUpdate {
        texture: TextureId(11),
        size: Extent {
            width: 16,
            height: 16,
        },
        rects: filled,
        pixels: Span {
            offset: 0,
            // (2*2 + 3*3) single-byte texels.
            count: 13,
        },
        format: TexturePixelType::Alpha as u8,
        rect_count: 2,
        channel_type: TextureChannelDataType::UnsignedByte as u8,
        packed: 1,
        _pad: [0; 4],
    };

    let mut ring = Ring::new(CAPACITY);
    let mut host = Host::new();
    {
        let (producer, _) = ring.split();
        producer
            .write(EnvelopeKind::TextureUpdate, update.as_bytes(), &[0; 13])
            .expect("room");
    }
    host.read(ring.consumer());

    let work = &host.uploads().work()[0];
    let Upload::Texture { shape, .. } = work else {
        panic!("a texture update");
    };
    assert_eq!(shape.format, TexturePixelType::Alpha);

    let rows = work.rows().expect("a texture").expect("a layout");
    assert_eq!(
        rows,
        [
            tessella_consume::upload::Rows { at: 0, stride: 2 },
            tessella_consume::upload::Rows { at: 4, stride: 3 },
        ],
        "one byte a texel, so the second glyph starts at byte 4 rather than byte 16"
    );
}
