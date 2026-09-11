//! Maps on one style build a tile once between them.
//!
//! Every hosted map used to get a tile cache of its own, so four views over overlapping covers
//! decoded and built each shared tile four times -- 157 builds for 40 tiles on the four-view sweep,
//! and three quarters of every tick. The cache is now the style's. What that has to mean, checked
//! here through the ABI a consumer uses:
//!
//! - a second map on the same style draws a tile the first built without asking for it;
//! - a map on another style asks for everything, since its buckets would differ;
//! - the tiles go with the last map on their style, so a later one starts cold;
//! - a hosted map and one that fetches for itself never share, since a host may answer a URL
//!   differently from the network.
//!
//! # Serial
//!
//! Not a libtest harness: `main` sets `TESSELLA_WORKERS=0` before anything reads it, which puts
//! every job on the ticking thread. A map is then settled when it says so, rather than when a
//! worker happens to finish, and "the first map built everything before the second started" is a
//! fact of the program instead of a hope about timing.

use std::process::ExitCode;

use tessella_capture_abi::EnvelopeKind;
use tessella_capture_abi::ring::{self, Consumer, RingControl};
use tessella_ffi::{
    Config, MapHandle, Regions, Status, tessella_answer, tessella_create, tessella_create_hosted,
    tessella_destroy, tessella_pending, tessella_regions, tessella_take_request, tessella_tick,
};

const TILE: &[u8] = include_bytes!("../../../tests/mvt-fixtures/protomaps-berlin-14-8802-5373.mvt");

/// A style over `origin`, one fill layer, told apart from other tests' by `name`.
///
/// Each case has a style of its own because the caches are process-wide: two cases sharing a
/// style would share tiles, and the second would see the first's.
fn style(name: &str, origin: &str, color: &str) -> String {
    format!(
        r##"{{
  "version": 8,
  "name": "{name}",
  "sources": {{
    "fixture": {{"type": "vector", "tiles": ["{origin}/{{z}}/{{x}}/{{y}}.pbf"], "maxzoom": 14}}
  }},
  "layers": [
    {{"id": "earth", "type": "fill", "source": "fixture", "source-layer": "earth",
      "paint": {{"fill-color": "{color}"}}}}
  ]
}}"##
    )
}

/// A live map and the consumer's end of its ring.
struct Held {
    map: MapHandle,
    consumer: Consumer,
}

impl Held {
    fn create(style: &str, hosted: bool) -> Self {
        let config = Config {
            style_json: style.as_ptr(),
            style_json_len: style.len(),
            width: 512,
            height: 512,
            ring_capacity: 1 << 22,
            slab_capacity: 0,
        };
        let mut map: MapHandle = core::ptr::null_mut();
        // SAFETY: the config and the style it names outlive the call, which copies the style.
        let status = unsafe {
            if hosted {
                tessella_create_hosted(&config, 51.505, -0.11, 12.0, &mut map)
            } else {
                tessella_create(&config, 51.505, -0.11, 12.0, &mut map)
            }
        };
        assert_eq!(status, Status::Ok, "the map did not create");
        let mut regions = Regions {
            ring: core::ptr::null(),
            ring_len: 0,
            slabs: core::ptr::null(),
            slabs_len: 0,
        };
        // SAFETY: a live map and a valid out pointer.
        assert_eq!(unsafe { tessella_regions(map, &mut regions) }, Status::Ok);
        // SAFETY: the ring region begins with an initialized, eight-aligned control block and
        // lives as long as the map.
        let capacity = unsafe { (*regions.ring.cast::<RingControl>()).capacity } as usize;
        // SAFETY: as above; the producer half this also returns is dropped unused.
        let (_, consumer) =
            unsafe { ring::attach(regions.ring.cast_mut(), capacity) }.expect("the ring attaches");
        Self { map, consumer }
    }

    /// Ticks until the map has nothing left to do, answering what it asks for from the fixture.
    /// Answers how many tiles it asked for and how many geometries it published.
    fn settle(&mut self) -> Drove {
        let mut drove = Drove::default();
        let mut asked: Vec<u64> = Vec::new();
        for _ in 0..200 {
            for ticket in asked.drain(..) {
                // SAFETY: a live map, and the fixture is valid for its length.
                unsafe { tessella_answer(self.map, ticket, 200, TILE.as_ptr(), TILE.len()) };
            }
            // SAFETY: a live map.
            assert_eq!(unsafe { tessella_tick(self.map) }, Status::Ok);
            let mut records = 0;
            while let Some(record) = self.consumer.peek() {
                records += 1;
                if record.kind == EnvelopeKind::GeometryAdd {
                    drove.geometry += 1;
                }
                let consumed = record.consumed();
                self.consumer.advance(consumed);
            }
            loop {
                let (mut ticket, mut url, mut len) = (0u64, core::ptr::null::<u8>(), 0usize);
                // SAFETY: a live map and valid out pointers. A map that fetches for itself
                // answers `NotHosted` and leaves the ticket zero.
                unsafe { tessella_take_request(self.map, &mut ticket, &mut url, &mut len) };
                if ticket == 0 {
                    break;
                }
                asked.push(ticket);
                drove.requests += 1;
            }
            let mut pending = 0u64;
            // SAFETY: a live map and a valid out pointer.
            unsafe { tessella_pending(self.map, &mut pending) };
            if records == 0 && pending == 0 && asked.is_empty() && drove.geometry > 0 {
                return drove;
            }
        }
        panic!("the map did not settle in 200 ticks: {drove:?}");
    }
}

impl Drop for Held {
    fn drop(&mut self) {
        // SAFETY: created by `Held::create` and destroyed once, here.
        unsafe { tessella_destroy(self.map) };
    }
}

#[derive(Debug, Default)]
struct Drove {
    requests: usize,
    geometry: usize,
}

const HOST: &str = "http://tiles.invalid";

/// A map that asked for tiles, which is what the first map of every case must do.
///
/// Every case compares a second map with a first, and a first map that inherited some other case's
/// tiles asks for nothing -- so "the second asked as much as the first" would hold at zero and say
/// nothing. Each case starting cold is what makes its comparison mean something.
fn cold(drove: &Drove) {
    assert!(
        drove.requests > 0,
        "a first map found tiles already built: {drove:?}"
    );
}

/// The case the cache is for: the second view over the same cover costs nothing.
fn a_second_map_on_a_style_asks_for_nothing_the_first_built() {
    let text = style("second-map", HOST, "#2f6f4f");
    let mut first = Held::create(&text, true);
    let drove = first.settle();
    cold(&drove);

    let mut second = Held::create(&text, true);
    let again = second.settle();
    assert_eq!(
        again.requests, 0,
        "the second map fetched what the first built: {again:?}"
    );
    assert!(again.geometry > 0, "the second map drew nothing: {again:?}");
}

/// Another style's buckets differ, so it shares nothing -- even over the same tiles.
fn another_style_builds_its_own() {
    let mut first = Held::create(&style("other-style", HOST, "#2f6f4f"), true);
    let drove = first.settle();
    cold(&drove);
    let mut second = Held::create(&style("other-style", HOST, "#c04030"), true);
    let again = second.settle();
    assert_eq!(
        again.requests, drove.requests,
        "a map on another style shared tiles it should have built: {again:?} after {drove:?}"
    );
}

/// The cache is held by the maps on its style and by nothing else.
fn the_tiles_go_with_the_last_map() {
    let text = style("last-map", HOST, "#2f6f4f");
    let drove = Held::create(&text, true).settle();
    cold(&drove);
    // The first map is gone by here: it was a temporary.
    let again = Held::create(&text, true).settle();
    assert_eq!(
        again.requests, drove.requests,
        "a map after the last one on its style found tiles still cached: {again:?}"
    );
}

/// A host's answers stay with the host's maps.
fn a_hosted_map_shares_nothing_with_one_that_fetches() {
    let server =
        tile_server::Server::start(tile_server::Routes::new().tiles(TILE.to_vec(), Some((0, 14))))
            .expect("the tile server starts");
    let text = style("hosted-apart", &server.origin(), "#2f6f4f");

    let mut fetching = Held::create(&text, false);
    fetching.settle();
    assert!(
        server.requests() > 0,
        "the map that fetches for itself fetched nothing"
    );

    let mut hosted = Held::create(&text, true);
    let drove = hosted.settle();
    assert!(
        drove.requests > 0,
        "a hosted map was handed tiles a map fetched for itself: {drove:?}"
    );
}

fn main() -> ExitCode {
    // SAFETY: nothing else is running yet -- this is `main`, before the first thread and before
    // anything has read the environment -- which is the whole reason this test has its own.
    unsafe { std::env::set_var("TESSELLA_WORKERS", "0") };

    let cases: [(&str, fn()); 4] = [
        (
            "a_second_map_on_a_style_asks_for_nothing_the_first_built",
            a_second_map_on_a_style_asks_for_nothing_the_first_built,
        ),
        ("another_style_builds_its_own", another_style_builds_its_own),
        (
            "the_tiles_go_with_the_last_map",
            the_tiles_go_with_the_last_map,
        ),
        (
            "a_hosted_map_shares_nothing_with_one_that_fetches",
            a_hosted_map_shares_nothing_with_one_that_fetches,
        ),
    ];
    let mut failed = 0;
    for (name, case) in cases {
        match std::panic::catch_unwind(case) {
            Ok(()) => println!("test {name} ... ok"),
            Err(_) => {
                println!("test {name} ... FAILED");
                failed += 1;
            }
        }
    }
    if failed == 0 {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}
