//! S3 object URLs.

use crate::ObjectStoreError;
use crate::S3Location;

/// The endpoint of AWS itself, which needs no endpoint override.
const AWS_ENDPOINT: &str = "amazonaws.com";

/// A parsed S3 object URL: `s3://bucket/key`, or an HTTP(S) URL in the virtual-hosted style
/// (`https://bucket.s3.[region.]endpoint/key`) or the path style
/// (`https://[s3.[region.]]endpoint/bucket/key`).
///
/// HTTP(S) URLs are recognized like C++ `clp::aws::S3Url` (`AwsAuthenticationSigner.cpp`): the
/// same lowercase character classes, the same greedy matching, and a key that ends at the first
/// `?`. Unlike C++, the AWS global endpoint `s3.amazonaws.com` is not split into the region
/// `amazonaws` and the endpoint `com`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct S3Url {
    location: S3Location,
    region: Option<String>,
    endpoint_url: Option<String>,
}

impl S3Url {
    /// Parses `url`.
    ///
    /// # Returns
    ///
    /// The object, region and endpoint `url` names on success.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * [`ObjectStoreError::InvalidUrl`] if `url` is not an `s3://`, `http://` or `https://` URL
    ///   of an object in one of the recognized styles.
    pub fn parse(url: &str) -> Result<Self, ObjectStoreError> {
        let invalid = |reason| ObjectStoreError::InvalidUrl {
            url: url.to_owned(),
            reason,
        };
        if let Some(rest) = url.strip_prefix("s3://") {
            let (bucket, key) = rest
                .split_once('/')
                .ok_or_else(|| invalid("expected `s3://bucket/key`"))?;
            if bucket.is_empty() || key.is_empty() {
                return Err(invalid("expected `s3://bucket/key`"));
            }
            return Ok(Self {
                location: S3Location {
                    bucket: bucket.to_owned(),
                    key: key.to_owned(),
                },
                region: None,
                endpoint_url: None,
            });
        }
        let (scheme, rest) = url
            .split_once("://")
            .filter(|(scheme, _)| matches!(*scheme, "http" | "https"))
            .ok_or_else(|| invalid("expected an `s3`, `http` or `https` scheme"))?;
        let (host, path) = rest
            .split_once('/')
            .ok_or_else(|| invalid("the URL names no object"))?;
        Self::parse_virtual_hosted(scheme, host, path)
            .or_else(|| Self::parse_path_style(scheme, host, path))
            .ok_or_else(|| invalid("the URL matches neither the virtual-hosted nor the path style"))
    }

    #[must_use]
    pub const fn location(&self) -> &S3Location {
        &self.location
    }

    #[must_use]
    pub fn into_location(self) -> S3Location {
        self.location
    }

    /// # Returns
    ///
    /// The region the URL names, if any.
    #[must_use]
    pub fn region(&self) -> Option<&str> {
        self.region.as_deref()
    }

    /// # Returns
    ///
    /// The `scheme://host[:port]` endpoint of a URL that names a non-AWS endpoint, if any.
    #[must_use]
    pub fn endpoint_url(&self) -> Option<&str> {
        self.endpoint_url.as_deref()
    }

    /// Matches `https://bucket.s3.[region.]endpoint/key`, taking the longest bucket that lets the
    /// rest of the host match (the greedy C++ regex).
    ///
    /// # Returns
    ///
    /// The URL, or `None` if it is not in the virtual-hosted style.
    fn parse_virtual_hosted(scheme: &str, host: &str, path: &str) -> Option<Self> {
        const SEPARATOR: &str = ".s3.";
        let key = parse_key(path)?;
        let mut search_end = host.len();
        while let Some(position) = host[..search_end].rfind(SEPARATOR) {
            let bucket = &host[..position];
            let service_host = &host[position + 1..];
            if is_bucket(bucket)
                && let Some((region, endpoint)) =
                    parse_region_and_endpoint(&service_host["s3.".len()..])
            {
                return Some(Self::new(
                    scheme,
                    service_host,
                    region,
                    endpoint,
                    S3Location {
                        bucket: bucket.to_owned(),
                        key: key.to_owned(),
                    },
                ));
            }
            // A match may overlap the separator just rejected, e.g. `b.s3.s3.e`.
            search_end = position + SEPARATOR.len() - 1;
        }
        None
    }

    /// Matches `https://[s3.[region.]]endpoint/bucket/key`.
    ///
    /// # Returns
    ///
    /// The URL, or `None` if it is not in the path style.
    fn parse_path_style(scheme: &str, host: &str, path: &str) -> Option<Self> {
        let (bucket, rest) = path.split_once('/')?;
        if !is_bucket(bucket) {
            return None;
        }
        let key = parse_key(rest)?;
        let (region, endpoint) = host
            .strip_prefix("s3.")
            .and_then(parse_region_and_endpoint)
            .or_else(|| is_endpoint(host).then_some((None, host)))?;
        Some(Self::new(
            scheme,
            host,
            region,
            endpoint,
            S3Location {
                bucket: bucket.to_owned(),
                key: key.to_owned(),
            },
        ))
    }

    /// Factory function.
    ///
    /// # Returns
    ///
    /// The URL of `location` served by `service_host`, which needs an endpoint override unless
    /// `endpoint` is AWS.
    fn new(
        scheme: &str,
        service_host: &str,
        region: Option<&str>,
        endpoint: &str,
        location: S3Location,
    ) -> Self {
        let (region, endpoint_url) = match (region, endpoint) {
            (Some("amazonaws"), "com") if "s3.amazonaws.com" == service_host => (None, None),
            (region, AWS_ENDPOINT) => (region, None),
            (region, _) => (region, Some(format!("{scheme}://{service_host}"))),
        };
        Self {
            location,
            region: region.map(str::to_owned),
            endpoint_url,
        }
    }
}

/// Matches `[region.]endpoint`, preferring a region (the greedy C++ regex).
///
/// # Returns
///
/// The region, if any, and the endpoint, or `None` if `host` matches neither form.
fn parse_region_and_endpoint(host: &str) -> Option<(Option<&str>, &str)> {
    if let Some((region, endpoint)) = host.split_once('.')
        && is_region(region)
        && is_endpoint(endpoint)
    {
        return Some((Some(region), endpoint));
    }
    is_endpoint(host).then_some((None, host))
}

/// # Returns
///
/// The key at the start of `path`, ending at the first `?`, or `None` if it is empty.
fn parse_key(path: &str) -> Option<&str> {
    let key = path.split_once('?').map_or(path, |(key, _)| key);
    (!key.is_empty()).then_some(key)
}

/// # Returns
///
/// Whether `bucket` matches `[a-z0-9.-]+`.
fn is_bucket(bucket: &str) -> bool {
    !bucket.is_empty()
        && bucket
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || b".-".contains(&byte))
}

/// # Returns
///
/// Whether `region` matches `[a-z0-9-]+`.
fn is_region(region: &str) -> bool {
    !region.is_empty()
        && region
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || b'-' == byte)
}

/// # Returns
///
/// Whether `endpoint` matches `[a-z0-9.-]+(:[0-9]+)?`.
fn is_endpoint(endpoint: &str) -> bool {
    match endpoint.split_once(':') {
        Some((name, port)) => {
            is_bucket(name) && !port.is_empty() && port.bytes().all(|byte| byte.is_ascii_digit())
        }
        None => is_bucket(endpoint),
    }
}

#[cfg(test)]
mod tests {
    use super::S3Url;
    use crate::ObjectStoreError;
    use crate::S3Location;

    /// # Returns
    ///
    /// The bucket, key, region and endpoint URL of `url` on success.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * Forwards [`S3Url::parse`]'s return values on failure.
    fn parts(url: &str) -> anyhow::Result<(String, String, Option<String>, Option<String>)> {
        let parsed = S3Url::parse(url)?;
        let region = parsed.region().map(str::to_owned);
        let endpoint_url = parsed.endpoint_url().map(str::to_owned);
        let S3Location { bucket, key } = parsed.into_location();
        Ok((bucket, key, region, endpoint_url))
    }

    #[test]
    fn s3_scheme_urls_name_a_bucket_and_a_key() -> anyhow::Result<()> {
        assert_eq!(
            parts("s3://logs/a/b/archive.clps")?,
            ("logs".to_owned(), "a/b/archive.clps".to_owned(), None, None)
        );
        assert_eq!(
            parts("s3://Logs_1/key?with=query")?,
            ("Logs_1".to_owned(), "key?with=query".to_owned(), None, None)
        );
        Ok(())
    }

    #[test]
    fn virtual_hosted_urls_follow_the_cpp_regex() -> anyhow::Result<()> {
        assert_eq!(
            parts("https://my.bucket.s3.us-east-2.amazonaws.com/dir/archive.clps?versionId=1")?,
            (
                "my.bucket".to_owned(),
                "dir/archive.clps".to_owned(),
                Some("us-east-2".to_owned()),
                None
            )
        );
        assert_eq!(
            parts("http://bucket.s3.minio.local:9000/archive")?,
            (
                "bucket".to_owned(),
                "archive".to_owned(),
                Some("minio".to_owned()),
                Some("http://s3.minio.local:9000".to_owned())
            )
        );
        assert_eq!(
            parts("https://bucket.s3.amazonaws.com/archive")?,
            ("bucket".to_owned(), "archive".to_owned(), None, None)
        );
        assert_eq!(
            parts("https://b.s3.s3.example.com/k")?,
            (
                "b.s3".to_owned(),
                "k".to_owned(),
                Some("example".to_owned()),
                Some("https://s3.example.com".to_owned())
            )
        );
        Ok(())
    }

    #[test]
    fn path_style_urls_follow_the_cpp_regex() -> anyhow::Result<()> {
        assert_eq!(
            parts("http://127.0.0.1:9000/bucket/dir/archive.clps")?,
            (
                "bucket".to_owned(),
                "dir/archive.clps".to_owned(),
                None,
                Some("http://127.0.0.1:9000".to_owned())
            )
        );
        assert_eq!(
            parts("https://s3.eu-west-1.amazonaws.com/bucket/archive")?,
            (
                "bucket".to_owned(),
                "archive".to_owned(),
                Some("eu-west-1".to_owned()),
                None
            )
        );
        assert_eq!(
            parts("https://s3.amazonaws.com/bucket/archive")?,
            ("bucket".to_owned(), "archive".to_owned(), None, None)
        );
        assert_eq!(
            parts("http://s3.localhost:9000/bucket/archive")?,
            (
                "bucket".to_owned(),
                "archive".to_owned(),
                None,
                Some("http://s3.localhost:9000".to_owned())
            )
        );
        Ok(())
    }

    #[test]
    fn malformed_urls_are_rejected() {
        for url in [
            "",
            "archive.clps",
            "/tmp/archive.clps",
            "s3://bucket",
            "s3://bucket/",
            "s3:///key",
            "ftp://host/bucket/key",
            "HTTPS://host/bucket/key",
            "https://host",
            "https://host/bucket",
            "https://host/bucket/",
            "https://host/bucket/?query",
            "https://Host/bucket/key",
            "https://host/Bucket/key",
            "https://host:port/bucket/key",
            "https://host_name/bucket/key",
        ] {
            let error = S3Url::parse(url).expect_err(url);
            assert!(
                matches!(error, ObjectStoreError::InvalidUrl { .. }),
                "{url}: {error:?}"
            );
        }
    }

    #[test]
    fn file_names_are_the_last_key_segment() {
        for (key, expected) in [
            ("a/b/archive.clps", Some("archive.clps")),
            ("archive.clps", Some("archive.clps")),
            ("dir/", None),
        ] {
            let location = S3Location {
                bucket: "bucket".to_owned(),
                key: key.to_owned(),
            };
            assert_eq!(location.file_name(), expected, "{key}");
        }
    }
}
