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

mod glyphs;
mod png;
mod raster;
mod scene;

use std::process::ExitCode;

use tessella_capture_abi::BuiltIn;
use tessella_capture_abi::envelope::{AttributeDesc, ViewId};
use tessella_capture_abi::generated::{ubo_layouts, ubo_slots};
use tessella_capture_abi::ring::Ring;
use tessella_glyph::fonts::Fonts;
use tessella_orchestrate::SlabArena;
use tessella_orchestrate::frame::{self, Frame};
use tessella_orchestrate::tile::{TileId, build_mvt_tile, build_raster_tile, build_sourceless};
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

/// Tile units across, which both a raster quad's position and its texture position are in.
const EXTENT: f32 = 8192.0;

/// A raster quad's position in the picture, in the same units as its position on the ground.
const RASTER_TEXTURE: u32 = 1;

/// An extrusion's packed fraction, whose low bit says the ring closes here.
const EXTRUSION_DECIMALS: u32 = 1;

/// An extrusion's per-feature height, in metres.
const EXTRUSION_HEIGHT: u32 = 5;

/// An extrusion's per-feature base, in metres.
const EXTRUSION_BASE: u32 = 3;

/// A symbol's anchor and this corner's offset from it.
const SYMBOL_POS_OFFSET: u32 = 0;

/// A symbol's place in the atlas, beside the packed size range.
const SYMBOL_DATA: u32 = 1;

/// Where a glyph's edge sits in its signed-distance encoding.
///
/// `(256 - 64) / 256`, which is mbgl's `buff` for SDF text: the rasterizer writes the distance
/// biased so that this value is the outline, and a threshold picked by eye instead would thicken
/// or thin every letter at once.
const SDF_EDGE: f32 = 0.75;

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
    /// A directory laid out as `{fontstack}/{range}.pbf`, for the symbol layers.
    glyphs: Option<String>,
    /// A picture for the raster layers, decoded and placed at the same address as the tile.
    raster: Option<String>,
    /// Where the tile belongs, as `z/x/y`.
    ///
    /// `None` means it is drawn at every address the cover names, which is a diagnostic rather
    /// than a map: the same features repeat across the viewport, each copy correctly placed for
    /// an address that is not the tile's own.
    at: Option<(u8, u32, u32)>,
    view: ViewTransform,
}

fn parse_args() -> Result<Args, String> {
    let mut style = None;
    let mut tile = None;
    let mut out = String::from("map.png");
    let mut source = String::from("src");
    let mut glyphs = None;
    let mut at = None;
    let mut raster = None;
    let (mut lon, mut lat, mut zoom) = (0.0, 0.0, 0.0);
    let (mut width, mut height) = (1024.0, 768.0);
    let (mut bearing, mut pitch) = (0.0, 0.0);

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
            "--glyphs" => glyphs = Some(value()?),
            "--tile-at" => at = Some(address(&value()?)?),
            "--raster" => raster = Some(value()?),
            "--lon" => lon = number(&value()?)?,
            "--lat" => lat = number(&value()?)?,
            "--zoom" => zoom = number(&value()?)?,
            "--width" => width = number(&value()?)?,
            "--height" => height = number(&value()?)?,
            "--bearing" => bearing = number(&value()?)?,
            // Without a pitch an extrusion is invisible: a roof raised straight up sits exactly
            // over its own footprint, and its walls are edge-on.
            "--pitch" => pitch = number(&value()?)?,
            "--help" | "-h" => return Err(usage()),
            other => return Err(format!("unknown flag {other}\n\n{}", usage())),
        }
    }

    Ok(Args {
        style: style.ok_or_else(|| format!("--style is required\n\n{}", usage()))?,
        tile: tile.ok_or_else(|| format!("--tile is required\n\n{}", usage()))?,
        out,
        source,
        glyphs,
        raster,
        at,
        view: camera::settled(&ViewTransform {
            longitude: lon,
            latitude: lat,
            zoom,
            width,
            height,
            bearing,
            pitch,
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

/// Parses `z/x/y`.
fn address(text: &str) -> Result<(u8, u32, u32), String> {
    let parts: Vec<&str> = text.split('/').collect();
    let [z, x, y] = parts[..] else {
        return Err(format!("`{text}` is not a z/x/y address"));
    };
    let parse = |part: &str| -> Result<u32, String> {
        part.parse()
            .map_err(|_| format!("`{text}` is not a z/x/y address"))
    };
    let z = u8::try_from(parse(z)?).map_err(|_| format!("`{text}`: zoom out of range"))?;
    Ok((z, parse(x)?, parse(y)?))
}

/// The address a fixture's filename ends in, as `...-z-x-y.mvt`.
///
/// Guessed, and only used when nothing better was given. A tile drawn at an address that is not
/// its own is still drawn *correctly* for that address -- the projection does not care that the
/// features came from somewhere else -- so getting this wrong repeats the map rather than
/// breaking it, which is why a guess is tolerable and why `--tile-at` exists to override it.
fn address_from_name(path: &str) -> Option<(u8, u32, u32)> {
    let stem = std::path::Path::new(path).file_stem()?.to_str()?;
    let mut parts = stem.rsplit('-');
    let y = parts.next()?.parse().ok()?;
    let x = parts.next()?.parse().ok()?;
    let z = parts.next()?.parse().ok()?;
    Some((z, x, y))
}

fn usage() -> String {
    "usage: capture-render --style <path> --tile <path.mvt> [--out map.png] [--source src]\n\
     \x20                     [--glyphs <dir>] [--raster <image>] [--tile-at z/x/y]\n\
      \x20                     [--lon 0] [--lat 0] [--zoom 0]\n\
      \x20                     [--width 1024] [--height 768] [--bearing 0] [--pitch 0]"
        .to_string()
}

fn run() -> Result<String, String> {
    let args = parse_args()?;

    let text = std::fs::read_to_string(&args.style)
        .map_err(|error| format!("reading {}: {error}", args.style))?;
    let mut style = Style::parse(&text).map_err(|error| format!("parsing the style: {error}"))?;
    let rejected = style.reject_uncompilable();

    // A raster layer draws from a *picture* rather than from features, so it is decoded here and
    // built by its own path -- the same picture at the tile's own address, which is what a
    // raster source would have fetched for it.
    let picture = match &args.raster {
        Some(path) => {
            let bytes = std::fs::read(path).map_err(|error| format!("reading {path}: {error}"))?;
            Some(std::sync::Arc::new(
                tessella_source::image::decode(&bytes)
                    .map_err(|error| format!("decoding {path}: {error}"))?,
            ))
        }
        None => None,
    };

    let bytes =
        std::fs::read(&args.tile).map_err(|error| format!("reading {}: {error}", args.tile))?;
    let decoded =
        Tile::decode(&bytes).map_err(|error| format!("decoding {}: {error}", args.tile))?;

    let tiles = cover::cover(&args.view).map_err(|error| format!("covering: {error}"))?;
    // Where this tile's features actually belong. Given, or read off the filename, or nowhere —
    // and "nowhere" means every address, which repeats the tile across the viewport.
    let at = args.at.or_else(|| address_from_name(&args.tile));

    let mut buckets = Vec::new();
    let mut placed = 0;
    for tile in &tiles {
        let id = TileId::new(tile.z, tile.x, tile.y);
        // A background reads no source, so it is built for every tile of the cover whether or
        // not this tile's features belong there. Dropping it with the features would leave the
        // rest of the viewport unpainted rather than empty.
        let mut built = Vec::new();
        if at.is_none_or(|(z, x, y)| (z, x, y) == (tile.z, tile.x, tile.y)) {
            built = build_mvt_tile(&style, &args.source, id, &decoded)
                .map_err(|error| format!("building {id}: {error}"))?;
            // A raster layer draws from a *picture*, not from features, so it is built by its
            // own path -- the same picture at the same address, which is what a raster source
            // would have fetched for this tile.
            if let Some(picture) = &picture {
                built.extend(
                    build_raster_tile(
                        &style,
                        &args.source,
                        std::sync::Arc::clone(picture),
                        &[tessella_tile::mask::WHOLE_TILE],
                    )
                    .map_err(|error| format!("raster {id}: {error}"))?,
                );
            }
            placed += 1;
        }
        built.extend(
            build_sourceless(&style, id).map_err(|error| format!("background {id}: {error}"))?,
        );
        built.sort_by_key(|bucket| bucket.layer_index);
        if std::env::var_os("CAPTURE_RENDER_TRACE").is_some() {
            for bucket in &built {
                let kind = match &bucket.content {
                    tessella_orchestrate::tile::Content::Symbol(layout) => {
                        format!(
                            "Symbol(empty={}, stacks={:?})",
                            layout.is_empty(),
                            layout.stacks()
                        )
                    }
                    tessella_orchestrate::tile::Content::Fill(b) => {
                        format!("Fill({})", b.vertices.len())
                    }
                    tessella_orchestrate::tile::Content::Line(b) => {
                        format!("Line({})", b.vertices.len())
                    }
                    tessella_orchestrate::tile::Content::Circle(b) => {
                        format!("Circle({})", b.vertices.len())
                    }
                    tessella_orchestrate::tile::Content::Fill3d(b) => {
                        format!("Fill3d({})", b.vertices.len())
                    }
                    _ => "other".to_string(),
                };
                eprintln!("    built {} -> {kind}", bucket.layer_id);
            }
        }
        buckets.push((id, built));
    }

    // Shaping is a two-phase thing and this is the round trip between the phases: the buckets
    // are built first, they say which glyphs they want, those are fetched, and only then can the
    // quads be made. A tool that loaded a font up front would have to load all of it.
    let mut fonts = None;
    if let Some(directory) = &args.glyphs {
        let mut store = Fonts::new("glyphs://{fontstack}/{range}.pbf");
        let files = glyphs::Directory::new(directory);
        for (_, tile_buckets) in &buckets {
            for bucket in tile_buckets {
                if let Some(layout) = bucket.content.as_symbol() {
                    store
                        .fetch(&layout.dependencies(), &files)
                        .map_err(|error| format!("glyphs: {error}"))?;
                }
            }
        }
        fonts = Some(store);
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
                fonts: fonts.as_ref(),
                patterns: None,
            },
        )
        .map_err(|error| format!("emitting: {error}"))?
    };

    // Seal before resolving anything. `SlabArena::resolve` looks in the *sealed* slabs, so the
    // one still open holds whatever was encoded last and answers `None` for it -- which a
    // consumer sees as a geometry whose position attribute has no bytes, and draws as nothing.
    // The layer that disappears is whichever happened to be encoded last, which is why the
    // symptom moves when the style changes.
    arena.seal();

    let scene = Scene::drain(ring.consumer(), &arena);
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let (width, height) = (args.view.width as u32, args.view.height as u32);
    let canvas = draw(&scene, &arena, width, height);

    let image = png::encode(width, height, &canvas.pixels);
    std::fs::write(&args.out, &image).map_err(|error| format!("writing {}: {error}", args.out))?;

    let mut note = format!(
        "{} geometries, {} drawables, {} of {} cover tiles carry features -> {} ({} bytes)",
        emitted.geometries,
        emitted.drawables,
        placed,
        tiles.len(),
        args.out,
        image.len()
    );
    match at {
        Some((z, x, y)) if placed == 0 => note.push_str(&format!(
            "\nthe tile belongs at {z}/{x}/{y}, which this view does not cover"
        )),
        Some((z, x, y)) => note.push_str(&format!("\nplaced at {z}/{x}/{y}")),
        None => note.push_str(
            "\nno address for this tile: drawn at every cover address, so the map repeats. \
             Pass --tile-at z/x/y.",
        ),
    }
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
        if tracing && drawn == 1 {
            for corner in [
                [0.0f32, 0.0f32],
                [8192.0, 0.0],
                [8192.0, 8192.0],
                [0.0, 8192.0],
            ] {
                eprintln!(
                    "    corner {corner:?} -> {:?}",
                    raster::project(&matrix, corner[0], corner[1], width as f32, height as f32)
                );
            }
        }

        match geometry.shader {
            BuiltIn::LineShader => {
                draw_line(&mut canvas, geometry, arena, &matrix, &paint);
            }
            BuiltIn::CircleShader => {
                draw_circle(&mut canvas, geometry, arena, &matrix, &paint);
            }
            BuiltIn::SymbolSDFShader | BuiltIn::SymbolIconShader => {
                draw_symbol(&mut canvas, geometry, arena, scene, &matrix, &paint);
            }
            BuiltIn::FillExtrusionShader | BuiltIn::FillExtrusionInstancedShader => {
                draw_extrusion(&mut canvas, geometry, arena, &matrix, &paint);
            }
            BuiltIn::RasterShader => {
                draw_raster(&mut canvas, geometry, arena, scene, &matrix, &paint);
            }
            _ => draw_triangles(&mut canvas, geometry, arena, &matrix, paint.color),
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
    /// An extrusion's layer-wide height and base, in metres. The per-feature values override
    /// them when the properties are data-driven, as they usually are.
    height: f32,
    base: f32,
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
        // A symbol's block opens with the text half -- fill colour, halo colour, opacity --
        // and carries the icon half behind it, because one shader samples both and the buffer
        // is its interface. The text half is what a label reads.
        BuiltIn::SymbolSDFShader | BuiltIn::SymbolIconShader => {
            (ubo_slots::ID_SYMBOL_EVALUATED_PROPS_UBO, 0, None)
        }
        // A raster's block has no colour at all -- its pixels are the colour. Only the opacity
        // is read, from where the generated layout puts it.
        BuiltIn::RasterShader => (ubo_slots::ID_RASTER_EVALUATED_PROPS_UBO, usize::MAX, None),
        _ => return None,
    };
    let bytes = scene.ubo(layer_index, slot)?;
    // `usize::MAX` means the block holds no colour; white leaves a sampled texture unchanged.
    let mut color = if color_at == usize::MAX {
        [1.0, 1.0, 1.0, 1.0]
    } else {
        read_color(bytes, color_at)?
    };

    // Opacity is a separate scalar in every one of these blocks, and a layer drawn without it is
    // opaque where the style asked for glass.
    let opacity_at = match shader {
        BuiltIn::FillShader | BuiltIn::FillOutlineShader => Some(32),
        BuiltIn::LineShader => Some(20),
        BuiltIn::CircleShader => Some(40),
        BuiltIn::FillExtrusionShader | BuiltIn::FillExtrusionInstancedShader => Some(60),
        // `text_opacity`, past the two colours.
        BuiltIn::SymbolSDFShader | BuiltIn::SymbolIconShader => Some(32),
        // `opacity`, past the spin weights, the parent-tile fade and the buffer scale.
        BuiltIn::RasterShader => Some(36),
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

    // Only an extrusion's block has these, at the offsets the generated layout gives. Reading
    // them from any other block would take whatever that block keeps at 44 and 48 -- a circle's
    // stroke width, for one -- and call it a building's height.
    let extrusion = matches!(
        shader,
        BuiltIn::FillExtrusionShader | BuiltIn::FillExtrusionInstancedShader
    );
    Some(Paint {
        color,
        width,
        base: if extrusion {
            read_f32(bytes, 44).unwrap_or(0.0)
        } else {
            0.0
        },
        height: if extrusion {
            read_f32(bytes, 48).unwrap_or(0.0)
        } else {
            0.0
        },
    })
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
        // A symbol's block opens with the clip matrix and carries two more behind it: the label
        // plane and the way back. Only the first is read here, because a point label is placed
        // in clip space directly -- a line-following one is not, and would need the other two.
        BuiltIn::SymbolSDFShader | BuiltIn::SymbolIconShader => (
            ubo_slots::ID_SYMBOL_DRAWABLE_UBO,
            ubo_layouts::SYMBOL_DRAWABLE_UBO.stride,
        ),
        // The smallest block of any layer: a matrix and nothing else, because a raster tile has
        // nothing per feature to interpolate.
        BuiltIn::RasterShader => (
            ubo_slots::ID_RASTER_DRAWABLE_UBO,
            ubo_layouts::RASTER_DRAWABLE_UBO.stride,
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
) {
    let Some((points, ())) = positions(geometry, arena, POSITION) else {
        return;
    };
    let projected: Vec<Option<[f32; 2]>> = points
        .iter()
        .map(|p| {
            raster::project(
                matrix,
                p[0],
                p[1],
                canvas.width as f32,
                canvas.height as f32,
            )
        })
        .collect();

    triangles(
        canvas,
        geometry,
        &projected,
        color,
        vertex_colors(geometry, arena).as_deref(),
    );
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

            let screen = raster::project(
                matrix,
                point[0],
                point[1],
                canvas.width as f32,
                canvas.height as f32,
            )?;
            // Y is negated with the projection's flip, so the offset follows the same axis the
            // point did rather than mirroring across it.
            Some([screen[0] + extrude[0] * half, screen[1] - extrude[1] * half])
        })
        .collect();

    triangles(
        canvas,
        geometry,
        &projected,
        paint.color,
        vertex_colors(geometry, arena).as_deref(),
    );
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
            let screen = raster::project(
                matrix,
                centre[0],
                centre[1],
                canvas.width as f32,
                canvas.height as f32,
            )?;
            Some([
                screen[0] + corner[0] * paint.width,
                screen[1] + corner[1] * paint.width,
            ])
        })
        .collect();

    triangles(
        canvas,
        geometry,
        &projected,
        paint.color,
        vertex_colors(geometry, arena).as_deref(),
    );
}

/// The attribute a shader reads a per-feature colour from, when it has one.
///
/// Each family numbers its own; there is no shared "colour is attribute one" rule, and assuming
/// there were reads a line's blur as its colour.
const fn color_attribute(shader: BuiltIn) -> Option<u32> {
    match shader {
        BuiltIn::FillShader | BuiltIn::FillOutlineShader | BuiltIn::CircleShader => Some(1),
        BuiltIn::LineShader => Some(2),
        BuiltIn::FillExtrusionShader | BuiltIn::FillExtrusionInstancedShader => Some(4),
        _ => None,
    }
}

/// Per-vertex colours, when the layer's colour is data-driven.
///
/// # Why a uniform is not enough
///
/// DR-11 splits a property by what it depends on, and the split decides how it reaches the GPU:
/// a constant or camera-only colour is a uniform, a colour that varies per feature is a vertex
/// attribute. A consumer that reads only the uniform therefore gets the property's *default* for
/// every data-driven layer — and `line-color`'s default is black, so a real style comes out as a
/// map drawn in thick black lines. That is not a wrong value on the wire; it is the wire's other
/// half never being read.
///
/// The bytes are in the binder's interleaved buffer, at the binder's stride, which is not the
/// vertex buffer's — the descriptor says which, and following it is the whole point.
fn vertex_colors(geometry: &Geometry, arena: &SlabArena) -> Option<Vec<[f32; 4]>> {
    let descriptor = geometry.attribute(color_attribute(geometry.shader)?)?;
    // -1 is the consumer's signal to drop an attribute the shader does not declare (§2.2).
    if descriptor.binding < 0 {
        return None;
    }
    let bytes = arena.resolve(descriptor.source)?;
    let stride = descriptor.stride as usize;
    let base = descriptor.offset as usize;
    let colors: Vec<[f32; 4]> = (0..geometry.vertex_count as usize)
        .map(|index| {
            let at = base + index * stride;
            let mut color = [0.0f32; 4];
            for (lane, channel) in color.iter_mut().enumerate() {
                *channel = bytes
                    .get(at + lane * 4..at + lane * 4 + 4)
                    .map(|four| f32::from_le_bytes([four[0], four[1], four[2], four[3]]))
                    .unwrap_or(0.0);
            }
            color
        })
        .collect();
    colors.iter().any(|c| c[3] > 0.0).then_some(colors)
}

/// Draws an extrusion: the roof at its height, and a wall under every edge.
///
/// # Why the walls are not in the buffer
///
/// DR-16 settled this build on Vulkan, where mbgl defines `MLN_USE_FILL_EXTRUSION_INSTANCING`,
/// so the bucket is the *instanced* branch's: the ring's own outline and an earcut roof, and
/// nothing else. The walls are instances the shader raises over the same vertices. A consumer
/// that draws only what is in the buffer therefore gets roofs — which, drawn at the ground, is
/// a fill layer wearing an extrusion's name.
///
/// The non-instanced branch would have put four extra vertices and six extra indices in the
/// buffer *per edge*, which is the five-times-the-geometry the layout module refuses.
///
/// # The height goes in as metres
///
/// `gl_Position = drawable.matrix * vec4(in_position + decimals, z, 1.0)`, with `z` the height
/// in metres and no conversion in front of it: the matrix's third column already carries
/// `pixelsPerMeter`, which `getWorldToCamera` puts there precisely so heights and positions can
/// share a matrix while being in different units.
///
/// `height_factor` is not that conversion, which is easy to assume and wrong. It appears once in
/// mbgl's shaders, in the *pattern* variant, to walk a texture up a wall:
/// `vec2 pos = vec2(edgedistance, z * drawable.height_factor)`. Using it on the position scales
/// every building by the tile count — at z14 a factor of four thousand — and buries the map
/// under one of them.
///
/// Per feature when `fill-extrusion-height` is data-driven, in which case it is a vertex
/// attribute and the uniform beside it is the property's default — the same split that made
/// every data-driven colour come out black.
///
/// # Which edges get a wall
///
/// The one the ring's closing point does not: it has no edge leaving it, and the layout packs
/// that as the low bit of `decimals`. Raising a wall there joins the last point of one ring to
/// the first point of the next, which draws a wall across the middle of a building.
fn draw_extrusion(
    canvas: &mut Canvas,
    geometry: &Geometry,
    arena: &SlabArena,
    matrix: &[f32; 16],
    paint: &Paint,
) {
    let Some(position) = geometry.attribute(POSITION) else {
        return;
    };
    let Some(decimals) = geometry.attribute(EXTRUSION_DECIMALS) else {
        return;
    };
    let (Some(pos_bytes), Some(dec_bytes)) = (
        arena.resolve(position.source),
        arena.resolve(decimals.source),
    ) else {
        return;
    };

    let heights = float_attribute(geometry, arena, EXTRUSION_HEIGHT);
    let bases = float_attribute(geometry, arena, EXTRUSION_BASE);
    let colors = vertex_colors(geometry, arena);
    let count = geometry.vertex_count as usize;

    let at = |index: usize| -> Option<([f32; 2], bool, f32, f32)> {
        let pos_at = position.offset as usize + index * position.stride as usize;
        let short = |offset: usize| {
            pos_bytes
                .get(pos_at + offset..pos_at + offset + 2)
                .map(|pair| f32::from(i16::from_le_bytes([pair[0], pair[1]])))
        };
        let dec_at = decimals.offset as usize + index * decimals.stride as usize;
        let packed = dec_bytes
            .get(dec_at..dec_at + 2)
            .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))?;
        // `(frac.x * 256 + frac.y) * 2 + discarded`: the flag rides in the low bit, which is
        // why the fraction was multiplied by two on the way in.
        let discarded = packed & 1 == 1;
        let height = heights.as_ref().and_then(|h| h.get(index).copied());
        let base = bases.as_ref().and_then(|b| b.get(index).copied());
        Some((
            [short(0)?, short(2)?],
            discarded,
            height.unwrap_or(paint.height),
            base.unwrap_or(paint.base),
        ))
    };

    let color_at = |index: usize| {
        colors
            .as_ref()
            .and_then(|c| c.get(index).copied())
            .map_or(paint.color, |mut supplied| {
                supplied[3] *= paint.color[3];
                supplied
            })
    };

    // Walls first, then the roof over them: with no depth buffer the roof has to be painted
    // last or a wall behind the building covers it.
    for index in 0..count.saturating_sub(1) {
        let (Some((a, discarded, height, base)), Some((b, ..))) = (at(index), at(index + 1)) else {
            continue;
        };
        if discarded {
            continue;
        }
        let (top, bottom) = (height, base);
        let corner = |p: [f32; 2], z: f32| {
            raster::project_3d(
                matrix,
                p[0],
                p[1],
                z,
                canvas.width as f32,
                canvas.height as f32,
            )
        };
        let (Some(a0), Some(a1), Some(b0), Some(b1)) = (
            corner(a, bottom),
            corner(a, top),
            corner(b, bottom),
            corner(b, top),
        ) else {
            continue;
        };
        // Walls are shaded so the geometry is legible as a solid rather than a silhouette. mbgl
        // computes this from the style light per face; a flat darkening says the same thing
        // about whether the wall is *there*, which is what this is for.
        let mut wall = color_at(index);
        for channel in &mut wall[..3] {
            *channel *= 0.78;
        }
        canvas.triangle(a0, a1, b1, wall);
        canvas.triangle(a0, b1, b0, wall);
    }

    let projected: Vec<Option<[f32; 2]>> = (0..count)
        .map(|index| {
            let (point, _, height, _) = at(index)?;
            raster::project_3d(
                matrix,
                point[0],
                point[1],
                height,
                canvas.width as f32,
                canvas.height as f32,
            )
        })
        .collect();
    triangles(canvas, geometry, &projected, paint.color, colors.as_deref());
}

/// A single-float vertex attribute, when the property behind it is data-driven.
fn float_attribute(geometry: &Geometry, arena: &SlabArena, attr_id: u32) -> Option<Vec<f32>> {
    let descriptor = geometry.attribute(attr_id)?;
    if descriptor.binding < 0 {
        return None;
    }
    let bytes = arena.resolve(descriptor.source)?;
    let stride = descriptor.stride as usize;
    let base = descriptor.offset as usize;
    Some(
        (0..geometry.vertex_count as usize)
            .map(|index| {
                let at = base + index * stride;
                bytes
                    .get(at..at + 4)
                    .map(|four| f32::from_le_bytes([four[0], four[1], four[2], four[3]]))
                    .unwrap_or(0.0)
            })
            .collect(),
    )
}

/// Draws a symbol layer's quads, sampling the glyph atlas the geometry names.
///
/// # What is in the vertex, and what is not
///
/// A symbol vertex is three attributes at a stride of twenty-four. The first holds the label's
/// *anchor* in tile units beside this corner's offset from it in thirty-seconds of a pixel — the
/// anchor is the same for all four corners of a glyph, so a consumer binding only the position
/// draws four coincident points. The second holds this corner's place in the atlas, which is why
/// the letter can be sampled at all rather than approximated with a box.
///
/// # Why the offset is applied after projection
///
/// It is in pixels. A label is a screen measurement — that is the whole reason `text-size` is a
/// uniform and a bucket survives a zoom — so the anchor projects and the corner offset is added
/// to the result. Adding it in tile units instead gives text that grows as you zoom in, which
/// looks almost right and is the failure this makes obvious.
fn draw_symbol(
    canvas: &mut Canvas,
    geometry: &Geometry,
    arena: &SlabArena,
    scene: &Scene,
    matrix: &[f32; 16],
    paint: &Paint,
) {
    let Some(pos) = geometry.attribute(SYMBOL_POS_OFFSET) else {
        return;
    };
    let Some(data) = geometry.attribute(SYMBOL_DATA) else {
        return;
    };
    let (Some(pos_bytes), Some(data_bytes)) =
        (arena.resolve(pos.source), arena.resolve(data.source))
    else {
        return;
    };
    // The texture the geometry itself names, not one chosen here. A drawable that referenced an
    // atlas nobody uploaded would otherwise sample whatever was last at the slot.
    let Some(atlas) = geometry
        .texture_refs
        .first()
        .and_then(|reference| scene.textures.get(&reference.texture.0))
    else {
        return;
    };

    let mut corners: Vec<Option<[f32; 2]>> = Vec::with_capacity(geometry.vertex_count as usize);
    let mut uvs: Vec<[f32; 2]> = Vec::with_capacity(geometry.vertex_count as usize);
    for index in 0..geometry.vertex_count as usize {
        let at = pos.offset as usize + index * pos.stride as usize;
        let short = |offset: usize| {
            pos_bytes
                .get(at + offset..at + offset + 2)
                .map(|pair| f32::from(i16::from_le_bytes([pair[0], pair[1]])))
                .unwrap_or(0.0)
        };
        let anchor = [short(0), short(2)];
        // Thirty-seconds of a pixel, which is what the layout packed.
        let offset = [short(4) / 32.0, short(6) / 32.0];

        let data_at = data.offset as usize + index * data.stride as usize;
        let ushort = |offset: usize| {
            data_bytes
                .get(data_at + offset..data_at + offset + 2)
                .map(|pair| f32::from(u16::from_le_bytes([pair[0], pair[1]])))
                .unwrap_or(0.0)
        };
        uvs.push([ushort(0), ushort(2)]);

        corners.push(
            raster::project(
                matrix,
                anchor[0],
                anchor[1],
                canvas.width as f32,
                canvas.height as f32,
            )
            .map(|screen| {
                // Y follows the projection's flip, so the offset moves the same way the
                // anchor did rather than mirroring across it.
                [screen[0] + offset[0], screen[1] + offset[1]]
            }),
        );
    }

    // The signed-distance edge. mbgl's SDF text shader takes its cutoff from the same constant
    // the glyph rasterizer encoded with, so a threshold chosen here rather than derived would
    // fatten or thin every letter uniformly.
    let sample = |uv: [f32; 2]| -> f32 {
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let distance = atlas.alpha(uv[0].round() as u32, uv[1].round() as u32);
        f32::from(distance > SDF_EDGE)
    };

    for segment in &geometry.segments {
        let start = segment.index_offset as usize;
        let end = start + segment.index_length as usize;
        let Some(indices) = geometry.indices.get(start..end) else {
            continue;
        };
        for triangle in indices.chunks_exact(3) {
            let slot = |index: u16| segment.vertex_offset as usize + index as usize;
            let (a, b, c) = (slot(triangle[0]), slot(triangle[1]), slot(triangle[2]));
            let (Some(Some(pa)), Some(Some(pb)), Some(Some(pc))) = (
                corners.get(a).copied(),
                corners.get(b).copied(),
                corners.get(c).copied(),
            ) else {
                continue;
            };
            let (Some(ua), Some(ub), Some(uc)) = (
                uvs.get(a).copied(),
                uvs.get(b).copied(),
                uvs.get(c).copied(),
            ) else {
                continue;
            };
            canvas.sampled_triangle([pa, pb, pc], [ua, ub, uc], paint.color, &sample);
        }
    }
}

/// Draws a raster layer: a quad, sampling the tile's own picture.
///
/// # The picture is the tile
///
/// A raster source carries no features, so the geometry is a quad — or one per entry of the
/// tile's mask, where a parent tile stands in for children that have not arrived. Both corners
/// of every vertex carry a position on the ground *and* a position in the image, in the same
/// units, which is what lets one quad show a sub-rectangle of a parent's picture without any
/// arithmetic on this side.
///
/// The texture comes from the geometry's own reference, as a symbol's atlas does. Choosing one
/// here would draw whichever picture happened to be uploaded last, which on a tiled source is a
/// neighbour's.
fn draw_raster(
    canvas: &mut Canvas,
    geometry: &Geometry,
    arena: &SlabArena,
    scene: &Scene,
    matrix: &[f32; 16],
    paint: &Paint,
) {
    let Some(position) = geometry.attribute(POSITION) else {
        return;
    };
    let Some(texture) = geometry.attribute(RASTER_TEXTURE) else {
        return;
    };
    let (Some(pos_bytes), Some(tex_bytes)) = (
        arena.resolve(position.source),
        arena.resolve(texture.source),
    ) else {
        return;
    };
    let Some(image) = geometry
        .texture_refs
        .first()
        .and_then(|reference| scene.textures.get(&reference.texture.0))
    else {
        return;
    };

    let count = geometry.vertex_count as usize;
    let mut projected = Vec::with_capacity(count);
    let mut uvs = Vec::with_capacity(count);
    for index in 0..count {
        let pos_at = position.offset as usize + index * position.stride as usize;
        let tex_at = texture.offset as usize + index * texture.stride as usize;
        let short = |bytes: &[u8], at: usize, offset: usize| {
            bytes
                .get(at + offset..at + offset + 2)
                .map(|pair| f32::from(i16::from_le_bytes([pair[0], pair[1]])))
                .unwrap_or(0.0)
        };
        projected.push(raster::project(
            matrix,
            short(pos_bytes, pos_at, 0),
            short(pos_bytes, pos_at, 2),
            canvas.width as f32,
            canvas.height as f32,
        ));
        // Tile units, so the extent maps to the image's own size.
        uvs.push([
            short(tex_bytes, tex_at, 0) / EXTENT * image.width as f32,
            short(tex_bytes, tex_at, 2) / EXTENT * image.height as f32,
        ]);
    }

    let sample_rgb = |uv: [f32; 2]| -> [f32; 4] {
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        image.rgba(uv[0].round() as u32, uv[1].round() as u32)
    };

    for segment in &geometry.segments {
        let start = segment.index_offset as usize;
        let end = start + segment.index_length as usize;
        let Some(indices) = geometry.indices.get(start..end) else {
            continue;
        };
        for triangle in indices.chunks_exact(3) {
            let slot = |index: u16| segment.vertex_offset as usize + index as usize;
            let (a, b, c) = (slot(triangle[0]), slot(triangle[1]), slot(triangle[2]));
            let (Some(Some(pa)), Some(Some(pb)), Some(Some(pc))) = (
                projected.get(a).copied(),
                projected.get(b).copied(),
                projected.get(c).copied(),
            ) else {
                continue;
            };
            let (Some(ua), Some(ub), Some(uc)) = (
                uvs.get(a).copied(),
                uvs.get(b).copied(),
                uvs.get(c).copied(),
            ) else {
                continue;
            };
            canvas.textured_triangle([pa, pb, pc], [ua, ub, uc], paint.color[3], &sample_rgb);
        }
    }
}

/// Walks a geometry's segments and fills its triangles from already-projected vertices.
fn triangles(
    canvas: &mut Canvas,
    geometry: &Geometry,
    projected: &[Option<[f32; 2]>],
    color: [f32; 4],
    per_vertex: Option<&[[f32; 4]]>,
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
                // One colour for the triangle rather than three interpolated: a data-driven
                // colour is per *feature*, so every vertex of a triangle already carries the
                // same one, and interpolating would only blur the seam between two features
                // that happen to share an edge.
                let color = per_vertex
                    .and_then(|colors| {
                        colors
                            .get(segment.vertex_offset as usize + triangle[0] as usize)
                            .copied()
                    })
                    .map_or(color, |mut supplied| {
                        // The layer's own opacity still applies: it is a uniform even when the
                        // colour beside it is not.
                        supplied[3] *= color[3];
                        supplied
                    });
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
