// SPDX-License-Identifier: Apache-2.0

//! What a subdivider can be held to when there is nothing to render against.
//!
//! plan.md §13.4: MapLibre Native has no globe, so `mbgl-render` answers no question here and the
//! gross-pixel metric that decided everything else says nothing. These are structural instead —
//! area, cell containment, and the absence of T-junctions — and they are what the bend rests on.

use tessella_layout::fill::Position;
use tessella_layout::subdivide::{MAX_CELLS, grid_step, subdivide_ring, subdivide_triangles};

/// Twice a triangle's unsigned area, exactly, in integer units.
fn double_area(triangle: &[Position; 3]) -> i64 {
    let [a, b, c] = triangle;
    let abx = i64::from(b[0]) - i64::from(a[0]);
    let aby = i64::from(b[1]) - i64::from(a[1]);
    let acx = i64::from(c[0]) - i64::from(a[0]);
    let acy = i64::from(c[1]) - i64::from(a[1]);
    (abx * acy - aby * acx).abs()
}

fn total_area(triangles: &[[Position; 3]]) -> i64 {
    triangles.iter().map(double_area).sum()
}

/// The widest a triangle reaches along either axis.
fn extent_of(triangle: &[Position; 3]) -> (i32, i32) {
    let xs = triangle.iter().map(|p| i32::from(p[0]));
    let ys = triangle.iter().map(|p| i32::from(p[1]));
    let (min_x, max_x) = (xs.clone().min().unwrap(), xs.max().unwrap());
    let (min_y, max_y) = (ys.clone().min().unwrap(), ys.max().unwrap());
    (max_x - min_x, max_y - min_y)
}

#[test]
fn a_triangle_inside_one_cell_is_untouched() {
    // Wholly within the cell `[0,64) x [0,64)`, so there is nothing to cut.
    let triangle = [[1, 1], [60, 4], [8, 55]];
    let out = subdivide_triangles(&[triangle], 64);
    assert_eq!(out, vec![triangle]);
}

#[test]
fn a_step_of_zero_is_the_identity() {
    let triangles = [[[0, 0], [4096, 0], [0, 4096]]];
    assert_eq!(subdivide_triangles(&triangles, 0), triangles.to_vec());
    assert_eq!(subdivide_triangles(&triangles, -8), triangles.to_vec());
}

#[test]
fn every_output_triangle_fits_in_one_cell() {
    // The tile-covering triangle earcut produces for a water layer that fills its tile.
    let triangles = [
        [[0, 0], [4096, 0], [4096, 4096]],
        [[0, 0], [4096, 4096], [0, 4096]],
    ];
    let step = 256;
    for triangle in subdivide_triangles(&triangles, step) {
        let (width, height) = extent_of(&triangle);
        assert!(
            width <= step && height <= step,
            "{triangle:?} spans {width}x{height}, past a {step}-unit cell",
        );
    }
}

#[test]
fn area_survives_the_split() {
    let triangles = [
        [[0, 0], [4096, 0], [4096, 4096]],
        [[0, 0], [4096, 4096], [0, 4096]],
        // An oblique one, so the cuts do not all land on vertices.
        [[137, 991], [3855, 210], [1204, 3999]],
    ];
    let before = total_area(&triangles);
    let after = total_area(&subdivide_triangles(&triangles, 256));
    // Rounding moves each cut vertex by up to half a unit, so the two agree to a bound rather
    // than exactly. A tenth of a percent over a tile is four orders below one cell.
    let slack = before / 1000;
    assert!(
        (before - after).abs() <= slack,
        "area {before} became {after}, past a slack of {slack}",
    );
}

#[test]
fn nothing_is_left_spanning_the_whole_tile() {
    let triangles = [[[0, 0], [4096, 0], [0, 4096]]];
    let out = subdivide_triangles(&triangles, 512);
    assert!(out.len() > 1, "a tile-sized triangle came back as {out:?}");
    // Eight cells a side, so the diagonal half of the grid, and no piece larger than a cell.
    for triangle in &out {
        let (width, height) = extent_of(triangle);
        assert!(width <= 512 && height <= 512);
    }
}

/// The property that decides whether the planet has cracks in it.
///
/// A vertex part-way along a neighbor's edge is invisible while the map is flat -- the two sit on
/// the same straight line -- and opens the moment both are bent, because the neighbor's edge stays
/// a chord while this one follows the sphere. The cut is by global grid lines precisely so that two
/// triangles sharing an edge cut it in the same places without being told they are neighbors.
#[test]
fn no_vertex_lands_inside_a_neighbors_edge() {
    let triangles = [
        [[0, 0], [4096, 0], [4096, 4096]],
        [[0, 0], [4096, 4096], [0, 4096]],
    ];
    let out = subdivide_triangles(&triangles, 256);

    let mut vertices: Vec<Position> = out.iter().flat_map(|t| t.iter().copied()).collect();
    vertices.sort_unstable();
    vertices.dedup();

    for triangle in &out {
        for edge in 0..3 {
            let a = triangle[edge];
            let b = triangle[(edge + 1) % 3];
            for vertex in &vertices {
                if *vertex == a || *vertex == b {
                    continue;
                }
                // Collinear and strictly between the two ends.
                let abx = i64::from(b[0]) - i64::from(a[0]);
                let aby = i64::from(b[1]) - i64::from(a[1]);
                let avx = i64::from(vertex[0]) - i64::from(a[0]);
                let avy = i64::from(vertex[1]) - i64::from(a[1]);
                if abx * avy - aby * avx != 0 {
                    continue;
                }
                let along = abx * avx + aby * avy;
                let length = abx * abx + aby * aby;
                assert!(
                    along <= 0 || along >= length,
                    "{vertex:?} sits inside the edge {a:?}-{b:?}",
                );
            }
        }
    }
}

/// The outline is drawn over the same vertices as the fill, so its cuts have to be the fill's.
#[test]
fn a_ring_is_cut_where_a_triangle_is() {
    let step = 256;
    // One edge, walked as a ring and as a triangle side.
    let ring = subdivide_ring(&[[0, 0], [4096, 4096]], step);
    let triangles = subdivide_triangles(&[[[0, 0], [4096, 4096], [0, 4096]]], step);

    let mut on_diagonal: Vec<Position> = triangles
        .iter()
        .flat_map(|t| t.iter().copied())
        .filter(|p| p[0] == p[1])
        .collect();
    on_diagonal.sort_unstable();
    on_diagonal.dedup();

    for point in &ring {
        assert!(
            on_diagonal.contains(point),
            "the ring cut at {point:?}, which the triangles did not",
        );
    }
}

#[test]
fn a_ring_keeps_its_own_vertices_and_its_closing_repeat() {
    let ring = [[10, 10], [3000, 10], [3000, 3000], [10, 3000], [10, 10]];
    let out = subdivide_ring(&ring, 256);
    assert_eq!(out.first(), Some(&[10, 10]));
    assert_eq!(out.last(), Some(&[10, 10]));
    for point in &ring {
        assert!(out.contains(point), "{point:?} was dropped");
    }
    assert!(out.len() > ring.len());
}

#[test]
fn a_ring_with_no_crossings_is_unchanged() {
    let ring = vec![[1, 1], [60, 1], [60, 60], [1, 60], [1, 1]];
    assert_eq!(subdivide_ring(&ring, 4096), ring);
}

#[test]
fn the_step_holds_the_cell_count_at_or_below_what_was_asked() {
    for segments in [1u32, 2, 3, 7, 16, 21, 29, 128] {
        let extent = 4096;
        let step = grid_step(extent, segments);
        assert!(step >= 1);
        let cells = (extent + step - 1) / step;
        assert!(
            cells <= segments as i32,
            "{segments} segments gave a {step}-unit step, which is {cells} cells",
        );
    }
}

#[test]
fn the_step_is_capped_the_way_the_segment_count_is() {
    // Past `MAX_CELLS` the step stops shrinking, so a vertex count cannot run away.
    let capped = grid_step(4096, MAX_CELLS as u32);
    assert_eq!(grid_step(4096, 100_000), capped);
}

#[test]
fn a_degenerate_extent_asks_for_no_grid() {
    assert_eq!(grid_step(0, 32), 1);
    assert_eq!(grid_step(-1, 32), 1);
    assert_eq!(grid_step(4096, 0), 4096);
    assert_eq!(grid_step(4096, 1), 4096);
}

#[test]
fn slivers_do_not_survive_rounding() {
    // A triangle a fraction of a unit tall: every piece rounds onto a line and none has area.
    let out = subdivide_triangles(&[[[0, 0], [4096, 0], [4096, 1]]], 256);
    for triangle in &out {
        assert!(double_area(triangle) > 0, "{triangle:?} has no area");
    }
}

#[test]
fn geometry_buffered_past_the_tile_edge_still_cuts() {
    // Tiles carry geometry outside `0..extent`, and the grid runs through negatives the same way.
    let out = subdivide_triangles(&[[[-2048, -2048], [4096, 0], [0, 4096]]], 512);
    for triangle in &out {
        let (width, height) = extent_of(triangle);
        assert!(width <= 512 && height <= 512, "{triangle:?}");
    }
    assert!(out.len() > 1);
}

/// What the split costs on a tile-filling polygon, for the record rather than as an assertion.
#[test]
#[ignore = "a measurement, not a check"]
fn subdivision_counts() {
    // The two triangles earcut gives a water layer that covers its tile.
    let triangles = [
        [[0, 0], [4096, 0], [4096, 4096]],
        [[0, 0], [4096, 4096], [0, 4096]],
    ];
    println!("segments  step   triangles  vertices");
    for segments in [1u32, 2, 4, 6, 8, 11, 15, 21, 29, 128] {
        let step = grid_step(4096, segments);
        let out = subdivide_triangles(&triangles, step);
        let mut vertices: Vec<Position> = out.iter().flat_map(|t| t.iter().copied()).collect();
        vertices.sort_unstable();
        vertices.dedup();
        println!(
            "{segments:>8}  {step:>4}   {:>9}  {:>8}",
            out.len(),
            vertices.len()
        );
    }
}
