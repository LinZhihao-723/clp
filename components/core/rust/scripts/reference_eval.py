"""Independent Python model of the design report §8.1 search semantics, used to derive the expected
Rust results of the fixture query suite.

It is deliberately small and separate from the Rust implementation: KQL subset parser (columns,
`:`/`>`/`>=`/`<`/`<=`, quoted and unquoted values with escapes, AND/OR with equal precedence and
left associativity, NOT, parentheses, nested `col: {q}`), istringstream-style literal typing, and
K3 evaluation over existential predicates. Records come from the C++ archive itself (`clp-s s '*'`
through the msgpack file output), so every value is the stored value and every timestamp is the
C++-decoded epoch ms.

Not modelled: structured arrays, `@` auto-generated keys (no fixture has any), `timestamp()`
literals, `$` metadata other than `$_filename`.
"""

import json
import math
import re
from fractions import Fraction

INT64_MIN = -(1 << 63)
INT64_MAX = (1 << 63) - 1

T, F, U = "T", "F", "U"


class QueryError(Exception):
    pass


class Text:
    """A text value pattern: list of ('lit', str) | ('star',) | ('any',) parts."""

    def __init__(self, parts):
        self.parts = parts

    def has_wildcard(self):
        return any(p[0] != "lit" for p in self.parts)

    def plain(self):
        return "".join(p[1] for p in self.parts if p[0] == "lit")

    def regex(self, ignore_case):
        out = []
        for p in self.parts:
            if p[0] == "lit":
                out.append(re.escape(p[1]))
            elif p[0] == "star":
                out.append(".*")
            else:
                out.append(".")
        flags = re.DOTALL | (re.IGNORECASE | re.ASCII if ignore_case else 0)
        return re.compile("".join(out) + r"\Z", flags)


ESCAPES = {"b": "\b", "f": "\f", "n": "\n", "r": "\r", "t": "\t"}
SPECIAL = set('()<>:{}"')
WS = " \t\r\n"


def tokenize(text):
    """Yields ('kw', AND|OR|NOT) | ('op', sym) | ('str', raw, quoted) tokens."""
    i = 0
    n = len(text)
    tokens = []
    while i < n:
        c = text[i]
        if c in WS:
            i += 1
            continue
        if c in "(){}":
            tokens.append(("op", c))
            i += 1
            continue
        if c in "<>":
            if i + 1 < n and text[i + 1] == "=":
                tokens.append(("op", c + "="))
                i += 2
            else:
                tokens.append(("op", c))
                i += 1
            continue
        if c == ":":
            tokens.append(("op", ":"))
            i += 1
            continue
        if c == '"':
            j = i + 1
            raw = []
            while j < n and text[j] != '"':
                if text[j] == "\\" and j + 1 < n:
                    raw.append(text[j : j + 2])
                    j += 2
                else:
                    raw.append(text[j])
                    j += 1
            if j >= n:
                raise QueryError("unterminated quote")
            tokens.append(("str", "".join(raw), True))
            i = j + 1
            continue
        j = i
        raw = []
        while j < n and text[j] not in WS and text[j] not in SPECIAL:
            if text[j] == "\\" and j + 1 < n:
                raw.append(text[j : j + 2])
                j += 2
            else:
                raw.append(text[j])
                j += 1
        word = "".join(raw)
        if word in ("AND", "OR", "NOT"):
            tokens.append(("kw", word))
        else:
            tokens.append(("str", word, False))
        i = j
    return tokens


def unescape_value(raw):
    """Returns a Text pattern for a raw (still escaped) value token."""
    parts = []
    lit = []
    i = 0
    while i < len(raw):
        c = raw[i]
        if c == "\\" and i + 1 < len(raw):
            d = raw[i + 1]
            if d == "u" and re.fullmatch(r"[0-9a-fA-F]{4}", raw[i + 2 : i + 6]):
                lit.append(chr(int(raw[i + 2 : i + 6], 16)))
                i += 6
                continue
            lit.append(ESCAPES.get(d, d))
            i += 2
            continue
        if c in "*?":
            if lit:
                parts.append(("lit", "".join(lit)))
                lit = []
            parts.append(("star",) if c == "*" else ("any",))
            i += 1
            continue
        lit.append(c)
        i += 1
    if lit:
        parts.append(("lit", "".join(lit)))
    return Text(parts)


def split_column(raw):
    """Returns the column tokens: a list of str (literal key) or None (`*` wildcard)."""
    tokens = []
    cur = []
    escaped_any = False
    i = 0
    while i < len(raw):
        c = raw[i]
        if c == "\\" and i + 1 < len(raw):
            d = raw[i + 1]
            if d == "u" and re.fullmatch(r"[0-9a-fA-F]{4}", raw[i + 2 : i + 6] or ""):
                cur.append(chr(int(raw[i + 2 : i + 6], 16)))
                i += 6
            else:
                cur.append(ESCAPES.get(d, d))
                i += 2
            escaped_any = True
            continue
        if c == ".":
            tokens.append(_column_token(cur, escaped_any))
            cur = []
            escaped_any = False
            i += 1
            continue
        cur.append(c)
        i += 1
    tokens.append(_column_token(cur, escaped_any))
    if any(t == "" for t in tokens):
        raise QueryError(f"invalid column {raw!r}")
    return tokens


def _column_token(chars, escaped_any):
    s = "".join(chars)
    if s == "*" and not escaped_any:
        return None
    return s


def parse(text):
    tokens = tokenize(text)
    pos = [0]

    def peek():
        return tokens[pos[0]] if pos[0] < len(tokens) else None

    def take():
        tok = peek()
        pos[0] += 1
        return tok

    def expr(prefix):
        node = unary(prefix)
        while True:
            tok = peek()
            if tok is None or tok == ("op", ")") or tok == ("op", "}"):
                return node
            if tok[0] == "kw" and tok[1] in ("AND", "OR"):
                take()
                rhs = unary(prefix)
                node = (tok[1].lower(), node, rhs)
                continue
            raise QueryError(f"unexpected token {tok}")

    def unary(prefix):
        tok = peek()
        if tok == ("kw", "NOT"):
            take()
            return ("not", unary(prefix))
        if tok == ("op", "("):
            take()
            node = expr(prefix)
            if take() != ("op", ")"):
                raise QueryError("missing )")
            return node
        return predicate(prefix)

    def predicate(prefix):
        tok = take()
        if tok is None or tok[0] != "str":
            raise QueryError(f"expected column or value, got {tok}")
        nxt = peek()
        if nxt is not None and nxt[0] == "op" and nxt[1] in (":", ">", ">=", "<", "<="):
            take()
            column = prefix + split_column(tok[1])
            if nxt[1] == ":" and peek() == ("op", "{"):
                take()
                node = expr(column)
                if take() != ("op", "}"):
                    raise QueryError("missing }")
                return node
            value = take()
            if value is None or value[0] != "str":
                raise QueryError("expected value")
            return ("pred", column, nxt[1], value[1])
        return ("pred", prefix + [None] if prefix else [None], ":", tok[1])

    node = expr([])
    if peek() is not None:
        raise QueryError(f"trailing tokens {tokens[pos[0]:]}")
    return node


INT_RE = re.compile(r"[+-]?[0-9]+\Z")
FLOAT_RE = re.compile(r"[+-]?([0-9]+\.?[0-9]*|\.[0-9]+)([eE][+-]?[0-9]+)?\Z")


def type_literal(raw):
    """Returns (kind, value, text): kind in int|float|bool|null|text|any."""
    pattern = unescape_value(raw)
    if pattern.parts == [("star",)]:
        return ("any", None, pattern)
    if pattern.has_wildcard():
        return ("text", None, pattern)
    s = pattern.plain()
    if INT_RE.match(s):
        v = int(s)
        if INT64_MIN <= v <= INT64_MAX:
            return ("int", v, pattern)
    if FLOAT_RE.match(s):
        v = float(s)
        if math.isfinite(v):
            return ("float", v, pattern)
    if s in ("true", "false"):
        return ("bool", s == "true", pattern)
    if s == "null":
        return ("null", None, pattern)
    return ("text", None, pattern)


class Record:
    def __init__(self, value, ts_ns, ts_path):
        self.value = value
        self.ts_ns = ts_ns
        self.ts_path = ts_path


class TsValue:
    """The authoritative timestamp value of a record (stored as Timestamp, epoch ns)."""

    def __init__(self, ns):
        self.ns = ns


def _is_ts_typed(v):
    return isinstance(v, (int, float, str)) and not isinstance(v, bool)


def resolve(record, column):
    """Returns the multiset V(r, C) as a list of values (containers included)."""
    out = []
    if column == [None]:
        _any_leaf(record.value, out, ())
        return [_maybe_ts(record, path, v) for path, v in out]
    _walk(record.value, column, 0, (), out, in_array=False)
    return [_maybe_ts(record, path, v) for path, v in out]


def _maybe_ts(record, path, v):
    if record.ts_path is not None and path == record.ts_path and _is_ts_typed(v):
        return TsValue(record.ts_ns)
    return v


def _any_leaf(v, out, path):
    if isinstance(v, dict):
        for k, x in v.items():
            _any_leaf(x, out, (path + (k,)) if path is not None else None)
    elif isinstance(v, list):
        for x in v:
            _any_leaf(x, out, None)
    else:
        out.append((path, v))


def _emit(v, out, path):
    out.append((path, v))
    if isinstance(v, list):
        for x in v:
            _emit_elem(x, out)


def _emit_elem(v, out):
    out.append((None, v))
    if isinstance(v, list):
        for x in v:
            _emit_elem(x, out)


def _walk(v, column, i, path, out, in_array):
    if i == len(column):
        _emit(v, out, path if not in_array else None)
        return
    if isinstance(v, list):
        for x in v:
            _walk(x, column, i, None, out, True)
        return
    if not isinstance(v, dict):
        return
    tok = column[i]
    child_path = lambda k: (path + (k,)) if path is not None else None  # noqa: E731
    if tok is None:
        _walk(v, column, i + 1, path, out, in_array)
        for k, x in v.items():
            _walk_star(x, column, i, child_path(k), out, in_array)
        return
    if tok in v:
        _walk(v[tok], column, i + 1, child_path(tok), out, in_array)


def _walk_star(v, column, i, path, out, in_array):
    # `*` has consumed this key; it may consume more keys or stop here.
    _walk(v, column, i + 1, path, out, in_array)
    if isinstance(v, dict):
        for k, x in v.items():
            _walk_star(x, column, i, (path + (k,)) if path is not None else None, out, in_array)
    elif isinstance(v, list):
        for x in v:
            _walk_star(x, column, i, None, out, True)


def _text_eq(value, pattern, ignore_case):
    return bool(pattern.regex(ignore_case).match(value))


def compare(values, op, kind, lit, pattern, ignore_case):
    comparable = []
    for v in values:
        res = _compare_one(v, op, kind, lit, pattern, ignore_case)
        if res is not None:
            comparable.append(res)
    if not comparable:
        return U
    return T if any(comparable) else F


def _num(v):
    if isinstance(v, TsValue):
        # D11: numeric literals against the Timestamp column are epoch ms; compare exactly in ns.
        return Fraction(v.ns, 1_000_000)
    if isinstance(v, bool) or not isinstance(v, (int, float)):
        return None
    return v


def _compare_one(v, op, kind, lit, pattern, ignore_case):
    """Returns True/False when comparable, None when not."""
    if isinstance(v, (dict, list)):
        return None
    if op != ":":
        if kind not in ("int", "float"):
            return None
        x = _num(v)
        if x is None:
            return None
        return {">": x > lit, ">=": x >= lit, "<": x < lit, "<=": x <= lit}[op]
    if kind in ("int", "float"):
        x = _num(v)
        if x is not None:
            return x == lit
        if isinstance(v, str):
            return _text_eq(v, pattern, ignore_case)
        return None
    if kind == "bool":
        if isinstance(v, bool):
            return v == lit
        if isinstance(v, str):
            return _text_eq(v, pattern, ignore_case)
        return None
    if kind == "null":
        if v is None:
            return True
        if isinstance(v, str):
            return v == "null"
        return False
    if kind == "text":
        if isinstance(v, str):
            return _text_eq(v, pattern, ignore_case)
        return None
    raise AssertionError(kind)


def k3_not(a):
    return {T: F, F: T, U: U}[a]


def k3_and(a, b):
    if a == F or b == F:
        return F
    if a == U or b == U:
        return U
    return T


def k3_or(a, b):
    if a == T or b == T:
        return T
    if a == U or b == U:
        return U
    return F


def evaluate(node, record, ignore_case):
    tag = node[0]
    if tag == "not":
        return k3_not(evaluate(node[1], record, ignore_case))
    if tag in ("and", "or"):
        a = evaluate(node[1], record, ignore_case)
        b = evaluate(node[2], record, ignore_case)
        return k3_and(a, b) if tag == "and" else k3_or(a, b)
    _, column, op, raw = node
    if column and column[0] == "$_filename":
        # Archives written with --disable-log-order have no range index, so no `$` fields.
        if getattr(record, "filename", None) is None:
            return F if type_literal(raw)[0] == "any" else U
        return (
            T
            if type_literal(raw)[0] == "any" or unescape_value(raw).plain() == record.filename
            else F
        )
    kind, lit, pattern = type_literal(raw)
    if column == [None] and kind == "any":
        return T
    values = resolve(record, column)
    if kind == "any":
        if op != ":":
            raise QueryError("wildcard with range operator")
        return T if values else F
    return compare(values, op, kind, lit, pattern, ignore_case)


def run_query(records, kql, ignore_case=False, tge=None, tle=None):
    """Returns the indices of the matching records."""
    node = parse(kql)
    matches = []
    for i, record in enumerate(records):
        result = evaluate(node, record, ignore_case)
        if tge is not None or tle is not None:
            if record.ts_ns is None:
                bound = U
            else:
                ok = (tge is None or record.ts_ns >= tge * 1_000_000) and (
                    tle is None or record.ts_ns <= tle * 1_000_000
                )
                bound = T if ok else F
            result = k3_and(result, bound)
        if result == T:
            matches.append(i)
    return matches


class _RawFloat(str):
    pass


def _wrap_int64(v):
    if isinstance(v, dict):
        return {k: _wrap_int64(x) for k, x in v.items()}
    if isinstance(v, list):
        return [_wrap_int64(x) for x in v]
    if isinstance(v, int) and not isinstance(v, bool) and not INT64_MIN <= v <= INT64_MAX:
        # Ingestion casts u64 to i64; C++ array search does the same to array elements.
        return ((v + (1 << 63)) % (1 << 64)) - (1 << 63)
    return v


EPOCH_1971_S = 365 * 24 * 3600


def _int_ts_ns(v):
    a = abs(v)
    if a > EPOCH_1971_S * 10**9:
        return v
    if a > EPOCH_1971_S * 10**6:
        return v * 1000
    if a > EPOCH_1971_S * 10**3:
        return v * 1_000_000
    return v * 1_000_000_000


FRACTION_RE = re.compile(r"[0-9]{2}[.,]([0-9]+)")


def _ts_ns(raw, ms):
    """Returns the stored epoch ns of a timestamp value, from its text and the C++ epoch ms."""
    if isinstance(raw, int):
        return _int_ts_ns(raw)
    digits = ""
    if isinstance(raw, _RawFloat):
        if "." in raw and "e" not in raw.lower():
            digits = raw.split(".", 1)[1]
    else:
        found = FRACTION_RE.search(raw)
        if found:
            digits = found.group(1)
    sub_ms = digits[3:9].ljust(6, "0") if len(digits) > 3 else "0"
    rem = int(sub_ms)
    return ms * 1_000_000 + (rem if ms >= 0 else -rem)


def load_records(msgpack_tuples, ts_key):
    """Builds records from the C++ `clp-s s '*' file --path` tuples of an archive."""
    ts_path = tuple(split_column(ts_key)) if ts_key else None
    records = []
    for tup in msgpack_tuples:
        value = _wrap_int64(json.loads(tup[1]))
        ts_ns = None
        if ts_path is not None:
            raw = json.loads(tup[1], parse_float=_RawFloat)
            for k in ts_path:
                raw = raw.get(k) if isinstance(raw, dict) else None
            if _is_ts_typed(raw):
                ts_ns = _ts_ns(raw, tup[0])
        records.append(Record(value, ts_ns, ts_path))
    return records
