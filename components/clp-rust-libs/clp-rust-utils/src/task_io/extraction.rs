//! Protocol types exchanged with the Spider task that extracts CLP-S JSON streams.

use std::num::NonZeroU64;

use serde::Deserialize;
use serde::Serialize;

use crate::dataset::DatasetName;
use crate::job_config::QueryJobId;
use crate::types::ArchiveId;

/// The `query::clp_s_extract_json` task's single input.
///
/// It's encoded as positional msgpack, so the field order is part of the wire contract.
#[derive(Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ExtractTaskInput {
    pub job_id: QueryJobId,
    pub dataset: DatasetName,
    pub archive_id: ArchiveId,
    pub target_chunk_size: Option<NonZeroU64>,
}

#[cfg(test)]
mod tests {
    use std::num::NonZeroU64;

    use super::ExtractTaskInput;
    use crate::dataset::DatasetName;

    #[test]
    fn extract_task_input_round_trips_through_positional_msgpack_in_field_order()
    -> anyhow::Result<()> {
        let input = ExtractTaskInput {
            job_id: 7,
            dataset: "logs".parse::<DatasetName>()?,
            archive_id: "018e90e5-8b2a-4a61-a2fc-cac799936caf".parse()?,
            target_chunk_size: NonZeroU64::new(1024),
        };

        let encoded = rmp_serde::to_vec(&input)?;

        assert_eq!(
            encoded,
            rmp_serde::to_vec(&(
                input.job_id,
                input.dataset.as_str(),
                input.archive_id,
                input.target_chunk_size,
            ))?
        );
        assert_eq!(rmp_serde::from_slice::<ExtractTaskInput>(&encoded)?, input);
        Ok(())
    }
}
