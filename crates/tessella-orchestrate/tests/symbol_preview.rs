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
use tessella_orchestrate::project::PlacedGlyph;
use tessella_orchestrate::symbols::{FrameLabel, FrameOptions, ViewSymbols};
use tessella_place::feature::Padding;
use tessella_source::mvt::{GeomType, Tile};

const TILE: &[u8] = include_bytes!("../../../tests/live-fixtures/world_z7-5-16-11.mvt");
const STREETS: &[u8] = include_bytes!("../../../tests/mvt-fixtures/streets-10-163-395.mvt");
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

/// The atlas, sampled bilinearly and normalised to 0..1.
///
/// A distance field is meant to be interpolated — that is what makes it scale — so sampling it
/// nearest-neighbour throws away most of the precision the encoding exists to carry.
fn sample(pixels: &[u8], width: u32, x: f32, y: f32) -> f32 {
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let (x0, y0) = (x.floor().max(0.0) as u32, y.floor().max(0.0) as u32);
    let (fx, fy) = (x - x.floor(), y - y.floor());
    let at = |px: u32, py: u32| {
        pixels
            .get((py * width + px) as usize)
            .map_or(0.0, |value| f32::from(*value) / 255.0)
    };
    let top = at(x0, y0).mul_add(1.0 - fx, at(x0 + 1, y0) * fx);
    let bottom = at(x0, y0 + 1).mul_add(1.0 - fx, at(x0 + 1, y0 + 1) * fx);
    top.mul_add(1.0 - fy, bottom * fy)
}

/// The shader's `smoothstep`: `t * t * (3 - 2t)`.
///
/// Written plainly. It was written as a `mul_add` to satisfy a lint, which changed it to
/// `t * t * (1 - 2t)` — negative for anything past the halfway point, so the *inside* of every
/// glyph came out with negative alpha and was skipped, leaving only the faint edge ramp. A
/// clippy suggestion is a suggestion about style, and applying one to an expression whose exact
/// shape is the point is how arithmetic quietly changes.
#[allow(clippy::suboptimal_flops)]
fn smoothstep(low: f32, high: f32, value: f32) -> f32 {
    let t = ((value - low) / (high - low)).clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
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
                let sx = f32::from(tex_left) + u * f32::from(tex_right - tex_left);
                let sy = f32::from(tex_top) + v * f32::from(tex_bottom - tex_top);
                let distance = sample(atlas.pixels(), atlas_width, sx, sy);

                // The shader's own constants. The edge sits at (256 - 64) / 256 -- *not* at the
                // midpoint, which is the mistake that makes every letter several pixels too fat
                // and reads as hopelessly blurry text. EDGE_GAMMA is the half-width of the ramp
                // either side of it, and 0.105 is the value at a device pixel ratio of one,
                // which is what drawing a glyph at its native size means.
                const INNER_EDGE: f32 = (256.0 - 64.0) / 256.0;
                const EDGE_GAMMA: f32 = 0.105;
                let alpha = smoothstep(INNER_EDGE - EDGE_GAMMA, INNER_EDGE + EDGE_GAMMA, distance);
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

/// Draws the streets tile's roads with their type printed along them.
///
/// The point of looking at this one is the along-line projection. A line label's glyphs are all
/// at the same anchor in the vertex buffer; what separates them is `glyph_offset`, which the
/// shader walks along the line to turn into a position and a rotation. So this does what the
/// shader does — and if the offset were baked into the corners instead, every road's name would
/// draw as a pile of letters at one point.
#[test]
#[ignore]
fn draw_line_labels() {
    use tessella_layout::symbol_bucket::{LineLabel, LineOptions, build_line_symbols};
    use tessella_orchestrate::project::{LineOffsets, Placement, place_upright};

    let glyphs = pbf::parse(
        Range {
            first: 0,
            last: 255,
        },
        GLYPHS,
    )
    .expect("the range parses");
    let tile = Tile::decode(STREETS).expect("the fixture decodes");
    let layer = tile
        .layers
        .iter()
        .find(|layer| layer.name == "road")
        .expect("a road layer");

    // The roads carry no name, but they carry a type, and a type printed along the road is a
    // real data-driven label rather than an invented one.
    let mut labels: Vec<LineLabel> = Vec::new();
    for feature in layer.features() {
        if feature.geom_type() != GeomType::LineString {
            continue;
        }
        let Some((_, value)) = feature
            .properties()
            .iter()
            .find(|(key, _)| &**key == "type")
        else {
            continue;
        };
        let tessella_source::mvt::Value::String(text) = value else {
            continue;
        };
        for ring in feature.rings() {
            if ring.len() < 2 {
                continue;
            }
            #[allow(clippy::cast_precision_loss)]
            labels.push(LineLabel {
                text: text.to_string(),
                line: ring
                    .iter()
                    .map(|point| (point[0] as f32, point[1] as f32))
                    .collect(),
            });
        }
    }
    assert!(!labels.is_empty(), "the fixture has roads");

    let used: BTreeSet<u32> = labels
        .iter()
        .flat_map(|label| label.text.chars().map(|character| character as u32))
        .collect();
    let mut atlas = Atlas::new(512, 512);
    for glyph in &glyphs {
        if used.contains(&glyph.id) {
            atlas.add(glyph.id, glyph);
        }
    }
    let font = Font { glyphs, atlas };

    let (buffers, laid) = build_line_symbols(
        &labels,
        &font,
        &LineOptions {
            spacing: 400.0,
            max_angle: core::f32::consts::PI / 5.0,
            ..LineOptions::default()
        },
    );

    // A window onto part of the tile rather than the whole of it. Text draws at its own size
    // whatever the camera, so a z10 tile shown whole puts eight thousand units of road behind
    // labels that are eighty pixels wide, and they pile into each other. This is what a camera
    // at that zoom would actually show.
    const WINDOW: f32 = 1400.0;
    const ORIGIN: (f32, f32) = (2300.0, 2800.0);
    let mut canvas = vec![24u8; (CANVAS * CANVAS) as usize];
    #[allow(clippy::cast_precision_loss)]
    let scale = CANVAS as f32 / WINDOW;

    let project = |point: (f32, f32)| ((point.0 - ORIGIN.0) * scale, (point.1 - ORIGIN.1) * scale);

    let mut drawn = 0usize;
    let mut flipped = 0usize;
    let mut no_room = 0usize;
    let mut repetition = 0usize;
    for label in &labels {
        // The line goes into screen space before anything is placed on it. Text draws at its own
        // size, so the distances layout recorded are screen pixels, and walking a line in tile
        // units with them would put every glyph in the wrong place by the scale factor.
        let screen: Vec<(f32, f32)> = label.line.iter().map(|point| project(*point)).collect();

        // Repetitions appear in label order, so walk them in step.
        while repetition < laid.len() {
            let entry = &laid[repetition];
            if !on_line(&label.line, entry.anchor) {
                break;
            }
            repetition += 1;

            let quads = entry.vertices.start / 4..entry.vertices.end / 4;
            let (placement, was_flipped) = place_upright(
                &screen,
                project(entry.anchor),
                entry.segment,
                &buffers.glyph_offsets[quads],
                &LineOffsets::default(),
            );
            let Placement::Placed(glyphs) = placement else {
                no_room += 1;
                continue;
            };
            flipped += usize::from(was_flipped);

            blit_along(
                &mut canvas,
                &buffers,
                &font.atlas,
                entry.vertices.clone(),
                &glyphs,
            );
            drawn += 1;
        }
    }

    let path = std::env::var("TESSELLA_LINE_PREVIEW")
        .unwrap_or_else(|_| "/tmp/tessella-roads.png".to_string());
    write_png(&path, CANVAS, CANVAS, &canvas);
    println!(
        "\n  {} roads, {drawn} label repetitions along them ({flipped} turned upright, \
         {no_room} with no room), written to {path}\n",
        labels.len()
    );
    assert!(drawn > 0);
}

/// Whether an anchor lies on this line, within rounding.
fn on_line(line: &[(f32, f32)], anchor: (f32, f32)) -> bool {
    line.windows(2).any(|pair| {
        let (a, b) = (pair[0], pair[1]);
        let length = ((b.0 - a.0).powi(2) + (b.1 - a.1).powi(2)).sqrt();
        if length == 0.0 {
            return false;
        }
        // Distance from the anchor to the segment, by the cross product over the length.
        let cross = (b.0 - a.0).mul_add(anchor.1 - a.1, -((b.1 - a.1) * (anchor.0 - a.0)));
        let along = (b.0 - a.0).mul_add(anchor.0 - a.0, (b.1 - a.1) * (anchor.1 - a.1)) / length;
        (cross / length).abs() <= 1.5 && (-1.0..=length + 1.0).contains(&along)
    })
}

/// Draws one repetition, walking each glyph along the line as the shader does.
fn blit_along(
    canvas: &mut [u8],
    buffers: &SymbolBuffers,
    atlas: &Atlas,
    vertices: core::ops::Range<usize>,
    placed: &[PlacedGlyph],
) {
    let (atlas_width, _) = atlas.size();

    for (index, quad) in vertices.step_by(4).enumerate() {
        if quad + 3 >= buffers.vertices.len() {
            break;
        }
        // Where the projection put this glyph. One entry per quad, in the same order, which is
        // the same order layout wrote the along-line distances in.
        let Some(&PlacedGlyph { point, angle }) = placed.get(index) else {
            break;
        };

        // The glyph's own box, relative to the point the projection reached.
        let box_of = |index: usize| {
            let vertex = &buffers.vertices[quad + index];
            (
                f32::from(vertex.pos_offset[2]) / 32.0,
                f32::from(vertex.pos_offset[3]) / 32.0,
            )
        };
        let (left, top) = box_of(0);
        let (right, bottom) = box_of(3);
        let texel = |index: usize| {
            let vertex = &buffers.vertices[quad + index];
            (vertex.data[0], vertex.data[1])
        };
        let (tex_left, tex_top) = texel(0);
        let (tex_right, tex_bottom) = texel(3);
        if right <= left || bottom <= top {
            continue;
        }

        let (sin, cos) = angle.sin_cos();
        let screen = point;

        // Gather, not scatter. Walking the glyph's own box and writing to the rotated position
        // it maps to leaves the canvas full of holes as soon as the angle is not a multiple of a
        // right angle: the samples are on a grid in glyph space, and rotating a grid does not
        // give a grid. So the loop runs over the output pixels the glyph can touch and asks each
        // one where it falls in the glyph, which is what a rasterizer does and what `blit` above
        // already did. Scattering is why the road labels came out ragged while the point labels,
        // which are never rotated, looked right.
        let corners = [(left, top), (right, top), (left, bottom), (right, bottom)];
        let mapped = corners.map(|(cx, cy)| {
            (
                screen.0 + cos.mul_add(cx, -(sin * cy)),
                screen.1 + sin.mul_add(cx, cos * cy),
            )
        });
        let bound = |pick: fn(&(f32, f32)) -> f32, fold: fn(f32, f32) -> f32| {
            mapped.iter().map(pick).fold(f32::NAN, fold)
        };
        let min_x = bound(|point| point.0, f32::min).floor().max(0.0);
        let max_x = bound(|point| point.0, f32::max).ceil();
        let min_y = bound(|point| point.1, f32::min).floor().max(0.0);
        let max_y = bound(|point| point.1, f32::max).ceil();

        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        for py in min_y as u32..(max_y.max(0.0) as u32).min(CANVAS) {
            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
            for px in min_x as u32..(max_x.max(0.0) as u32).min(CANVAS) {
                // The pixel's centre, rotated back into the glyph's own frame.
                let dx = f32::from(px as u16) + 0.5 - screen.0;
                let dy = f32::from(py as u16) + 0.5 - screen.1;
                let local = (cos.mul_add(dx, sin * dy), cos.mul_add(dy, -(sin * dx)));

                let u = (local.0 - left) / (right - left);
                let v = (local.1 - top) / (bottom - top);
                if !(0.0..1.0).contains(&u) || !(0.0..1.0).contains(&v) {
                    continue;
                }

                let tx = f32::from(tex_left) + u * f32::from(tex_right - tex_left);
                let ty = f32::from(tex_top) + v * f32::from(tex_bottom - tex_top);
                let distance = sample(atlas.pixels(), atlas_width, tx, ty);
                const INNER_EDGE: f32 = (256.0 - 64.0) / 256.0;
                const EDGE_GAMMA: f32 = 0.105;
                let alpha = smoothstep(INNER_EDGE - EDGE_GAMMA, INNER_EDGE + EDGE_GAMMA, distance);
                if alpha <= 0.0 {
                    continue;
                }
                let slot = &mut canvas[(py * CANVAS + px) as usize];
                #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
                let lit = alpha.mul_add(255.0 - f32::from(*slot), f32::from(*slot)) as u8;
                *slot = (*slot).max(lit);
            }
        }
    }
}
