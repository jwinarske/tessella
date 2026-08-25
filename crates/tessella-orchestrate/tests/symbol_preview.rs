//! A picture of a frame, rasterised from the wire format.
//!
//! `#[ignore]`d: it writes a file and asserts almost nothing. It exists because every other test
//! here checks a number, and a map is a thing you look at — shaping that is subtly wrong reads
//! as correct arithmetic and wrong-looking text, and no assertion catches that.
//!
//! ```sh
//! cargo test -p tessella-orchestrate --test symbol_preview -- --ignored --nocapture
//! ```
//!
//! # It draws from the vertex buffer, not from the shaper
//!
//! The corners and texture coordinates are decoded back out of the packed vertices, exactly as a
//! shader would: the anchor from the first two shorts, the corner offset from the next two at a
//! thirty-second of a pixel, the texel from the data attribute. So a packing error shows up as a
//! wrong picture rather than being bypassed by drawing from the shaper's own output.

use std::collections::BTreeSet;

use tessella_glyph::atlas::{Atlas, Rect};
use tessella_glyph::pbf::{self, Glyph, Metrics, Range};
use tessella_layout::symbol;
use tessella_layout::symbol_bucket::{Glyphs, Label, SymbolBuffers, SymbolOptions, build_symbols};
use tessella_orchestrate::symbols::{FrameLabel, FrameOptions, ViewSymbols};
use tessella_place::feature::Padding;
use tessella_source::mvt::{GeomType, Tile};

const TILE: &[u8] = include_bytes!("../../../tests/live-fixtures/world_z7-5-16-11.mvt");
const GLYPHS: &[u8] = include_bytes!("../../../tests/glyph-fixtures/TestFont/0-255.pbf");

const CANVAS: u32 = 700;
/// The tile's extent, and how much of it the canvas shows.
const EXTENT: f32 = 8192.0;

struct Font {
    glyphs: Vec<Glyph>,
    atlas: Atlas,
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

/// The place names in the fixture, with their tile anchors.
fn place_names() -> Vec<(String, (f32, f32))> {
    let tile = Tile::decode(TILE).expect("the fixture decodes");
    let layer = tile
        .layers
        .iter()
        .find(|layer| layer.name == "places")
        .expect("a places layer");
    let style: tessella_style::Layer = serde_json::from_str(
        r#"{"id": "l", "type": "symbol", "source": "v", "source-layer": "places",
            "layout": {"text-field": "{name}", "text-font": ["TestFont"]}}"#,
    )
    .expect("a layer");

    let mut out = Vec::new();
    for feature in layer.features() {
        if feature.geom_type() != GeomType::Point {
            continue;
        }
        let Some(label) = symbol::label(&style, 5.0, &feature) else {
            continue;
        };
        let anchor = feature
            .rings()
            .next()
            .and_then(|ring| ring.first().copied())
            .expect("a point");
        #[allow(clippy::cast_precision_loss)]
        out.push((label.text, (anchor[0] as f32, anchor[1] as f32)));
    }
    out
}

/// An 8-bit greyscale PNG, written without a compression library.
///
/// zlib permits *stored* blocks — uncompressed, length-prefixed — so a valid PNG needs only a
/// CRC and an Adler checksum. A dependency to make a debug picture would be a dependency in the
/// shipping graph.
fn write_png(path: &str, width: u32, height: u32, grey: &[u8]) {
    fn crc32(bytes: &[u8]) -> u32 {
        let mut table = [0u32; 256];
        for (index, entry) in table.iter_mut().enumerate() {
            let mut value = index as u32;
            for _ in 0..8 {
                value = if value & 1 == 1 {
                    0xedb8_8320 ^ (value >> 1)
                } else {
                    value >> 1
                };
            }
            *entry = value;
        }
        let mut crc = 0xffff_ffffu32;
        for byte in bytes {
            crc = table[((crc ^ u32::from(*byte)) & 0xff) as usize] ^ (crc >> 8);
        }
        crc ^ 0xffff_ffff
    }

    fn chunk(out: &mut Vec<u8>, kind: &[u8; 4], body: &[u8]) {
        #[allow(clippy::cast_possible_truncation)]
        out.extend_from_slice(&(body.len() as u32).to_be_bytes());
        let mut framed = kind.to_vec();
        framed.extend_from_slice(body);
        out.extend_from_slice(&framed);
        out.extend_from_slice(&crc32(&framed).to_be_bytes());
    }

    // Each scanline is prefixed with its filter type, which is zero: none.
    let mut raw = Vec::with_capacity((width as usize + 1) * height as usize);
    for row in 0..height as usize {
        raw.push(0);
        raw.extend_from_slice(&grey[row * width as usize..(row + 1) * width as usize]);
    }

    let mut zlib = vec![0x78, 0x01];
    for (index, block) in raw.chunks(65_535).enumerate() {
        let last = u8::from((index + 1) * 65_535 >= raw.len());
        zlib.push(last);
        #[allow(clippy::cast_possible_truncation)]
        let len = block.len() as u16;
        zlib.extend_from_slice(&len.to_le_bytes());
        zlib.extend_from_slice(&(!len).to_le_bytes());
        zlib.extend_from_slice(block);
    }
    let (mut a, mut b) = (1u32, 0u32);
    for byte in &raw {
        a = (a + u32::from(*byte)) % 65_521;
        b = (b + a) % 65_521;
    }
    zlib.extend_from_slice(&((b << 16) | a).to_be_bytes());

    let mut png = vec![0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a];
    let mut header = Vec::new();
    header.extend_from_slice(&width.to_be_bytes());
    header.extend_from_slice(&height.to_be_bytes());
    header.extend_from_slice(&[8, 0, 0, 0, 0]); // 8-bit greyscale
    chunk(&mut png, b"IHDR", &header);
    chunk(&mut png, b"IDAT", &zlib);
    chunk(&mut png, b"IEND", &[]);

    std::fs::write(path, png).expect("writes the picture");
}

/// Draws a frame of the z5 tile's place labels.
#[test]
#[ignore]
fn draw_a_frame() {
    let glyphs = pbf::parse(
        Range {
            first: 0,
            last: 255,
        },
        GLYPHS,
    )
    .expect("the range parses");
    let names = place_names();

    // Pack every glyph the labels use.
    let used: BTreeSet<u32> = names
        .iter()
        .flat_map(|(text, _)| text.chars().map(|character| character as u32))
        .collect();
    let mut atlas = Atlas::new(512, 512);
    for glyph in &glyphs {
        if used.contains(&glyph.id) {
            atlas.add(glyph.id, glyph);
        }
    }
    let font = Font { glyphs, atlas };

    let labels: Vec<Label> = names
        .iter()
        .map(|(text, anchor)| Label {
            text: text.clone(),
            anchor: *anchor,
        })
        .collect();
    let (buffers, laid) = build_symbols(&labels, &font, &SymbolOptions::default());

    let frame_labels: Vec<FrameLabel> = laid
        .into_iter()
        .enumerate()
        .map(|(index, laid_out)| FrameLabel {
            cross_tile_id: index as u32 + 1,
            laid_out,
        })
        .collect();

    #[allow(clippy::cast_precision_loss)]
    let project = |anchor: (f32, f32)| {
        (
            anchor.0 * CANVAS as f32 / EXTENT,
            anchor.1 * CANVAS as f32 / EXTENT,
        )
    };

    let mut view = ViewSymbols::new();
    let result = view.frame(
        &frame_labels,
        project,
        &FrameOptions {
            padding: Padding::uniform(2.0),
            increment: 1.0,
            #[allow(clippy::cast_precision_loss)]
            viewport: (CANVAS as f32, CANVAS as f32),
            ..FrameOptions::default()
        },
    );

    // Draw only what placement kept, which is the whole point of looking.
    let placed: BTreeSet<u32> = result
        .placed
        .iter()
        .filter(|symbol| symbol.text)
        .map(|symbol| symbol.cross_tile_id)
        .collect();

    let mut canvas = vec![24u8; (CANVAS * CANVAS) as usize];
    let mut drawn = 0usize;
    for label in &frame_labels {
        if !placed.contains(&label.cross_tile_id) {
            continue;
        }
        drawn += 1;
        let (anchor_x, anchor_y) = project(label.laid_out.anchor);
        blit(
            &mut canvas,
            &buffers,
            &font.atlas,
            label.laid_out.vertices.clone(),
            (anchor_x, anchor_y),
        );
    }

    let path =
        std::env::var("TESSELLA_PREVIEW").unwrap_or_else(|_| "/tmp/tessella-frame.png".to_string());
    write_png(&path, CANVAS, CANVAS, &canvas);

    println!(
        "\n  {} place labels, {drawn} placed after collision, written to {path}\n",
        frame_labels.len()
    );
    assert!(drawn > 0);
}

/// Draws one label's quads, decoding the corners and texels back out of the packed vertices.
fn blit(
    canvas: &mut [u8],
    buffers: &SymbolBuffers,
    atlas: &Atlas,
    vertices: core::ops::Range<usize>,
    anchor: (f32, f32),
) {
    let (atlas_width, _) = atlas.size();
    for quad in vertices.step_by(4).take(usize::MAX) {
        if quad + 3 >= buffers.vertices.len() {
            break;
        }
        let corner = |index: usize| {
            let vertex = &buffers.vertices[quad + index];
            (
                f32::from(vertex.pos_offset[2]) / 32.0,
                f32::from(vertex.pos_offset[3]) / 32.0,
            )
        };
        let texel = |index: usize| {
            let vertex = &buffers.vertices[quad + index];
            (vertex.data[0], vertex.data[1])
        };

        let (left, top) = corner(0);
        let (right, bottom) = corner(3);
        let (tex_left, tex_top) = texel(0);
        let (tex_right, tex_bottom) = texel(3);
        if right <= left || bottom <= top {
            continue;
        }

        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        for y in (anchor.1 + top).floor().max(0.0) as u32
            ..((anchor.1 + bottom).ceil().max(0.0) as u32).min(CANVAS)
        {
            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
            for x in (anchor.0 + left).floor().max(0.0) as u32
                ..((anchor.0 + right).ceil().max(0.0) as u32).min(CANVAS)
            {
                // Where in the glyph's rectangle this pixel falls.
                let u = (f32::from(x as u16) - (anchor.0 + left)) / (right - left);
                let v = (f32::from(y as u16) - (anchor.1 + top)) / (bottom - top);
                if !(0.0..1.0).contains(&u) || !(0.0..1.0).contains(&v) {
                    continue;
                }
                #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
                let sx = (f32::from(tex_left) + u * f32::from(tex_right - tex_left)) as u32;
                #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
                let sy = (f32::from(tex_top) + v * f32::from(tex_bottom - tex_top)) as u32;
                let distance = atlas.pixels()[(sy * atlas_width + sx) as usize];

                // The distance field encodes the edge at 128 and falls off either side. A
                // narrow ramp around it is what makes the letter's edge smooth rather than
                // stepped, which is the entire reason a distance field is used at all.
                let alpha = f32::from(distance).mul_add(1.0 / 16.0, -(128.0 / 16.0) + 0.5);
                let alpha = alpha.clamp(0.0, 1.0);
                if alpha <= 0.0 {
                    continue;
                }
                let slot = &mut canvas[(y * CANVAS + x) as usize];
                #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
                let lit = alpha.mul_add(255.0 - f32::from(*slot), f32::from(*slot)) as u8;
                *slot = (*slot).max(lit);
            }
        }
    }
}
