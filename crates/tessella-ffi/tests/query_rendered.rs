//! A map created through the C API answers what is drawn under a point.
//!
//! # Why this needs a server
//!
//! The answer is a property of the cover, not of the style: a feature in a tile that has not arrived
//! is not drawn, so it is not returned. A query against a map with no tiles is correct and empty,
//! which is exactly the answer that cannot tell a working query from a broken one. So the tiles have
//! to land, and landing them means an origin.
//!
//! # What would be caught
//!
//! The y axis, which is the one thing about this call that cannot be got right by accident.
//! `screen::from_screen` measures y up from the bottom edge and a host's pointer event measures it
//! down from the top, so the C entry point flips -- as `tessella_screen_to_geo` does. A query that
//! did not flip answers about the point mirrored across the middle of the viewport, which is correct
//! for a tap in the center and wrong everywhere else.
//!
//! So the fixture's feature is deliberately **off center**, and the test asks about both the point
//! and its mirror.

use tessella_ffi::{Config, MapHandle, Status};

/// One fill square, north of the camera's center so that its y is not its own mirror.
///
/// 52.60 against a center of 52.50 at zoom 9: far enough up the screen that a flip is unmistakable,
/// and well inside the viewport.
const DATA: &str = r##"{"type":"FeatureCollection","features":[
  {"type":"Feature","id":4242,"properties":{"kind":"park","name":"Tiergarten"},
   "geometry":{"type":"Polygon","coordinates":[
     [[13.2,52.55],[13.8,52.55],[13.8,52.65],[13.2,52.65],[13.2,52.55]]]}}]}"##;

#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
/// A coordinate inside the square, and the one every test is written around.
///
/// North of the camera's center on purpose: see the module note on the y axis.
const INSIDE: [f64; 2] = [13.5, 52.6];

const WIDTH: f64 = 800.0;
const HEIGHT: f64 = 600.0;

/// The style, over a GeoJSON source so no tile has to be fetched for the geometry.
fn style() -> String {
    format!(
        r##"{{"version":8,"sources":{{"s":{{"type":"geojson","data":{DATA}}}}},
             "layers":[
               {{"id":"bg","type":"background","paint":{{"background-color":"#101418"}}}},
               {{"id":"parks","type":"fill","source":"s","paint":{{"fill-color":"#00ff00"}}}}]}}"##
    )
}

/// A map on that style, ticked until it has drawn.
struct Live {
    map: MapHandle,
}

impl Live {
    fn create(style: &str) -> Self {
        let config = Config {
            style_json: style.as_ptr(),
            style_json_len: style.len(),
            width: WIDTH as u32,
            height: HEIGHT as u32,
            ring_capacity: 1 << 22,
            slab_capacity: 0,
            cache_path: core::ptr::null(),
            cache_path_len: 0,
        };
        let mut map: MapHandle = core::ptr::null_mut();
        // SAFETY: both pointers are valid and the style outlives the call.
        let status = unsafe { tessella_ffi::tessella_create(&config, 52.5, 13.5, 9.0, &mut map) };
        assert_eq!(status, Status::Ok, "the map did not create");
        let live = Self { map };
        // Ticked until the query can answer, which is the condition every test here needs, rather
        // than a count of frames.
        //
        // Two counts were tried and both were wrong. A fixed forty ticks read an empty answer because
        // the *sources* had not resolved; waiting for readiness 2 and then ticking eight more times
        // passed here and failed CI, because resolved is not drawn -- a GeoJSON source is still being
        // cut into tiles after its sources are known. A number of frames is never the thing a test
        // wants; what it wants is the state, and the state is observable.
        let inside = at(INSIDE);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
        let mut drawn = false;
        while std::time::Instant::now() < deadline && !drawn {
            // SAFETY: a live map.
            assert_eq!(unsafe { tessella_ffi::tessella_tick(live.map) }, Status::Ok);
            drawn = live.query(inside, None).contains("Tiergarten");
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        // Asserted here rather than left to each test, so a fixture that stops drawing says so once
        // and plainly instead of four times as "the feature was not named".
        let mut readiness = -1i32;
        let mut reason = [0i8; 256];
        // SAFETY: a live map, and the buffer is sized by its own length.
        unsafe {
            tessella_ffi::tessella_status(
                live.map,
                &mut readiness,
                reason.as_mut_ptr(),
                reason.len(),
            );
        }
        assert!(
            drawn,
            "the square was never drawn in 20s (readiness {readiness}), so no query here can mean \
             anything"
        );
        live
    }

    /// The answer at a point, as the JSON text.
    ///
    /// Two calls, which is the shape the ABI asks for: one with no buffer to learn the length, one
    /// with a buffer that long.
    fn query(&self, point: [f64; 2], layers: Option<&[&str]>) -> String {
        let ids: Vec<*const u8> = layers
            .unwrap_or(&[])
            .iter()
            .map(|name| name.as_ptr())
            .collect();
        let lens: Vec<usize> = layers
            .unwrap_or(&[])
            .iter()
            .map(|name| name.len())
            .collect();
        let (pointers, lengths) = if ids.is_empty() {
            (core::ptr::null(), core::ptr::null())
        } else {
            (ids.as_ptr(), lens.as_ptr())
        };

        let mut needed = 0usize;
        // SAFETY: a live map; the layer arrays are valid for their count; a null buffer with a zero
        // capacity is the sizing call.
        let sizing = unsafe {
            tessella_ffi::tessella_query_rendered_features(
                self.map,
                point[0],
                point[1],
                point[0],
                point[1],
                pointers,
                lengths,
                ids.len(),
                core::ptr::null_mut(),
                0,
                &mut needed,
            )
        };
        assert_eq!(sizing, Status::TooSmall, "the sizing call did not ask");
        assert!(needed > 0, "an empty answer is still a document");

        let mut room = vec![0u8; needed];
        let mut written = 0usize;
        // SAFETY: as above, with a buffer of the reported length.
        let filled = unsafe {
            tessella_ffi::tessella_query_rendered_features(
                self.map,
                point[0],
                point[1],
                point[0],
                point[1],
                pointers,
                lengths,
                ids.len(),
                room.as_mut_ptr(),
                room.len(),
                &mut written,
            )
        };
        assert_eq!(filled, Status::Ok, "a buffer of the reported length failed");
        assert_eq!(written, needed, "the two calls disagreed about the length");
        String::from_utf8(room).expect("the answer is UTF-8")
    }
}

impl Drop for Live {
    fn drop(&mut self) {
        // SAFETY: a live map, dropped once.
        unsafe { tessella_ffi::tessella_destroy(self.map) };
    }
}

/// Where a coordinate is drawn, in the top-down pixels a host hands over.
///
/// `to_screen` answers in the bottom-up convention, so this flips -- which is the same flip the C
/// entry point makes in the other direction, and the reason a test written in one convention and an
/// implementation written in the other would agree about the center of the screen and nothing else.
fn at(coordinate: [f64; 2]) -> [f64; 2] {
    let view = tessella_tile::camera::settled(&tessella_tile::cover::ViewTransform {
        longitude: 13.5,
        latitude: 52.5,
        zoom: 9.0,
        width: WIDTH,
        height: HEIGHT,
        bearing: 0.0,
        pitch: 0.0,
        ground_below: 0.0,
    });
    let bottom_up = tessella_tile::screen::to_screen(&view, coordinate[0], coordinate[1])
        .expect("the coordinate is on screen");
    [bottom_up[0], HEIGHT - bottom_up[1]]
}

/// The point inside the square, and its mirror across the middle of the viewport.
fn inside_and_mirror() -> ([f64; 2], [f64; 2]) {
    let inside = at(INSIDE);
    (inside, [inside[0], HEIGHT - inside[1]])
}

/// A tap on a feature answers a FeatureCollection naming it.
#[test]
fn a_tap_through_c_names_the_feature() {
    let live = Live::create(&style());
    let (inside, _) = inside_and_mirror();

    let answer = live.query(inside, None);
    assert!(
        answer.contains("\"FeatureCollection\""),
        "not a FeatureCollection: {answer}"
    );
    assert!(
        answer.contains("Tiergarten"),
        "the feature under the tap was not named: {answer}"
    );
    assert!(answer.contains("4242"), "its id is absent: {answer}");
    assert!(
        answer.contains("\"layer\":\"parks\""),
        "the layer that drew it is absent: {answer}"
    );
    assert!(
        answer.contains("\"source\":\"s\""),
        "the source is absent: {answer}"
    );
    assert!(
        answer.contains("\"geometry\":null"),
        "a query keeps no geometry, and says so: {answer}"
    );
}

/// The feature is not found at the mirror of where it is drawn.
///
/// The test the y flip exists for. Its premise is asserted, because a fixture whose feature happened
/// to sit in the middle of the screen would pass whichever way the axis ran.
#[test]
fn a_tap_is_not_answered_at_its_mirror() {
    let live = Live::create(&style());
    let (inside, mirrored) = inside_and_mirror();
    assert!(
        (inside[1] - mirrored[1]).abs() > 150.0,
        "the fixture's premise: {inside:?} and its mirror {mirrored:?} are far apart"
    );

    assert!(
        live.query(inside, None).contains("Tiergarten"),
        "the feature is not where it is drawn"
    );
    let at_mirror = live.query(mirrored, None);
    assert!(
        !at_mirror.contains("Tiergarten"),
        "the feature was found at the mirror of where it is drawn, so y is inverted: {at_mirror}"
    );
}

/// Naming a layer keeps only that layer's features.
#[test]
fn naming_a_layer_filters_the_answer() {
    let live = Live::create(&style());
    let (inside, _) = inside_and_mirror();

    assert!(
        live.query(inside, Some(&["parks"])).contains("Tiergarten"),
        "naming the layer that drew it kept nothing"
    );
    let other = live.query(inside, Some(&["absent"]));
    assert!(
        !other.contains("Tiergarten"),
        "naming another layer kept it anyway: {other}"
    );
    assert!(
        other.contains("\"features\":[]"),
        "an empty answer is still a FeatureCollection: {other}"
    );
}

/// A tap off the map is not an error, and not a hit.
#[test]
fn a_tap_outside_the_square_answers_an_empty_collection() {
    let live = Live::create(&style());

    // A corner of the viewport, far from the square.
    let answer = live.query([4.0, HEIGHT - 4.0], None);
    assert!(
        answer.contains("\"features\":[]"),
        "a tap in an empty corner answered something: {answer}"
    );
}
