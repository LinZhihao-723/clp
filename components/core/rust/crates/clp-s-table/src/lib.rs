//! The data model of clp-s tables: the mapping from node types to column bytes, compiled table
//! layouts and record shapes, loaded tables, and window-at-a-time scanning with selections.
//!
//! The crate holds no predicate logic, no JSON and no I/O: `clp-s-search` evaluates filters over
//! its windows and `clp-s-json` renders its batches.

mod archive;
mod array;
mod column;
mod demand;
mod encoding;
mod error;
mod ids;
mod layout;
mod projection;
mod record;
mod scan;
mod selection;
mod table;
mod value;

pub use archive::ArchiveTables;
pub use array::ArrayEvent;
pub use array::ArrayScalar;
pub use array::scan_array;
pub use column::ColumnBatch;
pub use column::EncodedTextBatch;
pub use column::LeF64;
pub use column::LeI64;
pub use column::LeU16;
pub use column::LeU64;
pub use demand::ColumnDemand;
pub use demand::ColumnSet;
pub use encoding::ColumnEncoding;
pub use error::LayoutError;
pub use error::TableError;
pub use ids::ColumnIndex;
pub use ids::RecordId;
pub use layout::ColumnSpec;
pub use layout::RecordShape;
pub use layout::ShapeEvent;
pub use layout::TableLayout;
pub use projection::ResolvedProjection;
pub use record::RecordView;
pub use record::RecordVisitor;
pub use scan::TableBatch;
pub use scan::TableScanState;
pub use scan::TableScanner;
pub use scan::WindowColumns;
pub use scan::WindowParts;
pub use selection::Selection;
pub use table::Table;
pub use value::EncodedTextRef;
pub use value::FloatRef;
pub use value::StringRef;
pub use value::ValueRef;

/// The default number of rows per scan window.
pub const DEFAULT_BATCH_NUM_ROWS: u32 = 4096;

const _: () = {
    const fn assert_send_sync<SendSyncType: Send + Sync + 'static>() {}
    const fn assert_send<SendType: Send + 'static>() {}

    assert_send_sync::<ArchiveTables>();
    assert_send_sync::<TableLayout>();
    assert_send_sync::<Table>();
    assert_send_sync::<ColumnDemand>();
    assert_send::<TableScanner>();
    assert_send::<TableScanState>();
};
