//! The reusable buffer of rendered records.

use crate::JsonError;

/// Rendered records: contiguous bytes, each record followed by `\n`, plus Arrow `Utf8` (`i32`)
/// record offsets. Reused across batches.
#[derive(Debug)]
pub struct JsonRecordBuffer {
    bytes: Vec<u8>,

    /// `offsets[i]` is the start of record `i`; the last entry is the end of the last record's
    /// newline, so there is always one more offset than records.
    offsets: Vec<i32>,
}

impl JsonRecordBuffer {
    /// Factory function.
    ///
    /// # Returns
    ///
    /// An empty buffer.
    #[must_use]
    pub fn new() -> Self {
        Self {
            bytes: Vec::new(),
            offsets: vec![0],
        }
    }

    /// Removes every record, keeping the allocations.
    pub fn clear(&mut self) {
        self.bytes.clear();
        self.offsets.truncate(1);
    }

    /// # Returns
    ///
    /// The number of records.
    #[must_use]
    pub const fn len(&self) -> usize {
        self.offsets.len() - 1
    }

    /// # Returns
    ///
    /// Whether the buffer holds no record.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        1 == self.offsets.len()
    }

    /// # Returns
    ///
    /// The JSON text of record `index`, without its trailing newline, or `None` if out of range.
    #[must_use]
    pub fn record(&self, index: usize) -> Option<&[u8]> {
        let begin = usize::try_from(*self.offsets.get(index)?).ok()?;
        let end = usize::try_from(*self.offsets.get(index.checked_add(1)?)?).ok()?;
        self.bytes.get(begin..end.checked_sub(1)?)
    }

    /// # Returns
    ///
    /// Every record as JSON lines (each record followed by `\n`).
    #[must_use]
    pub fn as_jsonl(&self) -> &[u8] {
        &self.bytes
    }

    /// # Returns
    ///
    /// The total size of [`Self::as_jsonl`] in bytes.
    #[must_use]
    pub const fn byte_size(&self) -> u64 {
        self.bytes.len() as u64
    }

    /// Appends a record rendered earlier (its JSON text without the trailing newline), followed by
    /// `\n`; used to gather records across batches (top-k winners).
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * Forwards [`Self::push_record`]'s return values on failure.
    pub fn push_rendered(&mut self, record: &[u8]) -> Result<(), JsonError> {
        self.push_record(|bytes| {
            bytes.extend_from_slice(record);
            Ok(())
        })
    }

    /// Appends one record whose text `write` appends to the byte buffer, followed by `\n`. On
    /// failure, the buffer is left as it was.
    ///
    /// # Type Parameters
    ///
    /// * `WriterType` - Appends the record's text.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * [`JsonError::BufferOverflow`] if the buffer would exceed what `i32` offsets address.
    /// * Forwards `write`'s return values on failure.
    pub(crate) fn push_record<WriterType: FnOnce(&mut Vec<u8>) -> Result<(), JsonError>>(
        &mut self,
        write: WriterType,
    ) -> Result<(), JsonError> {
        let begin = self.bytes.len();
        if let Err(error) = write(&mut self.bytes) {
            self.bytes.truncate(begin);
            return Err(error);
        }
        self.bytes.push(b'\n');
        let Ok(end) = i32::try_from(self.bytes.len()) else {
            self.bytes.truncate(begin);
            return Err(JsonError::BufferOverflow {
                limit_size: i32::MAX.unsigned_abs().into(),
            });
        };
        self.offsets.push(end);
        Ok(())
    }

    /// Removes every record after the first `num_records`.
    pub(crate) fn truncate_records(&mut self, num_records: usize) {
        if num_records >= self.len() {
            return;
        }
        self.offsets.truncate(num_records + 1);
        let end = self.offsets.last().copied().unwrap_or(0);
        self.bytes.truncate(usize::try_from(end).unwrap_or(0));
    }
}

impl Default for JsonRecordBuffer {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Appends a record holding `text` to `buffer`.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * Forwards [`JsonRecordBuffer::push_record`]'s return values on failure.
    fn push(buffer: &mut JsonRecordBuffer, text: &[u8]) -> Result<(), JsonError> {
        buffer.push_record(|bytes| {
            bytes.extend_from_slice(text);
            Ok(())
        })
    }

    #[test]
    fn records_are_newline_terminated_with_offsets() -> anyhow::Result<()> {
        let mut buffer = JsonRecordBuffer::new();
        assert!(buffer.is_empty(), "{buffer:?}");
        assert_eq!(buffer.record(0), None);
        push(&mut buffer, b"{\"a\":1}")?;
        push(&mut buffer, b"{}")?;
        assert_eq!(buffer.len(), 2);
        assert_eq!(buffer.record(0), Some(&b"{\"a\":1}"[..]));
        assert_eq!(buffer.record(1), Some(&b"{}"[..]));
        assert_eq!(buffer.record(2), None);
        assert_eq!(buffer.record(usize::MAX), None);
        assert_eq!(buffer.as_jsonl(), b"{\"a\":1}\n{}\n");
        assert_eq!(buffer.byte_size(), 11);
        assert_eq!(buffer.offsets, vec![0, 8, 11]);
        Ok(())
    }

    #[test]
    fn rendered_records_are_appended_with_a_newline() -> anyhow::Result<()> {
        let mut buffer = JsonRecordBuffer::new();
        buffer.push_rendered(b"{\"a\":1}")?;
        buffer.push_rendered(b"")?;
        assert_eq!(buffer.record(0), Some(&b"{\"a\":1}"[..]));
        assert_eq!(buffer.record(1), Some(&b""[..]));
        assert_eq!(buffer.as_jsonl(), b"{\"a\":1}\n\n");
        Ok(())
    }

    #[test]
    fn failed_records_leave_the_buffer_unchanged() -> anyhow::Result<()> {
        let mut buffer = JsonRecordBuffer::new();
        push(&mut buffer, b"{}")?;
        let result = buffer.push_record(|bytes| {
            bytes.extend_from_slice(b"{\"partial\":");
            Err(JsonError::BufferOverflow { limit_size: 0 })
        });
        assert!(
            matches!(result, Err(JsonError::BufferOverflow { .. })),
            "{result:?}"
        );
        assert_eq!(buffer.as_jsonl(), b"{}\n");
        assert_eq!(buffer.len(), 1);
        Ok(())
    }

    #[test]
    fn clear_and_truncate_keep_the_offsets_consistent() -> anyhow::Result<()> {
        let mut buffer = JsonRecordBuffer::default();
        for text in [&b"{\"a\":1}"[..], b"{\"b\":2}", b"{\"c\":3}"] {
            push(&mut buffer, text)?;
        }
        buffer.truncate_records(5);
        assert_eq!(buffer.len(), 3);
        buffer.truncate_records(1);
        assert_eq!(buffer.as_jsonl(), b"{\"a\":1}\n");
        assert_eq!(buffer.offsets, vec![0, 8]);
        buffer.clear();
        assert!(buffer.is_empty(), "{buffer:?}");
        assert_eq!(buffer.as_jsonl(), b"");
        push(&mut buffer, b"{}")?;
        assert_eq!(buffer.record(0), Some(&b"{}"[..]));
        Ok(())
    }
}
