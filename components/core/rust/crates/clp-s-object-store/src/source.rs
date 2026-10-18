//! The S3 byte source.

use std::num::NonZeroU64;
use std::sync::mpsc::RecvTimeoutError;
use std::time::Duration;

use clp_s_io::ByteSource;
use clp_s_io::ReadContext;
use clp_s_io::SourceCapabilities;
use clp_s_resources::CancellationToken;

use crate::ObjectStoreError;

/// The S3 client type hosts build and pass in.
pub type S3Client = aws_sdk_s3::Client;

/// The shared AWS configuration (credentials, region, endpoint) hosts may build an S3 source from.
pub type S3SdkConfig = aws_config::SdkConfig;

/// An S3 object.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct S3Location {
    pub bucket: String,
    pub key: String,
}

impl S3Location {
    /// # Returns
    ///
    /// The last `/`-separated segment of the key, or `None` if it is empty.
    #[must_use]
    pub fn file_name(&self) -> Option<&str> {
        self.key
            .rsplit('/')
            .next()
            .filter(|file_name| !file_name.is_empty())
    }
}

impl std::fmt::Display for S3Location {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "s3://{}/{}", self.bucket, self.key)
    }
}

/// Tunables of an S3 source.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct S3SourceConfig {
    /// Largest single ranged GET; longer reads are split into sequential requests.
    pub max_request_size: NonZeroU64,
}

impl Default for S3SourceConfig {
    fn default() -> Self {
        const DEFAULT_MAX_REQUEST_SIZE: NonZeroU64 = NonZeroU64::new(64 * 1024 * 1024)
            .expect("the default maximum request size is not zero");
        Self {
            max_request_size: DEFAULT_MAX_REQUEST_SIZE,
        }
    }
}

/// An S3 object as a byte source: each read is one or more sequential, bounded
/// `Range: bytes=a-b` GETs, blocking on the host runtime.
///
/// Requests run as tasks on the host runtime while the calling thread waits, so a read never
/// panics when it is issued from an async context, but it does block that thread; call it from a
/// blocking context. A cancelled read returns [`std::io::ErrorKind::Interrupted`] without waiting
/// for its request to finish.
#[derive(Debug)]
pub struct S3Source {
    client: S3Client,
    runtime: tokio::runtime::Handle,
    location: S3Location,
    config: S3SourceConfig,
    size: u64,
}

impl S3Source {
    /// Factory function.
    ///
    /// Issues a `HeadObject` to learn the object size. Like opening an archive, the request cannot
    /// be cancelled: it waits until it completes or fails, bounded only by the timeouts configured
    /// on `client`.
    ///
    /// # Returns
    ///
    /// The source on success.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * [`ObjectStoreError::CurrentThreadRuntime`] if `runtime` is a current-thread runtime.
    /// * [`ObjectStoreError::Head`] if the `HeadObject` request fails.
    /// * [`ObjectStoreError::MissingContentLength`] if the response has no valid content length.
    /// * [`ObjectStoreError::RequestAborted`] if the request task does not complete.
    pub fn new(
        client: S3Client,
        runtime: tokio::runtime::Handle,
        location: S3Location,
        config: S3SourceConfig,
    ) -> Result<Self, ObjectStoreError> {
        if tokio::runtime::RuntimeFlavor::CurrentThread == runtime.runtime_flavor() {
            return Err(ObjectStoreError::CurrentThreadRuntime);
        }
        let request = client
            .head_object()
            .bucket(&location.bucket)
            .key(&location.key)
            .send();
        let content_length = run_task(&runtime, request, None)
            .map_err(|_| ObjectStoreError::RequestAborted)?
            .map_err(|source| ObjectStoreError::Head {
                bucket: location.bucket.clone(),
                key: location.key.clone(),
                source: Box::new(source),
            })?
            .content_length();
        let size = content_length
            .and_then(|length| u64::try_from(length).ok())
            .ok_or_else(|| ObjectStoreError::MissingContentLength {
                bucket: location.bucket.clone(),
                key: location.key.clone(),
            })?;
        Ok(Self {
            client,
            runtime,
            location,
            config,
            size,
        })
    }

    /// Factory function.
    ///
    /// Builds the client from `sdk_config`, addressing buckets path-style when it names a custom
    /// endpoint (e.g. a `MinIO` server), then issues a `HeadObject` to learn the object size.
    ///
    /// # Returns
    ///
    /// The source on success.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * Forwards [`S3Source::new`]'s return values on failure.
    pub fn from_sdk_config(
        sdk_config: &S3SdkConfig,
        runtime: tokio::runtime::Handle,
        location: S3Location,
        config: S3SourceConfig,
    ) -> Result<Self, ObjectStoreError> {
        let client_config = aws_sdk_s3::config::Builder::from(sdk_config)
            .force_path_style(sdk_config.endpoint_url().is_some())
            .build();
        Self::new(
            S3Client::from_conf(client_config),
            runtime,
            location,
            config,
        )
    }

    /// # Returns
    ///
    /// The object this source reads.
    #[must_use]
    pub const fn location(&self) -> &S3Location {
        &self.location
    }

    /// Fetches `buffer.len()` bytes at `offset` with one ranged GET.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * [`std::io::ErrorKind::Interrupted`] if the read is cancelled through `token`.
    /// * [`std::io::Error`] wrapping [`ObjectStoreError::RequestAborted`] if the request task does
    ///   not complete.
    /// * [`std::io::Error`] wrapping [`RangeRequest::send`]'s errors on failure.
    fn read_range(
        &self,
        offset: u64,
        buffer: &mut [u8],
        token: &CancellationToken,
    ) -> Result<(), std::io::Error> {
        let request = RangeRequest {
            client: self.client.clone(),
            location: self.location.clone(),
            offset,
            size: buffer.len(),
        }
        .send();
        let bytes = run_task(&self.runtime, request, Some(token))
            .map_err(|interruption| match interruption {
                Interruption::Cancelled => {
                    std::io::Error::new(std::io::ErrorKind::Interrupted, "the read was cancelled")
                }
                Interruption::Aborted => std::io::Error::other(ObjectStoreError::RequestAborted),
            })?
            .map_err(std::io::Error::other)?;
        buffer.copy_from_slice(&bytes);
        Ok(())
    }
}

impl ByteSource for S3Source {
    fn capabilities(&self) -> SourceCapabilities {
        SourceCapabilities {
            is_resident: false,
            is_remote: true,
        }
    }

    fn size(&self) -> Result<u64, std::io::Error> {
        Ok(self.size)
    }

    fn read_exact_at(
        &self,
        offset: u64,
        buffer: &mut [u8],
        context: &ReadContext<'_>,
    ) -> Result<(), std::io::Error> {
        context.check()?;
        if offset
            .checked_add(buffer.len() as u64)
            .is_none_or(|end| end > self.size)
        {
            return Err(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "the read extends past the end of the object",
            ));
        }
        let max_request_size =
            usize::try_from(self.config.max_request_size.get()).unwrap_or(usize::MAX);
        let mut request_offset = offset;
        for request_buffer in buffer.chunks_mut(max_request_size) {
            context.check()?;
            self.read_range(request_offset, request_buffer, context.cancellation_token)?;
            request_offset += request_buffer.len() as u64;
        }
        Ok(())
    }
}

/// How often a waiting read checks its cancellation token.
const CANCELLATION_POLL_INTERVAL: Duration = Duration::from_millis(10);

/// Why a task's output is unavailable.
#[derive(Debug)]
enum Interruption {
    /// The caller's cancellation token was cancelled while the task ran.
    Cancelled,

    /// The task was dropped without completing: the runtime shut down, or the task panicked.
    Aborted,
}

/// One `Range: bytes=a-b` GET of `size` bytes at `offset`.
struct RangeRequest {
    client: S3Client,
    location: S3Location,
    offset: u64,
    size: usize,
}

impl RangeRequest {
    /// Sends the request and reads the response body.
    ///
    /// # Returns
    ///
    /// The bytes of the range on success.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * [`ObjectStoreError::Get`] if the request or the response body fails.
    /// * [`ObjectStoreError::UnexpectedResponse`] if the response is not exactly the requested
    ///   range.
    async fn send(self) -> Result<Vec<u8>, ObjectStoreError> {
        let last = self.last();
        tracing::debug!(
            bucket = % self.location.bucket,
            key = % self.location.key,
            begin = self.offset,
            end = last,
            "Issuing a ranged GET."
        );
        let mut output = self
            .client
            .get_object()
            .bucket(&self.location.bucket)
            .key(&self.location.key)
            .range(format!("bytes={}-{last}", self.offset))
            .send()
            .await
            .map_err(|source| self.get_error(Box::new(source)))?;
        if let Some(content_range) = output.content_range()
            && !content_range.starts_with(&format!("bytes {}-{last}/", self.offset))
        {
            return Err(self.unexpected_response(format!("the response covers `{content_range}`")));
        }
        let mut bytes = Vec::with_capacity(self.size);
        while let Some(chunk) = output
            .body
            .try_next()
            .await
            .map_err(|source| self.get_error(Box::new(source)))?
        {
            if chunk.len() > self.size - bytes.len() {
                return Err(self
                    .unexpected_response("the response body is longer than the range".to_owned()));
            }
            bytes.extend_from_slice(&chunk);
        }
        if bytes.len() != self.size {
            return Err(self.unexpected_response(format!(
                "the response body has {} of {} bytes",
                bytes.len(),
                self.size
            )));
        }
        Ok(bytes)
    }

    /// # Returns
    ///
    /// The offset of the last byte of the range.
    const fn last(&self) -> u64 {
        self.offset + self.size as u64 - 1
    }

    /// # Returns
    ///
    /// The [`ObjectStoreError::Get`] error of this request, caused by `source`.
    fn get_error(&self, source: Box<dyn std::error::Error + Send + Sync>) -> ObjectStoreError {
        ObjectStoreError::Get {
            bucket: self.location.bucket.clone(),
            key: self.location.key.clone(),
            begin: self.offset,
            end: self.last(),
            source,
        }
    }

    /// # Returns
    ///
    /// The [`ObjectStoreError::UnexpectedResponse`] error of this request, explained by `reason`.
    fn unexpected_response(&self, reason: String) -> ObjectStoreError {
        ObjectStoreError::UnexpectedResponse {
            bucket: self.location.bucket.clone(),
            key: self.location.key.clone(),
            begin: self.offset,
            end: self.last(),
            reason,
        }
    }
}

/// Runs `future` as a task on `runtime` and blocks the calling thread until it completes. With
/// `token`, the task is aborted as soon as `token` is cancelled.
///
/// # Type Parameters
///
/// * `FutureType` - The request to run.
///
/// # Returns
///
/// The output of `future` on success.
///
/// # Errors
///
/// Returns an error if:
///
/// * [`Interruption::Cancelled`] if `token` is cancelled before the task completes.
/// * [`Interruption::Aborted`] if the task is dropped without completing.
fn run_task<FutureType: Future<Output: Send + 'static> + Send + 'static>(
    runtime: &tokio::runtime::Handle,
    future: FutureType,
    token: Option<&CancellationToken>,
) -> Result<FutureType::Output, Interruption> {
    let (sender, receiver) = std::sync::mpsc::sync_channel(1);
    let task = runtime.spawn(async move {
        // The receiver is gone only when the caller stopped waiting (cancellation).
        drop(sender.send(future.await));
    });
    let Some(token) = token else {
        return receiver.recv().map_err(|_| Interruption::Aborted);
    };
    loop {
        match receiver.recv_timeout(CANCELLATION_POLL_INTERVAL) {
            Ok(output) => return Ok(output),
            Err(RecvTimeoutError::Timeout) => {
                if token.is_cancelled() {
                    task.abort();
                    return Err(Interruption::Cancelled);
                }
            }
            Err(RecvTimeoutError::Disconnected) => return Err(Interruption::Aborted),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::num::NonZeroU64;
    use std::time::Duration;
    use std::time::Instant;

    use clp_s_io::ByteSource;
    use clp_s_io::ReadContext;
    use clp_s_resources::CancellationToken;
    use clp_s_resources::ErrorKind;

    use super::S3Location;
    use super::S3SdkConfig;
    use super::S3Source;
    use super::S3SourceConfig;
    use crate::ObjectStoreError;
    use crate::test_util::MockBehavior;
    use crate::test_util::MockRequest;
    use crate::test_util::MockS3Server;

    const BUCKET: &str = "bucket";
    const KEY: &str = "dir/archive.clps";
    const OBJECT_SIZE: usize = 1000;

    /// A multi-thread runtime with a mock server holding the object [`BUCKET`]/[`KEY`] of
    /// [`OBJECT_SIZE`] bytes, where byte `i` is `i % 251`.
    struct Fixture {
        server: MockS3Server,
        runtime: tokio::runtime::Runtime,
    }

    impl Fixture {
        /// Factory function.
        ///
        /// # Returns
        ///
        /// The fixture.
        ///
        /// # Panics
        ///
        /// Panics if the runtime or the server cannot be started.
        fn new() -> Self {
            let runtime = multi_thread_runtime();
            let server = MockS3Server::start(runtime.handle()).expect("the mock server starts");
            server.put_object(BUCKET, KEY, object());
            Self { server, runtime }
        }

        /// # Returns
        ///
        /// A source of the fixture object with `max_request_size`, on success.
        ///
        /// # Errors
        ///
        /// Returns an error if:
        ///
        /// * Forwards [`S3Source::new`]'s return values on failure.
        fn source(&self, max_request_size: u64) -> Result<S3Source, ObjectStoreError> {
            S3Source::new(
                self.server.client(),
                self.runtime.handle().clone(),
                location(KEY),
                S3SourceConfig {
                    max_request_size: NonZeroU64::new(max_request_size)
                        .expect("tests use a non-zero request size"),
                },
            )
        }

        /// # Returns
        ///
        /// The `Range` headers of the GETs received so far.
        fn get_ranges(&self) -> Vec<String> {
            self.server
                .requests()
                .into_iter()
                .filter(|request| "GET" == request.method)
                .map(|request| request.range.unwrap_or_default())
                .collect()
        }
    }

    /// # Returns
    ///
    /// A multi-thread runtime.
    ///
    /// # Panics
    ///
    /// Panics if the runtime cannot be started.
    fn multi_thread_runtime() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .expect("the runtime starts")
    }

    /// # Returns
    ///
    /// The content of the fixture object.
    fn object() -> Vec<u8> {
        (0..OBJECT_SIZE)
            .map(|index| u8::try_from(index % 251).expect("the value fits a byte"))
            .collect()
    }

    /// # Returns
    ///
    /// The location of object `key` in the test bucket.
    fn location(key: &str) -> S3Location {
        S3Location {
            bucket: BUCKET.to_owned(),
            key: key.to_owned(),
        }
    }

    /// Reads `size` bytes at `offset` of `source` under `token`.
    ///
    /// # Returns
    ///
    /// The bytes on success.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * Forwards [`S3Source::read_exact_at`]'s return values on failure.
    fn read(
        source: &S3Source,
        offset: u64,
        size: usize,
        token: &CancellationToken,
    ) -> Result<Vec<u8>, std::io::Error> {
        let mut buffer = vec![0; size];
        source.read_exact_at(
            offset,
            &mut buffer,
            &ReadContext {
                cancellation_token: token,
            },
        )?;
        Ok(buffer)
    }

    /// # Returns
    ///
    /// The object-store error inside `error`, if any.
    fn object_store_error(error: &std::io::Error) -> Option<&ObjectStoreError> {
        error.get_ref()?.downcast_ref::<ObjectStoreError>()
    }

    #[test]
    fn current_thread_runtimes_are_rejected() -> anyhow::Result<()> {
        let fixture = Fixture::new();
        let current_thread = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?;
        let error = S3Source::new(
            fixture.server.client(),
            current_thread.handle().clone(),
            location(KEY),
            S3SourceConfig::default(),
        )
        .expect_err("a current-thread runtime is rejected");
        assert!(
            matches!(error, ObjectStoreError::CurrentThreadRuntime),
            "{error:?}"
        );
        assert_eq!(error.kind(), ErrorKind::InvalidArgument);
        assert_eq!(fixture.server.requests(), []);
        Ok(())
    }

    #[test]
    fn opening_issues_one_head_for_the_size() -> anyhow::Result<()> {
        let fixture = Fixture::new();
        let source = fixture.source(64)?;
        assert_eq!(source.size()?, OBJECT_SIZE as u64);
        assert_eq!(source.location(), &location(KEY));
        let capabilities = source.capabilities();
        assert!(capabilities.is_remote && !capabilities.is_resident);
        assert_eq!(
            fixture.server.requests(),
            [MockRequest {
                method: "HEAD".to_owned(),
                bucket: BUCKET.to_owned(),
                key: KEY.to_owned(),
                range: None,
            }]
        );
        Ok(())
    }

    #[test]
    fn missing_objects_fail_to_open() {
        let fixture = Fixture::new();
        let error = S3Source::new(
            fixture.server.client(),
            fixture.runtime.handle().clone(),
            location("missing.clps"),
            S3SourceConfig::default(),
        )
        .expect_err("the object does not exist");
        assert!(
            matches!(&error, ObjectStoreError::Head { key, .. } if "missing.clps" == key),
            "{error:?}"
        );
        assert_eq!(error.kind(), ErrorKind::Io);
    }

    #[test]
    fn reads_are_split_into_sequential_bounded_ranged_gets() -> anyhow::Result<()> {
        let fixture = Fixture::new();
        let token = CancellationToken::new();
        let source = fixture.source(300)?;
        assert_eq!(read(&source, 100, 650, &token)?, object()[100..750]);
        assert_eq!(
            fixture.get_ranges(),
            ["bytes=100-399", "bytes=400-699", "bytes=700-749"]
        );

        fixture.server.clear_requests();
        let source = fixture.source(u64::MAX)?;
        assert_eq!(read(&source, 0, OBJECT_SIZE, &token)?, object());
        assert_eq!(read(&source, 999, 1, &token)?, [object()[999]]);
        assert_eq!(fixture.get_ranges(), ["bytes=0-999", "bytes=999-999"]);
        Ok(())
    }

    #[test]
    fn reads_outside_the_object_fail_without_a_request() -> anyhow::Result<()> {
        let fixture = Fixture::new();
        let token = CancellationToken::new();
        let source = fixture.source(64)?;
        for (offset, size) in [(990, 11), (1000, 1), (u64::MAX, 1)] {
            let error = read(&source, offset, size, &token).expect_err("the read passes the end");
            assert_eq!(error.kind(), std::io::ErrorKind::UnexpectedEof, "{error}");
        }
        assert_eq!(read(&source, 1000, 0, &token)?, []);
        assert_eq!(fixture.get_ranges(), Vec::<String>::new());
        Ok(())
    }

    #[test]
    fn responses_that_are_not_the_requested_range_are_rejected() -> anyhow::Result<()> {
        let fixture = Fixture::new();
        let token = CancellationToken::new();
        let source = fixture.source(64)?;
        for (behavior, expected_reason) in [
            (
                MockBehavior {
                    ignores_range: true,
                    ..MockBehavior::default()
                },
                "the response body is longer than the range",
            ),
            (
                MockBehavior {
                    truncated_size: 1,
                    ..MockBehavior::default()
                },
                "the response body has 4 of 5 bytes",
            ),
            (
                MockBehavior {
                    range_shift: 1,
                    ..MockBehavior::default()
                },
                "the response covers `bytes 11-15/1000`",
            ),
        ] {
            fixture.server.set_behavior(behavior);
            let error = read(&source, 10, 5, &token).expect_err("the response is wrong");
            assert!(
                matches!(
                    object_store_error(&error),
                    Some(ObjectStoreError::UnexpectedResponse { begin: 10, end: 14, reason, .. })
                        if expected_reason == reason
                ),
                "{behavior:?}: {error:?}"
            );
        }
        Ok(())
    }

    #[test]
    fn failed_gets_are_typed_errors() -> anyhow::Result<()> {
        let fixture = Fixture::new();
        let source = fixture.source(64)?;
        fixture.server.put_object(BUCKET, KEY, vec![0; 10]);
        let error = read(&source, 500, 5, &CancellationToken::new())
            .expect_err("the object shrank below the range");
        assert!(
            matches!(
                object_store_error(&error),
                Some(ObjectStoreError::Get {
                    begin: 500,
                    end: 504,
                    ..
                })
            ),
            "{error:?}"
        );
        Ok(())
    }

    #[test]
    fn cancelled_reads_are_interrupted() -> anyhow::Result<()> {
        let fixture = Fixture::new();
        let source = fixture.source(64)?;
        let token = CancellationToken::new();
        token.cancel();
        let error = read(&source, 0, 5, &token).expect_err("the token is cancelled");
        assert_eq!(error.kind(), std::io::ErrorKind::Interrupted);
        assert_eq!(fixture.get_ranges(), Vec::<String>::new());

        fixture.server.set_behavior(MockBehavior {
            get_delay_ms: 10_000,
            ..MockBehavior::default()
        });
        let token = CancellationToken::new();
        let canceller = {
            let token = token.clone();
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(100));
                token.cancel();
            })
        };
        let start = Instant::now();
        let error = read(&source, 0, 5, &token).expect_err("the read is cancelled in flight");
        let elapsed = start.elapsed();
        canceller
            .join()
            .map_err(|_| anyhow::anyhow!("the canceller panicked"))?;
        assert_eq!(error.kind(), std::io::ErrorKind::Interrupted);
        assert!(elapsed < Duration::from_secs(5), "{elapsed:?}");
        assert_eq!(fixture.get_ranges(), ["bytes=0-4"]);
        Ok(())
    }

    #[test]
    fn reads_fail_once_the_runtime_shuts_down() -> anyhow::Result<()> {
        let fixture = Fixture::new();
        let source_runtime = multi_thread_runtime();
        let source = S3Source::new(
            fixture.server.client(),
            source_runtime.handle().clone(),
            location(KEY),
            S3SourceConfig::default(),
        )?;
        source_runtime.shutdown_background();
        let error =
            read(&source, 0, 5, &CancellationToken::new()).expect_err("the runtime is gone");
        assert!(
            matches!(
                object_store_error(&error),
                Some(ObjectStoreError::RequestAborted)
            ),
            "{error:?}"
        );
        Ok(())
    }

    #[test]
    fn sources_built_from_an_sdk_config_address_custom_endpoints_path_style() -> anyhow::Result<()>
    {
        use aws_sdk_s3::config::BehaviorVersion;
        use aws_sdk_s3::config::Credentials;
        use aws_sdk_s3::config::Region;
        use aws_sdk_s3::config::SharedCredentialsProvider;

        let fixture = Fixture::new();
        let sdk_config = S3SdkConfig::builder()
            .behavior_version(BehaviorVersion::latest())
            .region(Region::new("us-east-1"))
            // A host name, unlike an IP address, would be addressed virtual-hosted by default.
            .endpoint_url(
                fixture
                    .server
                    .endpoint_url()
                    .replace("127.0.0.1", "localhost"),
            )
            .credentials_provider(SharedCredentialsProvider::new(Credentials::new(
                "test", "test", None, None, "test",
            )))
            .build();
        let source = S3Source::from_sdk_config(
            &sdk_config,
            fixture.runtime.handle().clone(),
            location(KEY),
            S3SourceConfig::default(),
        )?;
        assert_eq!(
            read(&source, 3, 4, &CancellationToken::new())?,
            object()[3..7]
        );
        let requests = fixture.server.requests();
        assert_eq!(
            requests
                .iter()
                .map(|request| (
                    request.method.as_str(),
                    request.bucket.as_str(),
                    request.key.as_str()
                ))
                .collect::<Vec<_>>(),
            [("HEAD", BUCKET, KEY), ("GET", BUCKET, KEY)]
        );
        Ok(())
    }
}
