//! The style light, which travels in the camera block (§2.2, §6.3).
//!
//! # Why the light rides with the camera
//!
//! It is a property of the scene, not of any layer, and it is one value per frame. Putting it on
//! the camera block is what keeps it from being repeated per drawable — and it changes at camera
//! rate anyway, because a viewport-anchored light turns as the map turns.
//!
//! # The arithmetic is `f32`, and that is not incidental
//!
//! mbgl stores the light position as three `float`s and computes the cartesian direction in
//! `float` throughout — `deg2radf` rounds to `f32` after the multiply *and* after the divide,
//! then `cosf` and `sinf` round again. Computing the same expression in `f64` and narrowing once
//! at the end gives a different answer: the golden dump's direction is
//! `0.28750020265579224`, and the exact real number is `0.2875`. The two differ in the seventh
//! digit, which is what a `float` angle costs, and reproducing it means rounding where mbgl
//! rounds rather than where the algebra suggests.
//!
//! That is the same lesson the projection taught, arrived at from the other side: there the
//! field of view was read as `float` in one place and `double` in another, and here the whole
//! computation is `float`. Neither is recoverable from the formula.

use alloc::string::ToString;

use crate::property::{Color, PropertyError};
use crate::value::Value;

/// Where a light is anchored.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Anchor {
    /// Fixed to the viewport: the light stays put as the map rotates. mbgl's default.
    #[default]
    Viewport,
    /// Fixed to the map: the light turns with it.
    Map,
}

/// A style light, evaluated.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Light {
    /// Radial, azimuthal, polar — mbgl's spherical triple, in `f32` as it stores them.
    pub position: [f32; 3],
    /// Light color.
    pub color: Color,
    /// Light intensity, 0..1.
    pub intensity: f32,
    /// What the light is anchored to.
    pub anchor: Anchor,
}

impl Default for Light {
    /// mbgl's defaults: `[1.15, 210, 30]`, white, half intensity, viewport-anchored.
    ///
    /// These are what the oracle emits for a style with no `light` member, which is the case the
    /// golden dump exercises.
    fn default() -> Self {
        Self {
            position: [1.15, 210.0, 30.0],
            color: Color {
                r: 1.0,
                g: 1.0,
                b: 1.0,
                a: 1.0,
            },
            intensity: 0.5,
            anchor: Anchor::Viewport,
        }
    }
}

/// Degrees to radians in `f32`, rounding after each operation as mbgl's `deg2radf` does.
///
/// Written as two steps rather than one because that is what it is: `deg * pi_v<float> / 180.0f`
/// rounds the product before dividing. Folding it into a single multiply by a precomputed
/// constant changes the last bits, and the last bits are the whole comparison.
#[must_use]
pub fn deg2radf(degrees: f32) -> f32 {
    let scaled = degrees * core::f32::consts::PI;
    scaled / 180.0
}

impl Light {
    /// The direction toward the light, cartesian.
    ///
    /// mbgl treats compass north as zero where it is really ninety degrees, and corrects by
    /// adding ninety to the azimuth here. The correction is part of the stored convention rather
    /// than of the spherical-to-cartesian identity, so it belongs in this function and not in
    /// the caller.
    #[must_use]
    pub fn cartesian(&self) -> [f32; 3] {
        let [radial, azimuthal, polar] = self.position;
        let a = deg2radf(azimuthal + 90.0);
        let p = deg2radf(polar);
        [
            radial * a.cos() * p.sin(),
            radial * a.sin() * p.sin(),
            radial * p.cos(),
        ]
    }

    /// The light a style declares, or the default when it declares none.
    ///
    /// # Errors
    ///
    /// [`PropertyError`] when a member is present but not the type the spec gives it.
    pub fn resolve(light: Option<&Value>) -> Result<Self, PropertyError> {
        let Some(Value::Object(members)) = light else {
            // A style with no light, or a light that is not an object, gets the defaults. mbgl
            // does the same: the light is optional and every member of it is optional too.
            return Ok(Self::default());
        };

        let mut resolved = Self::default();
        if let Some(value) = members.get("position") {
            resolved.position = position_of(value)?;
        }
        if let Some(Value::String(text)) = members.get("color") {
            resolved.color = Color::parse(text)?;
        }
        if let Some(Value::Number(number)) = members.get("intensity") {
            #[allow(clippy::cast_possible_truncation)]
            {
                resolved.intensity = *number as f32;
            }
        }
        if let Some(Value::String(text)) = members.get("anchor") {
            resolved.anchor = match text.as_str() {
                "map" => Anchor::Map,
                "viewport" => Anchor::Viewport,
                other => {
                    return Err(PropertyError::Color {
                        text: other.to_string(),
                    });
                }
            };
        }
        Ok(resolved)
    }
}

/// A `[radial, azimuthal, polar]` triple.
fn position_of(value: &Value) -> Result<[f32; 3], PropertyError> {
    let Value::Array(items) = value else {
        return Err(PropertyError::Color {
            text: "light position must be an array".to_string(),
        });
    };
    if items.len() != 3 {
        return Err(PropertyError::Color {
            text: "light position must have three members".to_string(),
        });
    }
    let mut position = [0.0f32; 3];
    for (slot, item) in position.iter_mut().zip(items) {
        let Value::Number(number) = item else {
            return Err(PropertyError::Color {
                text: "light position must be numbers".to_string(),
            });
        };
        #[allow(clippy::cast_possible_truncation)]
        {
            *slot = *number as f32;
        }
    }
    Ok(position)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The oracle's light, as f64 bit patterns from the golden dump. The capture widens mbgl's
    /// `float` cartesian to `double` on the way out, so these are exactly representable.
    const ORACLE_DIRECTION: [u64; 3] = [
        0x3fd2_6667_4000_0000,
        0xbfdf_dea6_6000_0000,
        0x3fef_dea6_e000_0000,
    ];

    /// The default light's direction reproduces the oracle bit for bit.
    #[test]
    fn the_default_direction_matches_the_oracle() {
        let cartesian = Light::default().cartesian();
        for (index, value) in cartesian.iter().enumerate() {
            assert_eq!(
                f64::from(*value).to_bits(),
                ORACLE_DIRECTION[index],
                "component {index}: {value:?}"
            );
        }
    }

    /// And computing the same expression in f64 does *not* reproduce it, which is why the
    /// arithmetic is f32 throughout rather than narrowed at the end.
    ///
    /// The real number is 0.2875 exactly; the oracle's is 0.28750020265579224. The difference is
    /// what an f32 angle costs, and it is not recoverable from the formula.
    #[test]
    fn the_same_expression_in_f64_does_not_match() {
        let radial = 1.15_f64;
        let a = (210.0_f64 + 90.0) * core::f64::consts::PI / 180.0;
        let p = 30.0_f64 * core::f64::consts::PI / 180.0;
        let x = radial * a.cos() * p.sin();

        assert!(
            (x - 0.2875).abs() < 1e-12,
            "f64 lands on the real number: {x}"
        );
        assert_ne!(
            x.to_bits(),
            ORACLE_DIRECTION[0],
            "which is not what the oracle emits"
        );
    }

    /// `deg2radf` rounds after the multiply and again after the divide. Folding the constant
    /// changes the result, so the two-step form is load-bearing.
    #[test]
    fn deg2radf_rounds_between_its_two_operations() {
        let mut folded_differs = 0;
        for degrees in 0..3600 {
            let d = degrees as f32 / 10.0;
            let stepwise = deg2radf(d);
            let folded = d * (core::f32::consts::PI / 180.0);
            if stepwise.to_bits() != folded.to_bits() {
                folded_differs += 1;
            }
        }
        assert!(
            folded_differs > 0,
            "folding the constant would be a different function"
        );
    }

    /// A style with no light gets mbgl's defaults, which is the case the golden dump exercises.
    #[test]
    fn a_style_without_a_light_gets_the_defaults() {
        assert_eq!(Light::resolve(None).expect("resolves"), Light::default());
        assert_eq!(
            Light::resolve(Some(&Value::Null)).expect("resolves"),
            Light::default()
        );
    }

    /// A declared light overrides only the members it declares.
    #[test]
    fn a_declared_light_overrides_member_by_member() {
        let value: Value =
            serde_json::from_str(r#"{"intensity": 0.9, "anchor": "map"}"#).expect("parses");
        let light = Light::resolve(Some(&value)).expect("resolves");

        assert_eq!(light.intensity, 0.9);
        assert_eq!(light.anchor, Anchor::Map);
        assert_eq!(
            light.position,
            Light::default().position,
            "an undeclared member keeps its default"
        );
        assert_eq!(light.color, Light::default().color);
    }

    /// A declared position changes the direction, so the default is not baked in downstream.
    #[test]
    fn a_declared_position_moves_the_light() {
        let value: Value =
            serde_json::from_str(r#"{"position": [1.5, 90.0, 60.0]}"#).expect("parses");
        let light = Light::resolve(Some(&value)).expect("resolves");
        assert_eq!(light.position, [1.5, 90.0, 60.0]);
        assert_ne!(light.cartesian(), Light::default().cartesian());
    }

    /// A malformed position is reported rather than silently defaulted, because a light that
    /// quietly ignores the style is a scene lit wrongly with no indication why.
    #[test]
    fn a_malformed_position_is_an_error() {
        for text in [
            r#"{"position": [1.0, 2.0]}"#,
            r#"{"position": "overhead"}"#,
            r#"{"position": [1.0, 2.0, "up"]}"#,
            r#"{"anchor": "sideways"}"#,
        ] {
            let value: Value = serde_json::from_str(text).expect("parses");
            assert!(Light::resolve(Some(&value)).is_err(), "{text}");
        }
    }
}
