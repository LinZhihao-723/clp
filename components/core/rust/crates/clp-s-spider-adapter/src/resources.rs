//! Package-wide resources shared by every search task of an execution manager.

use std::collections::HashMap;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::OnceLock;
use std::sync::PoisonError;

use clp_rust_utils::clp_config::AwsAuthentication;
use clp_s_reader::ReaderContext;
use clp_s_reader::s3::S3Client;
use clp_s_results_cache::MongoDatabase;
use non_empty_string::NonEmptyString;

/// The adapter's settings that `SpiderTaskExecutorConfig` does not carry.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AdapterConfig {
    /// The CLP installation directory (the package's `CLP_HOME`): relative archive directories
    /// resolve against it, and the `clp-s` fallback runs its `bin/clp-s`.
    pub clp_home: PathBuf,

    /// How long a task waits for the memory budget to admit the base reservation of a stream
    /// (`WaitForBase` admission) before it fails with `ResourceExhausted`.
    pub base_admission_timeout_sec: u64,
}

/// The resources every search task of one execution manager shares: the tokio runtime, the reader
/// context (one memory budget and buffer pool), and the S3 and `MongoDB` clients, built once.
///
/// Like the package's runtime, one instance lives in a process-wide cell, installed once with
/// [`AdapterResources::init`].
#[derive(Debug)]
pub struct AdapterResources {
    runtime: tokio::runtime::Handle,
    reader_context: Arc<ReaderContext>,
    config: AdapterConfig,
    s3_clients: Mutex<Vec<S3ClientEntry>>,
    results_caches: Mutex<HashMap<String, MongoDatabase>>,
}

impl AdapterResources {
    /// Factory function.
    ///
    /// # Returns
    ///
    /// The resources on success.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * [`anyhow::Error`] if `runtime` is a current-thread runtime.
    pub fn new(
        runtime: tokio::runtime::Handle,
        reader_context: Arc<ReaderContext>,
        config: AdapterConfig,
    ) -> anyhow::Result<Self> {
        if tokio::runtime::RuntimeFlavor::CurrentThread == runtime.runtime_flavor() {
            anyhow::bail!("the Spider adapter requires a multi-thread tokio runtime");
        }
        Ok(Self {
            runtime,
            reader_context,
            config,
            s3_clients: Mutex::new(Vec::new()),
            results_caches: Mutex::new(HashMap::new()),
        })
    }

    /// Installs the process-wide resources.
    ///
    /// Like the package's `init_*` functions, a call made once the resources are installed keeps
    /// the installed ones (after validating the arguments), so concurrent tasks may all call it. A
    /// call whose config differs from the installed one is logged as a warning; its runtime and
    /// reader context are dropped.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * Forwards [`Self::new`]'s return values on failure.
    pub fn init(
        runtime: tokio::runtime::Handle,
        reader_context: Arc<ReaderContext>,
        config: AdapterConfig,
    ) -> anyhow::Result<()> {
        let resources = Self::new(runtime, reader_context, config)?;
        if let Err(rejected) = RESOURCES.set(resources)
            && let Some(installed) = RESOURCES.get()
            && installed.config != rejected.config
        {
            tracing::warn!(
                installed_config = ? installed.config,
                rejected_config = ? rejected.config,
                "The Spider adapter resources are already installed; ignoring a different config."
            );
        }
        Ok(())
    }

    /// # Returns
    ///
    /// The process-wide resources on success.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * [`anyhow::Error`] if [`Self::init`] has not been called.
    pub fn global() -> anyhow::Result<&'static Self> {
        RESOURCES
            .get()
            .ok_or_else(|| anyhow::anyhow!("the Spider adapter resources are not initialized"))
    }

    /// # Returns
    ///
    /// The runtime that drives S3 and `MongoDB` I/O.
    #[must_use]
    pub const fn runtime(&self) -> &tokio::runtime::Handle {
        &self.runtime
    }

    /// # Returns
    ///
    /// The shared reader context.
    #[must_use]
    pub const fn reader_context(&self) -> &Arc<ReaderContext> {
        &self.reader_context
    }

    /// # Returns
    ///
    /// The CLP installation directory.
    #[must_use]
    pub fn clp_home(&self) -> &Path {
        &self.config.clp_home
    }

    /// # Returns
    ///
    /// The `WaitForBase` admission timeout in seconds.
    #[must_use]
    pub const fn base_admission_timeout_sec(&self) -> u64 {
        self.config.base_admission_timeout_sec
    }

    /// Returns the S3 client for `(region, endpoint, authentication)`, creating it on first use
    /// with [`clp_rust_utils::s3::create_new_client`].
    ///
    /// # Returns
    ///
    /// The client.
    ///
    /// # Panics
    ///
    /// Panics if called from within an asynchronous execution context (`block_on` on the runtime).
    #[must_use]
    pub fn s3_client(
        &self,
        region: &str,
        endpoint: Option<&NonEmptyString>,
        authentication: &AwsAuthentication,
    ) -> S3Client {
        let mut s3_clients = self
            .s3_clients
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if let Some(entry) = s3_clients.iter().find(|entry| {
            entry.region == region
                && entry.endpoint.as_ref() == endpoint
                && &entry.authentication == authentication
        }) {
            return entry.client.clone();
        }
        let client = self.runtime.block_on(clp_rust_utils::s3::create_new_client(
            region,
            endpoint,
            authentication,
        ));
        s3_clients.push(S3ClientEntry {
            region: region.to_owned(),
            endpoint: endpoint.cloned(),
            authentication: authentication.clone(),
            client: client.clone(),
        });
        drop(s3_clients);
        client
    }

    /// Returns the results-cache database for `uri`, connecting on first use.
    ///
    /// # Returns
    ///
    /// The database on success.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * Forwards [`clp_s_results_cache::connect_database`]'s return values on failure.
    pub fn results_cache(&self, uri: &str) -> anyhow::Result<MongoDatabase> {
        let mut results_caches = self
            .results_caches
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if let Some(database) = results_caches.get(uri) {
            return Ok(database.clone());
        }
        let database = clp_s_results_cache::connect_database(&self.runtime, uri)?;
        results_caches.insert(uri.to_owned(), database.clone());
        drop(results_caches);
        Ok(database)
    }
}

static RESOURCES: OnceLock<AdapterResources> = OnceLock::new();

/// An S3 client and the settings it was built with.
#[derive(Debug)]
struct S3ClientEntry {
    region: String,
    endpoint: Option<NonEmptyString>,
    authentication: AwsAuthentication,
    client: S3Client,
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::sync::Arc;
    use std::sync::Mutex;
    use std::sync::PoisonError;

    use clp_rust_utils::clp_config::AwsAuthentication;
    use clp_rust_utils::clp_config::AwsCredentials;
    use clp_s_reader::ReaderContext;

    use super::AdapterConfig;
    use super::AdapterResources;

    /// A log writer appending to a buffer the test keeps.
    #[derive(Clone, Debug, Default)]
    struct LogBuffer {
        bytes: Arc<Mutex<Vec<u8>>>,
    }

    impl LogBuffer {
        /// # Returns
        ///
        /// The logged text.
        fn text(&self) -> String {
            String::from_utf8_lossy(&self.bytes.lock().unwrap_or_else(PoisonError::into_inner))
                .into_owned()
        }
    }

    impl std::io::Write for LogBuffer {
        fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
            self.bytes
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .extend_from_slice(data);
            Ok(data.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl<'writer> tracing_subscriber::fmt::MakeWriter<'writer> for LogBuffer {
        type Writer = Self;

        fn make_writer(&'writer self) -> Self::Writer {
            self.clone()
        }
    }

    /// # Returns
    ///
    /// A multi-thread runtime with one worker.
    fn multi_thread_runtime() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .expect("a multi-thread runtime can be built")
    }

    /// # Returns
    ///
    /// The config the tests install.
    fn config() -> AdapterConfig {
        AdapterConfig {
            clp_home: PathBuf::from("/clp"),
            base_admission_timeout_sec: 1,
        }
    }

    #[test]
    fn current_thread_runtimes_are_rejected() -> anyhow::Result<()> {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?;
        let error = AdapterResources::new(
            runtime.handle().clone(),
            ReaderContext::builder().build()?,
            config(),
        )
        .expect_err("a current-thread runtime is rejected");
        assert!(error.to_string().contains("multi-thread"), "{error}");
        let error = AdapterResources::init(
            runtime.handle().clone(),
            ReaderContext::builder().build()?,
            config(),
        )
        .expect_err("a current-thread runtime is rejected even by init");
        assert!(error.to_string().contains("multi-thread"), "{error}");
        Ok(())
    }

    #[test]
    fn global_resources_exist_only_after_init() -> anyhow::Result<()> {
        let error = AdapterResources::global().expect_err("nothing is installed yet");
        assert!(error.to_string().contains("not initialized"), "{error}");

        let runtime = multi_thread_runtime();
        let reader_context = ReaderContext::builder().build()?;
        AdapterResources::init(
            runtime.handle().clone(),
            Arc::clone(&reader_context),
            config(),
        )?;
        let installed = AdapterResources::global()?;
        assert!(Arc::ptr_eq(installed.reader_context(), &reader_context));
        assert_eq!(installed.clp_home(), PathBuf::from("/clp"));

        let logs = LogBuffer::default();
        let subscriber = tracing_subscriber::fmt()
            .with_writer(logs.clone())
            .with_ansi(false)
            .finish();
        tracing::subscriber::with_default(subscriber, || {
            AdapterResources::init(
                runtime.handle().clone(),
                ReaderContext::builder().build()?,
                config(),
            )?;
            anyhow::ensure!(logs.text().is_empty(), "{}", logs.text());
            AdapterResources::init(
                runtime.handle().clone(),
                ReaderContext::builder().build()?,
                AdapterConfig {
                    clp_home: PathBuf::from("/elsewhere"),
                    ..config()
                },
            )
        })?;
        let warnings = logs.text();
        assert!(warnings.contains("WARN"), "{warnings}");
        assert!(warnings.contains("/elsewhere"), "{warnings}");
        let installed = AdapterResources::global()?;
        assert!(
            Arc::ptr_eq(installed.reader_context(), &reader_context),
            "a second init keeps the installed resources"
        );
        assert_eq!(installed.clp_home(), PathBuf::from("/clp"));
        Ok(())
    }

    #[test]
    fn results_caches_are_cached_per_uri() -> anyhow::Result<()> {
        let runtime = multi_thread_runtime();
        let resources = AdapterResources::new(
            runtime.handle().clone(),
            ReaderContext::builder().build()?,
            config(),
        )?;
        let first = resources.results_cache("mongodb://127.0.0.1:1/first")?;
        let second = resources.results_cache("mongodb://127.0.0.1:1/second")?;
        assert_eq!(first.name(), "first");
        assert_eq!(second.name(), "second");
        assert_eq!(
            resources
                .results_caches
                .lock()
                .map(|caches| caches.len())
                .ok(),
            Some(2)
        );
        let again = resources.results_cache("mongodb://127.0.0.1:1/first")?;
        assert_eq!(again.name(), "first");
        assert_eq!(
            resources
                .results_caches
                .lock()
                .map(|caches| caches.len())
                .ok(),
            Some(2)
        );

        let error = resources
            .results_cache("mongodb://127.0.0.1:1")
            .expect_err("a URI without a database is rejected");
        assert!(error.to_string().contains("names no database"), "{error}");
        Ok(())
    }

    #[test]
    fn s3_clients_are_cached_per_settings() -> anyhow::Result<()> {
        let runtime = multi_thread_runtime();
        let resources = AdapterResources::new(
            runtime.handle().clone(),
            ReaderContext::builder().build()?,
            config(),
        )?;
        let credentials = AwsAuthentication::Credentials {
            credentials: AwsCredentials {
                access_key_id: "key".to_owned(),
                secret_access_key: "secret".to_owned(),
                session_token: None,
            },
        };
        let endpoint = non_empty_string::NonEmptyString::new("http://127.0.0.1:1".to_owned())
            .expect("the endpoint is not empty");
        let client = resources.s3_client("us-east-1", Some(&endpoint), &credentials);
        assert_eq!(
            client.config().region().map(ToString::to_string),
            Some("us-east-1".to_owned())
        );
        let _ = resources.s3_client("us-east-1", Some(&endpoint), &credentials);
        let _ = resources.s3_client("us-west-2", Some(&endpoint), &credentials);
        let _ = resources.s3_client("us-east-1", None, &credentials);
        let num_clients = resources
            .s3_clients
            .lock()
            .map(|clients| clients.len())
            .ok();
        assert_eq!(num_clients, Some(3));
        Ok(())
    }
}
