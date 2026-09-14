//! Color ramps: an expression over a 0..1 parameter, baked into a 256x1 texture.
//!
//! Transcribed from mbgl's `RenderLayer::applyColorRamp` (`renderer/render_layer.cpp`). Two
//! properties want one: `heatmap-color`, read through `["heatmap-density"]`, and
//! `line-gradient`, read through `["line-progress"]`. The shader samples the result rather than
//! evaluating the expression per fragment, which is the whole point — a ramp is evaluated 256
//! times per style change instead of once per pixel per frame.
//!
//! # The parameter never reaches one
//!
//! mbgl walks *bytes*, not texels:
//!
//! ```cpp
//! const auto length = image.bytes();            // 256 * 4
//! for (uint32_t i = 0; i < length; i += 4) {
//!     const auto color = colorValue.evaluate(static_cast<double>(i) / length);
//! ```
//!
//! so texel `n` is evaluated at `4n / 1024`, which is `n / 256`. The last texel is at
//! `255/256`, and **the ramp's final stop is never sampled**. A ramp ending in red is a ramp
//! whose hottest texel is very slightly not red, and a builder that divided by 255 to "reach
//! the end" would differ from the oracle in the three channels that matter most.
//!
//! # Floor, not round
//!
//! `static_cast<uint8_t>(std::floor(color.r * 255.f))`. Rounding instead moves roughly half the
//! texels by one, which against a smooth ramp is invisible in a screenshot and is not invisible
//! to a hash.

use alloc::vec::Vec;

use crate::expression::{EvaluationError, Expression};
use crate::property::Color;

/// Texels in a ramp. mbgl's `Size(256, 1)`.
pub const RAMP_TEXELS: usize = 256;

/// Bytes in a ramp: RGBA per texel.
pub const RAMP_BYTES: usize = RAMP_TEXELS * 4;

/// The spec's default `heatmap-color`, as the style would have written it.
///
/// A default that is an expression, which is why [`crate::property::PropertySpec`] cannot hold
/// it: that table's defaults are constants. mbgl has the same problem and the same answer —
/// `HeatmapColor::defaultValue()` returns a parsed expression rather than a `Color`.
///
/// The oracle pins it. A heatmap layer that sets no `heatmap-color` uploads a ramp whose bytes
/// hash to the same value as this string baked, so "the default is the six-stop ramp" is a
/// measured fact here rather than a reading of the spec.
pub const DEFAULT_HEATMAP_COLOR: &str = r#"["interpolate",["linear"],["heatmap-density"],
    0,"rgba(0, 0, 255, 0)",
    0.1,"royalblue",
    0.3,"cyan",
    0.5,"lime",
    0.7,"yellow",
    1,"red"]"#;

/// What the ramp's parameter is spelled as, which decides how it is fed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RampParameter {
    /// `["heatmap-density"]`, for `heatmap-color`.
    HeatmapDensity,
}

/// Bakes a ramp expression into `RAMP_BYTES` of straight RGBA.
///
/// # Errors
///
/// [`EvaluationError`] from the expression, and the first one rather than a partial texture: a
/// ramp that evaluates for some densities and not others is a style fault, and half a ramp
/// uploaded is worse than none.
pub fn bake(expression: &Expression, parameter: RampParameter) -> Result<Vec<u8>, EvaluationError> {
    let mut out = Vec::with_capacity(RAMP_BYTES);
    for texel in 0..RAMP_TEXELS {
        // `4 * texel / 1024`, as the module note explains, not `texel / 255`.
        #[allow(clippy::cast_precision_loss)]
        let parameter_value = (4 * texel) as f64 / RAMP_BYTES as f64;
        let value = match parameter {
            RampParameter::HeatmapDensity => {
                expression.evaluate_at(None, None, None, None, None, Some(parameter_value))?
            }
        };
        let crate::value::Value::Color(color) = value else {
            return Err(EvaluationError::Type {
                expected: "color",
                got: value.type_name(),
            });
        };
        out.extend_from_slice(&texel_bytes(color));
    }
    debug_assert_eq!(out.len(), RAMP_BYTES);
    Ok(out)
}

/// One texel, as mbgl narrows it.
fn texel_bytes(color: Color) -> [u8; 4] {
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let channel = |component: f32| (component * 255.0).floor() as u8;
    [
        channel(color.r),
        channel(color.g),
        channel(color.b),
        channel(color.a),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::expression::{PropertySpec, Type};
    use crate::value::Value;

    /// `heatmap-color` parses against a color expectation, the way mbgl's
    /// `Converter<ColorRampPropertyValue>` builds its `ParsingContext(type::Color)`. Without it
    /// the stops stay strings and `interpolate` refuses them as not interpolatable.
    fn ramp(json: &str) -> Expression {
        let value: Value = serde_json::from_str(json).expect("valid json");
        Expression::parse_for(
            &value,
            &PropertySpec {
                default: None,
                expected: Some(Type::Color),
            },
        )
        .expect("parses")
    }

    /// Opaque at both ends, so only the three color channels move and the arithmetic below is
    /// exactly `255 * n / 256`.
    const TWO_STOP: &str = r#"["interpolate",["linear"],["heatmap-density"],
        0,"rgb(0, 0, 0)",1,"rgb(255, 255, 255)"]"#;

    #[test]
    fn a_ramp_is_256_rgba_texels() {
        let baked = bake(&ramp(TWO_STOP), RampParameter::HeatmapDensity).expect("bakes");
        assert_eq!(baked.len(), RAMP_BYTES);
        assert_eq!(baked.len(), RAMP_TEXELS * 4);
    }

    /// The parameter is `4n / 1024`, so the last texel is at `255/256` and the final stop is
    /// never sampled. A ramp from black to white ends at 254, not 255 — which is the whole of
    /// the module note, in one assertion.
    #[test]
    fn the_last_texel_is_not_the_last_stop() {
        let baked = bake(&ramp(TWO_STOP), RampParameter::HeatmapDensity).expect("bakes");
        assert_eq!(&baked[..4], &[0, 0, 0, 255], "the first stop, exactly");
        assert_eq!(
            &baked[RAMP_BYTES - 4..],
            &[254, 254, 254, 255],
            "one short of white, because the parameter stops at 255/256"
        );
    }

    /// Floor, not round.
    ///
    /// Texel `n` is `255 * n / 256`, whose fractional part is `(256 - n) / 256` — at or past a
    /// half for every `n` in `1..=128`. So rounding instead moves exactly half the ramp by one,
    /// which no screenshot shows and every hash does.
    #[test]
    fn channels_floor_rather_than_round() {
        let baked = bake(&ramp(TWO_STOP), RampParameter::HeatmapDensity).expect("bakes");
        let rounded_would_differ = (0..RAMP_TEXELS)
            .filter(|texel| {
                #[allow(clippy::cast_precision_loss, clippy::cast_possible_truncation)]
                let exact = 255.0 * (*texel as f32) / 256.0;
                (exact.round() as u8) != baked[texel * 4]
            })
            .count();
        assert_eq!(rounded_would_differ, 128);
    }

    /// A ramp that does not read the density is legal and constant — the spec's own default for
    /// `line-gradient` shape — and produces 256 identical texels rather than an error.
    #[test]
    fn a_constant_ramp_is_a_flat_one() {
        let baked = bake(
            &ramp(r#"["to-color", "rgb(10, 20, 30)"]"#),
            RampParameter::HeatmapDensity,
        )
        .expect("bakes");
        assert!(baked.chunks(4).all(|texel| texel == [10, 20, 30, 255]));
    }

    /// An expression that is not a color is refused rather than narrowed to one.
    #[test]
    fn a_ramp_that_is_not_a_color_is_an_error() {
        let value: Value = serde_json::from_str(r#"["heatmap-density"]"#).expect("valid json");
        let expression = Expression::parse(&value).expect("parses");
        assert!(bake(&expression, RampParameter::HeatmapDensity).is_err());
    }
}
