//! `--auth s3` inputs: S3 object URLs read with credentials from the default AWS chain.

use std::sync::Arc;

use clp_s_reader::ArchiveId;
use clp_s_reader::ArchiveLocation;
use clp_s_reader::s3::S3SdkConfig;
use clp_s_reader::s3::S3Source;
use clp_s_reader::s3::S3SourceConfig;
use clp_s_reader::s3::S3Url;

use crate::CommandError;

/// The region of URLs that name none, when the AWS chain has none either (C++
/// `clp::aws::cDefaultRegion`).
const DEFAULT_REGION: &str = "us-east-1";

/// The shared AWS configuration of an invocation and the runtime its S3 sources block on.
///
/// The sources opened by [`S3Inputs::open`] read through the runtime, so `self` must outlive
/// every read of them.
#[derive(Debug)]
pub struct S3Inputs {
    sdk_config: S3SdkConfig,
    region: Option<String>,
    endpoint_url: Option<String>,
    runtime: tokio::runtime::Runtime,
}

impl S3Inputs {
    /// Factory function.
    ///
    /// Starts a multi-thread runtime and loads the default AWS configuration chain (environment,
    /// profiles, instance metadata) on it. `region` and `endpoint_url` (`--s3-region`,
    /// `--s3-endpoint-url`) override what each URL and the chain name.
    ///
    /// # Returns
    ///
    /// The inputs on success.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * [`CommandError::Runtime`] if the runtime cannot be started.
    pub fn new(region: Option<String>, endpoint_url: Option<String>) -> Result<Self, CommandError> {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .map_err(CommandError::Runtime)?;
        let sdk_config =
            runtime.block_on(aws_config::defaults(aws_config::BehaviorVersion::latest()).load());
        Ok(Self {
            sdk_config,
            region,
            endpoint_url,
            runtime,
        })
    }

    /// Opens the S3 object `url` names: its region is `--s3-region`, else the URL's, else the
    /// chain's, else `us-east-1`; its endpoint is `--s3-endpoint-url`, else the URL's (non-AWS),
    /// else the chain's. Custom endpoints are addressed path-style.
    ///
    /// # Returns
    ///
    /// The archive id (the last segment of the key, like C++) and location on success.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * [`CommandError::InvalidArgument`] if the key has no last segment to name the archive.
    /// * [`CommandError::S3`] if `url` is not an S3 object URL.
    /// * [`CommandError::S3`] wrapping [`S3Source::from_sdk_config`]'s errors on failure.
    pub fn open(&self, url: &str) -> Result<(ArchiveId, ArchiveLocation), CommandError> {
        let url = S3Url::parse(url).map_err(CommandError::S3)?;
        let mut config = self.sdk_config.to_builder();
        let region = self
            .region
            .as_deref()
            .or_else(|| url.region())
            .or_else(|| self.sdk_config.region().map(AsRef::as_ref))
            .unwrap_or(DEFAULT_REGION)
            .to_owned();
        if let Some(endpoint_url) = self.endpoint_url.as_deref().or_else(|| url.endpoint_url()) {
            config.set_endpoint_url(Some(endpoint_url.to_owned()));
        }
        let location = url.into_location();
        let archive_id = location.file_name().map(ArchiveId::from).ok_or_else(|| {
            CommandError::InvalidArgument(format!("cannot derive an archive id from `{location}`"))
        })?;
        tracing::debug!(
            location = % location,
            region = % region,
            "Opening an S3 archive."
        );
        config.set_region(Some(aws_config::Region::new(region)));
        let source = S3Source::from_sdk_config(
            &config.build(),
            self.runtime.handle().clone(),
            location,
            S3SourceConfig::default(),
        )
        .map_err(CommandError::S3)?;
        Ok((archive_id, ArchiveLocation::Source(Arc::new(source))))
    }
}
