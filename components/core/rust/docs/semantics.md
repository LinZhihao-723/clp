# Search semantics of the Rust clp-s reader (M0-M3)

**Scope:** M0c, `clp-s-query`. **Status:** the specification for this round. It writes out design
report §8.1 with the decisions of §8.2 and the in-scope entries of the divergence register (§8.3),
as [DESIGN.md](DESIGN.md) §6.1 and §10 scope them.

The executable form of this document is `Query::evaluate` over `JsonRecord` (crate `clp-s-query`).
Its tests follow the sections below: `tests/kql.rs` (§1-§2), `tests/semantics.rs` (§3-§7, one test
per rule and per divergence), `tests/normalize.rs` (§8), and `tests/suite.rs`, which runs the whole
query suite through the evaluator over the C++ `clp-s x` output of every fixture and dataset and
compares it with the C++ `clp-s s` oracles (§10).

## 1. KQL

The grammar is `components/core/src/clp_s/search/kql/Kql.g4`, parsed by hand without recursion.

### 1.1 Tokens

ANTLR's lexer rules apply: the longest match wins, and on a tie the earlier rule wins (implicit
tokens, then `AND`, `OR`, `NOT`, then literals).

| Token | Text |
|---|---|
| punctuation | `(` `)` `{` `}` `:`; `,` and `timestamp(` only where a literal cannot be longer |
| range operator | `<` `<=` `>` `>=` |
| keyword | a whole unquoted literal equal to `and`, `or` or `not` in any letter case |
| quoted literal | `"` ... `"`. A quote preceded by a backslash may continue the string; the longest match is taken (`"a\" b"` is one token, `"a\"` at the end of the query is the string `a\`). An unterminated string is a syntax error. |
| unquoted literal | any byte except `\ ( ) : < > " { }`, space, `\t`, `\r`, `\n`, plus the escapes `\t`, `\r`, `\n`, `\` before one of `\():<>"*?{}.@$!#`, and `\u` with four hexadecimal digits. Any other backslash is an error (`InvalidEscape`). |
| whitespace | space, `\t`, `\r`, `\n` only |

Keywords are never literals: a column or value named `not` must be quoted.

### 1.2 Grammar

```
query      := unary ((AND | OR) unary)*        AND and OR: equal precedence, left-associative
unary      := NOT unary | primary              NOT binds tighter than AND and OR
primary    := '(' query ')'
            | column ':' '{' query '}'         nested query
            | column ':' '(' [AND | OR | NOT] literal* ')'
            | column ':' (literal | timestamp)
            | column range (literal | timestamp)
            | literal                          bare value
timestamp  := 'timestamp(' quoted [',' quoted] ')'
```

- `a OR b AND c` is `(a OR b) AND c`. Runs of one connective form one n-ary node.
- A list is an OR of equalities on the column; `(AND ...)` is an AND of them; `(NOT ...)` is an
  AND of their negations. `()` is an OR of nothing (False) and `(AND)` an AND of nothing (True).
- A nested query prefixes every column inside it with its column, including the implicit `*` of a
  bare value (`a: {foo}` is `a.*: foo`). A column inside a nested query must not declare a
  namespace (`NamespaceInNestedQuery`); the prefix's namespace applies.
- Quoting a column does not protect its dots: `"a.b"` is the path `a`, `b`.
- Depth (a predicate is 1; NOT, AND and OR add 1; parentheses add nothing) is bounded by
  `QueryLimits::max_depth` (1,024): beyond it the parser fails with `LimitExceeded` before it
  builds the tree.

### 1.3 Columns

C++ `tokenize_column_descriptor`: a first byte `@`, `$`, `!` or `#` selects the namespace
(auto-generated, range index, reserved, reserved); unescaped dots split tokens; no token may be
empty; a token that is exactly an unescaped `*` is the wildcard `AnyDepth`; consecutive wildcards
collapse. In keys, `\.` is a dot, `?` is literal, `\uXXXX` is the code point, and `\*` is a
literal star. The pure wildcard `*` in the default namespace is `ColumnRef::AnyLeaf`; any column in
the `$` namespace is `ColumnRef::RangeIndexField`.

### 1.4 Literals

A literal is unescaped (C++ `unescape_kql_value`: `\\ \" \t \r \n \b \f \{ \} \( \) \< \> \@ \$ \!
\#`, and `\uXXXX` for a non-surrogate code point; `\*`, `\?` and the escapes that produce `*`, `?`
or `\` stay literal characters; anything else, including `\.` and `\:` in a value, is an
`InvalidEscape`). Then it is typed in C++ order, identically whether quoted or not:

1. an integer if `istringstream >> std::noskipws >> int64_t` reads all of it: an optional sign
   and decimal digits within the `i64` range (`+5`, `007`, `-0`);
2. else a float if libstdc++'s `istringstream >> double` reads all of it: an optional sign,
   digits with at most one point and at least one digit, an optional exponent with digits, and a
   finite result (`5.`, `.5`, `+.5e-2`, `9223372036854775808`, `1e-400`). `0x10`, `inf`, `nan`,
   `1e400` and `1,5` are text;
3. else `true` or `false` (lower case only), else `null`;
4. else a text pattern: unescaped `*` is any run of bytes, unescaped `?` is exactly one byte.

The literal `*` (after collapsing `**`) under `:` is the test `Exists`. Under a range operator it
is a text pattern, which no value is comparable with.

### 1.5 Timestamp literals

`timestamp("t")` and `timestamp("t", "pattern")` compare as the type Timestamp in epoch
nanoseconds. Both strings keep their quotes, which match each other, as in C++. This round supports
patterns made of literal bytes and one of `\E`, `\L`, `\C`, `\N` (epoch s, ms, us, ns) or `\P`
(precision estimated from the magnitude), and the one-argument form for an epoch integer string.
Date-time patterns and the C++ default date-time patterns fail with `InvalidTimestamp` (gap; they
need the timestamp-parser port in `clp-s-timestamp`).

## 2. Values reached by a column

For a record `r` and a column reference `C`, `V(r, C)` is the multiset of values `C` reaches.

- **Keys.** A key token descends into the object member of that name.
- **Wildcard.** `*` matches zero or more object levels, so `a.*.b` reaches `a.b`, `a.x.b`,
  `a.x.y.b`; it also matches a key that is the empty string. At the end of a path, `a.*` reaches
  every value strictly below `a` and, when `a` is an object, `a` itself.
- **Arrays are transparent** at every step, for literal paths and wildcards alike (DV-36): a path
  that meets an array continues into each element, at any array depth, and a path that ends on a
  non-empty array reaches its elements (recursively), not the array.
- **Duplicate keys** survive only inside objects in unstructured arrays (C++ drops a record with a
  duplicate key at ingestion, but keeps the raw array text). A key token reaches only the first
  member of that name, as C++ `QueryRunner::evaluate_array_filter_object` does: `a.b: 1` matches
  `{"a": [{"b": 1, "b": 2}]}` and `a.b: 2` does not. A wildcard reaches every member (`a.*: 2`
  matches). The reference evaluators (`clp_s_query::JsonRecord` over `serde_json`, and
  `scripts/reference_eval.py`) keep the last member instead, so they are known to disagree on such
  records; no fixture has one, and `filter::array` unit tests pin the archive behaviour.
- **Empty keys** are ordinary keys; they are never skipped (DV-16): `a.b` does not reach
  `{"a": {"": {"b": 1}}}`, `a.*.b` does (DV-38).
- **Bare values** (`AnyLeaf`) reach every scalar leaf at any depth, through objects and arrays, in
  every namespace (Q2).
- **Objects and empty containers** are reached as values that only `Exists` accepts: a non-empty
  object as `Object`, and `{}`, `[]` (also inside arrays) as `EmptyContainer`.
- **The timestamp column.** A value reached at the archive's timestamp key, outside any array, that
  is a number or a string is the authoritative timestamp (type Timestamp, epoch ns as stored).
  The timestamp index is bound to that column's node, so the same key in another namespace
  (`@timestamp` when the key is `timestamp`) is an ordinary column that the index never prunes
  (DV-18). No fixture has such a column (`log-converter` writes no auto-generated keys), so this
  half of DV-18 is covered only by a planner test, not at the record level.
- **Range-index fields** (`$` namespace) are the fields of the record's `log_event_idx` range, such
  as `$_filename`.
- **Stored values.** Values are typed as the C++ writer stores them: integers beyond `i64::MAX`
  wrap to `i64` (also inside arrays), `-0` is `0`, and plain floats keep their stored value (with
  `--no-retain-float-format` C++ prints them with `%f`, which is lossy).

## 3. Predicates and connectives

- `Exists` is True if `V(r, C)` is non-empty and False otherwise; it is never Unknown (DV-07,
  DV-19).
- A comparison is Unknown if no reached value is comparable with the literal under the operator
  (§4); otherwise it is True if some comparable value satisfies it, and False if none does.
- An in-set test (an OR of equalities on one column, see §8) is Unknown if no reached value is
  comparable with any member, True if some value equals some member, and False otherwise.
- AND, OR and NOT follow Kleene's strong three-valued logic. A record matches iff the root is True.
- A column pattern is a quantifier, not a disjunction: `NOT a.*.b: 1` is "some comparable value is
  reached and none equals 1" (DV-08). Expanding the pattern into an OR of paths would be wrong
  under K3.

## 4. Coercion table

"Comparable" is decided per reached value. `x` is the reached value, `v` the literal.

| Literal | `:` is comparable with | Ranges are comparable with |
|---|---|---|
| Integer `v` (text `t`) | Integer, Float, Timestamp, Text (`x` equals `t` exactly: `a:5` matches `"5"`, not `"5.0"` or `"+5"`) | Integer, Float, Timestamp |
| Float `v` (text `t`) | Integer, Float, Timestamp, Text (`x` equals `t`) | Integer, Float, Timestamp |
| Boolean | Boolean, Text `"true"` / `"false"` | nothing (Unknown) |
| `null` | every scalar: True iff `x` is null or the text `"null"` (DV-10, DV-12) | nothing |
| Text pattern | Text (whole-value byte glob) | nothing |
| Timestamp `v` (ns) | Timestamp (ns), Integer (ns, the 0.5.x unit), Float (seconds) | the same |

- **Exact numerics (DV-14).** Every Integer/Float/Timestamp comparison is exact over the rationals:
  `9007199254740993` does not equal `9007199254740992.0`, and `a > 1e30` is False for 2000.
- **Numeric literals against a Timestamp are in epoch milliseconds (DV-15):** `ts > 1700000004123`
  compares the stored nanoseconds with `1700000004123 * 10^6`.
- **Text comparisons** of numeric, boolean and null literals use the predicate's case folding.
- Objects and empty containers are comparable with nothing.

## 5. Storage rule

C++ stores a string as `ClpString` iff it contains a space, else as `VarString`. The rule only
prunes: a text pattern without a space or wildcard is False (not Unknown) on a `ClpString` value,
which is what the whole-value comparison gives anyway, so `NOT msg: hello` matches
`"hello world"`. The evaluator needs no special case; the archive planner uses it to skip columns.

## 6. Case and wildcards

- Case-insensitive matching (`-i`) is a property of each predicate and folds ASCII letters only.
- Wildcards count bytes: `?` matches one byte of a multi-byte character.
- `*` and `*:*` (and every pure-wildcard spelling) match every record, including `{}`; `NOT *`
  matches nothing (DV-09).

## 7. Time bounds

`--tge`/`--tle` (epoch ms, converted to ns with saturation, DV-42) add one inclusive comparison
leaf per set bound on the authoritative timestamp, ANDed with the query. A record without an
authoritative timestamp makes them Unknown, so it never matches a bounded search. Bounds with
`begin > end` fold the query to `MatchNone`. A query on an archive without a timestamp column
returns no record and is not an error (DV-20).

## 8. Normalization

`Query::from_kql` / `Query::from_expr` produce negation normal form over a table of distinct
leaves, once per request:

- negations are pushed to the leaves (De Morgan); nested ANDs and ORs are flattened;
- constants fold under K3: an AND of nothing is True, an OR of nothing is False;
- a comparison whose literal no value can be compared with under its operator (`a > foo`,
  `a < null`) is Unknown for every record and folds to False, in both polarities; this is exact for
  matching, since in negation normal form an Unknown operand never makes an AND or OR True. Such a
  query folds to `MatchNone` (DV-20: no error);
- `Exists` on `AnyLeaf` (`*`, `*:*`) is `MatchAll`, its negation `MatchNone`;
- equalities on one column with the same case sensitivity and the same comparable search types
  fold into one in-set leaf: in an OR, the positive ones; in an AND, the negated ones (De Morgan).
  Equal search types keep the fold exact under K3; `a: (1 foo)` stays two leaves;
- equal leaves share one `LeafId`, and a repeated leaf under one connective appears once;
- limits: depth ≤ `max_depth` (checked iteratively) and distinct leaves ≤ `max_leaves`
  (`LimitExceeded`).

`Query::evaluate` returns the K3 value of the normalized query, so a constant-Unknown subexpression
evaluates to False; per-record Unknowns are kept.

## 9. Divergences from C++ in scope

Each entry has a positive test in `tests/semantics.rs` or suite cases (`tests/queries/suite.toml`,
checked by `tests/suite.rs` and `clp-s-difftest queries`), usually both.

| ID | C++ | Rust | Test |
|---|---|---|---|
| DV-05 | `NOT v` / `NOT *: v` complements over the archive's string encodings: `NOT ERROR` on bench returns 1,000,000 | K3 over comparable leaves: 750,396 | `negated_bare_values_are_domain_restricted_dv_05`, `bench-not-bare` |
| DV-06 | the bug-254 rewrite adds `EXISTS` of the other string encoding to only one not-equal filter per AND parent, so `NOT a: foo AND NOT a: bar` loses `ClpString` values | K3; no rewrite (the storage rule prunes instead, §5) | `clp-negation-and`, `clp-negation-not-or` (suite only) |
| DV-07 | `NOT p: *` on a path absent from the archive matches nothing | matches every record without the path | `nexists_matches_every_record_lacking_the_path_dv_07` |
| DV-08 | NOT and NEXISTS over a partial wildcard mean ∃¬ | ¬∃ | `column_patterns_are_quantifiers_not_disjunctions_dv_08` |
| DV-09 | `*:null` returns everything when any record has a null; `NOT *:null`, `NOT *:*` return everything | K3; `NOT *` is `MatchNone` | `star_matches_every_record_and_not_star_none_dv_09` |
| DV-10 | `{a:"null"}` matches both `a:null` and `NOT a:null` | only `a:null` | `null_literals_follow_the_coercion_table_dv_10_dv_12` |
| DV-11 | array range operators evaluate as `!=` | typed element comparisons (`poly_range`: 30,781) | `array_elements_are_typed_and_ranges_apply_dv_11_dv_12`, `synthetic-poly-range` |
| DV-12 | a literal path into an array binds regardless of type (`a: null` matches every array) | elements are typed | same tests |
| DV-13 | per-row N/A inside arrays is False before NOT | K3 per row | `not_forms_are_unknown_on_empty_value_sets_dv_13` |
| DV-14 | `double_as_int` undefined behaviour | exact rational comparison | `float_literals_compare_exactly_dv_14` |
| DV-15 | integer literal vs Timestamp: ns at row level, ms in the index | ms everywhere | `numeric_literals_compare_with_timestamps_in_milliseconds_dv_15` |
| DV-16 | empty keys are transparent | never transparent (no structured arrays this round) | `empty_keys_are_never_transparent_dv_16_dv_38` |
| DV-19 | objects never satisfy `Exists` | objects and empty containers do | `objects_and_empty_containers_satisfy_exists_dv_19` |
| DV-20 | a logically false query exits 1 | folds to `MatchNone`, no error | `logically_false_queries_fold_without_an_error_dv_20` |
| DV-36 | partial wildcards reach array values only through another record's literal copy | wildcards descend into arrays | `literal_paths_and_wildcards_descend_into_arrays_dv_36` |
| DV-37 | one array suffix per descriptor | every binding with its own suffix | `wildcards_bind_every_matching_path_dv_37_dv_39` |
| DV-38 | a wildcard match below a `""` key is lost unless another record has the path without it | archive-independent | `empty_keys_are_never_transparent_dv_16_dv_38`, `empty-key-wildcard-*` |
| DV-39 | one descriptor binding two columns of a schema searches only the smaller node id | ∃ over every bound value | `wildcards_bind_every_matching_path_dv_37_dv_39`, `empty-key-order-*` |
| DV-42 | `tge * 10^6` overflows | saturates | `time_bounds_saturate_dv_42` |

Archive independence: a record's match status depends only on that record and the query. The
reference evaluator sees one record at a time, so it holds by construction; the archive search must
reproduce it (difftest classifier, design report §8.3).

**Not register entries, but deliberate:** an AND of nothing is True (C++ treats an empty AND as
false, DD §7 #6); multiple epoch specifiers in one timestamp pattern are rejected (C++ keeps the
last).

## 10. Evidence

- `tests/suite.rs`: all 308 suite queries parse. 262 fixture queries are evaluated over the C++ `x`
  output of their fixtures and select exactly the C++ `s` records (or, for the 48 tagged ones, the
  stated Rust ids, counts or exit), and all 32 dataset queries (bench, spider, synthetic) select the
  C++ count or the registered count (`--ignored`, release). 14 fixture queries are not evaluated:
  5 projections (planner layer), 3 results-cache top-k and 1 msgpack DV-21 query (execution layer),
  and 5 queries on `timestamps-string` fixtures whose non-ISO timestamps the test does not decode.
- The reference model `scripts/reference_eval.py` is an independent implementation of the same
  rules; it agrees with this evaluator on every checked query because both agree with the oracles.
  Where the two could differ: the model compares `null` with `"null"` case-sensitively under `-i`,
  while this evaluator folds case like every other text comparison (no suite query exercises it).

## 11. Decisions taken from design report §8.2

Numeric literal vs Timestamp in epoch ms; Timestamp literal vs Float in seconds; objects and empty
containers satisfy `Exists`; `null` matches the text `"null"`; wildcards descend into arrays; empty
keys are never transparent (no structured arrays in scope); AND/OR keep equal precedence and left
associativity; `AnyLeaf` searches `@` leaves.
