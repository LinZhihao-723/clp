//! Array probes: evaluate a leaf on the values an [`ArrayAccess`] reaches inside one decoded
//! unstructured array, streaming over the element scanner's events.
//!
//! The walk is the reference evaluator's (`clp_s_query::JsonRecord`) applied to the array: keys
//! descend into the first member of that name (C++ `evaluate_array_filter_object`; later
//! duplicates are ignored), `*` matches zero or more object levels, arrays are transparent at every
//! step, a path that ends on a non-empty array reaches its elements, and bare values reach every
//! scalar. Each open container carries the walk states its children inherit, so the scan needs no
//! tree and no recursion.

use clp_s_query::PathToken;
use clp_s_query::ScalarValue;
use clp_s_query::TruthValue;
use clp_s_table::ArrayEvent;
use clp_s_table::ArrayScalar;
use clp_s_table::TableError;
use clp_s_table::scan_array;

use crate::ArrayAccess;
use crate::filter::value::ElementTest;

/// A leaf bound to the values an access reaches inside an unstructured array.
#[derive(Debug)]
pub struct ArrayProbe {
    tokens: Box<[PathToken]>,
    root: Task,
    test: ElementTest,
}

impl ArrayProbe {
    /// Factory function.
    ///
    /// # Returns
    ///
    /// The probe of `test` through `access`.
    pub fn new(access: &ArrayAccess, test: ElementTest) -> Self {
        let (tokens, root) = match access {
            ArrayAccess::Walk(tokens) => (tokens.clone(), Task::Walk(0)),
            ArrayAccess::BelowWildcard(tokens) => (tokens.clone(), Task::Below(0)),
            ArrayAccess::AnyLeaf => (Box::default(), Task::AnyLeaf),
        };
        Self { tokens, root, test }
    }

    /// Evaluates the leaf on the array whose JSON text is `text`.
    ///
    /// # Returns
    ///
    /// True if some reached value is True; else False if some reached value is False (or, for
    /// `Exists`, if no value is reached); else Unknown, on success.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * Forwards [`scan_array`]'s return values on failure.
    pub fn evaluate(
        &self,
        text: &[u8],
        scratch: &mut WalkScratch,
    ) -> Result<TruthValue, TableError> {
        scratch.clear();
        let mut walk = Walk {
            probe: self,
            scratch,
            result: if self.test.is_exists() {
                TruthValue::False
            } else {
                TruthValue::Unknown
            },
        };
        scan_array(text, |event| walk.on_event(&event))?;
        Ok(walk.result)
    }
}

/// Reusable buffers of array walks.
#[derive(Debug, Default)]
pub struct WalkScratch {
    frames: Vec<Frame>,

    /// The tasks every child of an open container inherits; frames own contiguous ranges.
    child_tasks: Vec<Task>,

    /// The token positions of `Key` tokens an open object's children are matched against.
    keyed_positions: Vec<usize>,

    /// Whether a member already matched the key at the same index of `keyed_positions`.
    is_keyed_position_matched: Vec<bool>,

    /// The tasks of the next value of the current object (set by its key).
    member_tasks: Vec<Task>,

    incoming: Vec<Task>,
    closure: Vec<Task>,
}

impl WalkScratch {
    /// Empties every buffer.
    fn clear(&mut self) {
        self.frames.clear();
        self.child_tasks.clear();
        self.keyed_positions.clear();
        self.is_keyed_position_matched.clear();
        self.member_tasks.clear();
        self.incoming.clear();
        self.closure.clear();
    }
}

/// One step of the reference walk still to apply to a value.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Task {
    /// Walk the tokens from this position (`Walker::walk`); at the end, reach the value.
    Walk(usize),

    /// The `*` at this position has consumed at least one level (`Walker::walk_below_wildcard`).
    Below(usize),

    /// Reach every scalar below the value (`Walker::any_leaf`).
    AnyLeaf,
}

/// An open container of the scanned array.
#[derive(Debug)]
struct Frame {
    is_object: bool,

    /// The range of `WalkScratch::child_tasks` every child inherits.
    child_tasks_begin: usize,

    /// The range of `WalkScratch::keyed_positions` the children's keys are matched against.
    keyed_positions_begin: usize,

    /// Whether the container is an array reached as a value: it reaches its elements, or itself
    /// as an empty container if it has none.
    is_reach_pending: bool,

    has_child: bool,
}

/// The state of one array walk.
struct Walk<'walk> {
    probe: &'walk ArrayProbe,
    scratch: &'walk mut WalkScratch,
    result: TruthValue,
}

impl Walk<'_> {
    /// Applies one scanner event.
    ///
    /// # Returns
    ///
    /// Whether the scan must continue (the result is not known to be True yet).
    fn on_event(&mut self, event: &ArrayEvent<'_>) -> bool {
        match event {
            ArrayEvent::Key(key) => {
                self.member_tasks(key);
                true
            }
            ArrayEvent::EndArray | ArrayEvent::EndObject => self.close(),
            ArrayEvent::BeginArray => self.open(false),
            ArrayEvent::BeginObject => self.open(true),
            ArrayEvent::Scalar(scalar) => {
                self.collect_incoming();
                self.compute_closure(false);
                let value = scalar_value(scalar);
                for index in 0..self.scratch.closure.len() {
                    let reaches = match self.scratch.closure[index] {
                        Task::Walk(position) => position == self.probe.tokens.len(),
                        Task::AnyLeaf => true,
                        Task::Below(_) => false,
                    };
                    if reaches && !self.reach(value) {
                        return false;
                    }
                }
                true
            }
        }
    }

    /// Computes the tasks of the value after the key `key` of the current object.
    fn member_tasks(&mut self, key: &[u8]) {
        let scratch = &mut *self.scratch;
        scratch.member_tasks.clear();
        let Some(frame) = scratch.frames.last() else {
            return;
        };
        let (tasks_end, positions_end) = (scratch.child_tasks.len(), scratch.keyed_positions.len());
        scratch
            .member_tasks
            .extend_from_slice(&scratch.child_tasks[frame.child_tasks_begin..tasks_end]);
        for index in frame.keyed_positions_begin..positions_end {
            let position = scratch.keyed_positions[index];
            if let Some(PathToken::Key(expected)) = self.probe.tokens.get(position)
                && **expected == *key
                && !scratch.is_keyed_position_matched[index]
            {
                scratch.is_keyed_position_matched[index] = true;
                push_distinct(&mut scratch.member_tasks, Task::Walk(position + 1));
            }
        }
    }

    /// Collects into `incoming` the tasks of the value that starts now.
    fn collect_incoming(&mut self) {
        let scratch = &mut *self.scratch;
        scratch.incoming.clear();
        let tasks_end = scratch.child_tasks.len();
        match scratch.frames.last_mut() {
            None => scratch.incoming.push(self.probe.root),
            Some(frame) if frame.is_object => {
                scratch.incoming.extend_from_slice(&scratch.member_tasks);
            }
            Some(frame) => {
                frame.has_child = true;
                scratch
                    .incoming
                    .extend_from_slice(&scratch.child_tasks[frame.child_tasks_begin..tasks_end]);
            }
        }
    }

    /// Expands `incoming` into `closure`: a `*` (or a consumed `*`) also continues on the value
    /// itself with the next token.
    fn compute_closure(&mut self, is_object: bool) {
        let tokens = &self.probe.tokens;
        let scratch = &mut *self.scratch;
        scratch.closure.clear();
        while let Some(task) = scratch.incoming.pop() {
            if scratch.closure.contains(&task) {
                continue;
            }
            scratch.closure.push(task);
            match task {
                Task::Walk(position)
                    if is_object && Some(&PathToken::AnyDepth) == tokens.get(position) =>
                {
                    scratch.incoming.push(Task::Walk(position + 1));
                }
                Task::Below(position) => {
                    scratch
                        .incoming
                        .push(Task::Walk((position + 1).min(tokens.len())));
                }
                _ => {}
            }
        }
    }

    /// Opens a container value and derives the tasks its children inherit.
    ///
    /// # Returns
    ///
    /// Whether the scan must continue.
    fn open(&mut self, is_object: bool) -> bool {
        self.collect_incoming();
        self.compute_closure(is_object);
        let num_tokens = self.probe.tokens.len();
        let mut frame = Frame {
            is_object,
            child_tasks_begin: self.scratch.child_tasks.len(),
            keyed_positions_begin: self.scratch.keyed_positions.len(),
            is_reach_pending: false,
            has_child: false,
        };
        let mut reaches_itself = false;
        for index in 0..self.scratch.closure.len() {
            let task = self.scratch.closure[index];
            let child_task = match task {
                Task::Walk(position) if position == num_tokens => {
                    if is_object {
                        reaches_itself = true;
                        None
                    } else {
                        frame.is_reach_pending = true;
                        Some(task)
                    }
                }
                Task::Walk(position) if is_object => {
                    if Some(&PathToken::AnyDepth) == self.probe.tokens.get(position) {
                        Some(Task::Below(position))
                    } else {
                        self.scratch.keyed_positions.push(position);
                        self.scratch.is_keyed_position_matched.push(false);
                        None
                    }
                }
                Task::Walk(_) | Task::Below(_) | Task::AnyLeaf => Some(task),
            };
            if let Some(child_task) = child_task {
                let begin = frame.child_tasks_begin;
                if !self.scratch.child_tasks[begin..].contains(&child_task) {
                    self.scratch.child_tasks.push(child_task);
                }
            }
        }
        self.scratch.frames.push(frame);
        !reaches_itself || self.reach(ScalarValue::Object)
    }

    /// Closes the innermost container.
    ///
    /// # Returns
    ///
    /// Whether the scan must continue.
    fn close(&mut self) -> bool {
        let Some(frame) = self.scratch.frames.pop() else {
            return true;
        };
        self.scratch.child_tasks.truncate(frame.child_tasks_begin);
        self.scratch
            .keyed_positions
            .truncate(frame.keyed_positions_begin);
        self.scratch
            .is_keyed_position_matched
            .truncate(frame.keyed_positions_begin);
        if frame.is_reach_pending && !frame.has_child {
            return self.reach(ScalarValue::EmptyContainer);
        }
        true
    }

    /// Folds one reached value into the result.
    ///
    /// # Returns
    ///
    /// Whether the scan must continue (false once the result is True).
    fn reach(&mut self, value: ScalarValue<'_>) -> bool {
        match self.probe.test.outcome(value) {
            TruthValue::True => {
                self.result = TruthValue::True;
                false
            }
            TruthValue::False => {
                self.result = TruthValue::False;
                true
            }
            TruthValue::Unknown => true,
        }
    }
}

/// # Returns
///
/// The reference value of an array scalar (unsigned integers wrap to `i64`, like the writer).
fn scalar_value<'value>(scalar: &'value ArrayScalar<'_>) -> ScalarValue<'value> {
    match scalar {
        ArrayScalar::Null => ScalarValue::Null,
        ArrayScalar::Boolean(value) => ScalarValue::Boolean(*value),
        ArrayScalar::Integer(value) => ScalarValue::Integer(*value),
        ArrayScalar::UnsignedInteger(value) => ScalarValue::Integer(value.cast_signed()),
        ArrayScalar::Float(value) => ScalarValue::Float(*value),
        ArrayScalar::String(text) => ScalarValue::Text(text),
    }
}

/// Adds `task` to `tasks` unless it is already there.
fn push_distinct(tasks: &mut Vec<Task>, task: Task) {
    if !tasks.contains(&task) {
        tasks.push(task);
    }
}

#[cfg(test)]
mod tests {
    use clp_s_query::ColumnPattern;
    use clp_s_query::ColumnRef;
    use clp_s_query::JsonRecord;
    use clp_s_query::Leaf;
    use clp_s_query::Namespace;
    use clp_s_query::PathToken;
    use clp_s_query::Query;
    use clp_s_query::QueryOptions;
    use clp_s_query::TruthValue;

    use super::ArrayProbe;
    use super::WalkScratch;
    use crate::ArrayAccess;
    use crate::filter::value::ElementTest;

    const ARRAYS: &[&str] = &[
        "[]",
        "[[]]",
        "[1, 2, 3]",
        "[1.5, -0, 18446744073709551615]",
        r#"["a", "hello world", "5", "null", "TRUE"]"#,
        "[true, false, null]",
        "[[1, [2, [3]]], {}]",
        r#"[{"x": 1}, {"x": [2, {"x": 3}]}, {"y": {"x": 4}}]"#,
        r#"[{"a": {"b": {"c": 7}}}, {"a": [{"b": 8}]}, {"b": 9}]"#,
        r#"[{"x": {}}, {"x": []}, {"x": null}, {"x": "x y"}]"#,
        r#"[{"k": [1, {"k": "v"}]}, [[{"k": true}]]]"#,
        r#"[{"": 1}, {"a\"b": 2}, {"x": {"": {"y": 5}}}]"#,
    ];

    const ACCESSES: &[(&str, &str)] = &[
        ("walk", ""),
        ("walk", "x"),
        ("walk", "a.b"),
        ("walk", "a.b.c"),
        ("walk", "k"),
        ("walk", "*"),
        ("walk", "*.x"),
        ("walk", "a.*"),
        ("walk", "a.*.c"),
        ("below", "*"),
        ("below", "*.x"),
        ("below", "*.b"),
        ("below", "*.c"),
        ("any", ""),
    ];

    const TESTS: &[(&str, bool)] = &[
        ("v: *", true),
        ("v: 1", true),
        ("v: 3", true),
        ("v > 2", true),
        ("v < 0", true),
        ("v: 1.5", true),
        ("v: -1", true),
        ("v: null", true),
        ("v: true", true),
        ("v: true", false),
        ("v: 5", true),
        ("v: a", true),
        ("v: \"hello*\"", true),
        ("v: \"x y\"", true),
        ("v: (1 v)", true),
        ("v: \"?\"", true),
    ];

    /// # Returns
    ///
    /// The tokens of `path` (empty for the empty path).
    fn tokens(path: &str) -> Box<[PathToken]> {
        if path.is_empty() {
            return Box::new([]);
        }
        ColumnPattern::parse(path)
            .expect("the path should parse")
            .tokens
    }

    /// # Returns
    ///
    /// The reference value of `leaf` on what `access` reaches in `array`: the reference walker on
    /// a record holding the array under a key, reached through that key (or, below a wildcard,
    /// through `*` from the record).
    fn reference(leaf: &Leaf, kind: &str, path: &str, array: &serde_json::Value) -> TruthValue {
        const KEY: &str = "\u{1}array";
        let record = serde_json::json!({ KEY: array });
        let mut path_tokens = vec![PathToken::Key(KEY.as_bytes().into())];
        let column = match kind {
            "walk" => {
                path_tokens.extend(tokens(path).into_vec());
                ColumnRef::Path(ColumnPattern {
                    namespace: Namespace::Default,
                    tokens: path_tokens.into_boxed_slice(),
                })
            }
            "below" => {
                // `*` + rest from the record reaches the record itself and the array below `*`;
                // keep only the walk below the array by reaching the array through its key and
                // then a leading `*` over the array's members.
                let mut below = tokens(path).into_vec();
                below.remove(0);
                return below_reference(leaf, &below, array);
            }
            _ => ColumnRef::AnyLeaf,
        };
        let leaf = Leaf {
            column,
            ..leaf.clone()
        };
        leaf.evaluate(&JsonRecord::new(&record, None))
    }

    /// # Returns
    ///
    /// The reference value of `leaf` on `walk_below_wildcard(array, [*] + rest)`, computed as the
    /// union over every value at depth one or more below the array (objects descended, arrays
    /// transparent) of walking `rest` from it.
    fn below_reference(leaf: &Leaf, rest: &[PathToken], array: &serde_json::Value) -> TruthValue {
        const KEY: &str = "\u{1}value";
        let mut values = Vec::new();
        collect_below(array, &mut values);
        let mut result = if matches!(leaf.test, clp_s_query::PredicateTest::Exists) {
            TruthValue::False
        } else {
            TruthValue::Unknown
        };
        for value in values {
            let record = serde_json::json!({ KEY: value });
            let mut path_tokens = vec![PathToken::Key(KEY.as_bytes().into())];
            path_tokens.extend(rest.iter().cloned());
            let leaf = Leaf {
                column: ColumnRef::Path(ColumnPattern {
                    namespace: Namespace::Default,
                    tokens: path_tokens.into_boxed_slice(),
                }),
                ..leaf.clone()
            };
            match leaf.evaluate(&JsonRecord::new(&record, None)) {
                TruthValue::True => return TruthValue::True,
                TruthValue::False => result = TruthValue::False,
                TruthValue::Unknown => {}
            }
        }
        result
    }

    /// Collects `value` itself and, below objects and arrays, every value `walk_below_wildcard`
    /// continues from.
    fn collect_below(value: &serde_json::Value, values: &mut Vec<serde_json::Value>) {
        values.push(value.clone());
        match value {
            serde_json::Value::Object(object) => {
                for child in object.values() {
                    collect_below(child, values);
                }
            }
            serde_json::Value::Array(elements) => {
                for element in elements {
                    collect_below(element, values);
                }
            }
            _ => {}
        }
    }

    #[test]
    fn probes_agree_with_the_reference_walker() {
        let mut scratch = WalkScratch::default();
        let mut num_checked = 0;
        let mut num_true = 0;
        for (kql, is_case_sensitive) in TESTS {
            let options = QueryOptions::default().with_case_sensitivity(*is_case_sensitive);
            let query = Query::from_kql(kql, &options).expect("the query should parse");
            let leaf = &query.leaves()[0];
            for (kind, path) in ACCESSES {
                let access = match *kind {
                    "walk" => ArrayAccess::Walk(tokens(path)),
                    "below" => ArrayAccess::BelowWildcard(tokens(path)),
                    _ => ArrayAccess::AnyLeaf,
                };
                let probe = ArrayProbe::new(&access, ElementTest::new(leaf));
                for text in ARRAYS {
                    let array: serde_json::Value =
                        serde_json::from_str(text).expect("the array should parse");
                    let expected = reference(leaf, kind, path, &array);
                    let actual = probe
                        .evaluate(text.as_bytes(), &mut scratch)
                        .expect("the array should scan");
                    assert_eq!(
                        actual, expected,
                        "{kql} (case-sensitive {is_case_sensitive}) via {kind} `{path}` on {text}"
                    );
                    num_checked += 1;
                    num_true += usize::from(TruthValue::True == actual);
                }
            }
        }
        assert!(
            num_true > 100,
            "only {num_true} of {num_checked} probes are True"
        );
    }

    #[test]
    fn keys_reach_only_their_first_duplicate() {
        let mut scratch = WalkScratch::default();
        for (kql, path, text, expected) in [
            ("v: 1", "b", r#"[{"b": 1, "b": 2}]"#, TruthValue::True),
            ("v: 2", "b", r#"[{"b": 1, "b": 2}]"#, TruthValue::False),
            (
                "v: 6",
                "b.c",
                r#"[{"b": {"c": 5}, "b": {"c": 6}}]"#,
                TruthValue::False,
            ),
            (
                "v: 5",
                "b.c",
                r#"[{"b": {"c": 5}, "b": {"c": 6}}]"#,
                TruthValue::True,
            ),
            (
                "v: 2",
                "b",
                r#"[{"b": 1, "b": 2}, {"b": 2}]"#,
                TruthValue::True,
            ),
            (
                "v: 2",
                "b",
                r#"[{"b": {"b": 2}, "b": 2}]"#,
                TruthValue::Unknown,
            ),
            ("v: 2", "*", r#"[{"b": 1, "b": 2}]"#, TruthValue::True),
        ] {
            let query = Query::from_kql(kql, &QueryOptions::default()).expect("the query parses");
            let probe = ArrayProbe::new(
                &ArrayAccess::Walk(tokens(path)),
                ElementTest::new(&query.leaves()[0]),
            );
            assert_eq!(
                probe
                    .evaluate(text.as_bytes(), &mut scratch)
                    .expect("the array scans"),
                expected,
                "{kql} via `{path}` on {text}"
            );
        }
    }

    #[test]
    fn invalid_arrays_are_errors() {
        let query = Query::from_kql("v: 99", &QueryOptions::default()).expect("the query parses");
        let probe = ArrayProbe::new(&ArrayAccess::AnyLeaf, ElementTest::new(&query.leaves()[0]));
        let mut scratch = WalkScratch::default();
        for text in ["", "{}", "[1,", "[1 2]", "[\"x]"] {
            let result = probe.evaluate(text.as_bytes(), &mut scratch);
            assert!(result.is_err(), "{text:?}: {result:?}");
        }
        assert_eq!(
            probe
                .evaluate(b"[99, oops", &mut scratch)
                .expect("the scan stops at the first match"),
            TruthValue::True
        );
    }

    #[test]
    fn deep_arrays_are_walked_without_recursion() {
        let query = Query::from_kql("v: 1", &QueryOptions::default()).expect("the query parses");
        let probe = ArrayProbe::new(
            &ArrayAccess::Walk(Box::new([])),
            ElementTest::new(&query.leaves()[0]),
        );
        let depth = 100_000;
        let mut text = "[".repeat(depth);
        text.push('2');
        text.push_str(&"]".repeat(depth));
        let mut scratch = WalkScratch::default();
        assert_eq!(
            probe
                .evaluate(text.as_bytes(), &mut scratch)
                .expect("the deep array scans"),
            TruthValue::False
        );
    }
}
