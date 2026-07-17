use std::io::{Read, Write};

use brotli::{CompressorWriter, Decompressor};
use serde::{Serialize, de::DeserializeOwned};

use crate::Error;

/// Namespace for brotli compressed msgpack utils.
pub struct BrotliMsgpack {}

impl BrotliMsgpack {
    /// Serialize a value to a Brotli-compressed `MessagePack` byte sequence.
    ///
    /// # Return
    /// A vector of bytes containing the serialized byte sequence.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * Forwards [`rmp_serde::to_vec_named`]'s errors on failure.
    /// * Forwards [`std::io::Write::write_all`]'s errors on failure.
    pub fn serialize<T: Serialize + ?Sized>(val: &T) -> Result<Vec<u8>, Error> {
        let msgpack_data = rmp_serde::to_vec_named(val)?;
        let mut brotli_compressor = CompressorWriter::new(Vec::new(), 4096, 5, 22);
        brotli_compressor.write_all(&msgpack_data)?;
        Ok(brotli_compressor.into_inner())
    }

    /// Deserialize a value from a Brotli-compressed `MessagePack` byte sequence produced by
    /// [`BrotliMsgpack::serialize`].
    ///
    /// # Return
    /// The deserialized value.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * Forwards [`std::io::Read::read_to_end`]'s errors on failure (including invalid brotli
    ///   input).
    /// * Forwards [`rmp_serde::from_slice`]'s errors on failure.
    pub fn deserialize<T: DeserializeOwned>(bytes: &[u8]) -> Result<T, Error> {
        let mut brotli_decompressor = Decompressor::new(bytes, 4096);
        let mut msgpack_data = Vec::new();
        brotli_decompressor.read_to_end(&mut msgpack_data)?;
        Ok(rmp_serde::from_slice(&msgpack_data)?)
    }
}

#[cfg(test)]
mod tests {
    use non_empty_string::NonEmptyString;

    use super::BrotliMsgpack;
    use crate::{
        clp_config::{AwsAuthentication, S3Config},
        job_config::{ClpIoConfig, InputConfig, OutputConfig, S3ObjectMetadataInputConfig},
    };

    #[test]
    fn brotli_msgpack_round_trips_clp_io_config() -> anyhow::Result<()> {
        let config = ClpIoConfig {
            input: InputConfig::S3ObjectMetadataInputConfig {
                config: S3ObjectMetadataInputConfig {
                    s3_config: S3Config {
                        bucket: NonEmptyString::new("lzh-test".to_owned())
                            .expect("bucket should be non-empty"),
                        region_code: Some(
                            NonEmptyString::new("us-east-2".to_owned())
                                .expect("region should be non-empty"),
                        ),
                        key_prefix: NonEmptyString::new("LIB1/".to_owned())
                            .expect("key prefix should be non-empty"),
                        endpoint_url: None,
                        aws_authentication: AwsAuthentication::Default,
                    },
                    ingestion_job_id: 1,
                    s3_object_metadata_ids: vec![1, 2, 3],
                    dataset: Some(
                        NonEmptyString::new("default".to_owned())
                            .expect("dataset should be non-empty"),
                    ),
                    timestamp_key: None,
                    unstructured: false,
                },
            },
            output: OutputConfig {
                target_archive_size: 1_073_741_824,
                target_dictionaries_size: 33_554_432,
                target_encoded_file_size: 268_435_456,
                target_segment_size: 268_435_456,
                compression_level: 3,
            },
        };

        let serialized = BrotliMsgpack::serialize(&config)?;
        let deserialized: ClpIoConfig = BrotliMsgpack::deserialize(&serialized)?;

        assert_eq!(config, deserialized);
        Ok(())
    }
}
