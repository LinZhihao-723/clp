//! Record value sources for the reference evaluator.

use std::ops::ControlFlow;

use crate::ColumnPattern;
use crate::ColumnRef;
use crate::Namespace;
use crate::PathToken;
use crate::SearchType;

/// Supplies the values a column reference reaches in one record.
pub trait RecordValueSource {
    /// Calls `visit` with every value `column` reaches in the record, until `visit` returns
    /// `false`.
    ///
    /// # Type Parameters
    ///
    /// * `VisitorType` - The callback type.
    ///
    /// # Parameters
    ///
    /// * `column` - The column reference to resolve in the record.
    /// * `visit` - Receives each reached value; returns whether to continue.
    fn visit_values<VisitorType: FnMut(ScalarValue<'_>) -> bool>(
        &self,
        column: &ColumnRef,
        visit: VisitorType,
    );
}

/// One reached value, as the reference evaluator sees it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum ScalarValue<'value> {
    Integer(i64),
    Float(f64),

    /// A string value; whether it is stored as `ClpString` follows from whether it contains a
    /// space.
    Text(&'value [u8]),

    Boolean(bool),
    Null,
    Timestamp {
        epoch_ns: i64,
    },

    /// A non-empty object (satisfies `Exists` only).
    Object,

    /// An empty object or array (satisfies `Exists` only).
    EmptyContainer,
}

impl ScalarValue<'_> {
    /// # Returns
    ///
    /// The search type of the value, or `None` for objects and empty containers, which no
    /// comparison accepts.
    #[must_use]
    pub const fn search_type(&self) -> Option<SearchType> {
        match self {
            Self::Integer(_) => Some(SearchType::Integer),
            Self::Float(_) => Some(SearchType::Float),
            Self::Text(_) => Some(SearchType::Text),
            Self::Boolean(_) => Some(SearchType::Boolean),
            Self::Null => Some(SearchType::Null),
            Self::Timestamp { .. } => Some(SearchType::Timestamp),
            Self::Object | Self::EmptyContainer => None,
        }
    }
}

/// A raw JSON record as a value source: the reference oracle over the JSON-lines input of an
/// archive.
///
/// Values are typed as the C++ writer stores them: integers beyond `i64::MAX` wrap to `i64`,
/// non-empty arrays are transparent (their elements are reached instead), and the value at the
/// timestamp key (set with [`Self::with_timestamp_key`]) is reached as the authoritative
/// timestamp when it is a number or a string.
#[derive(Clone, Copy, Debug)]
pub struct JsonRecord<'record> {
    record: &'record serde_json::Value,
    authoritative_timestamp: Option<ScalarValue<'record>>,
    timestamp_key: Option<&'record [PathToken]>,
    range_index_fields: Option<&'record serde_json::Value>,
}

impl<'record> JsonRecord<'record> {
    /// Factory function.
    ///
    /// # Returns
    ///
    /// A source over `record`, whose authoritative timestamp (as the archive would store it) is
    /// `authoritative_timestamp`.
    #[must_use]
    pub const fn new(
        record: &'record serde_json::Value,
        authoritative_timestamp: Option<ScalarValue<'record>>,
    ) -> Self {
        Self {
            record,
            authoritative_timestamp,
            timestamp_key: None,
            range_index_fields: None,
        }
    }

    /// # Returns
    ///
    /// This source with `key` (a literal path in the default namespace, the archive's
    /// `--timestamp-key`) as the timestamp column: paths that reach it outside an array reach the
    /// authoritative timestamp instead of the raw value.
    #[must_use]
    pub fn with_timestamp_key(self, key: &'record ColumnPattern) -> Self {
        let is_literal_default = Namespace::Default == key.namespace && key.is_literal();
        Self {
            timestamp_key: is_literal_default.then_some(&*key.tokens),
            ..self
        }
    }

    /// # Returns
    ///
    /// This source with `fields` as the range-index fields of the record's range (the `$`
    /// namespace, e.g. `{"_filename": "input.jsonl"}`).
    #[must_use]
    pub const fn with_range_index_fields(self, fields: &'record serde_json::Value) -> Self {
        Self {
            range_index_fields: Some(fields),
            ..self
        }
    }
}

impl RecordValueSource for JsonRecord<'_> {
    fn visit_values<VisitorType: FnMut(ScalarValue<'_>) -> bool>(
        &self,
        column: &ColumnRef,
        mut visit: VisitorType,
    ) {
        match column {
            ColumnRef::Path(pattern) => {
                if Namespace::Default != pattern.namespace {
                    return;
                }
                let mut walker = Walker {
                    timestamp_key: self.timestamp_key,
                    timestamp: self.authoritative_timestamp,
                    visit: &mut visit,
                };
                let _ = walker.walk(self.record, &pattern.tokens, Some(0));
            }
            ColumnRef::AnyLeaf => {
                let mut walker = Walker {
                    timestamp_key: self.timestamp_key,
                    timestamp: self.authoritative_timestamp,
                    visit: &mut visit,
                };
                let _ = walker.any_leaf(self.record, Some(0));
            }
            ColumnRef::AuthoritativeTimestamp => {
                if let Some(timestamp) = self.authoritative_timestamp {
                    visit(timestamp);
                }
            }
            ColumnRef::RangeIndexField(pattern) => {
                if let Some(fields) = self.range_index_fields {
                    let mut walker = Walker {
                        timestamp_key: None,
                        timestamp: None,
                        visit: &mut visit,
                    };
                    let _ = walker.walk(fields, &pattern.tokens, None);
                }
            }
        }
    }
}

/// Walks a JSON value along a column pattern.
///
/// `timestamp_depth` tracks the walked path against the timestamp key: `Some(depth)` while the
/// path equals the key's first `depth` tokens, `None` once it diverges or enters an array.
struct Walker<'record, 'visit, VisitorType> {
    timestamp_key: Option<&'record [PathToken]>,
    timestamp: Option<ScalarValue<'record>>,
    visit: &'visit mut VisitorType,
}

impl<'record, VisitorType: FnMut(ScalarValue<'_>) -> bool> Walker<'record, '_, VisitorType> {
    /// Reaches the values of `tokens` below `value`: a key descends into an object member, `*`
    /// matches zero or more object levels, and arrays are transparent at every step.
    ///
    /// # Returns
    ///
    /// [`ControlFlow::Break`] once the visitor asks to stop.
    fn walk(
        &mut self,
        value: &'record serde_json::Value,
        tokens: &[PathToken],
        timestamp_depth: Option<usize>,
    ) -> ControlFlow<()> {
        let Some((token, rest)) = tokens.split_first() else {
            return self.emit(value, timestamp_depth);
        };
        let object = match value {
            serde_json::Value::Array(elements) => {
                for element in elements {
                    self.walk(element, tokens, None)?;
                }
                return ControlFlow::Continue(());
            }
            serde_json::Value::Object(object) => object,
            _ => return ControlFlow::Continue(()),
        };
        match token {
            PathToken::AnyDepth => {
                self.walk(value, rest, timestamp_depth)?;
                for (key, child) in object {
                    let depth = self.descend(timestamp_depth, key.as_bytes());
                    self.walk_below_wildcard(child, tokens, depth)?;
                }
            }
            PathToken::Key(key) => {
                if let Some(child) = std::str::from_utf8(key)
                    .ok()
                    .and_then(|key| object.get(key))
                {
                    let depth = self.descend(timestamp_depth, key);
                    self.walk(child, rest, depth)?;
                }
            }
        }
        ControlFlow::Continue(())
    }

    /// Continues a `*` (the first of `tokens`) that has consumed at least one level: it either
    /// stops at `value` or consumes more levels.
    ///
    /// # Returns
    ///
    /// [`ControlFlow::Break`] once the visitor asks to stop.
    fn walk_below_wildcard(
        &mut self,
        value: &'record serde_json::Value,
        tokens: &[PathToken],
        timestamp_depth: Option<usize>,
    ) -> ControlFlow<()> {
        self.walk(value, tokens.get(1..).unwrap_or_default(), timestamp_depth)?;
        match value {
            serde_json::Value::Object(object) => {
                for (key, child) in object {
                    let depth = self.descend(timestamp_depth, key.as_bytes());
                    self.walk_below_wildcard(child, tokens, depth)?;
                }
            }
            serde_json::Value::Array(elements) => {
                for element in elements {
                    self.walk_below_wildcard(element, tokens, None)?;
                }
            }
            _ => {}
        }
        ControlFlow::Continue(())
    }

    /// Reaches every leaf below `value` (`AnyLeaf`): scalars at any depth, through objects and
    /// arrays.
    ///
    /// # Returns
    ///
    /// [`ControlFlow::Break`] once the visitor asks to stop.
    fn any_leaf(
        &mut self,
        value: &'record serde_json::Value,
        timestamp_depth: Option<usize>,
    ) -> ControlFlow<()> {
        match value {
            serde_json::Value::Object(object) => {
                for (key, child) in object {
                    let depth = self.descend(timestamp_depth, key.as_bytes());
                    self.any_leaf(child, depth)?;
                }
                ControlFlow::Continue(())
            }
            serde_json::Value::Array(elements) => {
                for element in elements {
                    self.any_leaf(element, None)?;
                }
                ControlFlow::Continue(())
            }
            _ => self.emit(value, timestamp_depth),
        }
    }

    /// Reaches `value` itself (the authoritative timestamp at the timestamp key), or for a
    /// non-empty array its elements at any array depth.
    ///
    /// # Returns
    ///
    /// [`ControlFlow::Break`] once the visitor asks to stop.
    fn emit(
        &mut self,
        value: &'record serde_json::Value,
        timestamp_depth: Option<usize>,
    ) -> ControlFlow<()> {
        let is_timestamp = self
            .timestamp_key
            .is_some_and(|key| Some(key.len()) == timestamp_depth)
            && matches!(
                value,
                serde_json::Value::Number(_) | serde_json::Value::String(_)
            );
        if let (true, Some(timestamp)) = (is_timestamp, self.timestamp) {
            return self.visit_one(timestamp);
        }
        match value {
            serde_json::Value::Array(elements) if !elements.is_empty() => {
                for element in elements {
                    self.emit(element, None)?;
                }
                ControlFlow::Continue(())
            }
            _ => self.visit_one(scalar_of(value)),
        }
    }

    /// Passes one value to the visitor.
    ///
    /// # Returns
    ///
    /// [`ControlFlow::Break`] once the visitor asks to stop.
    fn visit_one(&mut self, value: ScalarValue<'_>) -> ControlFlow<()> {
        if (self.visit)(value) {
            ControlFlow::Continue(())
        } else {
            ControlFlow::Break(())
        }
    }

    /// # Returns
    ///
    /// The timestamp-key depth after descending into member `key`.
    fn descend(&self, timestamp_depth: Option<usize>, key: &[u8]) -> Option<usize> {
        let depth = timestamp_depth?;
        match self.timestamp_key?.get(depth) {
            Some(PathToken::Key(expected)) if **expected == *key => Some(depth + 1),
            _ => None,
        }
    }
}

/// # Returns
///
/// The scalar a non-array (or empty array) JSON value is reached as.
fn scalar_of(value: &serde_json::Value) -> ScalarValue<'_> {
    match value {
        serde_json::Value::Null => ScalarValue::Null,
        serde_json::Value::Bool(value) => ScalarValue::Boolean(*value),
        serde_json::Value::Number(number) => number.as_i64().map_or_else(
            || {
                number.as_u64().map_or_else(
                    || ScalarValue::Float(number.as_f64().unwrap_or(f64::NAN)),
                    |unsigned| ScalarValue::Integer(unsigned.cast_signed()),
                )
            },
            ScalarValue::Integer,
        ),
        serde_json::Value::String(text) => ScalarValue::Text(text.as_bytes()),
        serde_json::Value::Object(object) if !object.is_empty() => ScalarValue::Object,
        serde_json::Value::Object(_) | serde_json::Value::Array(_) => ScalarValue::EmptyContainer,
    }
}

#[cfg(test)]
mod tests {
    use super::JsonRecord;
    use super::RecordValueSource;
    use super::ScalarValue;
    use crate::ColumnPattern;
    use crate::ColumnRef;

    /// An owned copy of a [`ScalarValue`].
    #[derive(Debug, PartialEq)]
    enum Owned {
        Integer(i64),
        Float(f64),
        Text(String),
        Boolean(bool),
        Null,
        Timestamp(i64),
        Object,
        EmptyContainer,
    }

    /// # Returns
    ///
    /// The values of `source` that `column` reaches, in a canonical order.
    fn reached(source: &JsonRecord<'_>, column: &ColumnRef) -> Vec<Owned> {
        let mut values = Vec::new();
        source.visit_values(column, |value| {
            values.push(match value {
                ScalarValue::Integer(value) => Owned::Integer(value),
                ScalarValue::Float(value) => Owned::Float(value),
                ScalarValue::Text(text) => Owned::Text(String::from_utf8_lossy(text).into()),
                ScalarValue::Boolean(value) => Owned::Boolean(value),
                ScalarValue::Null => Owned::Null,
                ScalarValue::Timestamp { epoch_ns } => Owned::Timestamp(epoch_ns),
                ScalarValue::Object => Owned::Object,
                ScalarValue::EmptyContainer => Owned::EmptyContainer,
            });
            true
        });
        values.sort_by_key(|value| format!("{value:?}"));
        values
    }

    /// # Returns
    ///
    /// The values that `column` reaches in the JSON record `text`, in a canonical order.
    ///
    /// # Panics
    ///
    /// Panics if `text` isn't valid JSON.
    fn reached_in(text: &str, column: &ColumnRef) -> Vec<Owned> {
        let record = json(text);
        reached(&JsonRecord::new(&record, None), column)
    }

    /// # Returns
    ///
    /// The column that the pattern `text` names.
    ///
    /// # Panics
    ///
    /// Panics if `text` isn't a valid column pattern.
    fn path(text: &str) -> ColumnRef {
        ColumnRef::from_pattern(ColumnPattern::parse(text).expect("column should parse"))
    }

    /// # Returns
    ///
    /// The JSON value that `text` spells.
    ///
    /// # Panics
    ///
    /// Panics if `text` isn't valid JSON.
    fn json(text: &str) -> serde_json::Value {
        serde_json::from_str(text).expect("test JSON should parse")
    }

    #[test]
    fn literal_paths_reach_members_and_array_elements() {
        let record = json(r#"{"a": {"b": [1, [2, {"c": 3}], []]}, "d": {}, "e": {"x": null}}"#);
        let source = JsonRecord::new(&record, None);
        assert_eq!(
            reached(&source, &path("a.b")),
            [
                Owned::EmptyContainer,
                Owned::Integer(1),
                Owned::Integer(2),
                Owned::Object
            ]
        );
        assert_eq!(reached(&source, &path("a.b.c")), [Owned::Integer(3)]);
        assert_eq!(reached(&source, &path("d")), [Owned::EmptyContainer]);
        assert_eq!(reached(&source, &path("e")), [Owned::Object]);
        assert_eq!(reached(&source, &path("e.x")), [Owned::Null]);
        assert_eq!(reached(&source, &path("a.b.zzz")), []);
        assert_eq!(reached(&source, &path("@a")), []);
    }

    #[test]
    fn wildcards_match_zero_or_more_object_levels_and_descend_into_arrays() {
        let record = json(r#"{"a": {"x": {"c": 1}, "c": 2, "arr": [{"c": 3}]}, "c": 4}"#);
        let source = JsonRecord::new(&record, None);
        let mut values = reached(&source, &path("a.*.c"));
        values.dedup();
        assert_eq!(
            values,
            [Owned::Integer(1), Owned::Integer(2), Owned::Integer(3)]
        );
        let mut values = reached(&source, &path("*.c"));
        values.dedup();
        assert_eq!(
            values,
            [
                Owned::Integer(1),
                Owned::Integer(2),
                Owned::Integer(3),
                Owned::Integer(4)
            ]
        );
        assert_eq!(reached_in(r#"{"a": 1}"#, &path("a.*")), []);
        assert_eq!(
            reached_in(r#"{"a": {}}"#, &path("a.*")),
            [Owned::EmptyContainer]
        );
        assert_eq!(
            reached_in(r#"{"a": [{"b": 1}]}"#, &path("a.*")),
            [Owned::Integer(1), Owned::Object]
        );
    }

    #[test]
    fn any_leaf_reaches_scalars_only() {
        let record = json(r#"{"a": {"b": "x", "e": {}}, "l": [1, [true], []], "n": null}"#);
        let source = JsonRecord::new(&record, None);
        assert_eq!(
            reached(&source, &ColumnRef::AnyLeaf),
            [
                Owned::Boolean(true),
                Owned::Integer(1),
                Owned::Null,
                Owned::Text("x".into()),
            ]
        );
    }

    #[test]
    fn numbers_are_typed_like_the_writer() {
        let record = json(r#"{"big": 18446744073709551615, "f": 1.5, "zero": 0, "e": 1e2}"#);
        let source = JsonRecord::new(&record, None);
        assert_eq!(reached(&source, &path("big")), [Owned::Integer(-1)]);
        assert_eq!(reached(&source, &path("f")), [Owned::Float(1.5)]);
        assert_eq!(reached(&source, &path("zero")), [Owned::Integer(0)]);
        assert_eq!(reached(&source, &path("e")), [Owned::Float(100.0)]);
    }

    #[test]
    fn the_timestamp_key_reaches_the_authoritative_timestamp() {
        let key = ColumnPattern::parse("meta.time").expect("key should parse");
        let record =
            json(r#"{"meta": {"time": "2023-11-14", "other": 5}, "l": [{"meta": {"time": 1}}]}"#);
        let timestamp = ScalarValue::Timestamp { epoch_ns: 42 };
        let source = JsonRecord::new(&record, Some(timestamp)).with_timestamp_key(&key);
        assert_eq!(reached(&source, &path("meta.time")), [Owned::Timestamp(42)]);
        assert_eq!(
            reached(&source, &path("*.time")),
            [Owned::Integer(1), Owned::Timestamp(42)]
        );
        assert_eq!(reached(&source, &path("meta.other")), [Owned::Integer(5)]);
        assert_eq!(
            reached(&source, &ColumnRef::AuthoritativeTimestamp),
            [Owned::Timestamp(42)]
        );
        assert_eq!(
            reached(&source, &ColumnRef::AnyLeaf),
            [Owned::Integer(1), Owned::Integer(5), Owned::Timestamp(42)]
        );

        let boolean_record = json(r#"{"meta": {"time": true}}"#);
        let boolean_source =
            JsonRecord::new(&boolean_record, Some(timestamp)).with_timestamp_key(&key);
        assert_eq!(
            reached(&boolean_source, &path("meta.time")),
            [Owned::Boolean(true)]
        );
        assert_eq!(
            reached(
                &JsonRecord::new(&record, None),
                &ColumnRef::AuthoritativeTimestamp
            ),
            []
        );
    }

    #[test]
    fn range_index_fields_are_reached_through_the_dollar_namespace() {
        let record = json(r#"{"_filename": "record field"}"#);
        let fields = json(r#"{"_filename": "input.jsonl"}"#);
        let source = JsonRecord::new(&record, None).with_range_index_fields(&fields);
        assert_eq!(
            reached(&source, &path("$_filename")),
            [Owned::Text("input.jsonl".into())]
        );
        assert_eq!(
            reached(&JsonRecord::new(&record, None), &path("$_filename")),
            []
        );
    }

    #[test]
    fn visiting_stops_when_the_visitor_says_so() {
        let record = json(r#"{"a": [1, 2, 3]}"#);
        let source = JsonRecord::new(&record, None);
        let mut count = 0;
        source.visit_values(&path("a"), |_| {
            count += 1;
            false
        });
        assert_eq!(count, 1);
    }
}
