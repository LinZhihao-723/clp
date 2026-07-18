mod client;
mod metadata;
mod url;

pub use client::create_new_client;
pub use metadata::get_object_metadata;
use non_empty_string::NonEmptyString;
pub use url::generate_s3_url;

/// Represents the unique identifier for an S3 object metadata entry in CLP DB.
pub type S3ObjectMetadataId = u64;

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct ObjectMetadata {
    pub bucket: NonEmptyString,
    pub key: NonEmptyString,
    pub size: u64,
    pub estimated_uncompressed_size: u64,
}

impl ObjectMetadata {
    #[must_use]
    pub fn new(bucket: NonEmptyString, key: NonEmptyString, size: u64) -> Self {
        const GZIP_COMPRESSION_RATIO_ESTIMATE: u64 = 13;
        const GZIP_SUFFIXES: &[&str] = &[".gz", ".gzip", ".tgz", ".tar.gz"];
        const ZSTD_COMPRESSION_RATIO_ESTIMATE: u64 = 8;
        const ZSTD_SUFFIXES: &[&str] = &[".zstd", ".zstandard", ".tar.zstd", ".tar.zstandard"];

        let key_str = key.as_str();
        let estimated_uncompressed_size =
            if GZIP_SUFFIXES.iter().any(|suffix| key_str.ends_with(suffix)) {
                size * GZIP_COMPRESSION_RATIO_ESTIMATE
            } else if ZSTD_SUFFIXES.iter().any(|suffix| key_str.ends_with(suffix)) {
                size * ZSTD_COMPRESSION_RATIO_ESTIMATE
            } else {
                size
            };

        Self {
            bucket,
            key,
            size,
            estimated_uncompressed_size,
        }
    }
}
