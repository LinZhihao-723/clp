//! Configs of stream extraction jobs.

use std::num::NonZeroU64;

use serde::Deserialize;
use serde::Serialize;
use serde_with::DisplayFromStr;
use serde_with::serde_as;

use crate::dataset::DatasetName;
use crate::types::ArchiveId;

/// The config of a job that extracts an archive's JSON stream as ordered chunks.
#[serde_as]
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ExtractJobConfig {
    pub dataset: DatasetName,

    #[serde_as(as = "DisplayFromStr")]
    pub archive_id: ArchiveId,

    /// The target uncompressed size of each chunk, or `None` to extract the stream as one chunk.
    pub target_chunk_size: Option<NonZeroU64>,
}
