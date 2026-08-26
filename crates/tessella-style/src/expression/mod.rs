//! Expression parsing and classification (DR-11).
//!
//! # Classification is the part that has to be right
//!
//! DR-11 splits every expression three ways before anything evaluates it, and the split is
//! what the whole performance argument rests on (§12.1):
//!
//! - **Constant** — folded once, at parse. Never evaluated again.
//! - **Camera-only** — depends on zoom and nothing else, so it is evaluated once per
//!   `(layer, integer-zoom interval)` process-wide and cached as interpolation endpoints. Every
//!   view at every fractional zoom then costs one mix factor. mbgl re-walks the tree per frame
//!   per map instead, which is the cost this exists to delete.
//! - **Data-driven** — depends on the feature, so it is evaluated per feature at bucket build
//!   and never per frame.
//!
//! A misclassification is not a wrong pixel, which is what makes it dangerous. Classifying a
//! data-driven expression as camera-only gives every feature in a layer the first feature's
//! value; classifying a camera-only one as data-driven merely makes it slow, and slow in a way
//! that looks like the port being inherently slower rather than like a bug. So the dependency
//! is computed as a lattice join over the tree, once, at parse, and stored — never recomputed
//! per evaluation, and never inferred from what an evaluation happened to touch.
//!
//! # Why not the bytecode VM yet
//!
//! §10 schedules the VM for R1. The direct evaluator here produces the values the VM will have
//! to reproduce, and the golden oracle (§9.1) is what will confirm they agree. Building the VM
//! first would mean optimizing something not yet known to be correct.

mod evaluate;
mod parse;

pub use evaluate::{EvaluationError, Feature};
pub use parse::ParseError;

use alloc::boxed::Box;
use alloc::string::String;
use alloc::vec::Vec;

use crate::value::Value;

/// What an expression's value depends on.
///
/// A lattice, joined up the tree: an expression depends on whatever its children do, plus
/// whatever it introduces itself. `zoom` introduces [`Dependency::Zoom`]; `get`, `has`,
/// `geometry-type`, `id` and `properties` introduce [`Dependency::Feature`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default)]
pub enum Dependency {
    /// Nothing. The expression is a constant and folds at parse.
    #[default]
    None,
    /// Zoom only. Evaluated once per `(layer, zoom interval)`, process-wide (§12.1).
    Zoom,
    /// The feature only. Evaluated per feature at bucket build.
    Feature,
    /// Both. Evaluated per feature, and re-evaluated when the zoom interval changes.
    ZoomAndFeature,
}

impl Dependency {
    /// The least dependency covering both.
    #[must_use]
    pub const fn join(self, other: Self) -> Self {
        match (self, other) {
            (Self::None, other) => other,
            (other, Self::None) => other,
            (Self::Zoom, Self::Zoom) => Self::Zoom,
            (Self::Feature, Self::Feature) => Self::Feature,
            _ => Self::ZoomAndFeature,
        }
    }

    /// True when the value cannot change: no zoom, no feature.
    #[must_use]
    pub const fn is_constant(self) -> bool {
        matches!(self, Self::None)
    }

    /// True when zoom is involved.
    #[must_use]
    pub const fn needs_zoom(self) -> bool {
        matches!(self, Self::Zoom | Self::ZoomAndFeature)
    }

    /// True when the feature is involved.
    ///
    /// This is the one that decides whether a property becomes a vertex attribute or a
    /// uniform (§2.2), so it is load-bearing well beyond the evaluator.
    #[must_use]
    pub const fn needs_feature(self) -> bool {
        matches!(self, Self::Feature | Self::ZoomAndFeature)
    }
}

/// Comparison operators.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompareOp {
    /// `==`
    Eq,
    /// `!=`
    Ne,
    /// `<`
    Lt,
    /// `<=`
    Le,
    /// `>`
    Gt,
    /// `>=`
    Ge,
}

/// Arithmetic operators.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArithmeticOp {
    /// `+`
    Add,
    /// `-`, which is negation when given one argument.
    Subtract,
    /// `*`
    Multiply,
    /// `/`
    Divide,
    /// `%`
    Modulo,
    /// `^`
    Power,
    /// `min`
    Min,
    /// `max`
    Max,
    /// `abs`
    Abs,
    /// `floor`
    Floor,
    /// `ceil`
    Ceil,
    /// `round`
    Round,
}

/// A type coercion.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CastKind {
    /// `to-number`
    Number,
    /// `to-string`
    String,
    /// `to-boolean`
    Boolean,
    /// `to-color`
    Color,
}

/// What an expression is known to produce.
///
/// # `Value` is "unknown", not "anything"
///
/// The spec's type checker is deliberately permissive where it cannot see. `["get", "x"]` has
/// type `Value` because a feature property could be anything, and `["==", ["get", "x"],
/// ["get", "y"]]` is therefore *valid* — it may fail at evaluation, and that is the right place
/// for it to fail. The same comparison between a `["string", …]` and a `["number", …]` is
/// rejected at parse, because there both types are known and no input could make it work.
///
/// So this is not a type system that proves programs correct. It rejects what cannot possibly
/// succeed and admits the rest, which is what makes it usable on styles that read arbitrary
/// vector-tile data.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Type {
    /// Not known statically.
    Value,
    /// Always null.
    Null,
    /// A number.
    Number,
    /// A string.
    String,
    /// A boolean.
    Boolean,
    /// An object.
    Object,
    /// An array.
    Array,
    /// A color.
    Color,
    /// Formatted text: sections with per-section font, scale and colour.
    Formatted,
}

impl Type {
    /// Whether two values of these types could ever be equal.
    ///
    /// Unknowns compare with anything, because the unknown might turn out to match.
    #[must_use]
    pub const fn could_equal(self, other: Self) -> bool {
        matches!(self, Self::Value) || matches!(other, Self::Value) || {
            // Equality is defined on scalars. Arrays and objects are rejected outright rather
            // than compared structurally, which is the spec's choice and not an omission.
            self.is_scalar() && other.is_scalar() && self.same_as(other)
        }
    }

    /// Whether this type is ordered, so `<` and friends apply.
    ///
    /// Numbers and strings only. Booleans have no order the spec is willing to invent, and null
    /// has nothing to compare.
    #[must_use]
    pub const fn is_ordered(self) -> bool {
        matches!(self, Self::Value | Self::Number | Self::String)
    }

    /// Scalars can be compared for equality; aggregates and colours cannot.
    ///
    /// A colour looks comparable and is not: two colours that render identically may hold
    /// different channel values, so the spec declines to define equality on them rather than
    /// pick a tolerance.
    #[must_use]
    pub const fn is_scalar(self) -> bool {
        matches!(
            self,
            Self::Null | Self::Number | Self::String | Self::Boolean
        )
    }

    const fn same_as(self, other: Self) -> bool {
        matches!(
            (self, other),
            (Self::Null, Self::Null)
                | (Self::Number, Self::Number)
                | (Self::String, Self::String)
                | (Self::Boolean, Self::Boolean)
                | (Self::Color, Self::Color)
                | (Self::Object, Self::Object)
                | (Self::Array, Self::Array)
        )
    }

    /// The type a value has.
    #[must_use]
    pub const fn of(value: &Value) -> Self {
        match value {
            Value::Null => Self::Null,
            Value::Bool(_) => Self::Boolean,
            Value::Number(_) => Self::Number,
            Value::String(_) => Self::String,
            Value::Array(_) => Self::Array,
            Value::Object(_) => Self::Object,
            Value::Color(_) => Self::Color,
        }
    }

    /// A name for error messages.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Value => "value",
            Self::Null => "null",
            Self::Number => "number",
            Self::String => "string",
            Self::Boolean => "boolean",
            Self::Object => "object",
            Self::Array => "array",
            Self::Color => "color",
            Self::Formatted => "formatted",
        }
    }
}

/// A type an assertion requires.
///
/// Distinct from [`CastKind`], and the difference is the whole point of both: `["number", v]`
/// *asserts* that `v` is already a number and errors otherwise, while `["to-number", v]`
/// converts one. A style uses the first to say "this data is numeric and I want to know when it
/// is not", and the second to say "make it numeric".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AssertKind {
    /// `number`
    Number,
    /// `string`
    String,
    /// `boolean`
    Boolean,
    /// `object`
    Object,
}

impl AssertKind {
    /// The type name the spec uses in the error message.
    #[must_use]
    pub const fn type_name(self) -> &'static str {
        match self {
            Self::Number => "number",
            Self::String => "string",
            Self::Boolean => "boolean",
            Self::Object => "object",
        }
    }

    /// Whether a value already has this type.
    #[must_use]
    pub const fn matches(self, value: &Value) -> bool {
        matches!(
            (self, value),
            (Self::Number, Value::Number(_))
                | (Self::String, Value::String(_))
                | (Self::Boolean, Value::Bool(_))
                | (Self::Object, Value::Object(_))
        )
    }
}

/// How to interpolate between stops.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Interpolation {
    /// Straight line between stops.
    Linear,
    /// Exponential, with the given base. A base of 1 is linear.
    Exponential {
        /// Rate of change.
        base: f64,
    },
}

/// What a legacy filter comparison reads from the feature.
///
/// Legacy filters name the feature's geometry type and id with the pseudo-properties `$type`
/// and `$id`, which is why they cannot simply become `["get", ...]`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FilterTarget {
    /// A named property.
    Property(alloc::string::String),
    /// The feature id, written `$id`.
    Id,
    /// The geometry type, written `$type`.
    Type,
}

/// Which of the four pre-expression function forms a [`LegacyFunction`] is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LegacyKind {
    /// The property's own value, passed through.
    Identity,
    /// Exact match against stop inputs.
    Categorical,
    /// The output of the last stop at or below the input.
    Interval,
    /// Interpolated between the surrounding stops. The default when `type` is absent.
    Exponential,
}

/// One section of formatted text.
#[derive(Debug, Clone, PartialEq)]
pub struct FormatSection {
    /// The text, or an image to place inline.
    pub content: Box<Expr>,
    /// `font-scale`, relative to the layer's text size.
    pub scale: Option<Box<Expr>>,
    /// `text-font`, a font stack for this section alone.
    pub font: Option<Box<Expr>>,
    /// `text-color`, for this section alone.
    pub color: Option<Box<Expr>>,
}

/// What the style spec says about the property an expression is being parsed for.
///
/// Pre-expression functions need both halves. The default is what a function falls back to, and
/// the type is what `identity` checks its property against — a `number` property whose feature
/// carries a string falls back rather than passing the string through. Neither is in the style,
/// so neither can be recovered from the expression alone.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct PropertySpec {
    /// The property's default value.
    pub default: Option<Value>,
    /// The type the property must have, when the spec names one.
    pub expected: Option<Type>,
}

/// A pre-expression function.
#[derive(Debug, Clone, PartialEq)]
pub struct LegacyFunction {
    /// Which of the four forms this is.
    pub kind: LegacyKind,
    /// The feature property read, or `None` for a zoom function.
    pub property: Option<String>,
    /// Stop inputs and outputs, in the order the style gave them.
    pub stops: Vec<(Value, Value)>,
    /// Exponential base. One is linear.
    pub base: f64,
    /// The function's own `default`, which takes precedence over the property's.
    pub function_default: Option<Value>,
    /// The property spec's default, used when nothing else matches.
    ///
    /// Carried on the node because it comes from the *spec* rather than the style, and a
    /// function parsed without it would silently return null where the spec returns a value.
    pub property_default: Option<Value>,
    /// The type the property must have, for `identity` to check against.
    pub property_type: Option<Type>,
}

/// An expression tree node.
#[derive(Debug, Clone, PartialEq)]
pub enum Expr {
    /// A value used as-is.
    Literal(Value),
    /// The current zoom.
    Zoom,
    /// A feature property.
    Get {
        /// Property name.
        key: Box<Expr>,
        /// The object read, or `None` to read the feature.
        object: Option<Box<Expr>>,
    },
    /// Whether a feature property is present.
    Has {
        /// Property name.
        key: Box<Expr>,
        /// The object tested, or `None` to test the feature.
        object: Option<Box<Expr>>,
    },
    /// The feature's geometry type: `Point`, `LineString` or `Polygon`.
    GeometryType,
    /// The feature's id.
    Id,
    /// All of a feature's properties, as an object.
    Properties,
    /// `["number", v, fallback…]` and its siblings: assert a type, or try the next argument.
    ///
    /// The fallbacks are what make this more than a type check. `["number", ["get", "x"], 0]`
    /// says "x is a number, and where it is not, zero" — the arguments are tried in order and
    /// only exhausting them is an error.
    Assert {
        /// The type required.
        kind: AssertKind,
        /// The value, then any fallbacks.
        args: Vec<Expr>,
    },
    /// `["array", v]`, `["array", item, v]`, `["array", item, n, v]`.
    ///
    /// Separate from [`Expr::Assert`] because it carries an element type and a length, and
    /// because its fallback is one expression rather than a list of candidates.
    AssertArray {
        /// Required element type, if the style named one.
        item: Option<AssertKind>,
        /// Required length, if the style named one.
        length: Option<usize>,
        /// The value asserted.
        value: Box<Expr>,
        /// Used when the assertion fails, instead of erroring.
        fallback: Option<Box<Expr>>,
    },
    /// `["format", content, options, …]`: text in sections.
    ///
    /// The unit R2's shaping consumes. A section carries its own font, scale and colour, which
    /// is what lets one label mix a place name with a smaller elevation in a different face —
    /// and why formatted text is a type rather than a string with markup in it.
    Format {
        /// One entry per `content, options` pair.
        sections: Vec<FormatSection>,
    },
    /// `["concat", …]`: the arguments, coerced to text and run together.
    Concat(Vec<Expr>),
    /// `["join", array, separator]`: an array of strings with a separator between.
    Join {
        /// The array joined.
        items: Box<Expr>,
        /// The separator placed between elements.
        separator: Box<Expr>,
    },
    /// `["length", v]`: the length of a string or array.
    Length(Box<Expr>),
    /// `["in", needle, haystack]`: membership in an array, or substring in a string.
    In {
        /// What is looked for.
        needle: Box<Expr>,
        /// Where it is looked for.
        haystack: Box<Expr>,
    },
    /// `["index-of", needle, haystack, from?]`: where it is, or -1.
    IndexOf {
        /// What is looked for.
        needle: Box<Expr>,
        /// Where it is looked for.
        haystack: Box<Expr>,
        /// Index to start from, which may be negative.
        from: Option<Box<Expr>>,
    },
    /// `["slice", v, start, end?]`: a sub-range of a string or array.
    Slice {
        /// What is sliced.
        value: Box<Expr>,
        /// First index, which may be negative.
        start: Box<Expr>,
        /// One past the last index, which may be negative.
        end: Option<Box<Expr>>,
    },
    /// `["rgb", r, g, b]` and `["rgba", r, g, b, a]`.
    ///
    /// Its own node rather than a function call because its *type* is what matters: a colour is
    /// distinct from the four-element array it looks like, and the distinction is static. That
    /// is what lets `["to-color", ["rgba", …]]` be a pass-through while `["to-color", [0, 255,
    /// 0, 1]]` rescales — the first is already a colour, the second is an array of numbers.
    Rgba {
        /// Red, green, blue, and optionally alpha.
        args: Vec<Expr>,
    },
    /// `["let", name, value, …, body]`: bind names, then evaluate the body.
    ///
    /// Kept as a binding form rather than substituted at parse, which is the other way to
    /// implement it. Substitution would duplicate a bound expression once per `var` that reads
    /// it, and evaluating once is the entire reason `let` exists — a style writes it precisely
    /// when a subexpression is expensive and used several times.
    Let {
        /// Names and their values, in order. A later binding may read an earlier one.
        bindings: Vec<(String, Expr)>,
        /// The expression evaluated with those names in scope.
        body: Box<Expr>,
    },
    /// `["var", name]`: read a bound name.
    ///
    /// Unbound names are rejected at parse, so reaching evaluation means the name is in scope.
    Var(String),
    /// A pre-expression function: `{"property": …, "type": …, "stops": […]}`.
    ///
    /// The style spec's original way of varying a property, still supported and still common in
    /// styles written before expressions existed. Kept as its own node rather than desugared
    /// into `interpolate`/`match`, because the two are not the same function: a legacy function
    /// falls back to the *property's* default where an expression errors, and `identity` has no
    /// expression equivalent at all.
    LegacyFunction(Box<LegacyFunction>),
    /// A comparison.
    Compare {
        /// Which comparison.
        op: CompareOp,
        /// Left side.
        lhs: Box<Expr>,
        /// Right side.
        rhs: Box<Expr>,
    },
    /// Logical negation.
    Not(Box<Expr>),
    /// Logical conjunction, short-circuiting.
    All(Vec<Expr>),
    /// Logical disjunction, short-circuiting.
    Any(Vec<Expr>),
    /// Selects by matching an input against label sets.
    Match {
        /// Value to match.
        input: Box<Expr>,
        /// Label sets and their outputs, in order.
        arms: Vec<(Vec<Value>, Expr)>,
        /// Output when nothing matches.
        fallback: Box<Expr>,
    },
    /// Selects by the first true condition.
    Case {
        /// Condition and output pairs, in order.
        branches: Vec<(Expr, Expr)>,
        /// Output when no condition holds.
        fallback: Box<Expr>,
    },
    /// The first argument that is not null.
    Coalesce(Vec<Expr>),
    /// Arithmetic.
    Arithmetic {
        /// Which operator.
        op: ArithmeticOp,
        /// Operands.
        args: Vec<Expr>,
    },
    /// A type coercion.
    Cast {
        /// Target type.
        to: CastKind,
        /// Candidates, tried in order.
        args: Vec<Expr>,
    },
    /// Interpolation between stops.
    Interpolate {
        /// How to interpolate.
        interpolation: Interpolation,
        /// What to interpolate over, usually zoom.
        input: Box<Expr>,
        /// Stop positions and outputs, in ascending position order.
        stops: Vec<(f64, Expr)>,
    },
    /// A legacy filter comparison.
    ///
    /// Separate from [`Expr::Compare`] because the semantics differ where it matters: a legacy
    /// filter yields `false` for a missing property or a type mismatch, while an expression
    /// comparison raises an error. Folding the two together would make `["<", "height", 5]`
    /// fail a whole tile because one feature lacks the property.
    FilterCompare {
        /// What to read.
        target: FilterTarget,
        /// Which comparison.
        op: CompareOp,
        /// The value compared against, always a literal in legacy syntax.
        literal: Value,
    },
    /// A legacy `has` test.
    FilterHas {
        /// What to test for.
        target: FilterTarget,
    },
    /// A legacy `in` test.
    FilterIn {
        /// What to read.
        target: FilterTarget,
        /// Values to test membership against.
        values: Vec<Value>,
    },
    /// A step function over stops.
    Step {
        /// What to step over, usually zoom.
        input: Box<Expr>,
        /// Output below the first stop.
        base: Box<Expr>,
        /// Stop positions and outputs, in ascending position order.
        stops: Vec<(f64, Expr)>,
    },
}

/// A parsed and classified expression.
///
/// The dependency is computed once, at parse, and stored. It is never recomputed per
/// evaluation and never inferred from what an evaluation happened to touch: an expression
/// whose data-driven branch is not taken for one feature is still data-driven.
#[derive(Debug, Clone, PartialEq)]
pub struct Expression {
    root: Expr,
    dependency: Dependency,
}

impl Expression {
    /// Parses a style value into an expression.
    ///
    /// # Errors
    ///
    /// [`ParseError`] when the value is not a well-formed expression, names an operator this
    /// build does not implement, or gives one the wrong number of arguments.
    pub fn parse(value: &Value) -> Result<Self, ParseError> {
        Self::parse_with_default(value, None)
    }

    /// Parses a style value, carrying the property spec's default.
    ///
    /// Only pre-expression functions use it, and they need it: a legacy function whose input
    /// matches no stop returns the *property's* default, which lives in the spec rather than in
    /// the style. Parsing one without it yields a function that returns null where the spec says
    /// it returns a value, and null renders as nothing rather than as an error.
    ///
    /// # Errors
    ///
    /// As [`Expression::parse`].
    pub fn parse_with_default(
        value: &Value,
        property_default: Option<Value>,
    ) -> Result<Self, ParseError> {
        Self::parse_for(
            value,
            &PropertySpec {
                default: property_default,
                expected: None,
            },
        )
    }

    /// Parses a style value for a property whose spec is known.
    ///
    /// # Errors
    ///
    /// As [`Expression::parse`].
    pub fn parse_for(value: &Value, spec: &PropertySpec) -> Result<Self, ParseError> {
        Self::parse_rooted(value, spec, true)
    }

    /// Parses a filter, where a zoom curve may appear anywhere.
    ///
    /// # The rule is a property rule, not an expression rule
    ///
    /// mbgl has two entry points and only one of them checks: `parseLayerPropertyExpression`
    /// runs `findZoomCurve` and rejects a buried curve, while `parseExpression` — which
    /// `Converter<Filter>` calls — does not. The reason is what the check is *for*: a property
    /// is evaluated once per zoom interval and interpolated between the endpoints, which needs
    /// a single identifiable curve to take endpoints from. A filter is evaluated per feature at
    /// the tile's own zoom and never interpolated, so there is nothing to find.
    ///
    /// Applying the property rule here refuses ordinary styles. `["all", ["==", ["get",
    /// "class"], "path"], ["step", ["zoom"], …]]` is how a road layer drops footways above a
    /// zoom, and twenty-three layers of one real style are written that way.
    ///
    /// # Errors
    ///
    /// As [`Expression::parse`].
    pub fn parse_filter(value: &Value) -> Result<Self, ParseError> {
        Self::parse_rooted(
            value,
            &PropertySpec {
                default: None,
                expected: None,
            },
            false,
        )
    }

    fn parse_rooted(
        value: &Value,
        spec: &PropertySpec,
        zoom_placement: bool,
    ) -> Result<Self, ParseError> {
        let mut root = parse::parse_with_default(value, spec)?;

        // Checked on the tree the style actually wrote, before either wrapper below moves the
        // root. A coerced tree has the curve one level down and would be rejected as though the
        // style had buried it.
        if zoom_placement {
            check_zoom_placement(&root)?;
        }

        // A property the spec types as a colour gets its result coerced. The style writes
        // `"red"` or a function returning `"red"`, and what the renderer needs is RGBA — so the
        // conversion belongs at the boundary between the two rather than in every operator that
        // might produce a string.
        if spec.expected == Some(Type::Color) {
            root = coerce_to_color(root);
        }

        // A property the spec types as formatted wraps whatever it got in a single section.
        // Same shape as the colour coercion above and for the same reason: the style writes a
        // string and the shaper needs sections, so the conversion belongs at that boundary.
        if spec.expected == Some(Type::Formatted) && root.result_type() != Type::Formatted {
            root = Expr::Format {
                sections: alloc::vec![FormatSection {
                    content: Box::new(root),
                    scale: None,
                    font: None,
                    color: None,
                }],
            };
        }

        let dependency = classify(&root);
        let parsed = Self { root, dependency };

        // An expression that depends on nothing has one value, and this is where it is found
        // out. If evaluating it fails, no input could ever make it succeed, so the failure is
        // the style's rather than the data's and belongs at load.
        //
        // The spec does this too, and the suite requires it: `["number", ["get", "x",
        // ["literal", {"y": 0}]]]` reads a key that is not in a literal object, which is a
        // compile error there and would otherwise be an evaluation error here — once per
        // feature, forever, for a mistake that is visible on sight.
        if dependency.is_constant()
            && let Err(source) = evaluate::evaluate(&parsed.root, &evaluate::Context::empty())
        {
            return Err(ParseError::ConstantFolds { source });
        }
        Ok(parsed)
    }

    /// Wraps an already-built tree, classifying it.
    ///
    /// For trees that are constructed rather than parsed — legacy filter conversion is the
    /// only such case — so that they get the same classification pass as everything else
    /// rather than a hand-assigned dependency.
    #[must_use]
    pub fn from_expr(root: Expr) -> Self {
        let dependency = classify(&root);
        Self { root, dependency }
    }

    /// The expression tree.
    #[must_use]
    pub fn root(&self) -> &Expr {
        &self.root
    }

    /// What this expression is known to produce.
    ///
    /// [`Type::Value`] where it cannot be known, which is most data-driven expressions.
    #[must_use]
    pub fn result_type(&self) -> Type {
        self.root.result_type()
    }

    /// What this expression's value depends on.
    #[must_use]
    pub fn dependency(&self) -> Dependency {
        self.dependency
    }

    /// The value, when the expression is constant.
    ///
    /// This is DR-11's constant folding: a property whose expression turns out to depend on
    /// nothing never reaches the evaluator again.
    #[must_use]
    pub fn as_constant(&self) -> Option<Value> {
        if !self.dependency.is_constant() {
            return None;
        }
        evaluate::evaluate(&self.root, &evaluate::Context::empty()).ok()
    }

    /// Evaluates against a zoom and an optional feature.
    ///
    /// # Errors
    ///
    /// [`EvaluationError`] when a value has the wrong type for the operator applied to it, or
    /// when the expression needs a zoom or feature the caller did not supply.
    pub fn evaluate(
        &self,
        zoom: Option<f64>,
        feature: Option<&dyn Feature>,
    ) -> Result<Value, EvaluationError> {
        evaluate::evaluate(
            &self.root,
            &evaluate::Context {
                zoom,
                feature,
                scope: None,
            },
        )
    }
}

impl Expr {
    /// What this expression is known to produce, or [`Type::Value`] when it cannot be known.
    ///
    /// Only the cases the checker acts on are given precise types. Everything else is `Value`,
    /// which is honest: claiming a type this does not actually derive would reject valid styles,
    /// and being wrong in that direction is worse than being vague.
    #[must_use]
    pub fn result_type(&self) -> Type {
        match self {
            Self::Literal(value) => Type::of(value),
            Self::Zoom => Type::Number,
            Self::GeometryType => Type::String,
            Self::Has { .. } | Self::Not(_) | Self::Compare { .. } => Type::Boolean,
            Self::All(_) | Self::Any(_) => Type::Boolean,
            Self::Properties => Type::Object,
            Self::Arithmetic { .. } => Type::Number,
            Self::Assert { kind, .. } => match kind {
                AssertKind::Number => Type::Number,
                AssertKind::String => Type::String,
                AssertKind::Boolean => Type::Boolean,
                AssertKind::Object => Type::Object,
            },
            Self::AssertArray { .. } => Type::Array,
            Self::Cast { to, .. } => match to {
                CastKind::Number => Type::Number,
                CastKind::String => Type::String,
                CastKind::Boolean => Type::Boolean,
                CastKind::Color => Type::Color,
            },
            Self::Rgba { .. } => Type::Color,
            Self::Format { .. } => Type::Formatted,
            Self::Length(_) | Self::IndexOf { .. } => Type::Number,
            Self::Concat(_) | Self::Join { .. } => Type::String,
            Self::In { .. } => Type::Boolean,
            // `get`, `id`, and everything whose type depends on data or on branches this does
            // not unify.
            _ => Type::Value,
        }
    }
}

impl LegacyFunction {
    /// Whether the stops are composite: `[{"zoom": z, "value": v}, output]`.
    ///
    /// A composite function varies with zoom *and* the property, which is the only legacy form
    /// that lands on [`Dependency::ZoomAndFeature`].
    #[must_use]
    pub fn has_composite_stops(&self) -> bool {
        self.stops
            .iter()
            .any(|(input, _)| input.get("zoom").is_some() && input.get("value").is_some())
    }

    /// The fallback: the function's own default, then the property's.
    #[must_use]
    pub fn fallback(&self) -> Option<&Value> {
        self.function_default
            .as_ref()
            .or(self.property_default.as_ref())
    }
}

impl Expression {
    /// The shader's mix factor between this property's two zoom endpoints.
    ///
    /// A property that varies with zoom *and* per feature cannot be evaluated at the camera's
    /// zoom when its buckets are built, so its value at `bucket_zoom` and at `bucket_zoom + 1`
    /// goes into the vertex and the shader mixes between them by this scalar — recomputed per
    /// view, per frame, which is the only place the camera's fractional zoom enters.
    ///
    /// Zero for anything that does not vary with zoom, and zero for a `step` curve: a step
    /// selects rather than blends, so mbgl returns zero for it explicitly rather than letting
    /// the endpoints mix.
    ///
    /// # Why this is not the same arithmetic as interpolating between stops
    ///
    /// It looks like the same formula and it is a different function in mbgl —
    /// `util::interpolationFactor`, computed in `f32` from a camera zoom already narrowed to
    /// `f32`, where stop interpolation runs in `double`. Sharing one implementation would be
    /// right to within a rounding error, which is exactly the size of error the oracle diff
    /// exists to catch.
    #[must_use]
    pub fn zoom_mix_factor(&self, bucket_zoom: f64, view_zoom: f64) -> f32 {
        let Some(curve) = zoom_curve(&self.root) else {
            return 0.0;
        };
        let Expr::Interpolate { interpolation, .. } = curve else {
            // A step curve. See above.
            return 0.0;
        };

        #[allow(clippy::cast_possible_truncation)]
        let (min, max, z) = (
            bucket_zoom as f32,
            (bucket_zoom + 1.0) as f32,
            view_zoom as f32,
        );
        let diff = max - min;
        let progress = z - min;
        if diff == 0.0 {
            return 0.0;
        }
        let factor = match interpolation {
            Interpolation::Linear => progress / diff,
            Interpolation::Exponential { base } if (*base - 1.0).abs() < f64::EPSILON => {
                progress / diff
            }
            Interpolation::Exponential { base } => {
                // mbgl widens the base to double for the powers and narrows the quotient back,
                // which is not the same as computing the whole thing in f32.
                #[allow(clippy::cast_possible_truncation)]
                let value =
                    (base.powf(f64::from(progress)) - 1.0) / (base.powf(f64::from(diff)) - 1.0);
                value as f32
            }
        };
        factor.clamp(0.0, 1.0)
    }
}

/// The zoom curve at the root, through the wrappers that are transparent to it.
///
/// The same set `check_zoom_placement` treats as transparent, and for the same reason: a curve
/// behind a `coalesce` or in a `let` body is still the one curve the property varies by.
fn zoom_curve(expr: &Expr) -> Option<&Expr> {
    match expr {
        Expr::Interpolate { input, .. } | Expr::Step { input, .. }
            if matches!(**input, Expr::Zoom) =>
        {
            Some(expr)
        }
        Expr::Coalesce(args) => args.iter().find_map(zoom_curve),
        Expr::Let { body, .. } => zoom_curve(body),
        _ => None,
    }
}

/// Coerces an expression to a colour, at the leaves rather than at the root.
///
/// # Why not simply wrap the whole thing
///
/// Because of interpolation. `["interpolate", ["linear"], ["zoom"], 13, "#c04030", 15, "#20a080"]`
/// wrapped in a cast asks the mixer to blend two *strings* and then convert; there is no such
/// blend, and the style is a perfectly ordinary one. mbgl builds an `InterpolateImpl<Color>`
/// whose stops are already colours and mixes RGBA component-wise, so the conversion has to
/// happen on the way *in* to the curve, not on the way out.
///
/// A curve's input is deliberately untouched: it is a number, and the property being a colour
/// says nothing about it. Everything else — including a `match`, which is where the
/// data-driven styles put their colours — is wrapped whole, because it selects a value rather
/// than blending two.
///
/// An expression that already produces a colour is left alone: converting one again would read
/// its normalized channels as 0..255 and darken it by a factor of 255.
fn coerce_to_color(expr: Expr) -> Expr {
    match expr {
        Expr::Interpolate {
            interpolation,
            input,
            stops,
        } => Expr::Interpolate {
            interpolation,
            input,
            stops: stops
                .into_iter()
                .map(|(at, out)| (at, coerce_to_color(out)))
                .collect(),
        },
        Expr::Step { input, base, stops } => Expr::Step {
            input,
            base: Box::new(coerce_to_color(*base)),
            stops: stops
                .into_iter()
                .map(|(at, out)| (at, coerce_to_color(out)))
                .collect(),
        },
        // Transparent, as they are to the zoom-placement rule: the value may hide behind either.
        Expr::Coalesce(args) => Expr::Coalesce(args.into_iter().map(coerce_to_color).collect()),
        Expr::Let { bindings, body } => Expr::Let {
            bindings,
            body: Box::new(coerce_to_color(*body)),
        },
        other if other.result_type() == Type::Color => other,
        // A null is the *absence* of a value, not a value to convert. It reaches here from a
        // colour-typed property with no default — `line-gradient`, whose mbgl default is an
        // empty `PropertyValue` — when the style does not write one, and casting it raises
        // "cannot cast null to number" at constant-fold time, refusing the whole style over a
        // property nobody set.
        Expr::Literal(Value::Null) => Expr::Literal(Value::Null),
        other => Expr::Cast {
            to: CastKind::Color,
            args: alloc::vec![other],
        },
    }
}

/// Rejects `zoom` outside the one place the spec allows it.
///
/// # The rule
///
/// A style property may vary with zoom in exactly one way: a single `step` or `interpolate`
/// whose *input* is `["zoom"]`, sitting at the top of the expression. `let` bodies and
/// `coalesce` arguments are transparent, so the curve may hide behind those, but nothing else
/// is — a curve inside another curve's stops, or in a `let` *binding* rather than its body, or
/// two curves side by side, are all rejected.
///
/// # Why the spec is this strict
///
/// §12.1 is the reason, arriving from the other side. A camera-only expression is evaluated once
/// per `(layer, zoom interval)` and cached as interpolation endpoints, so every view at every
/// fractional zoom costs one mix factor. That only works if the zoom dependence has a single
/// known shape to precompute. An expression with zoom buried in arbitrary arithmetic has no
/// endpoints to cache and would have to be re-walked per frame, which is exactly the cost DR-11
/// exists to remove.
fn check_zoom_placement(root: &Expr) -> Result<(), ParseError> {
    let (allowed, total) = zoom_positions(root, true);
    if total == 0 {
        return Ok(());
    }
    if total > 1 {
        return Err(ParseError::Malformed {
            operator: "zoom".into(),
            detail: alloc::format!("{total} zoom curves; a property may vary with zoom once"),
        });
    }
    if allowed != total {
        return Err(ParseError::Malformed {
            operator: "zoom".into(),
            detail: "zoom may only be the input of a top-level step or interpolate".into(),
        });
    }
    Ok(())
}

/// Counts `zoom` references, and how many sit where the spec allows.
///
/// `at_curve` marks a position reachable from the root through transparent wrappers only.
fn zoom_positions(expr: &Expr, at_curve: bool) -> (usize, usize) {
    // A bare `["zoom"]` at a curve position is the property itself varying with zoom, which is
    // allowed. The suite has no case for it either way, so this stops at what the six cases it
    // does have establish rather than extrapolating a stricter rule from them.
    if at_curve && matches!(expr, Expr::Zoom) {
        return (1, 1);
    }
    let sum = |parts: &[(usize, usize)]| parts.iter().fold((0, 0), |(a, b), (c, d)| (a + c, b + d));

    match expr {
        Expr::Zoom => (0, 1),
        // The curve's input may be `zoom` when the curve itself is where it is allowed. Its
        // stops never are, whatever the curve's own position.
        Expr::Step { input, base, stops } => {
            let input = if at_curve && matches!(**input, Expr::Zoom) {
                (1, 1)
            } else {
                zoom_positions(input, false)
            };
            let mut parts = alloc::vec![input, zoom_positions(base, false)];
            parts.extend(stops.iter().map(|(_, out)| zoom_positions(out, false)));
            sum(&parts)
        }
        Expr::Interpolate { input, stops, .. } => {
            let input = if at_curve && matches!(**input, Expr::Zoom) {
                (1, 1)
            } else {
                zoom_positions(input, false)
            };
            let mut parts = alloc::vec![input];
            parts.extend(stops.iter().map(|(_, out)| zoom_positions(out, false)));
            sum(&parts)
        }
        // Transparent: the curve may hide behind either.
        Expr::Coalesce(args) => sum(&args
            .iter()
            .map(|arg| zoom_positions(arg, at_curve))
            .collect::<Vec<_>>()),
        Expr::Let { bindings, body } => {
            // The body only. A binding is an ordinary expression position, which is why
            // `["let", "x", <curve>, …]` is rejected while `["let", "x", …, <curve>]` is not.
            let mut parts = alloc::vec![zoom_positions(body, at_curve)];
            parts.extend(
                bindings
                    .iter()
                    .map(|(_, value)| zoom_positions(value, false)),
            );
            sum(&parts)
        }
        other => sum(&children(other)
            .into_iter()
            .map(|child| zoom_positions(child, false))
            .collect::<Vec<_>>()),
    }
}

/// Every direct child of a node, for walks that treat all of them alike.
fn children(expr: &Expr) -> Vec<&Expr> {
    match expr {
        Expr::Literal(_)
        | Expr::Zoom
        | Expr::GeometryType
        | Expr::Id
        | Expr::Properties
        | Expr::Var(_)
        | Expr::LegacyFunction(_) => Vec::new(),
        Expr::Format { sections } => sections
            .iter()
            .flat_map(|section| {
                let mut parts = alloc::vec![&*section.content];
                parts.extend(section.scale.as_deref());
                parts.extend(section.font.as_deref());
                parts.extend(section.color.as_deref());
                parts
            })
            .collect(),
        Expr::Not(inner) | Expr::Length(inner) => alloc::vec![&**inner],
        Expr::Get { key, object } | Expr::Has { key, object } => {
            let mut out = alloc::vec![&**key];
            out.extend(object.as_deref());
            out
        }
        Expr::Compare { lhs, rhs, .. } => alloc::vec![&**lhs, &**rhs],
        Expr::All(args)
        | Expr::Any(args)
        | Expr::Coalesce(args)
        | Expr::Arithmetic { args, .. }
        | Expr::Cast { args, .. }
        | Expr::Assert { args, .. }
        | Expr::Rgba { args }
        | Expr::Concat(args) => args.iter().collect(),
        Expr::Join { items, separator } => alloc::vec![&**items, &**separator],
        Expr::AssertArray {
            value, fallback, ..
        } => {
            let mut out = alloc::vec![&**value];
            out.extend(fallback.as_deref());
            out
        }
        Expr::In { needle, haystack } => alloc::vec![&**needle, &**haystack],
        Expr::IndexOf {
            needle,
            haystack,
            from,
        } => {
            let mut out = alloc::vec![&**needle, &**haystack];
            out.extend(from.as_deref());
            out
        }
        Expr::Slice { value, start, end } => {
            let mut out = alloc::vec![&**value, &**start];
            out.extend(end.as_deref());
            out
        }
        Expr::Match {
            input,
            arms,
            fallback,
        } => {
            let mut out = alloc::vec![&**input, &**fallback];
            out.extend(arms.iter().map(|(_, output)| output));
            out
        }
        Expr::Case { branches, fallback } => {
            let mut out = alloc::vec![&**fallback];
            for (test, output) in branches {
                out.push(test);
                out.push(output);
            }
            out
        }
        Expr::Step { input, base, stops } => {
            let mut out = alloc::vec![&**input, &**base];
            out.extend(stops.iter().map(|(_, output)| output));
            out
        }
        Expr::Interpolate { input, stops, .. } => {
            let mut out = alloc::vec![&**input];
            out.extend(stops.iter().map(|(_, output)| output));
            out
        }
        Expr::Let { bindings, body } => {
            let mut out = alloc::vec![&**body];
            out.extend(bindings.iter().map(|(_, value)| value));
            out
        }
        Expr::FilterCompare { .. } | Expr::FilterHas { .. } | Expr::FilterIn { .. } => Vec::new(),
    }
}

/// Computes an expression's dependency as a join over its tree.
fn classify(expr: &Expr) -> Dependency {
    match expr {
        Expr::Literal(_) => Dependency::None,
        Expr::Zoom => Dependency::Zoom,
        Expr::GeometryType | Expr::Id | Expr::Properties => Dependency::Feature,
        // A legacy function reads the feature when it names a property and the zoom when it
        // does not. Composite stops — `[{"zoom": z, "value": v}, out]` — read both, which is
        // the case that makes this a lattice join rather than a choice.
        // A `let` depends on whatever its bindings and body depend on. Taking only the body
        // would classify `["let", "a", ["get", "x"], ["var", "a"]]` as constant, which is how a
        // data-driven property gets evaluated once and every feature in the layer gets the
        // first one's value.
        Expr::Let { bindings, body } => bindings
            .iter()
            .map(|(_, value)| classify(value))
            .fold(classify(body), Dependency::join),
        // The binding it reads carries the dependency; the read itself has none.
        Expr::Var(_) => Dependency::None,
        Expr::Rgba { args } => join_all(args),
        Expr::Format { sections } => sections.iter().fold(Dependency::None, |acc, section| {
            let mut joined = acc.join(classify(&section.content));
            for part in [&section.scale, &section.font, &section.color]
                .into_iter()
                .flatten()
            {
                joined = joined.join(classify(part));
            }
            joined
        }),
        Expr::Length(inner) => classify(inner),
        Expr::Concat(args) => join_all(args),
        Expr::Join { items, separator } => classify(items).join(classify(separator)),
        Expr::In { needle, haystack } => classify(needle).join(classify(haystack)),
        Expr::IndexOf {
            needle,
            haystack,
            from,
        } => {
            let base = classify(needle).join(classify(haystack));
            from.as_ref().map_or(base, |from| base.join(classify(from)))
        }
        Expr::Slice { value, start, end } => {
            let base = classify(value).join(classify(start));
            end.as_ref().map_or(base, |end| base.join(classify(end)))
        }
        Expr::Assert { args, .. } => join_all(args),
        Expr::AssertArray {
            value, fallback, ..
        } => match fallback {
            Some(fallback) => classify(value).join(classify(fallback)),
            None => classify(value),
        },
        Expr::LegacyFunction(function) => {
            let from_property = if function.property.is_some() {
                Dependency::Feature
            } else {
                Dependency::Zoom
            };
            if function.has_composite_stops() {
                from_property.join(Dependency::Zoom)
            } else {
                from_property
            }
        }
        // Legacy filters read the feature by construction; there is no camera-only form.
        Expr::FilterCompare { .. } | Expr::FilterHas { .. } | Expr::FilterIn { .. } => {
            Dependency::Feature
        }
        // `get` and `has` read the feature even when the key itself is a constant.
        // Reading a named object does not touch the feature. Classifying it as feature-driven
        // anyway would be safe for correctness and wrong for cost: an expression over a literal
        // table would be re-evaluated per feature forever.
        Expr::Get { key, object } | Expr::Has { key, object } => match object {
            Some(object) => classify(key).join(classify(object)),
            None => Dependency::Feature.join(classify(key)),
        },
        Expr::Compare { lhs, rhs, .. } => classify(lhs).join(classify(rhs)),
        Expr::Not(inner) => classify(inner),
        Expr::All(args) | Expr::Any(args) | Expr::Coalesce(args) => join_all(args),
        Expr::Arithmetic { args, .. } | Expr::Cast { args, .. } => join_all(args),
        Expr::Match {
            input,
            arms,
            fallback,
        } => {
            // Every arm counts, taken or not. An expression is data-driven if any branch could
            // read the feature, because classification describes the expression rather than
            // one evaluation of it.
            let mut dependency = classify(input).join(classify(fallback));
            for (_, output) in arms {
                dependency = dependency.join(classify(output));
            }
            dependency
        }
        Expr::Case { branches, fallback } => {
            let mut dependency = classify(fallback);
            for (condition, output) in branches {
                dependency = dependency.join(classify(condition)).join(classify(output));
            }
            dependency
        }
        Expr::Interpolate { input, stops, .. } => {
            let mut dependency = classify(input);
            for (_, output) in stops {
                dependency = dependency.join(classify(output));
            }
            dependency
        }
        Expr::Step { input, base, stops } => {
            let mut dependency = classify(input).join(classify(base));
            for (_, output) in stops {
                dependency = dependency.join(classify(output));
            }
            dependency
        }
    }
}

fn join_all(args: &[Expr]) -> Dependency {
    args.iter()
        .fold(Dependency::None, |acc, arg| acc.join(classify(arg)))
}
