//! Every parser that reads bytes off a network, against bytes it was not written for.
//!
//! # Why this rather than `cargo-fuzz`
//!
//! libFuzzer needs nightly, and DR-17 pins this workspace to a stable toolchain the target's
//! Yocto release carries. A fuzz target CI cannot run is a fuzz target nobody runs — so the
//! mutation happens here, deterministically, in a test that goes with every commit. It is a
//! weaker search than coverage-guided fuzzing and it runs a thousand times more often, which for
//! the failures this is about — a panic on a truncated length, an index past the end of a
//! buffer, a count believed without checking — is the better trade. A `fuzz/` directory for
//! depth remains worth adding; it is not a substitute for this and this is not a substitute for
//! it.
//!
//! # What a failure means
//!
//! `forbid(unsafe_code)` holds across the workspace, so a malformed input cannot corrupt memory.
//! What it can do is *panic* — which on a worker thread takes down a tile build, and which a
//! hostile origin can then trigger at will — or allocate from a number it was told rather than
//! one it checked. Both are denial of service on a device that has one map and no supervisor to
//! restart it.
//!
//! So the contract every parser here is held to is: **return, either way**. `Ok` on something
//! that happens to remain valid is fine, `Err` is fine, and a panic is a bug however unlikely
//! the input.
//!
//! # The mutations are deterministic
//!
//! One seed, printed on failure, and the mutant that failed is printed with it. A random harness
//! that cannot reproduce its own failures reports a ghost.

use std::panic;

/// Mutants per fixture. Enough to reach past the header of every format here, and quick enough
/// that this runs on every commit rather than nightly.
const ROUNDS: usize = 3_000;

/// xorshift64*, seeded by a constant.
///
/// Not for quality — the mutations are structural rather than statistical — but for
/// reproducibility: the same build produces the same corpus, so a failure names a case that can
/// be looked at rather than one that vanishes on re-run.
struct Rng(u64);

impl Rng {
    fn new() -> Self {
        Self(0x2545_f491_4f6c_dd1d)
    }

    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }

    fn below(&mut self, bound: usize) -> usize {
        if bound == 0 {
            return 0;
        }
        #[allow(clippy::cast_possible_truncation)]
        {
            (self.next() % bound as u64) as usize
        }
    }
}

/// One mutation of `seed`.
///
/// The five kinds are chosen for what breaks a length-prefixed format rather than for variety.
/// A bit flip finds a believed tag; a truncation finds a read past the end; a corrupted length
/// finds a count taken on trust; a spliced run finds a structure that assumes it was written
/// once. Together they are most of what a real corpus of broken tiles looks like.
fn mutate(seed: &[u8], rng: &mut Rng) -> Vec<u8> {
    let mut out = seed.to_vec();
    if out.is_empty() {
        return out;
    }

    match rng.next() % 5 {
        // A single bit, which is what a bad link or a wrong offset gives.
        0 => {
            let at = rng.below(out.len());
            out[at] ^= 1 << (rng.below(8));
        }
        // A byte replaced by an extreme: the values that break a length or a tag.
        1 => {
            let at = rng.below(out.len());
            out[at] = match rng.next() % 3 {
                0 => 0x00,
                1 => 0xff,
                #[allow(clippy::cast_possible_truncation)]
                _ => rng.next() as u8,
            };
        }
        // Cut short, anywhere. The commonest real corruption and the one that finds a read past
        // the end of a buffer.
        2 => {
            let keep = rng.below(out.len());
            out.truncate(keep);
        }
        // A run repeated, which makes a structure appear twice where one was expected.
        3 => {
            let from = rng.below(out.len());
            let run = rng.below(out.len() - from).min(64);
            let piece = out[from..from + run].to_vec();
            let at = rng.below(out.len());
            out.splice(at..at, piece);
        }
        // A large varint written over whatever was there: a count or a length taken on trust.
        _ => {
            let at = rng.below(out.len());
            for (offset, byte) in [0xff, 0xff, 0xff, 0xff, 0x7f].into_iter().enumerate() {
                if at + offset < out.len() {
                    out[at + offset] = byte;
                }
            }
        }
    }
    out
}

/// Runs `parse` over mutations of every seed, and reports the one that panicked.
fn hammer(what: &str, seeds: &[&[u8]], parse: impl Fn(&[u8]) + panic::RefUnwindSafe) {
    // The default hook prints a backtrace per panic, and a failing run would bury its own
    // report under thousands of them.
    let previous = panic::take_hook();
    panic::set_hook(Box::new(|_| {}));

    let mut rng = Rng::new();
    let mut checked = 0usize;
    let mut failure = None;

    'outer: for seed in seeds {
        for round in 0..ROUNDS {
            let mutant = mutate(seed, &mut rng);
            if panic::catch_unwind(|| parse(&mutant)).is_err() {
                failure = Some((round, mutant));
                break 'outer;
            }
            checked += 1;
        }
    }

    panic::set_hook(previous);

    if let Some((round, mutant)) = failure {
        let head: Vec<String> = mutant
            .iter()
            .take(64)
            .map(|byte| format!("{byte:02x}"))
            .collect();
        panic!(
            "{what} panicked on a malformed input at round {round}\n  {} bytes, first 64: {}",
            mutant.len(),
            head.join("")
        );
    }

    assert!(checked > 0, "{what} was never exercised");
}

/// The harness reports a panic when there is one.
///
/// Every test below passes, and a harness that cannot fail would produce exactly that result. So
/// this feeds it a parser that panics on a byte the mutations reach, and asserts the failure is
/// reported rather than swallowed — `catch_unwind` is doing the swallowing, and a `hammer` that
/// caught the panic and then forgot to re-raise it would be silent about every real one.
#[test]
fn the_harness_reports_a_panic() {
    let seed: &[u8] = b"the quick brown fox jumps over the lazy dog";

    let reported = panic::catch_unwind(|| {
        hammer("a parser that panics", &[seed], |bytes| {
            assert!(!bytes.contains(&0xff), "found the byte");
        });
    });

    assert!(
        reported.is_err(),
        "the harness ran a panicking parser three thousand times and said nothing"
    );
}

/// A vector tile, which is the byte stream a map reads most of.
#[test]
fn a_vector_tile_decoder_survives_malformed_input() {
    // The conformance fixtures rather than a real tile, and not only for speed. A bit flip in
    // half a megabyte lands on a coordinate almost every time; in fifty bytes it lands on a tag,
    // a length or a geometry command, which is where a decoder breaks. One real tile is kept
    // beside them so the shapes a hand-made fixture does not have are covered too.
    const POINT: &[u8] =
        include_bytes!("../../../tests/mvt-fixtures/valid/Feature-single-point.mvt");
    const LINE: &[u8] =
        include_bytes!("../../../tests/mvt-fixtures/valid/Feature-single-linestring.mvt");
    const POLYGON: &[u8] =
        include_bytes!("../../../tests/mvt-fixtures/valid/Feature-single-polygon.mvt");
    const MULTI: &[u8] =
        include_bytes!("../../../tests/mvt-fixtures/valid/Feature-single-multipoint.mvt");
    const STREETS: &[u8] = include_bytes!("../../../tests/mvt-fixtures/streets-10-163-395.mvt");

    hammer(
        "Tile::decode",
        &[POINT, LINE, POLYGON, MULTI, STREETS],
        |bytes| {
            // The decode *and* a walk of what it produced: a decoder can return a structure whose
            // offsets are only followed later, and the geometry cursor is where a believed count
            // would be spent.
            if let Ok(tile) = tessella_source::mvt::Tile::decode(bytes) {
                for layer in &tile.layers {
                    for feature in layer.features() {
                        let _ = feature.geom_type();
                        let _ = feature.properties();
                        for ring in feature.rings() {
                            let _ = ring.len();
                        }
                    }
                }
            }
        },
    );
}

/// A glyph range, which arrives from a font origin.
#[test]
fn a_glyph_range_parser_survives_malformed_input() {
    const REAL: &[u8] = include_bytes!("../../../tests/glyph-fixtures/TestFont/0-255.pbf");
    const FAKE: &[u8] = include_bytes!("../../../tests/glyph-fixtures/fake_glyphs-0-255.pbf");

    hammer("glyph::pbf::parse", &[REAL, FAKE], |bytes| {
        let range = tessella_glyph::pbf::Range {
            first: 0,
            last: 255,
        };
        if let Ok(glyphs) = tessella_glyph::pbf::parse(range, bytes) {
            for glyph in &glyphs {
                // The metrics are believed by the shaper, and the bitmap length by the atlas.
                let _ = glyph.bitmap_size();
                let _ = glyph.bitmap.len();
            }
        }
    });
}

/// A sprite index, which is JSON from a style's origin.
#[test]
fn a_sprite_index_parser_survives_malformed_input() {
    const INDEX: &[u8] = include_bytes!("../../../tests/sprite-fixtures/emerald.json");

    hammer("sprite::parse", &[INDEX], |bytes| {
        if let Ok(index) = tessella_glyph::sprite::parse(bytes, Some((200, 299))) {
            for sprite in index.values() {
                let _ = sprite.logical_size();
            }
        }
    });
}

/// A sprite sheet, which is a PNG from a style's origin.
#[cfg(feature = "image")]
#[test]
fn a_sprite_sheet_decoder_survives_malformed_input() {
    const SHEET: &[u8] = include_bytes!("../../../tests/sprite-fixtures/emerald.png");

    hammer("sprite::decode_sheet", &[SHEET], |bytes| {
        if let Ok(sheet) = tessella_glyph::sprite::decode_sheet(bytes) {
            // The dimensions are believed by everything downstream, so the invariant they carry
            // is checked here rather than assumed.
            assert_eq!(
                sheet.pixels.len(),
                (sheet.width as usize) * (sheet.height as usize) * 4,
                "a decoded sheet's dimensions do not describe its pixels"
            );
        }
    });
}

/// A raster tile, which is a PNG or a JPEG from an imagery origin.
///
/// Fuzzed separately from the sprite sheet even though the two go through one decoder, because
/// the mutations that matter are not the same: a sheet fixture is a PNG, so a mutation of it
/// stays in the PNG decoder, and the JPEG half would never be reached. Both fixtures are here,
/// so a mutation that flips the signature crosses between them as well.
#[cfg(feature = "image")]
#[test]
fn a_raster_tile_decoder_survives_malformed_input() {
    const PNG: &[u8] = include_bytes!("../../../tests/image-fixtures/tile.png");
    const JPEG: &[u8] = include_bytes!("../../../tests/image-fixtures/tile.jpeg");

    hammer("image::decode", &[PNG, JPEG], |bytes| {
        if let Ok(image) = tessella_source::image::decode(bytes) {
            // Everything downstream indexes by these dimensions — an atlas rectangle, a texture
            // upload, a quad's coordinates — so a buffer that does not match them is a read past
            // the end several stages from here.
            assert_eq!(
                image.pixels.len(),
                (image.width as usize) * (image.height as usize) * 4,
                "a decoded image's dimensions do not describe its pixels"
            );
            assert!(image.width > 0 && image.height > 0, "a zero-area image");
        }
    });
}

/// A GeoJSON document, which a style may name by URL.
#[test]
fn a_geojson_reader_survives_malformed_input() {
    const STYLE: &[u8] = include_bytes!("../../tessella-style/tests/hermetic_style.json");

    hammer("geojson::read", &[STYLE], |bytes| {
        // Text first, because that is how it arrives. A body that is not JSON is the ordinary
        // case rather than the interesting one, and `serde_json` refusing it is the answer.
        if let Ok(value) = serde_json::from_slice::<tessella_style::Value>(bytes) {
            let _ = tessella_source::geojson::read(&value);
        }
    });
}

/// A style document, which is the first thing fetched and the one that names everything else.
#[test]
fn a_style_parser_survives_malformed_input() {
    const HERMETIC: &[u8] = include_bytes!("../../tessella-style/tests/hermetic_style.json");
    const SYMBOLS: &[u8] = include_bytes!("../../tessella-style/tests/symbol_style.json");

    hammer("Style::parse", &[HERMETIC, SYMBOLS], |bytes| {
        let Ok(text) = core::str::from_utf8(bytes) else {
            return;
        };
        if let Ok(style) = tessella_style::Style::parse(text) {
            // Compiling the layers is where an expression is actually walked, and a style that
            // parsed is not yet a style that resolves.
            for layer in &style.layers {
                let _ = tessella_style::property::resolve_paint(layer);
                if let Some(filter) = &layer.filter {
                    let _ = tessella_style::Filter::parse(filter);
                }
            }
        }
    });
}

/// A PMTiles archive, read in place off local storage.
///
/// Local, but not therefore trusted: an archive is a file a user downloaded from somewhere, and
/// the directory format is a chain of offsets and lengths that a reader follows.
#[test]
fn a_pmtiles_header_parser_survives_malformed_input() {
    // A minimal v3 header, built rather than vendored: the archives are not in this repository.
    // 127 bytes is the whole of it, which is enough for the offsets to be worth corrupting.
    let mut header = vec![0u8; 127];
    header[0..7].copy_from_slice(b"PMTiles");
    header[7] = 3;
    // Root directory at offset 127, length 0; the rest of the offsets left zero.
    header[8..16].copy_from_slice(&127u64.to_le_bytes());

    hammer("pmtiles::Header::parse", &[&header], |bytes| {
        let _ = tessella_storage::pmtiles::Header::parse(bytes);
    });
}
