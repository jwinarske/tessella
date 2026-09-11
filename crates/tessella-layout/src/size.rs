//! `text-size` and `icon-size`: which of them the shader reads, and from where.
//!
//! # Why size is not an ordinary layout property
//!
//! Every other layout property is read once when the tile is built. Size cannot be, because it
//! decides how large the glyphs *draw* as well as how they were shaped, and the camera's zoom
//! moves between the build and the frame. So mbgl gives it a binder of its own --
//! `SymbolSizeBinder`, "a 'custom' scheme for encoding the necessary attribute data" -- with
//! three implementations and two flags on the wire saying which one produced the drawable.
//!
//! The three are the four combinations of "varies with zoom" and "varies with the feature", with
//! the two zoom-only cases sharing an implementation:
//!
//! | | zoom-constant | zoom-varying |
//! |---|---|---|
//! | **feature-constant** | a uniform | a uniform, re-evaluated per frame |
//! | **feature-varying** | the vertex's own size | the vertex's two sizes, mixed per frame |
//!
//! The flags are what tell the shader which column it is in. Hardcoding both to "constant" --
//! which this did -- makes every label take the uniform, and the uniform is the layer's size
//! evaluated with no feature in hand. For `["interpolate", ["linear"], ["zoom"], …, ["case",
//! ["<", ["get", "population_rank"], 8], 12, 22]]` that evaluation cannot answer at all, so the
//! size falls back to the spec's sixteen and a capital is set at the size of a village.
//!
//! # The covering stops
//!
//! A composite size is sampled at the *stops* enclosing the tile's zoom interval rather than at
//! the interval's own ends. That is the one place mbgl uses `getCoveringStops`, and it is not
//! how the paint binders work -- see [`tessella_style::Expression::covering_stops`], which says
//! what the difference costs and why a golden dump settles it.

use tessella_style::expression::Feature;
use tessella_style::property::layout_value;
use tessella_style::{Expression, Layer, PropertyValue, Value};

use crate::symbol_bucket::SizeRange;

/// What the shader is told about a size, for one frame.
///
/// mbgl's `ZoomEvaluatedSize`, minus its `layoutSize`: that one is the *layout's* size and
/// travels with the shaped label rather than with the drawable.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct EvaluatedSize {
    /// Whether the size is the same at every zoom.
    pub zoom_constant: bool,
    /// Whether it is the same for every feature.
    pub feature_constant: bool,
    /// Where between the vertex's two sizes this frame sits. Zero unless both vary.
    pub size_t: f32,
    /// The size itself, when the shader reads a uniform rather than the vertex.
    pub size: f32,
}

/// How a layer's `text-size` or `icon-size` reaches the shader.
///
/// One per layer per property, built when the tile is. It holds the parsed expression because
/// the frame needs it twice more: once to sample the covering stops, once for the mix factor.
///
/// Boxed, and not to quiet a lint. An `Expression` is eighty bytes and two of these live on a
/// `SymbolLayout`, which is a variant of the `Content` enum every bucket is moved as -- so the
/// size a symbol layer carries is paid by every fill and every line in the frame. The expression
/// is read once per layer at build and once per layer per frame; the indirection costs nothing
/// that is measured and saves a hundred and sixty bytes on the hottest type in the tile path.
#[derive(Debug, Clone, PartialEq)]
pub enum SizeBinding {
    /// A literal, or a property the style did not set.
    Constant(f32),
    /// Varies with zoom and not with the feature: mbgl's `ConstantSymbolSizeBinder` holding an
    /// expression. The two sizes are the curve at the covering stops, and the frame interpolates
    /// between them rather than evaluating at the camera's zoom -- mbgl's own comment says why:
    /// to stay consistent with the restriction composite sizes are under.
    Zoom {
        /// The parsed `text-size`, for the mix factor.
        expression: alloc::boxed::Box<Expression>,
        /// The stops enclosing the tile's zoom interval.
        covering: (f64, f64),
        /// The curve at each of them.
        sizes: (f32, f32),
    },
    /// Varies with the feature and not with zoom: `SourceFunctionSymbolSizeBinder`. The size is
    /// in the vertex and there is nothing per frame to say about it, so the drawable's own size
    /// and mix factor go unread.
    Feature {
        /// The parsed `text-size`, evaluated per feature with no zoom.
        expression: alloc::boxed::Box<Expression>,
    },
    /// Both: `CompositeFunctionSymbolSizeBinder`. The vertex carries the feature's size at each
    /// covering stop and the frame carries where between them the camera is.
    Composite {
        /// The parsed `text-size`, for the mix factor and for the per-feature samples.
        expression: alloc::boxed::Box<Expression>,
        /// The stops enclosing the tile's zoom interval.
        covering: (f64, f64),
    },
}

impl SizeBinding {
    /// Classifies a layer's size property at a tile's zoom.
    ///
    /// `default` is the property's own spec default -- sixteen pixels for `text-size`, a
    /// multiplier of one for `icon-size` -- used where the style wrote nothing, or wrote
    /// something that does not evaluate.
    #[must_use]
    pub fn of(layer: &Layer, key: &str, tile_zoom: f64, default: f32) -> Self {
        let constant = |zoom: f64| -> f32 {
            #[allow(clippy::cast_possible_truncation)]
            layout_value(layer, key, zoom, None)
                .as_ref()
                .and_then(Value::as_number)
                .map_or(default, |value| value as f32)
        };
        let Some(PropertyValue::Expression(raw)) = layer.layout.get(key) else {
            // A literal, or absent. Either way the same number at every zoom for every feature.
            return Self::Constant(constant(tile_zoom));
        };
        let Ok(expression) = Expression::parse(raw.value()) else {
            return Self::Constant(default);
        };
        let dependency = expression.dependency();
        let covering = expression
            .covering_stops(tile_zoom, tile_zoom + 1.0)
            .unwrap_or((tile_zoom, tile_zoom + 1.0));
        #[allow(clippy::cast_possible_truncation)]
        let at = |zoom: f64| -> f32 {
            expression
                .evaluate(Some(zoom), None)
                .ok()
                .as_ref()
                .and_then(Value::as_number)
                .map_or(default, |value| value as f32)
        };
        match (dependency.needs_zoom(), dependency.needs_feature()) {
            (false, false) => Self::Constant(constant(tile_zoom)),
            (true, false) => {
                let sizes = (at(covering.0), at(covering.1));
                Self::Zoom {
                    expression: alloc::boxed::Box::new(expression),
                    covering,
                    sizes,
                }
            }
            (false, true) => Self::Feature {
                expression: alloc::boxed::Box::new(expression),
            },
            (true, true) => Self::Composite {
                expression: alloc::boxed::Box::new(expression),
                covering,
            },
        }
    }

    /// What one feature's vertices carry.
    ///
    /// mbgl's `getVertexSizeData`, and the zero pair is not an omission: where the shader reads
    /// a uniform it never looks at these bytes, and writing the size into them anyway would put
    /// a number in the stream that means nothing and would drift.
    #[must_use]
    pub fn vertex_size(&self, feature: &dyn Feature, default: f32) -> SizeRange {
        #[allow(clippy::cast_possible_truncation)]
        let at = |expression: &Expression, zoom: Option<f64>| -> f32 {
            expression
                .evaluate(zoom, Some(feature))
                .ok()
                .as_ref()
                .and_then(Value::as_number)
                .map_or(default, |value| value as f32)
        };
        match self {
            // The shader reads the uniform in both of these, so mbgl writes a zero pair and so
            // does this.
            Self::Constant(_) | Self::Zoom { .. } => SizeRange { min: 0.0, max: 0.0 },
            Self::Feature { expression } => {
                // With no zoom at all, not with the tile's: the expression does not read one,
                // and offering it would let a mis-classified size start depending on it.
                let size = at(expression, None);
                SizeRange {
                    min: size,
                    max: size,
                }
            }
            Self::Composite {
                expression,
                covering,
            } => SizeRange {
                min: at(expression, Some(covering.0)),
                max: at(expression, Some(covering.1)),
            },
        }
    }

    /// What the drawable tells the shader, for a camera at `view_zoom`.
    ///
    /// mbgl's `evaluateForZoom`. The unused fields are left at zero rather than at something
    /// plausible, as mbgl leaves them: a value the shader does not read is a value nothing keeps
    /// honest.
    #[must_use]
    pub fn at_zoom(&self, view_zoom: f64) -> EvaluatedSize {
        match self {
            Self::Constant(size) => EvaluatedSize {
                zoom_constant: true,
                feature_constant: true,
                size_t: 0.0,
                size: *size,
            },
            Self::Zoom {
                expression,
                covering,
                sizes,
            } => {
                // Interpolated between the covering stops rather than evaluated at the camera's
                // zoom, which mbgl does deliberately: "Even though we could get the exact value
                // of the camera function at z = currentZoom, we intentionally do not."
                let t = expression.interpolation_factor(*covering, view_zoom);
                EvaluatedSize {
                    zoom_constant: false,
                    feature_constant: true,
                    size_t: 0.0,
                    size: sizes.0 + t * (sizes.1 - sizes.0),
                }
            }
            Self::Feature { .. } => EvaluatedSize {
                zoom_constant: true,
                feature_constant: false,
                size_t: 0.0,
                size: 0.0,
            },
            Self::Composite {
                expression,
                covering,
                ..
            } => EvaluatedSize {
                zoom_constant: false,
                feature_constant: false,
                size_t: expression.interpolation_factor(*covering, view_zoom),
                size: 0.0,
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use alloc::collections::BTreeMap;
    use alloc::string::{String, ToString as _};

    use tessella_style::Style;

    use super::*;

    /// A feature with the properties a size expression might read.
    struct Ranked(BTreeMap<String, Value>);
    impl Feature for Ranked {
        fn property(&self, name: &str) -> Option<Value> {
            self.0.get(name).cloned()
        }
        fn id(&self) -> Option<Value> {
            None
        }
        fn geometry_type(&self) -> &str {
            "Point"
        }
    }
    fn ranked(rank: f64) -> Ranked {
        let mut properties = BTreeMap::new();
        properties.insert("rank".to_string(), Value::Number(rank));
        Ranked(properties)
    }

    fn layer(size: &str) -> tessella_style::Layer {
        let style = Style::parse(&alloc::format!(
            r#"{{"version": 8, "sources": {{}}, "layers": [
                 {{"id": "l", "type": "symbol", "source": "s",
                  "layout": {{"text-field": "x", "text-size": {size}}}}}]}}"#
        ))
        .expect("the style parses");
        style.layer("l").expect("l").clone()
    }

    /// A literal size is constant in both senses, and the shader reads the uniform.
    #[test]
    fn a_literal_is_a_uniform() {
        let binding = SizeBinding::of(&layer("22"), "text-size", 15.0, 16.0);
        assert_eq!(binding, SizeBinding::Constant(22.0));
        let evaluated = binding.at_zoom(15.0);
        assert!(evaluated.zoom_constant && evaluated.feature_constant);
        assert!((evaluated.size - 22.0).abs() < 1e-6);
        // And nothing goes in the vertex, which is mbgl's zero pair rather than a placeholder.
        assert_eq!(
            binding.vertex_size(&ranked(1.0), 16.0),
            SizeRange { min: 0.0, max: 0.0 }
        );
    }

    /// A zoom-only size stays a uniform, re-evaluated per frame between its covering stops.
    ///
    /// mbgl interpolates between the stops rather than evaluating at the camera's zoom, and says
    /// why: to stay consistent with the restriction a composite size is under. The two agree
    /// inside one stop interval, which is what this checks -- the point is that the *mechanism*
    /// is the one mbgl uses, and a curve whose stops straddle the interval would show it.
    #[test]
    fn a_zoom_curve_stays_a_uniform() {
        let binding = SizeBinding::of(
            &layer(r#"["interpolate", ["linear"], ["zoom"], 14, 10, 18, 30]"#),
            "text-size",
            15.0,
            16.0,
        );
        let SizeBinding::Zoom {
            covering, sizes, ..
        } = &binding
        else {
            panic!("a zoom curve binds as one: {binding:?}");
        };
        assert_eq!(*covering, (14.0, 18.0), "the stops enclosing [15, 16]");
        assert!((sizes.0 - 10.0).abs() < 1e-6 && (sizes.1 - 30.0).abs() < 1e-6);

        let evaluated = binding.at_zoom(15.0);
        assert!(!evaluated.zoom_constant && evaluated.feature_constant);
        // A quarter of the way from 14 to 18 is a quarter of the way from 10 to 30.
        assert!((evaluated.size - 15.0).abs() < 1e-4, "{}", evaluated.size);
        assert_eq!(evaluated.size_t, 0.0, "nothing in the vertex to mix");
    }

    /// A size that reads only the feature puts one value in the vertex and nothing per frame.
    #[test]
    fn a_source_function_goes_in_the_vertex() {
        let binding = SizeBinding::of(
            &layer(r#"["match", ["get", "rank"], 1, 12, 2, 24, 16]"#),
            "text-size",
            15.0,
            16.0,
        );
        assert!(matches!(binding, SizeBinding::Feature { .. }));
        let evaluated = binding.at_zoom(15.0);
        assert!(evaluated.zoom_constant && !evaluated.feature_constant);
        assert_eq!(
            binding.vertex_size(&ranked(2.0), 16.0),
            SizeRange {
                min: 24.0,
                max: 24.0
            },
            "both ends are the one size, because zoom does not move it"
        );
        assert_eq!(
            binding.vertex_size(&ranked(9.0), 16.0),
            SizeRange {
                min: 16.0,
                max: 16.0
            },
            "and the match's fallback is a size like any other"
        );
    }

    /// A composite size puts two values in the vertex and the mix factor on the drawable.
    ///
    /// This is the shape `places_locality` has in every Protomaps style -- a zoom curve whose
    /// stops are a `case` on `population_rank` -- and the one that drew a capital at the size of
    /// a village while both flags said "constant".
    #[test]
    fn a_composite_size_puts_both_ends_in_the_vertex() {
        let binding = SizeBinding::of(
            &layer(
                r#"["interpolate", ["linear"], ["zoom"],
                   14, ["match", ["get", "rank"], 1, 8, 2, 12, 10],
                   18, ["match", ["get", "rank"], 1, 16, 2, 24, 20]]"#,
            ),
            "text-size",
            15.0,
            16.0,
        );
        let SizeBinding::Composite { covering, .. } = &binding else {
            panic!("a composite size binds as one: {binding:?}");
        };
        assert_eq!(*covering, (14.0, 18.0));
        assert_eq!(
            binding.vertex_size(&ranked(2.0), 16.0),
            SizeRange {
                min: 12.0,
                max: 24.0
            }
        );
        let evaluated = binding.at_zoom(15.0);
        assert!(!evaluated.zoom_constant && !evaluated.feature_constant);
        assert!(
            (evaluated.size_t - 0.25).abs() < 1e-6,
            "{}",
            evaluated.size_t
        );
        // The vertex carries the size; the uniform is the field the shader does not read.
        assert_eq!(evaluated.size, 0.0);
    }

    /// A `step` curve selects rather than blends, so its mix factor is zero at every zoom.
    #[test]
    fn a_step_curve_does_not_blend() {
        let binding = SizeBinding::of(
            &layer(r#"["step", ["zoom"], 10, 15, 20]"#),
            "text-size",
            15.0,
            16.0,
        );
        let evaluated = binding.at_zoom(15.6);
        assert!(!evaluated.zoom_constant && evaluated.feature_constant);
        assert_eq!(evaluated.size_t, 0.0);
        // Above the stop at 15, so the curve has already selected twenty.
        assert!((evaluated.size - 20.0).abs() < 1e-6, "{}", evaluated.size);
    }

    /// A size the style did not write is the property's own default, as a uniform.
    #[test]
    fn an_unwritten_size_is_the_default() {
        let style = Style::parse(
            r#"{"version": 8, "sources": {}, "layers": [
                 {"id": "l", "type": "symbol", "source": "s", "layout": {"text-field": "x"}}]}"#,
        )
        .expect("the style parses");
        let layer = style.layer("l").expect("l");
        assert_eq!(
            SizeBinding::of(layer, "text-size", 15.0, 16.0),
            SizeBinding::Constant(16.0)
        );
        // `icon-size` is a multiplier and defaults to one, which is the reason the default is a
        // parameter rather than a constant in here.
        assert_eq!(
            SizeBinding::of(layer, "icon-size", 15.0, 1.0),
            SizeBinding::Constant(1.0)
        );
    }
}
