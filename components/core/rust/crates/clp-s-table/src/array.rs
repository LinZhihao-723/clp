//! The unstructured-array element scanner: walks the decoded JSON text of an unstructured array,
//! shared by the search kernels and future record views.

use std::borrow::Cow;

use crate::TableError;

/// One structural event or scalar of a decoded unstructured array.
#[derive(Clone, Debug, PartialEq)]
pub enum ArrayEvent<'text> {
    BeginArray,
    EndArray,
    BeginObject,
    EndObject,

    /// The key of the next value inside the current object (unescaped).
    Key(Cow<'text, [u8]>),

    Scalar(ArrayScalar<'text>),
}

/// A scalar element of a decoded unstructured array, typed as the C++ JSON parser types it.
#[derive(Clone, Debug, PartialEq)]
pub enum ArrayScalar<'text> {
    Null,
    Boolean(bool),
    Integer(i64),
    UnsignedInteger(u64),
    Float(f64),

    /// A string value (unescaped).
    String(Cow<'text, [u8]>),
}

/// Walks the JSON array `text`, calling `visit` with each event in document order; `visit`
/// returns whether to continue.
///
/// Numbers are typed as simdjson types them: an integer token is an [`ArrayScalar::Integer`] if it
/// fits `i64` (`-0` included), else an [`ArrayScalar::UnsignedInteger`] if it fits `u64`; every
/// other number is an [`ArrayScalar::Float`] (integers beyond `u64`, which simdjson rejects, are
/// read as floats). Unpaired UTF-16 surrogate escapes decode to U+FFFD.
///
/// # Type Parameters
///
/// * `VisitorType` - Receives each event; returns whether to continue the walk.
///
/// # Errors
///
/// Returns an error if:
///
/// * [`TableError::InvalidArray`] if `text` is not a valid JSON array (an error found after `visit`
///   stopped the walk is not reported).
pub fn scan_array<VisitorType: FnMut(ArrayEvent<'_>) -> bool>(
    text: &[u8],
    mut visit: VisitorType,
) -> Result<(), TableError> {
    let mut scanner = Scanner {
        parser: Parser { text, position: 0 },
        stack: Vec::new(),
    };
    scanner.parser.skip_whitespace();
    if Some(b'[') != scanner.parser.peek() {
        return Err(invalid("the text does not start with an array"));
    }
    let mut state = State::Value;
    loop {
        scanner.parser.skip_whitespace();
        let (event, next_state) = match state {
            State::Value => {
                let (event, next_state) = scanner.value()?;
                (Some(event), next_state)
            }
            State::Open => (None, scanner.open()),
            State::Key => (Some(scanner.key()?), State::Value),
            State::AfterValue => match scanner.after_value()? {
                Some(next_state) => (None, next_state),
                None => return Ok(()),
            },
            State::Close => (Some(scanner.close()?), State::AfterValue),
        };
        if let Some(event) = event
            && !visit(event)
        {
            return Ok(());
        }
        state = next_state;
    }
}

/// The state of one [`scan_array`] walk: the cursor and the open containers.
struct Scanner<'text> {
    parser: Parser<'text>,
    stack: Vec<Container>,
}

impl<'text> Scanner<'text> {
    /// Consumes one value, opening a container if it is one.
    ///
    /// # Returns
    ///
    /// A tuple on success:
    ///
    /// * The value's event.
    /// * The state after it.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * Forwards [`Parser::scalar`]'s return values on failure.
    fn value(&mut self) -> Result<(ArrayEvent<'text>, State), TableError> {
        let (event, container) = match self.parser.peek() {
            Some(b'[') => (ArrayEvent::BeginArray, Container::Array),
            Some(b'{') => (ArrayEvent::BeginObject, Container::Object),
            _ => return Ok((ArrayEvent::Scalar(self.parser.scalar()?), State::AfterValue)),
        };
        self.parser.position += 1;
        self.stack.push(container);
        Ok((event, State::Open))
    }

    /// # Returns
    ///
    /// The state after the opening bracket of the current container.
    fn open(&self) -> State {
        match (self.stack.last(), self.parser.peek()) {
            (Some(container), Some(byte)) if container.closing_bracket() == byte => State::Close,
            (Some(Container::Array), _) => State::Value,
            _ => State::Key,
        }
    }

    /// Consumes one object key and its colon.
    ///
    /// # Returns
    ///
    /// The key's event on success.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * [`TableError::InvalidArray`] if the key is not a string followed by a colon.
    /// * Forwards [`Parser::string`]'s return values on failure.
    fn key(&mut self) -> Result<ArrayEvent<'text>, TableError> {
        if Some(b'"') != self.parser.peek() {
            return Err(invalid("an object key is not a string"));
        }
        let key = self.parser.string()?;
        self.parser.skip_whitespace();
        if Some(b':') != self.parser.peek() {
            return Err(invalid("an object key is not followed by a colon"));
        }
        self.parser.position += 1;
        Ok(ArrayEvent::Key(key))
    }

    /// Consumes the separator after a value, if any.
    ///
    /// # Returns
    ///
    /// The next state, or `None` once the outermost array is closed and only whitespace follows,
    /// on success.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * [`TableError::InvalidArray`] if neither a separator nor the current container's closing
    ///   bracket follows, or bytes follow the outermost array.
    fn after_value(&mut self) -> Result<Option<State>, TableError> {
        let Some(container) = self.stack.last().copied() else {
            return match self.parser.peek() {
                None => Ok(None),
                Some(_) => Err(invalid("bytes follow the array")),
            };
        };
        match self.parser.peek() {
            Some(b',') => {
                self.parser.position += 1;
                Ok(Some(match container {
                    Container::Array => State::Value,
                    Container::Object => State::Key,
                }))
            }
            Some(byte) if container.closing_bracket() == byte => Ok(Some(State::Close)),
            _ => Err(invalid("a value is not followed by a separator")),
        }
    }

    /// Consumes the closing bracket of the current container.
    ///
    /// # Returns
    ///
    /// The container's end event on success.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * [`TableError::InvalidArray`] if no container is open.
    fn close(&mut self) -> Result<ArrayEvent<'text>, TableError> {
        self.parser.position += 1;
        match self.stack.pop() {
            Some(Container::Array) => Ok(ArrayEvent::EndArray),
            Some(Container::Object) => Ok(ArrayEvent::EndObject),
            None => Err(invalid("unbalanced brackets")),
        }
    }
}

/// The kind of an open container.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Container {
    Array,
    Object,
}

impl Container {
    /// # Returns
    ///
    /// The byte that closes the container.
    const fn closing_bracket(self) -> u8 {
        match self {
            Self::Array => b']',
            Self::Object => b'}',
        }
    }
}

/// What the scanner expects next.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum State {
    /// A value.
    Value,

    /// The first member of a container that was just opened, or its end.
    Open,

    /// An object key and its colon.
    Key,

    /// A separator or the end of the current container.
    AfterValue,

    /// The closing bracket of the current container.
    Close,
}

/// A cursor over JSON text.
struct Parser<'text> {
    text: &'text [u8],
    position: usize,
}

impl<'text> Parser<'text> {
    /// # Returns
    ///
    /// The next byte, or `None` at the end of the text.
    fn peek(&self) -> Option<u8> {
        self.text.get(self.position).copied()
    }

    /// Skips JSON whitespace.
    fn skip_whitespace(&mut self) {
        while matches!(self.peek(), Some(b' ' | b'\t' | b'\n' | b'\r')) {
            self.position += 1;
        }
    }

    /// Consumes `literal` if the text continues with it.
    ///
    /// # Returns
    ///
    /// Whether `literal` was consumed.
    fn consume(&mut self, literal: &[u8]) -> bool {
        let is_match = self.text[self.position..].starts_with(literal);
        if is_match {
            self.position += literal.len();
        }
        is_match
    }

    /// Consumes one scalar.
    ///
    /// # Returns
    ///
    /// The scalar on success.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * [`TableError::InvalidArray`] if the text does not continue with a scalar.
    /// * Forwards [`Self::string`]'s return values on failure.
    /// * Forwards [`Self::number`]'s return values on failure.
    fn scalar(&mut self) -> Result<ArrayScalar<'text>, TableError> {
        match self.peek() {
            Some(b'"') => Ok(ArrayScalar::String(self.string()?)),
            Some(b'-' | b'0'..=b'9') => self.number(),
            _ if self.consume(b"true") => Ok(ArrayScalar::Boolean(true)),
            _ if self.consume(b"false") => Ok(ArrayScalar::Boolean(false)),
            _ if self.consume(b"null") => Ok(ArrayScalar::Null),
            _ => Err(invalid("a value is missing or malformed")),
        }
    }

    /// Consumes one number (`-?(0|[1-9][0-9]*)(\.[0-9]+)?([eE][+-]?[0-9]+)?`).
    ///
    /// # Returns
    ///
    /// The typed number on success.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * [`TableError::InvalidArray`] if the number is malformed.
    fn number(&mut self) -> Result<ArrayScalar<'text>, TableError> {
        let malformed = || invalid("a number is malformed");
        let begin = self.position;
        let is_negative = self.consume(b"-");
        match self.peek() {
            Some(b'0') => self.position += 1,
            Some(b'1'..=b'9') => {
                self.skip_digits();
            }
            _ => return Err(malformed()),
        }
        let mut is_integer = true;
        if self.consume(b".") {
            is_integer = false;
            if 0 == self.skip_digits() {
                return Err(malformed());
            }
        }
        if matches!(self.peek(), Some(b'e' | b'E')) {
            is_integer = false;
            self.position += 1;
            if matches!(self.peek(), Some(b'+' | b'-')) {
                self.position += 1;
            }
            if 0 == self.skip_digits() {
                return Err(malformed());
            }
        }
        let token =
            std::str::from_utf8(&self.text[begin..self.position]).map_err(|_| malformed())?;
        if is_integer {
            if let Ok(value) = token.parse::<i64>() {
                return Ok(ArrayScalar::Integer(value));
            }
            if !is_negative && let Ok(value) = token.parse::<u64>() {
                return Ok(ArrayScalar::UnsignedInteger(value));
            }
        }
        token
            .parse::<f64>()
            .map(ArrayScalar::Float)
            .map_err(|_| malformed())
    }

    /// Consumes a run of ASCII digits.
    ///
    /// # Returns
    ///
    /// The number of digits consumed.
    fn skip_digits(&mut self) -> usize {
        let begin = self.position;
        while matches!(self.peek(), Some(b'0'..=b'9')) {
            self.position += 1;
        }
        self.position - begin
    }

    /// Consumes one string, starting at its opening quote.
    ///
    /// # Returns
    ///
    /// The unescaped string (borrowed if it has no escape) on success.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * [`TableError::InvalidArray`] if the string is unterminated, holds a control character or
    ///   has a malformed escape.
    fn string(&mut self) -> Result<Cow<'text, [u8]>, TableError> {
        self.position += 1;
        let mut owned: Option<Vec<u8>> = None;
        loop {
            let rest = &self.text[self.position..];
            let Some(offset) = memchr::memchr2(b'"', b'\\', rest) else {
                return Err(invalid("a string is unterminated"));
            };
            let chunk = &rest[..offset];
            if chunk.iter().any(|byte| *byte < 0x20) {
                return Err(invalid("a string holds a control character"));
            }
            self.position += offset + 1;
            if b'"' == rest[offset] {
                return Ok(owned.map_or(Cow::Borrowed(chunk), |mut owned| {
                    owned.extend_from_slice(chunk);
                    Cow::Owned(owned)
                }));
            }
            let owned = owned.get_or_insert_with(Vec::new);
            owned.extend_from_slice(chunk);
            self.escape(owned)?;
        }
    }

    /// Consumes the escape after a backslash and appends its UTF-8 bytes to `output`.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * [`TableError::InvalidArray`] if the escape is malformed.
    /// * Forwards [`Self::hex_code_unit`]'s return values on failure.
    fn escape(&mut self, output: &mut Vec<u8>) -> Result<(), TableError> {
        let Some(byte) = self.peek() else {
            return Err(invalid("a string escape is unterminated"));
        };
        self.position += 1;
        let unescaped = match byte {
            b'"' => b'"',
            b'\\' => b'\\',
            b'/' => b'/',
            b'b' => 0x08,
            b'f' => 0x0c,
            b'n' => b'\n',
            b'r' => b'\r',
            b't' => b'\t',
            b'u' => {
                let code_unit = self.hex_code_unit()?;
                let code_point = if (0xd800..0xdc00).contains(&code_unit) {
                    let low_begin = self.position;
                    if self.consume(b"\\u") {
                        let low = self.hex_code_unit()?;
                        if (0xdc00..0xe000).contains(&low) {
                            0x10000 + ((code_unit - 0xd800) << 10) + (low - 0xdc00)
                        } else {
                            self.position = low_begin;
                            char::REPLACEMENT_CHARACTER.into()
                        }
                    } else {
                        char::REPLACEMENT_CHARACTER.into()
                    }
                } else {
                    code_unit
                };
                let character = char::from_u32(code_point).unwrap_or(char::REPLACEMENT_CHARACTER);
                let mut buffer = [0; 4];
                output.extend_from_slice(character.encode_utf8(&mut buffer).as_bytes());
                return Ok(());
            }
            _ => return Err(invalid("a string escape is malformed")),
        };
        output.push(unescaped);
        Ok(())
    }

    /// Consumes four hex digits.
    ///
    /// # Returns
    ///
    /// The UTF-16 code unit they spell on success.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * [`TableError::InvalidArray`] if fewer than four hex digits follow.
    fn hex_code_unit(&mut self) -> Result<u32, TableError> {
        let digits = self
            .text
            .get(self.position..self.position + 4)
            .ok_or_else(|| invalid("a unicode escape is truncated"))?;
        let mut code_unit = 0;
        for digit in digits {
            let value = char::from(*digit)
                .to_digit(16)
                .ok_or_else(|| invalid("a unicode escape is malformed"))?;
            code_unit = code_unit * 16 + value;
        }
        self.position += 4;
        Ok(code_unit)
    }
}

/// # Returns
///
/// A [`TableError::InvalidArray`] for `reason`.
const fn invalid(reason: &'static str) -> TableError {
    TableError::InvalidArray { reason }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// # Returns
    ///
    /// `event` with its key or string owned.
    fn owned(event: ArrayEvent<'_>) -> ArrayEvent<'static> {
        let owned = |text: Cow<'_, [u8]>| Cow::Owned(text.into_owned());
        match event {
            ArrayEvent::BeginArray => ArrayEvent::BeginArray,
            ArrayEvent::EndArray => ArrayEvent::EndArray,
            ArrayEvent::BeginObject => ArrayEvent::BeginObject,
            ArrayEvent::EndObject => ArrayEvent::EndObject,
            ArrayEvent::Key(key) => ArrayEvent::Key(owned(key)),
            ArrayEvent::Scalar(scalar) => ArrayEvent::Scalar(match scalar {
                ArrayScalar::Null => ArrayScalar::Null,
                ArrayScalar::Boolean(value) => ArrayScalar::Boolean(value),
                ArrayScalar::Integer(value) => ArrayScalar::Integer(value),
                ArrayScalar::UnsignedInteger(value) => ArrayScalar::UnsignedInteger(value),
                ArrayScalar::Float(value) => ArrayScalar::Float(value),
                ArrayScalar::String(value) => ArrayScalar::String(owned(value)),
            }),
        }
    }

    /// # Returns
    ///
    /// Every event of `text` on success.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * Forwards [`scan_array`]'s return values on failure.
    fn events(text: &[u8]) -> Result<Vec<ArrayEvent<'static>>, TableError> {
        let mut events = Vec::new();
        scan_array(text, |event| {
            events.push(owned(event));
            true
        })?;
        Ok(events)
    }

    /// # Returns
    ///
    /// The scalars of `text`, which must scan.
    fn scalars(text: &[u8]) -> Vec<ArrayScalar<'static>> {
        events(text)
            .unwrap_or_else(|error| panic!("{text:?} scans: {error}"))
            .into_iter()
            .filter_map(|event| match event {
                ArrayEvent::Scalar(scalar) => Some(scalar),
                _ => None,
            })
            .collect()
    }

    /// # Returns
    ///
    /// The string scalar of the one-element array `text`.
    fn string(text: &[u8]) -> Vec<u8> {
        match scalars(text).as_slice() {
            [ArrayScalar::String(value)] => value.to_vec(),
            other => panic!("{text:?} holds one string: {other:?}"),
        }
    }

    #[test]
    fn events_follow_document_order() -> anyhow::Result<()> {
        use ArrayEvent::*;
        assert_eq!(
            events(br#" [1, {"a": [true, null], "b": {}}, [], "x"] "#)?,
            vec![
                BeginArray,
                Scalar(ArrayScalar::Integer(1)),
                BeginObject,
                Key(Cow::Borrowed(b"a")),
                BeginArray,
                Scalar(ArrayScalar::Boolean(true)),
                Scalar(ArrayScalar::Null),
                EndArray,
                Key(Cow::Borrowed(b"b")),
                BeginObject,
                EndObject,
                EndObject,
                BeginArray,
                EndArray,
                Scalar(ArrayScalar::String(Cow::Borrowed(b"x"))),
                EndArray,
            ]
        );
        assert_eq!(events(b"[]")?, vec![BeginArray, EndArray]);
        assert_eq!(
            events(b"\t[\n false ,\r\n0 ]\n")?,
            vec![
                BeginArray,
                Scalar(ArrayScalar::Boolean(false)),
                Scalar(ArrayScalar::Integer(0)),
                EndArray,
            ]
        );
        Ok(())
    }

    #[test]
    fn numbers_are_typed_like_simdjson() {
        assert_eq!(
            scalars(
                b"[0, -0, 9223372036854775807, -9223372036854775808, 9223372036854775808, \
                  18446744073709551615, 18446744073709551616, -9223372036854775809, 1.5, -2e3, \
                  1E-2, 0.0, -0.0]"
            ),
            vec![
                ArrayScalar::Integer(0),
                ArrayScalar::Integer(0),
                ArrayScalar::Integer(i64::MAX),
                ArrayScalar::Integer(i64::MIN),
                ArrayScalar::UnsignedInteger(1 << 63),
                ArrayScalar::UnsignedInteger(u64::MAX),
                ArrayScalar::Float(18_446_744_073_709_551_616.0),
                ArrayScalar::Float(-9_223_372_036_854_775_809.0),
                ArrayScalar::Float(1.5),
                ArrayScalar::Float(-2000.0),
                ArrayScalar::Float(0.01),
                ArrayScalar::Float(0.0),
                ArrayScalar::Float(-0.0),
            ]
        );
    }

    #[test]
    fn strings_are_unescaped() {
        assert_eq!(string(br#"["plain"]"#), b"plain");
        assert_eq!(string(br#"["\"\\\/\b\f\n\r\t"]"#), b"\"\\/\x08\x0c\n\r\t");
        assert_eq!(string(br#"["\u0041\u00e9\u4e2d"]"#), "Aé中".as_bytes());
        assert_eq!(string(br#"["\ud83d\ude00"]"#), "😀".as_bytes());
        assert_eq!(string("[\"raw é\"]".as_bytes()), "raw é".as_bytes());
    }

    #[test]
    fn unpaired_surrogates_become_replacement_characters() {
        assert_eq!(string(br#"["a\ud800b"]"#), "a\u{fffd}b".as_bytes());
        assert_eq!(string(br#"["\udc00"]"#), "\u{fffd}".as_bytes());
        assert_eq!(
            string(br#"["\ud800\u0041"]"#),
            "\u{fffd}A".as_bytes(),
            "a high surrogate followed by a non-surrogate escape keeps the escape"
        );
    }

    #[test]
    fn strings_without_escapes_are_borrowed() -> anyhow::Result<()> {
        let mut kinds = Vec::new();
        scan_array(br#"["a", "b\n", {"k": 1, "k\t": 2}]"#, |event| {
            match event {
                ArrayEvent::Key(key) | ArrayEvent::Scalar(ArrayScalar::String(key)) => {
                    kinds.push(matches!(key, Cow::Borrowed(_)));
                }
                _ => {}
            }
            true
        })?;
        assert_eq!(kinds, vec![true, false, true, false]);
        Ok(())
    }

    #[test]
    fn visitors_can_stop_the_walk() -> anyhow::Result<()> {
        let mut num_events = 0;
        scan_array(b"[1, 2, 3, oops", |_| {
            num_events += 1;
            num_events < 3
        })?;
        assert_eq!(num_events, 3);
        Ok(())
    }

    #[test]
    fn malformed_arrays_are_rejected() {
        for text in [
            &b""[..],
            b"{}",
            b"1",
            b"[",
            b"[1",
            b"[1,]",
            b"[,1]",
            b"[1 2]",
            b"[1] 2",
            b"[1]]",
            b"[}",
            b"[01]",
            b"[1.]",
            b"[.5]",
            b"[-]",
            b"[1e]",
            b"[+1]",
            b"[tru]",
            b"[\"a]",
            b"[\"a\x01\"]",
            b"[\"\\x\"]",
            b"[\"\\u12\"]",
            b"[\"\\u12g4\"]",
            b"[{1: 2}]",
            b"[{\"a\" 2}]",
            b"[{\"a\": 2,}]",
        ] {
            let result = events(text);
            assert!(
                matches!(result, Err(TableError::InvalidArray { .. })),
                "{:?}: {result:?}",
                String::from_utf8_lossy(text)
            );
        }
    }
}
