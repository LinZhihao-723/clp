//! Termination stages and prune reports.

/// The stage at which a search of one archive ended; the strings are the C++ telemetry strings
/// (`search/SearchTelemetry.hpp`) plus the stages C++ does not have.
#[non_exhaustive]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum TerminationStage {
    RangeIndexMatching,
    TimeRangeMatching,
    TimeRangeMatchingAfterColumnResolution,
    SchemaMatching,
    DictionarySearch,
    ErtScan,

    /// The query folded to False before any archive structure was consulted.
    LogicallyFalse,

    /// A sink asked the session to stop.
    StoppedBySink,
}

impl TerminationStage {
    /// # Returns
    ///
    /// The telemetry string of the stage.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::RangeIndexMatching => "range_index_matching",
            Self::TimeRangeMatching => "time_range_matching",
            Self::TimeRangeMatchingAfterColumnResolution => {
                "time_range_matching_after_column_resolution"
            }
            Self::SchemaMatching => "schema_matching",
            Self::DictionarySearch => "dictionary_search",
            Self::ErtScan => "ert_scan",
            Self::LogicallyFalse => "logically_false",
            Self::StoppedBySink => "stopped_by_sink",
        }
    }
}

/// Why planning proved an archive empty.
#[non_exhaustive]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum PruneReason {
    /// The query folds to False.
    LogicallyFalse,

    /// The query's time predicates are disjoint from the archive's timestamp range.
    TimeRangeDisjoint,

    /// The query has time bounds but the archive has no authoritative timestamp column (DV-20:
    /// zero rows plus a diagnostic, not an error).
    NoTimestampColumn,

    /// The range index proves no record can match.
    RangeIndexMismatch,

    /// No table's schema can satisfy the query.
    NoCandidateTable,

    /// The dictionaries prove no record can match.
    DictionaryMiss,
}

/// The outcome of planning an archive that cannot match.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct PruneReport {
    pub stage: TerminationStage,
    pub reason: PruneReason,
}
