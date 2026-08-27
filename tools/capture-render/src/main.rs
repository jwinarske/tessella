//! Rasterizes one settled frame of a style to a PNG, by consuming the capture stream.
//!
//! # What this is for
//!
//! Every other test of this producer inspects the values it computed: the vertex it packed, the
//! matrix it built, the offset it wrote a uniform at. That catches arithmetic and misses the
//! class of fault where each value is right and the *description* of it is wrong — an attribute
//! pointed at the wrong buffer, a stride from the wrong struct, a matrix read as row-major. Those
//! are arithmetically clean and produce noise, or a shape in the wrong place, and nothing in the
//! suite could see them because nothing in the suite bound the stream the way a renderer does.
//!
//! So this binds it that way. It reads the descriptors, resolves the slabs, takes the matrix out
//! of the layer's consolidated buffer at the order's own `ubo_index`, takes the colour out of the
//! layer's evaluated-properties block at the generated offset, and draws. Nothing here reaches
//! back into the producer's types for a value the stream is supposed to carry.
//!
//! # Usage
//!
//! ```text
//! capture-render --style <path> --tile <path.mvt> --out map.png \
//!                [--lon 0] [--lat 0] [--zoom 0] [--width 1024] [--height 768] [--source src]
//! ```
//!
//! The tile is used for every address in the cover, which is what makes this a *rendering* test
//! rather than a map: one tile's features repeated is enough to see whether a layer draws, where
//! it lands, and in what colour.

mod png;
mod raster;
mod scene;

use std::process::ExitCode;

use tessella_capture_abi::BuiltIn;
use tessella_capture_abi::envelope::{AttributeDesc, ViewId};
use tessella_capture_abi::generated::{ubo_layouts, ubo_slots};
use tessella_capture_abi::ring::Ring;
use tessella_orchestrate::SlabArena;
use tessella_orchestrate::frame::{self, Frame};
use tessella_orchestrate::tile::{TileId, build_mvt_tile, build_sourceless};
use tessella_source::mvt::Tile;
use tessella_style::Style;
use tessella_style::light::Light;
use tessella_tile::camera;
use tessella_tile::cover::{self, ViewTransform};

use raster::Canvas;
use scene::{Geometry, Scene};

/// The position attribute, which every shader family numbers zero.
const POSITION: u32 = 0;

/// A line's second attribute: the extrusion and the distance along the line.
const LINE_DATA: u32 = 1;

/// The scale a line's extrusion was stored at. 63 rather than 127, because the encoding must
/// also carry the longer extrusions a bevel join produces.
const EXTRUDE_SCALE: f32 = 63.0;

fn main() -> ExitCode {
    match run() {
        Ok(message) => {
            println!("{message}");
            ExitCode::SUCCESS
        }
        Err(message) => {
            eprintln!("capture-render: {message}");
            ExitCode::FAILURE
        }
    }
}

struct Args {
    style: String,
    tile: String,
    out: String,
    source: String,
    view: ViewTransform,
}

fn parse_args() -> Result<Args, String> {
    let mut style = None;
    let mut tile = None;
    let mut out = String::from("map.png");
    let mut source = String::from("src");
    let (mut lon, mut lat, mut zoom) = (0.0, 0.0, 0.0);
    let (mut width, mut height) = (1024.0, 768.0);

    let mut args = std::env::args().skip(1);
    while let Some(flag) = args.next() {
        let mut value = || {
            args.next()
                .ok_or_else(|| alloc_format(&format!("{flag} needs a value")))
        };
        match flag.as_str() {
            "--style" => style = Some(value()?),
            "--tile" => tile = Some(value()?),
            "--out" => out = value()?,
            "--source" => source = value()?,
            "--lon" => lon = number(&value()?)?,
            "--lat" => lat = number(&value()?)?,
            "--zoom" => zoom = number(&value()?)?,
            "--width" => width = number(&value()?)?,
            "--height" => height = number(&value()?)?,
            "--help" | "-h" => return Err(usage()),
            other => return Err(format!("unknown flag {other}\n\n{}", usage())),
        }
    }

    Ok(Args {
        style: style.ok_or_else(|| format!("--style is required\n\n{}", usage()))?,
        tile: tile.ok_or_else(|| format!("--tile is required\n\n{}", usage()))?,
        out,
        source,
        view: camera::settled(&ViewTransform {
            longitude: lon,
            latitude: lat,
            zoom,
            width,
            height,
            bearing: 0.0,
            pitch: 0.0,
        }),
    })
}

fn alloc_format(message: &str) -> String {
    message.to_string()
}

fn number(text: &str) -> Result<f64, String> {
    text.parse()
        .map_err(|_| format!("`{text}` is not a number"))
}

fn usage() -> String {
    "usage: capture-render --style <path> --tile <path.mvt> [--out map.png] [--source src]\n\
     \x20                     [--lon 0] [--lat 0] [--zoom 0] [--width 1024] [--height 768]"
        .to_string()
}

fn run() -> Result<String, String> {
    let args = parse_args()?;

    let text = std::fs::read_to_string(&args.style)
        .map_err(|error| format!("reading {}: {error}", args.style))?;
    let mut style = Style::parse(&text).map_err(|error| format!("parsing the style: {error}"))?;
    let rejected = style.reject_uncompilable();

    let bytes =
        std::fs::read(&args.tile).map_err(|error| format!("reading {}: {error}", args.tile))?;
    let decoded =
        Tile::decode(&bytes).map_err(|error| format!("decoding {}: {error}", args.tile))?;

    let tiles = cover::cover(&args.view).map_err(|error| format!("covering: {error}"))?;
    let mut buckets = Vec::new();
    for tile in &tiles {
        let id = TileId::new(tile.z, tile.x, tile.y);
        let mut built = build_mvt_tile(&style, &args.source, id, &decoded)
            .map_err(|error| format!("building {id}: {error}"))?;
        built.extend(
            build_sourceless(&style, id).map_err(|error| format!("background {id}: {error}"))?,
        );
        built.sort_by_key(|bucket| bucket.layer_index);
        buckets.push((id, built));
    }

    let mut ring = Ring::new(1 << 26);
    let mut arena = SlabArena::new();
    let emitted = {
        let (producer, _) = ring.split();
        frame::emit(
            producer,
            &mut arena,
            &Frame {
                style: &style,
                view: &args.view,
                view_id: ViewId(0),
                tiles: &tiles,
                buckets: &buckets,
                light: &Light::default(),
            },
        )
        .map_err(|error| format!("emitting: {error}"))?
    };

    let scene = Scene::drain(ring.consumer(), &arena);
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let (width, height) = (args.view.width as u32, args.view.height as u32);
    let canvas = draw(&scene, &arena, width, height);

    let image = png::encode(width, height, &canvas.pixels);
    std::fs::write(&args.out, &image).map_err(|error| format!("writing {}: {error}", args.out))?;

    let mut note = format!(
        "{} geometries, {} drawables, {} tiles -> {} ({} bytes)",
        emitted.geometries,
        emitted.drawables,
        tiles.len(),
        args.out,
        image.len()
    );
    if !rejected.is_empty() {
        note.push_str(&format!("\n{} layer(s) dropped:", rejected.len()));
        for layer in &rejected {
            note.push_str(&format!("\n  {} — {}", layer.id, layer.reason));
        }
    }
    Ok(note)
}

/// Draws the scene in the order the stream gave, which is the order the picture depends on.
fn draw(scene: &Scene, arena: &SlabArena, width: u32, height: u32) -> Canvas {
    // The background layer's own colour, taken from its properties block rather than assumed:
    // a background is the one layer whose geometry the consumer synthesizes, so if it is not
    // painted from the stream it is not tested by this at all.
    let background = scene
        .order
        .iter()
        .find_map(|drawable| {
            let bytes = scene.ubo(drawable.layer_index, ubo_slots::ID_BACKGROUND_PROPS_UBO)?;
            read_color(bytes, 0)
        })
        .unwrap_or([0.05, 0.06, 0.08, 1.0]);
    let mut canvas = Canvas::new(width, height, background);

    // Two things are worth being able to see when a picture comes out wrong, and neither is
    // visible in the picture: what order the stream asked for, and what was skipped rather than
    // drawn. A blank frame is the same blank frame whether nothing was emitted, nothing had a
    // matrix, or everything was painted in the wrong sequence and covered over.
    let tracing = std::env::var_os("CAPTURE_RENDER_TRACE").is_some();
    if tracing {
        let layers: Vec<u32> = scene.order.iter().map(|d| d.layer_index).collect();
        eprintln!("  draw order by layer: {layers:?}");
    }
    let (mut no_geometry, mut no_paint, mut no_matrix, mut drawn) = (0, 0, 0, 0);
    for index in painter_order(scene) {
        let drawable = &scene.order[index];
        let Some(geometry) = scene.geometries.get(&drawable.geometry.0) else {
            no_geometry += 1;
            continue;
        };
        let Some(paint) = layer_paint(scene, geometry.shader, drawable.layer_index) else {
            no_paint += 1;
            continue;
        };
        let Some(matrix) = matrix_for(
            scene,
            geometry.shader,
            drawable.layer_index,
            drawable.ubo_index,
        ) else {
            no_matrix += 1;
            continue;
        };
        drawn += 1;

        match geometry.shader {
            BuiltIn::LineShader => {
                draw_line(&mut canvas, geometry, arena, &matrix, &paint, width, height);
            }
            BuiltIn::CircleShader => {
                draw_circle(&mut canvas, geometry, arena, &matrix, &paint, width, height);
            }
            _ => draw_triangles(
                &mut canvas,
                geometry,
                arena,
                &matrix,
                paint.color,
                width,
                height,
            ),
        }
    }
    if tracing {
        eprintln!(
            "  drawn {drawn}, skipped: {no_geometry} without geometry, \
             {no_paint} without paint, {no_matrix} without a matrix"
        );
    }
    canvas
}

/// The order to paint in, which is not the order the stream gives.
///
/// # Why a consumer reverses what the producer sent
///
/// mbgl draws front-to-back against a depth buffer: within a pass the topmost layer goes first,
/// each layer at its own depth slot, and a lower layer is rejected where a higher one already
/// covered. The stream carries that order because the oracle does, and the golden capture is
/// unambiguous — style layer 4 draws at slot 1, layer 3 at slot 2.
///
/// This rasterizer has no depth buffer, so painting that sequence puts every lower layer on top
/// of the one above it: a map with its landcover over its buildings, and a first frame that came
/// out as nothing but background. Reversing each *pass run* — rather than the whole order —
/// keeps the passes in their emitted sequence, which is a real dependency: the opaque pass runs
/// before the translucent one, and only the entries inside a run are back-to-front.
fn painter_order(scene: &Scene) -> Vec<usize> {
    let mut out: Vec<usize> = Vec::with_capacity(scene.order.len());
    let mut run_start = 0;
    for index in 0..=scene.order.len() {
        let ends =
            index == scene.order.len() || scene.passes.get(index) != scene.passes.get(run_start);
        if ends && index > run_start {
            out.extend((run_start..index).rev());
            run_start = index;
        }
    }
    out
}

/// A layer's colour and the scalar its geometry is sized by.
///
/// One field for two different measurements, because they are the same thing to the shape: a
/// line's half-width and a circle's radius are both "how far, in pixels, a vertex moves from
/// the position the buffer holds". Neither is in the vertex, and both are why the geometry does
/// not have to be rebuilt when the style changes.
struct Paint {
    color: [f32; 4],
    /// A line's stroke width, or a circle's radius plus its stroke.
    width: f32,
}

fn layer_paint(scene: &Scene, shader: BuiltIn, layer_index: u32) -> Option<Paint> {
    let (slot, color_at, width_at) = match shader {
        BuiltIn::FillShader | BuiltIn::FillOutlineShader => {
            (ubo_slots::ID_FILL_EVALUATED_PROPS_UBO, 0, None)
        }
        // A line's block is colour, then blur, opacity, gap width, offset and width.
        BuiltIn::LineShader => (ubo_slots::ID_LINE_EVALUATED_PROPS_UBO, 0, Some(32)),
        // A circle's is colour, stroke colour, then radius — and the stroke widens the quad
        // beyond the radius, so a circle drawn at the radius alone is clipped by its own outline.
        BuiltIn::CircleShader => (ubo_slots::ID_CIRCLE_EVALUATED_PROPS_UBO, 0, Some(32)),
        BuiltIn::FillExtrusionShader | BuiltIn::FillExtrusionInstancedShader => {
            (ubo_slots::ID_FILL_EXTRUSION_PROPS_UBO, 0, None)
        }
        _ => return None,
    };
    let bytes = scene.ubo(layer_index, slot)?;
    let mut color = read_color(bytes, color_at)?;

    // Opacity is a separate scalar in every one of these blocks, and a layer drawn without it is
    // opaque where the style asked for glass.
    let opacity_at = match shader {
        BuiltIn::FillShader | BuiltIn::FillOutlineShader => Some(32),
        BuiltIn::LineShader => Some(20),
        BuiltIn::CircleShader => Some(40),
        BuiltIn::FillExtrusionShader | BuiltIn::FillExtrusionInstancedShader => Some(60),
        _ => None,
    };
    if let Some(at) = opacity_at
        && let Some(opacity) = read_f32(bytes, at)
    {
        color[3] *= opacity;
    }

    let mut width = width_at.and_then(|at| read_f32(bytes, at)).unwrap_or(1.0);
    if matches!(shader, BuiltIn::CircleShader) {
        width += read_f32(bytes, 44).unwrap_or(0.0);
    }

    Some(Paint { color, width })
}

/// The matrix for one drawable, out of its layer's consolidated buffer at the order's index.
///
/// `ubo_index` is the order's, not the geometry's — it is assigned per pass from the view's own
/// draw order, which is why sharing a geometry between views does not share it. Reading the
/// buffer at the geometry's position instead puts one tile's matrix on another's vertices, and
/// the result is a map whose tiles are shuffled.
fn matrix_for(
    scene: &Scene,
    shader: BuiltIn,
    layer_index: u32,
    ubo_index: u32,
) -> Option<[f32; 16]> {
    let (slot, stride) = match shader {
        BuiltIn::FillShader | BuiltIn::FillOutlineShader => (
            ubo_slots::ID_FILL_DRAWABLE_UBO,
            ubo_layouts::FILL_DRAWABLE_UNION_UBO.stride,
        ),
        BuiltIn::LineShader => (
            ubo_slots::ID_LINE_DRAWABLE_UBO,
            ubo_layouts::LINE_DRAWABLE_UNION_UBO.stride,
        ),
        BuiltIn::CircleShader => (
            ubo_slots::ID_CIRCLE_DRAWABLE_UBO,
            ubo_layouts::CIRCLE_DRAWABLE_UBO.stride,
        ),
        BuiltIn::FillExtrusionShader | BuiltIn::FillExtrusionInstancedShader => (
            ubo_slots::ID_FILL_EXTRUSION_DRAWABLE_UBO,
            ubo_layouts::FILL_EXTRUSION_DRAWABLE_UBO.stride,
        ),
        _ => return None,
    };
    let bytes = scene.ubo(layer_index, slot)?;
    let start = ubo_index as usize * stride as usize;
    let mut matrix = [0.0f32; 16];
    for (index, cell) in matrix.iter_mut().enumerate() {
        *cell = read_f32(bytes, start + index * 4)?;
    }
    Some(matrix)
}

/// Reads a geometry's positions through its own descriptor.
fn positions(geometry: &Geometry, arena: &SlabArena, attr_id: u32) -> Option<(Vec<[f32; 2]>, ())> {
    let descriptor = geometry.attribute(attr_id)?;
    let bytes = arena.resolve(descriptor.source)?;
    Some((read_short2(bytes, descriptor, geometry.vertex_count), ()))
}

fn read_short2(bytes: &[u8], descriptor: &AttributeDesc, count: u32) -> Vec<[f32; 2]> {
    let stride = descriptor.stride as usize;
    let base = descriptor.offset as usize;
    (0..count as usize)
        .map(|index| {
            let at = base + index * stride;
            let read = |offset: usize| {
                bytes
                    .get(at + offset..at + offset + 2)
                    .map(|pair| f32::from(i16::from_le_bytes([pair[0], pair[1]])))
                    .unwrap_or(0.0)
            };
            [read(0), read(2)]
        })
        .collect()
}

fn draw_triangles(
    canvas: &mut Canvas,
    geometry: &Geometry,
    arena: &SlabArena,
    matrix: &[f32; 16],
    color: [f32; 4],
    width: u32,
    height: u32,
) {
    let Some((points, ())) = positions(geometry, arena, POSITION) else {
        return;
    };
    let projected: Vec<Option<[f32; 2]>> = points
        .iter()
        .map(|p| raster::project(matrix, p[0], p[1], width as f32, height as f32))
        .collect();

    triangles(canvas, geometry, &projected, color);
}

/// Draws a line by widening its centreline, which is what its shader does.
///
/// The widening happens after projection, in pixels, because that is where mbgl's line shader
/// does it — a line's width is a screen measurement, which is why zooming does not rebuild the
/// bucket. Widening in tile units instead gives a road that thins as you zoom out until it
/// vanishes.
fn draw_line(
    canvas: &mut Canvas,
    geometry: &Geometry,
    arena: &SlabArena,
    matrix: &[f32; 16],
    paint: &Paint,
    width: u32,
    height: u32,
) {
    let Some(position) = geometry.attribute(POSITION) else {
        return;
    };
    let Some(data) = geometry.attribute(LINE_DATA) else {
        return;
    };
    let (Some(pos_bytes), Some(data_bytes)) =
        (arena.resolve(position.source), arena.resolve(data.source))
    else {
        return;
    };

    let half = (paint.width / 2.0).max(0.5);
    let projected: Vec<Option<[f32; 2]>> = (0..geometry.vertex_count as usize)
        .map(|index| {
            let at = position.offset as usize + index * position.stride as usize;
            let short = |offset: usize| {
                pos_bytes
                    .get(at + offset..at + offset + 2)
                    .map(|pair| i16::from_le_bytes([pair[0], pair[1]]))
                    .unwrap_or(0)
            };
            // The centreline is stored doubled, with the cap and side flags in the low bits.
            let point = [f32::from(short(0) >> 1), f32::from(short(2) >> 1)];

            let data_at = data.offset as usize + index * data.stride as usize;
            let byte = |offset: usize| f32::from(*data_bytes.get(data_at + offset).unwrap_or(&128));
            let extrude = [
                (byte(0) - 128.0) / EXTRUDE_SCALE,
                (byte(1) - 128.0) / EXTRUDE_SCALE,
            ];

            let screen = raster::project(matrix, point[0], point[1], width as f32, height as f32)?;
            // Y is negated with the projection's flip, so the offset follows the same axis the
            // point did rather than mirroring across it.
            Some([screen[0] + extrude[0] * half, screen[1] - extrude[1] * half])
        })
        .collect();

    triangles(canvas, geometry, &projected, paint.color);
}

/// Draws a circle layer by expanding each quad, which is what its shader does.
///
/// The buffer holds the centre *doubled*, with the corner's sign in the low bit of each axis —
/// four vertices per point, all at the same place until something expands them. The radius is a
/// uniform, so the expansion happens at draw time in pixels, and a consumer binding only the
/// position gets four coincident vertices and two degenerate triangles per circle.
///
/// The disc itself is the shader's: mbgl draws a quad and discards outside the radius. Here the
/// quad is filled, which is enough to say whether a circle layer is present, in the right place
/// and the right size — a square where a disc belongs is a difference nobody can mistake for a
/// bug in the producer.
fn draw_circle(
    canvas: &mut Canvas,
    geometry: &Geometry,
    arena: &SlabArena,
    matrix: &[f32; 16],
    paint: &Paint,
    width: u32,
    height: u32,
) {
    let Some((packed, ())) = positions(geometry, arena, POSITION) else {
        return;
    };
    let projected: Vec<Option<[f32; 2]>> = packed
        .iter()
        .map(|v| {
            // `floor(v / 2)` for the centre and `mod(v, 2) * 2 - 1` for the corner, which is
            // mbgl's own arithmetic rather than a bit test -- the vertex was built by doubling
            // and adding a zero or a one, so the halves recover exactly.
            let centre = [(v[0] * 0.5).floor(), (v[1] * 0.5).floor()];
            let corner = [
                (v[0] - centre[0] * 2.0) * 2.0 - 1.0,
                (v[1] - centre[1] * 2.0) * 2.0 - 1.0,
            ];
            let screen =
                raster::project(matrix, centre[0], centre[1], width as f32, height as f32)?;
            Some([
                screen[0] + corner[0] * paint.width,
                screen[1] + corner[1] * paint.width,
            ])
        })
        .collect();

    triangles(canvas, geometry, &projected, paint.color);
}

/// Walks a geometry's segments and fills its triangles from already-projected vertices.
fn triangles(
    canvas: &mut Canvas,
    geometry: &Geometry,
    projected: &[Option<[f32; 2]>],
    color: [f32; 4],
) {
    for segment in &geometry.segments {
        let start = segment.index_offset as usize;
        let end = start + segment.index_length as usize;
        let Some(indices) = geometry.indices.get(start..end) else {
            continue;
        };
        for triangle in indices.chunks_exact(3) {
            let at = |index: u16| {
                projected
                    .get(segment.vertex_offset as usize + index as usize)
                    .copied()
                    .flatten()
            };
            if let (Some(a), Some(b), Some(c)) = (at(triangle[0]), at(triangle[1]), at(triangle[2]))
            {
                canvas.triangle(a, b, c, color);
            }
        }
    }
}

fn read_f32(bytes: &[u8], at: usize) -> Option<f32> {
    bytes
        .get(at..at + 4)
        .map(|four| f32::from_le_bytes([four[0], four[1], four[2], four[3]]))
}

fn read_color(bytes: &[u8], at: usize) -> Option<[f32; 4]> {
    let mut color = [0.0f32; 4];
    for (index, channel) in color.iter_mut().enumerate() {
        *channel = read_f32(bytes, at + index * 4)?;
    }
    Some(color)
}
