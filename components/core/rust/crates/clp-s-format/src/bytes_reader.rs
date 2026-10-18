//! A bounds-checked cursor over decoded archive bytes.

use crate::CorruptionReason;
use crate::FormatError;
use crate::FormatLocation;

/// A forward cursor over a byte slice; every read is bounds-checked and fails as
/// [`FormatError::Corrupt`] at the cursor's location.
#[derive(Debug)]
pub struct ByteReader<'bytes> {
    bytes: &'bytes [u8],
    position: usize,
    location: FormatLocation,
}

impl<'bytes> ByteReader<'bytes> {
    /// Factory function.
    ///
    /// # Returns
    ///
    /// A cursor at the start of `bytes`, reporting errors at `location`.
    pub const fn new(bytes: &'bytes [u8], location: FormatLocation) -> Self {
        Self {
            bytes,
            position: 0,
            location,
        }
    }

    /// # Returns
    ///
    /// The location errors are reported at.
    pub const fn location(&self) -> FormatLocation {
        self.location
    }

    /// # Returns
    ///
    /// The offset of the cursor from the start of the bytes.
    pub const fn position(&self) -> usize {
        self.position
    }

    /// # Returns
    ///
    /// The number of bytes after the cursor.
    pub const fn remaining_size(&self) -> usize {
        self.bytes.len() - self.position
    }

    /// # Returns
    ///
    /// A [`FormatError::Corrupt`] for `reason` at the cursor's location.
    pub fn corrupt(&self, reason: CorruptionReason) -> FormatError {
        FormatError::corrupt(self.location, reason)
    }

    /// Reads the next `size` bytes.
    ///
    /// # Returns
    ///
    /// The bytes on success.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * [`FormatError::Corrupt`] if fewer than `size` bytes remain.
    pub fn read_bytes(&mut self, size: u64) -> Result<&'bytes [u8], FormatError> {
        let available_size = self.remaining_size();
        let Some(size) = usize::try_from(size)
            .ok()
            .filter(|size| *size <= available_size)
        else {
            return Err(self.corrupt(CorruptionReason::Truncated {
                needed_size: size,
                available_size: available_size as u64,
            }));
        };
        let bytes = &self.bytes[self.position..self.position + size];
        self.position += size;
        Ok(bytes)
    }

    /// Reads the next `SIZE` bytes as an array.
    ///
    /// # Returns
    ///
    /// The bytes on success.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * Forwards [`Self::read_bytes`]'s return values on failure.
    pub fn read_array<const SIZE: usize>(&mut self) -> Result<[u8; SIZE], FormatError> {
        let bytes = self.read_bytes(SIZE as u64)?;
        let mut array = [0_u8; SIZE];
        array.copy_from_slice(bytes);
        Ok(array)
    }

    /// # Returns
    ///
    /// The next byte on success.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * Forwards [`Self::read_array`]'s return values on failure.
    pub fn read_u8(&mut self) -> Result<u8, FormatError> {
        Ok(self.read_array::<1>()?[0])
    }

    /// # Returns
    ///
    /// The next little-endian `u32` on success.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * Forwards [`Self::read_array`]'s return values on failure.
    pub fn read_u32(&mut self) -> Result<u32, FormatError> {
        Ok(u32::from_le_bytes(self.read_array()?))
    }

    /// # Returns
    ///
    /// The next little-endian `u64` on success.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * Forwards [`Self::read_array`]'s return values on failure.
    pub fn read_u64(&mut self) -> Result<u64, FormatError> {
        Ok(u64::from_le_bytes(self.read_array()?))
    }

    /// # Returns
    ///
    /// The next little-endian `i32` on success.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * Forwards [`Self::read_array`]'s return values on failure.
    pub fn read_i32(&mut self) -> Result<i32, FormatError> {
        Ok(i32::from_le_bytes(self.read_array()?))
    }

    /// # Returns
    ///
    /// The next little-endian `i64` on success.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * Forwards [`Self::read_array`]'s return values on failure.
    pub fn read_i64(&mut self) -> Result<i64, FormatError> {
        Ok(i64::from_le_bytes(self.read_array()?))
    }

    /// Checks that `count` items of at least `min_item_size` bytes each fit in the remaining
    /// bytes, so that `count` can drive an allocation.
    ///
    /// # Returns
    ///
    /// `count` as a `usize` on success.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * [`FormatError::Corrupt`] if the items cannot fit.
    pub fn check_count(&self, count: u64, min_item_size: u64) -> Result<usize, FormatError> {
        let remaining_size = self.remaining_size() as u64;
        count
            .checked_mul(min_item_size)
            .filter(|total_size| *total_size <= remaining_size)
            .and_then(|_| usize::try_from(count).ok())
            .ok_or_else(|| {
                self.corrupt(CorruptionReason::CountExceedsRemaining {
                    count,
                    remaining_size,
                })
            })
    }
}

#[cfg(test)]
mod tests {
    use super::ByteReader;
    use crate::CorruptionReason;
    use crate::FormatError;
    use crate::FormatLocation;

    /// # Returns
    ///
    /// The corruption reason of `error`.
    fn reason(error: FormatError) -> CorruptionReason {
        match error {
            FormatError::Corrupt { reason, .. } => *reason,
            other => panic!("expected a corruption error, got {other:?}"),
        }
    }

    #[test]
    fn reads_little_endian_values_in_order() -> anyhow::Result<()> {
        let mut bytes = vec![7_u8];
        bytes.extend_from_slice(&0x0102_0304_u32.to_le_bytes());
        bytes.extend_from_slice(&(-2_i32).to_le_bytes());
        bytes.extend_from_slice(&u64::MAX.to_le_bytes());
        bytes.extend_from_slice(&i64::MIN.to_le_bytes());
        bytes.extend_from_slice(b"key");

        let mut reader = ByteReader::new(&bytes, FormatLocation::Header);
        assert_eq!(reader.read_u8()?, 7);
        assert_eq!(reader.read_u32()?, 0x0102_0304);
        assert_eq!(reader.read_i32()?, -2);
        assert_eq!(reader.read_u64()?, u64::MAX);
        assert_eq!(reader.read_i64()?, i64::MIN);
        assert_eq!(reader.remaining_size(), 3);
        assert_eq!(reader.read_bytes(3)?, b"key");
        assert_eq!(reader.position(), bytes.len());
        Ok(())
    }

    #[test]
    fn short_reads_are_truncation_errors() {
        let mut reader = ByteReader::new(&[1, 2, 3], FormatLocation::Header);
        let error = reader
            .read_u64()
            .expect_err("three bytes cannot hold a u64");
        assert_eq!(
            reason(error),
            CorruptionReason::Truncated {
                needed_size: 8,
                available_size: 3,
            }
        );
        assert_eq!(reader.position(), 0);

        let error = reader
            .read_bytes(u64::MAX)
            .expect_err("a huge length must not be read");
        assert_eq!(
            reason(error),
            CorruptionReason::Truncated {
                needed_size: u64::MAX,
                available_size: 3,
            }
        );
    }

    #[test]
    fn counts_must_fit_the_remaining_bytes() -> anyhow::Result<()> {
        let reader = ByteReader::new(&[0; 16], FormatLocation::Header);
        assert_eq!(reader.check_count(2, 8)?, 2);
        assert_eq!(reader.check_count(0, 8)?, 0);

        for count in [3, u64::MAX] {
            let error = reader
                .check_count(count, 8)
                .expect_err("the items cannot fit in 16 bytes");
            assert_eq!(
                reason(error),
                CorruptionReason::CountExceedsRemaining {
                    count,
                    remaining_size: 16,
                }
            );
        }
        Ok(())
    }
}
