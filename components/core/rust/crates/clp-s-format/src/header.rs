//! The fixed-size single-file-archive header (`SingleFileArchiveDefs.hpp`).

use crate::ClassifiedVersion;
use crate::CorruptionReason;
use crate::FormatError;
use crate::FormatLocation;
use crate::FormatVersion;
use crate::UnsupportedFeature;
use crate::bytes_reader::ByteReader;

/// The validated 64-byte header of a single-file archive (little-endian; magic number and
/// compression type checked).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ArchiveHeader {
    pub version: FormatVersion,
    pub uncompressed_size: u64,
    pub compressed_size: u64,
    pub metadata_section_size: u32,
}

impl ArchiveHeader {
    /// The magic number of a single-file clp-s archive.
    pub const MAGIC_NUMBER: [u8; 4] = [0xfd, 0x2f, 0xc5, 0x30];
    /// Size of the header in bytes.
    pub const SIZE: u64 = 64;
    /// The only supported compression type (zstd).
    pub const ZSTD_COMPRESSION_TYPE: u16 = 0;

    /// Factory function.
    ///
    /// Parses and validates the header at the start of `bytes` against the archive's file size.
    ///
    /// # Returns
    ///
    /// The validated header on success.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * [`FormatError::Corrupt`] if `bytes` is shorter than [`Self::SIZE`], the magic number is
    ///   wrong, or the sizes disagree with `file_size`.
    /// * [`FormatError::UnsupportedFeature`] if the compression type is not zstd.
    /// * Forwards [`crate::ClassifiedVersion::classify`]'s return values on failure.
    pub fn parse(bytes: &[u8], file_size: u64) -> Result<Self, FormatError> {
        let mut reader = ByteReader::new(bytes, FormatLocation::Header);
        if (bytes.len() as u64) < Self::SIZE {
            return Err(reader.corrupt(CorruptionReason::Truncated {
                needed_size: Self::SIZE,
                available_size: bytes.len() as u64,
            }));
        }
        if reader.read_array::<4>()? != Self::MAGIC_NUMBER {
            return Err(reader.corrupt(CorruptionReason::BadMagic));
        }
        let version = FormatVersion::from_packed(reader.read_u32()?);
        let uncompressed_size = reader.read_u64()?;
        let compressed_size = reader.read_u64()?;
        reader.read_bytes(32)?;
        let metadata_section_size = reader.read_u32()?;
        let compression_type = u16::from_le_bytes(reader.read_array()?);

        ClassifiedVersion::classify(version)?;
        if Self::ZSTD_COMPRESSION_TYPE != compression_type {
            return Err(FormatError::UnsupportedFeature {
                location: FormatLocation::Header,
                feature: UnsupportedFeature::CompressionType(compression_type),
            });
        }
        let is_size_consistent = compressed_size == file_size
            && Self::SIZE + u64::from(metadata_section_size) <= compressed_size;
        if !is_size_consistent {
            return Err(reader.corrupt(CorruptionReason::HeaderSizeMismatch));
        }

        Ok(Self {
            version,
            uncompressed_size,
            compressed_size,
            metadata_section_size,
        })
    }

    /// # Returns
    ///
    /// The offset of the first section, right after the metadata section.
    #[must_use]
    pub const fn sections_offset(&self) -> u64 {
        Self::SIZE + self.metadata_section_size as u64
    }
}

#[cfg(test)]
mod tests {
    use super::ArchiveHeader;
    use crate::CorruptionReason;
    use crate::FormatError;
    use crate::FormatVersion;
    use crate::UnsupportedFeature;

    const FILE_SIZE: u64 = 1000;
    const METADATA_SECTION_SIZE: u32 = 100;
    const VERSION_0_5_1: u32 = 0x0005_0001;

    /// # Returns
    ///
    /// An encoded header with the given fields.
    fn encode(version: u32, compressed_size: u64, compression_type: u16) -> Vec<u8> {
        let mut bytes = ArchiveHeader::MAGIC_NUMBER.to_vec();
        bytes.extend_from_slice(&version.to_le_bytes());
        bytes.extend_from_slice(&2000_u64.to_le_bytes());
        bytes.extend_from_slice(&compressed_size.to_le_bytes());
        bytes.extend_from_slice(&[0xaa; 32]);
        bytes.extend_from_slice(&METADATA_SECTION_SIZE.to_le_bytes());
        bytes.extend_from_slice(&compression_type.to_le_bytes());
        bytes.extend_from_slice(&[0, 0]);
        bytes
    }

    /// # Returns
    ///
    /// The corruption reason of `error`.
    fn corruption_reason(error: FormatError) -> CorruptionReason {
        match error {
            FormatError::Corrupt { reason, .. } => *reason,
            other => panic!("expected a corruption error, got {other:?}"),
        }
    }

    #[test]
    fn parses_a_valid_header() -> anyhow::Result<()> {
        let bytes = encode(VERSION_0_5_1, FILE_SIZE, 0);
        assert_eq!(bytes.len() as u64, ArchiveHeader::SIZE);
        let header = ArchiveHeader::parse(&bytes, FILE_SIZE)?;
        assert_eq!(
            header,
            ArchiveHeader {
                version: FormatVersion::from_packed(VERSION_0_5_1),
                uncompressed_size: 2000,
                compressed_size: FILE_SIZE,
                metadata_section_size: METADATA_SECTION_SIZE,
            }
        );
        assert_eq!(header.sections_offset(), 164);
        Ok(())
    }

    #[test]
    fn rejects_short_input_and_bad_magic() {
        let bytes = encode(VERSION_0_5_1, FILE_SIZE, 0);
        let error = ArchiveHeader::parse(&bytes[..63], FILE_SIZE)
            .expect_err("63 bytes cannot hold a header");
        assert_eq!(
            corruption_reason(error),
            CorruptionReason::Truncated {
                needed_size: 64,
                available_size: 63,
            }
        );

        let mut bad_magic = bytes;
        bad_magic[0] = 0x28;
        let error =
            ArchiveHeader::parse(&bad_magic, FILE_SIZE).expect_err("the magic number is wrong");
        assert_eq!(corruption_reason(error), CorruptionReason::BadMagic);
    }

    #[test]
    fn rejects_sizes_that_disagree_with_the_file() {
        for (compressed_size, file_size) in
            [(FILE_SIZE, FILE_SIZE + 1), (FILE_SIZE, 999), (100, 100)]
        {
            let bytes = encode(VERSION_0_5_1, compressed_size, 0);
            let error = ArchiveHeader::parse(&bytes, file_size)
                .expect_err("inconsistent sizes must be rejected");
            assert_eq!(
                corruption_reason(error),
                CorruptionReason::HeaderSizeMismatch,
                "compressed size {compressed_size}, file size {file_size}"
            );
        }
    }

    #[test]
    fn rejects_unsupported_versions_and_compression() {
        let error = ArchiveHeader::parse(&encode(0x0004_0000, FILE_SIZE, 0), FILE_SIZE)
            .expect_err("0.4.0 is outside the window");
        assert!(
            matches!(error, FormatError::UnsupportedVersion(version) if version.minor == 4),
            "{error:?}"
        );

        let error = ArchiveHeader::parse(&encode(VERSION_0_5_1, FILE_SIZE, 1), FILE_SIZE)
            .expect_err("compression type 1 is unsupported");
        assert!(
            matches!(
                error,
                FormatError::UnsupportedFeature {
                    feature: UnsupportedFeature::CompressionType(1),
                    ..
                }
            ),
            "{error:?}"
        );
    }
}
