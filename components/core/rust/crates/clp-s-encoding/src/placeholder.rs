//! Placeholder bytes inside logtypes (`clp/ir/types.hpp`).

/// A placeholder byte inside a logtype, marking where a variable or an escape sits.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum VariablePlaceholder {
    /// An eight-byte encoded integer variable (`0x11`).
    Integer,

    /// A variable-dictionary id (`0x12`).
    Dictionary,

    /// An eight-byte encoded float variable (`0x13`).
    Float,

    /// An escape (`\`) preceding a literal placeholder byte.
    Escape,
}

impl VariablePlaceholder {
    /// # Returns
    ///
    /// The placeholder for `byte`, or `None` if `byte` is not a placeholder.
    #[must_use]
    pub const fn from_byte(byte: u8) -> Option<Self> {
        match byte {
            0x11 => Some(Self::Integer),
            0x12 => Some(Self::Dictionary),
            0x13 => Some(Self::Float),
            b'\\' => Some(Self::Escape),
            _ => None,
        }
    }

    /// # Returns
    ///
    /// The byte that encodes this placeholder in a logtype.
    #[must_use]
    pub const fn as_byte(self) -> u8 {
        match self {
            Self::Integer => 0x11,
            Self::Dictionary => 0x12,
            Self::Float => 0x13,
            Self::Escape => b'\\',
        }
    }
}

#[cfg(test)]
mod tests {
    use super::VariablePlaceholder;

    #[test]
    fn placeholder_bytes_match_clp_ir() {
        for (placeholder, byte) in [
            (VariablePlaceholder::Integer, 0x11),
            (VariablePlaceholder::Dictionary, 0x12),
            (VariablePlaceholder::Float, 0x13),
            (VariablePlaceholder::Escape, b'\\'),
        ] {
            assert_eq!(placeholder.as_byte(), byte);
            assert_eq!(VariablePlaceholder::from_byte(byte), Some(placeholder));
        }
    }

    #[test]
    fn ordinary_bytes_are_not_placeholders() {
        for byte in (0_u8..=u8::MAX).filter(|byte| ![0x11, 0x12, 0x13, b'\\'].contains(byte)) {
            assert_eq!(
                VariablePlaceholder::from_byte(byte),
                None,
                "byte {byte:#04x}"
            );
        }
    }
}
