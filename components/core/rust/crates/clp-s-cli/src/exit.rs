//! Exit codes.

use clp_s_reader::ErrorKind;

/// Success, including searches with no match.
pub const EXIT_SUCCESS: u8 = 0;

/// Invalid arguments or query (also clap usage errors, which clap would report as 2).
pub const EXIT_INVALID_ARGUMENT: u8 = 1;

/// A corrupt or unsupported archive.
pub const EXIT_ARCHIVE: u8 = 2;

/// An I/O error.
pub const EXIT_IO: u8 = 3;

/// A sink or downstream failure.
pub const EXIT_SINK: u8 = 4;

/// A resource limit or the memory budget was exceeded.
pub const EXIT_RESOURCE_EXHAUSTED: u8 = 5;

/// A contained internal error.
pub const EXIT_INTERNAL: u8 = 70;

/// Cancelled (SIGINT).
pub const EXIT_CANCELLED: u8 = 130;

/// Maps an error category onto the process exit code.
///
/// # Returns
///
/// The exit code.
#[must_use]
pub const fn exit_code(kind: ErrorKind) -> u8 {
    match kind {
        ErrorKind::InvalidArgument | ErrorKind::InvalidQuery => EXIT_INVALID_ARGUMENT,
        ErrorKind::CorruptArchive | ErrorKind::UnsupportedArchive => EXIT_ARCHIVE,
        ErrorKind::Io => EXIT_IO,
        ErrorKind::Sink => EXIT_SINK,
        ErrorKind::ResourceExhausted => EXIT_RESOURCE_EXHAUSTED,
        ErrorKind::Cancelled => EXIT_CANCELLED,
        _ => EXIT_INTERNAL,
    }
}

#[cfg(test)]
mod tests {
    use clp_s_reader::ErrorKind;

    use super::exit_code;

    #[test]
    fn error_kinds_map_to_the_documented_exit_codes() {
        for (kind, code) in [
            (ErrorKind::InvalidArgument, 1),
            (ErrorKind::InvalidQuery, 1),
            (ErrorKind::CorruptArchive, 2),
            (ErrorKind::UnsupportedArchive, 2),
            (ErrorKind::Io, 3),
            (ErrorKind::Sink, 4),
            (ErrorKind::ResourceExhausted, 5),
            (ErrorKind::Internal, 70),
            (ErrorKind::Cancelled, 130),
        ] {
            assert_eq!(exit_code(kind), code, "{kind:?}");
        }
    }
}
