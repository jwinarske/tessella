//! Style configuration options: the `schema` root property and the `config` expression.
//!
//! # What this is
//!
//! A style may declare configuration options — `schema` at the root, a map of option name to a
//! definition carrying a default and how to constrain a value. Layers then read them with
//! `["config", name]`. It is Mapbox Style Spec v3, and maplibre-native has none of it: no
//! `schema`, no `imports`, no `config` in either expression registry.
//!
//! # Why it matters more than a missing operator usually does
//!
//! `config` appears where a style parameterizes itself, which in practice is labels. A vendor
//! style's `text-field` reads
//!
//! ```text
//! ["coalesce", ["get", ["concat", "name_", ["config", "language"]]], ["get", "name"]]
//! ```
//!
//! — the localized name if the configured language has one, the plain name otherwise. An
//! operator the parser does not know makes that expression unparseable, which drops the layer,
//! which is a map with no labels rather than a map with unlocalized ones. The spec's own answer
//! for an option that is not there is `null`, and `null` through that `coalesce` is exactly the
//! plain name: the fallback the style author wrote.
//!
//! # Resolved once, at load
//!
//! A config value cannot change while a style is loaded — there is no zoom, feature or camera in
//! it. So this substitutes literals into the expression trees rather than adding an operator the
//! evaluator carries: after [`substitute`] no `config` call remains, every expression that used
//! one is constant in that part, and DR-11's folding applies to it like anything else.

use alloc::string::{String, ToString};
use alloc::vec::Vec;

use serde::{Deserialize, Serialize};

use crate::value::Value;

/// What an option's value is coerced to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ConfigType {
    /// Coerce to a string.
    String,
    /// Coerce to a number.
    Number,
    /// Coerce to a boolean.
    Boolean,
    /// Coerce to a color.
    Color,
}

/// One configuration option's definition.
///
/// `default` is the only required field, and it is what a directly-loaded style resolves to: the
/// spec calls it "the required initial value for the configuration option", and with nothing
/// importing the style there is no override to displace it.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
pub struct ConfigOption {
    /// The initial value. Required.
    pub default: Value,
    /// What to coerce the result to.
    #[serde(rename = "type", default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<ConfigType>,
    /// Whether the option is an array.
    #[serde(default, skip_serializing_if = "core::ops::Not::not")]
    pub array: bool,
    /// Lower bound. A smaller value is clamped up to it.
    #[serde(rename = "minValue", default, skip_serializing_if = "Option::is_none")]
    pub min_value: Option<f64>,
    /// Upper bound. A larger value is clamped down to it.
    #[serde(rename = "maxValue", default, skip_serializing_if = "Option::is_none")]
    pub max_value: Option<f64>,
    /// Increment between permitted values. A result rounds to the nearest.
    #[serde(rename = "stepValue", default, skip_serializing_if = "Option::is_none")]
    pub step_value: Option<f64>,
    /// The permitted values. Anything else falls back to `default`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub values: Option<Vec<Value>>,
    /// Anything that does not affect rendering.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<Value>,
}

impl ConfigOption {
    /// The option's value, given whatever an importer supplied for it.
    ///
    /// The order is the order the spec describes the fields in, and it is not arbitrary:
    /// validating the enumeration first means an out-of-range *listed* value is kept rather than
    /// clamped into something the style never offered, and coercing last means the bounds are
    /// applied to the number before it becomes a string.
    #[must_use]
    pub fn resolve(&self, supplied: Option<&Value>) -> Value {
        let mut value = supplied.unwrap_or(&self.default).clone();

        // "Permitted enumerated values; invalid input uses default."
        if let Some(permitted) = &self.values
            && !permitted.contains(&value)
        {
            value = self.default.clone();
        }

        if let Some(number) = value.as_number() {
            let mut number = number;
            if let Some(min) = self.min_value {
                number = number.max(min);
            }
            if let Some(max) = self.max_value {
                number = number.min(max);
            }
            // "Increment between allowed values; results round to nearest."
            if let Some(step) = self.step_value
                && step > 0.0
            {
                number = (number / step).round() * step;
            }
            value = Value::Number(number);
        }

        match self.kind {
            Some(ConfigType::String) => match &value {
                Value::String(_) => value,
                Value::Number(number) => Value::String(number.to_string()),
                Value::Bool(flag) => Value::String(flag.to_string()),
                _ => value,
            },
            Some(ConfigType::Number) => value.as_number().map_or(value.clone(), Value::Number),
            Some(ConfigType::Boolean) => Value::Bool(match &value {
                Value::Bool(flag) => *flag,
                Value::Number(number) => *number != 0.0,
                Value::Null => false,
                _ => true,
            }),
            // A colour is a string until the property that reads it parses one, which is where
            // every other colour in a style is parsed too.
            Some(ConfigType::Color) | None => value,
        }
    }
}

/// One entry of the `imports` root property.
///
/// Carried so `["config", name, import-id]` can name one, and so a round trip keeps it. Nothing
/// fetches `url`: importing a style means merging its layers, which is a feature of its own.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
pub struct Import {
    /// Unique import name.
    pub id: String,
    /// The URL of the style.
    pub url: String,
    /// Values for the imported style's configuration options.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config: Option<alloc::collections::BTreeMap<String, Value>>,
}

/// A style's configuration options, resolved.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ConfigValues {
    own: alloc::collections::BTreeMap<String, Value>,
    imported: alloc::collections::BTreeMap<String, alloc::collections::BTreeMap<String, Value>>,
}

impl ConfigValues {
    /// Resolves a style's own `schema` against the overrides its `imports` carry.
    #[must_use]
    pub fn new(
        schema: &alloc::collections::BTreeMap<String, ConfigOption>,
        imports: &[Import],
    ) -> Self {
        let own = schema
            .iter()
            .map(|(name, option)| (name.clone(), option.resolve(None)))
            .collect();
        let imported = imports
            .iter()
            .map(|import| (import.id.clone(), import.config.clone().unwrap_or_default()))
            .collect();
        Self { own, imported }
    }

    /// An option's value, or [`Value::Null`] when the style does not declare one.
    ///
    /// Null rather than an error, because that is what the spec says `config` yields for an
    /// option that is missing — and because the styles that use it wrap it in a `coalesce`
    /// whose whole purpose is to have somewhere to go when it is.
    ///
    /// With an import named, the value is what this style supplied for that import. There is no
    /// fallback to the imported style's own default: that default lives in a document nothing
    /// here has fetched, and inventing one would be worse than saying nothing.
    #[must_use]
    pub fn get(&self, name: &str, import: Option<&str>) -> Value {
        match import {
            None => self.own.get(name).cloned().unwrap_or(Value::Null),
            Some(id) => self
                .imported
                .get(id)
                .and_then(|config| config.get(name))
                .cloned()
                .unwrap_or(Value::Null),
        }
    }
}

/// Replaces every `["config", …]` call in `value` with what it resolves to.
///
/// Walks arrays and objects alike, because a `config` call can sit anywhere an expression can —
/// inside a `concat` inside a `get` inside a `coalesce`, which is where the vendor style puts
/// one. A malformed call, where the option name is not a string or the arity is wrong, is left
/// alone so the expression parser reports it rather than this silently turning it into null.
#[must_use]
pub fn substitute(value: &Value, config: &ConfigValues) -> Value {
    match value {
        Value::Array(items) => {
            if let Some(resolved) = as_config_call(items, config) {
                return resolved;
            }
            Value::Array(
                items
                    .iter()
                    .map(|item| substitute(item, config))
                    .collect::<Vec<_>>(),
            )
        }
        Value::Object(entries) => Value::Object(
            entries
                .iter()
                .map(|(key, item)| (key.clone(), substitute(item, config)))
                .collect(),
        ),
        other => other.clone(),
    }
}

/// The value a well-formed `["config", name]` or `["config", name, import]` resolves to.
fn as_config_call(items: &[Value], config: &ConfigValues) -> Option<Value> {
    if items.first().and_then(Value::as_str) != Some("config") {
        return None;
    }
    let name = items.get(1).and_then(Value::as_str)?;
    match items.len() {
        2 => Some(config.get(name, None)),
        3 => items
            .get(2)
            .and_then(Value::as_str)
            .map(|import| config.get(name, Some(import))),
        _ => None,
    }
}
