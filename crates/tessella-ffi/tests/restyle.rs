// SPDX-License-Identifier: BSD-2-Clause
//! A running map takes a new style, and what that costs.
//!
//! The style was fixed at `tessella_create` (tessella#334), so a host switching day for night,
//! toggling a layer or adding its own had one route: destroy the map and build another, losing the
//! camera, the label fades, the drawable ids the consumer holds, and every tile.
//!
//! What a restyle has to do instead, and what is measured here: draw the new document, *remove* what
//! the old one left on the consumer, and -- with a store on disk -- reach no origin at all. The last
//! one is the reason `cache_path` and this call are one feature: the revision is in every tile key,
//! so the buckets are rebuilt either way, and the question is only whether their bytes come over the
//! network again.

#![cfg(feature = "cache")]

use tessella_capture_abi::EnvelopeKind;
use tessella_capture_abi::ring::{self, Consumer, RingControl};
use tessella_ffi::{Config, MapHandle, Regions, Status};

const TILE: &[u8] = include_bytes!("../../../tests/mvt-fixtures/protomaps-berlin-14-8802-5373.mvt");

/// One fill layer over a vector source.
fn one_layer(name: &str, origin: &str) -> String {
    format!(
        r##"{{
  "version": 8,
  "name": "{name}",
  "sources": {{
    "fixture": {{"type": "vector", "tiles": ["{origin}/{{z}}/{{x}}/{{y}}.pbf"],
                 "minzoom": 12, "maxzoom": 14}}
  }},
  "layers": [
    {{"id": "earth", "type": "fill", "source": "fixture", "source-layer": "earth",
      "paint": {{"fill-color": "#204060"}}}}
  ]
}}"##
    )
}

/// The same source under two layers, and a light the default is not.
///
/// Two layers so the drawable count changes, which the *source's* style decides; a light so the
/// camera record changes, which the *map's* own style decides and nothing else observes -- see
/// [`Counted::light`].
fn two_layers(name: &str, origin: &str) -> String {
    format!(
        r##"{{
  "version": 8,
  "name": "{name}",
  "light": {{"intensity": 0.75, "anchor": "map"}},
  "sources": {{
    "fixture": {{"type": "vector", "tiles": ["{origin}/{{z}}/{{x}}/{{y}}.pbf"],
                 "minzoom": 12, "maxzoom": 14}}
  }},
  "layers": [
    {{"id": "earth", "type": "fill", "source": "fixture", "source-layer": "earth",
      "paint": {{"fill-color": "#801020"}}}},
    {{"id": "earth-again", "type": "fill", "source": "fixture", "source-layer": "earth",
      "paint": {{"fill-color": "#108020"}}}}
  ]
}}"##
    )
}

/// What a run of ticks put on the wire.
#[derive(Debug, Default, PartialEq)]
struct Counted {
    adds: usize,
    removes: usize,
    uses: usize,
    releases: usize,
    /// The light the last camera record carried, which is what observes the *map's* style.
    ///
    /// Measured rather than assumed: with the map's own style left stale on a restyle, the adds,
    /// the uses *and* the draw order all doubled -- every one of those is derived from the buckets
    /// the source built. The light is resolved from the document the map holds and travels in every
    /// camera block, so it is the one field here that a stale style cannot move.
    light: f64,
    /// Entries in the last draw order.
    ///
    /// A geometry is announced because a bucket was built and a use names it, both of which the
    /// source's style decides -- measured: with the map's own style left stale, the adds and the
    /// uses both doubled on a restyle and only this did not. The order is the frame's own list,
    /// walked from the layers the *map* holds, so a drawable the new document has no layer for is
    /// built, announced, used and never drawn.
    ordered: usize,
}

struct Live {
    map: MapHandle,
    consumer: Consumer,
}

impl Live {
    fn create(style: &str, cache_path: Option<&str>) -> Self {
        let config = Config {
            style_json: style.as_ptr(),
            style_json_len: style.len(),
            width: 512,
            height: 512,
            ring_capacity: 1 << 22,
            slab_capacity: 0,
            cache_path: cache_path.map_or(core::ptr::null(), str::as_ptr),
            cache_path_len: cache_path.map_or(0, str::len),
        };
        let mut map: MapHandle = core::ptr::null_mut();
        // SAFETY: the config and everything it names outlive the call, which copies what it keeps.
        let status =
            unsafe { tessella_ffi::tessella_create(&config, 52.52, 13.405, 13.0, &mut map) };
        assert_eq!(status, Status::Ok, "the map did not create");

        let mut regions = Regions {
            ring: core::ptr::null(),
            ring_len: 0,
            slabs: core::ptr::null(),
            slabs_len: 0,
        };
        // SAFETY: a live map and a valid out pointer.
        assert_eq!(
            unsafe { tessella_ffi::tessella_regions(map, &mut regions) },
            Status::Ok
        );
        // SAFETY: the ring region begins with an initialized, eight-aligned control block and lives
        // as long as the map.
        let capacity = unsafe { (*regions.ring.cast::<RingControl>()).capacity } as usize;
        // SAFETY: as above; the producer half this also returns is dropped unused.
        let (_, consumer) =
            unsafe { ring::attach(regions.ring.cast_mut(), capacity) }.expect("the ring attaches");
        Self { map, consumer }
    }

    /// Ticks until the wire goes quiet, and counts what it carried.
    fn settle(&mut self, seconds: u64) -> Counted {
        let mut counted = Counted::default();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(seconds);
        let mut quiet = 0;
        while std::time::Instant::now() < deadline && quiet < 12 {
            // SAFETY: a live map.
            assert_eq!(unsafe { tessella_ffi::tessella_tick(self.map) }, Status::Ok);
            let mut records = 0;
            while let Some(record) = self.consumer.peek() {
                records += 1;
                match record.kind {
                    EnvelopeKind::GeometryAdd => counted.adds += 1,
                    EnvelopeKind::GeometryRemove => counted.removes += 1,
                    EnvelopeKind::ViewUse => counted.uses += 1,
                    EnvelopeKind::ViewRelease => counted.releases += 1,
                    EnvelopeKind::CameraUpdate => {
                        use tessella_capture_abi::envelope::WireRecord as _;
                        if let Some(camera) =
                            tessella_capture_abi::envelope::CameraUpdate::from_bytes(record.record)
                        {
                            counted.light = camera.light.intensity;
                        }
                    }
                    EnvelopeKind::OrderUpdate => {
                        use tessella_capture_abi::envelope::WireRecord as _;
                        if let Some(order) =
                            tessella_capture_abi::envelope::OrderUpdate::from_bytes(record.record)
                        {
                            // The latest order, not the sum of them: each one replaces the last.
                            counted.ordered = order.entries.count as usize;
                        }
                    }
                    _ => {}
                }
                let consumed = record.consumed();
                self.consumer.advance(consumed);
            }
            if records == 0 {
                quiet += 1;
            } else {
                quiet = 0;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        counted
    }

    fn restyle(&self, style: &str) -> Status {
        // SAFETY: a live map, and the style outlives the call.
        unsafe { tessella_ffi::tessella_set_style(self.map, style.as_ptr(), style.len()) }
    }
}

impl Drop for Live {
    fn drop(&mut self) {
        // SAFETY: a live map, dropped once.
        unsafe { tessella_ffi::tessella_destroy(self.map) };
    }
}

fn scratch(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("tessella-restyle-{name}-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("a scratch directory");
    dir
}

/// A restyle draws the new document, removes what the old one left, and with a store fetches
/// nothing.
#[test]
fn a_restyle_redraws_and_with_a_store_refetches_nothing() {
    let dir = scratch("store");
    let path = dir.join("cache.sqlite").to_string_lossy().into_owned();
    let server =
        tile_server::Server::start(tile_server::Routes::new().tiles(TILE.to_vec(), Some((12, 14))))
            .expect("the server starts");
    let origin = server.origin();

    let mut live = Live::create(&one_layer("day", &origin), Some(&path));
    let first = live.settle(20);
    assert!(first.adds > 0, "the first style drew nothing: {first:?}");
    assert_eq!(first.removes, 0, "nothing to remove yet: {first:?}");
    let fetched = server.requests();
    assert!(fetched > 0, "the first style reached no origin at all");

    assert_eq!(live.restyle(&two_layers("night", &origin)), Status::Ok);
    let second = live.settle(20);

    assert!(
        second.adds >= first.adds * 2,
        "the second style has twice the layers, so it has at least twice the drawables: \
         {first:?} then {second:?}"
    );
    assert!(
        second.uses >= first.uses * 2,
        "the new document's second layer was never used: {first:?} then {second:?}"
    );
    assert!(
        (first.light - 0.5).abs() < 1e-9,
        "the first document declares no light, so it is lit by mbgl's default half intensity: \
         {first:?}"
    );
    assert!(
        (second.light - 0.75).abs() < 1e-9,
        "the new document's light never reached the frame, which is what a stale style on the map \
         itself looks like -- everything else here is derived from the buckets the source built: \
         {first:?} then {second:?}"
    );
    assert!(
        second.ordered >= first.ordered * 2,
        "the second layer is built and used and not drawn, which is what a stale style on the map \
         itself looks like -- the order is the one list the frame walks from the map's own layers: \
         {first:?} then {second:?}"
    );
    assert!(
        second.removes > 0,
        "the old revision's geometry was never removed, so a consumer would draw both: {second:?}"
    );
    assert_eq!(
        server.requests(),
        fetched,
        "a restyle with a store on disk went back to the origin"
    );
    drop(live);
    std::fs::remove_dir_all(&dir).ok();
}

/// Without a store it refetches, which is the cost the store is what removes.
#[test]
fn a_restyle_without_a_store_refetches() {
    let server =
        tile_server::Server::start(tile_server::Routes::new().tiles(TILE.to_vec(), Some((12, 14))))
            .expect("the server starts");
    let origin = server.origin();

    let mut live = Live::create(&one_layer("plain-day", &origin), None);
    let first = live.settle(20);
    assert!(first.adds > 0, "the first style drew nothing: {first:?}");
    let fetched = server.requests();

    assert_eq!(
        live.restyle(&two_layers("plain-night", &origin)),
        Status::Ok
    );
    let second = live.settle(20);
    assert!(second.adds > 0, "the second style drew nothing: {second:?}");
    assert!(
        server.requests() > fetched,
        "a restyle with no store drew the new revision from nowhere"
    );
}

/// A restyle to a document with nothing to fetch still redraws.
///
/// The case the dirty mark is for, and the only one that isolates it: every other path to a frame
/// goes through a tile landing, which moves the source's generation and marks the map dirty anyway.
/// Here the new document has no sources at all, so nothing lands, and without the mark the old
/// revision's drawables would sit on the consumer for ever.
#[test]
fn a_restyle_with_nothing_to_fetch_still_redraws() {
    let server =
        tile_server::Server::start(tile_server::Routes::new().tiles(TILE.to_vec(), Some((12, 14))))
            .expect("the server starts");
    let origin = server.origin();

    let mut live = Live::create(&one_layer("before", &origin), None);
    let first = live.settle(20);
    assert!(first.adds > 0, "the first style drew nothing: {first:?}");

    // Two hashes, because a color is a `#` immediately after a quote and one hash ends the string
    // there. The styles above are spelled the same way for the same reason.
    const BACKGROUND: &str = r##"{"version": 8, "name": "after", "sources": {},
        "layers": [{"id": "bg", "type": "background",
                    "paint": {"background-color": "#101418"}}]}"##;
    assert_eq!(live.restyle(BACKGROUND), Status::Ok);
    let second = live.settle(10);
    assert!(
        second.removes > 0 || second.releases > 0,
        "nothing was emitted for a restyle with no tiles to wait for, so the old drawables stay \
         on the consumer: {second:?}"
    );
}

/// A document that will not parse changes nothing, which is what lets a host offer one it is unsure
/// of.
#[test]
fn a_style_that_does_not_parse_changes_nothing() {
    let server =
        tile_server::Server::start(tile_server::Routes::new().tiles(TILE.to_vec(), Some((12, 14))))
            .expect("the server starts");
    let origin = server.origin();

    let mut live = Live::create(&one_layer("kept", &origin), None);
    let first = live.settle(20);
    assert!(first.adds > 0, "the first style drew nothing: {first:?}");

    assert_eq!(live.restyle("{ this is not a style"), Status::BadStyle);
    let after = live.settle(3);
    assert_eq!(
        after,
        Counted::default(),
        "a refused style still replaced something: {after:?}"
    );
}
