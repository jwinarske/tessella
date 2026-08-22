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
    },
    /// Whether a feature property is present.
    Has {
        /// Property name.
        key: Box<Expr>,
    },
    /// The feature's geometry type: `Point`, `LineString` or `Polygon`.
    GeometryType,
    /// The feature's id.
    Id,
    /// All of a feature's properties, as an object.
    Properties,
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
        let root = parse::parse(value)?;
        let dependency = classify(&root);
        Ok(Self { root, dependency })
    }

    /// The expression tree.
    #[must_use]
    pub fn root(&self) -> &Expr {
        &self.root
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
        evaluate::evaluate(&self.root, &evaluate::Context { zoom, feature })
    }
}

/// Computes an expression's dependency as a join over its tree.
fn classify(expr: &Expr) -> Dependency {
    match expr {
        Expr::Literal(_) => Dependency::None,
        Expr::Zoom => Dependency::Zoom,
        Expr::GeometryType | Expr::Id | Expr::Properties => Dependency::Feature,
        // `get` and `has` read the feature even when the key itself is a constant.
        Expr::Get { key } | Expr::Has { key } => Dependency::Feature.join(classify(key)),
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
