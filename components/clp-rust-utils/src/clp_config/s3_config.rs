use serde::Serialize;

/// Represents the configuration for connecting to an S3 bucket.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct S3Config {
    pub bucket: String,
    pub region_code: String,
    pub key_prefix: String,
    pub aws_authentication: AwsAuthentication,
}

/// An enum representing AWS authentication methods.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "type")]
pub enum AwsAuthentication {
    #[serde(rename = "credentials")]
    Credentials { credentials: AwsCredentials },
}

/// Represents AWS credentials.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AwsCredentials {
    pub access_key_id: String,
    pub secret_access_key: String,
}
