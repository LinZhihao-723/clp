//! Errors of the variable encoding.

/// Errors raised while decoding encoded variables and messages read from an archive.
#[non_exhaustive]
#[derive(Debug, thiserror::Error)]
pub enum EncodingError {
    #[error("invalid encoded float variable {encoded:#018x}")]
    InvalidEncodedFloat { encoded: u64 },

    #[error("logtype has {num_placeholders} variable placeholders but {num_variables} variables")]
    VariableCountMismatch {
        num_placeholders: usize,
        num_variables: usize,
    },

    #[error("unknown dictionary variable id {id}")]
    UnknownDictionaryVariable { id: u64 },

    #[error("invalid logtype placeholder offset {offset}")]
    InvalidPlaceholderOffset { offset: usize },
}
