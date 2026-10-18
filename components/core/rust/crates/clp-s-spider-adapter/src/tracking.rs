//! A sink wrapper that records whether any result reached the wrapped sink, which decides whether
//! a failed in-process search may still fall back to the `clp-s` subprocess.

use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;

use clp_s_reader::MergeOrder;
use clp_s_reader::OutputRequirements;
use clp_s_reader::PartialSink;
use clp_s_reader::PartitionInfo;
use clp_s_reader::PartitionSummary;
use clp_s_reader::RecordBatch;
use clp_s_reader::Sink;
use clp_s_reader::SinkControl;
use clp_s_reader::SinkError;

/// Forwards everything to the wrapped sink and records whether a batch was handed to it.
///
/// A batch counts as written as soon as it is handed over, since the wrapped sink may write part of
/// it before failing.
#[derive(Debug)]
pub struct WriteTrackingSink<SinkType: Sink> {
    sink: SinkType,
    has_written: Arc<AtomicBool>,
}

impl<SinkType: Sink> WriteTrackingSink<SinkType> {
    /// Factory function.
    ///
    /// # Returns
    ///
    /// A wrapper of `sink` that has not written anything yet.
    pub fn new(sink: SinkType) -> Self {
        Self {
            sink,
            has_written: Arc::new(AtomicBool::new(false)),
        }
    }

    /// # Returns
    ///
    /// Whether any batch was handed to the wrapped sink.
    pub fn has_written(&self) -> bool {
        self.has_written.load(Ordering::Acquire)
    }
}

impl<SinkType: Sink> Sink for WriteTrackingSink<SinkType> {
    type Output = SinkType::Output;
    type Partial = WriteTrackingPartial<SinkType::Partial>;

    fn requirements(&self) -> OutputRequirements {
        self.sink.requirements()
    }

    fn merge_order(&self) -> MergeOrder {
        self.sink.merge_order()
    }

    fn open_partial(&self, partition: &PartitionInfo) -> Result<Self::Partial, SinkError> {
        Ok(WriteTrackingPartial {
            partial: self.sink.open_partial(partition)?,
            has_written: Arc::clone(&self.has_written),
        })
    }

    fn merge(
        &self,
        output: &mut Self::Output,
        partial: <Self::Partial as PartialSink>::Output,
    ) -> Result<SinkControl, SinkError> {
        self.sink.merge(output, partial)
    }
}

/// The partial sink of [`WriteTrackingSink`].
#[derive(Debug)]
pub struct WriteTrackingPartial<PartialType: PartialSink> {
    partial: PartialType,
    has_written: Arc<AtomicBool>,
}

impl<PartialType: PartialSink> PartialSink for WriteTrackingPartial<PartialType> {
    type Output = PartialType::Output;

    fn consume(&mut self, batch: &RecordBatch<'_>) -> Result<SinkControl, SinkError> {
        self.has_written.store(true, Ordering::Release);
        self.partial.consume(batch)
    }

    fn finish(self, summary: &PartitionSummary) -> Result<Self::Output, SinkError> {
        self.partial.finish(summary)
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::path::PathBuf;
    use std::sync::Arc;

    use clp_s_reader::ArchiveId;
    use clp_s_reader::ArchiveLocation;
    use clp_s_reader::MergeOrder;
    use clp_s_reader::OutputRequirements;
    use clp_s_reader::PartialSink;
    use clp_s_reader::PartitionInfo;
    use clp_s_reader::PartitionSummary;
    use clp_s_reader::Query;
    use clp_s_reader::QueryOptions;
    use clp_s_reader::ReaderContext;
    use clp_s_reader::RecordBatch;
    use clp_s_reader::ScanRequest;
    use clp_s_reader::SearchSession;
    use clp_s_reader::Sink;
    use clp_s_reader::SinkControl;
    use clp_s_reader::SinkError;

    use super::WriteTrackingSink;

    /// Counts the records it receives.
    #[derive(Debug)]
    struct CountingSink;

    /// The partial sink of [`CountingSink`].
    #[derive(Debug)]
    struct CountingPartial(u64);

    impl Sink for CountingSink {
        type Output = u64;
        type Partial = CountingPartial;

        fn requirements(&self) -> OutputRequirements {
            OutputRequirements::json()
        }

        fn merge_order(&self) -> MergeOrder {
            MergeOrder::Scan
        }

        fn open_partial(&self, _partition: &PartitionInfo) -> Result<Self::Partial, SinkError> {
            Ok(CountingPartial(0))
        }

        fn merge(&self, output: &mut u64, partial: u64) -> Result<SinkControl, SinkError> {
            *output += partial;
            Ok(SinkControl::Continue)
        }
    }

    impl PartialSink for CountingPartial {
        type Output = u64;

        fn consume(&mut self, batch: &RecordBatch<'_>) -> Result<SinkControl, SinkError> {
            self.0 += batch.num_records() as u64;
            Ok(SinkControl::Continue)
        }

        fn finish(self, _summary: &PartitionSummary) -> Result<u64, SinkError> {
            Ok(self.0)
        }
    }

    fn fixture(name: &str) -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../tests/fixtures/archives")
            .join(format!("{name}.clps"))
    }

    fn run(kql: &str) -> anyhow::Result<(u64, bool)> {
        let sink = Arc::new(WriteTrackingSink::new(CountingSink));
        let request = ScanRequest::builder()
            .query(Query::from_kql(kql, &QueryOptions::default())?)
            .build()?;
        let report = SearchSession::builder(ReaderContext::builder().build()?, Arc::new(request))
            .archive(
                ArchiveId::from("search.ts.clps"),
                ArchiveLocation::File(fixture("search.ts")),
            )
            .build()?
            .run(Arc::clone(&sink))?;
        Ok((report.output, sink.has_written()))
    }

    #[test]
    fn matches_are_recorded_as_written() -> anyhow::Result<()> {
        let (num_records, has_written) = run("*")?;
        assert_eq!(num_records, 40);
        assert!(has_written);
        Ok(())
    }

    #[test]
    fn searches_without_matches_write_nothing() -> anyhow::Result<()> {
        assert_eq!(run("no_such_key: 1")?, (0, false));
        Ok(())
    }
}
