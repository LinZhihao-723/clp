//! An S3 object as a [`clp_s_io::ByteSource`]: blocking, bounded ranged GETs issued through a
//! host-built `aws-sdk-s3` client on a host-provided multi-thread tokio runtime, and the parsing of
//! S3 object URLs. Nothing is read from the environment.

mod error;
mod source;
#[cfg(any(test, feature = "test-util"))]
pub mod test_util;
mod url;

pub use error::ObjectStoreError;
pub use source::S3Client;
pub use source::S3Location;
pub use source::S3SdkConfig;
pub use source::S3Source;
pub use source::S3SourceConfig;
pub use url::S3Url;
