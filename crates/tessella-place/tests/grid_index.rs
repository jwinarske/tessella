//! The collision grid, against mbgl's own expectations.
//!
//! mbgl's four `GridIndex` tests state exact result *lists*, not just counts, which pins the
//! order elements come back in as well as which ones do. Order matters downstream: placement
//! walks results and stops at the first that blocks, so a differently-ordered index makes a
//! different label win when two overlap.

use tessella_place::grid::{Bounds, Circle, GridIndex};

fn bounds(min: (f32, f32), max: (f32, f32)) -> Bounds {
    Bounds::new(min, max)
}

/// mbgl `GridIndex.IndexesFeatures`.
#[test]
fn indexes_features() {
    let mut grid: GridIndex<i16> = GridIndex::new(100.0, 100.0, 10);
    grid.insert_box(0, bounds((4.0, 10.0), (6.0, 30.0)));
    grid.insert_box(1, bounds((4.0, 10.0), (30.0, 12.0)));
    grid.insert_box(2, bounds((-10.0, 30.0), (5.0, 35.0)));

    assert_eq!(grid.query_box(bounds((4.0, 10.0), (5.0, 11.0))), [0, 1]);
    assert_eq!(grid.query_box(bounds((24.0, 10.0), (25.0, 11.0))), [1]);
    assert_eq!(
        grid.query_box(bounds((40.0, 40.0), (100.0, 100.0))),
        Vec::<i16>::new()
    );
    assert_eq!(grid.query_box(bounds((-6.0, 0.0), (3.0, 100.0))), [2]);
    assert_eq!(
        grid.query_box(bounds((-1000.0, -1000.0), (1000.0, 1000.0))),
        [0, 1, 2]
    );
}

/// mbgl `GridIndex.DuplicateKeys`: the value is not a key, and three insertions are three
/// entries.
///
/// A grid that deduplicated by value would collapse the three collision boxes of one label into
/// one, and the label would stop colliding along most of its length.
#[test]
fn duplicate_keys_are_three_entries() {
    let mut grid: GridIndex<i16> = GridIndex::new(100.0, 100.0, 10);
    grid.insert_box(123, bounds((3.0, 4.0), (4.0, 4.0)));
    grid.insert_box(123, bounds((13.0, 13.0), (14.0, 14.0)));
    grid.insert_box(123, bounds((23.0, 23.0), (24.0, 24.0)));

    assert_eq!(
        grid.query_box(bounds((0.0, 0.0), (30.0, 30.0))),
        [123, 123, 123]
    );
}

/// mbgl `GridIndex.CircleCircle`.
#[test]
fn circles_hit_circles() {
    let mut grid: GridIndex<i16> = GridIndex::new(100.0, 100.0, 10);
    grid.insert_circle(0, Circle::new((50.0, 50.0), 10.0));
    grid.insert_circle(1, Circle::new((60.0, 60.0), 15.0));
    grid.insert_circle(2, Circle::new((-10.0, 110.0), 20.0));

    assert!(grid.hit_test_circle(Circle::new((55.0, 55.0), 2.0)));
    assert!(!grid.hit_test_circle(Circle::new((10.0, 10.0), 10.0)));
    assert!(grid.hit_test_circle(Circle::new((0.0, 100.0), 10.0)));
    assert!(grid.hit_test_circle(Circle::new((80.0, 60.0), 10.0)));
}

/// mbgl `GridIndex.CircleBox`.
#[test]
fn boxes_find_circles() {
    let mut grid: GridIndex<i16> = GridIndex::new(100.0, 100.0, 10);
    grid.insert_circle(0, Circle::new((50.0, 50.0), 10.0));
    grid.insert_circle(1, Circle::new((60.0, 60.0), 15.0));
    grid.insert_circle(2, Circle::new((-10.0, 110.0), 20.0));

    assert_eq!(grid.query_box(bounds((45.0, 45.0), (55.0, 55.0))), [0, 1]);
    assert_eq!(
        grid.query_box(bounds((0.0, 0.0), (30.0, 30.0))),
        Vec::<i16>::new()
    );
    assert_eq!(grid.query_box(bounds((0.0, 80.0), (20.0, 100.0))), [2]);
}

/// mbgl `GridIndex.IndexesFeaturesOverflow`: a grid far larger than a viewport still indexes.
#[test]
fn a_large_grid_indexes() {
    let mut grid: GridIndex<i16> = GridIndex::new(5000.0, 5000.0, 25);
    grid.insert_box(0, bounds((4500.0, 4500.0), (4900.0, 4900.0)));
    assert_eq!(
        grid.query_box(bounds((4000.0, 4000.0), (5000.0, 5000.0))),
        [0]
    );
}

/// A shape spanning many cells is reported once, not once per cell.
///
/// The dedup the walk does. Without it a long label's box would come back as many copies as it
/// has cells, and placement would do that much redundant work per candidate.
#[test]
fn a_shape_spanning_cells_is_reported_once() {
    let mut grid: GridIndex<i16> = GridIndex::new(100.0, 100.0, 10);
    // Across ten columns and ten rows: a hundred cells.
    grid.insert_box(7, bounds((0.0, 0.0), (99.0, 99.0)));

    // A query covering most of the grid but not all of it, so the cell walk runs rather than
    // the whole-grid shortcut.
    assert_eq!(grid.query_box(bounds((1.0, 1.0), (98.0, 98.0))), [7]);
}

/// A query covering the whole grid reports everything exactly once.
///
/// mbgl's box query returns after its whole-grid shortcut and its circle query does not, so the
/// circle version reports every element twice. Nothing catches it there because the only caller
/// reaching that path stops at the first result. This asserts the counts.
#[test]
fn a_whole_grid_query_does_not_double_report() {
    let mut grid: GridIndex<i16> = GridIndex::new(100.0, 100.0, 10);
    grid.insert_box(0, bounds((10.0, 10.0), (20.0, 20.0)));
    grid.insert_circle(1, Circle::new((50.0, 50.0), 5.0));

    let by_box = grid.query_box(bounds((-1000.0, -1000.0), (1000.0, 1000.0)));
    assert_eq!(by_box, [0, 1], "each element once");

    let by_circle = grid.query_circle(Circle::new((50.0, 50.0), 1000.0));
    assert_eq!(by_circle, [0, 1], "each element once here too");
}

/// Nothing is found off the grid.
#[test]
fn a_query_that_misses_the_grid_finds_nothing() {
    let mut grid: GridIndex<i16> = GridIndex::new(100.0, 100.0, 10);
    grid.insert_box(0, bounds((10.0, 10.0), (20.0, 20.0)));

    assert!(
        grid.query_box(bounds((-50.0, -50.0), (-1.0, -1.0)))
            .is_empty()
    );
    assert!(
        grid.query_box(bounds((200.0, 200.0), (300.0, 300.0)))
            .is_empty()
    );
    assert!(!grid.hit_test_box(bounds((200.0, 200.0), (300.0, 300.0))));
}

/// Touching boxes collide and touching circles do not.
///
/// mbgl's asymmetry, transcribed rather than tidied: its box test is inclusive at the edges and
/// its circle test is strict. Placement's output depends on it, and evening it up would move
/// labels for a reason no oracle would explain.
#[test]
fn touching_boxes_collide_and_touching_circles_do_not() {
    let mut boxes: GridIndex<i16> = GridIndex::new(100.0, 100.0, 10);
    boxes.insert_box(0, bounds((10.0, 10.0), (20.0, 20.0)));
    // Shares exactly the edge at x = 20.
    assert!(boxes.hit_test_box(bounds((20.0, 10.0), (30.0, 20.0))));

    let mut circles: GridIndex<i16> = GridIndex::new(100.0, 100.0, 10);
    circles.insert_circle(0, Circle::new((20.0, 20.0), 5.0));
    // Exactly tangent: the radii sum to the distance.
    assert!(!circles.hit_test_circle(Circle::new((30.0, 20.0), 5.0)));
    // A hair closer, and they do.
    assert!(circles.hit_test_circle(Circle::new((29.9, 20.0), 5.0)));
}

/// A circle meeting a box at a corner is tested against the corner, not the bounding box.
///
/// The last branch of the circle-box test. Without it a circle diagonally outside a box's corner
/// collides with it, and labels keep a gap they do not need at every diagonal neighbour.
#[test]
fn a_circle_near_a_corner_is_tested_against_the_corner() {
    let mut grid: GridIndex<i16> = GridIndex::new(100.0, 100.0, 10);
    grid.insert_box(0, bounds((10.0, 10.0), (20.0, 20.0)));

    // Diagonally beyond the corner at (20, 20): 5 out on each axis is 7.07 away, so a radius
    // of 5 does not reach it even though both axes are within 5 + the half-extent.
    assert!(!grid.hit_test_circle(Circle::new((25.0, 25.0), 5.0)));
    // A radius past the diagonal does.
    assert!(grid.hit_test_circle(Circle::new((25.0, 25.0), 7.5)));
}

/// The grid actually prunes: a query is compared against the shapes near it, not all of them.
///
/// Every assertion above is about *results*, and results cannot see this. A grid whose cells are
/// mis-sized — one cell covering everything — returns exactly the same answers, because every
/// shape becomes a candidate and the exact tests filter them out. It just stops being an index,
/// and placement goes quadratic in a tile's label count at street zoom.
#[test]
fn the_grid_prunes_rather_than_comparing_everything() {
    let mut grid: GridIndex<i16> = GridIndex::new(100.0, 100.0, 10);
    assert_eq!(grid.cells(), (10, 10), "ten cells across, ten down");

    // One small box in the middle of each cell: a hundred shapes, evenly spread.
    for row in 0..10i16 {
        for column in 0..10i16 {
            let x = f32::from(column) * 10.0 + 5.0;
            let y = f32::from(row) * 10.0 + 5.0;
            grid.insert_box(
                row * 10 + column,
                bounds((x - 1.0, y - 1.0), (x + 1.0, y + 1.0)),
            );
        }
    }
    assert_eq!(grid.len(), 100);

    // A query inside one cell must consider that cell's shape and not the other ninety-nine.
    let candidates = grid.candidates_for_box(bounds((4.0, 4.0), (6.0, 6.0)));
    assert_eq!(
        candidates, 1,
        "a one-cell query considered {candidates} shapes"
    );

    // A query spanning four cells considers four.
    let candidates = grid.candidates_for_box(bounds((8.0, 8.0), (12.0, 12.0)));
    assert_eq!(
        candidates, 4,
        "a four-cell query considered {candidates} shapes"
    );

    // And the whole grid still considers everything, which is the shortcut working.
    assert_eq!(
        grid.candidates_for_box(bounds((0.0, 0.0), (100.0, 100.0))),
        100
    );
}
