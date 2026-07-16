use std::sync::LazyLock;

use regex::Regex;

/// The default dataset name (mirror of `clp_py_utils.clp_config.CLP_DEFAULT_DATASET_NAME`).
pub const CLP_DEFAULT_DATASET_NAME: &str = "default";

pub static VALID_DATASET_NAME_REGEX: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^[a-zA-Z0-9_]+$").unwrap());
