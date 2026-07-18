use crate::{job_config::S3InputConfig, s3::ObjectMetadata};

pub async fn get_object_metadata(_config: &S3InputConfig) -> anyhow::Result<Vec<ObjectMetadata>> {
    Ok(Vec::new())
}
