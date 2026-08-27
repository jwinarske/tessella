//! The two values a cross-faded property is between, and how far between them a frame is.
//!
//! # What these are checked against
//!
//! mbgl's `ZoomHistory::update`, `PropertyEvaluationParameters::getCrossfadeParameters` and
//! `CrossFadedPropertyEvaluator::calculate`, transcribed. There is no golden dump to diff
//! against: no capture in this tree carries a pattern layer, which is the reason the pattern
//! binders were recorded as *blocked* rather than deferred. So the arithmetic is pinned against
//! the source it came from, and the cases chosen are the ones where a plausible misreading gives
//! a different answer.

use tessella_style::crossfade::{Crossfade, ZoomHistory, crossfade, faded, names_at, tlbr};

/// Crossing an integer zoom is what a history is for, and the two directions differ.
///
/// Zooming *out* past an integer records `floor(z) + 1` — the level just left — where zooming
/// *in* records `floor(z)`, which is again the level just left. Both name the boundary crossed,
/// and reading either as "the level now occupied" gets one of them wrong.
#[test]
fn a_history_records_the_boundary_that_was_crossed() {
    let mut history = ZoomHistory::new();

    // The first update seeds rather than crosses.
    assert!(history.update(14.5, Some(1_000)));
    assert_eq!(history.last_integer_zoom, 14.0);
    assert_eq!(history.last_integer_zoom_time, Some(0), "seeded at zero");

    // Zooming in across fifteen.
    assert!(history.update(15.2, Some(2_000)));
    assert_eq!(history.last_integer_zoom, 15.0);
    assert_eq!(history.last_integer_zoom_time, Some(2_000));

    // Zooming back out across it: the level left is fifteen, not fourteen.
    assert!(history.update(14.8, Some(3_000)));
    assert_eq!(
        history.last_integer_zoom, 15.0,
        "zooming out records floor(z) + 1"
    );
    assert_eq!(history.last_integer_zoom_time, Some(3_000));

    // A repeat of the same zoom is not a change.
    assert!(!history.update(14.8, Some(4_000)));
    assert_eq!(
        history.last_integer_zoom_time,
        Some(3_000),
        "no new crossing"
    );
}

/// Moving within one integer level crosses nothing.
#[test]
fn moving_inside_a_level_starts_no_fade() {
    let mut history = ZoomHistory::new();
    history.update(14.1, Some(1_000));
    let stamped = history.last_integer_zoom_time;

    assert!(history.update(14.9, Some(2_000)), "the zoom did change");
    assert_eq!(history.last_integer_zoom, 14.0);
    assert_eq!(
        history.last_integer_zoom_time, stamped,
        "but nothing crossed"
    );
}

/// The mix, in both directions, and they are not mirror images.
#[test]
fn the_mix_follows_the_direction_of_travel() {
    // Zooming in: above the last integer zoom.
    let mut history = ZoomHistory::new();
    history.update(14.0, Some(0));
    history.update(14.25, Some(0));

    // With the fade already complete, t is one whichever way it was reached.
    let done = crossfade(14.25, &history, Some(10_000), 300);
    assert_eq!(done.from_scale, 2.0, "the level left is twice the size");
    assert_eq!(done.to_scale, 1.0);
    assert!((done.t - 1.0).abs() < 1e-6, "t is {} not 1", done.t);

    // With no time elapsed, t is the zoom's fractional part alone.
    let starting = crossfade(14.25, &history, Some(0), 300);
    assert!(
        (starting.t - 0.25).abs() < 1e-6,
        "zooming in starts at the fraction: {}",
        starting.t
    );

    // Zooming out: at or below the last integer zoom, and the scale inverts.
    let mut history = ZoomHistory::new();
    history.update(15.0, Some(0));
    history.update(14.75, Some(0));
    let out = crossfade(14.75, &history, Some(0), 300);
    assert_eq!(out.from_scale, 0.5, "the level left is half the size");
    // 1 - (1 - 0) * 0.75
    assert!(
        (out.t - 0.25).abs() < 1e-6,
        "zooming out is pulled back by the fraction: {}",
        out.t
    );
}

/// A fade duration of zero leaves the fraction in charge.
#[test]
fn no_fade_duration_means_no_time_term() {
    let mut history = ZoomHistory::new();
    history.update(14.0, Some(0));
    history.update(14.4, Some(0));

    let Crossfade { t, .. } = crossfade(14.4, &history, Some(0), 0);
    assert!((t - 1.0).abs() < 1e-6, "the time term is complete: {t}");
}

/// `to` is the current zoom; `from` is the level being left.
#[test]
fn the_pair_is_the_level_left_and_the_level_entered() {
    let mut history = ZoomHistory::new();
    history.update(14.0, Some(0));
    history.update(14.5, Some(0));

    // An expression that just reports the zoom it was asked about.
    let pair = faded(|z| (z * 10.0).round() as i64, 14.5, &history);
    assert_eq!(pair.to, 145, "to is the current zoom");
    assert_eq!(pair.from, 135, "zooming in, from is a level below");

    let mut history = ZoomHistory::new();
    history.update(15.0, Some(0));
    history.update(14.5, Some(0));
    let pair = faded(|z| (z * 10.0).round() as i64, 14.5, &history);
    assert_eq!(pair.to, 145);
    assert_eq!(pair.from, 155, "zooming out, from is a level above");
}

/// A constant fades between two copies of one value, which is correct rather than wasteful.
#[test]
fn a_constant_pattern_still_has_a_pair() {
    let history = ZoomHistory::new();
    let pair = faded(|_| 7, 14.5, &history);
    assert_eq!(pair.from, 7);
    assert_eq!(pair.to, 7);
}

/// The atlas rectangle is inset by the padding on every side.
///
/// The padded rect is what the packer allocated; the shader samples between these corners, and
/// sampling the padding would pick up whatever sprite was packed beside it.
#[test]
fn a_rectangle_is_inset_by_its_padding() {
    assert_eq!(tlbr(10, 20, 32, 16, 1), [11, 21, 41, 35]);
    // No padding leaves the rectangle as it was.
    assert_eq!(tlbr(10, 20, 32, 16, 0), [10, 20, 42, 36]);
}

/// A tile needs every image the expression could resolve to, not just the current one.
///
/// A pattern that steps at zoom fourteen is asked for at thirteen, fourteen and fifteen, and a
/// tile that fetched only the current answer would have nothing to fade from.
#[test]
fn every_image_the_fade_could_reach_is_named() {
    let stepped = |z: f64| Some(if z >= 14.0 { "large" } else { "small" }.to_owned());
    let names = names_at(stepped, 14.0);
    assert_eq!(names, ["small", "large"], "both sides of the step");

    // Deduplicated, and in the order first seen.
    let constant = |_: f64| Some("one".to_owned());
    assert_eq!(names_at(constant, 14.0), ["one"]);

    // An expression that resolves to nothing contributes nothing.
    let missing = |_: f64| None;
    assert!(names_at(missing, 14.0).is_empty());
}

/// A layer's pattern dependencies, across the three zooms a fade can reach.
mod dependencies {
    use tessella_style::crossfade::pattern_names;
    use tessella_style::{Layer, Style};

    fn layer(paint: &str) -> Layer {
        let style = Style::parse(&format!(
            r#"{{"version": 8, "sources": {{"s": {{"type": "vector", "tiles": []}}}},
                "layers": [{{"id": "l", "type": "fill", "source": "s", "source-layer": "x",
                            "paint": {paint}}}]}}"#
        ))
        .expect("parses");
        style.layers.into_iter().next().expect("one layer")
    }

    /// A pattern that steps with zoom needs both sides of the step.
    #[test]
    fn a_stepped_pattern_needs_both_images() {
        let layer =
            layer(r#"{"fill-pattern": ["step", ["zoom"], "hatch-small", 14, "hatch-large"]}"#);
        let paint = tessella_style::property::resolve_paint(&layer).expect("resolves");
        let names = pattern_names(|name| paint.get(name), 14.0);
        assert!(
            names.contains(&"hatch-small".to_owned()) && names.contains(&"hatch-large".to_owned()),
            "a tile at the step needs both: {names:?}"
        );
    }

    /// A constant pattern is asked for once, not three times.
    #[test]
    fn a_constant_pattern_is_named_once() {
        let layer = layer(r#"{"fill-pattern": "hatch"}"#);
        let paint = tessella_style::property::resolve_paint(&layer).expect("resolves");
        assert_eq!(pattern_names(|name| paint.get(name), 14.0), ["hatch"]);
    }

    /// A layer with no pattern needs no sprite.
    #[test]
    fn no_pattern_is_no_dependency() {
        let layer = layer(r#"{"fill-color": "red"}"#);
        let paint = tessella_style::property::resolve_paint(&layer).expect("resolves");
        assert!(pattern_names(|name| paint.get(name), 14.0).is_empty());
    }

    /// Away from the step, only the reachable neighbour is named.
    #[test]
    fn only_the_reachable_neighbour_is_named() {
        let layer =
            layer(r#"{"fill-pattern": ["step", ["zoom"], "hatch-small", 14, "hatch-large"]}"#);
        let paint = tessella_style::property::resolve_paint(&layer).expect("resolves");
        // At zoom ten, z-1, z and z+1 are all below the step.
        assert_eq!(pattern_names(|name| paint.get(name), 10.0), ["hatch-small"]);
        // At eighteen they are all above it.
        assert_eq!(pattern_names(|name| paint.get(name), 18.0), ["hatch-large"]);
    }
}
