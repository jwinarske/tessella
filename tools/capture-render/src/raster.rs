//! A software rasterizer, deliberately naive.
//!
//! It is a scanline fill over triangles with alpha blending, and nothing else — no depth buffer,
//! no stencil, no SDF. That is enough to answer the question this tool exists to answer, which is
//! whether the geometry on the wire describes the shapes the style asked for and lands where the
//! matrices put it. A wrong stride is a field of noise, a wrong matrix is a shape in the wrong
//! place, and both are visible at a glance and invisible to a test that asks the producer what it
//! computed.

/// An RGBA target.
pub(crate) struct Canvas {
    /// Width in pixels.
    pub(crate) width: u32,
    /// Height in pixels.
    pub(crate) height: u32,
    /// Rows of RGBA, top-left origin.
    pub(crate) pixels: Vec<u8>,
}

impl Canvas {
    /// A canvas filled with one colour.
    pub(crate) fn new(width: u32, height: u32, fill: [f32; 4]) -> Self {
        let rgba = to_bytes(fill);
        Self {
            width,
            height,
            pixels: rgba
                .iter()
                .copied()
                .cycle()
                .take(width as usize * height as usize * 4)
                .collect(),
        }
    }

    /// Blends one triangle, given in pixel coordinates.
    ///
    /// Half-open on the right and bottom edges, so two triangles sharing one produce no seam and
    /// no doubled blend along it — which on a translucent layer is a visible bright line.
    pub(crate) fn triangle(&mut self, a: [f32; 2], b: [f32; 2], c: [f32; 2], color: [f32; 4]) {
        let min_x = a[0].min(b[0]).min(c[0]).floor().max(0.0);
        let max_x = a[0].max(b[0]).max(c[0]).ceil().min(self.width as f32);
        let min_y = a[1].min(b[1]).min(c[1]).floor().max(0.0);
        let max_y = a[1].max(b[1]).max(c[1]).ceil().min(self.height as f32);
        if min_x >= max_x || min_y >= max_y {
            return;
        }

        let area = edge(a, b, c);
        if area.abs() < f32::EPSILON {
            return;
        }
        let flip = area < 0.0;

        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        for y in min_y as u32..max_y as u32 {
            for x in min_x as u32..max_x as u32 {
                let p = [x as f32 + 0.5, y as f32 + 0.5];
                let (w0, w1, w2) = (edge(b, c, p), edge(c, a, p), edge(a, b, p));
                let inside = if flip {
                    w0 <= 0.0 && w1 <= 0.0 && w2 <= 0.0
                } else {
                    w0 >= 0.0 && w1 >= 0.0 && w2 >= 0.0
                };
                if inside {
                    self.blend(x, y, color);
                }
            }
        }
    }

    fn blend(&mut self, x: u32, y: u32, color: [f32; 4]) {
        let at = (y as usize * self.width as usize + x as usize) * 4;
        let Some(target) = self.pixels.get_mut(at..at + 4) else {
            return;
        };
        let alpha = color[3].clamp(0.0, 1.0);
        for channel in 0..3 {
            let src = color[channel].clamp(0.0, 1.0) * 255.0;
            let dst = f32::from(target[channel]);
            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
            {
                target[channel] = (src * alpha + dst * (1.0 - alpha)).round() as u8;
            }
        }
        target[3] = 255;
    }
}

impl Canvas {
    /// Blends one triangle, taking its coverage from a sampled field rather than from the shape.
    ///
    /// The corners carry texture coordinates and `sample` answers a coverage in `0..=1` for a
    /// point in that space. That is what a glyph is: the quad is a rectangle, and the letter
    /// inside it comes out of the atlas — so drawing symbol quads as solid boxes would say a
    /// label is *somewhere* and nothing about whether the right glyphs were shaped, packed and
    /// addressed.
    pub(crate) fn sampled_triangle(
        &mut self,
        points: [[f32; 2]; 3],
        uv: [[f32; 2]; 3],
        color: [f32; 4],
        sample: &dyn Fn([f32; 2]) -> f32,
    ) {
        let [a, b, c] = points;
        let min_x = a[0].min(b[0]).min(c[0]).floor().max(0.0);
        let max_x = a[0].max(b[0]).max(c[0]).ceil().min(self.width as f32);
        let min_y = a[1].min(b[1]).min(c[1]).floor().max(0.0);
        let max_y = a[1].max(b[1]).max(c[1]).ceil().min(self.height as f32);
        if min_x >= max_x || min_y >= max_y {
            return;
        }
        let area = edge(a, b, c);
        if area.abs() < f32::EPSILON {
            return;
        }

        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        for y in min_y as u32..max_y as u32 {
            for x in min_x as u32..max_x as u32 {
                let p = [x as f32 + 0.5, y as f32 + 0.5];
                // Barycentric, normalized by the signed area so the winding cancels and the
                // weights are the same either way round.
                let (w0, w1, w2) = (
                    edge(b, c, p) / area,
                    edge(c, a, p) / area,
                    edge(a, b, p) / area,
                );
                if w0 < 0.0 || w1 < 0.0 || w2 < 0.0 {
                    continue;
                }
                let at = [
                    uv[0][0] * w0 + uv[1][0] * w1 + uv[2][0] * w2,
                    uv[0][1] * w0 + uv[1][1] * w1 + uv[2][1] * w2,
                ];
                let coverage = sample(at);
                if coverage > 0.0 {
                    let mut blended = color;
                    blended[3] *= coverage;
                    self.blend(x, y, blended);
                }
            }
        }
    }
}

fn edge(a: [f32; 2], b: [f32; 2], p: [f32; 2]) -> f32 {
    (b[0] - a[0]) * (p[1] - a[1]) - (b[1] - a[1]) * (p[0] - a[0])
}

fn to_bytes(color: [f32; 4]) -> [u8; 4] {
    let mut out = [0u8; 4];
    for (index, channel) in color.iter().enumerate() {
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        {
            out[index] = (channel.clamp(0.0, 1.0) * 255.0).round() as u8;
        }
    }
    out[3] = 255;
    out
}

/// Applies a column-major 4x4 to a tile-space point and lands it in pixels.
///
/// Column-major because that is what the drawable buffers carry — mbgl's matrices are, and they
/// are transcribed rather than rebuilt. Reading one as row-major transposes it, which for a map
/// projection is a picture that looks plausible and is wrong everywhere.
pub(crate) fn project(
    matrix: &[f32; 16],
    x: f32,
    y: f32,
    width: f32,
    height: f32,
) -> Option<[f32; 2]> {
    project_3d(matrix, x, y, 0.0, width, height)
}

/// As [`project`], with a height above the tile plane.
///
/// The third column is not decoration here. An extrusion's roof sits at `height * heightFactor`
/// in tile space, and dropping the term draws every building flat on the ground — a picture that
/// looks like a fill layer, from a matrix that was right all along.
pub(crate) fn project_3d(
    matrix: &[f32; 16],
    x: f32,
    y: f32,
    z: f32,
    width: f32,
    height: f32,
) -> Option<[f32; 2]> {
    let clip = [
        matrix[0] * x + matrix[4] * y + matrix[8] * z + matrix[12],
        matrix[1] * x + matrix[5] * y + matrix[9] * z + matrix[13],
        matrix[3] * x + matrix[7] * y + matrix[11] * z + matrix[15],
    ];
    let w = clip[2];
    if w.abs() < 1e-6 {
        return None;
    }
    let ndc = [clip[0] / w, clip[1] / w];
    // Y flips: clip space is up-positive and a raster row index is not.
    Some([
        (ndc[0] * 0.5 + 0.5) * width,
        (1.0 - (ndc[1] * 0.5 + 0.5)) * height,
    ])
}
