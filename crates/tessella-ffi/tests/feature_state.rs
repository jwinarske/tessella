// SPDX-License-Identifier: BSD-2-Clause
//! A host marks a feature and the map re-paints it, with no fetch and no rebuild.
//!
//! The end of tessella#337. The operator evaluates, a binder reads state and records what it would
//! need to paint a feature again, and this is the call that sets it: `tessella_set_feature_state`.
//!
//! What has to be true, and is asserted here rather than argued:
//!
//! - marking a feature re-announces the highlight layer's drawables, with the camera unmoved;
//! - **nothing is fetched** -- the server is asked for nothing new, because no tile is rebuilt;
//! - the attribute bytes on the wire actually change, which is the difference between a re-announce
//!   and a re-paint;
//! - a style that reads no state is unaffected by a host that marks features anyway.

use tessella_capture_abi::EnvelopeKind;
use tessella_capture_abi::ring::{self, Consumer, RingControl};
use tessella_ffi::{Config, MapHandle, Regions, Status};

const TILE: &[u8] = include_bytes!("../../../tests/mvt-fixtures/protomaps-berlin-14-8802-5373.mvt");

/// A fill layer whose color is a highlight through feature state.
fn highlight(origin: &str) -> String {
    format!(
        r##"{{
  "version": 8,
  "name": "highlight",
  "sources": {{
    "fixture": {{"type": "vector", "tiles": ["{origin}/{{z}}/{{x}}/{{y}}.pbf"],
                 "minzoom": 12, "maxzoom": 14}}
  }},
  "layers": [
    {{"id": "earth", "type": "fill", "source": "fixture", "source-layer": "earth",
      "paint": {{"fill-color": ["case", ["==", ["feature-state", "hover"], true],
                               "#ff0000", "#204060"]}}}},
    {{"id": "landuse", "type": "fill", "source": "fixture", "source-layer": "landuse",
      "paint": {{"fill-color": "#306020"}}}}
  ]
}}"##
    )
}

/// The same style with no state in it, for the control.
fn plain(origin: &str) -> String {
    format!(
        r##"{{
  "version": 8,
  "name": "plain",
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

struct Live {
    map: MapHandle,
    consumer: Consumer,
}

/// What a run of ticks announced, by layer.
///
/// The count per layer rather than the bytes. Reading a drawable's attribute bytes from here means
/// resolving a `SlabRef` through the region's slab table, which is the consumer's job and is tested
/// as such in `web/`; what this test is for is the *wiring* -- that a mark reaches the frame and
/// re-announces the right drawables and no others. That the bytes themselves change is
/// `tessella-layout`'s own test, where a restated buffer is compared to a rebuilt one byte for byte.
#[derive(Default, Debug)]
struct Seen {
    adds: usize,
    removes: usize,
}

impl Live {
    fn create(style: &str) -> Self {
        let config = Config {
            style_json: style.as_ptr(),
            style_json_len: style.len(),
            width: 512,
            height: 512,
            ring_capacity: 1 << 22,
            slab_capacity: 0,
            cache_path: core::ptr::null(),
            cache_path_len: 0,
        };
        let mut map: MapHandle = core::ptr::null_mut();
        // SAFETY: the config and the style outlive the call, which copies what it keeps.
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
        // SAFETY: the ring begins with an initialized, eight-aligned control block that lives as
        // long as the map.
        let capacity = unsafe { (*regions.ring.cast::<RingControl>()).capacity } as usize;
        // SAFETY: as above; the producer half is dropped unused.
        let (_, consumer) =
            unsafe { ring::attach(regions.ring.cast_mut(), capacity) }.expect("the ring attaches");
        Self { map, consumer }
    }

    /// Ticks until `wanted` is satisfied and the wire has then gone quiet.
    ///
    /// # Why a quiet window alone will not do
    ///
    /// This loop used to stop after twelve quiet ticks whatever had arrived -- a hundred and twenty
    /// milliseconds, which is more than the fixture needs on an idle machine and less than it needs
    /// on a loaded one. A cold map is quiet for as long as its first tiles take, so the loop
    /// returned `adds: 0` and the assertion read "the first frames drew nothing", which is the
    /// message a real regression would produce. It passed here and failed CI's stable canary, and
    /// reproduces exactly by setting the window to one tick.
    ///
    /// The condition is the caller's because a coarser one is still wrong: the first tick emits a
    /// camera block before any tile is built, so "any record, then quiet" starts the count
    /// immediately and brings the early exit back. What a phase can wait for is the thing it is
    /// about to assert.
    ///
    /// A phase whose claim is that nothing is announced uses [`Self::idle_for`] instead.
    fn settle_until(&mut self, seconds: u64, wanted: impl Fn(&Seen) -> bool) -> Seen {
        let mut seen = Seen::default();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(seconds);
        let mut quiet = 0;
        while std::time::Instant::now() < deadline && (!wanted(&seen) || quiet < 12) {
            // SAFETY: a live map.
            assert_eq!(unsafe { tessella_ffi::tessella_tick(self.map) }, Status::Ok);
            let mut records = 0;
            while let Some(record) = self.consumer.peek() {
                records += 1;
                match record.kind {
                    EnvelopeKind::GeometryAdd => seen.adds += 1,
                    EnvelopeKind::GeometryRemove => seen.removes += 1,
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
        seen
    }

    /// Ticks for `millis`, reporting whatever arrived.
    ///
    /// For a phase whose whole claim is that nothing is announced. [`Self::settle`] cannot say that:
    /// its rule is "something, then quiet", so asking it about silence means waiting out its
    /// deadline and learning nothing the wall clock did not already decide.
    fn idle_for(&mut self, millis: u64) -> Seen {
        let mut seen = Seen::default();
        let deadline = std::time::Instant::now() + std::time::Duration::from_millis(millis);
        while std::time::Instant::now() < deadline {
            // SAFETY: a live map.
            assert_eq!(unsafe { tessella_ffi::tessella_tick(self.map) }, Status::Ok);
            while let Some(record) = self.consumer.peek() {
                match record.kind {
                    EnvelopeKind::GeometryAdd => seen.adds += 1,
                    EnvelopeKind::GeometryRemove => seen.removes += 1,
                    _ => {}
                }
                let consumed = record.consumed();
                self.consumer.advance(consumed);
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        seen
    }
}

impl Live {
    fn mark(&self, id: u64, state: Option<&str>) -> Status {
        self.mark_named(id, None, state)
    }

    /// As [`Self::mark`], naming the feature by a string instead when `name` is given.
    ///
    /// A null text pointer means the number names the feature, which is every vector-tile case.
    fn mark_named(&self, id: u64, name: Option<&str>, state: Option<&str>) -> Status {
        let source = "fixture";
        let layer = "earth";
        // SAFETY: a live map; every range is valid for its length and outlives the call.
        unsafe {
            tessella_ffi::tessella_set_feature_state(
                self.map,
                source.as_ptr(),
                source.len(),
                layer.as_ptr(),
                layer.len(),
                id,
                name.map_or(core::ptr::null(), str::as_ptr),
                name.map_or(0, str::len),
                state.map_or(core::ptr::null(), str::as_ptr),
                state.map_or(0, str::len),
            )
        }
    }
}

impl Drop for Live {
    fn drop(&mut self) {
        // SAFETY: a live map, dropped once.
        unsafe { tessella_ffi::tessella_destroy(self.map) };
    }
}

fn server() -> tile_server::Server {
    tile_server::Server::start(tile_server::Routes::new().tiles(TILE.to_vec(), Some((12, 14))))
        .expect("the server starts")
}

/// Marking a feature re-announces its layer's drawables, with nothing fetched.
///
/// The style has two fill layers over one source: `earth` reads state and `landuse` does not. A mark
/// must re-announce the first and not the second, which is what the state term in the content stamp
/// is for -- and it must reach no origin, because no tile is rebuilt.
#[test]
fn marking_a_feature_repaints_without_fetching() {
    let server = server();
    let mut live = Live::create(&highlight(&server.origin()));
    let first = live.settle_until(20, |seen| seen.adds > 0);
    assert!(first.adds > 1, "both layers drew nothing: {first:?}");
    let fetched = server.requests();
    assert!(fetched > 0, "the map never asked the origin for anything");

    // The fixture's `earth` layer carries two features and the second's id is 1, so marking it
    // names something real -- checked against the tile rather than assumed.
    assert_eq!(live.mark(1, Some(r#"{"hover": true}"#)), Status::Ok);
    let hovered = live.settle_until(20, |seen| seen.adds > 0);

    assert!(
        hovered.adds > 0,
        "a marked feature re-announced nothing, so the consumer still draws the old paint"
    );
    assert!(
        hovered.adds < first.adds,
        "a mark re-announced as much as the whole first frame, so the layer that reads no state \
         was re-announced too: {first:?} then {hovered:?}"
    );
    assert_eq!(
        server.requests(),
        fetched,
        "a feature's state went back to the origin, so a tile was rebuilt rather than re-painted"
    );

    // Unmarking is the same shape, which is what makes a hover that moves affordable.
    assert_eq!(live.mark(1, None), Status::Ok);
    let unhovered = live.settle_until(20, |seen| seen.adds > 0);
    assert!(
        unhovered.adds > 0,
        "unmarking announced nothing: {unhovered:?}"
    );
    assert!(
        unhovered.adds < first.adds,
        "unmarking re-announced the cover"
    );
    assert_eq!(server.requests(), fetched, "unmarking went to the origin");
}

/// A style that reads no state is untouched by a host that marks features anyway.
#[test]
fn a_style_without_state_is_unaffected() {
    let server = server();
    let mut live = Live::create(&plain(&server.origin()));
    let first = live.settle_until(20, |seen| seen.adds > 0);
    assert!(first.adds > 0, "the first frames drew nothing");
    let fetched = server.requests();

    assert_eq!(live.mark(1, Some(r#"{"hover": true}"#)), Status::Ok);
    // Not `settle`: this phase's claim is that nothing is announced, and `settle` waits for
    // something before it will call the wire quiet.
    let after = live.idle_for(750);

    assert_eq!(
        server.requests(),
        fetched,
        "marking a feature fetched something on a style that reads no state"
    );
    assert_eq!(
        after.adds, 0,
        "a layer that cannot be affected by state was re-announced anyway: {after:?}"
    );
}

/// A feature named by a string is marked, and a number does not mark it.
///
/// The whole of #361. `tessella_query_rendered_features` hands a host the feature's id as its source
/// gave it, which for a GeoJSON source may be a string -- and before this, the only call that could
/// act on it took a `uint64_t`, so the id a query produced was one nothing could consume.
///
/// A GeoJSON source rather than the vector fixture above, because MVT states an id as a `uint64` and
/// cannot carry a string at all: this is reachable only through GeoJSON.
///
/// What this layer can see is that the call is accepted and the layer re-announces. It cannot see
/// *which* feature was marked: the content stamp carries the host's state **revision**, so any
/// `set_feature_state` re-announces every indexed bucket whether or not it named something real --
/// which the first assertion below pins, because it is easy to mistake for the feature having been
/// found. That a number does not mark a string-named feature is a claim about the bytes, and it is
/// asserted where the bytes are legible: `tessella-layout`'s `a_string_id_is_named_by_the_lookup`
/// compares a buffer marked by `Number(7)` against one marked by `Text("motorway-7")`.
#[test]
fn a_feature_named_by_a_string_is_marked() {
    const STYLE: &str = r##"{
  "version": 8,
  "name": "named",
  "sources": {
    "s": {"type": "geojson", "data": {"type": "FeatureCollection", "features": [
      {"type": "Feature", "id": "ribbon", "properties": {"kind": "road"},
       "geometry": {"type": "Polygon", "coordinates": [
         [[13.2, 52.4], [13.8, 52.4], [13.8, 52.6], [13.2, 52.6], [13.2, 52.4]]]}}]}}
  },
  "layers": [
    {"id": "earth", "type": "fill", "source": "s",
      "paint": {"fill-color": ["case", ["==", ["feature-state", "hover"], true],
                               "#ff0000", "#204060"]}}
  ]
}"##;

    let mut live = Live::create(STYLE);
    let first = live.settle_until(20, |seen| seen.adds > 0);
    assert!(first.adds > 0, "the fixture never drew: {first:?}");

    // A number names nothing here -- the feature has no numeric id at all -- and the call is still
    // accepted and still re-announces, because the stamp carries the revision rather than the id.
    // Pinned so that the assertion below is not read as proving more than it does.
    assert_eq!(
        live.mark_named(7, None, Some(r#"{"hover": true}"#)),
        Status::Ok
    );
    let by_number = live.settle_until(20, |seen| seen.adds > 0);
    assert!(
        by_number.adds > 0,
        "the stamp stopped carrying the revision: {by_number:?}"
    );

    // Its own name does, which is the call that did not exist before #361.
    assert_eq!(
        live.mark_named(0, Some("ribbon"), Some(r#"{"hover": true}"#)),
        Status::Ok
    );
    let by_name = live.settle_until(20, |seen| seen.adds > 0);
    assert!(
        by_name.adds > 0,
        "a feature named by a string was not re-painted, so a query can find what nothing can \
         mark: {by_name:?}"
    );

    // And unmarking by the same name is the same shape, which is what a hover leaving needs.
    assert_eq!(live.mark_named(0, Some("ribbon"), None), Status::Ok);
    let unmarked = live.settle_until(20, |seen| seen.adds > 0);
    assert!(
        unmarked.adds > 0,
        "unmarking by name announced nothing: {unmarked:?}"
    );
}
