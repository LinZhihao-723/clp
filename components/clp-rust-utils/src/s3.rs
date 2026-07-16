mod client;
mod url;

use std::path::Path;

pub use client::create_new_client;
use non_empty_string::NonEmptyString;
pub use url::generate_s3_url;

use crate::error::Error;

/// Represents the unique identifier for an S3 object metadata entry in CLP DB.
pub type S3ObjectMetadataId = u64;

/// Uploads a local file to S3 (mirror of Python's `s3_put`).
///
/// # Returns
///
/// `()` once the object has been uploaded.
///
/// # Errors
///
/// Returns [`Error::S3`] if reading `src` into a body stream fails or the `PutObject` request
/// fails.
pub async fn put_file(
    client: &aws_sdk_s3::Client,
    bucket: &str,
    key: &str,
    src: &Path,
) -> Result<(), Error> {
    let body = aws_sdk_s3::primitives::ByteStream::from_path(src)
        .await
        .map_err(|e| Error::S3(e.to_string()))?;
    client
        .put_object()
        .bucket(bucket)
        .key(key)
        .body(body)
        .send()
        .await
        .map_err(|e| Error::S3(e.to_string()))?;
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct ObjectMetadata {
    pub bucket: NonEmptyString,
    pub key: NonEmptyString,
    pub size: u64,
}
