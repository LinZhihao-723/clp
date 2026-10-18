//! The canonical form of JSON-lines output.
//!
//! It is byte-identical to `canonical_line` and `canonical_digest` in `scripts/corpus.py`: object
//! keys sorted, numbers kept as their verbatim token text, strings re-escaped like Python's
//! `json.dumps(ensure_ascii=False)`, invalid lines kept as `INVALID:<text>`.

use std::io::BufRead;
use std::path::Path;

use clp_s_query::ColumnPattern;
use clp_s_query::PathToken;
use sha2::Digest;
use sha2::Sha256;

/// The prefix of the canonical form of a line that is not valid JSON.
pub const INVALID_PREFIX: &[u8] = b"INVALID:";

/// The digest of a JSON-lines output.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CanonicalDigest {
    /// The number of non-empty lines.
    pub count: u64,

    /// The hex SHA-256 of the sorted canonical lines, each followed by `\n`.
    pub sha256: String,

    /// The number of lines that are not valid JSON.
    pub num_invalid: u64,
}

impl CanonicalDigest {
    /// Factory function.
    ///
    /// # Returns
    ///
    /// The digest of `data`, with the canonical lines sorted in memory.
    #[must_use]
    pub fn of_jsonl(data: &[u8]) -> Self {
        let mut lines = CanonicalLines::default();
        for line in data.split(|byte| *byte == b'\n') {
            lines.push(line);
        }
        lines.finish()
    }

    /// Factory function.
    ///
    /// Streams a JSON-lines file; only the canonical lines are kept in memory.
    ///
    /// # Returns
    ///
    /// The digest of the file's contents on success.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * [`anyhow::Error`] if the file cannot be opened.
    /// * Forwards [`BufRead::read_until`]'s return values on failure.
    pub fn of_jsonl_file(path: &Path) -> anyhow::Result<Self> {
        let file = std::fs::File::open(path)
            .map_err(|error| anyhow::anyhow!("cannot open {}: {error}", path.display()))?;
        let mut reader = std::io::BufReader::with_capacity(1 << 20, file);
        let mut lines = CanonicalLines::default();
        let mut line = Vec::new();
        loop {
            line.clear();
            if reader.read_until(b'\n', &mut line)? == 0 {
                break;
            }
            if line.last() == Some(&b'\n') {
                line.pop();
            }
            lines.push(&line);
        }
        Ok(lines.finish())
    }
}

/// A JSON value as Python's `json.loads` sees it in `scripts/corpus.py`.
///
/// Strings hold the WTF-8 encoding of the Python `str`: invalid UTF-8 input bytes become the
/// surrogate-escape code points U+DC80..U+DCFF and `\u` escapes may yield lone surrogates, so that
/// comparing the bytes orders strings by code point exactly like Python.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum JsonValue {
    Null,
    Boolean(bool),

    /// The verbatim token text (`NaN`, `Infinity` and `-Infinity` included).
    Number(Vec<u8>),

    String(Vec<u8>),
    Array(Vec<Self>),

    /// The members in input order, duplicates included.
    Object(Vec<(Vec<u8>, Self)>),
}

impl JsonValue {
    /// Parses one JSON document with Python's `json.loads` grammar.
    ///
    /// # Returns
    ///
    /// The value, or `None` if `text` is not valid JSON.
    #[must_use]
    pub fn parse(text: &[u8]) -> Option<Self> {
        Parser::new(text, false).parse_document()
    }

    /// Parses one JSON document like [`Self::parse`], but also accepts the non-finite tokens
    /// C++ `std::to_string(double)` prints (`nan`, `-nan`, `inf`, `-inf`) as `null` (DV-02).
    ///
    /// # Returns
    ///
    /// The value, or `None` if `text` is not valid JSON even with the C++ tokens.
    #[must_use]
    pub fn parse_with_cpp_non_finite(text: &[u8]) -> Option<Self> {
        Parser::new(text, true).parse_document()
    }

    /// Appends the canonical serialization: sorted keys (stable for duplicates), verbatim
    /// numbers, minimal escaping and no whitespace.
    pub fn write_canonical(&self, output: &mut Vec<u8>) {
        match self {
            Self::Null => output.extend_from_slice(b"null"),
            Self::Boolean(true) => output.extend_from_slice(b"true"),
            Self::Boolean(false) => output.extend_from_slice(b"false"),
            Self::Number(token) => output.extend_from_slice(token),
            Self::String(text) => write_string(text, output),
            Self::Array(elements) => {
                output.push(b'[');
                for (index, element) in elements.iter().enumerate() {
                    if index > 0 {
                        output.push(b',');
                    }
                    element.write_canonical(output);
                }
                output.push(b']');
            }
            Self::Object(members) => {
                let mut sorted: Vec<&(Vec<u8>, Self)> = members.iter().collect();
                sorted.sort_by(|left, right| left.0.cmp(&right.0));
                output.push(b'{');
                for (index, (key, value)) in sorted.into_iter().enumerate() {
                    if index > 0 {
                        output.push(b',');
                    }
                    write_string(key, output);
                    output.push(b':');
                    value.write_canonical(output);
                }
                output.push(b'}');
            }
        }
    }

    /// # Returns
    ///
    /// The value of the first member named `key`, if this is an object that has one.
    #[must_use]
    pub fn member(&self, key: &[u8]) -> Option<&Self> {
        let Self::Object(members) = self else {
            return None;
        };
        members
            .iter()
            .find(|(member_key, _)| member_key == key)
            .map(|(_, value)| value)
    }

    /// # Returns
    ///
    /// The value at the literal path `path` (first members, object by object), or `None` if a key
    /// is missing or `path` has a wildcard.
    #[must_use]
    pub fn member_at(&self, path: &ColumnPattern) -> Option<&Self> {
        path.tokens
            .iter()
            .try_fold(self, |current, token| match token {
                PathToken::Key(key) => current.member(key),
                PathToken::AnyDepth => None,
            })
    }
}

/// Canonicalizes one JSON line.
///
/// # Returns
///
/// The canonical line, or `None` if `line` is not valid JSON.
#[must_use]
pub fn canonical_line(line: &[u8]) -> Option<Vec<u8>> {
    let value = JsonValue::parse(line)?;
    let mut output = Vec::with_capacity(line.len());
    value.write_canonical(&mut output);
    Some(output)
}

/// Canonicalizes one non-empty JSON line the way `canonical_digest` records it.
///
/// # Returns
///
/// The canonical line, or `INVALID:` followed by the raw line if it is not valid JSON.
#[must_use]
pub fn canonical_record(line: &[u8]) -> Vec<u8> {
    canonical_line(line).unwrap_or_else(|| [INVALID_PREFIX, line].concat())
}

/// Computes the canonical digest of JSON-lines bytes.
///
/// # Returns
///
/// The digest.
#[must_use]
pub fn canonical_digest(data: &[u8]) -> CanonicalDigest {
    CanonicalDigest::of_jsonl(data)
}

/// # Returns
///
/// The lowercase hex encoding of `bytes`.
#[must_use]
pub fn hex(bytes: &[u8]) -> String {
    const HEX_DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut text = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        text.push(char::from(HEX_DIGITS[usize::from(byte >> 4)]));
        text.push(char::from(HEX_DIGITS[usize::from(byte & 0x0f)]));
    }
    text
}

/// The recursion bound of the parser; deeper documents are reported as invalid.
const MAX_DEPTH: usize = 1000;

/// Canonical lines stored back to back, sorted once at the end.
#[derive(Default)]
struct CanonicalLines {
    buffer: Vec<u8>,
    ranges: Vec<std::ops::Range<usize>>,
    num_invalid: u64,
}

impl CanonicalLines {
    /// Canonicalizes `line` and stores it, unless it is empty.
    fn push(&mut self, line: &[u8]) {
        if line.is_empty() {
            return;
        }
        let start = self.buffer.len();
        if let Some(value) = JsonValue::parse(line) {
            value.write_canonical(&mut self.buffer);
        } else {
            self.num_invalid += 1;
            self.buffer.extend_from_slice(INVALID_PREFIX);
            self.buffer.extend_from_slice(line);
        }
        self.ranges.push(start..self.buffer.len());
    }

    /// # Returns
    ///
    /// The digest of the stored lines.
    fn finish(mut self) -> CanonicalDigest {
        let buffer = &self.buffer;
        self.ranges
            .sort_unstable_by(|left, right| buffer[left.clone()].cmp(&buffer[right.clone()]));
        let mut hasher = Sha256::new();
        for range in &self.ranges {
            hasher.update(&buffer[range.clone()]);
            hasher.update(b"\n");
        }
        CanonicalDigest {
            count: self.ranges.len() as u64,
            sha256: hex(&hasher.finalize()),
            num_invalid: self.num_invalid,
        }
    }
}

/// A recursive-descent port of the `_json` scanner of `CPython` (`strict=True`).
struct Parser<'input> {
    input: &'input [u8],
    position: usize,
    is_cpp_non_finite_null: bool,
}

impl<'input> Parser<'input> {
    /// Factory function.
    ///
    /// # Returns
    ///
    /// A parser at the start of `input`.
    const fn new(input: &'input [u8], is_cpp_non_finite_null: bool) -> Self {
        Self {
            input,
            position: 0,
            is_cpp_non_finite_null,
        }
    }

    /// # Returns
    ///
    /// The document's value, or `None` if it is invalid or has trailing non-whitespace.
    fn parse_document(&mut self) -> Option<JsonValue> {
        self.skip_whitespace();
        let value = self.parse_value(0)?;
        self.skip_whitespace();
        (self.position == self.input.len()).then_some(value)
    }

    /// # Returns
    ///
    /// The value starting at the current position, or `None` if it is invalid.
    fn parse_value(&mut self, depth: usize) -> Option<JsonValue> {
        if depth > MAX_DEPTH {
            return None;
        }
        match self.peek()? {
            b'"' => {
                self.position += 1;
                self.parse_string().map(JsonValue::String)
            }
            b'{' => self.parse_object(depth),
            b'[' => self.parse_array(depth),
            b'n' if self.consume_literal(b"null") => Some(JsonValue::Null),
            b't' if self.consume_literal(b"true") => Some(JsonValue::Boolean(true)),
            b'f' if self.consume_literal(b"false") => Some(JsonValue::Boolean(false)),
            b'N' if self.consume_literal(b"NaN") => Some(JsonValue::Number(b"NaN".to_vec())),
            b'I' if self.consume_literal(b"Infinity") => {
                Some(JsonValue::Number(b"Infinity".to_vec()))
            }
            b'-' if self.consume_literal(b"-Infinity") => {
                Some(JsonValue::Number(b"-Infinity".to_vec()))
            }
            b'-' | b'0'..=b'9' => {
                if let Some(value) = self.parse_number() {
                    return Some(value);
                }
                self.parse_cpp_non_finite()
            }
            _ => self.parse_cpp_non_finite(),
        }
    }

    /// # Returns
    ///
    /// `null` for a C++ non-finite token when they are accepted, else `None`.
    fn parse_cpp_non_finite(&mut self) -> Option<JsonValue> {
        if !self.is_cpp_non_finite_null {
            return None;
        }
        for token in [b"-nan".as_slice(), b"-inf", b"nan", b"inf"] {
            if self.consume_literal(token) {
                return Some(JsonValue::Null);
            }
        }
        None
    }

    /// # Returns
    ///
    /// The number token starting at the current position, or `None` (with the position
    /// unchanged) if there is none.
    fn parse_number(&mut self) -> Option<JsonValue> {
        let input = self.input;
        let start = self.position;
        let mut index = start;
        if input.get(index) == Some(&b'-') {
            index += 1;
        }
        match input.get(index)? {
            b'0' => index += 1,
            b'1'..=b'9' => {
                index += 1;
                index = skip_digits(input, index);
            }
            _ => return None,
        }
        if input.get(index) == Some(&b'.') && input.get(index + 1).is_some_and(u8::is_ascii_digit) {
            index = skip_digits(input, index + 2);
        }
        if matches!(input.get(index), Some(b'e' | b'E')) {
            let mut exponent_index = index + 1;
            if matches!(input.get(exponent_index), Some(b'+' | b'-')) {
                exponent_index += 1;
            }
            let exponent_end = skip_digits(input, exponent_index);
            if exponent_end > exponent_index {
                index = exponent_end;
            }
        }
        self.position = index;
        Some(JsonValue::Number(input[start..index].to_vec()))
    }

    /// Parses a string body; the opening quote is already consumed.
    ///
    /// # Returns
    ///
    /// The WTF-8 bytes of the decoded string, or `None` if it is invalid.
    fn parse_string(&mut self) -> Option<Vec<u8>> {
        let mut output = Vec::new();
        loop {
            let byte = self.peek()?;
            match byte {
                b'"' => {
                    self.position += 1;
                    return Some(output);
                }
                b'\\' => {
                    self.position += 1;
                    self.parse_escape(&mut output)?;
                }
                0x00..=0x1f => return None,
                0x20..=0x7f => {
                    output.push(byte);
                    self.position += 1;
                }
                _ => {
                    let remaining = &self.input[self.position..];
                    let valid_size = leading_char_size(remaining);
                    if valid_size == 0 {
                        push_code_point(0xdc00 + u32::from(byte), &mut output);
                        self.position += 1;
                    } else {
                        output.extend_from_slice(&remaining[..valid_size]);
                        self.position += valid_size;
                    }
                }
            }
        }
    }

    /// Parses one escape sequence; the backslash is already consumed.
    ///
    /// # Returns
    ///
    /// `Some(())` on success, `None` if the escape is invalid.
    fn parse_escape(&mut self, output: &mut Vec<u8>) -> Option<()> {
        let byte = self.peek()?;
        self.position += 1;
        let decoded = match byte {
            b'"' => b'"',
            b'\\' => b'\\',
            b'/' => b'/',
            b'b' => 0x08,
            b'f' => 0x0c,
            b'n' => b'\n',
            b'r' => b'\r',
            b't' => b'\t',
            b'u' => {
                let mut code_point = self.parse_hex4()?;
                if (0xd800..=0xdbff).contains(&code_point)
                    && self.input.get(self.position..self.position + 2) == Some(b"\\u".as_slice())
                {
                    let saved_position = self.position;
                    self.position += 2;
                    let low = self.parse_hex4()?;
                    if (0xdc00..=0xdfff).contains(&low) {
                        code_point = 0x10000 + ((code_point - 0xd800) << 10) + (low - 0xdc00);
                    } else {
                        self.position = saved_position;
                    }
                }
                push_code_point(code_point, output);
                return Some(());
            }
            _ => return None,
        };
        output.push(decoded);
        Some(())
    }

    /// # Returns
    ///
    /// The value of the four hex digits at the current position, or `None` if they are not
    /// four hex digits.
    fn parse_hex4(&mut self) -> Option<u32> {
        let digits = self.input.get(self.position..self.position + 4)?;
        let mut value = 0;
        for digit in digits {
            value = (value << 4) | char::from(*digit).to_digit(16)?;
        }
        self.position += 4;
        Some(value)
    }

    /// # Returns
    ///
    /// The object starting at the current `{`, or `None` if it is invalid.
    fn parse_object(&mut self, depth: usize) -> Option<JsonValue> {
        self.position += 1;
        let mut members = Vec::new();
        self.skip_whitespace();
        if self.peek() == Some(b'}') {
            self.position += 1;
            return Some(JsonValue::Object(members));
        }
        loop {
            if self.peek()? != b'"' {
                return None;
            }
            self.position += 1;
            let key = self.parse_string()?;
            self.skip_whitespace();
            if self.peek()? != b':' {
                return None;
            }
            self.position += 1;
            self.skip_whitespace();
            let value = self.parse_value(depth + 1)?;
            members.push((key, value));
            self.skip_whitespace();
            match self.peek()? {
                b'}' => {
                    self.position += 1;
                    return Some(JsonValue::Object(members));
                }
                b',' => {
                    self.position += 1;
                    self.skip_whitespace();
                }
                _ => return None,
            }
        }
    }

    /// # Returns
    ///
    /// The array starting at the current `[`, or `None` if it is invalid.
    fn parse_array(&mut self, depth: usize) -> Option<JsonValue> {
        self.position += 1;
        let mut elements = Vec::new();
        self.skip_whitespace();
        if self.peek() == Some(b']') {
            self.position += 1;
            return Some(JsonValue::Array(elements));
        }
        loop {
            elements.push(self.parse_value(depth + 1)?);
            self.skip_whitespace();
            match self.peek()? {
                b']' => {
                    self.position += 1;
                    return Some(JsonValue::Array(elements));
                }
                b',' => {
                    self.position += 1;
                    self.skip_whitespace();
                }
                _ => return None,
            }
        }
    }

    /// # Returns
    ///
    /// Whether `literal` is at the current position; if so, it is consumed.
    fn consume_literal(&mut self, literal: &[u8]) -> bool {
        if self.input[self.position..].starts_with(literal) {
            self.position += literal.len();
            return true;
        }
        false
    }

    /// Skips JSON whitespace (space, tab, newline, carriage return).
    fn skip_whitespace(&mut self) {
        while matches!(self.peek(), Some(b' ' | b'\t' | b'\n' | b'\r')) {
            self.position += 1;
        }
    }

    /// # Returns
    ///
    /// The byte at the current position, if any.
    fn peek(&self) -> Option<u8> {
        self.input.get(self.position).copied()
    }
}

/// # Returns
///
/// The index of the first non-digit byte at or after `index`.
fn skip_digits(input: &[u8], mut index: usize) -> usize {
    while input.get(index).is_some_and(u8::is_ascii_digit) {
        index += 1;
    }
    index
}

/// Appends the WTF-8 encoding of `code_point` (a scalar value or a surrogate).
fn push_code_point(code_point: u32, output: &mut Vec<u8>) {
    match code_point {
        0..=0x7f => output.push(low_byte(code_point)),
        0x80..=0x7ff => {
            output.push(0xc0 | low_byte(code_point >> 6));
            output.push(0x80 | low_byte(code_point & 0x3f));
        }
        0x800..=0xffff => {
            output.push(0xe0 | low_byte(code_point >> 12));
            output.push(0x80 | low_byte((code_point >> 6) & 0x3f));
            output.push(0x80 | low_byte(code_point & 0x3f));
        }
        _ => {
            output.push(0xf0 | low_byte(code_point >> 18));
            output.push(0x80 | low_byte((code_point >> 12) & 0x3f));
            output.push(0x80 | low_byte((code_point >> 6) & 0x3f));
            output.push(0x80 | low_byte(code_point & 0x3f));
        }
    }
}

/// # Returns
///
/// The least significant byte of `value`.
const fn low_byte(value: u32) -> u8 {
    value.to_le_bytes()[0]
}

/// # Returns
///
/// The size of the valid UTF-8 character at the start of `bytes`, or 0 if it does not start with
/// one.
fn leading_char_size(bytes: &[u8]) -> usize {
    let window = &bytes[..bytes.len().min(4)];
    let valid = match std::str::from_utf8(window) {
        Ok(text) => text,
        Err(error) => std::str::from_utf8(&window[..error.valid_up_to()]).unwrap_or_default(),
    };
    valid.chars().next().map_or(0, char::len_utf8)
}

/// Appends `text` (WTF-8) as Python's `json.dumps(text, ensure_ascii=False)` followed by
/// `.encode("utf-8", errors="surrogateescape")`: surrogate escapes become their original bytes.
fn write_string(text: &[u8], output: &mut Vec<u8>) {
    const HEX_DIGITS: &[u8; 16] = b"0123456789abcdef";
    output.push(b'"');
    let mut index = 0;
    while index < text.len() {
        let byte = text[index];
        match byte {
            b'"' => output.extend_from_slice(b"\\\""),
            b'\\' => output.extend_from_slice(b"\\\\"),
            b'\n' => output.extend_from_slice(b"\\n"),
            b'\r' => output.extend_from_slice(b"\\r"),
            b'\t' => output.extend_from_slice(b"\\t"),
            0x08 => output.extend_from_slice(b"\\b"),
            0x0c => output.extend_from_slice(b"\\f"),
            0x00..=0x1f => {
                output.extend_from_slice(b"\\u00");
                output.push(HEX_DIGITS[usize::from(byte >> 4)]);
                output.push(HEX_DIGITS[usize::from(byte & 0x0f)]);
            }
            0xed if text.get(index + 1).is_some_and(|next| *next >= 0xa0) => {
                let surrogate = &text[index..(index + 3).min(text.len())];
                let code_point = surrogate
                    .iter()
                    .skip(1)
                    .fold(0xd_u32, |value, unit| (value << 6) | u32::from(unit & 0x3f));
                if (0xdc80..=0xdcff).contains(&code_point) {
                    output.push(low_byte(code_point - 0xdc00));
                } else {
                    output.extend_from_slice(surrogate);
                }
                index += surrogate.len();
                continue;
            }
            _ => output.push(byte),
        }
        index += 1;
    }
    output.push(b'"');
}

#[cfg(test)]
mod tests {
    use super::CanonicalDigest;
    use super::JsonValue;
    use super::canonical_digest;
    use super::canonical_line;
    use super::canonical_record;

    /// Inputs and the outputs of `scripts/corpus.py`'s `canonical_line` (`None` for invalid JSON),
    /// generated with the corpus venv's Python.
    const PYTHON_VECTORS: &[(&[u8], Option<&[u8]>)] = &[
        (br#"{"b":1,"a":2}"#, Some(br#"{"a":2,"b":1}"#)),
        (
            b" {\"a\" : [1, 2.50, -0, 1E10, 1.5e+10, -1e-7] , \"b\":{}} \r",
            Some(br#"{"a":[1,2.50,-0,1E10,1.5e+10,-1e-7],"b":{}}"#),
        ),
        (br#"{"a":1,"a":0}"#, Some(br#"{"a":1,"a":0}"#)),
        (
            b"{\"z\":\"x\",\"\\u00e9\":1,\"\\ud83d\\ude00\":2,\"\xc3\xa9t\xc3\xa9\":3,\"A\":4}",
            Some(
                b"{\"A\":4,\"z\":\"x\",\"\xc3\xa9\":1,\"\xc3\xa9t\xc3\xa9\":3,\
                  \"\xf0\x9f\x98\x80\":2}",
            ),
        ),
        (
            br#"{"s":"\"\\\/\b\f\n\r\t\u0001\u001f\u007f\u2028/"}"#,
            Some(b"{\"s\":\"\\\"\\\\/\\b\\f\\n\\r\\t\\u0001\\u001f\x7f\xe2\x80\xa8/\"}"),
        ),
        (
            b"[\"\xff\xfe\", \"a\xe2\x82b\", \"\xed\xa0\x80\"]",
            Some(b"[\"\xff\xfe\",\"a\xe2\x82b\",\"\xed\xa0\x80\"]"),
        ),
        (
            b"{\"\xff\":1,\"\xc3\xa9\":2,\"z\":3}",
            Some(b"{\"z\":3,\"\xc3\xa9\":2,\"\xff\":1}"),
        ),
        (
            br#"["\udc80", "\uDBFF\uDFFF", "\udcff\u0041"]"#,
            Some(b"[\"\x80\",\"\xf4\x8f\xbf\xbf\",\"\xffA\"]"),
        ),
        (
            br#"{"\udc80":1,"\u00e9":2}"#,
            Some(b"{\"\xc3\xa9\":2,\"\x80\":1}"),
        ),
        (
            br#"{"b":[{"d":1,"c":2}],"a":"\u0000"}"#,
            Some(br#"{"a":"\u0000","b":[{"c":2,"d":1}]}"#),
        ),
        (
            b"[NaN, Infinity, -Infinity]",
            Some(b"[NaN,Infinity,-Infinity]"),
        ),
        (b"[nan]", None),
        (b"01", None),
        (b"[1.]", None),
        (b"[1e]", None),
        (br#"{"a":1,}"#, None),
        (b"[1,]", None),
        (b"\"tab\there\"", None),
        (b"", None),
        (b"   ", None),
        (b"null", Some(b"null")),
        (
            br#"{"a":true,"b":false,"c":null}"#,
            Some(br#"{"a":true,"b":false,"c":null}"#),
        ),
        (b"\xef\xbb\xbf{}", None),
        (b"[-]", None),
        (b"[1e+]", None),
        (br#"{"k":"\u12"}"#, None),
        (
            b"[0.5e-3, -0.0, 123456789012345678901234567890]",
            Some(b"[0.5e-3,-0.0,123456789012345678901234567890]"),
        ),
        (
            br#"{"x" :1 , "y":[ ] , "z" : { } }"#,
            Some(br#"{"x":1,"y":[],"z":{}}"#),
        ),
        (b"[1 2]", None),
        (br#""\x""#, None),
    ];

    #[test]
    fn canonical_line_matches_corpus_py() {
        for (input, expected) in PYTHON_VECTORS {
            assert_eq!(
                canonical_line(input).as_deref(),
                *expected,
                "input {:?}",
                String::from_utf8_lossy(input)
            );
        }
    }

    #[test]
    fn canonical_digest_matches_corpus_py() {
        let digest = canonical_digest(b"{\"b\":1,\"a\":2}\n\n[1]\nnot json\n{\"a\":2,\"b\":1}\n");
        assert_eq!(
            digest,
            CanonicalDigest {
                count: 4,
                sha256: "bfee61e0cab51314f71a43ad07d239c1e222107822f2efa33dd14801b43f9b47"
                    .to_owned(),
                num_invalid: 1,
            }
        );
    }

    #[test]
    fn invalid_lines_keep_their_raw_bytes() {
        assert_eq!(
            canonical_record(b"not \xff json"),
            b"INVALID:not \xff json".to_vec()
        );
        assert_eq!(
            canonical_record(br#"{"b":1,"a":2}"#),
            br#"{"a":2,"b":1}"#.to_vec()
        );
    }

    #[test]
    fn file_digest_equals_in_memory_digest() -> anyhow::Result<()> {
        let data = b"{\"b\":1}\n[2]\n\n{\"a\":\"\\u00e9\"}\nnot json\n[2]";
        let file = tempfile::NamedTempFile::new()?;
        std::fs::write(file.path(), data)?;
        assert_eq!(
            CanonicalDigest::of_jsonl_file(file.path())?,
            canonical_digest(data)
        );
        Ok(())
    }

    #[test]
    fn cpp_non_finite_tokens_parse_as_null_only_when_asked() {
        let text = b"[nan, -nan, inf, -inf, 1.5, null]";
        assert_eq!(JsonValue::parse(text), None);
        assert_eq!(
            JsonValue::parse_with_cpp_non_finite(text),
            Some(JsonValue::Array(vec![
                JsonValue::Null,
                JsonValue::Null,
                JsonValue::Null,
                JsonValue::Null,
                JsonValue::Number(b"1.5".to_vec()),
                JsonValue::Null,
            ]))
        );
        assert_eq!(JsonValue::parse_with_cpp_non_finite(b"[nanx]"), None);
    }

    #[test]
    fn nesting_beyond_the_depth_bound_is_invalid_without_overflowing() {
        let deep = |depth: usize| [vec![b'['; depth], vec![b']'; depth]].concat();
        assert!(JsonValue::parse(&deep(500)).is_some());
        assert_eq!(JsonValue::parse(&deep(100_000)), None);
    }

    #[test]
    fn member_finds_the_first_member_with_the_key() {
        let value = JsonValue::parse(br#"{"a":1,"b":"x","a":2}"#).expect("valid JSON");
        assert_eq!(value.member(b"a"), Some(&JsonValue::Number(b"1".to_vec())));
        assert_eq!(value.member(b"c"), None);
        assert_eq!(JsonValue::Null.member(b"a"), None);
    }

    #[test]
    fn hex_is_lowercase() {
        assert_eq!(super::hex(&[0x00, 0xab, 0x0f]), "00ab0f");
    }
}
