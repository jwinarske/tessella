//! The warm loop: what a tick costs when nothing happened, and what it sends when something did.
//!
//! # Why this is the test that matters
//!
//! §13.1's invariant and §9.3's counters both reduce to one claim — traffic is proportional to
//! change — and until now nothing drove a *sequence* of frames to check it. `frame::emit` was
//! called from thirteen test files, each emitting one frame by hand, which can only ever show
//! that a frame is well-formed. Whether the second frame costs anything is a different question
//! and it is the one a running map lives or dies on.

use std::sync::Arc;

use tessella_capture_abi::envelope::ViewId;
use tessella_capture_abi::ring::{self, region_size};
use tessella_orchestrate::map::{Map, Tick, Tiles};
use tessella_orchestrate::tile::{LayerBucket, TileId, build_mvt_tile};
use tessella_source::mvt::Tile;
use tessella_style::Style;
use tessella_tile::camera;
use tessella_tile::cover::ViewTransform;

const REAL_TILE: &[u8] = include_bytes!("../../../tests/mvt-fixtures/real-world-0-0-0.mvt");
const STYLE: &str = r##"{
  "version": 8,
  "sources": {"src": {"type": "vector", "tiles": []}},
  "layers": [
    {"id": "bg", "type": "background", "paint": {"background-color": "#101418"}},
    {"id": "sea", "type": "fill", "source": "src", "source-layer": "water",
     "paint": {"fill-color": "#20344c"}},
    {"id": "banks", "type": "line", "source": "src", "source-layer": "water",
     "paint": {"line-color": "#88a", "line-width": 1.5}}
  ]
}"##;

/// Every cover tile answers with the same real tile's buckets.
///
/// The addresses differ and the geometry does not, which is what this test wants: it is about
/// how many *records* a second frame sends, and one tile's worth of real features is enough to
/// make a frame that is not trivially empty.
struct Warm {
    buckets: Arc<Vec<LayerBucket>>,
}

impl Warm {
    fn new(style: &Style) -> Self {
        let decoded = Tile::decode(REAL_TILE).expect("the fixture decodes");
        let built = build_mvt_tile(style, "src", TileId::new(0, 0, 0), &decoded).expect("builds");
        Self {
            buckets: Arc::new(built),
        }
    }
}

impl Tiles for Warm {
    fn buckets(&self, _tile: TileId) -> Option<Arc<Vec<LayerBucket>>> {
        Some(Arc::clone(&self.buckets))
    }
}

fn view(zoom: f64) -> ViewTransform {
    camera::settled(&ViewTransform {
        longitude: -0.11,
        latitude: 51.505,
        zoom,
        width: 1024.0,
        height: 768.0,
        bearing: 0.0,
        pitch: 0.0,
    })
}

/// A ring big enough that filling it is never what a test is measuring.
const CAPACITY: usize = 1 << 24;

/// A settled map sends nothing at all, and keeps sending nothing.
///
/// The first tick emits because the map has never drawn. Every one after it is idle, and idle
/// has to mean *no bytes*: a loop that re-sent an unchanged frame would satisfy every
/// well-formedness test in the suite and still hold a consumer's upload path awake forever.
#[test]
fn a_settled_map_emits_once_and_then_nothing() {
    let style = Style::parse(STYLE).expect("the style parses");
    let tiles = Warm::new(&style);
    let mut region = vec![0u64; region_size(CAPACITY).div_ceil(8)];
    // SAFETY: sized by `region_size`, eight-aligned as a `Vec<u64>`, outlives both halves.
    let (mut producer, _consumer) =
        unsafe { ring::init(region.as_mut_ptr().cast::<u8>(), CAPACITY) };

    let mut map = Map::new(style, view(4.0), ViewId(0));
    let first = map
        .tick(&mut producer, &tiles)
        .expect("the first frame emits");
    let Tick::Emitted(emitted) = first else {
        panic!("the first tick must emit: nothing has been drawn yet");
    };
    assert!(
        emitted.geometries > 0,
        "the first frame carried no geometry"
    );

    for round in 0..8 {
        assert_eq!(
            map.tick(&mut producer, &tiles).expect("a settled tick"),
            Tick::Idle,
            "tick {round} sent something on a map that had not moved"
        );
    }
}

/// A camera that moves emits again; one that moves back to where it was does too.
///
/// The second half is the part worth asserting. It would be easy to write a tracker that
/// remembered only the *first* camera and reported every later frame idle, and every test that
/// only ever moves away would pass.
#[test]
fn moving_the_camera_reopens_the_gate() {
    let style = Style::parse(STYLE).expect("the style parses");
    let tiles = Warm::new(&style);
    let mut region = vec![0u64; region_size(CAPACITY).div_ceil(8)];
    // SAFETY: as above.
    let (mut producer, _consumer) =
        unsafe { ring::init(region.as_mut_ptr().cast::<u8>(), CAPACITY) };

    let mut map = Map::new(style, view(4.0), ViewId(0));
    map.tick(&mut producer, &tiles).expect("the first frame");
    assert_eq!(
        map.tick(&mut producer, &tiles).expect("settled"),
        Tick::Idle
    );

    map.look_at(view(5.0));
    assert!(
        matches!(
            map.tick(&mut producer, &tiles).expect("the moved frame"),
            Tick::Emitted(_)
        ),
        "a moved camera must emit"
    );
    assert_eq!(
        map.tick(&mut producer, &tiles).expect("settled again"),
        Tick::Idle
    );

    map.look_at(view(4.0));
    assert!(
        matches!(
            map.tick(&mut producer, &tiles).expect("the returned frame"),
            Tick::Emitted(_)
        ),
        "moving back is still moving"
    );
}

/// A tile landing emits, without the camera having moved.
#[test]
fn a_tile_landing_emits() {
    let style = Style::parse(STYLE).expect("the style parses");
    let tiles = Warm::new(&style);
    let mut region = vec![0u64; region_size(CAPACITY).div_ceil(8)];
    // SAFETY: as above.
    let (mut producer, _consumer) =
        unsafe { ring::init(region.as_mut_ptr().cast::<u8>(), CAPACITY) };

    let mut map = Map::new(style, view(4.0), ViewId(0));
    map.tick(&mut producer, &tiles).expect("the first frame");
    assert_eq!(
        map.tick(&mut producer, &tiles).expect("settled"),
        Tick::Idle
    );

    map.mark_dirty();
    assert!(
        matches!(
            map.tick(&mut producer, &tiles).expect("the tile frame"),
            Tick::Emitted(_)
        ),
        "a source reporting churn must emit"
    );
    assert_eq!(
        map.tick(&mut producer, &tiles).expect("settled after"),
        Tick::Idle,
        "the dirty flag is consumed, not sticky"
    );
}

/// A pan re-announces only what entered, not the whole cover.
///
/// This is the claim `emit_incremental` exists to make and the one a frame loop can break
/// without failing anything else: a tile that stays in view keeps its geometry id, so its bytes
/// are not sent again. If the loop handed the emitter a fresh registry each frame — or rebuilt
/// the cover into different ids — every surviving tile would be re-announced and the frame would
/// still be perfectly well-formed.
///
/// Measured on this fixture: twenty-seven geometries on the first frame and nine on the pan, so
/// two thirds of the cover survived and cost nothing. The assertion is the inequality rather
/// than the numbers, which move with the fixture and the viewport.
#[test]
fn a_pan_sends_only_what_entered() {
    let style = Style::parse(STYLE).expect("the style parses");
    let tiles = Warm::new(&style);
    let mut region = vec![0u64; region_size(CAPACITY).div_ceil(8)];
    // SAFETY: as above.
    let (mut producer, _consumer) =
        unsafe { ring::init(region.as_mut_ptr().cast::<u8>(), CAPACITY) };

    let mut map = Map::new(style, view(4.0), ViewId(0));
    let Tick::Emitted(first) = map.tick(&mut producer, &tiles).expect("the first frame") else {
        panic!("the first tick emits");
    };

    // A nudge small enough that most of the cover survives it.
    let mut nudged = view(4.0);
    nudged.longitude += 0.35;
    map.look_at(camera::settled(&nudged));
    let Tick::Emitted(panned) = map.tick(&mut producer, &tiles).expect("the panned frame") else {
        panic!("a moved camera emits");
    };

    assert!(
        panned.geometries < first.geometries,
        "the pan announced {} geometries against the first frame's {} — a surviving tile's \
         bytes are being sent again",
        panned.geometries,
        first.geometries
    );
}

/// A map with glyphs draws labels; the same map without them draws the rest and no labels.
///
/// # What it caught
///
/// It failed first, and what it reported was **a symbol layer drawn before its glyphs arrive
/// never drawing at all** — not late, never, for the life of the map.
///
/// The emitter's incremental path encodes only *fresh* buckets: a geometry id is allocated in
/// the binding pass and the bucket is fresh the frame its id is new, known ever after. But a
/// symbol bucket is bound like any other, while `encode_parts` refuses it when `fonts` is
/// `None` — `layout.lay_out(fonts?, …)`. So the first frame binds the bucket, encodes nothing,
/// and marks it known; every frame after skips it at
/// `if registry.is_some() && !fresh_buckets.contains(…)`, and the glyphs arriving changes
/// nothing because freshness was already spent.
///
/// This is the normal case rather than a race. Which glyphs a style needs is discovered by
/// evaluating `text-field` against a tile's own features, so the fetch *cannot* precede the
/// first tile build — every map takes this path.
///
/// Measured here: the layout produces twenty vertices when called directly, and the same
/// buckets through the frame produce zero geometries both before and after the glyphs land.
///
/// # Why it was not caught before
///
/// Nothing drove a second frame. `frame::emit` is called from thirteen test files and each
/// emits once, where every bucket is fresh by construction and the skip never runs. It took a
/// loop to reach the frame where freshness has been spent.
///
/// # The fix
///
/// In the binding pass, which is where the chance is spent. `Content::is_encodable` asks whether
/// a bucket can be turned into records with what the frame *holds*, where `has_data` asks only
/// whether it has content — and `has_data`'s own comment already drew that line, noting a symbol
/// layer has data "whether or not the glyphs to shape it with have arrived". A bucket that fails
/// the new predicate is not bound, so it is still fresh when its glyphs land.
///
/// Deliberately not at the skip. The comment above `fresh_buckets` explains that the skip is
/// bucket-scoped precisely because getting its scope wrong encodes one drawable's bytes under
/// another's id, and "the corruption is silent, because the record is well formed and simply
/// draws the wrong thing". Both places that decide which buckets to skip now ask the same
/// question, which is what keeps `binding_index` paired with the bindings it counts.
///
/// Measured after: nothing announced while the glyphs were missing, one geometry once they
/// arrived.
///
/// The second half is the assertion. `Frame::fonts` being `None` is documented as a legitimate
/// frame rather than an error — a symbol layer's glyphs are a fetch, discovered by evaluating
/// `text-field` against the tile's own features — so a loop that quietly passed `None` forever
/// would produce frames that pass every well-formedness test in the suite and never draw a
/// label. That is precisely what this loop did until the fonts were threaded through, and
/// nothing would have said so.
#[test]
fn labels_draw_only_once_the_glyphs_are_handed_over() {
    use std::collections::BTreeMap;

    use tessella_glyph::fonts::{Dependencies, Fonts};
    use tessella_orchestrate::tile::{Content, build_tile};
    use tessella_source::geojson;
    use tessella_source::tiling::TilingOptions;
    use tessella_storage::source::{FetchError, FileSource, Response};

    /// Serves the `file://` URLs the style's `glyphs` template builds.
    struct Disk;
    impl FileSource for Disk {
        fn fetch(&self, url: &str) -> Result<Response, FetchError> {
            let path = url.strip_prefix("file://").unwrap_or(url);
            Ok(Response {
                status: 200,
                body: std::fs::read(path).unwrap_or_default(),
                ..Response::default()
            })
        }
    }

    let raw = include_str!("../../tessella-style/tests/symbol_style.json");
    let root = concat!(env!("CARGO_MANIFEST_DIR"), "/../..");
    let style: Style = serde_json::from_str(&raw.replace("TESSELLA", root)).expect("style parses");
    let Some(tessella_style::Source::Geojson(source)) = style.source("probe") else {
        panic!("one geojson source")
    };
    let features = geojson::read(&source.data).expect("features read");

    // The tile the symbol capture puts its labels in.
    let tile = TileId::new(13, 4093, 2723);
    let built = build_tile(&style, "probe", tile, &features, TilingOptions::default())
        .expect("the tile builds");
    let has_symbols = built
        .iter()
        .any(|bucket| matches!(bucket.content, Content::Symbol(_)));
    assert!(has_symbols, "the fixture must carry a symbol layer");

    /// One tile, at the address the labels are in.
    struct One {
        tile: TileId,
        buckets: Arc<Vec<LayerBucket>>,
    }
    impl Tiles for One {
        fn buckets(&self, tile: TileId) -> Option<Arc<Vec<LayerBucket>>> {
            (tile == self.tile).then(|| Arc::clone(&self.buckets))
        }
    }
    let tiles = One {
        tile,
        buckets: Arc::new(built),
    };

    let at = camera::settled(&ViewTransform {
        longitude: -0.11,
        latitude: 51.505,
        zoom: 13.0,
        width: 1024.0,
        height: 768.0,
        bearing: 0.0,
        pitch: 0.0,
    });

    let mut region = vec![0u64; region_size(CAPACITY).div_ceil(8)];
    // SAFETY: as above.
    let (mut producer, _consumer) =
        unsafe { ring::init(region.as_mut_ptr().cast::<u8>(), CAPACITY) };

    // Without glyphs first.
    let mut map = Map::new(style.clone(), at, ViewId(0));
    let Tick::Emitted(mute) = map.tick(&mut producer, &tiles).expect("a frame") else {
        panic!("the first tick emits");
    };

    // Then the same map, the same camera, with the glyphs the labels need.
    let mut fonts = Fonts::new(style.glyphs.clone().expect("a glyph URL"));
    let mut wanted: Dependencies = BTreeMap::new();
    for bucket in tiles.buckets(tile).expect("the tile").iter() {
        if let Content::Symbol(layout) = &bucket.content {
            for (stack, codepoints) in layout.dependencies() {
                wanted.entry(stack).or_default().extend(codepoints);
            }
        }
    }
    fonts.fetch(&wanted, &Disk).expect("the fonts read");
    map.set_fonts(fonts);

    let Tick::Emitted(lettered) = map.tick(&mut producer, &tiles).expect("a second frame") else {
        panic!("handing over glyphs must reopen the gate");
    };

    assert!(
        lettered.geometries > mute.geometries,
        "with glyphs the frame announced {} geometries against {} without — the labels are not \
         reaching the wire",
        lettered.geometries,
        mute.geometries
    );
}
