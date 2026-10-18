//! A test-only JSON tree for comparing records with the C++ oracles: strings are raw bytes,
//! integer tokens are kept verbatim and float tokens are normalized through `f64`, so values
//! compare equal exactly when C++ and Rust print the same number.

use std::fmt::Write;

use anyhow::Context;

/// A parsed JSON value; objects keep their keys in document order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Json {
    Null,
    Bool(bool),

    /// A number token, normalized with [`Json::number`].
    Number(String),

    String(Vec<u8>),
    Array(Vec<Self>),
    Object(Vec<(Vec<u8>, Self)>),
}

impl Json {
    /// Factory function.
    ///
    /// # Returns
    ///
    /// The number `token`: integer tokens verbatim, every other token as the `Debug` form of the
    /// nearest `f64`.
    ///
    /// # Panics
    ///
    /// Panics if `token` is not a number.
    pub fn number(token: &str) -> Self {
        if token.bytes().any(|byte| matches!(byte, b'.' | b'e' | b'E')) {
            let value: f64 = token
                .parse()
                .unwrap_or_else(|error| panic!("`{token}` is a float: {error}"));
            return Self::Number(format!("{value:?}"));
        }
        Self::Number(token.to_owned())
    }

    /// Factory function.
    ///
    /// # Returns
    ///
    /// The value of the JSON text `text` on success.
    ///
    /// # Errors
    ///
    /// Returns an error if `text` is not one JSON value.
    pub fn parse(text: &[u8]) -> anyhow::Result<Self> {
        let mut parser = Parser { text, position: 0 };
        let value = parser.value()?;
        parser.skip_whitespace();
        anyhow::ensure!(
            parser.position == text.len(),
            "trailing bytes at {}",
            parser.position
        );
        Ok(value)
    }

    /// # Returns
    ///
    /// The value serialized without whitespace, keys in their stored order and strings with every
    /// non-printable byte escaped.
    pub fn serialize(&self) -> String {
        let mut output = String::new();
        self.write(&mut output);
        output
    }

    /// Appends the serialized value to `output`.
    fn write(&self, output: &mut String) {
        match self {
            Self::Null => output.push_str("null"),
            Self::Bool(value) => output.push_str(if *value { "true" } else { "false" }),
            Self::Number(token) => output.push_str(token),
            Self::String(bytes) => write_string(bytes, output),
            Self::Array(values) => {
                output.push('[');
                for (index, value) in values.iter().enumerate() {
                    if 0 != index {
                        output.push(',');
                    }
                    value.write(output);
                }
                output.push(']');
            }
            Self::Object(members) => {
                output.push('{');
                for (index, (key, value)) in members.iter().enumerate() {
                    if 0 != index {
                        output.push(',');
                    }
                    write_string(key, output);
                    output.push(':');
                    value.write(output);
                }
                output.push('}');
            }
        }
    }
}

/// Appends `bytes` as a quoted string, escaping quotes, backslashes and every byte outside
/// printable ASCII as `\xNN`.
fn write_string(bytes: &[u8], output: &mut String) {
    output.push('"');
    for byte in bytes {
        match byte {
            b'"' => output.push_str("\\\""),
            b'\\' => output.push_str("\\\\"),
            0x20..=0x7e => output.push(char::from(*byte)),
            _ => {
                let _ = write!(output, "\\x{byte:02x}");
            }
        }
    }
    output.push('"');
}

/// A recursive-descent JSON parser over bytes.
struct Parser<'text> {
    text: &'text [u8],
    position: usize,
}

impl Parser<'_> {
    /// # Returns
    ///
    /// The next byte, or `None` at the end.
    fn peek(&self) -> Option<u8> {
        self.text.get(self.position).copied()
    }

    /// Skips whitespace.
    fn skip_whitespace(&mut self) {
        while matches!(self.peek(), Some(b' ' | b'\t' | b'\n' | b'\r')) {
            self.position += 1;
        }
    }

    /// Consumes `byte` after optional whitespace.
    ///
    /// # Errors
    ///
    /// Returns an error if the next byte is not `byte`.
    fn expect(&mut self, byte: u8) -> anyhow::Result<()> {
        self.skip_whitespace();
        anyhow::ensure!(
            Some(byte) == self.peek(),
            "expected `{}` at {}",
            char::from(byte),
            self.position
        );
        self.position += 1;
        Ok(())
    }

    /// # Returns
    ///
    /// The next value on success.
    ///
    /// # Errors
    ///
    /// Returns an error if the text does not continue with a value.
    fn value(&mut self) -> anyhow::Result<Json> {
        self.skip_whitespace();
        match self.peek().context("unexpected end")? {
            b'{' => {
                self.position += 1;
                let mut members = Vec::new();
                self.skip_whitespace();
                if Some(b'}') == self.peek() {
                    self.position += 1;
                    return Ok(Json::Object(members));
                }
                loop {
                    self.skip_whitespace();
                    let key = self.string()?;
                    self.expect(b':')?;
                    members.push((key, self.value()?));
                    self.skip_whitespace();
                    match self.peek() {
                        Some(b',') => self.position += 1,
                        Some(b'}') => {
                            self.position += 1;
                            return Ok(Json::Object(members));
                        }
                        _ => anyhow::bail!("bad object at {}", self.position),
                    }
                }
            }
            b'[' => {
                self.position += 1;
                let mut values = Vec::new();
                self.skip_whitespace();
                if Some(b']') == self.peek() {
                    self.position += 1;
                    return Ok(Json::Array(values));
                }
                loop {
                    values.push(self.value()?);
                    self.skip_whitespace();
                    match self.peek() {
                        Some(b',') => self.position += 1,
                        Some(b']') => {
                            self.position += 1;
                            return Ok(Json::Array(values));
                        }
                        _ => anyhow::bail!("bad array at {}", self.position),
                    }
                }
            }
            b'"' => Ok(Json::String(self.string()?)),
            b't' => self.literal(b"true", Json::Bool(true)),
            b'f' => self.literal(b"false", Json::Bool(false)),
            b'n' => self.literal(b"null", Json::Null),
            _ => {
                let begin = self.position;
                while matches!(
                    self.peek(),
                    Some(b'-' | b'+' | b'.' | b'e' | b'E' | b'0'..=b'9')
                ) {
                    self.position += 1;
                }
                anyhow::ensure!(begin != self.position, "bad value at {begin}");
                Ok(Json::number(std::str::from_utf8(
                    &self.text[begin..self.position],
                )?))
            }
        }
    }

    /// Consumes `literal`.
    ///
    /// # Returns
    ///
    /// `value` on success.
    ///
    /// # Errors
    ///
    /// Returns an error if the text does not continue with `literal`.
    fn literal(&mut self, literal: &[u8], value: Json) -> anyhow::Result<Json> {
        anyhow::ensure!(
            self.text[self.position..].starts_with(literal),
            "bad literal at {}",
            self.position
        );
        self.position += literal.len();
        Ok(value)
    }

    /// # Returns
    ///
    /// The unescaped bytes of the string at the cursor on success (unpaired surrogates as WTF-8).
    ///
    /// # Errors
    ///
    /// Returns an error if the text does not continue with a string.
    fn string(&mut self) -> anyhow::Result<Vec<u8>> {
        anyhow::ensure!(
            Some(b'"') == self.peek(),
            "expected a string at {}",
            self.position
        );
        self.position += 1;
        let mut output = Vec::new();
        loop {
            let byte = self.peek().context("unterminated string")?;
            self.position += 1;
            match byte {
                b'"' => return Ok(output),
                b'\\' => {
                    let escape = self.peek().context("unterminated escape")?;
                    self.position += 1;
                    match escape {
                        b'u' => {
                            let mut code_point = self.hex()?;
                            if (0xd800..0xdc00).contains(&code_point)
                                && self.text[self.position..].starts_with(b"\\u")
                            {
                                let saved = self.position;
                                self.position += 2;
                                let low = self.hex()?;
                                if (0xdc00..0xe000).contains(&low) {
                                    code_point =
                                        0x10000 + ((code_point - 0xd800) << 10) + (low - 0xdc00);
                                } else {
                                    self.position = saved;
                                }
                            }
                            push_wtf8(code_point, &mut output);
                        }
                        b'b' => output.push(0x08),
                        b'f' => output.push(0x0c),
                        b'n' => output.push(b'\n'),
                        b'r' => output.push(b'\r'),
                        b't' => output.push(b'\t'),
                        other => output.push(other),
                    }
                }
                other => output.push(other),
            }
        }
    }

    /// # Returns
    ///
    /// The four hex digits at the cursor on success.
    ///
    /// # Errors
    ///
    /// Returns an error if they are not four hex digits.
    fn hex(&mut self) -> anyhow::Result<u32> {
        let digits = self
            .text
            .get(self.position..self.position + 4)
            .context("short unicode escape")?;
        self.position += 4;
        Ok(u32::from_str_radix(std::str::from_utf8(digits)?, 16)?)
    }
}

/// Appends `code_point` encoded as (generalized) UTF-8.
fn push_wtf8(code_point: u32, output: &mut Vec<u8>) {
    let bytes: &[u32] = match code_point {
        0..0x80 => &[code_point],
        0x80..0x800 => &[0xc0 | code_point >> 6, 0x80 | (code_point & 0x3f)],
        0x800..0x10000 => &[
            0xe0 | code_point >> 12,
            0x80 | (code_point >> 6 & 0x3f),
            0x80 | (code_point & 0x3f),
        ],
        _ => &[
            0xf0 | code_point >> 18,
            0x80 | (code_point >> 12 & 0x3f),
            0x80 | (code_point >> 6 & 0x3f),
            0x80 | (code_point & 0x3f),
        ],
    };
    output.extend(
        bytes
            .iter()
            .map(|byte| u8::try_from(*byte).expect("one byte")),
    );
}

#[test]
fn the_tree_parser_normalizes_only_floats() -> anyhow::Result<()> {
    let value = Json::parse(br#" {"b": [1, -0, 1.50, 2E1, "x\u00e9\ud83d\ude00\n"], "a": {}} "#)?;
    assert_eq!(
        value.serialize(),
        r#"{"b":[1,-0,1.5,20.0,"x\xc3\xa9\xf0\x9f\x98\x80\x0a"],"a":{}}"#
    );
    Ok(())
}
