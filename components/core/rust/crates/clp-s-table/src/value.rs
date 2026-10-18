//! The closed value model: one typed, borrowed value per stored leaf.

use clp_s_format::LogTypeId;
use clp_s_format::TimestampPatternId;
use clp_s_format::VariableId;

use crate::EncodedTextBatch;
use crate::LeI64;
use crate::TableError;

/// One leaf value of a record, borrowed from its batch.
#[non_exhaustive]
#[derive(Clone, Copy, Debug)]
pub enum ValueRef<'batch> {
    Null,
    Boolean(bool),
    Integer(i64),
    Float(FloatRef<'batch>),
    String(StringRef<'batch>),

    /// A timestamp in epoch nanoseconds and the pattern it was parsed with.
    Timestamp {
        epoch_ns: i64,
        pattern_id: TimestampPatternId,
    },

    /// An unstructured array, still encoded; its decoded text is the raw JSON array.
    UnstructuredArray(EncodedTextRef<'batch>),

    EmptyObject,
}

/// A float in one of its three stored forms.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum FloatRef<'batch> {
    /// A plain `f64` (`Float` nodes).
    Plain(f64),

    /// An `f64` plus the format that restores its original text (`FormattedFloat` nodes).
    Formatted { value: f64, format: u16 },

    /// The original text, stored in the variable dictionary (`DictionaryFloat` nodes).
    Dictionary { id: VariableId, text: &'batch [u8] },
}

/// A string in one of its two stored forms.
#[derive(Clone, Copy, Debug)]
pub enum StringRef<'batch> {
    /// A variable-dictionary entry (`VarString` nodes).
    Variable { id: VariableId, text: &'batch [u8] },

    /// A logtype plus variables (`ClpString` nodes).
    Encoded(EncodedTextRef<'batch>),
}

/// One row of an encoded-text column.
#[derive(Clone, Copy, Debug)]
pub struct EncodedTextRef<'batch> {
    batch: EncodedTextBatch<'batch>,
    row: u32,
}

impl<'batch> EncodedTextRef<'batch> {
    /// # Returns
    ///
    /// The column window the value belongs to.
    #[must_use]
    pub const fn batch(&self) -> &EncodedTextBatch<'batch> {
        &self.batch
    }

    /// # Returns
    ///
    /// The window row of the value.
    #[must_use]
    pub const fn row(&self) -> u32 {
        self.row
    }

    /// # Returns
    ///
    /// The logtype id of the value.
    #[must_use]
    pub fn logtype_id(&self) -> LogTypeId {
        self.batch.logtype_id(self.row)
    }

    /// Returns the encoded variables of the value.
    ///
    /// # Returns
    ///
    /// The variables on success.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * Forwards [`EncodedTextBatch::variables`]'s return values on failure.
    pub fn variables(&self) -> Result<&'batch [LeI64], TableError> {
        self.batch.variables(self.row)
    }

    /// Appends the decoded text of the value to `output`.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * Forwards [`EncodedTextBatch::decode`]'s return values on failure.
    pub fn decode(&self, output: &mut Vec<u8>) -> Result<(), TableError> {
        self.batch.decode(self.row, output)
    }

    /// Factory function.
    ///
    /// # Returns
    ///
    /// The value of window row `row` of `batch`.
    pub(crate) const fn new(batch: EncodedTextBatch<'batch>, row: u32) -> Self {
        Self { batch, row }
    }
}
