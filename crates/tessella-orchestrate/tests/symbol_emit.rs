//! Symbols on the wire.
//!
//! The last link: a laid-out symbol layer becomes a `GeometryAdd` a consumer can draw from. What
//! it has to get right is the five attribute descriptors — which buffer each reads, at what
//! offset and stride — because a consumer believes them. An attribute pointed at the wrong slab
//! draws whatever is there, and nothing in the stream says it was wrong.
//!
//! The expected layout is not invented here: it is what `tests/golden/symbol_style.dump` shows
//! mbgl emitting, and `symbol_parity` checks the golden still says so.

use tessella_capture_abi::envelope::{GeometryId, TextureId};
use tessella_capture_abi::ring::Ring;
use tessella_capture_abi::{AttributeDataType, BuiltIn, EnvelopeKind};
use tessella_glyph::atlas::{Atlas, Rect};
use tessella_glyph::pbf::{self, Glyph, Metrics, Range};
use tessella_layout::symbol_bucket::{Glyphs, Label, SymbolOptions, build_symbols};
use tessella_orchestrate::emit::{self, SlabArena};

const GLYPHS: &[u8] = include_bytes!("../../../tests/glyph-fixtures/TestFont/0-255.pbf");

/// The glyph atlas a symbol drawable samples. Any id will do here — what is under test is the
/// slot it lands in, which comes from the shader's generated table rather than from this.
const ATLAS: TextureId = TextureId(3);

struct Font {
    glyphs: Vec<Glyph>,
    atlas: Atlas,
}

impl Font {
    fn new(pack: &str) -> Self {
        let glyphs = pbf::parse(
            Range {
                first: 0,
                last: 255,
            },
            GLYPHS,
        )
        .expect("the range parses");
        let mut atlas = Atlas::new(512, 512);
        for glyph in &glyphs {
            if pack.chars().any(|character| character as u32 == glyph.id) {
                atlas.add(glyph.id, glyph);
            }
        }
        Self { glyphs, atlas }
    }
}

impl Glyphs for Font {
    fn metrics(&self, codepoint: u32) -> Option<(Metrics, bool)> {
        let glyph = self.glyphs.iter().find(|glyph| glyph.id == codepoint)?;
        Some((glyph.metrics, glyph.bitmap_size().is_some()))
    }
    fn rect(&self, codepoint: u32) -> Option<Rect> {
        self.atlas.get(codepoint)
    }
}

fn labelled(text: &str) -> (SlabArena, emit::Encoded, usize) {
    let font = Font::new(text);
    let (buffers, _) = build_symbols(
        &[Label {
            pending: 0,
            sections: vec![tessella_layout::symbol::Section {
                text: text.to_string(),
                scale: 1.0,
            }],
            text: text.to_string(),
            anchor: (1000.0, 2000.0),
        }],
        &font,
        &SymbolOptions::default(),
    );
    let glyphs = buffers.glyphs();

    let mut arena = SlabArena::new();
    let encoded = emit::encode_symbol(&mut arena, GeometryId(7), &buffers, 0, true, ATLAS);
    arena.seal();
    (arena, encoded, glyphs)
}

/// The record names the symbol shader and counts every vertex.
#[test]
fn the_record_describes_the_symbol_geometry() {
    let (_, encoded, glyphs) = labelled("Alpha");

    assert_eq!(encoded.record.geometry, GeometryId(7));
    assert_eq!(encoded.record.vertex_count, glyphs as u32 * 4);
    assert_eq!(
        encoded.record.builtin_shader,
        BuiltIn::SymbolSDFShader as i32
    );
    assert_eq!(encoded.record.vertex_type, AttributeDataType::Short4 as u8);
}

/// An icon layer names the icon shader instead.
#[test]
fn a_non_sdf_symbol_names_the_icon_shader() {
    let font = Font::new("Alpha");
    let (buffers, _) = build_symbols(
        &[Label {
            pending: 0,
            sections: vec![tessella_layout::symbol::Section {
                text: "Alpha".to_string(),
                scale: 1.0,
            }],
            text: "Alpha".to_string(),
            anchor: (0.0, 0.0),
        }],
        &font,
        &SymbolOptions::default(),
    );
    let mut arena = SlabArena::new();
    let encoded = emit::encode_symbol(&mut arena, GeometryId(1), &buffers, 0, false, ATLAS);
    assert_eq!(
        encoded.record.builtin_shader,
        BuiltIn::SymbolIconShader as i32
    );
}

/// Five attributes, laid out the way the golden shows mbgl laying them out.
///
/// Three sharing one interleaved buffer at stride 24, then two with buffers of their own. A
/// consumer reads these descriptors literally, so an offset or a stride that is off draws
/// nonsense with nothing in the stream to say so.
#[test]
fn the_five_attributes_match_the_capture() {
    let (arena, encoded, _) = labelled("Alpha");
    let attrs = encoded.attributes();

    assert_eq!(attrs.len(), 5);

    // The interleaved three.
    for (index, (offset, data_type)) in [
        (0u32, AttributeDataType::Short4),
        (8, AttributeDataType::UShort4),
        (16, AttributeDataType::Short4),
    ]
    .into_iter()
    .enumerate()
    {
        let attr = &attrs[index];
        assert_eq!(attr.attr_id, index as u32, "attribute {index}");
        assert_eq!(attr.offset, offset);
        assert_eq!(attr.stride, 24, "the interleaved stride");
        assert_eq!(attr.data_type, data_type as u8);
        assert_eq!(attr.declared_data_type, data_type as u8);
    }

    // All three read the same slab, which is what interleaving means: three separate slabs
    // would describe the same bytes three times and cost three uploads.
    assert_eq!(attrs[0].source, attrs[1].source);
    assert_eq!(attrs[1].source, attrs[2].source);

    // And the two per-frame buffers are their own, tightly packed.
    assert_eq!((attrs[3].offset, attrs[3].stride), (0, 12));
    assert_eq!(attrs[3].data_type, AttributeDataType::Float3 as u8);
    assert_eq!((attrs[4].offset, attrs[4].stride), (0, 4));
    assert_eq!(attrs[4].data_type, AttributeDataType::Float as u8);

    assert_ne!(attrs[3].source, attrs[0].source, "a buffer of its own");
    assert_ne!(attrs[4].source, attrs[3].source);
    let _ = arena;
}

/// Every attribute points at a slab holding the right number of bytes.
///
/// The check that a descriptor and its data agree. A stride and a buffer that disagree is the
/// failure this rules out: the consumer reads past the end of the last vertex, or stops short.
#[test]
fn each_attribute_reads_a_slab_that_fits_it() {
    let (arena, encoded, glyphs) = labelled("Bravo");
    let vertices = glyphs * 4;
    let attrs = encoded.attributes();

    for attr in attrs {
        let bytes = arena.resolve(attr.source).expect("a slab");
        assert_eq!(
            bytes.len(),
            vertices * attr.stride as usize,
            "attribute {} has {} bytes for {vertices} vertices at stride {}",
            attr.attr_id,
            bytes.len(),
            attr.stride
        );
        // And the last vertex's field is inside it.
        assert!(
            attr.offset as usize + attr.stride as usize * (vertices - 1) < bytes.len(),
            "attribute {} reads past its slab",
            attr.attr_id
        );
    }
}

/// The index buffer holds two triangles per glyph.
#[test]
fn the_index_buffer_holds_two_triangles_a_glyph() {
    let (arena, encoded, glyphs) = labelled("Charlie");
    let bytes = arena.resolve(encoded.record.indexes).expect("a slab");
    assert_eq!(bytes.len(), glyphs * 6 * 2, "six u16 indices a glyph");
}

/// One segment covering the whole buffer.
///
/// The capture shows `segs=1` for both its symbol drawables, and a layer's labels share one
/// buffer. A second segment only appears past what a `u16` index reaches, which the bucket
/// refuses rather than wrapping into.
#[test]
fn the_geometry_is_one_segment() {
    let (_, encoded, glyphs) = labelled("Alpha");
    let segments = encoded.segments();

    assert_eq!(segments.len(), 1);
    assert_eq!(segments[0].vertex_offset, 0);
    assert_eq!(segments[0].index_offset, 0);
    assert_eq!(segments[0].vertex_length, glyphs as u32 * 4);
    assert_eq!(segments[0].index_length, glyphs as u32 * 6);
}

/// It goes on the ring as a `GeometryAdd`.
#[test]
fn it_reaches_the_ring() {
    let (_, encoded, _) = labelled("Alpha");
    let mut ring = Ring::new(1 << 16);
    let (producer, consumer) = ring.split();

    emit::write(producer, &encoded).expect("the ring takes it");

    let record = consumer.peek().expect("a record");
    assert_eq!(record.kind, EnvelopeKind::GeometryAdd);
}

/// A label with nothing drawable produces an empty buffer rather than a malformed one.
#[test]
fn an_empty_layer_encodes_to_nothing() {
    let font = Font::new("");
    let (buffers, _) = build_symbols(
        &[Label {
            pending: 0,
            sections: vec![tessella_layout::symbol::Section {
                text: "Alpha".to_string(),
                scale: 1.0,
            }],
            text: "Alpha".to_string(),
            anchor: (0.0, 0.0),
        }],
        &font,
        &SymbolOptions::default(),
    );
    assert!(buffers.is_empty());

    let mut arena = SlabArena::new();
    let encoded = emit::encode_symbol(&mut arena, GeometryId(1), &buffers, 0, true, ATLAS);
    assert_eq!(encoded.record.vertex_count, 0);
    assert_eq!(encoded.segments()[0].vertex_length, 0);
}
