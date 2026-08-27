//! Frustum-versus-box culling, for the cover a pitched view needs.
//!
//! # Why a pitched cover is a different algorithm
//!
//! Unpitched, the visible ground is a rectangle: project the viewport's corners, take the
//! bounding box, walk it. Pitched, it is a *trapezoid* that widens towards the horizon, and its
//! bounding box is enormously larger than the trapezoid — at fifty-five degrees the box holds
//! several times the tiles the view can see, and every one of them is a fetch and a build.
//!
//! So mbgl does not bound it. It walks the tile quadtree depth-first from the root, tests each
//! node's box against the view frustum, and discards a whole subtree the moment its box falls
//! outside. What comes back is the tiles the frustum actually crosses.
//!
//! # What is transcribed and what is left out
//!
//! The traversal, the frustum construction and the separating-axis test are mbgl's
//! `util::tileCover`, `Frustum::fromInvProjMatrix` and `Frustum::intersects`.
//!
//! What is left out is the level-of-detail pass. mbgl can return a *mixed* set — coarser tiles
//! far from the camera, finer near it — under `tileLodMode`, and this returns one level, which
//! is mbgl with LOD disabled rather than an approximation of it: the stop condition there is
//! `node.zoom == maxZoom || (!shouldSplitTile && node.zoom >= minZoom)`, and with `minZoom` and
//! `maxZoom` both the requested level the second clause can never fire first.
//!
//! mbgl's `intersectsPrecise` is also left out. It is a second, exhaustive separating-axis test
//! run only on nodes that already passed the first, and mbgl's own comment puts its yield at
//! under one percent of cases — a few extra tiles at the edge of the screen, which are fetched
//! and drawn rather than missed.

use crate::camera::Mat4;

/// An axis-aligned box. Tiles are flat, so `z` is zero at both ends.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Aabb {
    /// Lower corner.
    pub min: [f64; 3],
    /// Upper corner.
    pub max: [f64; 3],
}

impl Aabb {
    /// One of the four quadrants, in mbgl's index order: `{0,0}, {1,0}, {0,1}, {1,1}`.
    ///
    /// The order is the same one `childrenOf` uses to number a node's children — `x` from the
    /// low bit and `y` from the high — so a quadrant and its tile coordinate agree without a
    /// lookup between them.
    #[must_use]
    pub fn quadrant(&self, index: usize) -> Self {
        let centre_x = 0.5 * (self.max[0] + self.min[0]);
        let centre_y = 0.5 * (self.max[1] + self.min[1]);
        let (mut min, mut max) = (self.min, self.max);
        if index & 1 == 1 {
            min[0] = centre_x;
        } else {
            max[0] = centre_x;
        }
        if index & 2 == 2 {
            min[1] = centre_y;
        } else {
            max[1] = centre_y;
        }
        Self { min, max }
    }

    /// Whether two boxes overlap on every axis.
    #[must_use]
    pub fn intersects(&self, other: &Self) -> bool {
        (0..3).all(|axis| self.min[axis] <= other.max[axis] && other.min[axis] <= self.max[axis])
    }

    /// The per-axis distance from a point to the nearest point of the box, each absolute.
    #[must_use]
    pub fn distance_xyz(&self, point: [f64; 3]) -> [f64; 3] {
        let mut out = [0.0; 3];
        for axis in 0..3 {
            let closest = point[axis].clamp(self.min[axis], self.max[axis]);
            out[axis] = (closest - point[axis]).abs();
        }
        out
    }
}

/// Where a box sits relative to a frustum.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Intersection {
    /// No overlap; the whole subtree can go.
    Separate,
    /// Partly inside, so children have to be tested individually.
    Intersects,
    /// Wholly inside, so no child needs testing at all.
    Contains,
}

/// The viewing volume, as six planes and the eight corners they meet at.
#[derive(Debug, Clone)]
pub struct Frustum {
    /// `ax + by + cz + d = 0` per plane, the normal pointing inward.
    planes: [[f64; 4]; 6],
    /// The bounding box of the eight corners, for the cheap rejection that comes first.
    bounds: Aabb,
}

/// Corner indices, in the order `fromInvProjMatrix` builds them.
const NEAR_TL: usize = 0;
const NEAR_TR: usize = 1;
const NEAR_BR: usize = 2;
const NEAR_BL: usize = 3;
const FAR_TL: usize = 4;
const FAR_TR: usize = 5;
const FAR_BR: usize = 6;
const FAR_BL: usize = 7;

impl Frustum {
    /// The frustum of a projection, in tile units at `zoom`.
    ///
    /// The eight corners of clip space are pushed back through the inverse projection and then
    /// scaled out of world pixels into tiles, which is what lets the traversal compare them
    /// against a tile's own coordinates without a conversion at every node.
    ///
    /// # Errors
    ///
    /// `None` when the projection cannot be inverted, which means a degenerate camera rather
    /// than an empty view.
    #[must_use]
    pub fn from_projection(projection: &Mat4, world_size: f64, zoom: f64) -> Option<Self> {
        let inverse = crate::camera::invert(projection)?;
        let scale = zoom.exp2();

        let clip: [[f64; 4]; 8] = [
            [-1.0, 1.0, -1.0, 1.0],
            [1.0, 1.0, -1.0, 1.0],
            [1.0, -1.0, -1.0, 1.0],
            [-1.0, -1.0, -1.0, 1.0],
            [-1.0, 1.0, 1.0, 1.0],
            [1.0, 1.0, 1.0, 1.0],
            [1.0, -1.0, 1.0, 1.0],
            [-1.0, -1.0, 1.0, 1.0],
        ];

        let mut points = [[0.0f64; 3]; 8];
        for (corner, out) in clip.iter().zip(points.iter_mut()) {
            let transformed = transform(&inverse, *corner);
            let w = transformed[3];
            if w == 0.0 || !w.is_finite() {
                return None;
            }
            for axis in 0..3 {
                out[axis] = transformed[axis] / w / world_size * scale;
            }
        }

        // Three points per plane, wound so the normal points into the volume.
        let triples = [
            [NEAR_BL, NEAR_BR, FAR_BR],  // bottom
            [NEAR_TL, NEAR_BL, FAR_BL],  // left
            [NEAR_BR, NEAR_TR, FAR_TR],  // right
            [NEAR_TL, FAR_TL, FAR_TR],   // top
            [NEAR_TL, NEAR_TR, NEAR_BR], // near
            [FAR_BR, FAR_TR, FAR_TL],    // far
        ];
        let mut planes = [[0.0f64; 4]; 6];
        for (triple, plane) in triples.iter().zip(planes.iter_mut()) {
            let (p0, p1, p2) = (points[triple[0]], points[triple[1]], points[triple[2]]);
            let a = sub(p0, p1);
            let b = sub(p2, p1);
            let normal = normalize(cross(a, b));
            *plane = [normal[0], normal[1], normal[2], -dot(normal, p1)];
        }

        let mut min = [f64::INFINITY; 3];
        let mut max = [f64::NEG_INFINITY; 3];
        for point in &points {
            for axis in 0..3 {
                min[axis] = min[axis].min(point[axis]);
                max[axis] = max[axis].max(point[axis]);
            }
        }

        Some(Self {
            planes,
            bounds: Aabb { min, max },
        })
    }

    /// Where a box sits relative to this frustum.
    ///
    /// A separating-axis test over the six planes, preceded by a box-versus-box rejection. It is
    /// mbgl's conservative version: it can answer [`Intersection::Intersects`] for a box that is
    /// in fact outside, which costs a tile, and it never answers [`Intersection::Separate`] for
    /// one that is inside, which would lose one.
    ///
    /// Only the four ground corners are tested, because a tile has no height — the same
    /// assumption mbgl asserts.
    #[must_use]
    pub fn intersects(&self, aabb: &Aabb) -> Intersection {
        if !self.bounds.intersects(aabb) {
            return Intersection::Separate;
        }

        let corners = [
            [aabb.min[0], aabb.min[1], 0.0, 1.0],
            [aabb.max[0], aabb.min[1], 0.0, 1.0],
            [aabb.max[0], aabb.max[1], 0.0, 1.0],
            [aabb.min[0], aabb.max[1], 0.0, 1.0],
        ];

        // mbgl's epsilon, and it is on the *inside*: a corner exactly on a plane counts as in,
        // so a tile flush against the edge of the screen is drawn rather than dropped.
        const EPSILON: f64 = 1e-10;
        let mut fully_inside = true;
        for plane in &self.planes {
            let inside = corners
                .iter()
                .filter(|corner| dot4(*plane, **corner) >= -EPSILON)
                .count();
            if inside == 0 {
                return Intersection::Separate;
            }
            if inside != corners.len() {
                fully_inside = false;
            }
        }

        if fully_inside {
            Intersection::Contains
        } else {
            Intersection::Intersects
        }
    }
}

fn transform(matrix: &Mat4, point: [f64; 4]) -> [f64; 4] {
    let mut out = [0.0f64; 4];
    for row in 0..4 {
        out[row] = matrix[row] * point[0]
            + matrix[4 + row] * point[1]
            + matrix[8 + row] * point[2]
            + matrix[12 + row] * point[3];
    }
    out
}

fn sub(a: [f64; 3], b: [f64; 3]) -> [f64; 3] {
    [a[0] - b[0], a[1] - b[1], a[2] - b[2]]
}

fn cross(a: [f64; 3], b: [f64; 3]) -> [f64; 3] {
    [
        a[1] * b[2] - a[2] * b[1],
        a[2] * b[0] - a[0] * b[2],
        a[0] * b[1] - a[1] * b[0],
    ]
}

fn dot(a: [f64; 3], b: [f64; 3]) -> f64 {
    a[0] * b[0] + a[1] * b[1] + a[2] * b[2]
}

fn dot4(a: [f64; 4], b: [f64; 4]) -> f64 {
    a[0] * b[0] + a[1] * b[1] + a[2] * b[2] + a[3] * b[3]
}

fn normalize(a: [f64; 3]) -> [f64; 3] {
    let length = dot(a, a).sqrt();
    if length == 0.0 {
        return a;
    }
    [a[0] / length, a[1] / length, a[2] / length]
}

/// One node of the traversal: a tile, its box, and whether an ancestor was wholly visible.
struct Node {
    aabb: Aabb,
    zoom: u8,
    x: u32,
    y: u32,
    wrap: i32,
    fully_visible: bool,
}

/// The tiles at `zoom` whose ground the frustum crosses, nearest to the centre first.
///
/// `wraps` is how many copies of the world to walk on each side; mbgl uses three, so an
/// east-west view near the antimeridian sees the same ground from both directions.
#[must_use]
pub fn covered(
    frustum: &Frustum,
    zoom: u8,
    centre: [f64; 2],
    wraps: i32,
    limit: usize,
) -> Option<Vec<(u8, u32, u32, i32)>> {
    let tiles = f64::from(1u32 << zoom.min(30));

    let root = |wrap: i32| Node {
        aabb: Aabb {
            min: [f64::from(wrap) * tiles, 0.0, 0.0],
            max: [f64::from(wrap + 1) * tiles, tiles, 0.0],
        },
        zoom: 0,
        x: 0,
        y: 0,
        wrap,
        fully_visible: false,
    };

    // Nearest world copy last, so it is popped first and its tiles sort ahead on ties.
    let mut stack: Vec<Node> = Vec::with_capacity(128);
    for offset in (1..=wraps).rev() {
        stack.push(root(-offset));
        stack.push(root(offset));
    }
    stack.push(root(0));

    let mut found: Vec<(f64, (u8, u32, u32, i32))> = Vec::new();
    while let Some(mut node) = stack.pop() {
        // An ancestor wholly inside means every descendant is too, so the test is skipped
        // rather than repeated down the whole subtree.
        if !node.fully_visible {
            match frustum.intersects(&node.aabb) {
                Intersection::Separate => continue,
                Intersection::Contains => node.fully_visible = true,
                Intersection::Intersects => {}
            }
        }

        if node.zoom == zoom {
            let dx = f64::from(node.wrap) * tiles + f64::from(node.x) + 0.5 - centre[0];
            let dy = f64::from(node.y) + 0.5 - centre[1];
            found.push((dx * dx + dy * dy, (node.zoom, node.x, node.y, node.wrap)));
            if found.len() > limit {
                return None;
            }
            continue;
        }

        for index in 0..4 {
            stack.push(Node {
                aabb: node.aabb.quadrant(index),
                zoom: node.zoom + 1,
                x: (node.x << 1) + (index as u32 & 1),
                y: (node.y << 1) + (index as u32 >> 1),
                wrap: node.wrap,
                fully_visible: node.fully_visible,
            });
        }
    }

    // Nearest first, which is the order tiles are wanted in: the middle of the screen is what a
    // viewer is looking at, and a cover that loaded the horizon first would fill it last.
    found.sort_by(|a, b| a.0.total_cmp(&b.0));
    Some(found.into_iter().map(|(_, id)| id).collect())
}
