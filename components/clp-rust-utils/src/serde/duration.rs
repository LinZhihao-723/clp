use std::time::Duration;

use serde::{Deserialize, Deserializer, de::Error as _};

pub fn deserialize_duration_seconds<'de, D>(deserializer: D) -> Result<Duration, D::Error>
where
    D: Deserializer<'de>, {
    let seconds = f64::deserialize(deserializer)?;
    if seconds <= 0.0 {
        return Err(D::Error::custom("duration must be positive"));
    }

    Duration::try_from_secs_f64(seconds).map_err(D::Error::custom)
}
