//! A spatial index for collision testing: boxes and circles in a plane.
//!
//! A transcription of mbgl's `GridIndex`. Placement asks "does this label overlap anything
//! already placed" once per candidate, and a label can be a box or a run of circles following a
//! line. Comparing every candidate against every placed symbol is quadratic in a tile's label
//! count, which at street zoom is thousands.
//!
//! The plane is cut into cells; each shape records itself in every cell it touches; a query
//! compares only against shapes sharing a cell with it. That is the whole idea, and it works
//! because labels are spread over a viewport rather than piled at a point.
//!
//! # Boxes and circles are indexed separately, and that is not an implementation detail
//!
//! A box-box test is four comparisons; a circle-box test is a distance. Keeping them apart lets
//! each query run the cheap test where it can, and it is why the two collections are visible in
//! the API rather than merged behind one shape type.
//!
//! # Two deliberate differences from mbgl
//!
//! Its box-box test is inclusive at the edges and its circle-circle test is strict, so two boxes
//! that merely touch collide and two circles that merely touch do not. That asymmetry is
//! transcribed rather than tidied: placement's output depends on it, and "fixing" it would move
//! labels for reasons no oracle would explain.
//!
//! Its circle query is not transcribed exactly. mbgl's `query(BCircle)` returns early when the
//! query misses the grid, and for a query covering the *whole* grid it reports every element and
//! then falls through into the cell walk and reports them again — the box version has a `return`
//! there and the circle version does not. Nothing catches it because the only caller that
//! reaches that path is a hit test, which stops at the first result. Here the branch returns.

use std::collections::BTreeSet;

/// An axis-aligned rectangle.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Bounds {
    /// Minimum corner.
    pub min: (f32, f32),
    /// Maximum corner.
    pub max: (f32, f32),
}

impl Bounds {
    /// A rectangle from its corners.
    #[must_use]
    pub const fn new(min: (f32, f32), max: (f32, f32)) -> Self {
        Self { min, max }
    }
}

/// A circle.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Circle {
    /// Centre.
    pub center: (f32, f32),
    /// Radius.
    pub radius: f32,
}

impl Circle {
    /// A circle from its centre and radius.
    #[must_use]
    pub const fn new(center: (f32, f32), radius: f32) -> Self {
        Self { center, radius }
    }

    /// The rectangle that contains it.
    #[must_use]
    pub const fn bounds(self) -> Bounds {
        Bounds {
            min: (self.center.0 - self.radius, self.center.1 - self.radius),
            max: (self.center.0 + self.radius, self.center.1 + self.radius),
        }
    }
}

/// Whether two rectangles overlap, touching included.
#[must_use]
pub fn boxes_collide(first: Bounds, second: Bounds) -> bool {
    first.min.0 <= second.max.0
        && first.min.1 <= second.max.1
        && first.max.0 >= second.min.0
        && first.max.1 >= second.min.1
}

/// Whether two circles overlap, touching excluded.
///
/// Strict where [`boxes_collide`] is inclusive, which is mbgl's asymmetry and not a slip here.
#[must_use]
pub fn circles_collide(first: Circle, second: Circle) -> bool {
    let dx = second.center.0 - first.center.0;
    let dy = second.center.1 - first.center.1;
    let both = first.radius + second.radius;
    both * both > dx * dx + dy * dy
}

/// Whether a circle overlaps a rectangle.
#[must_use]
pub fn circle_and_box_collide(circle: Circle, bounds: Bounds) -> bool {
    let half_width = (bounds.max.0 - bounds.min.0) / 2.0;
    let dist_x = (circle.center.0 - (bounds.min.0 + half_width)).abs();
    if dist_x > half_width + circle.radius {
        return false;
    }
    let half_height = (bounds.max.1 - bounds.min.1) / 2.0;
    let dist_y = (circle.center.1 - (bounds.min.1 + half_height)).abs();
    if dist_y > half_height + circle.radius {
        return false;
    }
    // Inside the rectangle's cross, so the nearest point is on an edge rather than a corner.
    if dist_x <= half_width || dist_y <= half_height {
        return true;
    }
    let dx = dist_x - half_width;
    let dy = dist_y - half_height;
    dx * dx + dy * dy <= circle.radius * circle.radius
}

/// Boxes and circles in a plane, indexed by cell.
#[derive(Debug)]
pub struct GridIndex<T> {
    width: f32,
    height: f32,
    x_cells: usize,
    y_cells: usize,
    x_scale: f64,
    y_scale: f64,
    boxes: Vec<(T, Bounds)>,
    circles: Vec<(T, Circle)>,
    box_cells: Vec<Vec<u32>>,
    circle_cells: Vec<Vec<u32>>,
}

impl<T: Clone> GridIndex<T> {
    /// An empty index covering `width` by `height` in cells of `cell_size`.
    ///
    /// # Panics
    ///
    /// When the extent or the cell size is not positive: a zero-width grid has no cells to put
    /// anything in, and every insertion would silently do nothing.
    #[must_use]
    pub fn new(width: f32, height: f32, cell_size: u32) -> Self {
        assert!(width > 0.0 && height > 0.0, "a grid needs an extent");
        assert!(cell_size > 0, "a grid needs cells");
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let x_cells = (width / cell_size as f32).ceil() as usize;
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let y_cells = (height / cell_size as f32).ceil() as usize;
        Self {
            width,
            height,
            x_cells,
            y_cells,
            x_scale: f64::from(x_cells as u32) / f64::from(width),
            y_scale: f64::from(y_cells as u32) / f64::from(height),
            boxes: Vec::new(),
            circles: Vec::new(),
            box_cells: vec![Vec::new(); x_cells * y_cells],
            circle_cells: vec![Vec::new(); x_cells * y_cells],
        }
    }

    /// The cell column holding `x`, clamped to the grid.
    ///
    /// Clamped rather than refused: a label may hang off the edge of the viewport and still
    /// collide with one that does not, so a shape outside the grid is indexed against its
    /// nearest cell rather than dropped.
    fn x_cell(&self, x: f32) -> usize {
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let cell = (f64::from(x) * self.x_scale).floor();
        cell.clamp(0.0, self.x_cells as f64 - 1.0) as usize
    }

    fn y_cell(&self, y: f32) -> usize {
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let cell = (f64::from(y) * self.y_scale).floor();
        cell.clamp(0.0, self.y_cells as f64 - 1.0) as usize
    }

    /// Adds a rectangle.
    pub fn insert_box(&mut self, value: T, bounds: Bounds) {
        #[allow(clippy::cast_possible_truncation)]
        let uid = self.boxes.len() as u32;
        let (x1, y1) = (self.x_cell(bounds.min.0), self.y_cell(bounds.min.1));
        let (x2, y2) = (self.x_cell(bounds.max.0), self.y_cell(bounds.max.1));
        for x in x1..=x2 {
            for y in y1..=y2 {
                self.box_cells[self.x_cells * y + x].push(uid);
            }
        }
        self.boxes.push((value, bounds));
    }

    /// Adds a circle.
    pub fn insert_circle(&mut self, value: T, circle: Circle) {
        #[allow(clippy::cast_possible_truncation)]
        let uid = self.circles.len() as u32;
        let bounds = circle.bounds();
        let (x1, y1) = (self.x_cell(bounds.min.0), self.y_cell(bounds.min.1));
        let (x2, y2) = (self.x_cell(bounds.max.0), self.y_cell(bounds.max.1));
        for x in x1..=x2 {
            for y in y1..=y2 {
                self.circle_cells[self.x_cells * y + x].push(uid);
            }
        }
        self.circles.push((value, circle));
    }

    /// Whether the query is entirely off the grid.
    fn misses(&self, query: Bounds) -> bool {
        query.max.0 < 0.0
            || query.min.0 >= self.width
            || query.max.1 < 0.0
            || query.min.1 >= self.height
    }

    /// Whether the query covers the whole grid, so every element is a candidate.
    fn covers(&self, query: Bounds) -> bool {
        query.min.0 <= 0.0
            && query.min.1 <= 0.0
            && self.width <= query.max.0
            && self.height <= query.max.1
    }

    /// Walks candidates, stopping when `visit` returns true.
    ///
    /// `test_box` and `test_circle` decide whether a candidate actually collides; the walk only
    /// decides which candidates are worth testing.
    fn walk(
        &self,
        query: Bounds,
        test_box: &dyn Fn(Bounds) -> bool,
        test_circle: &dyn Fn(Circle) -> bool,
        visit: &mut dyn FnMut(&T) -> bool,
    ) {
        if self.misses(query) {
            return;
        }
        if self.covers(query) {
            for (value, _) in &self.boxes {
                if visit(value) {
                    return;
                }
            }
            for (value, _) in &self.circles {
                if visit(value) {
                    return;
                }
            }
            return;
        }

        let mut seen_boxes: BTreeSet<u32> = BTreeSet::new();
        let mut seen_circles: BTreeSet<u32> = BTreeSet::new();
        let (x1, y1) = (self.x_cell(query.min.0), self.y_cell(query.min.1));
        let (x2, y2) = (self.x_cell(query.max.0), self.y_cell(query.max.1));

        for x in x1..=x2 {
            for y in y1..=y2 {
                let cell = self.x_cells * y + x;
                for uid in &self.box_cells[cell] {
                    // Marked seen whether or not it collides: a shape spanning four cells is
                    // tested once, which is most of what the index saves.
                    if !seen_boxes.insert(*uid) {
                        continue;
                    }
                    let (value, bounds) = &self.boxes[*uid as usize];
                    if test_box(*bounds) && visit(value) {
                        return;
                    }
                }
                for uid in &self.circle_cells[cell] {
                    if !seen_circles.insert(*uid) {
                        continue;
                    }
                    let (value, circle) = &self.circles[*uid as usize];
                    if test_circle(*circle) && visit(value) {
                        return;
                    }
                }
            }
        }
    }

    /// Everything overlapping a rectangle.
    #[must_use]
    pub fn query_box(&self, query: Bounds) -> Vec<T> {
        let mut out = Vec::new();
        self.walk(
            query,
            &|bounds| boxes_collide(query, bounds),
            &|circle| circle_and_box_collide(circle, query),
            &mut |value| {
                out.push(value.clone());
                false
            },
        );
        out
    }

    /// Everything overlapping a circle.
    #[must_use]
    pub fn query_circle(&self, query: Circle) -> Vec<T> {
        let mut out = Vec::new();
        self.walk(
            query.bounds(),
            &|bounds| circle_and_box_collide(query, bounds),
            &|circle| circles_collide(query, circle),
            &mut |value| {
                out.push(value.clone());
                false
            },
        );
        out
    }

    /// Whether anything overlaps a rectangle.
    ///
    /// Stops at the first hit, which is what placement actually asks: a label that collides with
    /// one thing is rejected as surely as one that collides with fifty.
    #[must_use]
    pub fn hit_test_box(&self, query: Bounds) -> bool {
        let mut hit = false;
        self.walk(
            query,
            &|bounds| boxes_collide(query, bounds),
            &|circle| circle_and_box_collide(circle, query),
            &mut |_| {
                hit = true;
                true
            },
        );
        hit
    }

    /// Whether anything overlaps a circle.
    #[must_use]
    pub fn hit_test_circle(&self, query: Circle) -> bool {
        let mut hit = false;
        self.walk(
            query.bounds(),
            &|bounds| circle_and_box_collide(query, bounds),
            &|circle| circles_collide(query, circle),
            &mut |_| {
                hit = true;
                true
            },
        );
        hit
    }

    /// How many shapes share a cell with this rectangle, and so would be exactly tested.
    ///
    /// The index's own counter, and the only way to tell an index from a list. A grid whose
    /// cells are mis-sized still returns *correct* answers — every shape becomes a candidate and
    /// the exact tests filter them — so no assertion about query results can catch it. What it
    /// loses is the reason the grid exists, which is that a query costs the shapes near it
    /// rather than all of them.
    #[must_use]
    pub fn candidates_for_box(&self, query: Bounds) -> usize {
        if self.misses(query) {
            return 0;
        }
        if self.covers(query) {
            return self.len();
        }
        let mut seen_boxes: BTreeSet<u32> = BTreeSet::new();
        let mut seen_circles: BTreeSet<u32> = BTreeSet::new();
        let (x1, y1) = (self.x_cell(query.min.0), self.y_cell(query.min.1));
        let (x2, y2) = (self.x_cell(query.max.0), self.y_cell(query.max.1));
        for x in x1..=x2 {
            for y in y1..=y2 {
                let cell = self.x_cells * y + x;
                seen_boxes.extend(self.box_cells[cell].iter().copied());
                seen_circles.extend(self.circle_cells[cell].iter().copied());
            }
        }
        seen_boxes.len() + seen_circles.len()
    }

    /// The grid's shape, in cells.
    #[must_use]
    pub const fn cells(&self) -> (usize, usize) {
        (self.x_cells, self.y_cells)
    }

    /// Whether nothing has been inserted.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.boxes.is_empty() && self.circles.is_empty()
    }

    /// How many shapes are indexed.
    #[must_use]
    pub fn len(&self) -> usize {
        self.boxes.len() + self.circles.len()
    }
}
