//! The style spec's value type.
//!
//! A style document is JSON, but not every JSON value is a style value and the difference
//! matters at the edges. Numbers are the case worth naming: the spec has one number type and
//! it is a double, so `1` and `1.0` are the same value and must not parse into different
//! things. `serde_json::Value` distinguishes them, which would make a style that writes
//! `"fill-opacity": 1` and one that writes `"fill-opacity": 1.0` compare unequal and, worse,
//! take different paths through expression type-checking.

use alloc::borrow::ToOwned;
use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::vec::Vec;
use core::fmt;

use serde::de::{self, MapAccess, SeqAccess, Visitor};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// A value as the style spec defines it.
///
/// Objects are ordered maps rather than hash maps so that iteration is deterministic. The
/// golden-oracle diff (§9.1) is only meaningful if two runs of the same input produce the
/// same output, and an unordered map would leak allocation order into anything derived from a
/// style object.
#[derive(Debug, Clone, PartialEq, Default)]
pub enum Value {
    /// JSON `null`.
    #[default]
    Null,
    /// A boolean.
    Bool(bool),
    /// A number. The spec has exactly one, and it is a double.
    Number(f64),
    /// A string.
    String(String),
    /// An array.
    Array(Vec<Value>),
    /// An object, in key order.
    Object(BTreeMap<String, Value>),
}

impl Value {
    /// The boolean, if this is one.
    #[must_use]
    pub fn as_bool(&self) -> Option<bool> {
        match self {
            Self::Bool(value) => Some(*value),
            _ => None,
        }
    }

    /// The number, if this is one.
    #[must_use]
    pub fn as_number(&self) -> Option<f64> {
        match self {
            Self::Number(value) => Some(*value),
            _ => None,
        }
    }

    /// The string, if this is one.
    #[must_use]
    pub fn as_str(&self) -> Option<&str> {
        match self {
            Self::String(value) => Some(value),
            _ => None,
        }
    }

    /// The array, if this is one.
    #[must_use]
    pub fn as_array(&self) -> Option<&[Value]> {
        match self {
            Self::Array(values) => Some(values),
            _ => None,
        }
    }

    /// The object, if this is one.
    #[must_use]
    pub fn as_object(&self) -> Option<&BTreeMap<String, Value>> {
        match self {
            Self::Object(entries) => Some(entries),
            _ => None,
        }
    }

    /// Looks up a key, if this is an object.
    #[must_use]
    pub fn get(&self, key: &str) -> Option<&Value> {
        self.as_object()?.get(key)
    }

    /// A short name for this value's type, as the spec spells it.
    ///
    /// Used in error messages and, later, by the expression type checker, where "expected
    /// number, got string" is the difference between a useful diagnostic and a shrug.
    #[must_use]
    pub fn type_name(&self) -> &'static str {
        match self {
            Self::Null => "null",
            Self::Bool(_) => "boolean",
            Self::Number(_) => "number",
            Self::String(_) => "string",
            Self::Array(_) => "array",
            Self::Object(_) => "object",
        }
    }

    /// True when this value is an expression call.
    ///
    /// A style property is an expression when it is a non-empty array whose first element is a
    /// string *naming a registered operator*. Everything else — including an array of numbers,
    /// which is how a `line-dasharray` or a colour triple is written — is a literal.
    ///
    /// # Why the registry, and not just "starts with a string"
    ///
    /// That was the earlier rule, and it is the style spec's own words up to the last clause.
    /// The spec spells it `expression[0] in expressions`: a lookup, not a shape test. Without
    /// the lookup, `["Noto Sans Regular"]` — a font stack, and the ordinary way `text-font` is
    /// written — reads as a call to an operator of that name. Nothing errors; the value is
    /// simply classified wrong, and a caller that asks for its literal array gets nothing.
    ///
    /// The registry is [`crate::generated::operators::OPERATORS`], taken from mbgl rather than
    /// written down here, because a list that drifts from the engine is wrong silently: the
    /// symptom is a style that renders slightly differently, not a build that fails.
    #[must_use]
    pub fn looks_like_expression(&self) -> bool {
        match self {
            Self::Array(items) => matches!(
                items.first(),
                Some(Self::String(head)) if crate::is_operator(head)
            ),
            _ => false,
        }
    }
}

impl Serialize for Value {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            Self::Null => serializer.serialize_unit(),
            Self::Bool(value) => serializer.serialize_bool(*value),
            Self::Number(value) => serializer.serialize_f64(*value),
            Self::String(value) => serializer.serialize_str(value),
            Self::Array(values) => values.serialize(serializer),
            Self::Object(entries) => entries.serialize(serializer),
        }
    }
}

struct ValueVisitor;

impl<'de> Visitor<'de> for ValueVisitor {
    type Value = Value;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("a style value")
    }

    fn visit_unit<E: de::Error>(self) -> Result<Value, E> {
        Ok(Value::Null)
    }

    fn visit_none<E: de::Error>(self) -> Result<Value, E> {
        Ok(Value::Null)
    }

    fn visit_bool<E: de::Error>(self, value: bool) -> Result<Value, E> {
        Ok(Value::Bool(value))
    }

    // Every integer width funnels into one f64, which is what keeps `1` and `1.0` the same
    // style value.
    fn visit_i64<E: de::Error>(self, value: i64) -> Result<Value, E> {
        Ok(Value::Number(value as f64))
    }

    fn visit_u64<E: de::Error>(self, value: u64) -> Result<Value, E> {
        Ok(Value::Number(value as f64))
    }

    fn visit_f64<E: de::Error>(self, value: f64) -> Result<Value, E> {
        Ok(Value::Number(value))
    }

    fn visit_str<E: de::Error>(self, value: &str) -> Result<Value, E> {
        Ok(Value::String(value.to_owned()))
    }

    fn visit_string<E: de::Error>(self, value: String) -> Result<Value, E> {
        Ok(Value::String(value))
    }

    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Value, A::Error> {
        let mut items = Vec::with_capacity(seq.size_hint().unwrap_or(0));
        while let Some(item) = seq.next_element()? {
            items.push(item);
        }
        Ok(Value::Array(items))
    }

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Value, A::Error> {
        let mut entries = BTreeMap::new();
        while let Some((key, value)) = map.next_entry()? {
            entries.insert(key, value);
        }
        Ok(Value::Object(entries))
    }
}

impl<'de> Deserialize<'de> for Value {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserializer.deserialize_any(ValueVisitor)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(json: &str) -> Value {
        serde_json::from_str(json).expect("valid json")
    }

    /// The spec has one number type. A style writing `1` and one writing `1.0` mean the same
    /// thing, and anything derived from them has to agree.
    #[test]
    fn integers_and_floats_are_one_number_type() {
        assert_eq!(parse("1"), Value::Number(1.0));
        assert_eq!(parse("1.0"), Value::Number(1.0));
        assert_eq!(parse("1"), parse("1.0"));
        assert_eq!(parse("-3"), Value::Number(-3.0));
    }

    #[test]
    fn parses_the_value_kinds() {
        assert_eq!(parse("null"), Value::Null);
        assert_eq!(parse("true"), Value::Bool(true));
        assert_eq!(parse(r#""hi""#), Value::String("hi".into()));
        assert_eq!(
            parse("[1, \"a\"]"),
            Value::Array(alloc::vec![Value::Number(1.0), Value::String("a".into())])
        );
        assert_eq!(parse(r#"{"a": 1}"#).get("a"), Some(&Value::Number(1.0)));
    }

    /// Object iteration must not depend on insertion or allocation order, or anything derived
    /// from a style object leaks that order into the oracle diff.
    #[test]
    fn objects_iterate_in_key_order() {
        let value = parse(r#"{"z": 1, "a": 2, "m": 3}"#);
        let keys: Vec<&str> = value
            .as_object()
            .expect("object")
            .keys()
            .map(String::as_str)
            .collect();
        assert_eq!(keys, ["a", "m", "z"]);
    }

    /// Telling an expression from a literal is a first-element test, and the case that matters
    /// is the array of numbers: a dasharray or a color triple is data, not a call.
    #[test]
    fn distinguishes_expressions_from_literal_arrays() {
        assert!(parse(r#"["get", "kind"]"#).looks_like_expression());
        assert!(parse(r#"["==", "$type", "Polygon"]"#).looks_like_expression());

        assert!(!parse("[2, 4]").looks_like_expression());
        assert!(!parse("[]").looks_like_expression());
        assert!(!parse("3").looks_like_expression());
        assert!(!parse(r#""literal""#).looks_like_expression());
        assert!(!parse(r#"{"a": 1}"#).looks_like_expression());
    }

    #[test]
    fn type_names_match_the_spec() {
        assert_eq!(Value::Null.type_name(), "null");
        assert_eq!(Value::Bool(true).type_name(), "boolean");
        assert_eq!(Value::Number(1.0).type_name(), "number");
        assert_eq!(Value::String(String::new()).type_name(), "string");
    }
}
