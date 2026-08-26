//! The `EXT_meshopt_compression` bitstream, against meshoptimizer's own test vectors.
//!
//! The vectors are `demo/tests.cpp`'s, byte for byte. They are the right oracle because the
//! extension's specification does not describe the bitstream at all — it defers to
//! meshoptimizer, so the reference implementation *is* the specification and its tests are what
//! conformance means.
//!
//! Both decoders were additionally cross-checked against an independent transcription over the
//! real store: 564 vertex streams and 141 index streams, 116,740,428 bytes and 3,600,646
//! triangles, byte-identical throughout. That check is not committed — it needs a 26 GB store and
//! a second implementation — so what is here is the vectors plus the properties they cannot
//! reach.

#![cfg(feature = "gltf")]

use tessella_source::meshopt::{
    MeshoptError, decode_filter_exponential, decode_filter_octahedral8, decode_index_buffer,
    decode_vertex_buffer,
};

/// meshoptimizer's `kVertexDataV0`.
const VERTEX_DATA_V0: [u8; 85] = [
    0xa0, 0x01, 0x3f, 0x00, 0x00, 0x00, 0x58, 0x57, 0x58, 0x01, 0x26, 0x00, 0x00, 0x00, 0x01, 0x0c,
    0x00, 0x00, 0x00, 0x58, 0x01, 0x08, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x01, 0x3f, 0x00,
    0x00, 0x00, 0x17, 0x18, 0x17, 0x01, 0x26, 0x00, 0x00, 0x00, 0x01, 0x0c, 0x00, 0x00, 0x00, 0x17,
    0x01, 0x08, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    0x00, 0x00, 0x00, 0x00, 0x00,
];

/// meshoptimizer's `kVertexBuffer`, as the twelve bytes its `PV` occupies.
///
/// `struct PV { unsigned short px, py, pz; unsigned char nu, nv; unsigned short tx, ty; }` — six
/// shorts and two bytes, laid out little-endian with the two bytes between the third and fourth
/// short. Written out rather than derived so the fixture is the C struct's memory and not a
/// re-derivation of it.
fn vertex_buffer() -> Vec<u8> {
    let pv = |px: u16, py: u16, pz: u16, nu: u8, nv: u8, tx: u16, ty: u16| -> [u8; 12] {
        let mut out = [0u8; 12];
        out[0..2].copy_from_slice(&px.to_le_bytes());
        out[2..4].copy_from_slice(&py.to_le_bytes());
        out[4..6].copy_from_slice(&pz.to_le_bytes());
        out[6] = nu;
        out[7] = nv;
        out[8..10].copy_from_slice(&tx.to_le_bytes());
        out[10..12].copy_from_slice(&ty.to_le_bytes());
        out
    };
    [
        pv(0, 0, 0, 0, 0, 0, 0),
        pv(300, 0, 0, 0, 0, 500, 0),
        pv(0, 300, 0, 0, 0, 0, 500),
        pv(300, 300, 0, 0, 0, 500, 500),
    ]
    .concat()
}

/// meshoptimizer's `kIndexBuffer` and `kIndexDataV0`.
///
/// The comment in the reference is worth keeping: the `4 6 5` triangle is a combo-breaker — it is
/// encoded without rotating, so `next` is *not* bumped to 6 and the following triangle cannot use
/// next-sequencing. That is precisely the case a decoder gets wrong if its FIFO pushes do not
/// match the encoder step for step.
const INDEX_BUFFER: [u32; 12] = [0, 1, 2, 2, 1, 3, 4, 6, 5, 7, 8, 9];
const INDEX_DATA_V0: [u8; 27] = [
    0xe0, 0xf0, 0x10, 0xfe, 0xff, 0xf0, 0x0c, 0xff, 0x02, 0x02, 0x02, 0x00, 0x76, 0x87, 0x56, 0x67,
    0x78, 0xa9, 0x86, 0x65, 0x89, 0x68, 0x98, 0x01, 0x69, 0x00, 0x00,
];

/// meshoptimizer's `kIndexBufferTricky` and `kIndexDataV1`.
///
/// The reference's note: this exercises two features of the v1 format, restarts and `last`. A
/// decoder that took the version byte for decoration passes the v0 vector and fails this one,
/// because v1 moves the escape threshold from 15 to 13.
const INDEX_BUFFER_TRICKY: [u32; 15] = [0, 1, 2, 2, 1, 3, 0, 1, 2, 2, 1, 5, 2, 1, 4];
const INDEX_DATA_V1: [u8; 24] = [
    0xe1, 0xf0, 0x10, 0xfe, 0x1f, 0x3d, 0x00, 0x0a, 0x00, 0x76, 0x87, 0x56, 0x67, 0x78, 0xa9, 0x86,
    0x65, 0x89, 0x68, 0x98, 0x01, 0x69, 0x00, 0x00,
];

/// meshoptimizer's `decodeVertexV0`.
#[test]
fn the_vertex_vector_decodes() {
    let decoded = decode_vertex_buffer(4, 12, &VERTEX_DATA_V0).expect("decodes");
    assert_eq!(decoded, vertex_buffer());
}

/// meshoptimizer's `decodeIndexV0`.
#[test]
fn the_v0_index_vector_decodes() {
    let decoded = decode_index_buffer(INDEX_BUFFER.len(), &INDEX_DATA_V0).expect("decodes");
    assert_eq!(decoded, INDEX_BUFFER);
}

/// meshoptimizer's `decodeIndexV1`, which is the version-sensitive one.
#[test]
fn the_v1_index_vector_decodes() {
    let decoded = decode_index_buffer(INDEX_BUFFER_TRICKY.len(), &INDEX_DATA_V1).expect("decodes");
    assert_eq!(decoded, INDEX_BUFFER_TRICKY);

    // And the version genuinely changes the answer: reading v1 data with the v0 threshold
    // produces something, and it is not this.
    let mut as_v0 = INDEX_DATA_V1;
    as_v0[0] = 0xe0;
    let wrong = decode_index_buffer(INDEX_BUFFER_TRICKY.len(), &as_v0);
    assert!(
        wrong.is_err() || wrong.as_deref() != Ok(&INDEX_BUFFER_TRICKY[..]),
        "the version byte made no difference, so it is not being read"
    );
}

/// The tail is the initial predictor, not padding.
///
/// A meshopt vertex stream ends with one vertex's worth of bytes, and the first block's deltas
/// are taken against it. A stream whose tail is the wrong length decodes every vertex wrong by a
/// constant rather than failing, which is why the size is checked exactly rather than as a
/// minimum.
#[test]
fn a_stream_with_the_wrong_tail_is_refused() {
    let mut short = VERTEX_DATA_V0.to_vec();
    short.pop();
    match decode_vertex_buffer(4, 12, &short) {
        Err(MeshoptError::Tail { actual, expected }) => {
            assert_eq!(expected, 32, "the tail is the greater of 32 and the stride");
            assert_eq!(actual, 31);
        }
        other => panic!("expected a tail error, got {other:?}"),
    }

    let mut long = VERTEX_DATA_V0.to_vec();
    long.push(0);
    assert!(matches!(
        decode_vertex_buffer(4, 12, &long),
        Err(MeshoptError::Tail { .. })
    ));
}

/// A header that is not this codec's, or a version it does not implement.
#[test]
fn a_foreign_header_is_refused() {
    let mut wrong = VERTEX_DATA_V0;
    wrong[0] = 0xb0;
    assert_eq!(
        decode_vertex_buffer(4, 12, &wrong),
        Err(MeshoptError::Header { what: "vertex" })
    );

    // Version 1 of the *vertex* codec does not exist yet, and is refused rather than attempted.
    let mut future = VERTEX_DATA_V0;
    future[0] = 0xa1;
    assert_eq!(
        decode_vertex_buffer(4, 12, &future),
        Err(MeshoptError::Header { what: "vertex" })
    );

    let mut bad_index = INDEX_DATA_V0;
    bad_index[0] = 0xe2;
    assert!(matches!(
        decode_index_buffer(12, &bad_index),
        Err(MeshoptError::Header { .. })
    ));
}

/// A vertex size the codec cannot express is refused before anything is read.
///
/// The codec transposes by byte and encodes each column, so a stride that is not a multiple of
/// four is not a stream it could have produced.
#[test]
fn an_impossible_vertex_size_is_refused() {
    for size in [0usize, 3, 7, 257] {
        assert_eq!(
            decode_vertex_buffer(1, size, &VERTEX_DATA_V0),
            Err(MeshoptError::VertexSize(size)),
            "stride {size}"
        );
    }
}

/// A truncated stream is refused rather than decoding what it can.
#[test]
fn a_truncated_stream_is_refused() {
    for cut in [1usize, 8, 40] {
        assert!(
            decode_vertex_buffer(4, 12, &VERTEX_DATA_V0[..cut]).is_err(),
            "a vertex stream cut at {cut} decoded"
        );
    }
    for cut in [1usize, 10, 20] {
        assert!(
            decode_index_buffer(12, &INDEX_DATA_V0[..cut]).is_err(),
            "an index stream cut at {cut} decoded"
        );
    }
    // An index count that is not whole triangles is not a triangle stream.
    assert!(decode_index_buffer(11, &INDEX_DATA_V0).is_err());
}

/// The exponential filter is `2^e * m`, with the exponent in the top byte.
///
/// What a buildings tile's positions are stored as: a shared exponent per component and an
/// integer mantissa, which keeps coordinates at the precision they were quantised to rather than
/// spending float bits on precision that is not there.
#[test]
fn the_exponential_filter_reconstructs_a_float() {
    // e = 0, m = 1 → 1.0
    let mut data = 1u32.to_le_bytes().to_vec();
    decode_filter_exponential(&mut data);
    assert_eq!(f32::from_le_bytes(data[..4].try_into().unwrap()), 1.0);

    // e = -5 (0xfb in the top byte), m = 3 → 3 / 32
    let mut data = ((0xfbu32 << 24) | 3).to_le_bytes().to_vec();
    decode_filter_exponential(&mut data);
    assert_eq!(
        f32::from_le_bytes(data[..4].try_into().unwrap()),
        3.0 / 32.0
    );

    // A negative mantissa: the low 24 bits are signed.
    let mut data = 0x00ff_ffffu32.to_le_bytes().to_vec();
    decode_filter_exponential(&mut data);
    assert_eq!(f32::from_le_bytes(data[..4].try_into().unwrap()), -1.0);

    // Zero stays zero rather than becoming a denormal.
    let mut data = 0u32.to_le_bytes().to_vec();
    decode_filter_exponential(&mut data);
    assert_eq!(f32::from_le_bytes(data[..4].try_into().unwrap()), 0.0);
}

/// The octahedral filter returns unit vectors.
///
/// A normal costs three bytes instead of twelve because two components are stored and the third
/// is reconstructed. The property that matters downstream is that what comes out is a unit
/// vector — a shader lighting a wall with a non-unit normal gets the wrong brightness, not the
/// wrong direction, which is the kind of error that looks like a material setting.
#[test]
fn the_octahedral_filter_returns_unit_vectors() {
    // A spread of encoded inputs, including the z<0 fold.
    let mut data: Vec<u8> = Vec::new();
    for (x, y) in [
        (0i8, 0i8),
        (127, 0),
        (0, 127),
        (-64, 64),
        (40, -90),
        (-127, -1),
    ] {
        #[allow(clippy::cast_sign_loss)]
        data.extend_from_slice(&[x as u8, y as u8, 127u8, 0]);
    }
    decode_filter_octahedral8(&mut data);

    for quad in data.as_chunks::<4>().0 {
        #[allow(clippy::cast_possible_wrap)]
        let vector = [
            f64::from(quad[0] as i8),
            f64::from(quad[1] as i8),
            f64::from(quad[2] as i8),
        ];
        let length = (vector[0] * vector[0] + vector[1] * vector[1] + vector[2] * vector[2]).sqrt();
        // The components are signed bytes, so a unit vector has a length near 127 and the
        // rounding to integers is what the tolerance allows for.
        assert!(
            (length - 127.0).abs() < 2.0,
            "length {length} for {vector:?} is not a unit normal"
        );
    }
}
