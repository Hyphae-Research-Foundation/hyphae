// SPDX-License-Identifier: Apache-2.0

use std::collections::BTreeMap;

/// Canonical structured value used by the deterministic reference executor.
///
/// Variant declaration order is the normative cross-type sort order. Binary
/// floating point is intentionally absent.
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum Value {
    /// Explicit null.
    Null,
    /// Boolean value.
    Boolean(bool),
    /// Signed 64-bit integer.
    Integer(i64),
    /// UTF-8 string.
    String(String),
    /// Opaque binary bytes.
    Bytes(Vec<u8>),
    /// Ordered array.
    Array(Vec<Self>),
    /// Object ordered by UTF-8 field name.
    Object(BTreeMap<String, Self>),
}

/// Exact object-field path. An empty path selects the root value.
#[derive(Clone, Debug, Default, Eq, Ord, PartialEq, PartialOrd)]
pub struct FieldPath {
    segments: Vec<String>,
}

impl FieldPath {
    /// Creates a path from exact object field names.
    pub fn new<I, S>(segments: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        Self {
            segments: segments.into_iter().map(Into::into).collect(),
        }
    }

    /// Creates a single-field path.
    pub fn field(name: impl Into<String>) -> Self {
        Self::new([name.into()])
    }

    /// Returns the exact path segments.
    pub fn segments(&self) -> &[String] {
        &self.segments
    }

    /// Resolves this path, returning `None` for a missing field or traversal
    /// through a non-object value.
    pub fn resolve<'value>(&self, root: &'value Value) -> Option<&'value Value> {
        let mut current = root;
        for segment in &self.segments {
            let Value::Object(object) = current else {
                return None;
            };
            current = object.get(segment)?;
        }
        Some(current)
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::{FieldPath, Value};

    #[test]
    fn field_path_distinguishes_missing_from_explicit_null() {
        let root = Value::Object(BTreeMap::from([
            (
                "nested".to_owned(),
                Value::Object(BTreeMap::from([("value".to_owned(), Value::Null)])),
            ),
            ("scalar".to_owned(), Value::Integer(7)),
        ]));

        assert_eq!(
            FieldPath::new(["nested", "value"]).resolve(&root),
            Some(&Value::Null)
        );
        assert_eq!(FieldPath::new(["nested", "missing"]).resolve(&root), None);
        assert_eq!(FieldPath::new(["scalar", "child"]).resolve(&root), None);
        assert_eq!(FieldPath::default().resolve(&root), Some(&root));
    }

    #[test]
    fn variant_order_is_the_normative_total_order() {
        let values = [
            Value::Null,
            Value::Boolean(false),
            Value::Integer(-1),
            Value::String(String::new()),
            Value::Bytes(Vec::new()),
            Value::Array(Vec::new()),
            Value::Object(BTreeMap::new()),
        ];
        assert!(values.windows(2).all(|pair| pair[0] < pair[1]));
    }
}
