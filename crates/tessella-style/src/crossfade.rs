//! Cross-faded properties: the two values a pattern is between, and how far between them it is.
//!
//! # Why a pattern has two values and a colour has one
//!
//! A pattern property resolves to an image, and which image it resolves to can change with the
//! zoom — `["step", ["zoom"], "hatch-small", 14, "hatch-large"]` is an ordinary thing to write.
//! Swapping the image the instant the zoom crosses fourteen is a visible pop across every
//! polygon at once, so mbgl draws both and fades between them: `Faded { from, to }` and a mix
//! factor.
//!
//! That is why the binder for these is shaped differently from every other one. A colour binds
//! one value per feature; a pattern binds two atlas rectangles and a `t`, and the property is
//! *uninterpolated* in the usual sense — mbgl's `Interpolator<Faded<T>>` is `Uninterpolated`,
//! because the fade is the interpolation and doing it twice would be wrong.
//!
//! # Which two
//!
//! `to` is always the value at the current zoom. `from` is the value at the level being left:
//! `z - 1` when zooming in, `z + 1` when zooming out. Both come off the same expression,
//! evaluated at three zooms, and the direction is decided by comparing the zoom against the last
//! integer zoom crossed — which is why [`ZoomHistory`] has to exist at all. Nothing else in this
//! build needs to know which way the camera last moved.
//!
//! `line-dasharray` is cross-faded too, for the same reason and by the same machinery: mbgl
//! instantiates the evaluator for `Image` and for `vector<float>`.

use alloc::vec::Vec;

/// Where the zoom has been, which is what says whether the camera is zooming in or out.
///
/// mbgl's `ZoomHistory`. The interesting field is `last_integer_zoom`: crossing an integer zoom
/// is what starts a fade, and the direction of the crossing chooses which neighbour a pattern
/// fades from.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct ZoomHistory {
    /// The zoom this was last updated with.
    pub last_zoom: f64,
    /// Its floor, kept so a crossing can be detected without re-deriving it.
    pub last_floor_zoom: f64,
    /// The last integer zoom crossed.
    pub last_integer_zoom: f64,
    /// When that crossing happened, in milliseconds, or `None` before any has.
    pub last_integer_zoom_time: Option<u64>,
    /// Whether nothing has been recorded yet.
    pub first: bool,
}

impl ZoomHistory {
    /// A history that has seen nothing.
    #[must_use]
    pub fn new() -> Self {
        Self {
            first: true,
            ..Self::default()
        }
    }

    /// Records a zoom, returning whether it differed from the last.
    ///
    /// `now` is milliseconds, and `None` means "no time" — mbgl's `Clock::time_point::max()`
    /// sentinel, which it uses when evaluating outside a frame. A crossing recorded with no time
    /// is stamped as if it happened at zero, so a fade against it is already complete rather
    /// than starting from wherever the clock happens to be.
    ///
    /// The asymmetry in the two crossing branches is mbgl's and is not a slip. Zooming *out*
    /// past an integer sets `last_integer_zoom` to `floor(z) + 1` — the level just left, not the
    /// one just entered — while zooming *in* sets it to `floor(z)`, which is again the level
    /// just entered. Both name the boundary that was crossed.
    pub fn update(&mut self, z: f64, now: Option<u64>) -> bool {
        let floor = z.floor();

        if self.first {
            self.first = false;
            self.last_integer_zoom = floor;
            self.last_integer_zoom_time = Some(0);
            self.last_zoom = z;
            self.last_floor_zoom = floor;
            return true;
        }

        if self.last_floor_zoom > floor {
            self.last_integer_zoom = floor + 1.0;
            self.last_integer_zoom_time = Some(now.unwrap_or(0));
        } else if self.last_floor_zoom < floor {
            self.last_integer_zoom = floor;
            self.last_integer_zoom_time = Some(now.unwrap_or(0));
        }

        if (z - self.last_zoom).abs() > f64::EPSILON {
            self.last_zoom = z;
            self.last_floor_zoom = floor;
            return true;
        }
        false
    }
}

/// How far between a pattern's two images the frame is, and how each is scaled.
///
/// mbgl's `CrossfadeParameters`. `to_scale` is always one: the image at the current zoom is
/// drawn at its own size, and the one being left is drawn at twice or half of it depending on
/// which way the camera moved — so the pattern appears to keep its size on the ground while the
/// image under it changes.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Crossfade {
    /// Scale applied to the image being faded from.
    pub from_scale: f32,
    /// Scale applied to the image being faded to. Always one.
    pub to_scale: f32,
    /// The mix, zero at `from` and one at `to`.
    pub t: f32,
}

/// The crossfade for a zoom, given where the zoom has been.
///
/// `fade_duration` is milliseconds; zero means no fade, and then `t` is driven by the zoom's
/// fractional part alone. `now` is milliseconds, `None` meaning mbgl's "no time" sentinel, which
/// leaves the time term complete.
///
/// The two branches are not mirror images and mbgl's arithmetic is transcribed rather than
/// tidied. Zooming in, `t` starts at the fraction and runs to one; zooming out it starts at one
/// and is pulled back by the fraction. The difference is that zooming out crosses the boundary
/// at the *start* of the interval and zooming in at its end.
#[must_use]
pub fn crossfade(z: f64, history: &ZoomHistory, now: Option<u64>, fade_duration: u64) -> Crossfade {
    #[allow(clippy::cast_possible_truncation)]
    let fraction = (z - z.floor()) as f32;

    let elapsed = if fade_duration == 0 {
        1.0
    } else {
        let since = now
            .zip(history.last_integer_zoom_time)
            .map_or(0, |(now, then)| now.saturating_sub(then));
        #[allow(clippy::cast_precision_loss)]
        let ratio = since as f32 / fade_duration as f32;
        ratio.min(1.0)
    };

    if z > history.last_integer_zoom {
        Crossfade {
            from_scale: 2.0,
            to_scale: 1.0,
            t: fraction + (1.0 - fraction) * elapsed,
        }
    } else {
        Crossfade {
            from_scale: 0.5,
            to_scale: 1.0,
            t: 1.0 - (1.0 - elapsed) * fraction,
        }
    }
}

/// A property's value at the zoom being left and the zoom being entered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Faded<T> {
    /// The value at the level being left.
    pub from: T,
    /// The value at the current level.
    pub to: T,
}

/// The two values a cross-faded property is between.
///
/// `evaluate` is called at `z - 1`, `z` and `z + 1`; which of the outer two becomes `from`
/// depends on the direction the camera last crossed an integer zoom. A constant property gives
/// the same answer three times, and `from` and `to` are then equal — which is correct rather
/// than wasteful: the fade still runs, and it fades between two copies of one image.
#[must_use]
pub fn faded<T, F: Fn(f64) -> T>(evaluate: F, z: f64, history: &ZoomHistory) -> Faded<T> {
    let mid = evaluate(z);
    if z > history.last_integer_zoom {
        Faded {
            from: evaluate(z - 1.0),
            to: mid,
        }
    } else {
        Faded {
            from: evaluate(z + 1.0),
            to: mid,
        }
    }
}

/// The atlas rectangle an image occupies, as the shader reads it.
///
/// mbgl's `ImagePosition::tlbr` — the padded rectangle inset by the padding on every side, so a
/// sampler reading between these corners never picks up a neighbour's edge. Top and left first,
/// then bottom and right.
#[must_use]
pub fn tlbr(x: u16, y: u16, width: u16, height: u16, padding: u16) -> [u16; 4] {
    [
        x + padding,
        y + padding,
        x.saturating_add(width).saturating_sub(padding),
        y.saturating_add(height).saturating_sub(padding),
    ]
}

/// Every image name a cross-faded expression can resolve to, across the three zooms it is
/// evaluated at.
///
/// A tile has to know which sprites it needs before it can build a bucket that names them, and
/// a pattern that steps with zoom needs all of the ones it could step to. Deduplicated, in the
/// order first seen.
#[must_use]
pub fn names_at<F: Fn(f64) -> Option<alloc::string::String>>(
    evaluate: F,
    z: f64,
) -> Vec<alloc::string::String> {
    let mut names = Vec::new();
    for zoom in [z - 1.0, z, z + 1.0] {
        if let Some(name) = evaluate(zoom)
            && !names.contains(&name)
        {
            names.push(name);
        }
    }
    names
}

/// The four paint properties whose value is a sprite name.
///
/// One per layer type that can carry a pattern, and the spec gives each the same shape: an
/// `Image` property defaulting to nothing, so a layer that sets none resolves to null and needs
/// no sprite at all.
pub const PATTERN_PROPERTIES: [&str; 4] = [
    "background-pattern",
    "fill-extrusion-pattern",
    "fill-pattern",
    "line-pattern",
];

/// Every sprite a layer's patterns could need at `zoom`, in the order first seen.
///
/// # Why three zooms and not one
///
/// The value at the current zoom is the one drawn, and the value a level away is the one it is
/// fading from or to. A tile that fetched only the current answer would cross an integer zoom
/// and have nothing to fade against — mbgl asks its expression at `z - 1`, `z` and `z + 1` for
/// exactly this reason, and so does [`faded`].
///
/// Deduplicated, because the common case is a constant that gives one name three times, and a
/// tile should ask the sprite sheet for it once.
///
/// A property that resolves to anything but a string contributes nothing. That covers the layer
/// that set no pattern, whose default is null, and it also covers a data-driven pattern — whose
/// name depends on the feature, so a zoom alone cannot answer it. Those are collected per
/// feature at bucket build instead.
#[must_use]
pub fn pattern_names<'a, P, E>(paint: P, zoom: f64) -> Vec<alloc::string::String>
where
    P: Fn(&str) -> Option<&'a E>,
    E: PatternSource + 'a,
{
    let mut names = Vec::new();
    for property in PATTERN_PROPERTIES {
        let Some(source) = paint(property) else {
            continue;
        };
        for name in names_at(|z| source.image_at(z), zoom) {
            if !names.contains(&name) {
                names.push(name);
            }
        }
    }
    names
}

/// A paint property that may name a sprite at a zoom.
///
/// A trait rather than a concrete type so this module stays arithmetic: what a property *is*
/// belongs to `property`, and what a fade *needs* belongs here.
pub trait PatternSource {
    /// The sprite this names at `zoom`, if it names one.
    fn image_at(&self, zoom: f64) -> Option<alloc::string::String>;
}
