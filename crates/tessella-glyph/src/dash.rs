//! The dash atlas: a `line-dasharray` turned into the distance field a line shader samples.
//!
//! mbgl's `LineAtlas`. A dashed line is not drawn as separate segments -- the geometry is one
//! continuous strip either way -- so the pattern is a *texture* the fragment stage looks the
//! current distance-along-the-line up in. What the texture holds is a signed distance to the
//! nearest dash boundary, which is what lets the shader antialias a dash end as cleanly as it
//! antialiases the line's own edge.
//!
//! # One row, 256 pixels, whatever the pattern
//!
//! Every pattern is stretched to the full width of the atlas and its true length is reported back
//! in [`Position::width`]. The shader divides by that to get from a distance in pixels to a
//! coordinate in the texture, so a two-unit dash and a two-hundred-unit dash cost the same
//! texture and differ only in the number the shader scales by.
//!
//! # Two caps, and why round needs fifteen rows
//!
//! A butt cap ends square, so the distance field is one-dimensional: the distance along the line
//! to the nearest boundary, and one row is enough. A round cap ends in a semicircle, so the
//! distance depends on how far across the line's width the sample sits as well -- which is a
//! second dimension, and the pattern is drawn into fifteen rows spanning the width.
//!
//! # What is not here
//!
//! The cross-fade. mbgl holds two patterns in one texture, the one for the zoom below and the one
//! above, and blends them so a dash that scales with zoom does not step. This builds either
//! pattern; pairing them is [`Atlas::pair`], and the blend is the shader's.

/// How wide every dash atlas is, in pixels.
///
/// mbgl's, and a power of two on purpose: the texture repeats horizontally, and GL ES 2.0 will
/// not repeat a non-power-of-two texture -- it samples black instead, which is a dashed line that
/// draws as nothing.
pub const WIDTH: u32 = 256;

/// How the ends of each dash are drawn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Cap {
    /// Square ends. One row of distance field is enough.
    Butt,
    /// Semicircular ends, which need the distance across the line as well as along it.
    Round,
}

impl Cap {
    /// How many rows a pattern with this cap occupies.
    #[must_use]
    pub const fn rows(self) -> u32 {
        match self {
            Self::Butt => 1,
            Self::Round => 15,
        }
    }

    /// Half the row count, which is the radius the round cap's second dimension spans.
    const fn reach(self) -> i32 {
        match self {
            Self::Butt => 0,
            Self::Round => 7,
        }
    }
}

/// Where one pattern sits in the atlas, and how long it really is.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Position {
    /// The pattern's length in style units, before it was stretched to [`WIDTH`].
    ///
    /// The shader divides the distance along the line by this to land in the texture, so it is
    /// what makes one 256-pixel row serve a pattern of any length.
    pub width: f32,
    /// How tall the pattern is, as a fraction of the atlas height.
    pub height: f32,
    /// The center of the pattern's rows, as a fraction of the atlas height.
    pub y: f32,
}

/// One run of the pattern: dash or gap, and where it starts and ends after stretching.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Range {
    left: f32,
    right: f32,
    is_dash: bool,
    is_zero_length: bool,
}

/// The pattern's runs, stretched to the atlas width.
///
/// An odd-length dasharray starts *and* ends with a dash, and the two are the same dash seen from
/// either side of the seam -- so the first range starts at minus the last entry's length rather
/// than at zero, which is what joins them without a visible break where the pattern repeats.
fn ranges(dasharray: &[f32], stretch: f32) -> Vec<Range> {
    let odd = dasharray.len() % 2 == 1;
    let Some(&first_len) = dasharray.first() else {
        return Vec::new();
    };
    let mut left = if odd {
        -dasharray[dasharray.len() - 1] * stretch
    } else {
        0.0
    };
    let mut right = first_len * stretch;
    let mut is_dash = true;

    let mut out = Vec::with_capacity(dasharray.len());
    out.push(Range {
        left,
        right,
        is_dash,
        is_zero_length: first_len == 0.0,
    });

    let mut run = first_len;
    for &length in &dasharray[1..] {
        is_dash = !is_dash;
        left = run * stretch;
        run += length;
        right = run * stretch;
        out.push(Range {
            left,
            right,
            is_dash,
            is_zero_length: length == 0.0,
        });
    }
    out
}

/// The signed distance, biased into a byte the way mbgl stores it.
///
/// Zero distance is 128, so the shader's threshold is the midpoint and the field runs both ways
/// from it. Saturating rather than wrapping: a pattern longer than the atlas can reach saturates
/// at the ends, where wrapping would put a dash boundary in the middle of a dash.
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn encode(distance: f32) -> u8 {
    (distance + 128.0).clamp(0.0, 255.0) as u8
}

/// Writes a butt-capped pattern into one row.
fn write_butt(mut runs: Vec<Range>, row: u32, into: &mut [u8]) {
    runs.retain(|range| !range.is_zero_length);
    if runs.is_empty() {
        return;
    }
    // Neighboring runs of the same kind are one run. A zero-length entry between two dashes has
    // just been removed, and leaving the two as separate ranges would put a boundary -- and so a
    // distance of zero, and so a seam -- in the middle of what is now one dash.
    let mut merged: Vec<Range> = Vec::with_capacity(runs.len());
    for range in runs {
        match merged.last_mut() {
            Some(last) if last.is_dash == range.is_dash => last.right = range.right,
            _ => merged.push(range),
        }
    }
    // And the seam itself, where the pattern repeats: if it begins and ends with the same kind,
    // the two are one run wrapping around, so each is told about the other's side.
    //
    // A single run is the same statement about itself, and is why there is no length guard: a
    // pattern that collapsed to one dash has no boundary anywhere, and reading the row's own
    // ends as boundaries would put a seam at both of them.
    if let (Some(&first), Some(&last)) = (merged.first(), merged.last())
        && first.is_dash == last.is_dash
    {
        #[allow(clippy::cast_precision_loss)]
        let width = WIDTH as f32;
        merged[0].left = last.left - width;
        let end = merged.len() - 1;
        merged[end].right = first.right + width;
    }

    let base = (WIDTH * row) as usize;
    let mut index = 0usize;
    for x in 0..WIDTH {
        #[allow(clippy::cast_precision_loss)]
        let at = x as f32;
        if at / merged[index].right > 1.0 && index + 1 < merged.len() {
            index += 1;
        }
        let range = merged[index];
        let nearest = (at - range.left).abs().min((at - range.right).abs());
        let signed = if range.is_dash { nearest } else { -nearest };
        into[base + x as usize] = encode(signed);
    }
}

/// Writes a round-capped pattern into the rows spanning the line's width.
///
/// The second dimension is what makes the cap round: `distance_across` runs from one edge of the
/// line to the other, and the distance to a dash *end* is the hypotenuse of that and the distance
/// along. Inside a gap the sign flips and the same hypotenuse is measured from the cap's center,
/// which is what rounds the gap's ends as well as the dash's.
fn write_round(runs: &[Range], first_row: u32, stretch: f32, reach: i32, into: &mut [u8]) {
    if runs.is_empty() {
        return;
    }
    let half = stretch * 0.5;
    for y in -reach..=reach {
        #[allow(clippy::cast_sign_loss)]
        let row = (first_row as i32 + reach + y) as u32;
        let base = (WIDTH * row) as usize;
        let mut index = 0usize;
        for x in 0..WIDTH {
            #[allow(clippy::cast_precision_loss)]
            let at = x as f32;
            let advance = if runs[index].right == 0.0 {
                x != 0
            } else {
                at / runs[index].right > 1.0
            };
            if advance && index + 1 < runs.len() {
                index += 1;
            }
            let range = runs[index];
            let nearest = (at - range.left).abs().min((at - range.right).abs());
            #[allow(clippy::cast_precision_loss)]
            let across = y as f32 / reach as f32 * (half + 1.0);
            let signed = if range.is_dash {
                let edge = half - across.abs();
                nearest.hypot(edge)
            } else {
                half - nearest.hypot(across)
            };
            into[base + x as usize] = encode(signed);
        }
    }
}

/// A dash atlas: one single-channel image, and where each pattern sits in it.
#[derive(Debug, Clone, PartialEq)]
pub struct Atlas {
    /// `WIDTH` by [`Self::height`], one byte a pixel.
    pub data: Vec<u8>,
    /// How many rows the image has, always a power of two.
    pub height: u32,
}

impl Atlas {
    /// One pattern's atlas.
    ///
    /// `None` for a dasharray of fewer than two entries, which mbgl warns about and draws as a
    /// plain line: one entry is a dash with no gap after it, which is not a pattern.
    #[must_use]
    pub fn single(dasharray: &[f32], cap: Cap) -> Option<(Self, Position)> {
        let (atlas, from, _) = Self::build(dasharray, dasharray, cap)?;
        Some((atlas, from))
    }

    /// Two patterns in one atlas, for the cross-fade between zoom levels.
    ///
    /// The two share a row when they are the same pattern, which is the ordinary case: a
    /// dasharray that does not vary with zoom is the same either side of the fade, and giving it
    /// two identical rows would double the texture for nothing.
    #[must_use]
    pub fn pair(from: &[f32], to: &[f32], cap: Cap) -> Option<(Self, Position, Position)> {
        Self::build(from, to, cap)
    }

    fn build(from: &[f32], to: &[f32], cap: Cap) -> Option<(Self, Position, Position)> {
        if from.len() < 2 || to.len() < 2 {
            return None;
        }
        let identical = from == to;
        let rows = cap.rows();
        let wanted = if identical { rows } else { 2 * rows };
        // Rounded up to a power of two, for the repeat that `WIDTH` is a power of two for.
        let height = wanted.next_power_of_two();

        let mut atlas = Self {
            data: vec![0; (WIDTH * height) as usize],
            height,
        };
        let first = atlas.write(from, 0, cap, height)?;
        let second = if identical {
            first
        } else {
            atlas.write(to, rows, cap, height)?
        };
        Some((atlas, first, second))
    }

    fn write(&mut self, dasharray: &[f32], row: u32, cap: Cap, height: u32) -> Option<Position> {
        let length: f32 = dasharray.iter().sum();
        // Negated `>` rather than `<=`, and the lint is answered rather than silenced: a NaN
        // fails every comparison, so `<= 0.0` would *accept* one and divide it into the stretch,
        // putting a NaN in every pixel of the row.
        #[allow(clippy::neg_cmp_op_on_partial_ord)]
        if !(length > 0.0) {
            return None;
        }
        #[allow(clippy::cast_precision_loss)]
        let stretch = WIDTH as f32 / length;
        let runs = ranges(dasharray, stretch);
        let reach = cap.reach();
        match cap {
            Cap::Butt => write_butt(runs, row, &mut self.data),
            Cap::Round => write_round(&runs, row, stretch, reach, &mut self.data),
        }
        #[allow(clippy::cast_precision_loss)]
        Some(Position {
            width: length,
            height: (2.0 * reach as f32 + 1.0) / height as f32,
            y: (0.5 + row as f32 + reach as f32) / height as f32,
        })
    }
}
