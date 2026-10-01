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
use tessella_capture_abi::{EnvelopeKind, TexturePixelType};
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

/// A texture update over `rects`, whose pixels are `len` bytes of payload.
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
        channel_type: 0,
        packed: 0,
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
            .write(EnvelopeKind::TextureUpdate, &texture(5, &two, 16), &[7; 16])
            .expect("room");
    }
    host.read(ring.consumer());

    let Upload::Texture {
        texture: id,
        rects,
        bytes,
        format,
        ..
    } = &host.uploads().work()[0]
    else {
        panic!("a texture update");
    };
    assert_eq!(*id, TextureId(5));
    assert_eq!(rects.as_slice(), two.as_slice(), "both rects, and no more");
    assert_eq!(*format, TexturePixelType::RGBA as u8);
    assert_eq!(host.uploads().bytes(bytes).map(<[u8]>::len), Some(16));
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

    let mut bytes = texture(1, &[rect(0, 0, 1, 1)], 4);
    // Find `rect_count` by the value the fixture gave it rather than by counting backwards from
    // the end: the struct has gained a tail field before and an offset computed here would go on
    // compiling and stop testing anything.
    let one = tessella_capture_abi::envelope::TextureUpdate::from_bytes(&bytes).expect("reads");
    assert_eq!(one.rect_count, 1, "the fixture wrote one rect");
    let at = bytes
        .iter()
        .rposition(|byte| *byte == 1)
        .expect("the rect count");
    bytes[at] = 9;

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
