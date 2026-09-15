// SPDX-License-Identifier: BSD-2-Clause
//! The grid a terrain's elevation is applied to, and the skirt around it.
//!
//! MapLibre GL JS's `getTerrainMesh` and `_buildSkirts`. maplibre-native has no terrain to check
//! this against -- `tessella_source`'s `terrain` module records the three ways that was
//! established -- so the reference is GL JS and the transcription is literal, index pattern
//! included.
//!
//! # One mesh, every tile
//!
//! This is the whole of the geometry and it is built once. The grid is a tile's own coordinates,
//! `0..EXTENT` on both axes, and *nothing about a tile is in it*: the elevation arrives as a
//! texture and is applied per vertex when the surface is drawn, so two tiles at different
//! heights draw the same vertices through different matrices and different DEMs. GL JS caches
//! it in `_meshCache` for exactly that reason.
//!
//! That is worth stating early because it decides how this reaches the consumer. A terrain is one
//! geometry and N uses of it, which is the shape the stream was built for (§5.3) -- not one
//! bucket per tile like every layer family here.
//!
//! # The third component is a flag, not a height
//!
//! A vertex is `[x, y, z]` and `z` is zero or one: one marks a *skirt* vertex, which the vertex
//! shader drops by the skirt length. GL JS's line is
//!
//! ```glsl
//! float ele_delta = a_pos3d.z == 1.0 ? u_ele_delta : 0.0;
//! gl_Position = projectTileFor3D(a_pos3d.xy, ele - ele_delta);
//! ```
//!
//! so the height still comes from the DEM at `xy` and the flag only says how far to drop it. A
//! reader who takes `z` for an elevation gets a terrain one unit tall.
//!
//! # What the skirt is for
//!
//! Neighboring tiles are not always at the same zoom, and where they are not their edges sample
//! the DEM at different resolutions and disagree by a fraction of a meter. Seen from the side
//! that is a hairline crack through the planet. The skirt is a curtain hanging from every edge --
//! the same `xy` as the edge vertices, dropped -- so the crack is behind something rather than
//! open. It is not visible geometry and its length does not have to be right, only long enough:
//! [`skirt_length`] is GL JS's guess, divided by five "by trial and error".

use alloc::vec::Vec;

/// Cells across a tile's mesh. GL JS's `meshSize`, and its default.
///
/// The grid is one larger in each direction, because a 128-cell grid has 129 vertices on a side.
pub const MESH_SIZE: u16 = 128;

/// A tile's coordinate extent, which the grid spans.
pub const EXTENT: u16 = 8192;

/// Tile units between adjacent grid vertices. GL JS's `delta`.
pub const DELTA: u16 = EXTENT / MESH_SIZE;

/// Vertices along one side of the grid.
pub const GRID_SIDE: u16 = MESH_SIZE + 1;

/// The terrain surface as one mesh, in tile coordinates.
///
/// Built by [`mesh`] and shared by every tile the terrain covers. See the module note for why
/// there is only one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TerrainMesh {
    /// `[x, y, skirt]` per vertex: the grid first, then the skirt rows around it.
    pub vertices: Vec<[i16; 3]>,
    /// Triangles over them, three indices each.
    pub indices: Vec<u16>,
    /// How many of [`Self::vertices`] are the grid, before the skirt begins.
    ///
    /// The grid alone is the surface; a caller that wants to sample or measure it can stop here
    /// rather than filtering on the flag.
    pub grid_vertices: usize,
}

impl TerrainMesh {
    /// How many triangles it draws.
    #[must_use]
    pub fn triangles(&self) -> usize {
        self.indices.len() / 3
    }
}

/// Builds the terrain mesh: the grid, then the four skirts.
///
/// GL JS's `getTerrainMesh` with `_buildSkirts` always on. Its "none" skirt strategy is a map
/// option rather than a style property, and a terrain with no skirt shows the cracks the skirt
/// exists to hide, so there is one strategy here until something asks for the other.
///
/// The poles are not handled. GL JS moves the north and south skirt rows out to
/// `NORTH_POLE_Y` / `SOUTH_POLE_Y` when a globe is on, so the mesh closes over the pole rather
/// than ending at the top of the world; this build's globe (§13.4) bends tile geometry by its own
/// path and a terrain on it is a question for whoever joins the two.
#[must_use]
pub fn mesh() -> TerrainMesh {
    let side = usize::from(GRID_SIDE);
    let cells = usize::from(MESH_SIZE);
    let delta = i16::try_from(DELTA).unwrap_or(i16::MAX);
    let extent = i16::try_from(EXTENT).unwrap_or(i16::MAX);

    let mut vertices: Vec<[i16; 3]> = Vec::with_capacity(side * side + side * 2 + side * 4);
    let mut indices: Vec<u16> = Vec::with_capacity(cells * cells * 6 + cells * 24);

    for y in 0..side {
        for x in 0..side {
            #[allow(clippy::cast_possible_truncation)]
            vertices.push([x as i16 * delta, y as i16 * delta, 0]);
        }
    }
    let grid_vertices = vertices.len();

    // Two triangles a cell, walking rows of `side` vertices. GL JS writes the row step into its
    // loop bound rather than its index -- `y` runs to `meshSize * meshSize` in steps of
    // `meshSize + 1` -- which is the same 128 rows said a harder way.
    for row in 0..cells {
        let base = row * side;
        for column in 0..cells {
            let top_left = base + column;
            let bottom_left = top_left + side;
            #[allow(clippy::cast_possible_truncation)]
            indices.extend_from_slice(&[
                top_left as u16,
                bottom_left as u16,
                (bottom_left + 1) as u16,
                top_left as u16,
                (bottom_left + 1) as u16,
                (top_left + 1) as u16,
            ]);
        }
    }

    skirts(&mut vertices, &mut indices, delta, extent);

    TerrainMesh {
        vertices,
        indices,
        grid_vertices,
    }
}

/// The curtain around the grid: a row above and below, then a column each side.
///
/// Transcribed from `_buildSkirts`, offsets and winding included. The two halves are not the same
/// shape and that is GL JS's doing: the north and south skirts are a single row of vertices each,
/// stitched to the grid's own edge, while the east and west skirts carry *both* the surface
/// vertex and the dropped one, interleaved in pairs. Either would have done; what matters is that
/// the winding matches, because a terrain is the one surface here with a back face.
fn skirts(vertices: &mut Vec<[i16; 3]>, indices: &mut Vec<u16>, delta: i16, extent: i16) {
    let side = usize::from(GRID_SIDE);
    let cells = usize::from(MESH_SIZE);

    // North and south, as rows below and above the grid's first and last.
    let offset_top = vertices.len();
    let offset_top_edge = 0;
    let offset_bottom = offset_top + side;
    let offset_bottom_edge = side * cells;
    for x in 0..side {
        #[allow(clippy::cast_possible_truncation)]
        vertices.push([x as i16 * delta, 0, 1]);
    }
    for x in 0..side {
        #[allow(clippy::cast_possible_truncation)]
        vertices.push([x as i16 * delta, extent, 1]);
    }
    #[allow(clippy::cast_possible_truncation)]
    for x in 0..cells {
        indices.extend_from_slice(&[
            (offset_bottom_edge + x) as u16,
            (offset_bottom + x) as u16,
            (offset_bottom + x + 1) as u16,
            (offset_bottom_edge + x) as u16,
            (offset_bottom + x + 1) as u16,
            (offset_bottom_edge + x + 1) as u16,
            (offset_top_edge + x) as u16,
            (offset_top + x + 1) as u16,
            (offset_top + x) as u16,
            (offset_top_edge + x) as u16,
            (offset_top_edge + x + 1) as u16,
            (offset_top + x + 1) as u16,
        ]);
    }

    // West and east, as pairs: the surface vertex and the dropped one, per row.
    let offset_left = vertices.len();
    let offset_right = offset_left + side * 2;
    for x in [0i16, 1] {
        for y in 0..side {
            for z in [0i16, 1] {
                #[allow(clippy::cast_possible_truncation)]
                vertices.push([x * extent, y as i16 * delta, z]);
            }
        }
    }
    #[allow(clippy::cast_possible_truncation)]
    for row in 0..cells {
        let y = row * 2;
        indices.extend_from_slice(&[
            (offset_left + y) as u16,
            (offset_left + y + 1) as u16,
            (offset_left + y + 3) as u16,
            (offset_left + y) as u16,
            (offset_left + y + 3) as u16,
            (offset_left + y + 2) as u16,
            (offset_right + y) as u16,
            (offset_right + y + 3) as u16,
            (offset_right + y + 1) as u16,
            (offset_right + y) as u16,
            (offset_right + y + 2) as u16,
            (offset_right + y + 3) as u16,
        ]);
    }
}

/// How far a skirt hangs below the surface, in meters.
///
/// GL JS's `getSkirtLength`, comment and all: a fifth of the ground a tile covers at the equator,
/// "evaluated by trial and error to get a frame in the right height". It is a curtain rather than
/// a measurement -- too short leaves the crack it hides open and too long costs nothing but
/// fragments behind the surface.
#[must_use]
pub fn skirt_length(zoom: f64) -> f64 {
    const EARTH_RADIUS_M: f64 = 6_378_137.0;
    core::f64::consts::TAU * EARTH_RADIUS_M / zoom.max(0.0).exp2() / 5.0
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The counts GL JS's own loops produce, arrived at here by a different route.
    ///
    /// 129 vertices a side is 16,641 for the grid; the north and south skirts add a row each and
    /// the east and west two columns of pairs, for 17,415. The triangles are two a cell over
    /// 16,384 cells, plus two a step along each of the four edges, which is 33,792.
    #[test]
    fn the_counts_are_the_references() {
        let mesh = mesh();
        assert_eq!(mesh.grid_vertices, 129 * 129);
        assert_eq!(mesh.vertices.len(), 129 * 129 + 129 * 2 + 129 * 4);
        assert_eq!(mesh.vertices.len(), 17_415);
        assert_eq!(mesh.triangles(), 128 * 128 * 2 + 128 * 2 * 4);
        assert_eq!(mesh.triangles(), 33_792);
    }

    /// The grid's indices are GL JS's, written its way.
    ///
    /// The loop above walks rows; GL JS walks a flat counter in steps of `meshSize + 1` and
    /// bounds it by `meshSize * meshSize`, which is the same 128 rows arrived at sideways. The
    /// two are transcribed separately here and compared, because "the same said more simply" is
    /// the claim the comment makes and it is worth more as a check than as a comment.
    #[test]
    fn the_grid_indices_are_the_references() {
        const MESH: usize = MESH_SIZE as usize;
        let mut want: Vec<u16> = Vec::new();
        let mesh_size2 = MESH * MESH;
        let mut y = 0;
        while y < mesh_size2 {
            for x in 0..MESH {
                #[allow(clippy::cast_possible_truncation)]
                want.extend_from_slice(&[
                    (x + y) as u16,
                    (MESH + x + y + 1) as u16,
                    (MESH + x + y + 2) as u16,
                    (x + y) as u16,
                    (MESH + x + y + 2) as u16,
                    (x + y + 1) as u16,
                ]);
            }
            y += MESH + 1;
        }
        let mesh = mesh();
        assert_eq!(&mesh.indices[..want.len()], &want[..]);
        assert_eq!(want.len(), 128 * 128 * 6);
    }

    /// Every index addresses a vertex, and the whole mesh fits the `u16` the wire carries.
    ///
    /// 17,415 is comfortably inside 65,536 and would not be at a mesh size of 180. Asserting it
    /// is what turns raising `MESH_SIZE` from a silent wrap into a failing test.
    #[test]
    fn every_index_is_in_range() {
        let mesh = mesh();
        assert!(mesh.vertices.len() <= usize::from(u16::MAX));
        for index in &mesh.indices {
            assert!(usize::from(*index) < mesh.vertices.len(), "{index}");
        }
        assert_eq!(mesh.indices.len() % 3, 0);
    }

    /// The grid spans the tile exactly, corner to corner, with no vertex outside it.
    #[test]
    fn the_grid_spans_the_tile() {
        let mesh = mesh();
        let grid = &mesh.vertices[..mesh.grid_vertices];
        assert_eq!(grid[0], [0, 0, 0]);
        assert_eq!(grid[128], [8192, 0, 0]);
        assert_eq!(grid[129 * 128], [0, 8192, 0]);
        assert_eq!(grid[129 * 129 - 1], [8192, 8192, 0]);
        for vertex in grid {
            assert!((0..=8192).contains(&vertex[0]), "{vertex:?}");
            assert!((0..=8192).contains(&vertex[1]), "{vertex:?}");
            // The grid is the surface: nothing in it is flagged as a skirt.
            assert_eq!(vertex[2], 0, "{vertex:?}");
        }
    }

    /// The grid's own triangles all wind the same way, which is what lets a terrain cull.
    ///
    /// Measured as a signed area in tile coordinates, where y runs south: a consistently negative
    /// cross product is one winding, and a mesh with both signs in it has a hole wherever the
    /// camera is on the wrong side.
    #[test]
    fn the_grid_winds_one_way() {
        let mesh = mesh();
        let surface = 128 * 128 * 2 * 3;
        for triangle in mesh.indices[..surface].as_chunks::<3>().0 {
            let point = |index: u16| {
                let vertex = mesh.vertices[usize::from(index)];
                (f64::from(vertex[0]), f64::from(vertex[1]))
            };
            let (ax, ay) = point(triangle[0]);
            let (bx, by) = point(triangle[1]);
            let (cx, cy) = point(triangle[2]);
            let cross = (bx - ax) * (cy - ay) - (by - ay) * (cx - ax);
            assert!(cross < 0.0, "{triangle:?} {cross}");
        }
    }

    /// Every skirt vertex sits on an edge of the tile and is flagged.
    ///
    /// The flag is the whole of what makes it a skirt -- the shader drops a vertex by the skirt
    /// length only when `z` is one -- so a skirt vertex that lost its flag is a hole in the
    /// curtain and a surface vertex that gained one is a notch cut out of the ground.
    #[test]
    fn a_skirt_hangs_from_an_edge() {
        let mesh = mesh();
        let skirt = &mesh.vertices[mesh.grid_vertices..];
        let mut flagged = 0;
        for vertex in skirt {
            let on_edge =
                vertex[0] == 0 || vertex[0] == 8192 || vertex[1] == 0 || vertex[1] == 8192;
            assert!(on_edge, "{vertex:?}");
            flagged += usize::from(vertex[2] == 1);
        }
        // The north and south rows are all skirt; the east and west columns are pairs, half of
        // which are the surface vertices the curtain hangs from.
        assert_eq!(flagged, 129 * 2 + 129 * 2);
    }

    /// The east and west skirts interleave a surface vertex with its dropped twin, in that order.
    #[test]
    fn the_side_skirts_are_pairs() {
        let mesh = mesh();
        let sides = &mesh.vertices[mesh.grid_vertices + 129 * 2..];
        assert_eq!(sides.len(), 129 * 4);
        for pair in sides.as_chunks::<2>().0 {
            assert_eq!(pair[0][2], 0, "{pair:?}");
            assert_eq!(pair[1][2], 1, "{pair:?}");
            // Same place on the map, different height.
            assert_eq!(pair[0][0], pair[1][0]);
            assert_eq!(pair[0][1], pair[1][1]);
        }
        assert_eq!(sides[0][0], 0, "the west column first");
        assert_eq!(sides[129 * 2][0], 8192, "then the east");
    }

    /// The skirt is a fifth of the ground a tile covers, and halves with every zoom.
    #[test]
    fn a_skirt_shortens_with_the_zoom() {
        let zero = skirt_length(0.0);
        assert!(
            (zero - core::f64::consts::TAU * 6_378_137.0 / 5.0).abs() < 1e-6,
            "{zero}"
        );
        for zoom in 1..20 {
            let here = skirt_length(f64::from(zoom));
            let above = skirt_length(f64::from(zoom - 1));
            assert!((here * 2.0 - above).abs() < 1e-6, "{zoom}: {here} {above}");
        }
        // A camera above the world is not a shorter skirt than one at zoom zero.
        assert_eq!(skirt_length(-3.0), zero);
    }
}
