//! The run of circles a line-following label collides as.
//!
//! mbgl's `bboxifyLabel`. What makes it worth testing rather than eyeballing is that every
//! failure mode is quiet: a run that is too short lets labels overlap, one that is too long
//! blanks a street tile, and one that is offset along the line reserves the wrong road.

use tessella_place::feature::line_circles;

/// A straight line running east at y = 100.
fn straight() -> Vec<(f32, f32)> {
    (0..=20i16)
        .map(|index| (f32::from(index) * 20.0, 100.0))
        .collect()
}

/// A hundred-unit label, twenty tall, anchored in the middle of the line.
const LENGTH: f32 = 100.0;
const HEIGHT: f32 = 20.0;

fn run() -> Vec<tessella_place::feature::LineCircle> {
    line_circles(&straight(), (200.0, 100.0), 10, LENGTH, HEIGHT, 1.0)
}

/// The circles follow the line and cover the label.
#[test]
fn the_circles_cover_the_label() {
    let circles = run();
    assert!(!circles.is_empty());

    // Every one is on the line, and their radius is half the label's height.
    for entry in &circles {
        assert!(
            (entry.circle.center.1 - 100.0).abs() < 0.01,
            "{entry:?} is off the line"
        );
        assert!(
            (entry.circle.radius - HEIGHT / 2.0).abs() < 0.01,
            "{entry:?}"
        );
    }

    // The label spans 150..250. Circles step by half a box, so consecutive ones overlap, which
    // is what makes the run a covering rather than a dotted line with gaps between the dots.
    let centers: Vec<f32> = circles.iter().map(|entry| entry.circle.center.0).collect();
    for pair in centers.windows(2) {
        let step = pair[1] - pair[0];
        assert!(
            step > 0.0 && step <= HEIGHT,
            "{centers:?} has a gap of {step}"
        );
    }

    // And the run reaches both ends of the label.
    let covered: Vec<f32> = centers
        .iter()
        .copied()
        .filter(|x| (150.0..=250.0).contains(x))
        .collect();
    assert!(
        covered.first().expect("some") - 150.0 < HEIGHT,
        "{centers:?}"
    );
    assert!(
        250.0 - covered.last().expect("some") < HEIGHT,
        "{centers:?}"
    );
}

/// The run extends past the end of the label.
///
/// Not slack: a pitched camera draws a distant label *larger* than the box it was laid out for,
/// and a label that has outgrown its collision shape overlaps its neighbour with nothing
/// detecting it.
#[test]
fn the_run_extends_past_the_label() {
    let centers: Vec<f32> = run().iter().map(|entry| entry.circle.center.0).collect();
    assert!(
        *centers.last().expect("some") > 250.0,
        "{centers:?} ends at the label"
    );

    // And they thin out as they go, because the padding circles are spread further apart than
    // the label's own -- a short run of them covers the distance a pitched label grows by.
    let steps: Vec<f32> = centers.windows(2).map(|pair| pair[1] - pair[0]).collect();
    assert!(
        steps.last().expect("some") > steps.first().expect("some"),
        "{steps:?} does not spread"
    );
}

/// The padding *before* the label depends on where the line's vertices fall.
///
/// A quirk of mbgl's, transcribed rather than tidied. The walk backwards stops at the first
/// vertex at or before the label's start, and a leading padding circle is skipped when it falls
/// before that vertex -- so on a finely divided line the walk stops right at the label and the
/// leading padding is dropped, while on a coarse one it overshoots and the padding survives.
/// mbgl's own comment on the skip says it "could allow for line collisions on distant tiles".
///
/// It is asserted because it is the kind of asymmetry a later reader corrects on sight.
#[test]
fn the_leading_padding_depends_on_the_vertices() {
    // Vertices every twenty units: the walk lands exactly on the label's start.
    let fine: Vec<f32> = run().iter().map(|entry| entry.circle.center.0).collect();
    assert!(
        fine.first().expect("some") >= &150.0,
        "{fine:?} has leading padding on a finely divided line"
    );

    // One long segment: the walk back overshoots, and the padding fits.
    let coarse_line = vec![(0.0, 100.0), (400.0, 100.0)];
    let coarse: Vec<f32> = line_circles(&coarse_line, (200.0, 100.0), 0, LENGTH, HEIGHT, 1.0)
        .iter()
        .map(|entry| entry.circle.center.0)
        .collect();
    assert!(
        coarse.first().expect("some") < &150.0,
        "{coarse:?} has no leading padding on a coarse line"
    );
}

/// An overscaled tile gets a longer padding run, and only the padding grows.
#[test]
fn overscaling_widens_only_the_padding() {
    let plain = run();
    let overscaled = line_circles(&straight(), (200.0, 100.0), 10, LENGTH, HEIGHT, 4.0);
    assert!(
        overscaled.len() > plain.len(),
        "{} then {}",
        plain.len(),
        overscaled.len()
    );

    // The circles covering the label itself are unchanged; the extra ones are outside it.
    let over_label = |run: &[tessella_place::feature::LineCircle]| {
        run.iter()
            .filter(|entry| (150.0..=250.0).contains(&entry.circle.center.0))
            .count()
    };
    assert_eq!(over_label(&plain), over_label(&overscaled));
}

/// The distance from the anchor grows outwards and is padded down by a fifth.
#[test]
fn the_distance_is_signed_and_padded() {
    let circles = run();

    // It is signed: circles before the anchor are at negative distances, after it positive.
    assert!(
        circles.iter().any(|entry| entry.distance_from_anchor < 0.0),
        "{circles:?} has nothing before the anchor"
    );
    assert!(
        circles.iter().any(|entry| entry.distance_from_anchor > 0.0),
        "{circles:?} has nothing after it"
    );

    // And it is four fifths of the real distance rather than all of it -- the conservative
    // padding that keeps a circle near the edge of a test inside it.
    for entry in &circles {
        if entry.distance_from_anchor == 0.0 {
            continue;
        }
        let real = entry.circle.center.0 - 200.0;
        let ratio = entry.distance_from_anchor / real;
        assert!(
            (ratio - 0.8).abs() < 0.05,
            "{entry:?} is {ratio} of its distance, not four fifths"
        );
    }
}

/// A bend puts the circles on the bend rather than through it.
///
/// The reason a line label is circles at all. A single box over a right-angled corner reserves
/// the whole square, including the streets in it that the label does not touch.
#[test]
fn the_circles_follow_a_bend() {
    // East to (200, 100), then south.
    let corner = vec![(0.0, 100.0), (200.0, 100.0), (200.0, 400.0)];
    let circles = line_circles(&corner, (180.0, 100.0), 0, LENGTH, HEIGHT, 1.0);
    assert!(!circles.is_empty());

    let past = circles
        .iter()
        .filter(|entry| entry.circle.center.1 > 110.0)
        .count();
    assert!(past > 0, "nothing turned the corner: {circles:?}");

    // And nothing is off the two runs -- a circle at (250, 200) would be inside the box a single
    // rectangle would have reserved, and on neither arm of the road.
    for entry in &circles {
        let on_horizontal = (entry.circle.center.1 - 100.0).abs() < 0.01;
        let on_vertical = (entry.circle.center.0 - 200.0).abs() < 0.01;
        assert!(on_horizontal || on_vertical, "{entry:?} is off the line");
    }
}

/// A line that ends mid-run stops there rather than running off the end.
#[test]
fn a_short_line_stops_at_its_end() {
    let stub = vec![(150.0, 100.0), (250.0, 100.0)];
    let circles = line_circles(&stub, (200.0, 100.0), 0, LENGTH, HEIGHT, 1.0);

    assert!(!circles.is_empty(), "the label's own length does fit");
    for entry in &circles {
        assert!(
            (150.0..=250.0).contains(&entry.circle.center.0),
            "{entry:?} is past the end of the line"
        );
    }
}

/// A zero-height label reserves nothing.
///
/// The step is half the height, so a zero height is a zero step and the run would not terminate.
#[test]
fn a_zero_height_label_reserves_nothing() {
    assert!(line_circles(&straight(), (200.0, 100.0), 10, LENGTH, 0.0, 1.0).is_empty());
}

/// A zero-width label still reserves one circle.
///
/// mbgl forces it, and the reason is that a label with no width is not a label with no presence:
/// it still has height, and something has to stop another label being placed on top of it.
#[test]
fn a_zero_width_label_still_reserves_one() {
    let circles = line_circles(&straight(), (200.0, 100.0), 10, 0.0, HEIGHT, 1.0);
    assert_eq!(circles.len(), 1, "a zero-width label reserved {circles:?}");
    assert!(
        (circles[0].circle.center.0 - 200.0).abs() <= HEIGHT,
        "{circles:?} is not on the anchor"
    );
}

/// A degenerate line is refused rather than divided by zero.
#[test]
fn a_degenerate_line_is_refused() {
    assert!(line_circles(&[], (0.0, 0.0), 0, LENGTH, HEIGHT, 1.0).is_empty());
    assert!(line_circles(&[(1.0, 1.0)], (1.0, 1.0), 0, LENGTH, HEIGHT, 1.0).is_empty());
}
