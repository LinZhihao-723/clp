use num_enum::{IntoPrimitive, TryFromPrimitive};
use serde::{Deserialize, Serialize};
use sqlx::{Database, Decode, Encode, MySql, encode::IsNull, error::BoxDynError};
use strum::EnumString;
use utoipa::ToSchema;

pub type CompressionJobId = i32;

// Mirror of `job_orchestration.scheduler.constants.CompressionJobStatus`. Must be kept in sync.
#[derive(
    Clone,
    Copy,
    Debug,
    Deserialize,
    EnumString,
    Eq,
    IntoPrimitive,
    PartialEq,
    Serialize,
    ToSchema,
    TryFromPrimitive,
)]
#[repr(i32)]
#[strum(ascii_case_insensitive)]
pub enum CompressionJobStatus {
    /// Job is waiting to be scheduled.
    Pending = 0,
    /// Job is currently executing.
    Running = 1,
    /// Job completed successfully.
    Succeeded = 2,
    /// Job failed.
    Failed = 3,
    /// Job was killed by a user.
    Killed = 4,
}

crate::impl_sqlx_type!(CompressionJobStatus => i32);

impl<'r> Decode<'r, MySql> for CompressionJobStatus {
    fn decode(value: <MySql as Database>::ValueRef<'r>) -> Result<Self, BoxDynError> {
        let raw = <i32 as Decode<MySql>>::decode(value)?;
        Ok(Self::try_from(raw)?)
    }
}

impl<'q> Encode<'q, MySql> for CompressionJobStatus {
    fn encode_by_ref(
        &self,
        buf: &mut <MySql as Database>::ArgumentBuffer<'q>,
    ) -> Result<IsNull, BoxDynError> {
        <i32 as Encode<MySql>>::encode_by_ref(&i32::from(*self), buf)
    }
}
