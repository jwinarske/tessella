//! A static two-dimensional index, transcribed from `kdbush.hpp`.
//!
//! # Why a transcription rather than any k-d tree
//!
//! Because the *order* it visits neighbours in is load-bearing. Clustering walks a zoom level
//! marking points visited as it goes, so which neighbour a query reaches first decides which
//! cluster absorbs it — and supercluster's own expectations pin the result down to the point
//! counts of a named cluster's four children. A tree with the same contents in a different
//! layout answers the same *set* and a different sequence, and the numbers move.
//!
//! So this is `kdbush.hpp` line for line: the same implicit layout in one array, the same
//! `nodeSize` leaf threshold, the same quickselect with the same median-of-medians shortcut for
//! large ranges, and the same in-order visit at each node before its two subtrees.
//!
//! # The one thing that could not be transcribed
//!
//! `sortKD` recurses on `m - 1` with an unsigned index. In C++ that underflows to a huge number
//! when `m` is zero and is caught by the leaf test on the next call; here it would panic. It
//! cannot happen — the recursion is only entered when `right - left > nodeSize`, which puts `m`
//! at least `nodeSize / 2` above `left` — but the arithmetic is written so that it could not
//! wrap even if the invariant were broken.

use alloc::vec::Vec;

/// The leaf size, and `kdbush.hpp`'s default.
///
/// Not a tuning knob here: it decides where the tree stops splitting and therefore the visit
/// order, so changing it changes which points cluster together.
const NODE_SIZE: usize = 64;

/// A static index over points, queried by range or by radius.
#[derive(Debug, Default, Clone)]
pub struct KdBush {
    /// Original indices, permuted alongside the coordinates.
    ids: Vec<u32>,
    /// The coordinates, in tree layout.
    points: Vec<(f64, f64)>,
}

impl KdBush {
    /// Builds the index over `points`, whose order gives each point its id.
    #[must_use]
    pub fn new(points: &[(f64, f64)]) -> Self {
        #[allow(clippy::cast_possible_truncation)]
        let ids = (0..points.len() as u32).collect();
        let mut bush = Self {
            ids,
            points: points.to_vec(),
        };
        if points.len() > 1 {
            bush.sort_kd(0, points.len() - 1, 0);
        }
        bush
    }

    /// How many points it holds.
    #[must_use]
    pub fn len(&self) -> usize {
        self.points.len()
    }

    /// Whether it holds none.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.points.is_empty()
    }

    /// Visits every point inside the rectangle, in tree order.
    pub fn range(&self, min_x: f64, min_y: f64, max_x: f64, max_y: f64, visit: &mut dyn FnMut(u32)) {
        if self.points.is_empty() {
            return;
        }
        self.range_in(min_x, min_y, max_x, max_y, visit, 0, self.points.len() - 1, 0);
    }

    /// Visits every point within `r` of `(qx, qy)`, in tree order.
    pub fn within(&self, qx: f64, qy: f64, r: f64, visit: &mut dyn FnMut(u32)) {
        if self.points.is_empty() {
            return;
        }
        self.within_in(qx, qy, r, visit, 0, self.points.len() - 1, 0);
    }

    #[allow(clippy::too_many_arguments)]
    fn range_in(
        &self,
        min_x: f64,
        min_y: f64,
        max_x: f64,
        max_y: f64,
        visit: &mut dyn FnMut(u32),
        left: usize,
        right: usize,
        axis: u8,
    ) {
        if right - left <= NODE_SIZE {
            for index in left..=right {
                let (x, y) = self.points[index];
                if x >= min_x && x <= max_x && y >= min_y && y <= max_y {
                    visit(self.ids[index]);
                }
            }
            return;
        }

        let middle = (left + right) >> 1;
        let (x, y) = self.points[middle];
        if x >= min_x && x <= max_x && y >= min_y && y <= max_y {
            visit(self.ids[middle]);
        }

        if if axis == 0 { min_x <= x } else { min_y <= y } {
            self.range_in(min_x, min_y, max_x, max_y, visit, left, middle - 1, (axis + 1) % 2);
        }
        if if axis == 0 { max_x >= x } else { max_y >= y } {
            self.range_in(min_x, min_y, max_x, max_y, visit, middle + 1, right, (axis + 1) % 2);
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn within_in(
        &self,
        qx: f64,
        qy: f64,
        r: f64,
        visit: &mut dyn FnMut(u32),
        left: usize,
        right: usize,
        axis: u8,
    ) {
        let r2 = r * r;
        if right - left <= NODE_SIZE {
            for index in left..=right {
                let (x, y) = self.points[index];
                if sq_dist(x, y, qx, qy) <= r2 {
                    visit(self.ids[index]);
                }
            }
            return;
        }

        let middle = (left + right) >> 1;
        let (x, y) = self.points[middle];
        if sq_dist(x, y, qx, qy) <= r2 {
            visit(self.ids[middle]);
        }

        if if axis == 0 { qx - r <= x } else { qy - r <= y } {
            self.within_in(qx, qy, r, visit, left, middle - 1, (axis + 1) % 2);
        }
        if if axis == 0 { qx + r >= x } else { qy + r >= y } {
            self.within_in(qx, qy, r, visit, middle + 1, right, (axis + 1) % 2);
        }
    }

    fn sort_kd(&mut self, left: usize, right: usize, axis: u8) {
        if right - left <= NODE_SIZE {
            return;
        }
        let middle = (left + right) >> 1;
        self.select(middle, left, right, axis);
        // `middle > left` because the leaf test above put at least `NODE_SIZE` between the two.
        self.sort_kd(left, middle - 1, (axis + 1) % 2);
        self.sort_kd(middle + 1, right, (axis + 1) % 2);
    }

    /// Partitions `[left, right]` so that the `k`th element is the one that belongs there.
    ///
    /// Floyd–Rivest, as `kdbush.hpp` has it including the sampling shortcut for wide ranges —
    /// which is not an optimisation detail but part of the layout, since a different pivot
    /// choice permutes the equal elements differently and the visit order with them.
    fn select(&mut self, k: usize, mut left: usize, mut right: usize, axis: u8) {
        let at = |points: &Vec<(f64, f64)>, index: usize| -> f64 {
            if axis == 0 {
                points[index].0
            } else {
                points[index].1
            }
        };

        while right > left {
            if right - left > 600 {
                #[allow(clippy::cast_precision_loss)]
                let n = (right - left + 1) as f64;
                #[allow(clippy::cast_precision_loss)]
                let m = (k - left + 1) as f64;
                let z = libm::log(n);
                let s = 0.5 * libm::exp(2.0 * z / 3.0);
                #[allow(clippy::cast_precision_loss)]
                let sign = if 2.0 * m < n { -1.0 } else { 1.0 };
                #[allow(clippy::cast_precision_loss)]
                let r = (k as f64) - m * s / n + 0.5 * libm::sqrt(z * s * (1.0 - s / n)) * sign;
                #[allow(
                    clippy::cast_possible_truncation,
                    clippy::cast_sign_loss,
                    clippy::cast_precision_loss
                )]
                let (lo, hi) = (r.max(0.0) as usize, (r + s).max(0.0) as usize);
                self.select(k, left.max(lo), right.min(hi), axis);
            }

            let t = at(&self.points, k);
            let mut i = left;
            let mut j = right;

            self.swap(left, k);
            if at(&self.points, right) > t {
                self.swap(left, right);
            }

            while i < j {
                self.swap(i, j);
                i += 1;
                j -= 1;
                while at(&self.points, i) < t {
                    i += 1;
                }
                while at(&self.points, j) > t {
                    j -= 1;
                }
            }

            if (at(&self.points, left) - t).abs() < f64::EPSILON {
                self.swap(left, j);
            } else {
                j += 1;
                self.swap(j, right);
            }

            if j <= k {
                left = j + 1;
            }
            if k <= j {
                // `j` is at least `left`, which is at least one, whenever this is reached: the
                // partition above cannot place the pivot below its own left bound.
                right = j.saturating_sub(1);
            }
        }
    }

    fn swap(&mut self, i: usize, j: usize) {
        self.ids.swap(i, j);
        self.points.swap(i, j);
    }
}

fn sq_dist(ax: f64, ay: f64, bx: f64, by: f64) -> f64 {
    let dx = ax - bx;
    let dy = ay - by;
    dx * dx + dy * dy
}
